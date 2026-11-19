//! A private Xvfb for the key tests, and XTEST key presses into it. Never the desktop's X
//! server: the display is an explicit high number (400 and up) whose socket and lock files
//! don't exist. Not `-displayfd` alone: that starts the search at :0, and in a sandbox with its
//! own network namespace the abstract socket of the desktop's display looks free, so Xvfb
//! takes :0 and unlinks the desktop's `/tmp/.X11-unix/X0` (then exits, its lock being
//! held), leaving the desktop's Xwayland without its socket file.

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
    /// SIGTERM first, so Xvfb removes its socket and lock file (a SIGKILL leaves both behind
    /// in /tmp, and they hold display numbers until someone clears them); SIGKILL after 2 s.
    fn drop(&mut self) {
        // SAFETY: our own child's pid.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Xvfb {
    /// `None` (and a note on stderr) when Xvfb isn't installed or doesn't start.
    pub fn start() -> Option<Xvfb> {
        Xvfb::start_size("640x480x24")
    }

    /// With a screen of `size` (`<w>x<h>x<depth>`).
    pub fn start_size(size: &str) -> Option<Xvfb> {
        let start = 400 + std::process::id() % 400;
        let free = |n: u32| {
            !std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() && !std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists()
        };
        let Some(n) = (start..start + 400).find(|&n| free(n)) else {
            eprintln!("skipping: no free X display number");
            return None;
        };
        let mut fds = [0; 2];
        // SAFETY: a plain pipe; the write end is inherited by Xvfb (no CLOEXEC).
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // With an explicit display, -displayfd only reports it once Xvfb accepts connections.
        let spawned = Command::new("Xvfb")
            .args([format!(":{n}").as_str(), "-displayfd", &fds[1].to_string(), "-nolisten", "tcp", "-screen", "0", size])
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
