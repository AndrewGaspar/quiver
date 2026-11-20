//! What the panel's Session and Achievements sections ask quiverd for beyond `Sessions`,
//! `GameDetail` and the mods state (design/SIGHT.md §3.3, §3.7): the session's Proton build
//! and box art, what happened while the game ran, the achievements with their report state,
//! achievement icons on this machine, and screenshots of the game's frames.
//!
//! Everything here is read-only towards the stores: no request sends an achievement, play
//! time or a save anywhere.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::achievements::ReportState;
use crate::art::ArtRef;
use crate::{ActivityEntry, GameRef, StoreId};

/// `SightRequest::Session`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    /// The Proton build the game runs on (`GE-Proton10-25`), as its launch recorded it or its
    /// processes say; `None`: not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proton: Option<String>,
    /// Box art (portrait), best first. The app's art cache may hold any of them; nothing is
    /// downloaded for this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub art: Vec<ArtRef>,
    /// The first of them the art cache (or Steam's library cache) has as a file, and whether
    /// it's the app's box-art crop (240:280) rather than the original image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub art_tile: bool,
    /// What happened while the game ran, oldest first (at most [`MAX_EVENTS`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SessionEvent>,
    /// Why a screenshot can't be taken now (`None`: it can).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_off: Option<String>,
}

/// The most events a session reports (the newest ones).
pub const MAX_EVENTS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    Achievement,
    /// A download or update finished.
    Download,
    /// A mod, a mod version or a profile changed.
    Mods,
    /// Options, Tweaks or HUD settings changed (by someone other than the panel).
    Options,
    Saves,
    Screenshot,
    /// Anything else an agent did.
    Agent,
    #[default]
    Other,
}

/// One thing that happened during the session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub at: DateTime<Utc>,
    pub kind: SessionEventKind,
    /// A line for people ("Download finished: Far Cry 5").
    pub text: String,
    /// Who did it, when not the person (`agent:claude` → `claude`, `mod:aco-dlss`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// `agent:claude` → `claude`, `mod:x` → `mod x`; people (`gui`, `cli`, `sight`) and Quiver
/// itself → `None`.
pub fn who(source: &str) -> Option<String> {
    if crate::mods::is_human_source(source) || matches!(source, "quiver" | "system" | "quiverd" | "steam" | "") {
        return None;
    }
    Some(match source.split_once(':') {
        Some(("agent", name)) => name.to_owned(),
        Some((kind, name)) => format!("{kind} {name}"),
        None => source.to_owned(),
    })
}

impl SessionEvent {
    /// An activity log entry as a session event, or `None` for what the panel doesn't show:
    /// the launch itself, the panel's own changes, lab launch material, holds and restarts,
    /// queue housekeeping.
    pub fn from_activity(e: &ActivityEntry) -> Option<SessionEvent> {
        let a = e.action.trim();
        if a.is_empty() || e.source.starts_with("lab:") || (e.source == "sight" && !a.starts_with("screenshot")) {
            return None;
        }
        let by = who(&e.source);
        let skip = ["play ", "playing ", "issued ", "quiverd ", "hold ", "released hold", "queued ", "paused ", "resumed ", "removed ", "download settings", "cancelled job", "asked "];
        if skip.iter().any(|p| a.starts_with(p)) {
            return None;
        }
        let (kind, text) = if a.starts_with("screenshot") {
            (SessionEventKind::Screenshot, "Screenshot saved".to_owned())
        } else if let Some(rest) = a.strip_prefix("achievement unlocked: ") {
            (SessionEventKind::Achievement, format!("Achievement unlocked: {rest}"))
        } else if let Some(rest) = a.strip_prefix("downloaded ").filter(|r| !r.contains("save file")) {
            // "downloaded Far Cry 5 (1.2.3) to /path": the title.
            let title = rest.split(" (").next().unwrap_or(rest).split(" to /").next().unwrap_or(rest);
            (SessionEventKind::Download, format!("Download finished: {title}"))
        } else if a.starts_with("options r") || a.starts_with("tweaks r") || a.starts_with("settings r") || a.starts_with("HUD settings") || a.starts_with("restored settings") {
            (SessionEventKind::Options, capitalized(a))
        } else if a.contains("mod ") || a.starts_with("mods ") || a.starts_with("added version") || a.starts_with("installed mod") {
            (SessionEventKind::Mods, capitalized(a))
        } else if a.contains("save") {
            (SessionEventKind::Saves, capitalized(a))
        } else if by.is_some() {
            (SessionEventKind::Agent, capitalized(a))
        } else {
            return None;
        };
        // Long lines (an options change lists every value) stop at a sentence's worth.
        let text = if text.chars().count() > 140 { format!("{}…", text.chars().take(139).collect::<String>().trim_end()) } else { text };
        Some(SessionEvent { at: e.at, kind, text, by })
    }
}

/// The events of a session from the activity log (any order in), oldest first: entries
/// since `started` about `game` or no game in particular, and finished downloads of any game.
pub fn session_events(log: &[ActivityEntry], game: GameRef, started: DateTime<Utc>) -> Vec<SessionEvent> {
    let mut out: Vec<SessionEvent> = log
        .iter()
        .filter(|e| e.at >= started)
        .filter_map(|e| SessionEvent::from_activity(e).filter(|ev| ev.kind == SessionEventKind::Download || e.game.is_none_or(|g| g == game)))
        .collect();
    out.sort_by_key(|e| e.at);
    let n = out.len();
    out.drain(..n.saturating_sub(MAX_EVENTS));
    out
}

/// `SightRequest::Achievements`: the game's achievements for the panel, whichever store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionAchievements {
    pub game: Option<GameRef>,
    /// `None`: the store has nothing for the game (or Quiver hasn't read it yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievements: Option<crate::stores::Achievements>,
    /// Where the unlocks go.
    #[serde(default)]
    pub reporting: Reporting,
    /// Ubisoft: the report journal's state per achievement id (unlocks a game earned through
    /// Quiver; others were already on Ubisoft's side).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reports: BTreeMap<String, ReportState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Who records a game's unlocks with its store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reporting {
    pub store: Option<StoreId>,
    /// Ubisoft: the game's `achievements` setting (`on`, `off`, `dry-run`). Epic: `by_game`
    /// (the game reports through Epic Online Services). Steam: `client` (Steam itself).
    #[serde(default)]
    pub mode: String,
}

/// The card's title and line: "Reported to Ubisoft" / "All 31 synced".
pub fn reporting_words(r: &Reporting, reports: &BTreeMap<String, ReportState>, unlocked: usize) -> (String, String) {
    let count = |s: &[ReportState]| reports.values().filter(|x| s.contains(x)).count();
    match r.store {
        Some(StoreId::Ubisoft) => {
            let title = "Reported to Ubisoft".to_owned();
            let line = match r.mode.as_str() {
                "off" => "Reporting is off for this game: unlocks stay on this PC".into(),
                "dry-run" => "Dry run: unlocks are logged, not sent".into(),
                _ => {
                    let (pending, failed) = (count(&[ReportState::Pending]), count(&[ReportState::Failed]));
                    match (pending, failed) {
                        (0, 0) if unlocked == 0 => "Nothing unlocked yet".into(),
                        (0, 0) => format!("All {unlocked} synced"),
                        (p, 0) => format!("{p} waiting to send · {} synced", unlocked.saturating_sub(p)),
                        (p, f) => format!("{f} failed{} · Quiver retries them", if p > 0 { format!(", {p} waiting") } else { String::new() }),
                    }
                }
            };
            (title, line)
        }
        Some(StoreId::Epic) => ("Reported to Epic".into(), "The game reports its unlocks through Epic Online Services".into()),
        Some(StoreId::Steam) => ("Recorded by Steam".into(), "Steam records unlocks as they happen".into()),
        _ => ("Unlocks".into(), "Kept by the game's store".into()),
    }
}

/// `SightRequest::Screenshot`: a capture of the game's own frame (before the HUD and the
/// panel), saved as a PNG.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Screenshot {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// An HDR swapchain: a 16-bit PNG of the PQ (BT.2020) values.
    #[serde(default)]
    pub hdr: bool,
}

/// The folder a game's screenshots go to: `<pictures>/Quiver/<game name>`.
pub fn captures_dir(pictures: &std::path::Path, game_name: &str) -> PathBuf {
    let clean: String = game_name.chars().map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '\'' | '&' | '!' | ',') { c } else { ' ' }).collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean = clean.trim_matches(|c| c == '.' || c == ' ').to_owned();
    pictures.join("Quiver").join(if clean.is_empty() { "Game".to_owned() } else { clean })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: &str, source: &str, action: &str, game: Option<GameRef>) -> ActivityEntry {
        ActivityEntry { at: at.parse().unwrap(), source: source.into(), action: action.into(), game }
    }

    #[test]
    fn activity_becomes_session_events() {
        let g = GameRef::ubisoft(3539);
        let other = GameRef::ubisoft(5059);
        let log = vec![
            entry("2026-11-19T19:59:00Z", "gui", "downloaded Far Cry 5 (1.0) to /x", None),
            entry("2026-11-19T20:00:00Z", "gui", "play Assassin's Creed Origins (settings r3)", Some(g)),
            entry("2026-11-19T20:15:00Z", "agent:aco-dlss", "added version 0.9.4 (abc) of mod 'DLSS' to 3539; current", Some(g)),
            entry("2026-11-19T20:20:00Z", "sight", "options r15 for AC Origins: fg.enabled on", Some(g)),
            entry("2026-11-19T20:41:00Z", "gui", "downloaded Far Cry 5 (1.2.3) to /games/fc5", Some(other)),
            entry("2026-11-19T20:41:30Z", "agent:claude", "options r2 for Far Cry 5: x", Some(other)),
            entry("2026-11-19T20:42:00Z", "lab:x", "issued lab launch material for X", Some(g)),
            entry("2026-11-19T20:43:00Z", "agent:claude", "download settings: {}", None),
            entry("2026-11-19T20:50:00Z", "agent:claude", "options r16 for AC Origins: DLSS preset K", Some(g)),
            entry("2026-11-19T21:02:00Z", "quiver", "achievement unlocked: Hidden Ones", Some(g)),
            entry("2026-11-19T21:03:00Z", "quiver", "downloaded 2 newer Epic cloud save file(s) of X in the background", Some(g)),
        ];
        let started = "2026-11-19T20:00:00Z".parse().unwrap();
        let ev = session_events(&log, g, started);
        let texts: Vec<&str> = ev.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Added version 0.9.4 (abc) of mod 'DLSS' to 3539; current",
                "Download finished: Far Cry 5",
                "Options r16 for AC Origins: DLSS preset K",
                "Achievement unlocked: Hidden Ones",
                "Downloaded 2 newer Epic cloud save file(s) of X in the background",
            ],
            "before the session, the launch, the panel's own change, another game's options, lab material and queue housekeeping are left out; any game's finished download is in"
        );
        assert_eq!(ev[0].by.as_deref(), Some("aco-dlss"));
        assert_eq!(ev[0].kind, SessionEventKind::Mods);
        assert_eq!((ev[1].kind, ev[1].by.as_deref()), (SessionEventKind::Download, None));
        assert_eq!(ev[2].kind, SessionEventKind::Options);
        assert_eq!(ev[3].kind, SessionEventKind::Achievement);
        assert_eq!(ev[4].kind, SessionEventKind::Saves);
        // At most MAX_EVENTS, the newest.
        let many: Vec<ActivityEntry> = (0..40).map(|i| entry(&format!("2026-11-19T20:{:02}:00Z", i + 1), "agent:x", &format!("options r{i} for g: a"), Some(g))).collect();
        let ev = session_events(&many, g, started);
        assert_eq!(ev.len(), MAX_EVENTS);
        assert!(ev.last().unwrap().text.starts_with("Options r39"));
    }

    #[test]
    fn reporting_lines() {
        let ubi = Reporting { store: Some(StoreId::Ubisoft), mode: "on".into() };
        let mut reports = BTreeMap::new();
        assert_eq!(reporting_words(&ubi, &reports, 31).1, "All 31 synced");
        assert_eq!(reporting_words(&ubi, &reports, 0).1, "Nothing unlocked yet");
        reports.insert("12".into(), ReportState::Pending);
        reports.insert("13".into(), ReportState::Reported);
        assert_eq!(reporting_words(&ubi, &reports, 31).1, "1 waiting to send · 30 synced");
        reports.insert("14".into(), ReportState::Failed);
        assert_eq!(reporting_words(&ubi, &reports, 31).1, "1 failed, 1 waiting · Quiver retries them");
        assert!(reporting_words(&Reporting { mode: "off".into(), ..ubi.clone() }, &reports, 3).1.starts_with("Reporting is off"));
        assert_eq!(reporting_words(&Reporting { store: Some(StoreId::Epic), mode: "by_game".into() }, &reports, 3).0, "Reported to Epic");
        assert_eq!(reporting_words(&Reporting { store: Some(StoreId::Steam), mode: "client".into() }, &reports, 3).0, "Recorded by Steam");
    }

    #[test]
    fn capture_folders() {
        let p = std::path::Path::new("/home/u/Pictures");
        assert_eq!(captures_dir(p, "Assassin's Creed® Origins"), p.join("Quiver/Assassin's Creed Origins"));
        assert_eq!(captures_dir(p, "../.."), p.join("Quiver/Game"));
        assert_eq!(captures_dir(p, "A/B: C"), p.join("Quiver/A B C"));
    }

    #[test]
    fn wire() {
        let r = crate::Request::Sight(super::super::SightRequest::Session { session: "s-1".into() });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, serde_json::json!({"method": "sight", "params": {"op": "session", "session": "s-1"}}));
        let info = SessionInfo { session: "s-1".into(), art: vec![ArtRef::Url("https://x/y.jpg".into()), ArtRef::File("/a.jpg".into())], ..Default::default() };
        let back: SessionInfo = serde_json::from_value(serde_json::to_value(&info).unwrap()).unwrap();
        assert_eq!(back, info);
    }
}
