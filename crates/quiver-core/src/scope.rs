//! A launch's processes in scopes of their own, and the session's display for them
//! (design/SERVICE.md).
//!
//! - **Scopes.** The bridge and the game each start as `systemd-run --user --scope
//!   --slice=quiver.slice -- …` (it execs in place: same pid, environment and descriptors,
//!   and the overlay namespace the command joined in `pre_exec` is kept). So a game is never
//!   in quiverd's cgroup, nor a terminal's: stopping or restarting the daemon, closing a
//!   terminal or systemd-oomd acting on `app.slice` leaves it alone. Only for the person's
//!   own runtime folder (a scratch daemon's games stay its children), only when a probe
//!   scope worked once, and off with `QUIVER_GAME_SCOPE=0`.
//! - **Display.** quiverd started at boot (lingering) has no `WAYLAND_DISPLAY`; under the
//!   unit, each launch takes the session variables from the user manager as they are now
//!   ([`session_env`]), so a game started after someone signs in finds the screen.

use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use quiver_api::service::{self, SESSION_VARS, Systemd};

static SCOPES: OnceLock<bool> = OnceLock::new();

/// Whether launches get scopes (probed once per process).
pub fn available() -> bool {
    *SCOPES.get_or_init(probe)
}

fn probe() -> bool {
    if std::env::var("QUIVER_GAME_SCOPE").is_ok_and(|v| v == "0") {
        return false;
    }
    let real = service::real_runtime_dir();
    if std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|r| std::path::Path::new(&r) != real) {
        return false;
    }
    let sd = Systemd::from_env();
    if !sd.can_scope() {
        return false;
    }
    let Some(a) = sd.scope_argv(None, None) else { return false };
    let ok = Command::new(&a[0]).args(&a[1..]).arg("true").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if ok {
        tracing::info!("launches run in scopes of their own under {}", service::SLICE);
    } else {
        tracing::warn!("systemd-run couldn't make a scope; launches stay quiverd's children");
    }
    ok
}

/// `name` as part of a unit name: letters, digits, `_` and `-`, at most 40.
fn unit_part(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(40).collect();
    if s.is_empty() { "game".into() } else { s }
}

/// A fresh scope name for `role` (`bridge`, `game`) of `name` (a game ref): unique, since a
/// previous launch's scope may linger while its wineserver exits.
pub fn unit_name(role: &str, name: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tag = (nanos ^ std::process::id().rotate_left(16)).wrapping_add(N.fetch_add(1, Ordering::Relaxed)) & 0xff_ffff;
    format!("quiver-{role}-{}-{tag:06x}.scope", unit_part(name))
}

/// `wrappers` with the scope's `systemd-run` in front when launches get scopes.
pub fn wrap(role: &str, name: &str, wrappers: Vec<String>) -> Vec<String> {
    if !available() {
        return wrappers;
    }
    match Systemd::from_env().scope_argv(Some(&unit_name(role, name)), None) {
        Some(mut a) => {
            a.extend(wrappers);
            a
        }
        None => wrappers,
    }
}

static AS_UNIT: OnceLock<bool> = OnceLock::new();
#[allow(clippy::type_complexity)]
static SESSION: Mutex<Option<(Instant, Vec<(String, String)>)>> = Mutex::new(None);

/// Under the unit: the session's display variables ([`SESSION_VARS`]) as the user manager
/// has them now (looked up at most every few seconds). Empty otherwise, where the
/// process's own environment is the caller's.
pub fn session_env() -> Vec<(String, String)> {
    if !*AS_UNIT.get_or_init(|| service::current_unit().is_some_and(|u| u == service::unit_name())) {
        return Vec::new();
    }
    let mut cache = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, v)) = cache.as_ref()
        && at.elapsed() < Duration::from_secs(5)
    {
        return v.clone();
    }
    let v: Vec<(String, String)> = Systemd::from_env().manager_environment().unwrap_or_default().into_iter().filter(|(k, _)| SESSION_VARS.contains(&k.as_str())).collect();
    *cache = Some((Instant::now(), v.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let a = unit_name("game", "epic:Fortnite/abc");
        assert!(a.starts_with("quiver-game-epic_Fortnite_abc-") && a.ends_with(".scope"), "{a}");
        assert_ne!(a, unit_name("game", "epic:Fortnite/abc"), "fresh each time");
        assert!(unit_name("bridge", "").starts_with("quiver-bridge-game-"));
        assert!(unit_name("game", &"x".repeat(200)).len() < 80);
    }
}
