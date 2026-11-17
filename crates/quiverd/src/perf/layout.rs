//! HUD layouts (SIGHT-HUD §1.1, §1.4): the built-in presets and the user's TOML files.
//!
//! A layout is rows of cells over metric ids and canonical metrics. A cell referencing a
//! metric nobody provides is dropped when the model is built, and a row left with no
//! metric is dropped, so presets work for any producer set. `when = "<metric>"` shows a
//! cell only while that metric has a value that isn't off (`fg.active` gates the
//! "base →" part of `79 → 158`).
//!
//! Every layout has a fixed card size (logical px at scale 1, so it follows the swapchain
//! height and the `scale` setting like the text does): `width` (default 260, or 640 for a
//! one-line layout) and `height` (default: every row's slot stacked, one line per text row
//! and `rows` × 15 px per graph, [`hud_card`]). The card keeps that size whatever it holds:
//! rows that are dropped or gated off leave their space empty at the bottom (the rows that
//! are shown pack from the top), and values that don't fit the width are truncated.
//!
//! ```toml
//! # <data>/hud/layouts/dlss-tuning.toml
//! format = 1
//! name = "DLSS tuning"
//! anchor = "top-left"
//! background = 0.45
//! width = 280            # logical px; height = 20 + 30 + padding unless set
//!
//! [[row]]
//! cells = [
//!   { metric = "fps.base", stat = "mean", short = "FPS" },
//!   { text = "→" },
//!   { metric = "fps", stat = "mean" },
//!   { metric = "fps.p1low", label = "1% low" },
//! ]
//!
//! [[row]]
//! cells = [{ graph = "frametime", series = ["frametime"], span = "3s" }]
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use quiver_api::perf::{Anchor, HudLayout, HudLayoutInfo, Level, Orientation, Stat, hud_card};
use quiver_perf::descriptor::Display;

pub const LAYOUT_FORMAT: u32 = 1;
const MAX_ROWS: usize = 16;
const MAX_CELLS: usize = 24;
const MAX_TEXT: usize = 24;
const MAX_FILE: u64 = 64 * 1024;

/// The preset cycle (`quiver hud next`), in order.
pub const PRESETS: [&str; 4] = ["fps", "compact", "standard", "full"];
pub const BUILTINS: [&str; 5] = ["fps", "compact", "standard", "full", "clock"];

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<Stat>,
    /// A short dim prefix ("FPS"); `label` is a longer one ("1% low").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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
    #[serde(default)]
    pub cells: Vec<Cell>,
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

    /// Each row's slot height: one line, or a graph's rows.
    pub fn slots(&self) -> Vec<f32> {
        self.rows.iter().map(|r| r.cells.iter().filter(|c| c.graph.is_some()).map(|c| hud_card::slot(Some(c.rows.unwrap_or(2)))).fold(hud_card::LINE, f32::max)).collect()
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
            for (j, c) in r.cells.iter().enumerate() {
                c.validate().map_err(|e| format!("row {} cell {}: {e}", i + 1, j + 1))?;
            }
        }
        Ok(())
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.split('.').all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'))
}

impl Cell {
    fn validate(&self) -> Result<(), String> {
        let kinds = [self.metric.is_some(), self.text.is_some(), self.graph.is_some(), self.separator].iter().filter(|b| **b).count();
        if kinds != 1 {
            return Err("needs exactly one of metric, text, graph, separator".into());
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
        for (name, v, max) in [("short", &self.short, 6), ("label", &self.label, 24)] {
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

fn graph() -> Cell {
    Cell { graph: Some("frametime".into()), series: vec!["frametime".into()], rows: Some(2), ..Default::default() }
}

/// "79 → 158" with a base provider and FG on, else "158".
fn fps_pair(short: Option<&str>) -> Vec<Cell> {
    let mut base = when(m("fps.base"), "fg.active");
    let mut presented = m("fps");
    if let Some(s) = short {
        // The label goes on whichever comes first.
        base.short = Some(s.into());
        presented = Cell { short: Some(s.into()), when: None, ..presented };
    }
    let arrow = when(t("→"), "fg.active");
    // With FG on, the presented value's label would repeat: the model builder drops a
    // label equal to the one before it in the same group.
    vec![base, arrow, presented]
}

fn row(cells: Vec<Cell>) -> Row {
    Row { cells }
}

pub fn builtin(id: &str) -> Option<Layout> {
    // Each preset's card width: `hud_card::preset_width`.
    let width = hud_card::preset_width(id);
    let mut l = Layout { format: 1, name: String::new(), anchor: None, background: None, orientation: None, width, height: None, rows: Vec::new(), id: id.into(), builtin: true };
    match id {
        "fps" => {
            l.name = "FPS only".into();
            l.background = Some(0.0);
            l.rows = vec![row(fps_pair(None))];
        }
        "compact" => {
            l.name = "Compact".into();
            let mut r = fps_pair(Some("FPS"));
            r.extend([m("frametime"), ms("fps.p1low", "1%↓"), sep(), ms("gpu.load", "GPU"), m("gpu.temp"), m("gpu.power"), sep(), ms("cpu.load", "CPU"), m("cpu.temp"), sep()]);
            r.extend([badge(m("upscaler")), when(m("render.resolution"), "upscaler"), when(ms("fg.ratio", "FG"), "fg.active"), ms("latency.render", "LAT")]);
            l.rows = vec![row(r)];
        }
        "standard" | "full" => {
            l.name = if id == "full" { "Full".into() } else { "Standard".into() };
            let mut first = fps_pair(Some("FPS"));
            first.extend([Cell { label: Some("1% low".into()), ..m("fps.p1low") }]);
            l.rows = vec![
                row(first),
                row(vec![ms("frametime", "FT"), when(Cell { label: Some("base".into()), ..m("frametime.base") }, "fg.active")]),
                row(vec![graph()]),
                row(vec![ms("gpu.load", "GPU"), m("gpu.temp"), m("gpu.power"), m("gpu.vram.used")]),
                row(vec![ms("cpu.load", "CPU"), m("cpu.temp"), ms("mem.used", "RAM")]),
                row(vec![badge(m("upscaler")), when(m("render.resolution"), "upscaler"), when(t("→"), "upscaler"), when(m("output.resolution"), "upscaler")]),
                row(vec![ms("latency.render", "LAT")]),
            ];
            if id == "full" {
                l.rows.extend([
                    row(vec![ms("gpu.clock", "CLK"), m("cpu.clock")]),
                    row(vec![ms("gpu.vram.total", "VRAM"), ms("proc.rss", "GAME"), m("proc.cpu")]),
                    row(vec![ms("bat.level", "BAT"), m("bat.power"), badge(m("bat.status"))]),
                    row(vec![ms("display.mode", "DISP"), ms("display.vrr", "VRR"), ms("display.tearing", "TEAR"), m("display.path")]),
                    row(vec![ms("session.time", "TIME"), m("clock.time")]),
                    // Quiver Tweaks' chips (design/TWEAKS.md §6): `CAP 160 · DLSS K · LL on`.
                    row(vec![when(ms("tweaks.summary", "TWK"), "tweaks.summary")]),
                ]);
            }
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
        assert_eq!(next_preset("fps"), "compact");
        assert_eq!(next_preset("full"), "fps");
        assert_eq!(next_preset("my-layout"), "fps");
        // Full is Standard plus rows, and has no latency breakdown in this version.
        let (s, f) = (builtin("standard").unwrap(), builtin("full").unwrap());
        assert!(f.rows.len() > s.rows.len());
        assert!(!serde_json::to_string(&f.to_api()).unwrap().contains("breakdown"));
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
        ];
        for (text, want) in bad {
            let e = parse("x", text).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }

    #[test]
    fn layouts_have_a_fixed_size() {
        let s = builtin("standard").unwrap();
        // Seven rows, one of them a two-row graph: 6 × 20 + 30, six gaps of 4, padding 6 and
        // a 1 px border top and bottom.
        assert_eq!(s.size(true), [260.0, 188.0]);
        assert_eq!(s.size(false), [260.0, 174.0]);
        assert_eq!(builtin("compact").unwrap().size(true), [780.0, 34.0]);
        assert_eq!(builtin("full").unwrap().size(true)[1], 12.0 * 20.0 + 30.0 + 12.0 * 4.0 + 14.0);
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
