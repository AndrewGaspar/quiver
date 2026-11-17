//! The display producer (`display.*`) and focus tracking, from Hyprland's IPC socket.
//!
//! Read-only queries only (`j/monitors`, `j/activewindow`): quiverd never dispatches or
//! sets keywords here. Polled at 1 Hz while a session runs (Hyprland's event socket would
//! make the display line react faster; noted as a follow-up).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Hyprland's request socket, if a Hyprland session is reachable.
pub fn socket() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty() && !s.contains('/'))?;
    let mut bases = Vec::new();
    if let Some(r) = std::env::var_os("XDG_RUNTIME_DIR") {
        bases.push(PathBuf::from(r));
    }
    // SAFETY: getuid has no preconditions.
    bases.push(PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    bases.into_iter().map(|b| b.join("hypr").join(&sig).join(".socket.sock")).find(|p| p.exists())
}

/// One read-only query (`j/monitors`), answered as JSON.
pub fn query(sock: &Path, cmd: &str) -> std::io::Result<String> {
    debug_assert!(matches!(cmd, "j/monitors" | "j/activewindow"), "read-only queries only");
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    s.set_write_timeout(Some(Duration::from_millis(500)))?;
    s.write_all(cmd.as_bytes())?;
    let mut out = String::new();
    s.take(1 << 20).read_to_string(&mut out)?;
    Ok(out)
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub refresh_rate: f64,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub vrr: bool,
    #[serde(default)]
    pub actively_tearing: bool,
    /// `"0"` when nothing is scanned out directly.
    #[serde(default)]
    pub direct_scanout_to: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub direct_scanout_blocked_by: Vec<String>,
    /// `null` (not `[]`) while a window is solitary.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub solitary_blocked_by: Vec<String>,
}

/// Hyprland reports an empty blocker list as `null`; one such field must not fail the
/// whole `j/monitors` parse (that blanked the display line while the game was solitary).
fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ActiveWindow {
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub monitor: i64,
    #[serde(default)]
    pub fullscreen: i64,
    #[serde(default)]
    pub class: String,
}

/// What a poll found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayState {
    pub monitors: Vec<Monitor>,
    pub active: Option<ActiveWindow>,
}

impl DisplayState {
    pub fn poll(sock: &Path) -> Option<DisplayState> {
        let monitors: Vec<Monitor> = serde_json::from_str(&query(sock, "j/monitors").ok()?).ok()?;
        // `activewindow` is `{}` with nothing focused.
        let active = query(sock, "j/activewindow").ok().and_then(|s| serde_json::from_str::<ActiveWindow>(&s).ok()).filter(|a| a.pid > 0);
        Some(DisplayState { monitors, active })
    }

    /// The monitor of the focused window, else the focused monitor.
    pub fn game_monitor(&self, game_focused: bool) -> Option<&Monitor> {
        let by_window = self.active.as_ref().filter(|_| game_focused).and_then(|a| self.monitors.iter().find(|m| m.id == a.monitor));
        by_window.or_else(|| self.monitors.iter().find(|m| m.focused)).or_else(|| self.monitors.first())
    }
}

/// `display.*` values for one monitor: (text and state metrics).
pub struct DisplayValues {
    pub mode: String,
    pub refresh: f64,
    pub vrr: &'static str,
    pub tearing: &'static str,
    /// `direct` (scan-out) or `composited`.
    pub path: String,
    /// What keeps Hyprland from scanning out, in words (`none`); for the app, too long for the HUD.
    pub blocked: String,
}

pub fn values(m: &Monitor) -> DisplayValues {
    let refresh = (m.refresh_rate * 100.0).round() / 100.0;
    let hz = if refresh.fract() == 0.0 { format!("{refresh:.0}") } else { format!("{refresh}") };
    let scanout = !m.direct_scanout_to.is_empty() && m.direct_scanout_to != "0";
    // Why Hyprland isn't scanning out, in words ("CANDIDATE", no fullscreen window, is
    // implied by "composited").
    let blockers: Vec<String> = m.direct_scanout_blocked_by.iter().filter(|b| b.as_str() != "CANDIDATE").map(|b| blocker_words(b)).collect();
    let path = if scanout { "direct" } else { "composited" }.to_owned();
    let blocked = if scanout || blockers.is_empty() { "none".to_owned() } else { blockers.join(", ") };
    DisplayValues {
        mode: format!("{}×{}@{hz}", m.width, m.height),
        refresh,
        vrr: if m.vrr { "on" } else { "off" },
        tearing: if m.actively_tearing { "on" } else { "off" },
        path,
        blocked,
    }
}

/// One of Hyprland's `directScanoutBlockedBy` reasons, as the HUD says it.
fn blocker_words(b: &str) -> String {
    match b {
        "USER" => "scan-out off in config".into(),
        "RECORD" => "screen share".into(),
        "MIRROR" => "mirrored".into(),
        "SW" => "software cursor".into(),
        "WINDOWED" => "not fullscreen".into(),
        "CONTENT" => "not a game window".into(),
        "TRANSFORM" => "scaled or rotated".into(),
        "DMA" => "buffer can't scan out".into(),
        "TEARING" => "tearing".into(),
        "FAILED" => "scan-out failed".into(),
        "CM" => "colour management".into(),
        other => other.to_lowercase().replace('_', " "),
    }
}

/// Whether `pid` belongs to a game session: its environment names the session dir or
/// session id, or the game's Wine prefix. Reads `/proc/<pid>/environ` (same user).
pub fn pid_in_session(proc_root: &Path, pid: i64, markers: &[String]) -> bool {
    if pid <= 0 || markers.is_empty() {
        return false;
    }
    let Ok(env) = std::fs::read(proc_root.join(pid.to_string()).join("environ")) else { return false };
    env.split(|&b| b == 0).any(|kv| markers.iter().any(|m| kv == m.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine's `hyprctl monitors -j`, trimmed.
    const MONITORS: &str = r#"[{"id": 1, "name": "eDP-2", "width": 2560, "height": 1600, "refreshRate": 165.00000,
        "focused": true, "vrr": false, "solitary": "0", "solitaryBlockedBy": ["WINDOWED","CANDIDATE"],
        "activelyTearing": false, "tearingBlockedBy": ["NOT_TORN","USER","CANDIDATE","HW_CURSOR"],
        "directScanoutTo": "0", "directScanoutBlockedBy": ["USER","CANDIDATE"], "disabled": false}]"#;

    #[test]
    fn monitor_values() {
        let m: Vec<Monitor> = serde_json::from_str(MONITORS).unwrap();
        let v = values(&m[0]);
        assert_eq!(v.mode, "2560×1600@165");
        assert_eq!((v.vrr, v.tearing), ("off", "off"));
        assert_eq!((v.path.as_str(), v.blocked.as_str()), ("composited", "scan-out off in config"));
        let mut so = m[0].clone();
        so.direct_scanout_to = "0x55d".into();
        so.vrr = true;
        so.refresh_rate = 59.951;
        let v = values(&so);
        assert_eq!((v.path.as_str(), v.vrr, v.blocked.as_str()), ("direct", "on", "none"));
        assert_eq!(v.mode, "2560×1600@59.95");
        let st = DisplayState { monitors: m, active: Some(ActiveWindow { pid: 1, monitor: 1, ..Default::default() }) };
        assert_eq!(st.game_monitor(true).unwrap().name, "eDP-2");
        let empty: ActiveWindow = serde_json::from_str("{}").unwrap();
        // A solitary fullscreen game: Hyprland sends `null` blocker lists.
        let solitary = MONITORS.replace(r#"["WINDOWED","CANDIDATE"]"#, "null").replace(r#""directScanoutBlockedBy": ["USER","CANDIDATE"]"#, r#""directScanoutBlockedBy": null"#);
        let m: Vec<Monitor> = serde_json::from_str(&solitary).unwrap();
        assert!(m[0].solitary_blocked_by.is_empty() && m[0].direct_scanout_blocked_by.is_empty());
        assert_eq!((values(&m[0]).path.as_str(), values(&m[0]).blocked.as_str()), ("composited", "none"));
        assert_eq!(empty.pid, 0);
    }

    #[test]
    fn session_membership_by_environment() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("42")).unwrap();
        std::fs::write(t.path().join("42/environ"), b"HOME=/h\0QUIVER_SESSION=s-abc\0WINEPREFIX=/p\0").unwrap();
        assert!(pid_in_session(t.path(), 42, &["QUIVER_SESSION=s-abc".into()]));
        assert!(pid_in_session(t.path(), 42, &["WINEPREFIX=/p".into()]));
        assert!(!pid_in_session(t.path(), 42, &["QUIVER_SESSION=s-ab".into()]));
        assert!(!pid_in_session(t.path(), 43, &["QUIVER_SESSION=s-abc".into()]));
    }

    /// Against the live session, read-only (skipped outside Hyprland).
    #[test]
    fn live_query_is_read_only_and_parses() {
        let Some(sock) = socket() else { return };
        let Some(st) = DisplayState::poll(&sock) else { return };
        assert!(!st.monitors.is_empty());
    }
}
