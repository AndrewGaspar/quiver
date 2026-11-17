//! Starting `quiver-sight` (SIGHT.md §5, SIGHT-HUD.md §2.9): the GPUI process that renders the
//! HUD into `hud.image` (or a layer-shell surface) and shows the toast lane.
//!
//! quiverd starts it when a Quiver launch begins (toasts are on by default for Quiver
//! launches; the HUD needs it whenever it is enabled), when a running game's HUD is toggled
//! on, and for dev sessions that ask for it. One instance serves every running session: it
//! attaches itself to each session's HUD, holds a lock so a second copy exits at once, and
//! exits a few seconds after the last session ends. quiverd only makes sure one is running.
//!
//! The binary is `quiver-sight` next to quiverd's own executable, else on `PATH`;
//! `QUIVER_SIGHT_BIN` overrides it and `QUIVER_SIGHT_SPAWN=0` turns starting it off. Its
//! output goes to `sight.log` in Quiver's data dir.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

pub const BIN: &str = "quiver-sight";

#[derive(Default)]
pub struct Spawner {
    child: Mutex<Option<Child>>,
}

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
        if std::env::var_os("RUST_LOG").is_none() {
            cmd.env("RUST_LOG", "quiver_sight=info");
        }
        match cmd.spawn() {
            Ok(c) => {
                tracing::info!(pid = c.id(), "started {} ({why})", bin.display());
                *slot = Some(c);
            }
            Err(e) => {
                tracing::warn!("starting {}: {e} ({why})", bin.display());
                *slot = None;
            }
        }
    }

    /// Reaps an exited child (called from the publish loop, so no zombie lingers).
    pub fn reap(&self) {
        let _ = self.running();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
