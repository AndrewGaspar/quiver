//! A private Xvfb for the key tests, and XTEST key presses into it. Never the desktop's X
//! server: Xvfb picks a free display itself (`-displayfd`).

#![allow(dead_code)]

use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::os::fd::FromRawFd;
use std::process::{Child, Command, Stdio};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;

pub struct Xvfb {
    child: Child,
    /// `:<n>`.
    pub display: String,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Xvfb {
    /// `None` (and a note on stderr) when Xvfb isn't installed or doesn't start.
    pub fn start() -> Option<Xvfb> {
        let mut fds = [0; 2];
        // SAFETY: a plain pipe; the write end is inherited by Xvfb (no CLOEXEC).
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let spawned = Command::new("Xvfb")
            .args(["-displayfd", &fds[1].to_string(), "-nolisten", "tcp", "-screen", "0", "640x480x24"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        // SAFETY: our fds; the read end becomes a File.
        let r = unsafe {
            libc::close(fds[1]);
            std::fs::File::from_raw_fd(fds[0])
        };
        let child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipping: no Xvfb ({e})");
                return None;
            }
        };
        // Xvfb writes "<n>\n" once it accepts connections.
        let mut line = String::new();
        let _ = BufReader::new(r).read_line(&mut line);
        let n = line.trim();
        if n.is_empty() {
            eprintln!("skipping: Xvfb didn't start");
            return None;
        }
        Some(Xvfb { child, display: format!(":{n}") })
    }

    pub fn name(&self) -> CString {
        CString::new(self.display.clone()).unwrap()
    }

    /// A connection that types into this server with XTEST.
    pub fn keyboard(&self) -> Keyboard {
        let (conn, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&self.name())).unwrap();
        let root = conn.setup().roots[screen].root;
        Keyboard { conn, root }
    }
}

pub struct Keyboard {
    conn: x11rb::xcb_ffi::XCBConnection,
    root: u32,
}

impl Keyboard {
    /// Presses (`down`) or releases keycode `code`; returns once the server processed it.
    pub fn key(&self, code: u8, down: bool) {
        self.conn.xtest_fake_input(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, code, 0, self.root, 0, 0, 0).unwrap();
        self.conn.sync().unwrap();
    }
}
