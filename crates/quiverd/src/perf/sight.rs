//! Starting `quiver-sight` (SIGHT.md §5, SIGHT-HUD.md §2.9): the GPUI process that renders the
//! HUD into `hud.image` (or a layer-shell surface), shows the toast lane and the panel.
//!
//! quiverd starts it when a Quiver launch begins (toasts are on by default for Quiver
//! launches; the HUD needs it whenever it is enabled), on a Sight panel command (`panel.rs`),
//! when a running game's HUD is toggled on, and for dev sessions that ask for it. One instance serves every running session: it
//! attaches itself to each session's HUD, holds a lock so a second copy exits at once, and
//! exits a few seconds after the last session ends. quiverd only makes sure one is running.
//!
//! The binary is `quiver-sight` next to quiverd's own executable, else on `PATH`;
//! `QUIVER_SIGHT_BIN` overrides it and `QUIVER_SIGHT_SPAWN=0` turns starting it off. Its
//! output goes to `sight.log` in Quiver's data dir.
//!
//! **Hot swap.** When a new build replaces the binary (another inode at the same path),
//! quiverd restarts the child it started onto it ([`Spawner::refresh`]), unless the panel
//! is open. A running game's layer keeps compositing the last `hud.image` meanwhile and
//! the new process attaches and publishes within a second, so HUD rendering changes reach
//! running games without a relaunch. The model (rows, labels) still comes from quiverd and
//! the layer lives in the game; those need their own restarts.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const BIN: &str = "quiver-sight";

#[derive(Default)]
pub struct Spawner {
    child: Mutex<Option<Child>>,
    /// The binary the child was started from, as it was then.
    started: Mutex<Option<(PathBuf, FileId)>>,
    /// When `refresh` last looked at the binary.
    checked: Mutex<Option<Instant>>,
}

/// A file's identity: a new build is another inode (installs rename into place), or the
/// same inode rewritten (a later mtime or size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
}

impl FileId {
    pub fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        m.is_file().then(|| Self { dev: m.dev(), ino: m.ino(), size: m.size(), mtime: (m.mtime(), m.mtime_nsec()) })
    }
}

/// How often `refresh` stats the binary.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Where the binary is: `QUIVER_SIGHT_BIN`, next to quiverd, else `PATH`.
pub fn binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("QUIVER_SIGHT_BIN").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe().ok().as_deref().and_then(Path::parent) {
        let p = dir.join(BIN);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join(BIN)).find(|p| p.is_file()))
}

/// `QUIVER_SIGHT_SPAWN=0`, and always in unit tests (they toggle HUDs of fake sessions and
/// must never put anything on the desktop).
pub fn disabled() -> bool {
    cfg!(test) || std::env::var("QUIVER_SIGHT_SPAWN").is_ok_and(|v| v == "0")
}

impl Spawner {
    /// The child quiverd started is still running.
    pub fn running(&self) -> bool {
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(_) => {
                *slot = None;
                false
            }
            None => false,
        }
    }

    /// Starts `quiver-sight` unless the one quiverd started is still running (a copy someone
    /// else started exits by itself: it holds a lock). `why` goes to the log. Never fails
    /// the caller: a missing binary is a log line.
    pub fn ensure(&self, data_dir: &Path, why: &str) {
        if disabled() {
            return;
        }
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = slot.as_mut()
            && matches!(c.try_wait(), Ok(None))
        {
            return;
        }
        let Some(bin) = binary() else {
            tracing::warn!("{BIN} not found next to quiverd or on PATH: no HUD renderer and no toasts ({why})");
            *slot = None;
            return;
        };
        let log = std::fs::create_dir_all(data_dir).and_then(|_| std::fs::File::create(data_dir.join("sight.log")));
        let mut cmd = Command::new(&bin);
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        match log {
            Ok(f) => cmd.stderr(f),
            Err(_) => cmd.stderr(Stdio::null()),
        };
        let id = FileId::of(&bin);
        match cmd.spawn() {
            Ok(c) => {
                tracing::info!(pid = c.id(), "started {} ({why})", bin.display());
                *slot = Some(c);
                *self.started.lock().unwrap_or_else(|e| e.into_inner()) = id.map(|id| (bin.clone(), id));
            }
            Err(e) => {
                tracing::warn!("starting {}: {e} ({why})", bin.display());
                *slot = None;
            }
        }
    }

    /// The running child's binary was replaced by a new build since it started.
    pub fn stale(&self) -> bool {
        let started = self.started.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match started {
            Some((path, was)) => FileId::of(&path).is_some_and(|now| now != was),
            None => false,
        }
    }

    /// Called from the publish loop: reaps an exited child (no zombie lingers) and, at most
    /// once a second, says whether the running child should move onto a new build of the
    /// binary ([`Self::restart`], off the async loop: it waits for the old process).
    /// `hold`: not now (the panel is open); it's asked again on a later tick.
    pub fn refresh_due(&self, hold: bool) -> bool {
        if !self.running() {
            return false;
        }
        {
            let mut checked = self.checked.lock().unwrap_or_else(|e| e.into_inner());
            if checked.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
                return false;
            }
            *checked = Some(Instant::now());
        }
        !hold && self.stale()
    }

    /// Restarts the child onto the binary as it is now.
    pub fn restart(&self, data_dir: &Path) {
        self.stop();
        self.ensure(data_dir, "a new build of quiver-sight");
    }

    /// Ends the child: SIGTERM (it tears its surfaces down cleanly), then SIGKILL after 2 s.
    fn stop(&self) {
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut c) = slot.take() else { return };
        let pid = c.id();
        tracing::info!(pid, "restarting {BIN} onto a new build");
        // SAFETY: plain kill(2) on our own child's pid, which we haven't reaped yet.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(c.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuilt_binary_is_another_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(BIN);
        std::fs::write(&p, b"old").unwrap();
        let s = Spawner::default();
        *s.started.lock().unwrap() = Some((p.clone(), FileId::of(&p).unwrap()));
        assert!(!s.stale());
        // Installs write the new build beside it and rename it into place.
        let new = tmp.path().join("new");
        std::fs::write(&new, b"new build").unwrap();
        std::fs::rename(&new, &p).unwrap();
        assert!(s.stale());
        // Without a running child, nothing is due (and nothing spawns in tests).
        assert!(!s.refresh_due(false));
        assert!(!s.running());
    }

    #[test]
    fn never_spawns_in_tests() {
        assert!(disabled());
        let s = Spawner::default();
        let tmp = tempfile::tempdir().unwrap();
        s.ensure(tmp.path(), "test");
        assert!(!s.running());
        assert!(!tmp.path().join("sight.log").exists());
    }
}
