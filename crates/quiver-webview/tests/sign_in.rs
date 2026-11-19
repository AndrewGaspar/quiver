//! The sign-in window end to end, against a local server standing in for Epic, in a
//! private Xvfb (never the desktop's display). Skipped, with a note, when Xvfb or
//! WebKitGTK isn't installed.
//!
//! The server's `/login` redirects (HTTP 302, or JavaScript for `/login-js`) to its own
//! `/id/api/redirect`, which serves Epic's JSON; `--redirect-prefix` points the helper at it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const CODE: &str = "0123456789abcdef0123456789abcdef";

/// How many redirect pages say `null` before one has the code.
struct Server {
    base: String,
    nulls: Arc<AtomicUsize>,
    redirects: Arc<AtomicUsize>,
}

fn server() -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let nulls = Arc::new(AtomicUsize::new(0));
    let redirects = Arc::new(AtomicUsize::new(0));
    let (n, r) = (nulls.clone(), redirects.clone());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let (n, r) = (n.clone(), r.clone());
            std::thread::spawn(move || {
                let mut line = String::new();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let reply = |status: &str, kind: &str, extra: &str, body: &str| {
                    format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}\r\n{body}", body.len())
                };
                let resp = if path.starts_with("/login-js") {
                    reply("200 OK", "text/html", "", "<html><body>Signing in…<script>setTimeout(function(){location.href='/id/api/redirect?clientId=test&responseType=code'},100)</script></body></html>")
                } else if path.starts_with("/login") {
                    reply("302 Found", "text/html", "Location: /id/api/redirect?clientId=test&responseType=code\r\n", "")
                } else if path.starts_with("/stay") {
                    reply("200 OK", "text/html", "", "<html><body>Sign in here (never finishes)</body></html>")
                } else if path.starts_with("/id/api/redirect") {
                    r.fetch_add(1, Ordering::SeqCst);
                    let null = n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1)).is_ok();
                    let code = if null { "null".to_owned() } else { format!("\"{CODE}\"") };
                    let body = format!(
                        r#"{{"warning":"Do not share this code with any 3rd party service.","redirectUrl":"https://localhost/launcher/authorized?code=x","authorizationCode":{code},"exchangeCode":null,"sid":null}}"#
                    );
                    reply("200 OK", "application/json;charset=UTF-8", "", &body)
                } else {
                    reply("404 Not Found", "text/plain", "", "")
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    Server { base, nulls, redirects }
}

/// A private X server on a free display number, killed when dropped.
struct Xvfb {
    child: Child,
    display: String,
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

fn xvfb() -> Option<Xvfb> {
    let bin = ["/usr/bin/Xvfb", "/usr/local/bin/Xvfb"].into_iter().find(|p| Path::new(p).exists())?;
    for n in 140..200 {
        if Path::new(&format!("/tmp/.X{n}-lock")).exists() || Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() {
            continue;
        }
        let mut child = Command::new(bin)
            .arg(format!(":{n}"))
            .args(["-nolisten", "tcp", "-screen", "0", "1280x1024x24"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() {
                return Some(Xvfb { child, display: format!(":{n}") });
            }
            if child.try_wait().ok().flatten().is_some() {
                break; // someone else took it
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    None
}

fn webkit_installed() -> bool {
    ["/usr/lib/libwebkit2gtk-4.1.so.0", "/usr/lib64/libwebkit2gtk-4.1.so.0", "/usr/lib/x86_64-linux-gnu/libwebkit2gtk-4.1.so.0"].iter().any(|p| Path::new(p).exists())
}

struct Run {
    display: String,
    runtime: PathBuf,
    shm: PathBuf,
}

impl Run {
    fn helper(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiver-webview"));
        c.arg("epic-login")
            .args(args)
            .env("DISPLAY", &self.display)
            .env_remove("WAYLAND_DISPLAY")
            .env("GDK_BACKEND", "x11")
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("QUIVER_SHM_ROOT", &self.shm)
            .env("WEBKIT_DISABLE_COMPOSITING_MODE", "1")
            .env("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("NO_AT_BRIDGE", "1")
            // Keep off the desktop's session bus (portals, accessibility).
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/quiver-webview-test")
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        eprintln!("[{}] {:?}\n{}", out.status, args, String::from_utf8_lossy(&out.stderr).lines().filter(|l| l.contains("quiver-webview")).collect::<Vec<_>>().join("\n"));
        // Whatever exit: the private directory is gone.
        let left: Vec<_> = std::fs::read_dir(&self.runtime).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("quiver-webview-")).collect();
        assert!(left.is_empty(), "private dir left behind: {left:?}");
        out
    }
}

#[test]
fn sign_in_window() {
    if !webkit_installed() {
        eprintln!("skipping: WebKitGTK (webkit2gtk-4.1) isn't installed");
        return;
    }
    let Some(x) = xvfb() else {
        eprintln!("skipping: no Xvfb");
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let runtime = scratch.path().join("run");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let run = Run { display: x.display.clone(), runtime, shm: scratch.path().join("shm") };
    let srv = server();
    let redirect = format!("{}/id/api/redirect", srv.base);
    let login = format!("{}/login", srv.base);
    let code = |o: &Output| o.status.code();

    // Signed in: the code, and only the code, on stdout; never on stderr.
    let o = run.helper(&["--url", &login, "--redirect-prefix", &redirect, "--timeout", "60"], &[]);
    assert_eq!(code(&o), Some(0));
    assert_eq!(String::from_utf8_lossy(&o.stdout), format!("{CODE}\n"));
    assert!(!String::from_utf8_lossy(&o.stderr).contains(CODE));

    // A JavaScript redirect works the same.
    let o = run.helper(&["--url", &format!("{}/login-js", srv.base), "--redirect-prefix", &redirect, "--timeout", "60"], &[]);
    assert_eq!((code(&o), String::from_utf8_lossy(&o.stdout).trim().to_owned()), (Some(0), CODE.to_owned()));

    // Epic says null first: the sign-in page loads again, and the second visit has the code.
    srv.nulls.store(1, Ordering::SeqCst);
    let before = srv.redirects.load(Ordering::SeqCst);
    let o = run.helper(&["--url", &login, "--redirect-prefix", &redirect, "--timeout", "60"], &[]);
    assert_eq!((code(&o), String::from_utf8_lossy(&o.stdout).trim().to_owned()), (Some(0), CODE.to_owned()));
    assert_eq!(srv.redirects.load(Ordering::SeqCst) - before, 2);
    assert!(String::from_utf8_lossy(&o.stderr).contains("opening the sign-in page again"));

    // Null every time: gives up after a few tries, with nothing on stdout.
    srv.nulls.store(100, Ordering::SeqCst);
    let o = run.helper(&["--url", &login, "--redirect-prefix", &redirect, "--timeout", "60"], &[]);
    assert_eq!(code(&o), Some(1));
    assert!(o.stdout.is_empty());
    srv.nulls.store(0, Ordering::SeqCst);

    // The person closes the window: cancelled.
    let o = run.helper(&["--url", &format!("{}/stay", srv.base), "--redirect-prefix", &redirect, "--close-after-load", "--timeout", "60"], &[]);
    assert_eq!(code(&o), Some(2));
    assert!(o.stdout.is_empty());

    // Nobody signs in: timed out.
    let t = Instant::now();
    let o = run.helper(&["--url", &format!("{}/stay", srv.base), "--redirect-prefix", &redirect, "--timeout", "2"], &[]);
    assert_eq!(code(&o), Some(3));
    assert!(t.elapsed() < Duration::from_secs(30));

    // No WebKit: unavailable, said in one line.
    let o = run.helper(&["--url", &login, "--redirect-prefix", &redirect], &[("QUIVER_WEBVIEW_TEST_LIB", "libquiver-no-such-webkit.so.0")]);
    assert_eq!(code(&o), Some(4));
    assert!(String::from_utf8_lossy(&o.stderr).contains("WebKitGTK isn't available"));

    // No display: unavailable too.
    let o = Command::new(env!("CARGO_BIN_EXE_quiver-webview"))
        .args(["epic-login", "--url", &login, "--redirect-prefix", &redirect])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_RUNTIME_DIR", &run.runtime)
        .env("QUIVER_SHM_ROOT", &run.shm)
        .output()
        .unwrap();
    assert_eq!(code(&o), Some(4), "{}", String::from_utf8_lossy(&o.stderr));
}
