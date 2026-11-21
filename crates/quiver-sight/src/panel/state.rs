//! The panel's model, without GPUI (unit-tested below): what quiverd told us about the
//! session ([`PanelData`]), the rows each section shows ([`rows`]), focus and navigation
//! ([`Nav`]), and what Left / Right / A do to a value ([`step_value`]).
//!
//! Navigation (the shell of the mock-ups, design/SIGHT.md §9): a tab row of sections on top
//! (LB / RB from anywhere, wrapping) and the section's rows. D-pad or stick moves, A
//! activates, Left / Right adjust the focused stepper or toggle, B goes back (in Mods, from
//! the rows to its surface list) and then closes. LT / RT are the section's tabs (Mods' and
//! Tweaks' pages), X and Y the section's own (reset, pin), R3 docks or centres the panel
//! ([`DockPrefs`], remembered per section).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use quiver_api::GameRef;
use quiver_api::options::{Level, OptionState, OptionType, SurfaceState, same_value};
use quiver_api::perf::{Anchor, HudSettingsView, HudStatus, PerfValue, Stat};
use quiver_api::sight::PanelSection;
use quiver_ui::input::Input;
use quiver_ui::options::{allowed_keys, at_level, pad_next, step_number};
use serde_json::Value;

use super::sections::achievements::Filter;
use super::sections::prefs::{Pref, Prefs};
use super::sections::session::Extras;

/// Something quiverd is asked for.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Load<T> {
    #[default]
    Loading,
    Ready(T),
    Failed(String),
    /// Doesn't exist for this game ("no mod options in this profile", another store's game).
    Unavailable(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// The running session, as the Session section shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub session: String,
    pub game: GameRef,
    pub name: String,
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    /// "DLSS v3 (current)", bottom to top.
    pub mods: Vec<String>,
    /// The profile's game settings in words ("its own (Remedy/AlanWakeRemastered)",
    /// "Vanilla's files"); `None`: vanilla, or not known (design/PROFILES.md).
    pub settings: Option<String>,
    pub started: Option<DateTime<Utc>>,
    /// The store's total play time, before this session.
    pub total_secs: Option<u64>,
    /// "Quiver install", "Steam copy".
    pub copy: String,
    /// Ubisoft accepted the session (play time recorded upstream); `None`: not a Ubisoft one.
    pub reported: Option<bool>,
    pub pids: Vec<u32>,
}

impl Default for SessionView {
    fn default() -> Self {
        Self {
            session: String::new(),
            game: GameRef::ubisoft(0),
            name: String::new(),
            profile_id: None,
            profile_name: None,
            mods: Vec::new(),
            settings: None,
            started: None,
            total_secs: None,
            copy: String::new(),
            reported: None,
            pids: Vec::new(),
        }
    }
}

impl SessionView {
    /// The profile's id to edit options in (`None`: vanilla).
    pub fn profile(&self) -> Option<&str> {
        self.profile_id.as_deref().filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case(quiver_api::mods::VANILLA))
    }
}

#[derive(Debug, Clone)]
pub struct HudState {
    /// The running HUD (`None`: the HUD isn't set up for this launch).
    pub status: Option<HudStatus>,
    /// The game scope's settings.
    pub settings: HudSettingsView,
}

impl HudState {
    /// The HUD is set up for this launch, so changes apply live.
    pub fn live(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.enabled)
    }
}

/// One achievement, as the panel lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ach {
    pub id: String,
    pub name: String,
    pub description: String,
    pub hidden: bool,
    pub unlocked: bool,
    pub unlocked_at: Option<DateTime<Utc>>,
    /// Icon URLs (unlocked, greyed); [`Achs::icons`] has them as files.
    pub icon: Option<String>,
    pub icon_gray: Option<String>,
    pub progress: Option<quiver_api::stores::AchievementProgress>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Achs {
    pub unlocked: usize,
    pub total: usize,
    /// Unlocked first (newest first), then locked.
    pub list: Vec<Ach>,
    /// Who records the unlocks, and (Ubisoft) the report journal per achievement id.
    pub reporting: Option<quiver_api::sight::session::Reporting>,
    pub reports: BTreeMap<String, quiver_api::achievements::ReportState>,
    /// Icon URL → file on this machine.
    pub icons: BTreeMap<String, std::path::PathBuf>,
    /// What quiverd couldn't read ("unlocks not read yet").
    pub note: Option<String>,
}

impl Achs {
    pub fn sorted(mut list: Vec<Ach>) -> Achs {
        list.sort_by(|a, b| b.unlocked.cmp(&a.unlocked).then(b.unlocked_at.cmp(&a.unlocked_at)));
        Achs { unlocked: list.iter().filter(|a| a.unlocked).count(), total: list.len(), list, ..Default::default() }
    }
}

/// Everything the panel shows for a session.
#[derive(Debug, Clone, Default)]
pub struct PanelData {
    pub session: Load<SessionView>,
    /// The running profile's option surfaces (mods and runtimes; not the platform's).
    pub options: Load<Vec<SurfaceState>>,
    /// Quiver Tweaks at [`Self::tweak_scope`].
    pub tweaks: Load<SurfaceState>,
    /// "DLSS profile", "this game".
    pub tweak_scope_name: String,
    pub hud: Load<HudState>,
    pub achievements: Load<Achs>,
    /// Live metrics: id → stat → value.
    pub metrics: BTreeMap<String, BTreeMap<Stat, PerfValue>>,
    /// The mods' readouts ("DLSS right now": a surface's `state` and `text` metrics, SIGHT-HUD
    /// §3.13), by surface key: (metric id, label), in the catalog's order. Their values are
    /// in [`Self::metrics`].
    pub readouts: BTreeMap<String, Vec<(String, String)>>,
    /// The Session section's cards beyond the session (`sections::session`).
    pub extras: Extras,
    /// The panel's own settings (Do not disturb, On unlock, hidden achievements).
    pub prefs: Prefs,
}

// ---- Rows -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HudRow {
    /// The Sight layer in the game's launches (shown while this launch has none).
    Enable,
    Shown,
    Preset,
    Position,
    Scale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Quit,
    ForceQuit,
    /// The game's next frame as a PNG (no confirmation).
    Screenshot,
    Replay,
    StopReplay,
}

impl Button {
    /// Asks "are you sure?" first.
    pub fn confirms(self) -> bool {
        matches!(self, Button::Quit | Button::ForceQuit)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Heading { title: String, detail: Option<String> },
    Fact { label: String, value: String },
    Note(String),
    /// A mod option: (surface key, option id).
    Opt { surface: String, id: String },
    Tweak { id: String },
    Hud(HudRow),
    Button(Button),
    /// The key metrics, as tiles.
    Metrics,
    Achievement(usize),
    /// The Performance section's body: its own controls and editor
    /// (`sections::performance`), one row for the panel's navigation.
    Performance,
    /// A surface's preset picker (surface key).
    Preset { surface: String },
    /// One of the Mods and Tweaks sections' buttons (Undo, Revert this session, Session only).
    Act(Act),
    /// One of the panel's own settings (`sections::prefs`).
    Pref(Pref),
    /// The Achievements list's filter (also LT / RT anywhere in the section).
    AchFilter,
}

/// The Mods and Tweaks sections' buttons (design/SIGHT.md §9 "Mods section").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Act {
    /// Undo the saved changes that wait for the next launch (`tweaks`: the Tweaks section's).
    UndoRestart { tweaks: bool },
    /// Put back the values the session launched with (`OptionsRevertSession`).
    RevertSession,
    /// This session's changes for this session only: the profile goes back to what it
    /// launched with, the game keeps them (`live: only`), and later changes aren't saved.
    SessionOnly,
}

impl Row {
    pub fn focusable(&self) -> bool {
        matches!(self, Row::Opt { .. } | Row::Tweak { .. } | Row::Hud(_) | Row::Button(_) | Row::Achievement(_) | Row::Preset { .. } | Row::Act(_) | Row::Pref(_) | Row::AchFilter | Row::Performance)
    }
}

/// The HUD presets, in `Ctrl+Shift+F3` order.
pub const PRESETS: [(&str, &str); 6] = [("fps", "FPS only"), ("compact", "Compact"), ("standard", "Standard"), ("full", "Full"), ("diagnostics", "Diagnostics"), ("clock", "Clock")];
pub const ANCHORS: [(Anchor, &str); 8] = [
    (Anchor::TopLeft, "Top left"),
    (Anchor::Top, "Top"),
    (Anchor::TopRight, "Top right"),
    (Anchor::Right, "Right"),
    (Anchor::BottomRight, "Bottom right"),
    (Anchor::Bottom, "Bottom"),
    (Anchor::BottomLeft, "Bottom left"),
    (Anchor::Left, "Left"),
];
pub const SCALE: (f32, f32, f32) = (0.75, 2.5, 0.25);

/// Options shown in game: at most the Advanced level, not hidden from the overlay, visible.
pub fn shown_in_game(st: &OptionState) -> bool {
    st.def.overlay != Some(false) && st.visible && at_level(&st.def, Level::Advanced)
}

/// Can Left / Right / A change it here? (Dangerous options and free text are the app's.)
pub fn adjustable(st: &OptionState) -> bool {
    st.def.danger.is_none()
        && match st.def.ty {
            OptionType::Bool | OptionType::Enum | OptionType::Int | OptionType::Float => true,
            OptionType::Key => allowed_keys(&st.def).is_some(),
            _ => false,
        }
}

/// A note while `l` isn't ready ("Loading mod options…", why it failed).
pub fn status<T>(l: &Load<T>, what: &str) -> Option<Row> {
    match l {
        Load::Loading => Some(Row::Note(format!("Loading {what}…"))),
        Load::Failed(e) => Some(Row::Note(format!("Couldn't load {what}: {e}"))),
        Load::Unavailable(why) => Some(Row::Note(why.clone())),
        Load::Ready(_) => None,
    }
}

/// The rows of a section.
pub fn rows(section: PanelSection, d: &PanelData, now: DateTime<Utc>) -> Vec<Row> {
    let mut out = Vec::new();
    match section {
        // The Session section's cards (`sections::session`): its rows are the focusable ones.
        PanelSection::Session => out = super::sections::session::rows(d),
        // Mods and Tweaks lay out by surface and page (`panel/sections/mods.rs`): their first
        // page here, for callers without the panel's navigation.
        PanelSection::Mods => out = super::sections::mods::rows(d, &Nav::default(), &Default::default()),
        PanelSection::Tweaks => out = super::sections::mods::tweak_rows(d, &Nav::default(), &Default::default()),
        PanelSection::Performance => {
            if let Some(r) = status(&d.hud, "the HUD") {
                out.push(r);
            }
            out.push(Row::Performance);
        }
        // Its rows with the filter in `Nav` come from `sections::rows`.
        PanelSection::Achievements => out = super::sections::achievements::rows(d, Filter::All, now),
    }
    out
}

// ---- Values --------------------------------------------------------------------------------

/// The value Left (`dir` < 0), Right (`dir` > 0) or A (`dir` 0) gives an option. Numbers
/// step and stop at their ends, A cycles (as in the app), toggles flip either way, choices
/// go to the previous or next one.
pub fn step_value(st: &OptionState, draft: &Value, dir: i32) -> Option<Value> {
    if !adjustable(st) {
        return None;
    }
    if dir == 0 {
        return pad_next(st, draft, Level::Advanced);
    }
    let list = |l: Vec<Value>| -> Option<Value> {
        let n = l.len() as i32;
        if n == 0 {
            return None;
        }
        let i = l.iter().position(|v| same_value(v, draft)).map_or(0, |i| (i as i32 + dir).rem_euclid(n));
        l.get(i as usize).cloned()
    };
    match st.def.ty {
        OptionType::Bool => Some(Value::Bool(!draft.as_bool().unwrap_or(false))),
        OptionType::Enum => list(st.def.values.iter().filter(|e| e.level.is_none_or(|l| l <= Level::Advanced)).map(|e| e.value.clone()).collect()),
        OptionType::Int | OptionType::Float => {
            // From a special value (0 = "No cap"), Right goes to the range's start.
            if st.def.special_of(draft).is_some() {
                return if dir > 0 { st.def.min.map(|m| if st.def.ty == OptionType::Int { Value::from(m.round() as i64) } else { Value::from(m) }) } else { None };
            }
            let n = step_number(&st.def, draft, dir as f64);
            if same_value(&n, draft) {
                // At the bottom, Left reaches the special value if there is one.
                return (dir < 0).then(|| st.def.special.first().map(|m| m.value.clone())).flatten();
            }
            Some(n)
        }
        OptionType::Key => list(allowed_keys(&st.def)?.into_iter().map(Value::String).collect()),
        _ => None,
    }
}

/// The next of `list` from `cur` in direction `dir` (A: forward), wrapping.
pub fn cycle<T: PartialEq + Clone>(list: &[T], cur: &T, dir: i32) -> T {
    let n = list.len() as i32;
    let i = list.iter().position(|x| x == cur).unwrap_or(0) as i32;
    list[((i + if dir == 0 { 1 } else { dir.signum() }).rem_euclid(n)) as usize].clone()
}

pub fn step_scale(cur: f32, dir: i32) -> f32 {
    let (lo, hi, step) = SCALE;
    let d = if dir == 0 { 1.0 } else { dir.signum() as f32 };
    let snapped = (cur / step).round() * step;
    let next = snapped + d * step;
    if dir == 0 && next > hi + 1e-3 { lo } else { next.clamp(lo, hi) }
}

// ---- Navigation -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zone {
    /// The section's rows.
    #[default]
    Body,
    /// Mods' surface list (the mods, the game's settings, DXVK, …).
    Surfaces,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirm {
    pub what: Button,
    /// The confirm button is focused (else Cancel, the default).
    pub yes: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    None,
    Moved,
    /// Close the panel.
    Close,
    /// Adjust the focused row (-1 Left, +1 Right, 0 A).
    Adjust(Row, i32),
    /// The confirmed button.
    Confirmed(Button),
    /// A on an achievement (reveal a hidden one).
    Reveal(usize),
    /// LT / RT (-1 / +1): the section's tabs (Mods' and Tweaks' pages); a section without
    /// tabs switches sections.
    Tab(i32),
    /// X on the focused row (`None`: nothing focused): Mods and Tweaks reset it to default.
    X(Option<Row>),
    /// Y tapped on the focused row: pin it to the quick menu (held, on a live option, Y
    /// compares instead: the view times it).
    Pin(Option<Row>),
    /// A on one of the Mods / Tweaks buttons.
    Act(Act),
    /// Another surface in Mods' list.
    Surface,
}

/// Which sections are docked (design/SIGHT.md §9, the docked proposal): Mods and Tweaks open
/// docked as a column on the left with the game in full view, the others centred over the
/// blurred game; R3 (or Tab) swaps, and each section remembers (`sight.toml`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DockPrefs {
    set: BTreeMap<usize, bool>,
}

impl DockPrefs {
    /// Until toggled: the sections you tune the game with dock.
    pub fn default_for(s: PanelSection) -> bool {
        matches!(s, PanelSection::Mods | PanelSection::Tweaks)
    }

    pub fn docked(&self, s: PanelSection) -> bool {
        self.set.get(&s.index()).copied().unwrap_or_else(|| Self::default_for(s))
    }

    /// Docks or centres `s`; returns whether it's docked now.
    pub fn toggle(&mut self, s: PanelSection) -> bool {
        let d = !self.docked(s);
        self.set.insert(s.index(), d);
        d
    }

    /// As `sight.toml` keeps it: section (lowercase) → docked.
    pub fn to_map(&self) -> BTreeMap<String, bool> {
        self.set.iter().filter_map(|(i, d)| PanelSection::ALL.get(*i).map(|s| (s.label().to_lowercase(), *d))).collect()
    }

    pub fn from_map(m: &BTreeMap<String, bool>) -> DockPrefs {
        DockPrefs { set: m.iter().filter_map(|(k, d)| PanelSection::parse(k).map(|s| (s.index(), *d))).collect() }
    }
}

/// Where Mods and Tweaks are: the surface picked in Mods' list, and the page per surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pages {
    /// Mods' surface (an index into its list).
    pub surface: usize,
    /// How many surfaces Mods lists now (the view sets it before applying input).
    pub surfaces: usize,
    /// Surface key → page index.
    page: BTreeMap<String, usize>,
}

impl Pages {
    pub fn page(&self, key: &str, n: usize) -> usize {
        self.page.get(key).copied().unwrap_or(0).min(n.saturating_sub(1))
    }

    /// Steps the page of `key` (of `n`), wrapping; false if there's one page or none.
    pub fn step(&mut self, key: &str, n: usize, d: i32) -> bool {
        if n < 2 {
            return false;
        }
        let i = (self.page(key, n) as i32 + d).rem_euclid(n as i32) as usize;
        self.page.insert(key.to_owned(), i);
        true
    }

    pub fn set_page(&mut self, key: &str, i: usize) {
        self.page.insert(key.to_owned(), i);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Nav {
    pub section: PanelSection,
    pub zone: Zone,
    /// The focused row (an index into the section's focusable rows), per section (Mods: per
    /// surface, [`Nav::body_key`]).
    body: BTreeMap<(usize, usize), usize>,
    pub confirm: Option<Confirm>,
    /// Hidden achievements revealed with A (or X).
    pub revealed: BTreeSet<usize>,
    /// The Achievements list's filter (LT / RT in that section).
    pub ach_filter: Filter,
    pub dock: DockPrefs,
    pub pages: Pages,
}

impl Nav {
    pub fn open(&mut self, section: Option<PanelSection>) {
        if let Some(s) = section {
            self.section = s;
        }
        self.zone = Zone::Body;
        self.confirm = None;
    }

    /// The section's panel is docked.
    pub fn docked(&self) -> bool {
        self.dock.docked(self.section)
    }

    /// R3 / Tab: docks or centres the section; returns whether it's docked now.
    pub fn toggle_dock(&mut self) -> bool {
        self.dock.toggle(self.section)
    }

    /// Where the focused row is remembered: the section, and Mods' surface (a new page
    /// starts at its top: [`Nav::set_focus_index`]).
    fn body_key(&self) -> (usize, usize) {
        (self.section.index(), if self.section == PanelSection::Mods { self.pages.surface } else { 0 })
    }

    fn focus_list(rows: &[Row]) -> Vec<usize> {
        rows.iter().enumerate().filter(|(_, r)| r.focusable()).map(|(i, _)| i).collect()
    }

    /// The focused row's index in `rows`, when the rows have focus.
    pub fn focused_row(&self, rows: &[Row]) -> Option<usize> {
        if self.zone != Zone::Body {
            return None;
        }
        let f = Self::focus_list(rows);
        let i = self.body.get(&self.body_key()).copied().unwrap_or(0).min(f.len().checked_sub(1)?);
        f.get(i).copied()
    }

    /// Focus a row directly (a click).
    pub fn focus_row(&mut self, rows: &[Row], row: usize) {
        if let Some(i) = Self::focus_list(rows).iter().position(|r| *r == row) {
            self.zone = Zone::Body;
            let k = self.body_key();
            self.body.insert(k, i);
        }
    }

    /// Focus the `i`th focusable row (a new page starts at its top).
    pub fn set_focus_index(&mut self, i: usize) {
        let k = self.body_key();
        self.body.insert(k, i);
    }

    pub fn select_section(&mut self, s: PanelSection) {
        self.section = s;
        self.zone = Zone::Body;
        self.confirm = None;
    }

    pub fn step_section(&mut self, d: i32) {
        let all = PanelSection::ALL;
        let n = all.len() as i32;
        self.section = all[((self.section.index() as i32 + d).rem_euclid(n)) as usize];
        self.zone = Zone::Body;
    }

    /// Mods lists surfaces to go back to.
    fn has_surfaces(&self) -> bool {
        self.section == PanelSection::Mods && self.pages.surfaces > 0
    }

    /// `rows`: the current section's; `adjustable`: whether a focused row takes Left / Right.
    pub fn apply(&mut self, input: Input, rows: &[Row], adjustable: impl Fn(&Row) -> bool) -> Effect {
        if let Some(c) = &mut self.confirm {
            return match input {
                Input::Left | Input::Right | Input::Up | Input::Down => {
                    c.yes = !c.yes;
                    Effect::Moved
                }
                Input::Activate => {
                    let c = self.confirm.take().unwrap();
                    if c.yes { Effect::Confirmed(c.what) } else { Effect::Moved }
                }
                Input::Back => {
                    self.confirm = None;
                    Effect::Moved
                }
                _ => Effect::None,
            };
        }
        let f = Self::focus_list(rows);
        let key = self.body_key();
        let cur = self.body.get(&key).copied().unwrap_or(0).min(f.len().saturating_sub(1));
        let focused = (self.zone == Zone::Body).then(|| f.get(cur).map(|&i| &rows[i])).flatten();
        match input {
            Input::PrevSection | Input::NextSection => {
                self.step_section(if input == Input::NextSection { 1 } else { -1 });
                Effect::Moved
            }
            Input::PrevTab | Input::NextTab => Effect::Tab(if input == Input::NextTab { 1 } else { -1 }),
            Input::Up | Input::Down => {
                let d = if input == Input::Down { 1 } else { -1 };
                match self.zone {
                    Zone::Surfaces => {
                        let i = self.pages.surface as i32 + d;
                        if !(0..self.pages.surfaces as i32).contains(&i) {
                            return Effect::None;
                        }
                        self.pages.surface = i as usize;
                        Effect::Surface
                    }
                    Zone::Body => {
                        let i = cur as i32 + d;
                        if f.is_empty() || !(0..f.len() as i32).contains(&i) {
                            return Effect::None;
                        }
                        self.body.insert(key, i as usize);
                        Effect::Moved
                    }
                }
            }
            Input::Right | Input::Left => {
                let d = if input == Input::Right { 1 } else { -1 };
                match (self.zone, focused) {
                    (Zone::Surfaces, _) if d > 0 => {
                        self.zone = Zone::Body;
                        Effect::Moved
                    }
                    (Zone::Body, Some(r)) if adjustable(r) => Effect::Adjust(r.clone(), d),
                    (Zone::Body, _) if d < 0 && self.has_surfaces() => {
                        self.zone = Zone::Surfaces;
                        Effect::Moved
                    }
                    _ => Effect::None,
                }
            }
            Input::Activate => match (self.zone, focused) {
                (Zone::Surfaces, _) => {
                    self.zone = Zone::Body;
                    Effect::Moved
                }
                (Zone::Body, Some(Row::Button(b))) if !b.confirms() => Effect::Confirmed(*b),
                (Zone::Body, Some(Row::Button(b))) => {
                    self.confirm = Some(Confirm { what: *b, yes: false });
                    Effect::Moved
                }
                (Zone::Body, Some(Row::Achievement(i))) => {
                    self.revealed.insert(*i);
                    Effect::Reveal(*i)
                }
                (Zone::Body, Some(Row::Act(a))) => Effect::Act(*a),
                (Zone::Body, Some(r)) if adjustable(r) => Effect::Adjust(r.clone(), 0),
                _ => Effect::None,
            },
            Input::Back => match self.zone {
                Zone::Body if self.has_surfaces() => {
                    self.zone = Zone::Surfaces;
                    Effect::Moved
                }
                _ => Effect::Close,
            },
            Input::PlayOptions => Effect::X(focused.cloned()),
            Input::Menu => Effect::Pin(focused.cloned()),
            _ => Effect::None,
        }
    }
}

/// What a key does in the panel: one of the shared [`Input`]s, or the panel's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Input(Input),
    /// R3 (hyprpad types Tab): dock or centre.
    Dock,
    /// Y: tapped, pin to the quick menu; held, compare (the view times it on its key-up).
    Y,
}

/// The keyboard's keys ([`Key`]); hyprpad's controller mapping types these too
/// (`tools/hyprpad/quiver-sight.lua`: D-pad arrows, A Enter, B Escape, X `x`, Y `y`, LB / RB
/// Page Up / Down, LT / RT Home / End, R3 Tab). `None`: not the panel's.
pub fn panel_key(key: &str, shift: bool, ctrl: bool) -> Option<Key> {
    Some(match (key, shift, ctrl) {
        ("tab", false, false) => Key::Dock,
        ("y", false, false) => Key::Y,
        _ => Key::Input(key_input(key, shift, ctrl)?),
    })
}

/// The keyboard's keys as [`Input`]s. `None`: not one.
pub fn key_input(key: &str, shift: bool, ctrl: bool) -> Option<Input> {
    Some(match (key, shift, ctrl) {
        ("up", _, _) | ("k", false, false) => Input::Up,
        ("down", _, _) | ("j", false, false) => Input::Down,
        ("left", _, _) | ("h", false, false) => Input::Left,
        ("right", _, _) | ("l", false, false) => Input::Right,
        ("enter", _, _) | ("space", _, _) => Input::Activate,
        ("escape", _, _) | ("backspace", _, _) => Input::Back,
        ("tab", true, _) | ("pageup", _, _) | ("q", false, false) => Input::PrevSection,
        ("pagedown", _, _) | ("e", false, false) => Input::NextSection,
        ("home", _, _) | ("[", _, _) => Input::PrevTab,
        ("end", _, _) | ("]", _, _) => Input::NextTab,
        ("x", false, false) | ("delete", _, _) => Input::PlayOptions,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::options::{EnumValue, Mark, OptionDef, PageState, GroupState};
    use serde_json::json;

    fn st(ty: OptionType) -> OptionState {
        OptionState { def: OptionDef { ty, ..Default::default() }, visible: true, ..Default::default() }
    }

    fn surface() -> SurfaceState {
        let mut s = SurfaceState { key: "m/aco-dlss".into(), title: "DLSS".into(), version_label: Some("v3".into()), ..Default::default() };
        let mut fg = st(OptionType::Bool);
        fg.def.label = Some("Frame generation".into());
        s.options.insert("fg.enabled".into(), fg);
        let mut path = st(OptionType::String);
        path.def.overlay = Some(false);
        s.options.insert("paths.log".into(), path);
        let mut dbg = st(OptionType::Bool);
        dbg.def.level = Level::Debug;
        s.options.insert("debug.x".into(), dbg);
        s.pages = vec![PageState {
            id: "main".into(),
            title: "Main".into(),
            groups: vec![GroupState { title: "Frame generation".into(), options: vec!["fg.enabled".into(), "paths.log".into(), "debug.x".into()] }],
        }];
        s
    }

    #[test]
    fn mods_rows_hide_what_isnt_for_the_overlay() {
        let d = PanelData { options: Load::Ready(vec![surface()]), ..Default::default() };
        let r = rows(PanelSection::Mods, &d, Utc::now());
        let opts: Vec<&str> = r.iter().filter_map(|r| if let Row::Opt { id, .. } = r { Some(id.as_str()) } else { None }).collect();
        assert_eq!(opts, ["fg.enabled"], "no overlay=false option, no debug-level one");
        let empty = PanelData { options: Load::Ready(vec![]), ..Default::default() };
        assert!(matches!(&rows(PanelSection::Mods, &empty, Utc::now())[0], Row::Note(_)));
        // A profile without mods is a settings profile, not an error.
        let settings_only = PanelData { options: Load::Ready(vec![]), session: Load::Ready(SessionView::default()), ..Default::default() };
        assert!(matches!(&rows(PanelSection::Mods, &settings_only, Utc::now())[0], Row::Note(n) if n.starts_with("This profile has no mods")));
        assert!(matches!(&rows(PanelSection::Mods, &PanelData::default(), Utc::now())[0], Row::Note(n) if n.starts_with("Loading")));
    }

    #[test]
    fn session_rows_and_buttons() {
        let s = SessionView { name: "Assassin's Creed Origins".into(), copy: "Quiver install".into(), ..Default::default() };
        let d = PanelData { session: Load::Ready(s), ..Default::default() };
        let r = rows(PanelSection::Session, &d, Utc::now());
        assert_eq!(r, [Row::Pref(Pref::Dnd), Row::Button(Button::Screenshot), Row::Button(Button::Quit), Row::Button(Button::ForceQuit)], "the cards draw the facts; the rows are what takes focus");
        // Quit is there even while the session is still loading.
        assert!(rows(PanelSection::Session, &PanelData::default(), Utc::now()).contains(&Row::Button(Button::ForceQuit)));
        // A screenshot needs no confirmation.
        let rows = vec![Row::Button(Button::Screenshot)];
        let mut n = Nav::default();
        assert_eq!(n.apply(Input::Activate, &rows, |_| false), Effect::Confirmed(Button::Screenshot));
        assert_eq!(n.confirm, None);
    }

    #[test]
    fn navigation() {
        let rows = vec![Row::Heading { title: "x".into(), detail: None }, Row::Hud(HudRow::Shown), Row::Note("n".into()), Row::Hud(HudRow::Preset), Row::Button(Button::Quit)];
        let adj = |r: &Row| matches!(r, Row::Hud(_));
        let mut n = Nav::default();
        // No side list: the rows have focus from the start.
        assert_eq!(n.zone, Zone::Body);
        assert_eq!(n.focused_row(&rows), Some(1), "the first focusable row");
        assert_eq!(n.apply(Input::Up, &rows, adj), Effect::None, "top of the rows");
        assert_eq!(n.apply(Input::Right, &rows, adj), Effect::Adjust(Row::Hud(HudRow::Shown), 1));
        assert_eq!(n.apply(Input::Activate, &rows, adj), Effect::Adjust(Row::Hud(HudRow::Shown), 0));
        assert_eq!(n.apply(Input::Down, &rows, adj), Effect::Moved);
        assert_eq!(n.focused_row(&rows), Some(3), "skips the note");
        n.apply(Input::Down, &rows, adj);
        assert_eq!(n.apply(Input::Down, &rows, adj), Effect::None, "end of the rows");
        // Left on a button: nothing (no list to go back to); B closes.
        assert_eq!(n.apply(Input::Left, &rows, adj), Effect::None);
        assert_eq!(n.apply(Input::Back, &rows, adj), Effect::Close);
        // The tab row: LB / RB wrap, from anywhere; each section remembers its row.
        assert_eq!(n.focused_row(&rows), Some(4));
        assert_eq!(n.apply(Input::PrevSection, &rows, adj), Effect::Moved);
        assert_eq!(n.section, PanelSection::Achievements);
        assert_eq!(n.focused_row(&rows), Some(1), "Achievements starts at its top");
        assert_eq!(n.apply(Input::NextSection, &rows, adj), Effect::Moved);
        assert_eq!((n.section, n.focused_row(&rows)), (PanelSection::Session, Some(4)));
        // LT / RT are the section's tabs (a section without them steps sections: the view).
        assert_eq!(n.apply(Input::NextTab, &rows, adj), Effect::Tab(1));
        assert_eq!(n.apply(Input::PrevTab, &rows, adj), Effect::Tab(-1));
        n.step_section(-1);
        assert_eq!(n.section, PanelSection::Achievements);
        n.step_section(1);
        // X and Y name the focused row.
        assert_eq!(n.apply(Input::PlayOptions, &rows, adj), Effect::X(Some(Row::Button(Button::Quit))));
        assert_eq!(n.apply(Input::Menu, &rows, adj), Effect::Pin(Some(Row::Button(Button::Quit))));
        // A click focuses a row directly.
        n.focus_row(&rows, 3);
        assert_eq!(n.focused_row(&rows), Some(3));
        // Opening on a section focuses its rows.
        let mut n = Nav::default();
        n.open(Some(PanelSection::Performance));
        assert_eq!((n.section, n.zone), (PanelSection::Performance, Zone::Body));
    }

    #[test]
    fn docking_per_section() {
        let mut d = DockPrefs::default();
        // Mods and Tweaks dock until told otherwise; the rest centre.
        assert!(d.docked(PanelSection::Mods) && d.docked(PanelSection::Tweaks));
        assert!(!d.docked(PanelSection::Session) && !d.docked(PanelSection::Performance) && !d.docked(PanelSection::Achievements));
        assert!(!d.toggle(PanelSection::Mods), "Mods centred");
        assert!(d.toggle(PanelSection::Achievements), "Achievements docked");
        assert!(!d.docked(PanelSection::Mods) && d.docked(PanelSection::Tweaks) && d.docked(PanelSection::Achievements));
        // As sight.toml keeps it, and back.
        let m = d.to_map();
        assert_eq!(m, [("achievements".to_owned(), true), ("mods".to_owned(), false)].into());
        assert_eq!(DockPrefs::from_map(&m), d);
        assert_eq!(DockPrefs::from_map(&[("nonsense".to_owned(), true)].into()), DockPrefs::default());
        // Through the navigation: the section switch carries the layout with it.
        let mut n = Nav { dock: d, ..Default::default() };
        n.open(Some(PanelSection::Tweaks));
        assert!(n.docked());
        n.step_section(-1);
        assert_eq!(n.section, PanelSection::Mods);
        assert!(!n.docked());
        assert!(n.toggle_dock());
        assert!(n.docked() && n.dock.docked(PanelSection::Mods));
    }
    #[test]
    fn quitting_needs_a_confirmation() {
        let rows = vec![Row::Button(Button::Quit), Row::Button(Button::ForceQuit)];
        let mut n = Nav::default();
        n.focus_row(&rows, 1);
        assert_eq!(n.apply(Input::Activate, &rows, |_| false), Effect::Moved);
        assert_eq!(n.confirm, Some(Confirm { what: Button::ForceQuit, yes: false }));
        // Cancel is the default: A there does nothing.
        assert_eq!(n.apply(Input::Activate, &rows, |_| false), Effect::Moved);
        assert_eq!(n.confirm, None);
        n.apply(Input::Activate, &rows, |_| false);
        // B cancels without closing the panel.
        assert_eq!(n.apply(Input::Back, &rows, |_| false), Effect::Moved);
        assert_eq!(n.confirm, None);
        n.apply(Input::Activate, &rows, |_| false);
        assert_eq!(n.apply(Input::Right, &rows, |_| false), Effect::Moved);
        assert_eq!(n.apply(Input::Activate, &rows, |_| false), Effect::Confirmed(Button::ForceQuit));
        // Sections don't switch under a dialog.
        n.apply(Input::Activate, &rows, |_| false);
        assert_eq!(n.apply(Input::NextSection, &rows, |_| false), Effect::None);
    }

    #[test]
    fn stepping_values() {
        let b = st(OptionType::Bool);
        assert_eq!(step_value(&b, &json!(false), -1), Some(json!(true)));
        assert_eq!(step_value(&b, &json!(true), 0), Some(json!(false)));
        let mut e = st(OptionType::Enum);
        e.def.values = ["dlss", "fsr", "taa"].iter().map(|v| EnumValue { value: json!(v), ..Default::default() }).collect();
        assert_eq!(step_value(&e, &json!("dlss"), -1), Some(json!("taa")), "wraps");
        assert_eq!(step_value(&e, &json!("dlss"), 1), Some(json!("fsr")));
        let mut f = st(OptionType::Float);
        f.def.min = Some(0.5);
        f.def.max = Some(1.0);
        f.def.step = Some(0.25);
        assert_eq!(step_value(&f, &json!(1.0), 1), None, "stops at the end");
        assert_eq!(step_value(&f, &json!(0.75), 1), Some(json!(1.0)));
        assert_eq!(step_value(&f, &json!(1.0), 0), Some(json!(0.5)), "A cycles like the app");
        let mut cap = st(OptionType::Int);
        cap.def.min = Some(30.0);
        cap.def.max = Some(300.0);
        cap.def.special = vec![Mark { value: json!(0), label: "No cap".into() }];
        assert_eq!(step_value(&cap, &json!(30), -1), Some(json!(0)), "below the range: the special value");
        assert_eq!(step_value(&cap, &json!(0), 1), Some(json!(30)));
        assert_eq!(step_value(&cap, &json!(0), -1), None);
        let mut danger = st(OptionType::Bool);
        danger.def.danger = Some("resets your saves".into());
        assert_eq!(step_value(&danger, &json!(false), 1), None, "dangerous options are the app's");
        assert_eq!(step_value(&st(OptionType::String), &json!("x"), 1), None);
        assert_eq!(cycle(&PRESETS.map(|p| p.0), &"full", 1), "diagnostics");
        assert_eq!(cycle(&PRESETS.map(|p| p.0), &"diagnostics", 1), "clock");
        assert_eq!(cycle(&PRESETS.map(|p| p.0), &"fps", -1), "clock");
        assert_eq!(step_scale(1.0, 1), 1.25);
        assert_eq!(step_scale(2.5, 1), 2.5);
        assert_eq!(step_scale(2.5, 0), 0.75, "A wraps");
        assert_eq!(step_scale(0.75, -1), 0.75);
    }

    #[test]
    fn keys() {
        assert_eq!(key_input("tab", true, false), Some(Input::PrevSection));
        assert_eq!(key_input("pagedown", false, false), Some(Input::NextSection));
        assert_eq!(key_input("]", false, false), Some(Input::NextTab), "[ ] are the section's tabs now");
        assert_eq!(key_input("home", false, false), Some(Input::PrevTab), "hyprpad's LT");
        assert_eq!(key_input("escape", false, false), Some(Input::Back));
        assert_eq!(key_input("enter", false, false), Some(Input::Activate));
        assert_eq!(key_input("x", false, false), Some(Input::PlayOptions), "X: reset");
        assert_eq!(key_input("f2", true, false), None, "Shift+F2 is the panel's own close, handled before");
        // The panel's own: R3 (Tab) docks, Y is timed (tap pins, hold compares).
        assert_eq!(panel_key("tab", false, false), Some(Key::Dock));
        assert_eq!(panel_key("y", false, false), Some(Key::Y));
        assert_eq!(panel_key("down", false, false), Some(Key::Input(Input::Down)));
        assert_eq!(panel_key("y", false, true), None, "Ctrl+Y isn't the panel's");
    }

    #[test]
    fn performance_is_one_row_with_its_own_controls() {
        let view = HudSettingsView {
            scope: Default::default(),
            set: Default::default(),
            effective: quiver_api::perf::HudConfig::default(),
            revision: 1,
            anti_cheat: None,
        };
        let d = PanelData { hud: Load::Ready(HudState { status: None, settings: view }), ..Default::default() };
        assert_eq!(rows(PanelSection::Performance, &d, Utc::now()), [Row::Performance]);
        assert!(Row::Performance.focusable());
        assert!(matches!(&rows(PanelSection::Performance, &PanelData::default(), Utc::now())[..], [Row::Note(_), Row::Performance]));
        // Y reaches the section as Input::Menu (PanelView::key), X as PlayOptions.
        assert_eq!(panel_key("y", false, false), Some(Key::Y));
    }

    #[test]
    fn achievements_sorted() {
        let t = Utc::now();
        let a = |id: &str, unlocked: bool, at: Option<DateTime<Utc>>| Ach { id: id.into(), unlocked, unlocked_at: at, ..Default::default() };
        let s = Achs::sorted(vec![a("locked", false, None), a("old", true, Some(t - chrono::Duration::days(3))), a("new", true, Some(t))]);
        assert_eq!(s.list.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["new", "old", "locked"]);
        assert_eq!((s.unlocked, s.total), (2, 3));
    }
}
