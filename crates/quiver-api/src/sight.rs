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
    /// Replay, screenshots and pinned actions.
    #[default]
    Quick,
    /// Game, profile, mod versions, play time, clock, battery, quit.
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
    pub const ALL: [PanelSection; 6] = [PanelSection::Quick, PanelSection::Session, PanelSection::Mods, PanelSection::Tweaks, PanelSection::Performance, PanelSection::Achievements];

    pub fn label(self) -> &'static str {
        match self {
            PanelSection::Quick => "Quick",
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

    /// `quick`, `session`, `mods` (also `options`), `tweaks`, `performance` (also `perf`, `hud`),
    /// `achievements`.
    pub fn parse(s: &str) -> Option<PanelSection> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "quick" => PanelSection::Quick,
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
    /// Open if closed (on `section`, else Quick), close if open.
    Toggle,
    /// Open (or switch to `section` when already open).
    Open,
    Close,
    // ---- appended (Sight quick menu, design/SIGHT.md §16). A quiver-sight from before skips
    // the command (an unknown variant).
    /// Open the panel on Quick, including when another section is already open.
    Quick,
    /// Show the status chip for a few seconds ("Sight · 118 fps · 21:14 · 1 h 12 m").
    Chip,
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
    // ---- appended (design/SIGHT.md §16)
    /// Open the Quick tab in the panel ([`PanelAction::Quick`]).
    Quick {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// The status chip for a few seconds ([`PanelAction::Chip`]).
    Chip {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Posts a notice: a toast over the running game ([`SightNotice`], `Event::SightNotice`).
    /// From agents and the CLI (`quiver sight notify`); mods post them over the SDK
    /// (`{"t":"notice"}`). Rate-limited per source. Answers with the notice as posted.
    Notify { notice: NoticeSpec },
    /// A notice's button was chosen (quiver-sight, the CLI): `Event::SightNoticeAction` goes
    /// to every subscriber, and a mod that posted it gets `{"t":"notice_action"}`.
    NoticeAct { id: String, button: String },
    /// Sight's settings ([`SightSettings`]).
    Settings,
    /// Sets one setting by its dotted key ([`SightSettings::set`]): `toasts.dnd`,
    /// `achievements.on_unlock`, `status_chip.enabled`, `hint.shown`. Answers with the
    /// settings; `Event::SightSettingsChanged` goes out.
    SettingsSet { key: String, value: serde_json::Value },
    /// Pins (or with `on: false` unpins) an item to the quick menu for a game, or one of its
    /// profiles (`scope`: [`pin_scope`]).
    Pin { scope: String, pin: QuickPin, on: bool },
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

// ---- Notices (toasts from quiverd, agents and mods) ----------------------------------------

/// What a notice is about (picks its icon and colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// An agent's or a mod's note, maybe with buttons.
    #[default]
    Notice,
    /// Saves went up to the store's cloud (Ubisoft Connect, Epic).
    CloudSaved,
    /// A mod changed an option itself on a hotkey (`changed`, not persisted): "Frame
    /// Generation off · you pressed F9 · this session only".
    ModHotkey,
    /// A download or update finished.
    Download,
    /// Something went wrong (shown even in do-not-disturb).
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// A button on a notice. Choosing it sends `id` back to whoever posted the notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeButton {
    pub id: String,
    pub label: String,
    /// The filled (accent) button; the others are outlined.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primary: bool,
}

/// At most this many buttons on a notice, and these lengths (longer is cut).
pub const NOTICE_MAX_BUTTONS: usize = 3;
pub const NOTICE_MAX_TITLE: usize = 80;
pub const NOTICE_MAX_BODY: usize = 240;
pub const NOTICE_MAX_LABEL: usize = 32;
/// A notice shows this long unless it says (actionable ones longer: there's something to do).
pub const NOTICE_SECS: u32 = 5;
pub const NOTICE_ACTION_SECS: u32 = 12;
pub const NOTICE_MAX_SECS: u32 = 30;

/// What a poster says.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NoticeSpec {
    #[serde(default)]
    pub kind: NoticeKind,
    #[serde(default)]
    pub level: NoticeLevel,
    /// The small coloured line over the title ("Agent · aco-dlss", "DLSS"); quiverd fills it
    /// in for agents and mods when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// A key to show as a keycap in the body ("You pressed [F9] · this session only").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The running session it is about (`None`: the focused game, else any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<NoticeButton>,
    /// How long it shows (default [`NOTICE_SECS`], [`NOTICE_ACTION_SECS`] with buttons; at most
    /// [`NOTICE_MAX_SECS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u32>,
    /// A poster's own id for it: a second notice with the same tag from the same poster
    /// replaces the first (and a button's answer names it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// A notice as quiverd passes it on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SightNotice {
    /// `n-<seq>` (per quiverd run).
    pub id: String,
    /// Who posted it: `quiverd`, `agent:<name>`, `mod:<surface>`, `cli`.
    pub from: String,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub spec: NoticeSpec,
}

fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Characters a notice's text may hold (no control characters but newlines).
fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' { ' ' } else { c }).collect()
}

impl NoticeSpec {
    /// Checks and trims what a poster sent: a title, at most [`NOTICE_MAX_BUTTONS`] buttons with
    /// distinct short ids, text cut to length.
    pub fn checked(mut self) -> Result<NoticeSpec, String> {
        self.title = cut(&clean(&self.title), NOTICE_MAX_TITLE);
        if self.title.is_empty() {
            return Err("a notice needs a title".into());
        }
        self.body = self.body.map(|b| cut(&clean(&b), NOTICE_MAX_BODY)).filter(|b| !b.is_empty());
        self.caption = self.caption.map(|c| cut(&clean(&c), 40)).filter(|c| !c.is_empty());
        self.key = self.key.map(|k| cut(&clean(&k), 16)).filter(|k| !k.is_empty());
        self.tag = self.tag.map(|t| cut(&clean(&t), 64)).filter(|t| !t.is_empty());
        if self.buttons.len() > NOTICE_MAX_BUTTONS {
            return Err(format!("at most {NOTICE_MAX_BUTTONS} buttons"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for b in &mut self.buttons {
            if b.id.is_empty() || b.id.len() > 48 || !b.id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) {
                return Err(format!("button id {:?}: 1–48 of [A-Za-z0-9._-]", b.id));
            }
            if !seen.insert(b.id.clone()) {
                return Err(format!("button id {:?} twice", b.id));
            }
            b.label = cut(&clean(&b.label), NOTICE_MAX_LABEL);
            if b.label.is_empty() {
                b.label = b.id.clone();
            }
        }
        self.secs = self.secs.map(|s| s.clamp(2, NOTICE_MAX_SECS));
        Ok(self)
    }

    /// How long it shows.
    pub fn secs(&self) -> u32 {
        self.secs.unwrap_or(if self.buttons.is_empty() { NOTICE_SECS } else { NOTICE_ACTION_SECS })
    }

    /// Shown even in do-not-disturb ([`ToastSettings::dnd`]): errors.
    pub fn urgent(&self) -> bool {
        self.kind == NoticeKind::Error || self.level == NoticeLevel::Error
    }
}

// ---- Sight settings (`$XDG_DATA_HOME/quiver/sight.toml`) ------------------------------------

/// What happens when an achievement unlocks (`achievements.on_unlock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnUnlock {
    #[default]
    Toast,
    /// The toast, and the game's frame half a second later (the Sight layer's capture, which
    /// is taken under Sight's own drawing: SIGHT §3.7).
    ToastScreenshot,
    /// Nothing (no toast).
    Off,
}

impl OnUnlock {
    pub const ALL: [OnUnlock; 3] = [OnUnlock::Toast, OnUnlock::ToastScreenshot, OnUnlock::Off];

    /// As the file and `SettingsSet` spell it.
    pub fn key(self) -> &'static str {
        match self {
            OnUnlock::Toast => "toast",
            OnUnlock::ToastScreenshot => "toast_screenshot",
            OnUnlock::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OnUnlock::Toast => "Toast",
            OnUnlock::ToastScreenshot => "Toast and screenshot",
            OnUnlock::Off => "Nothing",
        }
    }

    /// `toast`, `toast_screenshot` (also `toast-screenshot`, `toast and screenshot`), `off`.
    pub fn parse(s: &str) -> Option<OnUnlock> {
        let s = s.trim().to_ascii_lowercase().replace(" and ", "_").replace(['-', ' '], "_");
        OnUnlock::ALL.into_iter().find(|o| o.key() == s)
    }
}

/// Hidden achievements' names and descriptions (`achievements.hidden`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HiddenAchievements {
    /// Masked until revealed (A on the row).
    #[default]
    Masked,
    Shown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToastSettings {
    /// Do not disturb: toasts are silenced while playing, except errors.
    #[serde(default)]
    pub dnd: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AchievementSettings {
    #[serde(default)]
    pub on_unlock: OnUnlock,
    #[serde(default)]
    pub hidden: HiddenAchievements,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChipSettings {
    /// The status chip shows for a few seconds when the panel or the quick menu closes (and on
    /// `quiver sight chip`). Off by default.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HintSettings {
    /// The first-launch hint ("Open Sight: Shift+F2 or Guide+Start") has been shown on this
    /// install. Setting it false shows it again at the next game.
    #[serde(default)]
    pub shown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_at: Option<DateTime<Utc>>,
}

/// What a quick-menu pin points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinKind {
    /// A mod's option (`surface` is the surface key, `<mod>/<surface>`).
    #[default]
    Option,
    /// A Quiver Tweak (`surface` is empty).
    Tweak,
}

/// An item pinned to the quick menu ("pin to quick menu" in a Sight page).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuickPin {
    #[serde(default)]
    pub kind: PinKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    pub id: String,
    /// The label when pinned (the tile shows the live one when it can).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

impl QuickPin {
    pub fn same_item(&self, o: &QuickPin) -> bool {
        self.kind == o.kind && self.surface == o.surface && self.id == o.id
    }
}

/// At most this many pins per scope.
pub const MAX_PINS: usize = 8;

/// The key pins are kept under: a game (`ubisoft:3539`), or one of its profiles
/// (`ubisoft:3539/dlss`). The quick menu shows the profile's pins, then the game's.
pub fn pin_scope(game: &crate::GameRef, profile: Option<&str>) -> String {
    match profile.filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case(crate::mods::VANILLA)) {
        Some(p) => format!("{game}/{p}"),
        None => game.to_string(),
    }
}

/// Sight's own settings. Unknown keys in the file are kept by every writer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SightSettings {
    /// Select+Start held 0.6 s opens the panel (without hyprpad; `quiver sight pad-chord`).
    #[serde(default)]
    pub pad_chord: bool,
    #[serde(default)]
    pub toasts: ToastSettings,
    #[serde(default)]
    pub achievements: AchievementSettings,
    #[serde(default)]
    pub status_chip: ChipSettings,
    #[serde(default)]
    pub hint: HintSettings,
    /// Quick-menu pins per scope ([`pin_scope`]).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub pins: std::collections::BTreeMap<String, Vec<QuickPin>>,
    /// The panel's sections docked (true) or centred (false) by R3, where that differs from
    /// their default (design/SIGHT.md §9): section id → docked.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub docked: std::collections::BTreeMap<String, bool>,
}

/// The settings `SettingsSet` takes, with their values.
pub const SETTING_KEYS: &[(&str, &str)] = &[
    ("toasts.dnd", "true | false: silence toasts while playing, except errors"),
    ("achievements.on_unlock", "toast | toast_screenshot | off"),
    ("achievements.hidden", "masked | shown: hidden achievements' names"),
    ("status_chip.enabled", "true | false: the corner chip when the panel closes"),
    ("hint.shown", "true | false: false shows the first-launch hint again"),
    ("pad_chord", "true | false: Select+Start held opens the panel (no hyprpad)"),
];

fn as_bool(key: &str, v: &serde_json::Value) -> Result<bool, String> {
    match v {
        serde_json::Value::Bool(b) => Ok(*b),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "1" | "yes" => Ok(true),
            "false" | "off" | "0" | "no" => Ok(false),
            _ => Err(format!("{key} takes true or false, not {s:?}")),
        },
        serde_json::Value::Number(n) if n.as_u64() == Some(0) || n.as_u64() == Some(1) => Ok(n.as_u64() == Some(1)),
        other => Err(format!("{key} takes true or false, not {other}")),
    }
}

impl SightSettings {
    /// The value of a dotted key, as JSON.
    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        let v = serde_json::to_value(self).ok()?;
        key.split('.').try_fold(&v, |v, k| v.get(k)).cloned()
    }

    /// Sets a dotted key ([`SETTING_KEYS`]).
    pub fn set(&mut self, key: &str, value: &serde_json::Value) -> Result<(), String> {
        match key {
            "toasts.dnd" => self.toasts.dnd = as_bool(key, value)?,
            "status_chip.enabled" => self.status_chip.enabled = as_bool(key, value)?,
            "pad_chord" => self.pad_chord = as_bool(key, value)?,
            "hint.shown" => {
                self.hint.shown = as_bool(key, value)?;
                self.hint.shown_at = if self.hint.shown { Some(Utc::now()) } else { None };
            }
            "achievements.on_unlock" => {
                let s = value.as_str().unwrap_or_default();
                self.achievements.on_unlock = OnUnlock::parse(s).ok_or_else(|| format!("{key} takes toast, toast_screenshot or off, not {value}"))?;
            }
            "achievements.hidden" => {
                self.achievements.hidden = match value.as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
                    Some("masked") => HiddenAchievements::Masked,
                    Some("shown") => HiddenAchievements::Shown,
                    _ => return Err(format!("{key} takes masked or shown, not {value}")),
                };
            }
            // The panel's layout: the whole map (`{"mods": true, …}`).
            "docked" => {
                let m: std::collections::BTreeMap<String, bool> = serde_json::from_value(value.clone()).map_err(|_| format!("{key} takes a map of section → bool"))?;
                if m.len() > 32 || m.keys().any(|k| k.len() > 32) {
                    return Err(format!("{key}: too many sections"));
                }
                self.docked = m;
            }
            other => {
                let known: Vec<&str> = SETTING_KEYS.iter().map(|(k, _)| *k).collect();
                return Err(format!("unknown Sight setting {other:?} (known: {})", known.join(", ")));
            }
        }
        Ok(())
    }

    /// Pins (`on`) or unpins an item in `scope`. Pinning what's pinned updates its label;
    /// a scope holds at most [`MAX_PINS`].
    pub fn pin(&mut self, scope: &str, pin: QuickPin, on: bool) -> Result<(), String> {
        if pin.id.is_empty() || pin.id.len() > 64 || scope.is_empty() || scope.len() > 160 {
            return Err("a pin needs an option id and a scope".into());
        }
        let list = self.pins.entry(scope.to_owned()).or_default();
        let at = list.iter().position(|p| p.same_item(&pin));
        match (on, at) {
            (true, Some(i)) => list[i].label = pin.label,
            (true, None) if list.len() >= MAX_PINS => return Err(format!("at most {MAX_PINS} pins per game or profile")),
            (true, None) => list.push(pin),
            (false, Some(i)) => {
                list.remove(i);
            }
            (false, None) => {}
        }
        if list.is_empty() {
            self.pins.remove(scope);
        }
        Ok(())
    }

    /// The pins the quick menu shows for a game running `profile`: the profile's, then the
    /// game's, each item once.
    pub fn pins_for(&self, game: &crate::GameRef, profile: Option<&str>) -> Vec<QuickPin> {
        let mut out: Vec<QuickPin> = Vec::new();
        let scopes = [pin_scope(game, profile), pin_scope(game, None)];
        for s in scopes.iter() {
            for p in self.pins.get(s).into_iter().flatten() {
                if !out.iter().any(|o| o.same_item(p)) {
                    out.push(p.clone());
                }
            }
        }
        out
    }

    /// Parses `sight.toml` (missing or broken: the defaults).
    pub fn from_toml(s: &str) -> SightSettings {
        toml::from_str(s).unwrap_or_default()
    }
}

/// `$XDG_DATA_HOME/quiver/sight.toml`.
pub fn settings_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("quiver").join("sight.toml")
}

/// Reads Sight's settings from `path` (missing or unreadable: the defaults).
pub fn load_settings_at(path: &std::path::Path) -> SightSettings {
    std::fs::read_to_string(path).map(|s| SightSettings::from_toml(&s)).unwrap_or_default()
}

/// Writes `s` to `path`, keeping keys it doesn't know, atomically (a temporary file renamed).
pub fn save_settings_at(path: &std::path::Path, s: &SightSettings) -> std::io::Result<()> {
    let mut table: toml::Table = std::fs::read_to_string(path).ok().and_then(|t| t.parse().ok()).unwrap_or_default();
    let ours: toml::Table = toml::Table::try_from(s).map_err(std::io::Error::other)?;
    for k in ["pad_chord", "toasts", "achievements", "status_chip", "hint", "pins", "docked"] {
        table.remove(k);
    }
    table.extend(ours);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string(&table).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, path)
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
    fn notices_on_the_wire() {
        let spec = NoticeSpec {
            title: "  DLSS v4 is ready\u{7} ".into(),
            body: Some("x".repeat(300)),
            buttons: vec![NoticeButton { id: "keep_v3".into(), label: "".into(), primary: true }],
            ..Default::default()
        };
        let c = spec.checked().unwrap();
        assert_eq!(c.title, "DLSS v4 is ready");
        assert_eq!(c.body.as_ref().unwrap().chars().count(), NOTICE_MAX_BODY);
        assert_eq!(c.buttons[0].label, "keep_v3", "an empty label is the id");
        assert_eq!((c.secs(), c.urgent()), (NOTICE_ACTION_SECS, false));
        assert!(NoticeSpec { title: "x".into(), buttons: vec![NoticeButton { id: "a b".into(), label: "x".into(), primary: false }], ..Default::default() }.checked().is_err());
        assert!(NoticeSpec { title: "x".into(), level: NoticeLevel::Error, ..Default::default() }.urgent());
        // Request and event round trips.
        let r = crate::Request::Sight(SightRequest::Notify { notice: c.clone() });
        let back: crate::Request = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert!(matches!(back, crate::Request::Sight(SightRequest::Notify { notice }) if notice == c));
        let n = SightNotice { id: "n-1".into(), from: "agent:x".into(), at: Utc::now(), spec: c };
        let line = serde_json::to_string(&crate::Reply::Event(crate::Event::SightNotice(n.clone()))).unwrap();
        let crate::Reply::Event(crate::Event::SightNotice(back)) = serde_json::from_str(&line).unwrap() else { panic!() };
        assert_eq!(back, n);
        let q: crate::Request = serde_json::from_value(serde_json::json!({"method": "sight", "params": {"op": "quick"}})).unwrap();
        assert!(matches!(q, crate::Request::Sight(SightRequest::Quick { session: None })));
    }

    #[test]
    fn settings_and_pins() {
        let mut s = SightSettings::default();
        assert!(!s.toasts.dnd && !s.status_chip.enabled && !s.hint.shown && s.achievements.on_unlock == OnUnlock::Toast, "the defaults");
        s.set("toasts.dnd", &serde_json::json!("on")).unwrap();
        s.set("achievements.on_unlock", &serde_json::json!("toast-screenshot")).unwrap();
        s.set("status_chip.enabled", &serde_json::json!(true)).unwrap();
        s.set("hint.shown", &serde_json::json!(true)).unwrap();
        assert!(s.toasts.dnd && s.status_chip.enabled && s.hint.shown_at.is_some());
        assert_eq!(s.get("achievements.on_unlock"), Some(serde_json::json!("toast_screenshot")));
        assert!(s.set("toasts.dnd", &serde_json::json!("maybe")).is_err());
        assert!(s.set("nope", &serde_json::json!(1)).is_err());
        s.set("hint.shown", &serde_json::json!(false)).unwrap();
        assert!(!s.hint.shown && s.hint.shown_at.is_none(), "reset");
        // Sight C's keys and values (the Session and Achievements sections' rows).
        s.set("achievements.on_unlock", &serde_json::json!("off")).unwrap();
        s.set("achievements.hidden", &serde_json::json!("shown")).unwrap();
        assert_eq!((s.achievements.on_unlock, s.achievements.hidden), (OnUnlock::Off, HiddenAchievements::Shown));
        let t = toml::to_string(&s).unwrap();
        assert!(t.contains("on_unlock = \"off\"") && t.contains("hidden = \"shown\""), "{t}");
        assert_eq!(SightSettings::from_toml("[achievements]\non_unlock = \"toast_screenshot\"\nhidden = \"masked\"\n").achievements.on_unlock, OnUnlock::ToastScreenshot);
        s.set("docked", &serde_json::json!({"mods": false})).unwrap();
        assert_eq!(s.docked.get("mods"), Some(&false));
        assert!(s.set("docked", &serde_json::json!("yes")).is_err());
        // Pins: per profile, then the game's; no twice; at most MAX_PINS.
        let g = crate::GameRef::ubisoft(3539);
        let pin = |id: &str| QuickPin { kind: PinKind::Option, surface: "m1/aco-dlss".into(), id: id.into(), label: id.into() };
        assert_eq!(pin_scope(&g, Some("vanilla")), "ubisoft:3539");
        s.pin(&pin_scope(&g, Some("dlss")), pin("a"), true).unwrap();
        s.pin(&pin_scope(&g, None), pin("b"), true).unwrap();
        s.pin(&pin_scope(&g, None), pin("a"), true).unwrap();
        let ids: Vec<String> = s.pins_for(&g, Some("dlss")).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(s.pins_for(&g, None).len(), 2);
        s.pin(&pin_scope(&g, Some("dlss")), pin("a"), false).unwrap();
        assert!(!s.pins.contains_key("ubisoft:3539/dlss"), "an empty scope goes");
        for i in 0..MAX_PINS {
            let _ = s.pin("x", pin(&format!("p{i}")), true);
        }
        assert!(s.pin("x", pin("one-more"), true).is_err());
        // TOML round trip, unknown keys kept.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sight.toml");
        std::fs::write(&p, "other = 1\n").unwrap();
        save_settings_at(&p, &s).unwrap();
        assert_eq!(load_settings_at(&p), s);
        assert!(std::fs::read_to_string(&p).unwrap().contains("other = 1"));
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
