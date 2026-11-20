//! The Sight panel (design/SIGHT.md §6, §9, as built in §14): the in-game panel
//! `quiver-sight` shows on a layer-shell surface over the game, summoned with Shift+F2 (read
//! in the game by the Sight layer), a hyprpad chord, or `quiver sight toggle`.
//!
//! quiverd doesn't draw anything: it resolves which session a command is for, makes sure
//! `quiver-sight` runs, and passes the command on as [`PanelCommand`] (a perf event, so it
//! reaches the connection `quiver-sight` already listens on). `quiver-sight` reports back
//! what it shows ([`PanelReport`]), which `quiver sight status` prints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The Session and Achievements sections' requests' answers.
pub mod session;

/// The panel's sections, in the order of its side list (LB/RB step through them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelSection {
    /// Game, profile, mod versions, play time, clock, battery, quit.
    #[default]
    Session,
    /// Live mod options (MOD-CONFIG) of the running profile.
    Mods,
    /// Quiver Tweaks for the game.
    Tweaks,
    /// The HUD (on, preset, position, scale) and the key metrics.
    Performance,
    /// The game's achievements.
    Achievements,
}

impl PanelSection {
    pub const ALL: [PanelSection; 5] = [PanelSection::Session, PanelSection::Mods, PanelSection::Tweaks, PanelSection::Performance, PanelSection::Achievements];

    pub fn label(self) -> &'static str {
        match self {
            PanelSection::Session => "Session",
            PanelSection::Mods => "Mods",
            PanelSection::Tweaks => "Tweaks",
            PanelSection::Performance => "Performance",
            PanelSection::Achievements => "Achievements",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// `session`, `mods` (also `options`), `tweaks`, `performance` (also `perf`, `hud`),
    /// `achievements`.
    pub fn parse(s: &str) -> Option<PanelSection> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "session" | "home" => PanelSection::Session,
            "mods" | "options" | "mod" => PanelSection::Mods,
            "tweaks" => PanelSection::Tweaks,
            "performance" | "perf" | "hud" => PanelSection::Performance,
            "achievements" | "achievement" => PanelSection::Achievements,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelAction {
    /// Open if closed (on `section`, else where it was), close if open.
    Toggle,
    /// Open (or switch to `section` when already open).
    Open,
    Close,
}

/// A panel command, as quiverd passes it to `quiver-sight`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelCommand {
    /// Increases with every command quiverd passes on (per quiverd run).
    pub seq: u64,
    pub action: PanelAction,
    /// The session it is for (quiverd's choice: the one named, else the focused game, else
    /// the only one running). `None` for `Close`, and when no session is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<PanelSection>,
    /// The game's monitor, when quiverd knows it (Hyprland output name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    /// Who asked: `keys` (Shift+F2 in the game), `cli`, `hyprpad` (the chord runs the CLI,
    /// so it says `cli`), an agent's source.
    pub by: String,
    pub at: DateTime<Utc>,
}

/// What sits behind the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backdrop {
    /// The Sight layer blurs the game's own frames (any compositor), under a light tint.
    GameBlur,
    /// The compositor blurs the game behind a light tint (a Hyprland layer rule).
    Blur,
    /// A plain dark scrim over the game.
    #[default]
    Dim,
}

impl Backdrop {
    pub fn as_str(self) -> &'static str {
        match self {
            Backdrop::GameBlur => "game blur",
            Backdrop::Blur => "blur",
            Backdrop::Dim => "dim",
        }
    }

    /// Something blurs the game (the panel draws the light tint, not the dim).
    pub fn blurred(self) -> bool {
        matches!(self, Backdrop::GameBlur | Backdrop::Blur)
    }
}

/// `quiver-sight`'s account of the panel (sent on every change).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PanelReport {
    pub open: bool,
    #[serde(default)]
    pub section: PanelSection,
    /// The section is docked: a column on the left with the game in full view, no blur and
    /// no dim (design/SIGHT.md §9). Absent from a quiver-sight from before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub docked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    /// The backdrop in use (or that the last open used).
    #[serde(default)]
    pub backdrop: Backdrop,
    /// Why that backdrop ("the Sight layer blurs the game", "Hyprland layer rule, blur size
    /// 8 × 2 passes", "blur is off in decoration:blur", "not Hyprland", "the rule failed: …").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backdrop_detail: String,
    /// The controller: `none`, a pad's name, or why gilrs can't read pads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pad: String,
    /// The Select+Start long-press fallback is on (off by default).
    #[serde(default)]
    pub pad_chord: bool,
    /// The window focus goes back to on close (Hyprland address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_to: Option<String>,
    /// Open but hidden because the game isn't in front (design/SIGHT.md §14): why ("another
    /// window is active (kitty)"). It comes back, as it was, when the game is in front again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<String>,
    /// Whether the game is in front and how that's known ("the game's window is active", "no
    /// way to tell (…)").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub front: String,
    /// The game's toplevel the panel is tied to (app id, title, how it was found), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_window: Option<String>,
    /// Where the panel is drawn: `game` (composited into the game's frames by the Sight layer,
    /// input through the preload; design/SIGHT.md §14) or `desktop` (a layer-shell surface
    /// with the keyboard). Empty from a quiver-sight from before.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub presenter: String,
    /// Why that one ("the Sight layer draws it in the game", "the input preload isn't in the
    /// game …").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub presenter_detail: String,
    /// The last command it acted on.
    #[serde(default)]
    pub handled_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `SightRequest::Status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SightStatus {
    /// A `quiver-sight` quiverd started is running (one started otherwise isn't seen here,
    /// but its report is).
    pub running: bool,
    /// The last report and when it came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<PanelReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command: Option<PanelCommand>,
    /// Shift+F2 presses the Sight layer saw, per running session with a layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<PanelKeys>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PanelKeys {
    pub session: String,
    /// The layer's key reading (`active`, `active_wayland`, `no_display`, …), as
    /// `quiver hud status` shows it.
    pub keys: String,
    pub presses: u32,
    /// How the session's frames pack their eyes (appended; design/SIGHT.md §15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stereo: Option<crate::perf::StereoInfo>,
}

// `Report` carries a whole `PanelReport`; a request is sent on a panel change, not per frame.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SightRequest {
    /// Open or close the panel (`PanelCommand`). No session named: the focused game, else
    /// the only running one.
    Toggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<PanelSection>,
    },
    Open {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<PanelSection>,
    },
    Close,
    /// `SightStatus`.
    Status,
    /// `quiver-sight` only: what the panel shows now.
    Report { report: PanelReport },
    // ---- The Session and Achievements sections (appended; see [`session`]). All read-only
    // towards the stores.
    /// `session::SessionInfo`: the Proton build, box art, what happened while it ran.
    Session { session: String },
    /// `session::SessionAchievements`: the game's achievements with their report state.
    Achievements { game: crate::GameRef },
    /// Achievement icons on this machine (`Vec<Option<PathBuf>>`, one per URL): from
    /// quiverd's icon cache, downloaded into it first when missing (known image hosts only).
    Icons { urls: Vec<String> },
    /// `session::Screenshot`: the game's next frame, saved under `~/Pictures/Quiver/<game>/`.
    /// No session named: the focused game, else the only one running.
    Screenshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
}

/// Commands older than this are not replayed to a `quiver-sight` that starts after them.
pub const COMMAND_FRESH_SECS: i64 = 8;

impl PanelCommand {
    /// Worth acting on for a `quiver-sight` that just started (it may have been started for
    /// this command and missed the event).
    pub fn fresh(&self, now: DateTime<Utc>, handled: u64) -> bool {
        self.seq > handled && (now - self.at).num_seconds() < COMMAND_FRESH_SECS && self.action != PanelAction::Close
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire() {
        let r = crate::Request::Sight(SightRequest::Toggle { session: None, section: Some(PanelSection::Mods) });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, serde_json::json!({"method": "sight", "params": {"op": "toggle", "section": "mods"}}));
        let back: crate::Request = serde_json::from_value(v).unwrap();
        assert!(matches!(back, crate::Request::Sight(SightRequest::Toggle { section: Some(PanelSection::Mods), .. })));
        assert_eq!(PanelSection::parse("perf"), Some(PanelSection::Performance));
        assert_eq!(PanelSection::parse("Options"), Some(PanelSection::Mods));
        assert_eq!(PanelSection::parse("x"), None);
    }

    #[test]
    fn panel_events_round_trip() {
        let c = PanelCommand { seq: 3, action: PanelAction::Open, session: Some("s-1".into()), section: None, monitor: None, by: "keys".into(), at: Utc::now() };
        let e: crate::Event = crate::perf::PerfEvent::Panel(c.clone()).into();
        assert!(matches!(&e, crate::Event::SightPanel(x) if *x == c));
        assert!(matches!(e.perf(), Some(crate::perf::PerfEvent::Panel(x)) if x == c));
        let line = serde_json::to_string(&crate::Reply::Perf(crate::perf::PerfEvent::Panel(c))).unwrap();
        let r: crate::Reply = serde_json::from_str(&line).unwrap();
        assert!(matches!(r.folded(), crate::Reply::Event(crate::Event::SightPanel(_))));
    }

    #[test]
    fn freshness() {
        let now = Utc::now();
        let c = PanelCommand { seq: 3, action: PanelAction::Toggle, session: None, section: None, monitor: None, by: "cli".into(), at: now };
        assert!(c.fresh(now, 0));
        assert!(!c.fresh(now, 3), "already handled");
        assert!(!c.fresh(now + chrono::Duration::seconds(30), 0), "stale");
        assert!(!PanelCommand { action: PanelAction::Close, ..c }.fresh(now, 0), "a close needs no replay");
    }
}
