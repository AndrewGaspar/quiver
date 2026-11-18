//! `quiver-webview`: Quiver's sign-in window (design/STORE-EPIC.md §9.6).
//!
//! `quiver-webview epic-login --url <Epic's sign-in URL>` opens Epic's own sign-in page in
//! a window of its own. When the sign-in ends on Epic's redirect page
//! (`https://www.epicgames.com/id/api/redirect…`, a page of JSON), it reads that page's
//! `authorizationCode`, prints it on stdout and exits; the caller (`quiver epic login`, the
//! app) hands it to quiverd. The code is never logged, never on stderr, never in a file.
//!
//! - **No cookies on disk:** the web view's website data is ephemeral (in memory), and the
//!   process's `XDG_CACHE_HOME`/`XDG_DATA_HOME` point into a private 0700 directory that is
//!   deleted on the way out, so nothing WebKit writes outlives the sign-in.
//! - **Separate process:** neither quiverd nor the app links WebKit. This binary doesn't
//!   either: it opens `libwebkit2gtk-4.1.so.0` at run time ([`ffi`]), so WebKitGTK is an
//!   optional dependency and exit code 4 says it's missing.
//! - Exit codes: 0 signed in (the code on stdout), 1 failed, 2 the window was closed,
//!   3 timed out, 4 no WebKitGTK or no display, 64 bad arguments.

mod args;
mod ffi;
mod window;

use std::io::Write;
use std::path::{Path, PathBuf};

use args::{EXIT_CANCELLED, EXIT_FAILED, EXIT_OK, EXIT_TIMED_OUT, EXIT_UNAVAILABLE, EXIT_USAGE, TEST_LIB_ENV};
use window::Outcome;

/// A private directory for whatever WebKit writes, removed when dropped.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create() -> std::io::Result<PrivateDir> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(std::env::temp_dir);
        let template = base.join("quiver-webview-XXXXXX");
        let mut buf = std::ffi::CString::new(template.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?.into_bytes_with_nul();
        // SAFETY: a writable NUL-terminated template; mkdtemp makes the directory 0700.
        let p = unsafe { libc::mkdtemp(buf.as_mut_ptr().cast()) };
        if p.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        buf.pop();
        Ok(PrivateDir(PathBuf::from(String::from_utf8_lossy(&buf).into_owned())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let opts = match args::parse(&argv, std::env::var(TEST_LIB_ENV).ok()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return if argv.iter().any(|a| a == "-h" || a == "--help") { EXIT_OK } else { EXIT_USAGE };
        }
    };
    // If the caller (the app, the terminal) goes away, so does the window, through the
    // SIGTERM handler that cleans up.
    // SAFETY: prctl with a signal number.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    let dir = match PrivateDir::create() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("quiver-webview: can't make a private directory: {e}");
            return EXIT_FAILED;
        }
    };
    // SAFETY: no other thread exists yet (WebKit isn't loaded until below).
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", dir.path().join("cache"));
        std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
    }
    let code = sign_in(&opts);
    drop(dir);
    code
}

fn sign_in(opts: &args::Options) -> i32 {
    let api: &'static ffi::Api = match ffi::Api::load(&opts.lib) {
        Ok(a) => Box::leak(Box::new(a)),
        Err(e) => {
            eprintln!("quiver-webview: WebKitGTK isn't available: {e}");
            return EXIT_UNAVAILABLE;
        }
    };
    match window::run(api, opts) {
        Err(e) => {
            eprintln!("quiver-webview: {e}");
            EXIT_UNAVAILABLE
        }
        Ok(Outcome::Code(c)) => {
            let mut out = std::io::stdout().lock();
            match writeln!(out, "{c}").and_then(|_| out.flush()) {
                Ok(()) => EXIT_OK,
                Err(_) => {
                    eprintln!("quiver-webview: couldn't hand the code back (stdout closed)");
                    EXIT_FAILED
                }
            }
        }
        Ok(Outcome::Cancelled) => {
            eprintln!("quiver-webview: the sign-in window was closed");
            EXIT_CANCELLED
        }
        Ok(Outcome::TimedOut) => {
            eprintln!("quiver-webview: the sign-in window timed out after {} s", opts.timeout);
            EXIT_TIMED_OUT
        }
        Ok(Outcome::Failed(e)) => {
            eprintln!("quiver-webview: {e}");
            EXIT_FAILED
        }
    }
}
