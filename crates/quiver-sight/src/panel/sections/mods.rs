//! The Mods and Tweaks sections' model, without GPUI (design/SIGHT.md §9 "Mods section", the
//! SightMods and docked mock-ups): what each lists, the session's changes ([`SessionLog`]),
//! the change chip ([`Chip`]) and A/B compare ([`compare_with`]). The drawing is in
//! [`super::mods_view`].
//!
//! - **Mods**: a list of the running profile's option surfaces (each mod's, the game's own
//!   settings, runtimes such as DXVK), each saying where its changes take effect. The picked
//!   surface's pages are tabs (LT / RT), its presets a picker, its options rows: label and help
//!   on the left, the control on the right, the outcome badge (LIVE, NEXT AREA LOAD, NEXT
//!   LAUNCH) and a result line. Beside them: the mod's readouts ("DLSS right now", its `state`
//!   and `text` metrics), this session's changes (Revert this session, Session only), and the
//!   options the mod marks dangerous ("Locked in game": the app changes those). Saved changes
//!   that wait for the next launch get a bar with Undo.
//! - **Tweaks**: Quiver's platform surface at the running profile's scope, paged the same way.
//!
//! X resets the focused option to its default (the hint bar shows the default and range).

use std::time::{Duration, Instant};

use quiver_api::options::{Apply, LiveChannel, OpOutcome, OptionDef, OptionOp, OptionState, Outcome, PageState, SurfaceKind, SurfaceState, same_value};
use quiver_ui::options::{Tone, outcome_text, pending_text, short_moment, short_value};
use serde_json::Value;

use crate::panel::state::{Act, Load, Nav, PanelData, Row, adjustable, shown_in_game, status};

/// The surface key the Tweaks section's rows and drafts go under.
pub const TWEAKS: &str = "tweaks";

// ---- Surfaces and pages -----------------------------------------------------------------------

/// A row for the option here: shown in game and not marked dangerous ("Locked in game").
pub fn is_row(st: &OptionState) -> bool {
    shown_in_game(st) && st.def.danger.is_none()
}

/// The surface's pages that have something to change in game (a surface without pages: one
/// page of everything).
pub fn pages(s: &SurfaceState) -> Vec<PageState> {
    let mut list: Vec<PageState> = s.pages.clone();
    if list.is_empty() && !s.options.is_empty() {
        list.push(PageState { id: "all".into(), title: s.title.clone(), groups: vec![quiver_api::options::GroupState { title: String::new(), options: s.options.keys().cloned().collect() }] });
    }
    list.retain(|p| p.groups.iter().flat_map(|g| &g.options).any(|id| s.options.get(id).is_some_and(is_row)));
    list
}

/// The options the mod marks dangerous: (label, why). The app changes those.
pub fn locked(s: &SurfaceState) -> Vec<(String, String)> {
    let mut ids: Vec<&String> = s.pages.iter().flat_map(|p| p.groups.iter().flat_map(|g| &g.options)).collect();
    if ids.is_empty() {
        ids = s.options.keys().collect();
    }
    ids.into_iter()
        .filter_map(|id| s.options.get(id).filter(|st| shown_in_game(st) && st.def.danger.is_some()).map(|st| (st.def.label_or(id).to_owned(), st.def.danger.clone().unwrap_or_default())))
        .collect()
}

/// "DLSS v3".
pub fn surface_title(s: &SurfaceState) -> String {
    match (&s.version_label, &s.version) {
        (Some(l), _) | (None, Some(l)) => format!("{} {l}", s.title),
        _ => s.title.clone(),
    }
}

/// Where a surface's changes take effect, for its line in the list: "live · SDK connected",
/// "live · re-reads aco_dlss.ini", "ACO.ini · next launch".
pub fn surface_where(s: &SurfaceState) -> String {
    let file = s.files.first().map(|f| f.rel.rsplit(['/', '\\']).next().unwrap_or(&f.rel).to_owned()).filter(|f| !f.is_empty());
    match (s.live.channel, s.live.connected) {
        (LiveChannel::Sdk, true) => "live · SDK connected".into(),
        (LiveChannel::Sdk, false) => "SDK not connected · next launch".into(),
        (LiveChannel::File, _) => match file {
            Some(f) => format!("live · re-reads {f}"),
            None => "live · re-reads its file".into(),
        },
        (LiveChannel::None, _) => match file {
            Some(f) => format!("{f} · next launch"),
            None => "next launch".into(),
        },
    }
}

/// The surface's live changes reach the running game now.
pub fn reaches_game(s: &SurfaceState) -> bool {
    match s.live.channel {
        LiveChannel::Sdk => s.live.connected,
        LiveChannel::File => true,
        LiveChannel::None => s.kind == SurfaceKind::Platform,
    }
}

/// The preset the values match now, if any.
pub fn preset_now(s: &SurfaceState) -> Option<usize> {
    s.presets.iter().position(|p| !p.set.is_empty() && p.set.iter().all(|(id, v)| s.options.get(id).is_some_and(|st| same_value(&st.value, v))))
}

fn surface_rows(s: &SurfaceState, nav: &Nav, out: &mut Vec<Row>, row: impl Fn(&str) -> Row) {
    if !s.presets.is_empty() {
        out.push(Row::Preset { surface: s.key.clone() });
    }
    let pages = pages(s);
    let Some(p) = pages.get(nav.pages.page(&s.key, pages.len())) else {
        out.push(Row::Note("Nothing here to change in game: the app has the rest.".into()));
        return;
    };
    let groups: Vec<(&str, Vec<&String>)> =
        p.groups.iter().map(|g| (g.title.as_str(), g.options.iter().filter(|id| s.options.get(*id).is_some_and(is_row)).collect::<Vec<_>>())).filter(|(_, ids)| !ids.is_empty()).collect();
    let headed = groups.len() > 1;
    for (title, ids) in groups {
        if headed && !title.is_empty() && title != p.title {
            out.push(Row::Heading { title: title.to_owned(), detail: None });
        }
        out.extend(ids.into_iter().map(|id| row(id)));
    }
}

/// The surface Mods shows (the list's pick, kept in range).
pub fn surface<'a>(d: &'a PanelData, nav: &Nav) -> Option<&'a SurfaceState> {
    let list = d.options.ready()?;
    list.get(nav.pages.surface.min(list.len().checked_sub(1)?))
}

/// The Mods section's rows, in focus order: the preset picker, the page's options, then the
/// buttons (Undo for next-launch changes, Revert this session, Session only).
pub fn rows(d: &PanelData, nav: &Nav, log: &SessionLog) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(r) = status(&d.options, "mod options") {
        out.push(r);
        return out;
    }
    let Some(s) = surface(d, nav) else {
        let no_mods = d.session.ready().is_some_and(|s| s.mods.is_empty());
        out.push(Row::Note(if no_mods {
            "This profile has no mods: it's a setup of the game's settings. Tweaks has the ones Quiver can change in game.".into()
        } else {
            "No mod in this profile has options to change in game.".into()
        }));
        return out;
    };
    let key = s.key.clone();
    surface_rows(s, nav, &mut out, |id| Row::Opt { surface: key.clone(), id: id.to_owned() });
    if !log.waiting(false).is_empty() {
        out.push(Row::Act(Act::UndoRestart { tweaks: false }));
    }
    if log.count(false) > 0 {
        out.push(Row::Act(Act::RevertSession));
    }
    if d.session.ready().is_some() {
        out.push(Row::Act(Act::SessionOnly));
    }
    out
}

/// The Tweaks section's rows: the preset picker, the page's tweaks, Undo.
pub fn tweak_rows(d: &PanelData, nav: &Nav, log: &SessionLog) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(r) = status(&d.tweaks, "tweaks") {
        out.push(r);
        return out;
    }
    let Load::Ready(s) = &d.tweaks else { return out };
    surface_rows(s, nav, &mut out, |id| Row::Tweak { id: id.to_owned() });
    if !log.waiting(true).is_empty() {
        out.push(Row::Act(Act::UndoRestart { tweaks: true }));
    }
    out
}

// ---- What a row says --------------------------------------------------------------------------

/// The outcome badge: LIVE, NEXT AREA LOAD (an `at` moment), NEXT LAUNCH. A live option of a
/// surface that can't reach the running game says NEXT LAUNCH.
pub fn badge(st: &OptionState, s: &SurfaceState) -> (String, Tone) {
    match st.def.apply {
        Apply::Live if !reaches_game(s) => ("NEXT LAUNCH".into(), Tone::Warn),
        Apply::Live => match &st.def.latency {
            Some(l) => (format!("LIVE · {}", quiver_ui::options::short_latency(l)).to_uppercase(), Tone::Ok),
            None => ("LIVE".into(), Tone::Ok),
        },
        Apply::At => {
            let m = short_moment(st.def.at.as_deref().unwrap_or("a moment")).to_uppercase();
            (if m.starts_with("NEXT ") { m } else { format!("AT {m}") }, Tone::Warn)
        }
        Apply::Restart | Apply::Launch => ("NEXT LAUNCH".into(), Tone::Warn),
    }
}

/// "0.2 s ago", "12 s ago", "3 min ago".
pub fn ago(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 10.0 {
        format!("{s:.1} s ago")
    } else if s < 90.0 {
        format!("{} s ago", s.round() as u64)
    } else {
        format!("{} min ago", (s / 60.0).round() as u64)
    }
}

/// The line under a row after this session changed it: what came of it, and when.
pub fn result_line(c: &Changed, who: &str, now: Instant) -> (String, Tone) {
    let age = ago(now.saturating_duration_since(c.at));
    match &c.outcome {
        Outcome::AppliedLive { channel: LiveChannel::Sdk, .. } if c.session_only => (format!("Live for this session only · {who} confirmed it {age}"), Tone::Ok),
        Outcome::AppliedLive { channel: LiveChannel::Sdk, .. } => (format!("Applied live · {who} confirmed it {age}"), Tone::Ok),
        Outcome::AppliedLive { .. } if who.is_empty() => (format!("Applied live · {age}"), Tone::Ok),
        Outcome::AppliedLive { latency: Some(l), .. } => (format!("Applied live · {who} picks it up within {l} · {age}"), Tone::Ok),
        Outcome::AppliedLive { .. } => (format!("Applied live · {who} re-reads its file · {age}"), Tone::Ok),
        Outcome::SavedAt { at } => (format!("Saved · applies at {}", short_moment(at)), Tone::Warn),
        o => outcome_text(o),
    }
}

/// The line under a row: an answer pending ("Saving…"), else this session's change, else a
/// saved value the game hasn't picked up, else a higher tweak scope that overrides it.
pub fn row_line(st: &OptionState, saving: Option<&(String, Tone)>, change: Option<&Changed>, who: &str, now: Instant) -> Option<(String, Tone)> {
    if let Some(s) = saving {
        return Some(s.clone());
    }
    if let Some(c) = change {
        return Some(result_line(c, who, now));
    }
    if let Some(t) = pending_text(&st.pending) {
        return Some((t, Tone::Warn));
    }
    st.tweak
        .as_ref()
        .and_then(|t| t.overridden_by)
        .map(|by| (format!("The {} scope sets it too, so this change doesn't reach the game", by.label().to_lowercase()), Tone::Warn))
}

/// The hint bar's line for a focused option: "Default 1.0× · range 1–5×".
pub fn range_detail(st: &OptionState) -> Option<String> {
    let def = &st.def;
    let mut parts = Vec::new();
    let default = def.default.clone().filter(|v| !v.is_null()).unwrap_or_else(|| st.default.clone());
    if !default.is_null() {
        parts.push(format!("Default {}", short_value(def, &default)));
    }
    if let (Some(a), Some(b)) = (def.min, def.max) {
        // The unit once: "range 1–5 ×", not "1 ×–5 ×".
        let bare = OptionDef { unit: None, ..def.clone() };
        let num = |x: f64| if def.ty == quiver_api::options::OptionType::Int { Value::from(x.round() as i64) } else { Value::from(x) };
        let unit = def.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
        parts.push(format!("range {}–{}{unit}", short_value(&bare, &num(a)), short_value(&bare, &num(b))));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The option's default, for X (`None`: it has none, or it's there already).
pub fn reset_op(surface: &str, id: &str, st: &OptionState) -> Option<OptionOp> {
    let d = st.def.default.clone().filter(|v| !v.is_null()).unwrap_or_else(|| st.default.clone());
    if !adjustable(st) || (!d.is_null() && same_value(&d, &st.value)) {
        return None;
    }
    Some(OptionOp::Reset { surface: surface.to_owned(), id: id.to_owned() })
}

// ---- The session's changes ------------------------------------------------------------------

/// One option this session changed in game (Sight's own changes; the app's aren't here).
#[derive(Debug, Clone, PartialEq)]
pub struct Changed {
    /// Surface key (Tweaks: the platform surface's) and option id.
    pub surface: String,
    pub id: String,
    pub tweaks: bool,
    pub label: String,
    /// Before the session's first change (what Undo and the session's revert go back to),
    /// before the last one (what A/B compare flips to), and now.
    pub first: Value,
    pub prev: Value,
    pub after: Value,
    pub first_text: String,
    pub prev_text: String,
    pub after_text: String,
    pub outcome: Outcome,
    pub at: Instant,
    /// Set with `live: only`: the game has it, the profile doesn't.
    pub session_only: bool,
}

impl Changed {
    /// It waits for the next launch (the Restart bar).
    pub fn waits(&self) -> bool {
        matches!(self.outcome, Outcome::SavedRestart { .. })
    }
}

/// The session's changes made in game, newest last. Kept across opens of the panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionLog {
    pub changes: Vec<Changed>,
    /// "Session only" is on: Mods' changes go out `live: only` (not saved).
    pub session_only: bool,
}

impl SessionLog {
    pub fn get(&self, tweaks: bool, surface: &str, id: &str) -> Option<&Changed> {
        self.changes.iter().find(|c| c.tweaks == tweaks && c.surface == surface && c.id == id)
    }

    /// How many options of Mods (`tweaks` false) or Tweaks this session changed.
    pub fn count(&self, tweaks: bool) -> usize {
        self.changes.iter().filter(|c| c.tweaks == tweaks).count()
    }

    /// The saved changes that wait for the next launch.
    pub fn waiting(&self, tweaks: bool) -> Vec<&Changed> {
        self.changes.iter().filter(|c| c.tweaks == tweaks && c.waits()).collect()
    }

    /// Notes what came of one change, and returns its chip. An unchanged value notes nothing; a
    /// rejected one only gets a chip; a value back where the session started is no change any
    /// more.
    pub fn record(&mut self, tweaks: bool, label: &str, def: &OptionDef, o: &OpOutcome, session_only: bool, now: Instant) -> Option<Chip> {
        if o.outcome == Outcome::Unchanged {
            return None;
        }
        let before = o.before.clone().unwrap_or(Value::Null);
        let after = o.after.clone().unwrap_or(Value::Null);
        let chip = Chip { label: label.to_owned(), before: short_value(def, &before), after: short_value(def, &after), when: ChipWhen::of(&o.outcome, session_only), at: now, comparing: false };
        if matches!(o.outcome, Outcome::Rejected { .. }) {
            return Some(chip);
        }
        let i = self.changes.iter().position(|c| c.tweaks == tweaks && c.surface == o.surface && c.id == o.id);
        let (first, first_text) = match i {
            Some(i) => (self.changes[i].first.clone(), self.changes[i].first_text.clone()),
            None => (before.clone(), short_value(def, &before)),
        };
        if let Some(i) = i {
            self.changes.remove(i);
        }
        if !first.is_null() && same_value(&first, &after) {
            return Some(chip);
        }
        self.changes.push(Changed {
            surface: o.surface.clone(),
            id: o.id.clone(),
            tweaks,
            label: label.to_owned(),
            first,
            prev: before,
            after,
            first_text,
            prev_text: chip.before.clone(),
            after_text: chip.after.clone(),
            outcome: o.outcome.clone(),
            at: now,
            session_only,
        });
        Some(chip)
    }

    /// Undo for the changes that wait for the next launch: each back to its first value.
    pub fn undo_ops(&self, tweaks: bool) -> Vec<OptionOp> {
        self.waiting(tweaks).into_iter().map(back_op).collect()
    }

    /// "Session only" for the changes saved so far: back in the profile to the first values
    /// (`live: no`, the game isn't touched), and the new values again for the running session
    /// only (`live: only`).
    pub fn session_only_ops(&self) -> (Vec<OptionOp>, Vec<OptionOp>) {
        let saved: Vec<&Changed> = self.changes.iter().filter(|c| !c.tweaks && !c.session_only).collect();
        let back = saved.iter().map(|c| back_op(c)).collect();
        let live = saved.iter().map(|c| OptionOp::Set { surface: c.surface.clone(), id: c.id.clone(), value: c.after.clone() }).collect();
        (back, live)
    }

    /// After "Revert this session": Mods' changes are gone, and so is the mode.
    pub fn reverted(&mut self) {
        self.changes.retain(|c| c.tweaks);
        self.session_only = false;
    }

    /// After "Session only": the saved changes are the session's only.
    pub fn made_session_only(&mut self) {
        for c in self.changes.iter_mut().filter(|c| !c.tweaks) {
            c.session_only = true;
        }
        self.session_only = true;
    }
}

fn back_op(c: &Changed) -> OptionOp {
    if c.first.is_null() {
        OptionOp::Reset { surface: c.surface.clone(), id: c.id.clone() }
    } else {
        OptionOp::Set { surface: c.surface.clone(), id: c.id.clone(), value: c.first.clone() }
    }
}

// ---- The change chip ------------------------------------------------------------------------

/// When a change takes effect, as the chip says it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChipWhen {
    Live { session_only: bool },
    NextLaunch,
    At(String),
    Later(String),
    Rejected(String),
}

impl ChipWhen {
    pub fn of(o: &Outcome, session_only: bool) -> ChipWhen {
        match o {
            Outcome::AppliedLive { .. } => ChipWhen::Live { session_only },
            Outcome::SavedRestart { .. } => ChipWhen::NextLaunch,
            Outcome::SavedAt { at } => ChipWhen::At(short_moment(at)),
            Outcome::Rejected { reason } => ChipWhen::Rejected(reason.clone()),
            Outcome::SavedPending { .. } | Outcome::TimedOut => ChipWhen::Later("when the mod picks it up".into()),
            Outcome::Dormant { reason } => ChipWhen::Rejected(reason.clone()),
            Outcome::Unchanged => ChipWhen::Later("unchanged".into()),
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            ChipWhen::Live { .. } => Tone::Ok,
            ChipWhen::Rejected(_) => Tone::Bad,
            _ => Tone::Warn,
        }
    }
}

/// The chip at the top of the game area after a change (the docked proposal): "Motion blur
/// On → Off · live, 0.2 s ago", fading after a few seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub label: String,
    pub before: String,
    pub after: String,
    pub when: ChipWhen,
    pub at: Instant,
    /// Y is held: the game shows `after` (the compared value) until it's let go.
    pub comparing: bool,
}

/// Fully shown this long, then fading out over [`CHIP_FADE`].
pub const CHIP_HOLD: Duration = Duration::from_millis(3500);
pub const CHIP_FADE: Duration = Duration::from_millis(700);

impl Chip {
    pub fn opacity(&self, now: Instant) -> f32 {
        if self.comparing {
            return 1.0;
        }
        let t = now.saturating_duration_since(self.at);
        if t <= CHIP_HOLD {
            1.0
        } else {
            (1.0 - (t - CHIP_HOLD).as_secs_f32() / CHIP_FADE.as_secs_f32()).max(0.0)
        }
    }

    pub fn alive(&self, now: Instant) -> bool {
        self.opacity(now) > 0.0
    }

    /// "· live, 0.2 s ago", "· next launch", "· at area load", "· not applied: …".
    pub fn tail(&self, now: Instant) -> String {
        if self.comparing {
            return "· comparing, let go of Y to go back".into();
        }
        match &self.when {
            ChipWhen::Live { session_only: true } => format!("· live for this session, {}", ago(now.saturating_duration_since(self.at))),
            ChipWhen::Live { .. } => format!("· live, {}", ago(now.saturating_duration_since(self.at))),
            ChipWhen::NextLaunch => "· next launch".into(),
            ChipWhen::At(m) => format!("· at {m}"),
            ChipWhen::Later(w) => format!("· {w}"),
            ChipWhen::Rejected(r) => format!("· not applied: {r}"),
        }
    }
}

// ---- A/B compare ----------------------------------------------------------------------------

/// What holding Y on a live option flips it to: the value before this session's last change
/// to it, else its default; `None` when it can't be compared live here (not live, the game
/// can't be reached, nothing different to show).
pub fn compare_with(st: &OptionState, s: &SurfaceState, change: Option<&Changed>, current: &Value) -> Option<Value> {
    if !adjustable(st) || st.def.apply != Apply::Live || !reaches_game(s) || s.kind == SurfaceKind::Platform {
        return None;
    }
    let differs = |v: &Value| !v.is_null() && !same_value(v, current);
    change.map(|c| c.prev.clone()).filter(differs).or_else(|| Some(st.default.clone()).filter(differs))
}

/// Y held this long compares; a shorter press pins (to the quick menu).
pub const HOLD: Duration = Duration::from_millis(350);

/// An A/B compare in progress: the option shows `showing` while Y is held, then goes back to
/// `back_to`.
#[derive(Debug, Clone, PartialEq)]
pub struct Compare {
    pub surface: String,
    pub id: String,
    pub showing: Value,
    pub back_to: Value,
}

/// What a Y press turns into, by how long it was held and what's focused.
#[derive(Debug, Clone, PartialEq)]
pub enum YPress {
    /// Released before [`HOLD`]: pin the focused row.
    Pin,
    /// Held on a live option: compare (until released).
    Compare,
    /// Held on something that can't be compared: nothing.
    Nothing,
}

pub fn y_press(held: Duration, comparable: bool) -> YPress {
    match (held >= HOLD, comparable) {
        (false, _) => YPress::Pin,
        (true, true) => YPress::Compare,
        (true, false) => YPress::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::state::{Effect, SessionView};
    use quiver_api::options::{GroupState, LiveInfo, OptionType, PresetInfo};
    use quiver_ui::input::Input;
    use serde_json::json;

    fn st(ty: OptionType, apply: Apply, v: Value) -> OptionState {
        OptionState { def: OptionDef { ty, apply, ..Default::default() }, value: v.clone(), default: v, visible: true, ..Default::default() }
    }

    fn surface(key: &str) -> SurfaceState {
        let mut s = SurfaceState { key: key.into(), title: "DLSS".into(), version_label: Some("v3".into()), live: LiveInfo { channel: LiveChannel::Sdk, connected: true, session: None }, ..Default::default() };
        s.options.insert("a".into(), st(OptionType::Bool, Apply::Live, json!(true)));
        let mut n = st(OptionType::Float, Apply::Live, json!(1.0));
        (n.def.min, n.def.max, n.def.step, n.def.default) = (Some(1.0), Some(5.0), Some(0.5), Some(json!(1.0)));
        n.def.label = Some("Crowd density".into());
        s.options.insert("n".into(), n);
        let mut d = st(OptionType::Bool, Apply::Restart, json!(false));
        d.def.danger = Some("resets your saves".into());
        d.def.label = Some("Rebuild cache".into());
        s.options.insert("d".into(), d);
        let mut hidden = st(OptionType::String, Apply::Live, json!("x"));
        hidden.def.overlay = Some(false);
        s.options.insert("h".into(), hidden);
        s.options.insert("r".into(), st(OptionType::Int, Apply::Restart, json!(2)));
        let g = |t: &str, ids: &[&str]| GroupState { title: t.into(), options: ids.iter().map(|s| s.to_string()).collect() };
        s.pages = vec![
            PageState { id: "p1".into(), title: "Upscaling".into(), groups: vec![g("", &["a", "n"])] },
            PageState { id: "p2".into(), title: "Debug".into(), groups: vec![g("", &["d", "h"])] },
            PageState { id: "p3".into(), title: "World".into(), groups: vec![g("Crowds", &["r"]), g("Other", &["a"])] },
        ];
        s.presets = vec![PresetInfo { id: "busy".into(), label: "Busy cities".into(), help: None, set: [("n".to_owned(), json!(1.0))].into() }];
        s
    }

    fn outcome(surface: &str, id: &str, before: Value, after: Value, o: Outcome) -> OpOutcome {
        OpOutcome { surface: surface.into(), id: id.into(), outcome: o, before: Some(before), after: Some(after) }
    }

    fn live() -> Outcome {
        Outcome::AppliedLive { channel: LiveChannel::Sdk, latency: None }
    }

    #[test]
    fn pages_presets_and_locked_options() {
        let s = surface("m/dlss");
        let p = pages(&s);
        assert_eq!(p.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(), ["Upscaling", "World"], "a page of only dangerous / hidden options isn't one");
        assert_eq!(locked(&s), [("Rebuild cache".to_owned(), "resets your saves".to_owned())]);
        assert_eq!(preset_now(&s), Some(0));
        let d = PanelData { options: Load::Ready(vec![s.clone()]), session: Load::Ready(SessionView::default()), ..Default::default() };
        let mut nav = Nav::default();
        let r = rows(&d, &nav, &SessionLog::default());
        assert_eq!(r, [Row::Preset { surface: "m/dlss".into() }, Row::Opt { surface: "m/dlss".into(), id: "a".into() }, Row::Opt { surface: "m/dlss".into(), id: "n".into() }, Row::Act(Act::SessionOnly)]);
        // The World page: two groups, so headings.
        nav.pages.step("m/dlss", 2, 1);
        let r = rows(&d, &nav, &SessionLog::default());
        assert!(matches!(&r[1], Row::Heading { title, .. } if title == "Crowds"), "{r:?}");
        assert_eq!(r.iter().filter(|r| matches!(r, Row::Opt { .. })).count(), 2);
        // Pages wrap.
        assert!(nav.pages.step("m/dlss", 2, 1));
        assert_eq!(nav.pages.page("m/dlss", 2), 0);
        assert!(!nav.pages.step("one", 1, 1), "one page: LT / RT do nothing");
        assert_eq!(surface_where(&s), "live · SDK connected");
        assert_eq!(surface_title(&s), "DLSS v3");
        // Nothing yet, or nothing at all.
        assert!(matches!(&rows(&PanelData::default(), &nav, &SessionLog::default())[0], Row::Note(n) if n.starts_with("Loading")));
        let none = PanelData { options: Load::Ready(vec![]), session: Load::Ready(SessionView::default()), ..Default::default() };
        assert!(matches!(&rows(&none, &nav, &SessionLog::default())[0], Row::Note(n) if n.starts_with("This profile has no mods")));
    }

    #[test]
    fn badges_and_lines() {
        let mut s = surface("m/dlss");
        let n = s.options["n"].clone();
        assert_eq!(badge(&n, &s), ("LIVE".into(), Tone::Ok));
        let mut at = n.clone();
        (at.def.apply, at.def.at) = (Apply::At, Some("next area load (the game streams areas in)".into()));
        assert_eq!(badge(&at, &s).0, "NEXT AREA LOAD");
        at.def.at = Some("swapchain creation".into());
        assert_eq!(badge(&at, &s).0, "AT SWAPCHAIN CREATION");
        assert_eq!(badge(&s.options["r"], &s).0, "NEXT LAUNCH");
        s.live.connected = false;
        assert_eq!(badge(&n, &s).0, "NEXT LAUNCH", "live, but the mod's SDK isn't connected");
        assert_eq!(range_detail(&n).as_deref(), Some("Default 1 · range 1–5"));
        assert_eq!(ago(Duration::from_millis(200)), "0.2 s ago");
        assert_eq!(ago(Duration::from_secs(42)), "42 s ago");
        assert_eq!(ago(Duration::from_secs(600)), "10 min ago");
        // X: back to the default; nothing to do there already, or for a dangerous option.
        let mut m = n.clone();
        m.value = json!(2.0);
        assert_eq!(reset_op("m/dlss", "n", &m), Some(OptionOp::Reset { surface: "m/dlss".into(), id: "n".into() }));
        assert_eq!(reset_op("m/dlss", "n", &n), None);
        assert_eq!(reset_op("m/dlss", "d", &s.options["d"]), None);
    }

    #[test]
    fn session_log_chips_undo_and_session_only() {
        let t0 = Instant::now();
        let mut log = SessionLog::default();
        let def = OptionDef { ty: OptionType::Float, precision: Some(2), ..Default::default() };
        let c = log.record(false, "Render scale", &def, &outcome("m/dlss", "scale", json!(0.5), json!(0.58), Outcome::SavedRestart { running: true }), false, t0).unwrap();
        assert_eq!((c.before.as_str(), c.after.as_str(), c.when.clone()), ("0.50", "0.58", ChipWhen::NextLaunch));
        assert_eq!(c.tail(t0), "· next launch");
        let b = OptionDef { ty: OptionType::Bool, ..Default::default() };
        let c = log.record(false, "Motion blur", &b, &outcome("m/dlss", "blur", json!(true), json!(false), live()), false, t0).unwrap();
        assert_eq!(c.tail(t0 + Duration::from_millis(200)), "· live, 0.2 s ago");
        assert_eq!((log.count(false), log.waiting(false).len()), (2, 1));
        // Undo: the next-launch change back to where it started.
        assert_eq!(log.undo_ops(false), [OptionOp::Set { surface: "m/dlss".into(), id: "scale".into(), value: json!(0.5) }]);
        // A second change keeps the first value; changing it back removes it.
        log.record(false, "Motion blur", &b, &outcome("m/dlss", "blur", json!(false), json!(true), live()), false, t0);
        assert_eq!(log.count(false), 1, "back where it started: no change");
        log.record(false, "Motion blur", &b, &outcome("m/dlss", "blur", json!(true), json!(false), live()), false, t0);
        log.record(false, "Motion blur", &b, &outcome("m/dlss", "blur", json!(false), json!(false), Outcome::Unchanged), false, t0);
        let c = log.get(false, "m/dlss", "blur").unwrap();
        assert_eq!((c.first.clone(), c.prev.clone(), c.after.clone()), (json!(true), json!(true), json!(false)));
        // Rejected: a chip, no change.
        let r = log.record(false, "Motion blur", &b, &outcome("m/dlss", "x", json!(1), json!(2), Outcome::Rejected { reason: "no".into() }), false, t0).unwrap();
        assert_eq!(r.when.tone(), Tone::Bad);
        assert!(log.get(false, "m/dlss", "x").is_none());
        // Tweaks are counted apart.
        log.record(true, "Frame-rate cap", &def, &outcome("quiver/tweaks", "cap", json!(162), json!(120), Outcome::SavedRestart { running: true }), false, t0);
        assert_eq!((log.count(true), log.waiting(true).len(), log.count(false)), (1, 1, 2));
        // Session only: the profile goes back, the game keeps them.
        let (back, live_ops) = log.session_only_ops();
        assert_eq!(back.len(), 2);
        assert!(live_ops.contains(&OptionOp::Set { surface: "m/dlss".into(), id: "blur".into(), value: json!(false) }));
        log.made_session_only();
        assert!(log.session_only && log.changes.iter().filter(|c| !c.tweaks).all(|c| c.session_only));
        assert_eq!(log.session_only_ops().0.len(), 0, "nothing saved to take back any more");
        // Revert this session: Mods' changes go, Tweaks' stay.
        log.reverted();
        assert_eq!((log.count(false), log.count(true), log.session_only), (0, 1, false));
        // The rows get the buttons.
        let mut log = SessionLog::default();
        log.record(false, "Render scale", &def, &outcome("m/dlss", "scale", json!(0.5), json!(0.58), Outcome::SavedRestart { running: true }), false, t0);
        let d = PanelData { options: Load::Ready(vec![surface("m/dlss")]), session: Load::Ready(SessionView::default()), ..Default::default() };
        let r = rows(&d, &Nav::default(), &log);
        assert!(r.ends_with(&[Row::Act(Act::UndoRestart { tweaks: false }), Row::Act(Act::RevertSession), Row::Act(Act::SessionOnly)]), "{r:?}");
    }

    #[test]
    fn chip_lifecycle() {
        let t0 = Instant::now();
        let mut c = Chip { label: "Motion blur".into(), before: "On".into(), after: "Off".into(), when: ChipWhen::Live { session_only: false }, at: t0, comparing: false };
        assert_eq!(c.opacity(t0), 1.0);
        assert_eq!(c.opacity(t0 + CHIP_HOLD), 1.0);
        let half = c.opacity(t0 + CHIP_HOLD + CHIP_FADE / 2);
        assert!((half - 0.5).abs() < 0.01, "{half}");
        assert!(!c.alive(t0 + CHIP_HOLD + CHIP_FADE));
        c.comparing = true;
        assert_eq!(c.opacity(t0 + Duration::from_secs(60)), 1.0, "shown for as long as Y is held");
        assert!(c.tail(t0).contains("let go of Y"));
        assert_eq!(ChipWhen::of(&Outcome::SavedAt { at: "next area load (x)".into() }, false), ChipWhen::At("next area load".into()));
    }

    #[test]
    fn a_b_compare() {
        let s = surface("m/dlss");
        let a = &s.options["a"];
        // Nothing changed this session and at its default: nothing to compare with.
        assert_eq!(compare_with(a, &s, None, &json!(true)), None);
        // Changed away from the default: the default.
        assert_eq!(compare_with(a, &s, None, &json!(false)), Some(json!(true)));
        // After a change: what it was before it.
        let mut log = SessionLog::default();
        let b = OptionDef { ty: OptionType::Bool, ..Default::default() };
        log.record(false, "a", &b, &outcome("m/dlss", "a", json!(true), json!(false), live()), false, Instant::now());
        assert_eq!(compare_with(a, &s, log.get(false, "m/dlss", "a"), &json!(false)), Some(json!(true)));
        // Not live, or the game can't be reached: no compare.
        assert_eq!(compare_with(&s.options["r"], &s, None, &json!(3)), None);
        let mut off = s.clone();
        off.live.connected = false;
        assert_eq!(compare_with(a, &off, None, &json!(false)), None);
        // Y: a tap pins, a hold compares (or does nothing where it can't).
        assert_eq!(y_press(Duration::from_millis(120), true), YPress::Pin);
        assert_eq!(y_press(HOLD, true), YPress::Compare);
        assert_eq!(y_press(Duration::from_secs(1), false), YPress::Nothing);
    }

    #[test]
    fn surfaces_zone_and_pages_by_pad() {
        let mut s2 = surface("m/two");
        s2.title = "Game settings".into();
        s2.presets.clear();
        let d = PanelData { options: Load::Ready(vec![surface("m/dlss"), s2]), session: Load::Ready(SessionView::default()), ..Default::default() };
        let mut nav = Nav::default();
        nav.open(Some(quiver_api::sight::PanelSection::Mods));
        nav.pages.surfaces = 2;
        let adj = |_: &Row| false;
        let r = rows(&d, &nav, &SessionLog::default());
        // B from the rows: the surface list; Down there: the next surface; Right: its rows.
        assert_eq!(nav.apply(Input::Back, &r, adj), Effect::Moved);
        assert_eq!(nav.zone, crate::panel::state::Zone::Surfaces);
        assert_eq!(nav.focused_row(&r), None);
        assert_eq!(nav.apply(Input::Down, &r, adj), Effect::Surface);
        assert_eq!(nav.apply(Input::Down, &r, adj), Effect::None, "the end of the list");
        assert_eq!(super::surface(&d, &nav).map(|s| s.key.as_str()), Some("m/two"));
        assert_eq!(nav.apply(Input::Right, &r, adj), Effect::Moved);
        assert_eq!(nav.zone, crate::panel::state::Zone::Body);
        // Left on a row that doesn't adjust goes to the list too; B there closes.
        nav.apply(Input::Left, &r, adj);
        assert_eq!(nav.apply(Input::Back, &r, adj), Effect::Close);
        // LT / RT are the section's tabs.
        assert_eq!(nav.apply(Input::NextTab, &r, adj), Effect::Tab(1));
        // X and Y on the focused row.
        let mut nav = Nav::default();
        nav.open(Some(quiver_api::sight::PanelSection::Mods));
        let r = rows(&d, &nav, &SessionLog::default());
        nav.apply(Input::Down, &r, adj);
        assert_eq!(nav.apply(Input::PlayOptions, &r, adj), Effect::X(Some(Row::Opt { surface: "m/dlss".into(), id: "a".into() })));
        assert_eq!(nav.apply(Input::Menu, &r, adj), Effect::Pin(Some(Row::Opt { surface: "m/dlss".into(), id: "a".into() })));
    }
}
