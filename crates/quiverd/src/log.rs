//! quiverd's log: `<data>/quiverd.log` (`quiver_api::client::daemon_log`), written by quiverd
//! itself however it was started (the unit, a transient service, a scope, a plain child), so
//! the app's log view, `grep … quiverd.log` and every diagnosis keep reading one file. Lines
//! also go to stdout (the journal under the unit; a terminal), except when stdout already is
//! that file (a client from before this redirected the daemon's output into it), so no line
//! lands twice. Panics are appended to the file too.
//!
//! The file is opened for appending and never rotated (it never was).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The log file.
pub(crate) fn path() -> PathBuf {
    quiver_api::client::daemon_log()
}

fn open(p: &Path) -> Option<File> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    OpenOptions::new().create(true).append(true).open(p).ok()
}

/// Whether descriptor `fd` is the file at `p` (same device and inode).
fn fd_is(fd: i32, p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::metadata(p) else { return false };
    // SAFETY: fstat into a zeroed stat of the right type.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return false;
    }
    st.st_dev == m.dev() && st.st_ino == m.ino()
}

/// Append panics to the log file (after the default hook, which prints to stderr), unless
/// stderr is that file already. Install first thing in `main`.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let p = path();
        if fd_is(2, &p) {
            return;
        }
        if let Some(mut f) = open(&p) {
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ");
            let bt = std::backtrace::Backtrace::capture();
            let bt = if bt.status() == std::backtrace::BacktraceStatus::Captured { format!("\n{bt}") } else { String::new() };
            let _ = writeln!(f, "{now} PANIC quiverd (pid {}): {info}{bt}", std::process::id());
        }
    }));
}

/// The tracing subscriber: the filter from `RUST_LOG` (default `default_filter`), stdout,
/// and the log file.
pub(crate) fn init(default_filter: &str) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into());
    let p = path();
    let file = open(&p);
    let stdout_is_file = file.is_some() && fd_is(1, &p);
    let stdout = (!stdout_is_file).then(|| tracing_subscriber::fmt::layer().with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())));
    let to_file = file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(Mutex::new(f)));
    tracing_subscriber::registry().with(filter).with(stdout).with(to_file).init();
    if !p.parent().is_some_and(Path::is_dir) {
        tracing::warn!("can't write {}", p.display());
    }
}

#[cfg(test)]
mod tests {
    /// Run in a child test process (the parent below sets `QUIVER_LOG_PANIC_CHILD` and a
    /// scratch data folder): installs the hook and panics.
    #[test]
    fn panic_child() {
        if std::env::var_os("QUIVER_LOG_PANIC_CHILD").is_none() {
            return;
        }
        super::install_panic_hook();
        panic!("the panic the log should keep");
    }

    #[test]
    fn panics_land_in_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "log::tests::panic_child", "--nocapture", "--test-threads=1"])
            .env("QUIVER_LOG_PANIC_CHILD", "1")
            .env("XDG_DATA_HOME", dir.path())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!st.success(), "the child panicked");
        let log = std::fs::read_to_string(dir.path().join("quiver/quiverd.log")).unwrap_or_default();
        assert!(log.contains("PANIC quiverd") && log.contains("the panic the log should keep"), "{log}");
    }
}
