//! The panel's model, without GPUI (unit-tested below): what quiverd told us about the
//! session ([`PanelData`]), the rows each section shows ([`rows`]), focus and navigation
//! ([`Nav`]), and what Left / Right / A do to a value ([`step_value`]).
//!
//! Navigation is the app's model in small: a side list of sections and a body of rows.
//! D-pad or stick moves, A activates, B goes back (body → side list → closes), LB / RB
//! (and LT / RT) switch sections from anywhere, Left / Right adjust the focused stepper or
//! toggle (and, on a row with nothing to adjust, Left goes back to the side list).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use quiver_api::GameRef;
use quiver_api::options::{Level, OptionState, OptionType, SurfaceState, same_value};
use quiver_api::perf::{Anchor, HudSettingsView, HudStatus, PerfValue, Stat};
use quiver_api::sight::PanelSection;
use quiver_ui::input::Input;
use quiver_ui::options::{allowed_keys, at_level, pad_next, step_number};
use serde_json::Value;

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
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Achs {
    pub unlocked: usize,
    pub total: usize,
    /// Unlocked first (newest first), then locked.
    pub list: Vec<Ach>,
}

impl Achs {
    pub fn sorted(mut list: Vec<Ach>) -> Achs {
        list.sort_by(|a, b| b.unlocked.cmp(&a.unlocked).then(b.unlocked_at.cmp(&a.unlocked_at)));
        Achs { unlocked: list.iter().filter(|a| a.unlocked).count(), total: list.len(), list }
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
}

// ---- Rows -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HudRow {
    /// "Use Quiver's HUD" for the next launch (shown while it isn't set up).
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
}

impl Row {
    pub fn focusable(&self) -> bool {
        matches!(self, Row::Opt { .. } | Row::Tweak { .. } | Row::Hud(_) | Row::Button(_) | Row::Achievement(_))
    }
}

/// The HUD presets, in `Ctrl+Shift+F3` order.
pub const PRESETS: [(&str, &str); 5] = [("fps", "FPS only"), ("compact", "Compact"), ("standard", "Standard"), ("full", "Full"), ("clock", "Clock")];
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

fn option_rows(surface: &SurfaceState, out: &mut Vec<Row>, row: impl Fn(&str) -> Row) -> usize {
    let mut n = 0;
    for page in &surface.pages {
        for g in &page.groups {
            let ids: Vec<&String> = g.options.iter().filter(|id| surface.options.get(*id).is_some_and(shown_in_game)).collect();
            if ids.is_empty() {
                continue;
            }
            let title = if surface.pages.len() > 1 && !g.title.is_empty() && g.title != page.title {
                format!("{} · {}", page.title, g.title)
            } else if !g.title.is_empty() {
                g.title.clone()
            } else {
                page.title.clone()
            };
            if !title.is_empty() {
                out.push(Row::Heading { title, detail: None });
            }
            for id in ids {
                out.push(row(id));
                n += 1;
            }
        }
    }
    n
}

fn hm(secs: u64) -> String {
    let m = secs / 60;
    if m >= 60 { format!("{} h {} m", m / 60, m % 60) } else { format!("{m} m") }
}

fn status<T>(l: &Load<T>, what: &str) -> Option<Row> {
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
    let fact = |l: &str, v: String| Row::Fact { label: l.into(), value: v };
    match section {
        PanelSection::Session => {
            if let Some(r) = status(&d.session, "the session") {
                out.push(r);
            }
            if let Some(s) = d.session.ready() {
                out.push(fact("Game", format!("{} · {}", s.name, s.copy)));
                out.push(fact("Profile", s.profile_name.clone().unwrap_or_else(|| "Vanilla".into())));
                if s.mods.is_empty() {
                    out.push(fact("Mods", "none".into()));
                }
                for (i, m) in s.mods.iter().enumerate() {
                    out.push(fact(if i == 0 { "Mods" } else { "" }, m.clone()));
                }
                let this = s.started.map(|t| (now - t).num_seconds().max(0) as u64);
                let mut play = this.map(|x| format!("This session {}", hm(x))).unwrap_or_else(|| "This session —".into());
                if let Some(t) = s.total_secs {
                    play.push_str(&format!("  ·  Total {}", hm(t + this.unwrap_or(0))));
                }
                out.push(fact("Play time", play));
                if let Some(r) = s.reported {
                    out.push(fact("Ubisoft", if r { "play time reported".into() } else { "play time not reported".into() }));
                }
            }
            out.push(Row::Button(Button::Quit));
            out.push(Row::Button(Button::ForceQuit));
        }
        PanelSection::Mods => {
            if let Some(r) = status(&d.options, "mod options") {
                out.push(r);
            }
            if let Some(list) = d.options.ready() {
                let mut any = false;
                for s in list {
                    let title = match (&s.version_label, &s.version) {
                        (Some(l), _) | (None, Some(l)) => format!("{} {l}", s.title),
                        _ => s.title.clone(),
                    };
                    let detail = Some(if s.live.connected { "live".to_owned() } else { live_word(s) });
                    let at = out.len();
                    out.push(Row::Heading { title, detail });
                    let key = s.key.clone();
                    if option_rows(s, &mut out, |id| Row::Opt { surface: key.clone(), id: id.to_owned() }) == 0 {
                        out.truncate(at);
                    } else {
                        any = true;
                    }
                }
                if !any {
                    out.push(Row::Note("No mod in this profile has options to change in game.".into()));
                }
            }
        }
        PanelSection::Tweaks => {
            if let Some(r) = status(&d.tweaks, "tweaks") {
                out.push(r);
            }
            if let Some(s) = d.tweaks.ready() {
                out.push(Row::Note(format!("Saved to {}. Most tweaks apply when the game next starts.", d.tweak_scope_name)));
                if option_rows(s, &mut out, |id| Row::Tweak { id: id.to_owned() }) == 0 {
                    out.push(Row::Note("No tweaks to change in game.".into()));
                }
            }
        }
        PanelSection::Performance => {
            out.push(Row::Metrics);
            if let Some(r) = status(&d.hud, "the HUD") {
                out.push(r);
            }
            if let Some(h) = d.hud.ready() {
                out.push(Row::Heading { title: "HUD".into(), detail: Some(if h.live() { "live".into() } else { "next launch".into() }) });
                if !h.live() {
                    out.push(Row::Note("Quiver's HUD isn't set up for this launch: these settings apply when the game next starts.".into()));
                    out.push(Row::Hud(HudRow::Enable));
                } else {
                    out.push(Row::Hud(HudRow::Shown));
                }
                out.extend([Row::Hud(HudRow::Preset), Row::Hud(HudRow::Position), Row::Hud(HudRow::Scale)]);
                out.push(Row::Note("Shift+F3 shows or hides the HUD in game, Ctrl+Shift+F3 steps the preset.".into()));
            }
        }
        PanelSection::Achievements => {
            if let Some(r) = status(&d.achievements, "achievements") {
                out.push(r);
            }
            if let Some(a) = d.achievements.ready() {
                out.push(Row::Heading { title: format!("{} / {} earned", a.unlocked, a.total), detail: None });
                out.extend((0..a.list.len()).map(Row::Achievement));
            }
        }
    }
    out
}

fn live_word(s: &SurfaceState) -> String {
    use quiver_api::options::LiveChannel;
    match s.live.channel {
        LiveChannel::None => "next launch".into(),
        _ => "live".into(),
    }
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
    #[default]
    Side,
    Body,
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
}

#[derive(Debug, Clone, Default)]
pub struct Nav {
    pub section: PanelSection,
    pub zone: Zone,
    /// The focused row (an index into the section's focusable rows), per section.
    body: BTreeMap<usize, usize>,
    pub confirm: Option<Confirm>,
    /// Hidden achievements revealed with A.
    pub revealed: BTreeSet<usize>,
}

impl Nav {
    pub fn open(&mut self, section: Option<PanelSection>) {
        if let Some(s) = section {
            self.section = s;
            self.zone = Zone::Body;
        }
        self.confirm = None;
    }

    fn focus_list(rows: &[Row]) -> Vec<usize> {
        rows.iter().enumerate().filter(|(_, r)| r.focusable()).map(|(i, _)| i).collect()
    }

    /// The focused row's index in `rows`, when the body has focus.
    pub fn focused_row(&self, rows: &[Row]) -> Option<usize> {
        if self.zone != Zone::Body {
            return None;
        }
        let f = Self::focus_list(rows);
        let i = self.body.get(&self.section.index()).copied().unwrap_or(0).min(f.len().checked_sub(1)?);
        f.get(i).copied()
    }

    /// Focus a row directly (a click).
    pub fn focus_row(&mut self, rows: &[Row], row: usize) {
        if let Some(i) = Self::focus_list(rows).iter().position(|r| *r == row) {
            self.zone = Zone::Body;
            self.body.insert(self.section.index(), i);
        }
    }

    pub fn select_section(&mut self, s: PanelSection) {
        self.section = s;
        self.confirm = None;
    }

    fn step_section(&mut self, d: i32) {
        let all = PanelSection::ALL;
        let n = all.len() as i32;
        self.section = all[((self.section.index() as i32 + d).rem_euclid(n)) as usize];
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
        let key = self.section.index();
        let cur = self.body.get(&key).copied().unwrap_or(0).min(f.len().saturating_sub(1));
        let focused = (self.zone == Zone::Body).then(|| f.get(cur).map(|&i| &rows[i])).flatten();
        match input {
            Input::PrevSection | Input::PrevTab | Input::NextSection | Input::NextTab => {
                self.step_section(if matches!(input, Input::NextSection | Input::NextTab) { 1 } else { -1 });
                Effect::Moved
            }
            Input::Up | Input::Down => {
                let d = if input == Input::Down { 1 } else { -1 };
                match self.zone {
                    Zone::Side => {
                        let n = PanelSection::ALL.len() as i32;
                        let i = self.section.index() as i32 + d;
                        if !(0..n).contains(&i) {
                            return Effect::None;
                        }
                        self.step_section(d);
                        Effect::Moved
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
                    (Zone::Side, _) if d > 0 && !f.is_empty() => {
                        self.zone = Zone::Body;
                        Effect::Moved
                    }
                    (Zone::Body, Some(r)) if adjustable(r) => Effect::Adjust(r.clone(), d),
                    (Zone::Body, _) if d < 0 => {
                        self.zone = Zone::Side;
                        Effect::Moved
                    }
                    _ => Effect::None,
                }
            }
            Input::Activate => match (self.zone, focused) {
                (Zone::Side, _) if !f.is_empty() => {
                    self.zone = Zone::Body;
                    Effect::Moved
                }
                (Zone::Body, Some(Row::Button(b))) => {
                    self.confirm = Some(Confirm { what: *b, yes: false });
                    Effect::Moved
                }
                (Zone::Body, Some(Row::Achievement(i))) => {
                    self.revealed.insert(*i);
                    Effect::Reveal(*i)
                }
                (Zone::Body, Some(r)) if adjustable(r) => Effect::Adjust(r.clone(), 0),
                _ => Effect::None,
            },
            Input::Back => match self.zone {
                Zone::Body => {
                    self.zone = Zone::Side;
                    Effect::Moved
                }
                Zone::Side => Effect::Close,
            },
            _ => Effect::None,
        }
    }
}

/// The keyboard's keys as [`Input`]s (hyprpad's controller mapping types these too; see
/// `tools/hyprpad/quiver-sight.lua`). `None`: not the panel's.
pub fn key_input(key: &str, shift: bool, ctrl: bool) -> Option<Input> {
    Some(match (key, shift, ctrl) {
        ("up", _, _) | ("k", false, false) => Input::Up,
        ("down", _, _) | ("j", false, false) => Input::Down,
        ("left", _, _) | ("h", false, false) => Input::Left,
        ("right", _, _) | ("l", false, false) => Input::Right,
        ("enter", _, _) | ("space", _, _) => Input::Activate,
        ("escape", _, _) | ("backspace", _, _) => Input::Back,
        ("tab", true, _) | ("[", _, _) | ("pageup", _, _) | ("q", false, false) => Input::PrevSection,
        ("tab", false, _) | ("]", _, _) | ("pagedown", _, _) | ("e", false, false) => Input::NextSection,
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
        assert!(matches!(&r[0], Row::Heading { title, .. } if title == "DLSS v3"));
        let empty = PanelData { options: Load::Ready(vec![]), ..Default::default() };
        assert!(matches!(&rows(PanelSection::Mods, &empty, Utc::now())[0], Row::Note(_)));
        assert!(matches!(&rows(PanelSection::Mods, &PanelData::default(), Utc::now())[0], Row::Note(n) if n.starts_with("Loading")));
    }

    #[test]
    fn session_rows_and_buttons() {
        let now = Utc::now();
        let s = SessionView {
            name: "Assassin's Creed Origins".into(),
            copy: "Quiver install".into(),
            profile_name: Some("DLSS".into()),
            mods: vec!["DLSS v3 (current)".into(), "Skip intro".into()],
            started: Some(now - chrono::Duration::minutes(72)),
            total_secs: Some(3600 * 211),
            reported: Some(true),
            ..Default::default()
        };
        let d = PanelData { session: Load::Ready(s), ..Default::default() };
        let r = rows(PanelSection::Session, &d, now);
        assert!(r.contains(&Row::Fact { label: "Play time".into(), value: "This session 1 h 12 m  ·  Total 212 h 12 m".into() }));
        assert!(r.contains(&Row::Fact { label: "".into(), value: "Skip intro".into() }));
        assert_eq!(r.iter().filter(|r| r.focusable()).count(), 2, "Quit and Force quit");
        // Quit is there even while the session is still loading.
        assert!(rows(PanelSection::Session, &PanelData::default(), now).contains(&Row::Button(Button::ForceQuit)));
    }

    #[test]
    fn navigation() {
        let rows = vec![Row::Heading { title: "x".into(), detail: None }, Row::Hud(HudRow::Shown), Row::Note("n".into()), Row::Hud(HudRow::Preset), Row::Button(Button::Quit)];
        let adj = |r: &Row| matches!(r, Row::Hud(_));
        let mut n = Nav::default();
        assert_eq!(n.focused_row(&rows), None, "the side list has focus first");
        assert_eq!(n.apply(Input::Down, &rows, adj), Effect::Moved);
        assert_eq!(n.section, PanelSection::Mods);
        assert_eq!(n.apply(Input::Up, &rows, adj), Effect::Moved);
        assert_eq!(n.apply(Input::Up, &rows, adj), Effect::None, "top of the list");
        assert_eq!(n.apply(Input::Right, &rows, adj), Effect::Moved);
        assert_eq!(n.focused_row(&rows), Some(1), "the first focusable row");
        assert_eq!(n.apply(Input::Right, &rows, adj), Effect::Adjust(Row::Hud(HudRow::Shown), 1));
        assert_eq!(n.apply(Input::Activate, &rows, adj), Effect::Adjust(Row::Hud(HudRow::Shown), 0));
        assert_eq!(n.apply(Input::Down, &rows, adj), Effect::Moved);
        assert_eq!(n.focused_row(&rows), Some(3), "skips the note");
        n.apply(Input::Down, &rows, adj);
        assert_eq!(n.apply(Input::Down, &rows, adj), Effect::None, "end of the body");
        // Left on a button: back to the side list; B there closes.
        assert_eq!(n.apply(Input::Left, &rows, adj), Effect::Moved);
        assert_eq!(n.zone, Zone::Side);
        assert_eq!(n.apply(Input::Back, &rows, adj), Effect::Close);
        // The body remembers its row per section; LB/RB wrap.
        n.apply(Input::Activate, &rows, adj);
        assert_eq!(n.focused_row(&rows), Some(4));
        assert_eq!(n.apply(Input::PrevSection, &rows, adj), Effect::Moved);
        assert_eq!(n.section, PanelSection::Achievements);
        n.apply(Input::NextTab, &rows, adj);
        assert_eq!((n.section, n.focused_row(&rows)), (PanelSection::Session, Some(4)));
        // B in the body goes back to the side list.
        assert_eq!(n.apply(Input::Back, &rows, adj), Effect::Moved);
        assert_eq!(n.zone, Zone::Side);
        // A click focuses a row directly.
        n.focus_row(&rows, 3);
        assert_eq!(n.focused_row(&rows), Some(3));
        // Opening on a section focuses its body.
        let mut n = Nav::default();
        n.open(Some(PanelSection::Performance));
        assert_eq!((n.section, n.zone), (PanelSection::Performance, Zone::Body));
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
        assert_eq!(cycle(&PRESETS.map(|p| p.0), &"full", 1), "clock");
        assert_eq!(cycle(&PRESETS.map(|p| p.0), &"fps", -1), "clock");
        assert_eq!(step_scale(1.0, 1), 1.25);
        assert_eq!(step_scale(2.5, 1), 2.5);
        assert_eq!(step_scale(2.5, 0), 0.75, "A wraps");
        assert_eq!(step_scale(0.75, -1), 0.75);
    }

    #[test]
    fn keys() {
        assert_eq!(key_input("tab", true, false), Some(Input::PrevSection));
        assert_eq!(key_input("]", false, false), Some(Input::NextSection));
        assert_eq!(key_input("escape", false, false), Some(Input::Back));
        assert_eq!(key_input("enter", false, false), Some(Input::Activate));
        assert_eq!(key_input("f2", true, false), None, "Shift+F2 is the panel's own close, handled before");
    }

    #[test]
    fn performance_rows_say_when_settings_apply() {
        let view = HudSettingsView {
            scope: Default::default(),
            set: Default::default(),
            effective: quiver_api::perf::HudConfig::default(),
            revision: 1,
        };
        let d = PanelData { hud: Load::Ready(HudState { status: None, settings: view }), ..Default::default() };
        let r = rows(PanelSection::Performance, &d, Utc::now());
        assert!(r.contains(&Row::Hud(HudRow::Enable)) && !r.contains(&Row::Hud(HudRow::Shown)));
        assert!(r.contains(&Row::Heading { title: "HUD".into(), detail: Some("next launch".into()) }));
        assert_eq!(r[0], Row::Metrics);
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
