//! Running `quiver-webview`, Quiver's sign-in window (design/STORE-EPIC.md §9.6), for the
//! app and the CLI.
//!
//! [`sign_in`] opens Epic's sign-in page (the URL from `EpicRequest::LoginBegin`) in the
//! helper's window and waits for the code it prints. The caller sends the code on with
//! `EpicRequest::LoginComplete`. The helper's stdout is read here and nowhere else, and
//! the code is only ever inside a [`SignInCode`], which doesn't print.
//!
//! The helper is found at `$QUIVER_WEBVIEW`, then next to the running program (installs
//! put it beside `quiver` and `quiver-app`), then on `PATH`.

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

/// The helper's name.
pub const HELPER: &str = "quiver-webview";
/// Overrides where the helper is.
pub const HELPER_ENV: &str = "QUIVER_WEBVIEW";
/// How long the window stays open by default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The code Epic's sign-in ended with: a credential for a few minutes, so it never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct SignInCode(String);

impl SignInCode {
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for SignInCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignInCode(<redacted>)")
    }
}

/// How the sign-in window ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Code(SignInCode),
    /// The person closed the window.
    Cancelled,
    TimedOut,
    /// No window here: the helper isn't installed, WebKitGTK is missing, or there is no
    /// display. Paste the code instead.
    Unavailable(String),
    Failed(String),
}

/// Where the helper is, if anywhere.
pub fn find_helper() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(HELPER_ENV).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()) {
        return Some(p);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) {
        let p = dir.join(HELPER);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(HELPER)).find(|p| p.is_file())
}

/// Open the sign-in window on `url` and wait for it (at most `timeout`, which the helper
/// enforces itself; this side gives it a little longer).
pub async fn sign_in(url: &str, timeout: Duration) -> Outcome {
    match find_helper() {
        Some(helper) => run(&helper, &["epic-login", "--url", url, "--timeout", &timeout.as_secs().max(1).to_string()], timeout + Duration::from_secs(15)).await,
        None => Outcome::Unavailable(format!("{HELPER} isn't installed")),
    }
}

/// Run the helper with `args` and read its answer, stopping it after `wait`.
pub async fn run(helper: &std::path::Path, args: &[&str], wait: Duration) -> Outcome {
    let mut child = match tokio::process::Command::new(helper)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Outcome::Unavailable(format!("{HELPER} isn't installed")),
        Err(e) => return Outcome::Unavailable(format!("{HELPER} couldn't start: {e}")),
    };
    let (mut stdout, mut stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let read_out = async {
        let mut v = Vec::new();
        let _ = (&mut stdout).take(4096).read_to_end(&mut v).await;
        v
    };
    let read_err = async {
        let mut v = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut v).await;
        v
    };
    let waited = tokio::time::timeout(wait, async {
        let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
        (out, err, status)
    })
    .await;
    let (out, err, status) = match waited {
        Ok(r) => r,
        Err(_) => {
            stop(&mut child).await;
            return Outcome::TimedOut;
        }
    };
    let reason = last_line(&err);
    match status.map(|s| s.code()) {
        Ok(Some(0)) => {
            let code = String::from_utf8_lossy(&out).trim().to_owned();
            if code.is_empty() || code.contains(char::is_whitespace) {
                Outcome::Failed(format!("{HELPER} finished without a code"))
            } else {
                Outcome::Code(SignInCode(code))
            }
        }
        Ok(Some(2)) => Outcome::Cancelled,
        Ok(Some(3)) => Outcome::TimedOut,
        Ok(Some(4)) => Outcome::Unavailable(reason.unwrap_or_else(|| "WebKitGTK isn't available".into())),
        Ok(Some(n)) => Outcome::Failed(reason.unwrap_or_else(|| format!("{HELPER} failed (exit {n})"))),
        Ok(None) => Outcome::Failed(format!("{HELPER} was stopped by a signal")),
        Err(e) => Outcome::Failed(format!("{HELPER}: {e}")),
    }
}

/// SIGTERM first, so the helper deletes its private directory, then kill.
async fn stop(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: a signal to our own child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

/// The helper's last line on stderr, without its `quiver-webview: ` prefix.
fn last_line(err: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(err);
    text.lines().rev().map(str::trim).find(|l| l.starts_with("quiver-webview:")).map(|l| l.trim_start_matches("quiver-webview:").trim().to_owned()).filter(|l| !l.is_empty())
}

// Running stand-in helpers is tested in tests/webview.rs, a test binary of its own: child
// processes exiting (SIGCHLD) would disturb the blocking fake daemons of the unit tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_never_print_and_reasons_are_the_helpers_last_word() {
        let c = SignInCode("0123456789abcdef0123456789abcdef".into());
        assert_eq!(format!("{:?}", Outcome::Code(c.clone())), "Code(SignInCode(<redacted>))");
        assert_eq!(c.expose(), "0123456789abcdef0123456789abcdef");
        assert_eq!(last_line(b"Gtk-WARNING: x\nquiver-webview: first\nquiver-webview: no display\n(more noise)\n").as_deref(), Some("no display"));
        assert_eq!(last_line(b"noise only\n"), None);
    }
}
