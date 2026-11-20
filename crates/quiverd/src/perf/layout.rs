//! HUD layouts (SIGHT-HUD §1.1, §1.4): the built-in presets and the user's TOML files.
//!
//! A layout is rows of cells over metric ids and canonical metrics. A cell referencing a
//! metric nobody provides is dropped when the model is built, and a row left with no
//! metric is dropped, so presets work for any producer set. `when = "<metric>"` shows a
//! cell only while that metric has a value that isn't off (`fg.active` gates base FPS).
//!
//! The hierarchy is the layout's (design/SIGHT-HUD.md §1.1, as built 2026-11-19): a cell's
//! `emphasis` (`hero`, `secondary`, `strong`, `normal`, `quiet`, `tag`) and `suffix` (a small
//! label after the value: "118 FPS") go into the model's cell style with the value's producer;
//! `spacer` pushes the rest of a row to its far edge; `{ split = "latency", parts = [...] }`
//! is a stacked bar (`numbers = true`: its numbers); a row with `mods = true` becomes a strip
//! per connected mod, `divider = true` draws a rule above a row.
//!
//! Every layout has a card size (logical px at scale 1, so it follows the swapchain height and
//! the `scale` setting like the text does): `width` (default 260, or 640 for a one-line
//! layout) and `height` ([`Layout::size`]: every row's slot stacked, one line per text row and
//! `rows` × 15 px per graph, [`hud_card`]). Without a `height` of its own the model's card is
//! fitted to the rows it shows and held while the session runs (`hud::fit_size`,
//! `hud::CardHold`); values that don't fit the width are truncated.
//!
//! ```toml
//! # <data>/hud/layouts/dlss-tuning.toml
//! format = 1
//! name = "DLSS tuning"
//! anchor = "top-left"
//! background = 0.45
//! width = 280            # logical px; the height from the rows unless set
//!
//! [[row]]
//! cells = [
//!   { metric = "fps", emphasis = "hero", suffix = "FPS" },
//!   { spacer = true },
//!   { metric = "fps.base", emphasis = "secondary", suffix = "BASE", when = "fg.active" },
//! ]
//!
//! [[row]]
//! cells = [{ graph = "frametime", series = ["frametime", "frametime.base"], span = "3s" }]
//!
//! [[row]]
//! cells = [{ metric = "fps.p1low", label = "1% low", emphasis = "quiet" }]
//!
//! [[row]]
//! mods = true
//! divider = true
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use quiver_api::perf::{Anchor, Emphasis, HudLayout, HudLayoutInfo, Level, Orientation, Stat, hud_card};
use quiver_perf::descriptor::Display;

pub const LAYOUT_FORMAT: u32 = 1;
const MAX_ROWS: usize = 16;
const MAX_CELLS: usize = 24;
const MAX_TEXT: usize = 24;
const MAX_FILE: u64 = 64 * 1024;

/// The preset cycle (`quiver hud next`, Ctrl+Shift+F3 in game), in order.
pub const PRESETS: [&str; 6] = ["fps", "compact", "standard", "full", "diagnostics", "clock"];
pub const BUILTINS: [&str; 6] = PRESETS;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A graph cell: the graph's name (`frametime`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub separator: bool,
    /// Pushes the cells after it to the row's far edge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spacer: bool,
    /// A stacked bar of `parts` across the rest of the row (`split = "latency"`): drawn
    /// while at least two parts have values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<String>,
    /// A split's numbers in a line ("SIM 6 · RENDER 9 · QUEUE 4 · OUT 9") instead of the bar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub numbers: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<Stat>,
    /// A short dim prefix ("GPU"); `label` is a longer one ("1% low").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A small label after the value instead ("FPS" in "118 FPS").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    /// How much the value stands out: `hero` (the one big number; a taller row),
    /// `secondary`, `strong`, `normal`, `quiet` (dimmer), `tag` (a small label-style word).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emphasis: Option<Emphasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Show only while this metric has a value that isn't off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Graph: the series (`frametime`, later `frametime.base`), span ("3s" or ms), height
    /// in text rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u32>,
}

/// `"3s"`, `"500ms"` or a number of ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Span {
    Ms(u32),
    Text(String),
}

impl Span {
    pub fn ms(&self) -> Option<u32> {
        match self {
            Span::Ms(v) => Some(*v),
            Span::Text(s) => {
                let s = s.trim();
                if let Some(v) = s.strip_suffix("ms") {
                    v.trim().parse().ok()
                } else if let Some(v) = s.strip_suffix('s') {
                    v.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0).map(|v| (v * 1000.0) as u32)
                } else {
                    s.parse().ok()
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<Cell>,
    /// A thin rule above the row.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub divider: bool,
    /// Instead of cells: a strip per connected mod, its name and the metrics it declares
    /// (up to `max` strips, default [`hud_card::MOD_STRIPS`]; the card keeps room for them).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mods: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

impl Row {
    /// How many mod strips a `mods` row holds.
    pub fn strips(&self) -> u32 {
        self.max.unwrap_or(hud_card::MOD_STRIPS).clamp(1, 4)
    }
}

/// A layout file (or a preset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default = "format_one")]
    pub format: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<Orientation>,
    /// The card's width, logical px at scale 1 (default by orientation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    /// The card's height (default: the rows' slots, [`hud_card::height`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(default, rename = "row")]
    pub rows: Vec<Row>,
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub builtin: bool,
}

fn format_one() -> u32 {
    LAYOUT_FORMAT
}

impl Layout {
    pub fn orientation(&self) -> Orientation {
        self.orientation.unwrap_or(if self.rows.len() <= 1 { Orientation::Row } else { Orientation::Block })
    }

    /// Each row's slot height: one line, a hero's taller line, a split bar's thin one, or a
    /// graph's rows (a `mods` row: a line per strip it can hold), the divider's room added.
    /// The model's rows come out the same ([`hud_card::row_height`]).
    pub fn slots(&self) -> Vec<f32> {
        let mut out = Vec::new();
        for r in &self.rows {
            if r.mods {
                for i in 0..r.strips() {
                    out.push(hud_card::slot_of(None, false, false, r.divider && i == 0));
                }
                continue;
            }
            let graph = r.cells.iter().filter(|c| c.graph.is_some()).map(|c| c.rows.unwrap_or(2)).max();
            let hero = r.cells.iter().any(|c| c.emphasis == Some(Emphasis::Hero));
            let split_only = !r.cells.is_empty() && r.cells.iter().all(|c| c.split.is_some() && !c.numbers);
            out.push(hud_card::slot_of(graph, hero, split_only, r.divider));
        }
        out
    }

    /// The card's fixed size `[w, h]` (logical px at scale 1) with or without a panel.
    pub fn size(&self, panel: bool) -> [f32; 2] {
        let o = self.orientation();
        let w = hud_card::default_width(o);
        let w = self.width.map_or(w, |v| hud_card::clamp_width(v, w));
        let h = hud_card::height(o, &self.slots(), panel);
        let h = self.height.map_or(h, |v| hud_card::clamp_height(v, h));
        [w, h]
    }

    pub fn to_api(&self) -> HudLayout {
        HudLayout {
            id: self.id.clone(),
            name: self.name.clone(),
            builtin: self.builtin,
            orientation: self.orientation(),
            anchor: self.anchor,
            background: self.background,
            size: self.size(hud_card::panel(self.background.unwrap_or(quiver_api::perf::HudConfig::default().background))),
            rows: self.rows.iter().map(|r| r.cells.iter().map(|c| serde_json::to_value(c).unwrap_or_default()).collect()).collect(),
            row_options: if self.rows.iter().any(|r| r.mods || r.divider) {
                self.rows.iter().map(|r| serde_json::to_value(Row { cells: Vec::new(), ..r.clone() }).unwrap_or_default()).collect()
            } else {
                Vec::new()
            },
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format != LAYOUT_FORMAT {
            return Err(format!("format {} (this Quiver reads format {LAYOUT_FORMAT})", self.format));
        }
        if self.name.chars().count() > 32 {
            return Err("name is longer than 32 characters".into());
        }
        if self.rows.is_empty() || self.rows.len() > MAX_ROWS {
            return Err(format!("needs 1–{MAX_ROWS} rows"));
        }
        if let Some(b) = self.background
            && !(b.is_finite() && (0.0..=1.0).contains(&b))
        {
            return Err("background must be 0–1".into());
        }
        for (name, v, lo, hi) in [("width", self.width, hud_card::MIN_WIDTH, hud_card::MAX_WIDTH), ("height", self.height, hud_card::MIN_HEIGHT, hud_card::MAX_HEIGHT)] {
            if let Some(v) = v
                && !(v.is_finite() && (lo..=hi).contains(&v))
            {
                return Err(format!("{name} must be {lo}–{hi} (logical px)"));
            }
        }
        for (i, r) in self.rows.iter().enumerate() {
            if r.cells.len() > MAX_CELLS {
                return Err(format!("row {}: more than {MAX_CELLS} cells", i + 1));
            }
            if r.mods && !r.cells.is_empty() {
                return Err(format!("row {}: a mods row has no cells of its own", i + 1));
            }
            if r.max.is_some_and(|m| !(1..=4).contains(&m)) || (r.max.is_some() && !r.mods) {
                return Err(format!("row {}: max (1–4) is for a mods row", i + 1));
            }
            for (j, c) in r.cells.iter().enumerate() {
                c.validate().map_err(|e| format!("row {} cell {}: {e}", i + 1, j + 1))?;
            }
        }
        Ok(())
    }
}

/// A layout from the API's form (`HudLayout`: cells as JSON, row options beside them), as the
/// Sight panel's editor sends it to be saved. Validated like a file.
pub fn from_api(l: &HudLayout) -> Result<Layout, String> {
    let mut rows = Vec::with_capacity(l.rows.len());
    for (i, cells) in l.rows.iter().enumerate() {
        let mut row: Row = match l.row_options.get(i) {
            Some(o) if !o.is_null() => serde_json::from_value(o.clone()).map_err(|e| format!("row {}: {e}", i + 1))?,
            _ => Row::default(),
        };
        row.cells = cells.iter().enumerate().map(|(j, c)| serde_json::from_value(c.clone()).map_err(|e| format!("row {} cell {}: {e}", i + 1, j + 1))).collect::<Result<_, _>>()?;
        rows.push(row);
    }
    let width = (l.size[0] > 0.0).then_some(l.size[0]);
    let out = Layout { format: LAYOUT_FORMAT, name: l.name.clone(), anchor: l.anchor, background: l.background, orientation: Some(l.orientation), width, height: None, rows, id: l.id.clone(), builtin: false };
    out.validate()?;
    Ok(out)
}

/// Writes `l` as the user layout `<id>.toml` (replacing one of that id), atomically. A
/// preset's id, or one that isn't a file name, is refused.
pub fn save(hud_dir: &Path, l: &HudLayout) -> Result<HudLayoutInfo, String> {
    let layout = from_api(l)?;
    let path = layouts_dir(hud_dir).join(format!("{}.toml", l.id));
    if file_id(&path).as_deref() != Some(l.id.as_str()) {
        return Err(format!("{:?} can't be a layout's id (lowercase letters, digits, - and _; not a preset's)", l.id));
    }
    let text = toml::to_string(&layout).map_err(|e| e.to_string())?;
    let text = format!("# A HUD layout saved by Quiver (the Sight panel's layout editor).\n{text}");
    std::fs::create_dir_all(layouts_dir(hud_dir)).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| format!("{}: {e}", path.display()))?;
    // Read back as `load` will.
    let back = load(hud_dir, &l.id)?;
    Ok(HudLayoutInfo { id: back.id, name: back.name, builtin: false, path: Some(path), error: None })
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.split('.').all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'))
}

impl Cell {
    fn validate(&self) -> Result<(), String> {
        let kinds = [self.metric.is_some(), self.text.is_some(), self.graph.is_some(), self.separator, self.spacer, self.split.is_some()].iter().filter(|b| **b).count();
        if kinds != 1 {
            return Err("needs exactly one of metric, text, graph, separator, spacer, split".into());
        }
        if self.split.is_some() == self.parts.is_empty() || (self.numbers && self.split.is_none()) || self.parts.len() > 6 || !self.parts.iter().all(|p| valid_id(p)) {
            return Err("a split needs 1–6 parts (metric ids), and parts need a split".into());
        }
        if let Some(m) = &self.metric
            && !valid_id(m)
        {
            return Err(format!("bad metric id {m:?}"));
        }
        if let Some(w) = &self.when
            && !valid_id(w)
        {
            return Err(format!("bad when {w:?}"));
        }
        if let Some(t) = &self.text
            && (t.chars().count() > MAX_TEXT || t.chars().any(char::is_control))
        {
            return Err(format!("text longer than {MAX_TEXT} characters or with control characters"));
        }
        for (name, v, max) in [("short", &self.short, 6), ("label", &self.label, 24), ("suffix", &self.suffix, 16)] {
            if let Some(v) = v
                && (v.chars().count() > max || v.chars().any(char::is_control))
            {
                return Err(format!("{name} longer than {max} characters"));
            }
        }
        if let Some(s) = &self.span {
            match s.ms() {
                Some(ms) if (1000..=10_000).contains(&ms) => {}
                _ => return Err("span must be 1–10 s".into()),
            }
        }
        if self.rows.is_some_and(|r| !(1..=6).contains(&r)) {
            return Err("rows must be 1–6".into());
        }
        if self.series.len() > 2 || !self.series.iter().all(|s| valid_id(s)) {
            return Err("series: up to two metric ids".into());
        }
        Ok(())
    }
}

// ---- presets

fn m(id: &str) -> Cell {
    Cell { metric: Some(id.into()), ..Default::default() }
}

fn ms(id: &str, short: &str) -> Cell {
    Cell { short: Some(short.into()), ..m(id) }
}

fn t(text: &str) -> Cell {
    Cell { text: Some(text.into()), level: Some(Level::Dim), ..Default::default() }
}

fn sep() -> Cell {
    Cell { separator: true, ..Default::default() }
}

fn when(mut c: Cell, w: &str) -> Cell {
    c.when = Some(w.into());
    c
}

fn badge(mut c: Cell) -> Cell {
    c.display = Some(Display::Badge);
    c
}

/// The frame-time graph: presented frames, and the rendered (base) ones while frame
/// generation is on.
fn graph() -> Cell {
    Cell { graph: Some("frametime".into()), series: vec!["frametime".into(), "frametime.base".into()], rows: Some(2), ..Default::default() }
}

fn emph(mut c: Cell, e: Emphasis) -> Cell {
    c.emphasis = Some(e);
    c
}

fn quiet(c: Cell) -> Cell {
    emph(c, Emphasis::Quiet)
}

/// A small label after the value ("118 FPS").
fn after(mut c: Cell, suffix: &str) -> Cell {
    c.suffix = Some(suffix.into());
    c
}

fn labelled(mut c: Cell, label: &str) -> Cell {
    c.label = Some(label.into());
    c
}

fn spacer() -> Cell {
    Cell { spacer: true, ..Default::default() }
}

/// The frame's latency stages as a stacked bar.
fn latency_split() -> Cell {
    Cell { split: Some("latency".into()), parts: super::catalog::LATENCY_SPLIT.iter().map(|p| p.0.to_owned()).collect(), ..Default::default() }
}

/// Shown FPS, and the base FPS beside it while frame generation is on.
fn fps_pair(shown: Emphasis, base: Emphasis, spaced: bool) -> Vec<Cell> {
    let mut v = vec![emph(after(m("fps"), "FPS"), shown)];
    if spaced {
        v.push(spacer());
    }
    v.push(when(emph(after(m("fps.base"), "BASE"), base), "fg.active"));
    v
}

/// "87% 72° 96W" after a "GPU" prefix.
fn gpu(vram: bool) -> Vec<Cell> {
    let mut v = vec![ms("gpu.load", "GPU"), quiet(m("gpu.temp")), quiet(m("gpu.power"))];
    if vram {
        v.push(quiet(m("gpu.vram.used")));
    }
    v
}

fn cpu() -> Vec<Cell> {
    vec![ms("cpu.load", "CPU"), quiet(m("cpu.temp"))]
}

/// The FG multiplier the layer counts (or Smooth Motion's note), as a tag.
fn fg_tag() -> Vec<Cell> {
    vec![when(emph(ms("fg.ratio", "FG"), Emphasis::Tag), "fg.active"), emph(ms("nv.fg.driver", "FG"), Emphasis::Tag)]
}

/// "28 MS PC LATENCY ······ REFLEX BOOST"; Full adds game-to-render latency.
fn latency_row(full: bool) -> Row {
    let mut v = vec![after(m("latency.pc"), if full { "PC LAT" } else { "PC LATENCY" })];
    if full {
        v.push(quiet(after(m("latency.render"), "RENDER")));
    }
    v.extend([spacer(), when(emph(ms("nv.reflex.mode", "REFLEX"), Emphasis::Tag), "nv.reflex.mode")]);
    row(v)
}

fn row(cells: Vec<Cell>) -> Row {
    Row { cells, ..Default::default() }
}

pub fn builtin(id: &str) -> Option<Layout> {
    // Each preset's card width: `hud_card::preset_width`.
    let width = hud_card::preset_width(id);
    let mut l = Layout { format: 1, name: String::new(), anchor: None, background: None, orientation: None, width, height: None, rows: Vec::new(), id: id.into(), builtin: true };
    match id {
        // "118 FPS 59 BASE".
        "fps" => {
            l.name = "FPS only".into();
            l.rows = vec![row(fps_pair(Emphasis::Strong, Emphasis::Strong, false))];
        }
        // One line: "118 FPS 59 BASE 8.5 MS 94 1% LOW 28 MS LAT │ GPU 87% 72° 96W │ CPU 41% 78° │ DLSS K FG 2.0×".
        "compact" => {
            l.name = "Compact".into();
            let mut r = fps_pair(Emphasis::Strong, Emphasis::Strong, false);
            r.extend([quiet(m("frametime")), quiet(after(m("fps.p1low"), "1% LOW")), after(m("latency.pc"), "LAT"), sep()]);
            r.extend(gpu(false));
            r.push(sep());
            r.extend(cpu());
            r.extend([sep(), badge(m("upscaler"))]);
            r.extend(fg_tag());
            l.rows = vec![row(r)];
        }
        // The mock-up's block (design canvas, HudOverGame): the shown FPS big, base FPS beside
        // it, the dual frame-time graph, frame time and 1% low, latency with Reflex's mode and
        // its split, then GPU and CPU.
        "standard" | "full" => {
            let full = id == "full";
            l.name = if full { "Full".into() } else { "Standard".into() };
            let mut third = vec![quiet(m("frametime")), t("·"), quiet(labelled(m("fps.p1low"), "1% low")), spacer()];
            third.extend(fg_tag());
            l.rows = vec![row(fps_pair(Emphasis::Hero, Emphasis::Secondary, true)), row(vec![graph()]), row(third), latency_row(full), row(vec![latency_split()])];
            if full {
                // The split's numbers, then GPU and CPU on their own rows with VRAM and RAM.
                l.rows.extend([row(vec![Cell { numbers: true, ..latency_split() }]), row(gpu(true)), row([cpu(), vec![quiet(ms("mem.used", "RAM"))]].concat())]);
                // The display's flags (VRR and tearing explain pacing), Reflex's cap.
                l.rows.push(row(vec![ms("display.vrr", "VRR"), ms("display.tearing", "TEAR"), quiet(m("display.refresh")), ms("nv.reflex.cap", "CAP")]));
                // DLSS as the agnostic sources see it: render → output, quality and model.
                l.rows.push(row(vec![badge(m("upscaler")), quiet(when(m("render.resolution"), "upscaler")), when(t("→"), "upscaler"), quiet(when(m("output.resolution"), "upscaler"))]));
                l.rows.push(row(vec![ms("dlss.sr.quality", "SR"), quiet(m("dlss.sr.model"))]));
                // A strip per mod from the metrics it declares.
                l.rows.push(Row { mods: true, divider: true, ..Default::default() });
            } else {
                l.rows.push(row([gpu(false), cpu()].concat()));
            }
        }
        // What Full used to carry: clocks, memory, battery, the display path, session and
        // time of day, DLSS's DLLs and override, Quiver Tweaks.
        "diagnostics" => {
            l.name = "Diagnostics".into();
            l.rows = vec![
                row(vec![emph(after(m("fps"), "FPS"), Emphasis::Strong), quiet(m("frametime")), spacer(), ms("session.time", "TIME"), quiet(m("clock.time"))]),
                row(vec![ms("gpu.clock", "GPU"), ms("cpu.clock", "CPU")]),
                row(vec![ms("gpu.vram.used", "VRAM"), quiet(labelled(m("gpu.vram.total"), "of")), ms("mem.used", "RAM")]),
                row(vec![ms("proc.rss", "GAME"), quiet(m("proc.cpu"))]),
                row(vec![ms("bat.level", "BAT"), quiet(m("bat.power")), badge(m("bat.status"))]),
                row(vec![ms("display.mode", "DISP"), ms("display.vrr", "VRR"), ms("display.tearing", "TEAR")]),
                // The path on its own row: "composited (blocked: …)" needs the width.
                row(vec![ms("display.path", "PATH"), quiet(labelled(m("display.blocked"), "blocked by"))]),
                row(vec![ms("dlss.version", "DLL"), ms("dlss.fg.version", "FG"), ms("dlss.sr.requested", "REQ")]),
                // Quiver Tweaks' chips (design/TWEAKS.md §6): `CAP 160 · DLSS K · LL on`.
                row(vec![when(ms("tweaks.summary", "TWK"), "tweaks.summary")]),
            ];
        }
        "clock" => {
            l.name = "Clock".into();
            l.anchor = Some(Anchor::TopLeft);
            l.rows = vec![row(vec![m("clock.time"), sep(), m("session.time"), sep(), ms("bat.level", "BAT")])];
        }
        _ => return None,
    }
    Some(l)
}

/// The next preset after `current` (from a custom layout: the first preset).
pub fn next_preset(current: &str) -> &'static str {
    match PRESETS.iter().position(|p| *p == current) {
        Some(i) => PRESETS[(i + 1) % PRESETS.len()],
        None => PRESETS[0],
    }
}

/// User layouts live in `<data>/hud/layouts/<id>.toml`.
pub fn layouts_dir(hud_dir: &Path) -> PathBuf {
    hud_dir.join("layouts")
}

pub fn parse(id: &str, text: &str) -> Result<Layout, String> {
    let mut l: Layout = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    l.id = id.to_owned();
    if l.name.is_empty() {
        l.name = id.to_owned();
    }
    l.validate()?;
    Ok(l)
}

fn file_id(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_str()?;
    (p.extension()? == "toml" && valid_id(stem) && !stem.contains('.') && !BUILTINS.contains(&stem)).then(|| stem.to_owned())
}

/// A layout by id: a preset, else `<id>.toml` in the layouts dir.
pub fn load(hud_dir: &Path, id: &str) -> Result<Layout, String> {
    if let Some(l) = builtin(id) {
        return Ok(l);
    }
    let path = layouts_dir(hud_dir).join(format!("{id}.toml"));
    if file_id(&path).as_deref() != Some(id) {
        return Err(format!("no layout {id:?}"));
    }
    let md = std::fs::symlink_metadata(&path).map_err(|_| format!("no layout {id:?} (presets: {})", BUILTINS.join(", ")))?;
    if !md.is_file() || md.len() > MAX_FILE {
        return Err(format!("{} is not a layout file", path.display()));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(id, &text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn list(hud_dir: &Path) -> Vec<HudLayoutInfo> {
    let mut out: Vec<HudLayoutInfo> = BUILTINS
        .iter()
        .filter_map(|id| builtin(id))
        .map(|l| HudLayoutInfo { id: l.id, name: l.name, builtin: true, path: None, error: None })
        .collect();
    let mut files: Vec<PathBuf> = std::fs::read_dir(layouts_dir(hud_dir)).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.sort();
    for p in files {
        let Some(id) = file_id(&p) else { continue };
        match load(hud_dir, &id) {
            Ok(l) => out.push(HudLayoutInfo { id, name: l.name, builtin: false, path: Some(p), error: None }),
            Err(e) => out.push(HudLayoutInfo { id: id.clone(), name: id, builtin: false, path: Some(p), error: Some(e) }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_validate_and_cycle() {
        for id in BUILTINS {
            let l = builtin(id).unwrap();
            l.validate().unwrap_or_else(|e| panic!("{id}: {e}"));
        }
        assert_eq!(builtin("compact").unwrap().orientation(), Orientation::Row);
        assert_eq!(builtin("standard").unwrap().orientation(), Orientation::Block);
        // Ctrl+Shift+F3 goes FPS only → Compact → Standard → Full → Diagnostics → Clock.
        let mut seen = vec!["fps"];
        while seen.len() < 7 {
            seen.push(next_preset(seen.last().unwrap()));
        }
        assert_eq!(seen, ["fps", "compact", "standard", "full", "diagnostics", "clock", "fps"]);
        assert_eq!(next_preset("my-layout"), "fps");
        assert_eq!(list(std::path::Path::new("/nonexistent")).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), PRESETS);
    }

    /// Every metric a preset references, by preset.
    fn metrics(l: &Layout) -> Vec<String> {
        l.rows.iter().flat_map(|r| &r.cells).flat_map(|c| c.metric.iter().chain(&c.parts).cloned()).collect()
    }

    #[test]
    fn presets_follow_the_mock_ups() {
        let cell = |l: &Layout, id: &str| l.rows.iter().flat_map(|r| &r.cells).find(|c| c.metric.as_deref() == Some(id)).cloned().unwrap();
        // One big number, the shown FPS, with base FPS beside it while FG is on; labels after.
        let s = builtin("standard").unwrap();
        let first = &s.rows[0].cells;
        assert_eq!((first[0].metric.as_deref(), first[0].emphasis, first[0].suffix.as_deref()), (Some("fps"), Some(Emphasis::Hero), Some("FPS")));
        assert!(first[1].spacer);
        assert_eq!((first[2].metric.as_deref(), first[2].emphasis, first[2].when.as_deref()), (Some("fps.base"), Some(Emphasis::Secondary), Some("fg.active")));
        // The dual graph, the latency split, and a taller first row.
        let g = s.rows[1].cells[0].clone();
        assert_eq!(g.series, ["frametime", "frametime.base"]);
        assert!(s.rows.iter().any(|r| r.cells.iter().any(|c| c.split.as_deref() == Some("latency") && c.parts.len() == 4)));
        assert_eq!(s.slots()[0], hud_card::HERO_LINE);
        assert_eq!(cell(&s, "latency.pc").suffix.as_deref(), Some("PC LATENCY"));
        assert_eq!(cell(&s, "nv.reflex.mode").emphasis, Some(Emphasis::Tag));
        // Full = Standard + the split's numbers, VRAM/RAM, the display's flags, DLSS and a
        // strip per mod; the old long tail moved to Diagnostics.
        let f = builtin("full").unwrap();
        let (sm, fm, dm) = (metrics(&s), metrics(&f), metrics(&builtin("diagnostics").unwrap()));
        for id in &sm {
            assert!(fm.contains(id), "full lacks standard's {id}");
        }
        for id in ["latency.split.sim", "display.vrr", "display.tearing", "upscaler", "render.resolution", "gpu.vram.used"] {
            assert!(fm.iter().any(|m| m == id), "full: {id}");
        }
        assert!(f.rows.iter().any(|r| r.mods && r.divider));
        // `quiver hud layout show` carries the row options beside the cells.
        let api = f.to_api();
        assert_eq!(api.row_options.len(), api.rows.len());
        assert_eq!(api.row_options.last().unwrap(), &serde_json::json!({"mods": true, "divider": true}));
        assert!(s.to_api().row_options.is_empty(), "no row options: none on the wire");
        for id in ["gpu.clock", "cpu.clock", "bat.level", "display.path", "display.mode", "session.time", "clock.time", "proc.rss", "dlss.version", "dlss.sr.requested", "tweaks.summary"] {
            assert!(!fm.iter().any(|m| m == id), "full still has {id}");
            assert!(dm.iter().any(|m| m == id), "diagnostics lacks {id}");
        }
        // Full's size has room for its mod strips.
        let strips = hud_card::MOD_STRIPS as usize;
        assert_eq!(f.slots().len(), f.rows.len() - 1 + strips);
        // Compact is one line, FPS only shows base FPS beside the shown one.
        assert_eq!(builtin("compact").unwrap().rows.len(), 1);
        let fps = builtin("fps").unwrap();
        assert_eq!(metrics(&fps), ["fps", "fps.base"]);
        assert_eq!(cell(&fps, "fps.base").suffix.as_deref(), Some("BASE"));
    }

    #[test]
    fn the_design_example_parses() {
        let text = r#"
format = 1
name = "DLSS tuning"
anchor = "top-left"
background = 0.45

[[row]]
cells = [
  { metric = "fps.base", stat = "mean", short = "FPS" },
  { text = "→" },
  { metric = "fps", stat = "mean" },
  { metric = "fps.base", stat = "p1low", label = "1% low" },
]

[[row]]
cells = [{ graph = "frametime", series = ["frametime", "frametime.base"], span = "3s" }]

[[row]]
cells = [
  { metric = "aco-dlss.upscaler" },
  { metric = "render.resolution" },
  { metric = "aco-dlss.dlss.ms", stat = "mean" },
  { metric = "latency.render", stat = "mean", short = "LAT" },
  { metric = "fg.state", display = "badge" },
]
"#;
        let l = parse("dlss-tuning", text).unwrap();
        assert_eq!(l.name, "DLSS tuning");
        assert_eq!(l.rows.len(), 3);
        assert_eq!(l.rows[0].cells[3].stat, Some(Stat::P1low));
        assert_eq!(l.rows[1].cells[0].span.as_ref().unwrap().ms(), Some(3000));
        assert_eq!(l.rows[2].cells[4].display, Some(Display::Badge));
    }

    #[test]
    fn bad_layouts_are_rejected_with_a_reason() {
        let bad = [
            ("format = 2\n[[row]]\ncells=[{text=\"x\"}]", "format 2"),
            ("[[row]]\ncells=[{text=\"x\", metric=\"fps\"}]", "exactly one"),
            ("[[row]]\ncells=[{metric=\"Bad Id\"}]", "bad metric id"),
            ("[[row]]\ncells=[{graph=\"frametime\", span=\"30s\"}]", "span"),
            ("[[row]]\ncells=[{metric=\"fps\", colour=\"pink\"}]", "unknown field"),
            ("name = \"x\"", "rows"),
            ("[[row]]\ncells=[{split=\"latency\"}]", "a split needs"),
            ("[[row]]\ncells=[{metric=\"fps\", parts=[\"a.b\"]}]", "a split needs"),
            ("[[row]]\ncells=[{spacer=true, metric=\"fps\"}]", "exactly one"),
            ("[[row]]\ncells=[{metric=\"fps\", suffix=\"a suffix far too long\"}]", "suffix longer"),
            ("[[row]]\ncells=[{metric=\"fps\", emphasis=\"huge\"}]", "unknown variant"),
            ("[[row]]\nmods=true\ncells=[{metric=\"fps\"}]", "no cells of its own"),
            ("[[row]]\nmax=2\ncells=[{metric=\"fps\"}]", "for a mods row"),
        ];
        for (text, want) in bad {
            let e = parse("x", text).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }

    #[test]
    fn layouts_have_a_fixed_size() {
        let s = builtin("standard").unwrap();
        // Six rows: the hero's 30, the two-row graph's 30, three lines and the split's 8;
        // five gaps of 4, padding 6 and a 1 px border top and bottom.
        assert_eq!(s.size(true), [284.0, 30.0 + 30.0 + 3.0 * 20.0 + 8.0 + 5.0 * 4.0 + 14.0]);
        assert_eq!(s.size(false), [284.0, 148.0]);
        assert_eq!(builtin("compact").unwrap().size(true), [720.0, 34.0]);
        // Full: hero, graph, split, eight lines, and two mod strips (the first with its rule).
        assert_eq!(builtin("full").unwrap().size(true), [320.0, 30.0 + 30.0 + 8.0 + 8.0 * 20.0 + (20.0 + hud_card::DIVIDER) + 20.0 + 12.0 * 4.0 + 14.0]);
        // A user layout with a hero, a split row and a mods row.
        let h = parse("x", "[[row]]\ncells = [{ metric = \"fps\", emphasis = \"hero\", suffix = \"FPS\" }, { spacer = true }]\n[[row]]\ncells = [{ split = \"latency\", parts = [\"a.b\", \"a.c\"] }]\n[[row]]\nmods = true\nmax = 1\ndivider = true").unwrap();
        assert_eq!(h.slots(), [30.0, 8.0, 20.0 + hud_card::DIVIDER]);
        // A user layout: defaults by orientation, or what it asks for.
        let one = parse("x", "[[row]]\ncells = [{ metric = \"fps\" }]").unwrap();
        assert_eq!(one.size(true), [hud_card::DEFAULT_WIDTH_ROW, 34.0]);
        let two = parse("x", "[[row]]\ncells = [{ metric = \"fps\" }]\n[[row]]\ncells = [{ graph = \"frametime\", rows = 4 }]").unwrap();
        assert_eq!(two.size(true), [hud_card::DEFAULT_WIDTH_BLOCK, 20.0 + 60.0 + 4.0 + 14.0]);
        let sized = parse("x", "width = 300\nheight = 120\n[[row]]\ncells = [{ metric = \"fps\" }]").unwrap();
        assert_eq!(sized.size(true), [300.0, 120.0]);
        assert_eq!(sized.to_api().size, [300.0, 120.0]);
        // Silly sizes are refused with a reason.
        for (text, want) in [("width = 10", "width must be"), ("width = 5000", "width must be"), ("height = 0", "height must be"), ("width = nan", "width must be")] {
            let e = parse("x", &format!("{text}\n[[row]]\ncells = [{{ metric = \"fps\" }}]")).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }

    #[test]
    fn an_edited_preset_saves_as_a_user_layout() {
        let t = tempfile::tempdir().unwrap();
        // The editor's round trip: the API form of Standard, rows moved and a row added.
        let mut api = builtin("standard").unwrap().to_api();
        api.rows.swap(2, 3);
        api.rows.push(vec![serde_json::json!({ "metric": "gpu.vram.used", "short": "VRAM" })]);
        api.id = "standard-edited".into();
        api.name = "Standard (edited)".into();
        let info = save(t.path(), &api).unwrap();
        assert_eq!((info.id.as_str(), info.name.as_str(), info.builtin), ("standard-edited", "Standard (edited)", false));
        let back = load(t.path(), "standard-edited").unwrap();
        assert_eq!(back.rows.len(), api.rows.len());
        assert_eq!(back.rows[2].cells[0].metric.as_deref(), Some("latency.pc"), "the moved row");
        assert_eq!(back.rows.last().unwrap().cells[0].short.as_deref(), Some("VRAM"));
        assert_eq!(back.width, Some(284.0), "the preset's width");
        assert!(list(t.path()).iter().any(|i| i.id == "standard-edited" && i.error.is_none()));
        // Full's row options (the mods strip, its rule) survive.
        let mut full = builtin("full").unwrap().to_api();
        full.id = "mine".into();
        save(t.path(), &full).unwrap();
        let f = load(t.path(), "mine").unwrap();
        assert!(f.rows.last().unwrap().mods && f.rows.last().unwrap().divider);
        // Saved again: replaced.
        full.name = "Mine".into();
        assert_eq!(save(t.path(), &full).unwrap().name, "Mine");
        // A preset's id, a path, a bad cell: refused, nothing written.
        for id in ["standard", "../x", "Bad"] {
            let mut bad = api.clone();
            bad.id = id.into();
            assert!(save(t.path(), &bad).is_err(), "{id}");
        }
        let mut bad = api.clone();
        bad.rows[0].push(serde_json::json!({ "metric": "fps", "colour": "pink" }));
        assert!(save(t.path(), &bad).unwrap_err().contains("unknown field"));
    }

    #[test]
    fn user_layouts_load_from_files() {
        let t = tempfile::tempdir().unwrap();
        let dir = layouts_dir(t.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mine.toml"), "name = \"Mine\"\n[[row]]\ncells = [{ metric = \"fps\" }]\n").unwrap();
        std::fs::write(dir.join("broken.toml"), "[[row]]\ncells = [{ nope = 1 }]\n").unwrap();
        std::fs::write(dir.join("compact.toml"), "shadowing a preset is ignored").unwrap();
        std::fs::write(dir.join("README.md"), "not a layout").unwrap();
        assert_eq!(load(t.path(), "mine").unwrap().name, "Mine");
        assert!(load(t.path(), "broken").is_err());
        assert!(load(t.path(), "../etc/passwd").is_err());
        assert_eq!(load(t.path(), "compact").unwrap().name, "Compact");
        let l = list(t.path());
        assert_eq!(l.iter().filter(|i| !i.builtin).count(), 2);
        assert!(l.iter().any(|i| i.id == "broken" && i.error.is_some()));
    }
}
