//! The panel's Performance section (design canvas: SightPerformance, HudEditor; design/SIGHT.md
//! §14): the game's numbers with where each comes from, a 30-second frame-time graph, the
//! frame-rate cap and who enforces it, the HUD's preset, the latency card, a 30-second
//! performance log (X), and the HUD layout editor.
//!
//! This file is the model without GPUI (unit-tested below): what quiverd sent for the section
//! ([`PerfData`]), the controls and their values, the layout editor ([`Editor`]: metrics by
//! producer with a checkbox each, rows picked up with A and moved with ▲▼, the layout's
//! settings, saving an edited preset as a user layout), and what an input does ([`Act`]).
//! [`view`] draws it; [`link`] asks quiverd.
//!
//! **Input** is gamepad-first and follows the panel's rules: ▲▼ move, ◀▶ change the focused
//! value, A the next value or the action, B goes back one level (the editor's settings → its
//! preview → the section; the section → the side list), LB/RB switch sections. X saves the
//! log (in the editor: shows or hides the HUD now), Y saves an edited layout as a preset.
//! In the editor ◀▶ also move between its columns where there's nothing to change.

pub mod link;
pub mod view;

use quiver_api::options::{OptionState, same_value, scalar_text};
use quiver_api::perf::{
    Anchor, Captures, FrameTimes, HudConfig, HudLayout, HudLayoutInfo, HudPreview, HudScope, HudScopeRef, HudSettingsView, MetricInfo, MetricKind, MetricState, PerfValue,
    PresenterSetting, SourceKind, Stat, Visibility,
};
use quiver_api::tweaks::{Effect, Planned};
use quiver_ui::input::Input;
use quiver_ui::options::Tone;
use serde_json::{Value, json};

use super::super::hints::Glyph;
use super::super::state::{ANCHORS, Load, PRESETS, PanelData, SessionView, cycle, step_scale};
pub use link::{Reply, Req};

/// The tweak the cap card sets (Quiver Tweaks, design/TWEAKS.md).
pub const CAP_TWEAK: &str = "fps.cap";
/// The span of the graph and the log.
pub const SPAN_MS: u32 = 30_000;
/// The graph's slots (one per 100 ms).
pub const BUCKETS: u32 = 300;

/// What quiverd sent for this section, beside [`PanelData`]'s HUD state and metrics.
#[derive(Debug, Clone, Default)]
pub struct PerfData {
    pub frames: Option<FrameTimes>,
    /// The session's metric catalog (the editor's metric list).
    pub catalog: Load<Vec<MetricInfo>>,
    /// Presets and the user's layouts.
    pub layouts: Load<Vec<HudLayoutInfo>>,
    /// HUD settings at the editor's scope, when it isn't the game's (which `PanelData` has).
    pub scoped: Option<HudSettingsView>,
}

/// A focusable control of the section (top level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctl {
    /// The frame-rate cap (Quiver Tweaks' `fps.cap`).
    Cap,
    /// Hidden, or a preset (shown).
    Hud,
    /// The Sight layer in the game's launches (only while this launch has none and it's off).
    Enable,
    /// Opens the layout editor.
    Edit,
    /// The layer's opt-in Reflex timing reads (latency split, game to render).
    Timings,
}

/// The editor's columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zone {
    #[default]
    Metrics,
    Preview,
    Settings,
}

/// The editor's settings, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Preset,
    Position,
    Opacity,
    Scale,
    Rate,
    Captures,
    AppliesTo,
    Drawn,
}

pub const SETTINGS: [Setting; 8] = [Setting::Preset, Setting::Position, Setting::Opacity, Setting::Scale, Setting::Rate, Setting::Captures, Setting::AppliesTo, Setting::Drawn];

/// Where the editor's settings and saved layout go ("Applies to").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Applies {
    Profile,
    #[default]
    Game,
    All,
}

impl Applies {
    pub fn label(self) -> &'static str {
        match self {
            Applies::Profile => "Profile",
            Applies::Game => "Game",
            Applies::All => "All",
        }
    }

    /// The scope for a session; a profile scope without a running profile is the game's.
    pub fn scope(self, s: &SessionView) -> HudScopeRef {
        match (self, s.profile()) {
            (Applies::Profile, Some(p)) => HudScopeRef { game: Some(s.game), profile: Some(p.to_owned()) },
            (Applies::All, _) => HudScopeRef::global(),
            _ => HudScopeRef::of(s.game),
        }
    }

    /// The profile's scope when one runs (a change there always reaches this game, as the
    /// Tweaks section does), else the game's.
    pub fn default_for(s: &SessionView) -> Applies {
        if s.profile().is_some() { Applies::Profile } else { Applies::Game }
    }

    fn choices(s: &SessionView) -> Vec<Applies> {
        if s.profile().is_some() { vec![Applies::Profile, Applies::Game, Applies::All] } else { vec![Applies::Game, Applies::All] }
    }
}

/// Synthetic entries of the metric list: the graph's two series and the latency split.
pub const GRAPH_PRESENTED: &str = "graph:presented";
pub const GRAPH_RENDERED: &str = "graph:rendered";
pub const SPLIT_LATENCY: &str = "split:latency";
const SPLIT_PARTS: [&str; 4] = ["latency.split.sim", "latency.split.render", "latency.split.queue", "latency.split.gpu"];

/// A metric the editor lists.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub label: String,
    /// "number", "graph", "stacked bar", "text", "state", "bar".
    pub kind: &'static str,
    /// A short label for a new cell ("GPU").
    pub short: Option<String>,
}

/// The metrics of one producer.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub source: SourceKind,
    /// "Sight layer", "DLSS mod", "System · quiverd".
    pub title: String,
    /// "every present", "SDK · connected", "1 Hz".
    pub note: String,
    pub entries: Vec<Entry>,
}

/// The session's metrics grouped by who produces them now (`MetricInfo::live_producer`): the
/// Sight layer, each mod (by name), the system, Quiver Tweaks, Quiver. Only metrics with a
/// value in this session; a canonical metric stands in for the producer metric under it.
pub fn groups(catalog: &[MetricInfo]) -> Vec<Group> {
    let live = |m: &MetricInfo| matches!(m.state, MetricState::Live | MetricState::Stale);
    let has = |id: &str| catalog.iter().any(|m| m.descriptor.id == id && live(m));
    let mut out: Vec<(String, Group)> = Vec::new();
    let put = |key: String, source: SourceKind, title: String, note: &str, e: Entry, out: &mut Vec<(String, Group)>| {
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.entries.push(e),
            None => out.push((key, Group { source, title, note: note.into(), entries: vec![e] })),
        }
    };
    let layer = |out: &mut Vec<(String, Group)>, e: Entry| put("layer".into(), SourceKind::Layer, "Sight layer".into(), "every present", e, out);
    if has("frametime") || has("fps") {
        layer(&mut out, Entry { id: GRAPH_PRESENTED.into(), label: "Presented frame times".into(), kind: "graph", short: None });
    }
    for m in catalog {
        let d = &m.descriptor;
        if !live(m) || !matches!(d.kind, MetricKind::Gauge | MetricKind::State | MetricKind::Text) || d.visibility == Some(Visibility::Advanced) {
            continue;
        }
        // A producer's metric that fills a role shows as the canonical metric over it.
        if !m.canonical && !m.roles.is_empty() {
            continue;
        }
        if SPLIT_PARTS.contains(&d.id.as_str()) || matches!(d.id.as_str(), "frametime.base" | "fg.active" | "gpu.source" | "gpu.name") {
            continue;
        }
        let kind = match (d.kind, d.display) {
            (MetricKind::State, _) => "state",
            (MetricKind::Text, _) => "text",
            (_, Some(quiver_perf::descriptor::Display::Bar)) => "bar",
            _ => "number",
        };
        let e = Entry { id: d.id.clone(), label: d.label.clone().unwrap_or_else(|| d.id.clone()), kind, short: d.short.clone().filter(|s| s.chars().count() <= 6) };
        let producer = m.live_producer.clone().unwrap_or_else(|| m.producer.clone());
        let source = SourceKind::of_producer(&producer);
        match source {
            SourceKind::Layer => layer(&mut out, e),
            SourceKind::Mod => {
                let name = m.live_producer_label.clone().unwrap_or_else(|| producer.trim_start_matches("mod:").to_owned());
                put(producer.clone(), source, format!("{name} mod"), "SDK · connected", e, &mut out);
            }
            SourceKind::System => put("system".into(), source, "System · quiverd".into(), "1 Hz", e, &mut out),
            SourceKind::Tweaks => put("tweaks".into(), source, "Quiver Tweaks".into(), "launch", e, &mut out),
            SourceKind::Quiver => put("quiver".into(), source, "Quiver".into(), "session", e, &mut out),
        }
    }
    // The rendered series beside base FPS, the split beside its producer's latency.
    if has("fps.base") {
        layer(&mut out, Entry { id: GRAPH_RENDERED.into(), label: "Rendered frame times".into(), kind: "graph", short: None });
    }
    if let Some(m) = catalog.iter().find(|m| m.descriptor.id == SPLIT_PARTS[0] && live(m)) {
        let e = Entry { id: SPLIT_LATENCY.into(), label: "Latency split".into(), kind: "stacked bar", short: None };
        let producer = m.live_producer.clone().unwrap_or_default();
        match SourceKind::of_producer(&producer) {
            SourceKind::Mod => {
                let name = m.live_producer_label.clone().unwrap_or_else(|| producer.trim_start_matches("mod:").to_owned());
                put(producer.clone(), SourceKind::Mod, format!("{name} mod"), "SDK · connected", e, &mut out);
            }
            _ => layer(&mut out, e),
        }
    }
    let rank = |s: SourceKind| match s {
        SourceKind::Layer => 0,
        SourceKind::Mod => 1,
        SourceKind::System => 2,
        SourceKind::Tweaks => 3,
        SourceKind::Quiver => 4,
    };
    out.sort_by(|a, b| rank(a.1.source).cmp(&rank(b.1.source)).then(a.1.title.cmp(&b.1.title)));
    out.into_iter().map(|(_, g)| g).collect()
}

/// The list's focusable entries in order: (group, entry).
pub fn entries(groups: &[Group]) -> Vec<(usize, usize)> {
    groups.iter().enumerate().flat_map(|(g, gr)| (0..gr.entries.len()).map(move |e| (g, e))).collect()
}

// ---- Layout editing ------------------------------------------------------------------------

fn str_field<'a>(c: &'a Value, k: &str) -> Option<&'a str> {
    c.get(k).and_then(Value::as_str)
}

/// A cell that shows something (not text, a spacer or a separator).
fn is_content(c: &Value) -> bool {
    c.get("metric").is_some() || c.get("graph").is_some() || c.get("split").is_some()
}

/// Whether cell `c` shows the list entry `id`.
pub fn cell_shows(c: &Value, id: &str) -> bool {
    match id {
        GRAPH_PRESENTED => c.get("graph").is_some(),
        GRAPH_RENDERED => c.get("graph").is_some() && c.get("series").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x == "frametime.base")),
        SPLIT_LATENCY => c.get("split").is_some(),
        _ => str_field(c, "metric") == Some(id),
    }
}

fn is_mods_row(l: &HudLayout, i: usize) -> bool {
    l.row_options.get(i).and_then(|o| o.get("mods")).and_then(Value::as_bool).unwrap_or(false)
}

/// The layout shows entry `id` somewhere.
pub fn shows(l: &HudLayout, id: &str) -> bool {
    l.rows.iter().flatten().any(|c| cell_shows(c, id))
}

/// Adds or removes entry `e` (A on its checkbox). Removing drops every cell that shows it (the
/// rendered series from its graph) and rows left with nothing to show; adding puts it on a
/// new row at the end. Returns the rows removed (indexes before the change) and whether one
/// was added.
pub fn toggle(l: &mut HudLayout, e: &Entry) -> (Vec<usize>, bool) {
    if shows(l, &e.id) {
        for row in &mut l.rows {
            if e.id == GRAPH_RENDERED {
                for c in row.iter_mut().filter(|c| c.get("graph").is_some()) {
                    if let Some(s) = c.get_mut("series").and_then(Value::as_array_mut) {
                        s.retain(|x| x != "frametime.base");
                    }
                }
            } else {
                row.retain(|c| !cell_shows(c, &e.id));
            }
        }
        let gone: Vec<usize> = (0..l.rows.len()).filter(|i| !is_mods_row(l, *i) && !l.rows[*i].iter().any(is_content)).collect();
        for i in gone.iter().rev() {
            l.rows.remove(*i);
            if *i < l.row_options.len() {
                l.row_options.remove(*i);
            }
        }
        return (gone, false);
    }
    let cell = match e.id.as_str() {
        GRAPH_PRESENTED => json!({ "graph": "frametime", "series": ["frametime"], "rows": 2 }),
        GRAPH_RENDERED => {
            // Into the graph there is, else a graph of both.
            if let Some(c) = l.rows.iter_mut().flatten().find(|c| c.get("graph").is_some()) {
                match c.get_mut("series").and_then(Value::as_array_mut) {
                    Some(s) => s.push(json!("frametime.base")),
                    None => c["series"] = json!(["frametime", "frametime.base"]),
                }
                return (Vec::new(), false);
            }
            json!({ "graph": "frametime", "series": ["frametime", "frametime.base"], "rows": 2 })
        }
        SPLIT_LATENCY => json!({ "split": "latency", "parts": SPLIT_PARTS }),
        id => match &e.short {
            Some(s) => json!({ "metric": id, "short": s }),
            None => json!({ "metric": id }),
        },
    };
    l.rows.push(vec![cell]);
    if !l.row_options.is_empty() {
        l.row_options.push(json!({}));
    }
    (Vec::new(), true)
}

/// Moves row `i` one up (`d` < 0) or down; returns its new index.
pub fn move_row(l: &mut HudLayout, i: usize, d: i32) -> usize {
    let j = i as i32 + d.signum();
    if j < 0 || j as usize >= l.rows.len() || i >= l.rows.len() {
        return i;
    }
    let j = j as usize;
    l.rows.swap(i, j);
    if l.row_options.len() == l.rows.len() {
        l.row_options.swap(i, j);
    }
    j
}

/// The id and name an edited layout is saved under: an edited preset becomes `<id>-edited`,
/// "<Name> (edited)"; a user layout is saved over itself.
pub fn save_as(base: &str, base_name: &str, builtin: bool) -> (String, String) {
    if builtin {
        let name: String = format!("{base_name} (edited)").chars().take(32).collect();
        (format!("{base}-edited"), name)
    } else {
        (base.to_owned(), base_name.to_owned())
    }
}

/// The layout editor (HudEditor mock-up).
#[derive(Debug, Clone)]
pub struct Editor {
    /// The layout it started from, and whether that's a preset.
    pub base: String,
    pub base_name: String,
    pub builtin: bool,
    /// The draft.
    pub layout: HudLayout,
    pub edited: bool,
    pub zone: Zone,
    pub metric: usize,
    pub row: usize,
    pub setting: usize,
    /// A row picked up with A: the rows as they were, for B to put back.
    pub moving: Option<HudLayout>,
    pub applies: Applies,
    /// The last preview, and the layout it was asked for.
    pub preview: Option<(HudLayout, HudPreview)>,
    /// For each draft row, its row in the last preview's layout (`None`: added since).
    pub perm: Vec<Option<usize>>,
}

impl Editor {
    pub fn new(layout: HudLayout, applies: Applies) -> Editor {
        let perm = vec![None; layout.rows.len()];
        Editor {
            base: layout.id.clone(),
            base_name: layout.name.clone(),
            builtin: layout.builtin,
            layout,
            edited: false,
            zone: Zone::Metrics,
            metric: 0,
            row: 0,
            setting: 0,
            moving: None,
            applies,
            preview: None,
            perm,
        }
    }

    /// "Standard (edited)".
    pub fn title(&self) -> String {
        if self.edited { format!("{} (edited)", self.base_name) } else { self.base_name.clone() }
    }

    /// A preview arrived for `sent`: rows map one to one if it's the draft as it is now.
    pub fn previewed(&mut self, sent: HudLayout, p: HudPreview) {
        if sent.rows == self.layout.rows {
            self.perm = (0..self.layout.rows.len()).map(Some).collect();
        }
        self.preview = Some((sent, p));
    }

    /// The preview's model rows for the draft's row `i` (where the last preview put that row).
    pub fn model_rows(&self, i: usize) -> Option<std::ops::Range<usize>> {
        let (_, p) = self.preview.as_ref()?;
        let j = (*self.perm.get(i)?)?;
        let start = *p.rows.get(j)?;
        let end = p.rows.get(j + 1).copied().unwrap_or(p.model.rows.len());
        Some(start..end.max(start))
    }

    fn changed(&mut self) {
        self.edited = true;
    }
}

// ---- The section ------------------------------------------------------------------------------

/// What an input asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// To quiverd.
    Send(Req),
    /// A Quiver Tweak through the panel's tweak path (drafts, debounce, outcome on the row).
    SetTweak(String, Value),
    /// A line over the hint bar.
    Note(String, Tone),
}

/// What [`PerfSection::input`] did with an input.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Not the section's: the panel's navigation takes it (B to the side list, LB/RB).
    Pass,
    Done(Vec<Act>),
}

#[derive(Debug, Clone, Default)]
pub struct PerfSection {
    pub data: PerfData,
    /// Last cap delivery, including releases back to the starting value.
    pub cap_result: Option<quiver_api::options::Outcome>,
    /// The focused control (an index into [`PerfSection::ctls`]).
    pub focus: usize,
    pub editor: Option<Editor>,
    /// The layout asked for to open the editor.
    pub opening: Option<String>,
    /// What the HUD control was set to, until the HUD state says so.
    pub hud_draft: Option<String>,
}

/// The cap's choices: off, 30, 60, 120, the refresh rate − 3, and the current value when it's
/// another.
pub fn cap_choices(cur: &Value) -> Vec<Value> {
    let mut v = vec![json!(0), json!(30), json!(60), json!(120), json!("vrr")];
    if !v.iter().any(|x| same_value(x, cur)) && cur.as_f64().is_some() {
        let at = v.iter().position(|x| x.as_f64().is_none_or(|n| n > cur.as_f64().unwrap())).unwrap_or(v.len());
        v.insert(at, cur.clone());
    }
    v
}

pub fn cap_label(v: &Value) -> String {
    match v {
        Value::String(s) if s == "vrr" => "VRR −3".into(),
        v if v.as_f64() == Some(0.0) => "Off".into(),
        v => scalar_text(v),
    }
}

/// Who enforces the cap, in words, from what the next launch does with it and what the last
/// session saw (Quiver Tweaks' plan and effect), and the game's own Reflex cap.
pub fn cap_enforcer(st: Option<&OptionState>, value: &Value, reflex_cap: Option<&PerfValue>) -> String {
    let mut out = Vec::new();
    let off = value.as_f64() == Some(0.0);
    match st.and_then(|s| s.tweak.as_ref()).and_then(|t| t.planned.as_ref()) {
        _ if off => out.push("No cap from Quiver.".to_owned()),
        Some(Planned::Apply { mech, writes, .. }) => {
            let who = match mech.as_str() {
                "dxvk" => "DXVK's frame limiter".to_owned(),
                "env" if writes.iter().any(|w| w.contains("VKD3D")) => "vkd3d-proton's frame limiter".to_owned(),
                "layer" => "the Sight layer's limiter".to_owned(),
                m => m.to_owned(),
            };
            out.push(format!("Set by {who}, from the next launch."));
        }
        Some(Planned::Provided { by, option, value: provider_value, .. }) => {
            let setting = provider_value.as_ref().map(|v| format!("{option} = {}", scalar_text(v))).unwrap_or_else(|| option.clone());
            out.push(format!("Set by {by} ({setting}), so Quiver's limiters stay off."));
        }
        Some(Planned::Superseded { by }) => out.push(format!("Your own launch setting wins: {by}.")),
        Some(Planned::Blocked { reason }) => out.push(format!("Not applied: {reason}.")),
        Some(Planned::Unavailable { reason }) => out.push(format!("Not available: {reason}.")),
        Some(Planned::Default) | None => out.push("Applies from the next launch.".to_owned()),
    }
    if let Some(Effect::Measured { value }) = st.and_then(|s| s.tweak.as_ref()).and_then(|t| t.last.as_ref()) {
        out.push(format!("Last session: {value}."));
    }
    if let Some(c) = reflex_cap.filter(|v| !v.stale) {
        out.push(format!("The game's Reflex cap: {} fps.", c.text));
    }
    out.join(" ")
}

/// Reflex's mode in words ("On + Boost").
pub fn reflex_words(mode: Option<&PerfValue>) -> String {
    match mode.map(|v| v.text.as_str()) {
        Some("boost") => "On + Boost".into(),
        Some("on") => "On".into(),
        Some("off") => "Off".into(),
        Some(t) => t.to_owned(),
        None => "Not used by this game".into(),
    }
}

/// The HUD control's choices: hidden, the presets, the user's layouts.
pub fn hud_choices(layouts: &Load<Vec<HudLayoutInfo>>) -> Vec<String> {
    let mut v: Vec<String> = std::iter::once("hidden".to_owned()).chain(PRESETS.iter().map(|p| p.0.to_owned())).collect();
    if let Some(l) = layouts.ready() {
        v.extend(l.iter().filter(|i| !i.builtin && i.error.is_none()).map(|i| i.id.clone()));
    }
    v
}

/// A layout's name ("Standard", a user layout's own).
pub fn layout_name(id: &str, layouts: &Load<Vec<HudLayoutInfo>>) -> String {
    if id == "hidden" {
        return "Hidden".into();
    }
    PRESETS
        .iter()
        .find(|p| p.0 == id)
        .map(|p| p.1.to_owned())
        .or_else(|| layouts.ready().and_then(|l| l.iter().find(|i| i.id == id)).map(|i| i.name.clone()))
        .unwrap_or_else(|| id.to_owned())
}

/// A metric's value at `stat` (else its last, else any).
pub fn metric<'a>(d: &'a PanelData, id: &str, stat: Stat) -> Option<&'a PerfValue> {
    let m = d.metrics.get(id)?;
    m.get(&stat).or_else(|| m.get(&Stat::Last)).or_else(|| m.values().next())
}

impl PerfSection {
    /// The controls shown, in focus order.
    pub fn ctls(&self, d: &PanelData) -> Vec<Ctl> {
        let mut v = Vec::new();
        if d.tweaks.ready().is_some_and(|t| t.options.contains_key(CAP_TWEAK)) {
            v.push(Ctl::Cap);
        }
        if let Some(h) = d.hud.ready() {
            v.push(Ctl::Hud);
            if !h.live() && !h.settings.effective.enabled {
                v.push(Ctl::Enable);
            }
            v.push(Ctl::Edit);
            v.push(Ctl::Timings);
        }
        v
    }

    pub fn focused(&self, d: &PanelData) -> Option<Ctl> {
        let c = self.ctls(d);
        c.get(self.focus.min(c.len().saturating_sub(1))).copied()
    }

    /// The HUD control's value: hidden, or the shown layout.
    pub fn hud_value(&self, d: &PanelData) -> String {
        if let Some(v) = &self.hud_draft {
            return v.clone();
        }
        let Some(h) = d.hud.ready() else { return "hidden".into() };
        let c = &h.settings.effective;
        match h.status.as_ref().filter(|_| h.live()) {
            Some(s) if !s.on => "hidden".into(),
            Some(s) => s.layout.clone(),
            None if !c.on => "hidden".into(),
            None => c.layout.clone(),
        }
    }

    /// The scope top-level changes go to (the editor's own when it's open).
    pub fn scope(&self, d: &PanelData) -> Option<HudScopeRef> {
        let s = d.session.ready()?;
        Some(self.editor.as_ref().map_or(Applies::default_for(s), |e| e.applies).scope(s))
    }

    /// The settings the editor shows: at its scope, else the game's.
    pub fn config(&self, d: &PanelData) -> HudConfig {
        let want = self.scope(d);
        if let Some(v) = self.data.scoped.as_ref().filter(|v| Some(&v.scope) == want.as_ref()) {
            return v.effective.clone();
        }
        d.hud.ready().map(|h| h.settings.effective.clone()).unwrap_or_default()
    }

    fn set(&self, d: &PanelData, set: HudScope) -> Vec<Act> {
        match self.scope(d) {
            Some(scope) => vec![Act::Send(Req::Set { scope, set })],
            None => vec![Act::Note("The session isn't loaded yet.".into(), Tone::Warn)],
        }
    }

    /// The section's loads when it's shown: the graph always, the rest once.
    pub fn refresh(&self, d: &PanelData) -> Vec<Req> {
        let mut v = vec![Req::Frames];
        if matches!(self.data.layouts, Load::Loading) {
            v.push(Req::Layouts);
        }
        if let Some(e) = &self.editor {
            v.push(Req::Preview(e.layout.clone()));
            if matches!(self.data.catalog, Load::Loading) || e.zone == Zone::Metrics {
                v.push(Req::Catalog);
            }
        }
        let _ = d;
        v
    }

    /// One input while the section's body has focus.
    pub fn input(&mut self, i: Input, d: &PanelData, cap: &Value) -> Outcome {
        if self.editor.is_some() {
            return self.editor_input(i, d);
        }
        let ctls = self.ctls(d);
        if ctls.is_empty() {
            return Outcome::Pass;
        }
        self.focus = self.focus.min(ctls.len() - 1);
        let ctl = ctls[self.focus];
        let dir = match i {
            Input::Up | Input::Down => {
                let n = self.focus as i32 + if i == Input::Down { 1 } else { -1 };
                if !(0..ctls.len() as i32).contains(&n) {
                    return Outcome::Pass;
                }
                self.focus = n as usize;
                return Outcome::Done(vec![]);
            }
            Input::PlayOptions => return Outcome::Done(vec![Act::Send(Req::Log), Act::Note(format!("Saving the last {} s…", SPAN_MS / 1000), Tone::Muted)]),
            Input::Left => -1,
            Input::Right => 1,
            Input::Activate => 0,
            _ => return Outcome::Pass,
        };
        Outcome::Done(match ctl {
            Ctl::Cap => {
                let choices = cap_choices(cap);
                let next = if dir == 0 {
                    let i = choices.iter().position(|x| same_value(x, cap)).map_or(0, |i| (i + 1) % choices.len());
                    choices[i].clone()
                } else {
                    step_value_in(&choices, cap, dir)
                };
                if same_value(&next, cap) {
                    return Outcome::Done(vec![]);
                }
                vec![Act::SetTweak(CAP_TWEAK.into(), next)]
            }
            Ctl::Hud => {
                let choices = hud_choices(&self.data.layouts);
                let cur = self.hud_value(d);
                let next = if dir == 0 { cycle(&choices, &cur, 1) } else { step(&choices, &cur, dir) };
                if next == cur {
                    return Outcome::Done(vec![]);
                }
                self.hud_draft = Some(next.clone());
                let live = d.hud.ready().is_some_and(|h| h.live());
                let shown = d.hud.ready().and_then(|h| h.status.as_ref()).is_some_and(|s| s.on);
                if next == "hidden" {
                    if live && shown { vec![Act::Send(Req::Toggle)] } else { self.set(d, HudScope { on: Some(false), ..Default::default() }) }
                } else {
                    self.set(d, HudScope { layout: Some(next), on: Some(true), ..Default::default() })
                }
            }
            Ctl::Enable => {
                let on = !d.hud.ready().is_some_and(|h| h.settings.effective.enabled);
                self.set(d, HudScope { enabled: Some(on), ..Default::default() })
            }
            Ctl::Timings => {
                let on = !self.config(d).reflex_timings;
                self.set(d, HudScope { reflex_timings: Some(on), ..Default::default() })
            }
            Ctl::Edit if dir == 0 => {
                let id = self.hud_value(d);
                let id = if id == "hidden" { d.hud.ready().map(|h| h.settings.effective.layout.clone()).unwrap_or_else(|| "standard".into()) } else { id };
                self.opening = Some(id.clone());
                let mut v = vec![Act::Send(Req::Layout(id)), Act::Send(Req::Catalog)];
                if let Some(scope) = self.scope(d) {
                    v.push(Act::Send(Req::Settings(scope)));
                }
                v
            }
            // Nothing to change: Left goes back to the side list, as on any such row.
            Ctl::Edit if dir < 0 => return Outcome::Pass,
            Ctl::Edit => vec![],
        })
    }

    /// The layout to edit arrived ([`Req::Layout`]): open the editor on it.
    pub fn open_editor(&mut self, l: HudLayout, d: &PanelData) {
        self.opening = None;
        let applies = match (&self.editor, d.session.ready()) {
            (Some(e), _) => e.applies,
            (None, Some(s)) => Applies::default_for(s),
            (None, None) => Applies::Game,
        };
        let mut e = Editor::new(l, applies);
        if let Some(old) = &self.editor {
            (e.zone, e.setting) = (old.zone, old.setting);
        }
        self.editor = Some(e);
    }

    fn editor_input(&mut self, i: Input, d: &PanelData) -> Outcome {
        let groups = self.data.catalog.ready().map(|c| groups(c)).unwrap_or_default();
        let list = entries(&groups);
        let cfg = self.config(d);
        let session = d.session.ready().cloned();
        let ed = self.editor.as_mut().unwrap();
        let mut acts = Vec::new();
        let n_rows = ed.layout.rows.len();
        match (ed.zone, i) {
            // B: put a moving row back, else one level out.
            (_, Input::Back) => {
                if let Some(before) = ed.moving.take() {
                    ed.layout = before;
                    ed.perm = (0..ed.layout.rows.len()).map(|k| ed.perm.get(k).copied().flatten()).collect();
                    acts.push(Act::Send(Req::Preview(ed.layout.clone())));
                } else if ed.zone == Zone::Settings {
                    ed.zone = Zone::Preview;
                } else {
                    self.editor = None;
                }
            }
            (_, Input::Menu) => {
                let (id, name) = save_as(&ed.base, &ed.base_name, ed.builtin);
                let mut l = ed.layout.clone();
                (l.id, l.name, l.builtin) = (id.clone(), name.clone(), false);
                match session.as_ref() {
                    Some(s) => {
                        acts.push(Act::Send(Req::Save { layout: l, scope: ed.applies.scope(s) }));
                        acts.push(Act::Note(format!("Saving {name}…"), Tone::Muted));
                    }
                    None => acts.push(Act::Note("The session isn't loaded yet.".into(), Tone::Warn)),
                }
            }
            (_, Input::PlayOptions) => acts.push(Act::Send(Req::Toggle)),
            (_, Input::PrevSection | Input::NextSection | Input::PrevTab | Input::NextTab) => return Outcome::Pass,
            // The metric list.
            (Zone::Metrics, Input::Up | Input::Down) => {
                let n = ed.metric as i32 + if i == Input::Down { 1 } else { -1 };
                if (0..list.len() as i32).contains(&n) {
                    ed.metric = n as usize;
                }
            }
            (Zone::Metrics, Input::Activate) => {
                if let Some((g, e)) = list.get(ed.metric) {
                    let entry = groups[*g].entries[*e].clone();
                    let (gone, added) = toggle(&mut ed.layout, &entry);
                    for k in gone.iter().rev() {
                        if *k < ed.perm.len() {
                            ed.perm.remove(*k);
                        }
                    }
                    if added {
                        ed.perm.push(None);
                    }
                    ed.row = ed.row.min(ed.layout.rows.len().saturating_sub(1));
                    ed.changed();
                    acts.push(Act::Send(Req::Preview(ed.layout.clone())));
                }
            }
            (Zone::Metrics, Input::Right) => ed.zone = Zone::Preview,
            (Zone::Metrics, Input::Left) => {}
            // The preview: A picks a row up and puts it down, ▲▼ move it.
            (Zone::Preview, Input::Up | Input::Down) => {
                let d = if i == Input::Down { 1 } else { -1 };
                if ed.moving.is_some() {
                    let to = move_row(&mut ed.layout, ed.row, d);
                    if to != ed.row {
                        ed.perm.swap(ed.row, to);
                        ed.row = to;
                        ed.changed();
                    }
                } else {
                    let n = ed.row as i32 + d;
                    if (0..n_rows as i32).contains(&n) {
                        ed.row = n as usize;
                    }
                }
            }
            (Zone::Preview, Input::Activate) => {
                if ed.moving.take().is_some() {
                    acts.push(Act::Send(Req::Preview(ed.layout.clone())));
                } else if n_rows > 0 {
                    ed.moving = Some(ed.layout.clone());
                }
            }
            (Zone::Preview, Input::Left) if ed.moving.is_none() => ed.zone = Zone::Metrics,
            (Zone::Preview, Input::Right) if ed.moving.is_none() => ed.zone = Zone::Settings,
            (Zone::Preview, _) => {}
            // The settings.
            (Zone::Settings, Input::Up | Input::Down) => {
                let n = ed.setting as i32 + if i == Input::Down { 1 } else { -1 };
                if (0..SETTINGS.len() as i32).contains(&n) {
                    ed.setting = n as usize;
                }
            }
            (Zone::Settings, Input::Left | Input::Right | Input::Activate) => {
                let dir = match i {
                    Input::Left => -1,
                    Input::Right => 1,
                    _ => 0,
                };
                let Some(s) = session.as_ref() else { return Outcome::Done(vec![Act::Note("The session isn't loaded yet.".into(), Tone::Warn)]) };
                let scope = ed.applies.scope(s);
                let set = |h: HudScope| Act::Send(Req::Set { scope: scope.clone(), set: h });
                match SETTINGS[ed.setting] {
                    Setting::Preset => {
                        let choices: Vec<String> = hud_choices(&self.data.layouts).into_iter().filter(|c| c != "hidden").collect();
                        let next = if dir == 0 { cycle(&choices, &ed.base, 1) } else { step(&choices, &ed.base, dir) };
                        if next != ed.base {
                            acts.push(set(HudScope { layout: Some(next.clone()), ..Default::default() }));
                            acts.push(Act::Send(Req::Layout(next.clone())));
                            self.opening = Some(next);
                        }
                    }
                    Setting::Position => {
                        let all = ANCHORS.map(|a| a.0);
                        let next = if dir == 0 { cycle(&all, &cfg.anchor, 1) } else { cycle(&all, &cfg.anchor, dir) };
                        acts.push(set(HudScope { anchor: Some(next), ..Default::default() }));
                    }
                    Setting::Opacity => {
                        let cur = (cfg.background * 10.0).round() / 10.0;
                        let next = if dir == 0 { if cur >= 0.95 { 0.0 } else { cur + 0.1 } } else { (cur + 0.1 * dir as f32).clamp(0.0, 1.0) };
                        if (next - cfg.background).abs() > 1e-3 {
                            acts.push(set(HudScope { background: Some((next * 100.0).round() / 100.0), ..Default::default() }));
                        }
                    }
                    Setting::Scale => {
                        let next = step_scale(cfg.scale, dir);
                        if (next - cfg.scale).abs() > 1e-3 {
                            acts.push(set(HudScope { scale: Some(next), ..Default::default() }));
                        }
                    }
                    Setting::Rate => {
                        let all = [2u32, 4, 10];
                        let cur = all.iter().copied().min_by_key(|r| r.abs_diff(cfg.rate_hz)).unwrap_or(2);
                        let next = if dir == 0 { cycle(&all, &cur, 1) } else { step(&all, &cur, dir) };
                        if next != cfg.rate_hz {
                            acts.push(set(HudScope { rate_hz: Some(next), ..Default::default() }));
                        }
                    }
                    Setting::Captures => {
                        let next = if cfg.captures == Captures::Shown { Captures::Hidden } else { Captures::Shown };
                        acts.push(set(HudScope { captures: Some(next), ..Default::default() }));
                    }
                    Setting::AppliesTo => {
                        let all = Applies::choices(s);
                        let next = if dir == 0 { cycle(&all, &ed.applies, 1) } else { step(&all, &ed.applies, dir) };
                        if next != ed.applies {
                            ed.applies = next;
                            acts.push(Act::Send(Req::Settings(next.scope(s))));
                        }
                    }
                    Setting::Drawn => {
                        let next = if cfg.presenter == PresenterSetting::Desktop { PresenterSetting::Auto } else { PresenterSetting::Desktop };
                        acts.push(set(HudScope { presenter: Some(next), ..Default::default() }));
                    }
                }
            }
            _ => {}
        }
        Outcome::Done(acts)
    }

    /// The hint bar's entries: (glyphs, words).
    pub fn hints(&self, d: &PanelData) -> Vec<(Vec<Glyph>, String)> {
        let h = |g: &[Glyph], t: &str| (g.to_vec(), t.to_owned());
        let Some(e) = &self.editor else {
            let mut v = match self.focused(d) {
                Some(Ctl::Edit) => vec![h(&[Glyph::A], "Edit the HUD layout")],
                Some(_) => vec![h(&[Glyph::LeftRight], "Change"), h(&[Glyph::A], "Next value")],
                None => vec![],
            };
            v.push(h(&[Glyph::X], &format!("Save a {} s performance log", SPAN_MS / 1000)));
            v.push(h(&[Glyph::B], "Back"));
            return v;
        };
        let mut v = match (e.zone, e.moving.is_some()) {
            (_, true) => vec![h(&[Glyph::UpDown], "Move the row"), h(&[Glyph::A], "Put it down"), h(&[Glyph::B], "Put it back")],
            (Zone::Metrics, _) => vec![h(&[Glyph::A], "Add or remove"), h(&[Glyph::LeftRight], "Rows")],
            (Zone::Preview, _) => vec![h(&[Glyph::A], "Pick up the row"), h(&[Glyph::LeftRight], "Metrics · Layout")],
            (Zone::Settings, _) => vec![h(&[Glyph::LeftRight], "Change"), h(&[Glyph::A], "Next value")],
        };
        if e.moving.is_none() {
            let shown = d.hud.ready().and_then(|h| h.status.as_ref()).is_some_and(|s| s.on);
            v.push(h(&[Glyph::X], if shown { "Hide the HUD now" } else { "Pin the HUD now" }));
            v.push(h(&[Glyph::Y], "Save as preset"));
            v.push(h(&[Glyph::B], if e.zone == Zone::Settings { "Back to the rows" } else { "Back" }));
        }
        v
    }

    /// quiverd answered. Returns a line for the hint bar, and whether the HUD state changed
    /// (the panel reloads it).
    pub fn reply(&mut self, r: Reply, d: &PanelData) -> (Option<(String, Tone)>, bool) {
        match r {
            Reply::Frames(Ok(f)) => self.data.frames = Some(f),
            Reply::Frames(Err(_)) => {}
            Reply::Catalog(c) => self.data.catalog = load(c),
            Reply::Layouts(l) => self.data.layouts = load(l),
            // The layout asked for (to open the editor, or a preset picked in it).
            Reply::Layout(Ok(l)) => {
                if self.opening.as_deref() == Some(l.id.as_str()) {
                    self.open_editor(l, d);
                }
            }
            Reply::Layout(Err(e)) => {
                self.opening = None;
                return (Some((format!("Couldn't load the layout: {e}"), Tone::Bad)), false);
            }
            Reply::Preview { sent, result: Ok(p) } => {
                if let Some(e) = &mut self.editor {
                    e.previewed(sent, p);
                }
            }
            Reply::Preview { result: Err(e), .. } => return (Some((format!("Preview: {e}"), Tone::Warn)), false),
            Reply::Settings(Ok(v)) => self.data.scoped = Some(v),
            Reply::Settings(Err(_)) => {}
            Reply::Set(r) => {
                self.hud_draft = None;
                return match r {
                    Ok(v) => {
                        let live = d.hud.ready().is_some_and(|h| h.live());
                        self.data.scoped = Some(v);
                        (Some(if live { ("Applied live".into(), Tone::Ok) } else { ("Saved · applies next launch".into(), Tone::Muted) }), true)
                    }
                    Err(e) => (Some((format!("Not saved: {e}"), Tone::Bad)), true),
                };
            }
            Reply::Toggled(r) => {
                self.hud_draft = None;
                return match r {
                    Ok(s) => (Some((if s.on { "HUD shown".into() } else { "HUD hidden".into() }, Tone::Ok)), true),
                    Err(e) => (Some((format!("Couldn't toggle the HUD: {e}"), Tone::Bad)), true),
                };
            }
            Reply::Saved(r) => {
                return match r {
                    Ok((info, v)) => {
                        if let Some(e) = &mut self.editor {
                            (e.base, e.base_name, e.builtin, e.edited) = (info.id.clone(), info.name.clone(), false, false);
                            e.layout.id = info.id.clone();
                            e.layout.name = info.name.clone();
                            e.layout.builtin = false;
                        }
                        self.data.scoped = Some(v);
                        self.data.layouts = Load::Loading;
                        (Some((format!("Saved as {} and selected", info.name), Tone::Ok)), true)
                    }
                    Err(e) => (Some((format!("Not saved: {e}"), Tone::Bad)), false),
                };
            }
            Reply::Log(r) => {
                return (
                    Some(match r {
                        Ok(s) => (format!("Saved {:.0} s ({} frames): {}", s.seconds, s.frames, tilde(&s.path)), Tone::Ok),
                        Err(e) => (format!("No log: {e}"), Tone::Bad),
                    }),
                    false,
                );
            }
        }
        (None, false)
    }
}

/// `path` with the home directory as `~`.
pub fn tilde(path: &std::path::Path) -> String {
    let p = path.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p,
    }
}

fn load<T>(r: Result<T, String>) -> Load<T> {
    match r {
        Ok(t) => Load::Ready(t),
        Err(e) => Load::Failed(e),
    }
}

/// The previous or next of `list` from `cur`, stopping at the ends (a segmented control).
pub fn step<T: PartialEq + Clone>(list: &[T], cur: &T, dir: i32) -> T {
    let i = list.iter().position(|x| x == cur).unwrap_or(0) as i32;
    list[(i + dir.signum()).clamp(0, list.len() as i32 - 1) as usize].clone()
}

/// `step` over values compared as numbers ("120" and 120.0 are the same cap).
fn step_value_in(list: &[Value], cur: &Value, dir: i32) -> Value {
    let i = list.iter().position(|x| same_value(x, cur)).unwrap_or(0) as i32;
    list[(i + dir.signum()).clamp(0, list.len() as i32 - 1) as usize].clone()
}

/// The anchor's cell in the position grid (row, column), `None` for none of the nine.
pub fn anchor_cell(a: Anchor) -> (usize, usize) {
    match a {
        Anchor::TopLeft => (0, 0),
        Anchor::Top => (0, 1),
        Anchor::TopRight => (0, 2),
        Anchor::Left => (1, 0),
        Anchor::Right => (1, 2),
        Anchor::BottomLeft => (2, 0),
        Anchor::Bottom => (2, 1),
        Anchor::BottomRight => (2, 2),
    }
}

#[cfg(test)]
mod tests;
