//! The HUD model builder (§2.7) and the presenter choice (§2.6).
//!
//! quiverd resolves the session's layout, formats every value (decision 3) and sends
//! `quiver-sight` a [`HudModel`]: rows of cells with text and theme levels, no pixels.

use std::path::{Path, PathBuf};

use quiver_api::perf::{
    CellStyle, Emphasis, HudCell, HudConfig, HudImageStatus, HudModel, HudRow, LayerBackdrop, Level, MetricState, PerfValue, Presenter, PresenterSetting, SourceKind, SplitPart, Stat,
    hud_card,
};
use quiver_perf::descriptor::{Display, MetricKind, Range, Unit};
use quiver_perf::layout::{
    BACKDROP_ERR_DEVICE_LOST, BACKDROP_ERR_FORMAT, BACKDROP_ERR_INTERNAL, BACKDROP_ERR_NONE, BACKDROP_ERR_OFF, BACKDROP_ERR_QUEUE, BACKDROP_ERR_USAGE, DRAW_ERR_DEVICE_LOST,
    DRAW_ERR_FORMAT, DRAW_ERR_INTERNAL, DRAW_ERR_NO_IMAGE, DRAW_ERR_NONE, DRAW_ERR_QUEUE, DRAW_ERR_USAGE, LAYER_BLUR_READY, LAYER_BLURRING, LAYER_FOCUS_KNOWN, LAYER_FOCUSED,
    LAYER_PANICKED,
};

use super::format::{self, Raw};
use super::layout::{Cell, Layout};
use super::registry::{Store, WINDOW_1S};
use super::rings::LayerRing;

/// Before the layer shows up, the HUD waits this long for it before going to the desktop
/// (the game is still loading; a desktop surface that unmaps a second later helps nobody).
pub const LAYER_GRACE_NS: i64 = 15_000_000_000;
/// A layer heartbeat older than this means it stopped (§2.6: "within a second" of noticing).
const LAYER_STALE_NS: i64 = 3_000_000_000;
/// A fresh image the layer hasn't taken for this long: it can't use it.
const IMAGE_UNTAKEN_NS: i64 = 2_000_000_000;
/// `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR`.
const COLOR_SPACE_SRGB: u32 = 0;
/// §1.2: text height = 1.6% of the swapchain height.
const TEXT_FRACTION: f32 = 0.016;
const SPARK_POINTS: usize = 30;

/// `hud.image` as quiverd sees it (read-only).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageView {
    pub path: PathBuf,
    pub buffer_bytes: u32,
    pub published: u64,
    pub heartbeat: i64,
    pub writer_pid: u32,
    /// When `published` last changed (quiverd's clock).
    pub changed_at: i64,
}

impl ImageView {
    pub fn fresh(&self, now: i64) -> bool {
        self.heartbeat > 0 && now - self.heartbeat <= quiver_perf::hud::HUD_STALE_NS
    }

    pub fn status(&self, now: i64) -> HudImageStatus {
        HudImageStatus {
            path: self.path.clone(),
            buffer_bytes: self.buffer_bytes,
            published: self.published,
            heartbeat_age_ms: (self.heartbeat > 0).then(|| ((now - self.heartbeat).max(0) / 1_000_000) as u64),
            writer_pid: (self.writer_pid != 0).then_some(self.writer_pid),
        }
    }
}

pub fn draw_error_name(e: u32, flags: u32) -> &'static str {
    if flags & LAYER_PANICKED != 0 {
        return "panicked";
    }
    match e {
        DRAW_ERR_NONE => "none",
        DRAW_ERR_FORMAT => "format",
        DRAW_ERR_USAGE => "usage",
        DRAW_ERR_QUEUE => "queue",
        DRAW_ERR_DEVICE_LOST => "device_lost",
        DRAW_ERR_NO_IMAGE => "no_image",
        DRAW_ERR_INTERNAL => "internal",
        _ => "unknown",
    }
}

/// Who presents the HUD, and why (§2.6). `layer` is the session's most recent live layer
/// ring; `age_ns` the session's age.
pub fn choose_presenter(setting: PresenterSetting, anti_cheat: bool, layer: Option<&LayerRing>, image: Option<&ImageView>, age_ns: i64, now: i64) -> (Presenter, String) {
    match setting {
        PresenterSetting::Desktop => return (Presenter::Desktop, "set to desktop".into()),
        PresenterSetting::Game => return (Presenter::Game, "set to game".into()),
        PresenterSetting::Auto => {}
    }
    if anti_cheat {
        return (Presenter::Desktop, "anti-cheat game: the Sight layer is off".into());
    }
    let Some(l) = layer else {
        return if age_ns < LAYER_GRACE_NS {
            (Presenter::Game, "waiting for the Sight layer".into())
        } else {
            (Presenter::Desktop, "no Sight layer in the game (OpenGL, not loaded, or turned off)".into())
        };
    };
    if l.ring.starts_with("invalid") {
        return (Presenter::Desktop, format!("the layer's ring is {}", l.ring));
    }
    if !l.alive {
        return (Presenter::Desktop, "the game process with the layer is gone".into());
    }
    if l.heartbeat > 0 && now - l.heartbeat > LAYER_STALE_NS {
        return (Presenter::Desktop, "the layer stopped (no heartbeat)".into());
    }
    let Some(s) = l.status else {
        return (Presenter::Game, "waiting for the layer's first swapchain".into());
    };
    if s.flags & LAYER_PANICKED != 0 {
        return (Presenter::Desktop, "the layer disabled drawing after a panic".into());
    }
    match s.draw_error {
        DRAW_ERR_NONE => {}
        DRAW_ERR_NO_IMAGE => {
            // Expected until quiver-sight publishes; a fresh image it won't take means it can't.
            if let Some(img) = image
                && img.fresh(now)
                && img.published > 0
                && s.hud_published < img.published
                && now - img.changed_at > IMAGE_UNTAKEN_NS
            {
                return (Presenter::Desktop, "the layer doesn't take the HUD image".into());
            }
            return (Presenter::Game, "waiting for the HUD image".into());
        }
        e => return (Presenter::Desktop, format!("the layer can't draw ({})", draw_error_name(e, s.flags))),
    }
    if s.color_space != COLOR_SPACE_SRGB {
        return (Presenter::Desktop, format!("HDR swapchain (colour space {}): desktop until phase 2", s.color_space));
    }
    (Presenter::Game, "the Sight layer draws it".into())
}

/// What the builder needs besides the store.
pub struct ModelInput<'a> {
    pub session: &'a str,
    pub product_id: u32,
    pub cfg: &'a HudConfig,
    pub on: bool,
    pub layout: &'a Layout,
    pub presenter: Presenter,
    pub reason: &'a str,
    pub image: Option<&'a Path>,
    pub monitor: Option<&'a str>,
    /// The layer's backdrop blur ([`layer_backdrop`]).
    pub backdrop: Option<LayerBackdrop>,
    /// The game's window in front, as its layer sees it ([`game_focus`]).
    pub game_focus: Option<bool>,
    /// How its frames pack their eyes ([`super::Session::stereo`]).
    pub stereo: Option<quiver_api::perf::StereoInfo>,
}

/// Whether the game's window is in front as its layer reports it (design/SIGHT.md §14):
/// `None` when the layer is gone, stale, or can't tell (`LAYER_FOCUS_KNOWN` unset, as from a
/// layer before it).
pub fn game_focus(layer: Option<&LayerRing>, now: i64) -> Option<bool> {
    let l = layer.filter(|l| l.alive && !l.ring.starts_with("invalid"))?;
    if l.heartbeat > 0 && now - l.heartbeat > LAYER_STALE_NS {
        return None;
    }
    let s = l.status?;
    (s.flags & LAYER_FOCUS_KNOWN != 0).then_some(s.flags & LAYER_FOCUSED != 0)
}

/// `LayerStatus::backdrop_error` as a word.
pub fn backdrop_error_name(e: u32) -> &'static str {
    match e {
        BACKDROP_ERR_NONE => "not reported (a layer from before the backdrop blur)",
        BACKDROP_ERR_FORMAT => "format",
        BACKDROP_ERR_USAGE => "usage",
        BACKDROP_ERR_QUEUE => "queue",
        BACKDROP_ERR_DEVICE_LOST => "device_lost",
        BACKDROP_ERR_INTERNAL => "internal",
        BACKDROP_ERR_OFF => "off",
        _ => "unknown",
    }
}

/// Whether the session's layer can blur the game behind the Sight panel (design/SIGHT.md
/// §14), for `quiver-sight`: `image` is the session's `hud.image` (where the request goes),
/// `layer` the layer on the game's swapchain.
pub fn layer_backdrop(image: Option<&Path>, layer: Option<&LayerRing>, now: i64) -> Option<LayerBackdrop> {
    let image = image?.to_path_buf();
    let not = |reason: &str| Some(LayerBackdrop { image: image.clone(), ready: false, blurring: false, reason: reason.into() });
    let Some(l) = layer.filter(|l| l.alive && !l.ring.starts_with("invalid")) else { return not("no Sight layer in the game") };
    if l.heartbeat > 0 && now - l.heartbeat > LAYER_STALE_NS {
        return not("the layer stopped (no heartbeat)");
    }
    let Some(s) = l.status else { return not("waiting for the layer's first swapchain") };
    if s.flags & LAYER_PANICKED != 0 {
        return not("the layer disabled drawing after a panic");
    }
    if s.flags & LAYER_BLUR_READY == 0 {
        return not(&format!("the layer can't blur this swapchain ({})", backdrop_error_name(s.backdrop_error as u32)));
    }
    Some(LayerBackdrop { image, ready: true, blurring: s.flags & LAYER_BLURRING != 0, reason: String::new() })
}

/// The layer's backdrop blur for `quiver hud status`.
pub fn backdrop_status(s: &quiver_perf::layout::LayerStatus) -> String {
    let gpu = if s.backdrop_gpu_ns > 0 { format!(", {:.2} ms GPU", s.backdrop_gpu_ns as f64 / 1e6) } else { String::new() };
    if s.flags & LAYER_BLURRING != 0 {
        format!("blurring{gpu}")
    } else if s.flags & LAYER_BLUR_READY != 0 {
        format!("ready{}", if gpu.is_empty() { String::new() } else { format!(" (last{gpu})") })
    } else {
        format!("unavailable: {}", backdrop_error_name(s.backdrop_error as u32))
    }
}

fn shown_condition(store: &Store, id: &str, now: i64) -> bool {
    let Some(desc) = store.descriptor(id) else { return false };
    match store.value(id, Stat::Last, WINDOW_1S, now) {
        None => false,
        Some(Raw::State(i)) => desc.values.get(i).is_some_and(|v| v.value != "off"),
        Some(Raw::Text(s)) => !s.is_empty(),
        Some(Raw::Num(v)) => v.is_finite(),
    }
}

fn fraction(desc: &quiver_perf::descriptor::MetricDescriptor, v: Option<f64>) -> f32 {
    let (lo, hi) = match desc.range {
        Some(Range::Fixed { min, max }) => (min, max),
        _ if desc.unit == Unit::Percent => (0.0, 100.0),
        _ => (0.0, 1.0),
    };
    v.map(|v| ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32).unwrap_or(0.0)
}

/// Who produced `id`'s value right now, for the renderer's tint.
fn source_style(store: &Store, id: &str, now: i64) -> CellStyle {
    let Some(p) = store.producer_of(id, now) else { return CellStyle::default() };
    let source = SourceKind::of_producer(p);
    let source_name = (source == SourceKind::Mod).then(|| p.trim_start_matches("mod:").to_owned());
    CellStyle { source, source_name, ..Default::default() }
}

fn metric_cell(store: &Store, c: &Cell, id: &str, now: i64, label: Option<String>, mut style: CellStyle) -> Option<HudCell> {
    let desc = store.descriptor(id)?.clone();
    match store.state(id, now) {
        MetricState::Declared | MetricState::Gone => return None,
        MetricState::Stale | MetricState::Live => {}
    }
    let stat = c.stat.unwrap_or_else(|| format::default_stat(&desc));
    let value: PerfValue = store.formatted(id, stat, WINDOW_1S, now)?;
    if value.text == format::NO_VALUE && !value.stale {
        // A canonical metric nobody provides right now (no upscaler, no latency).
        return None;
    }
    let src = source_style(store, id, now);
    (style.source, style.source_name) = (src.source, src.source_name);
    let display = c.display.or(desc.display).unwrap_or(if desc.kind == MetricKind::State { Display::Badge } else { Display::Number });
    // A tag is a word in the label style, not a pill.
    let display = if style.emphasis == Emphasis::Tag && display == Display::Badge { Display::Number } else { display };
    let metric = id.to_owned();
    Some(match display {
        Display::Badge => HudCell::Badge { metric, label, value, style },
        Display::Bar => {
            let fraction = fraction(&desc, value.value);
            HudCell::Bar { metric, label, value, fraction, style }
        }
        Display::Sparkline | Display::Graph => {
            let tail = store.tail(id, SPARK_POINTS);
            let (lo, hi) = tail.iter().fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
            let points = tail.iter().map(|v| if hi > lo { ((v - lo) / (hi - lo)) as f32 } else { 0.5 }).collect();
            HudCell::Sparkline { metric, label, value, points, style }
        }
        Display::Number | Display::Stack => HudCell::Value { metric, label, value, style },
    })
}

/// A split bar over `parts`: drawn while at least two have a positive value.
fn split_cell(store: &Store, c: &Cell, now: i64) -> Option<HudCell> {
    let mut parts = Vec::new();
    for id in &c.parts {
        if !matches!(store.state(id, now), MetricState::Live) {
            continue;
        }
        let Some(v) = store.value(id, c.stat.unwrap_or(Stat::Mean), WINDOW_1S, now).and_then(|r| r.num()).filter(|v| v.is_finite() && *v > 0.0) else { continue };
        let label = store.descriptor(id).and_then(|d| d.short.clone().or_else(|| d.label.clone())).unwrap_or_else(|| id.clone());
        parts.push(SplitPart { metric: id.clone(), label, value: v as f32 });
    }
    if parts.len() < 2 {
        return None;
    }
    let mut style = source_style(store, &parts[0].metric, now);
    style.emphasis = c.emphasis.unwrap_or_default();
    Some(HudCell::Split { label: c.short.clone().or_else(|| c.label.clone()), parts, numbers: c.numbers, style })
}

/// Most metrics a mod's strip shows.
const STRIP_METRICS: usize = 6;

/// The `mods` row: a strip per connected mod, its name and the gauges, states and texts it
/// declares (basic visibility), each with its short label, "·" between them.
fn mod_strips(store: &Store, row: &super::layout::Row, now: i64) -> Vec<HudRow> {
    let mut out = Vec::new();
    for (pid, p) in &store.producers {
        if !pid.starts_with("mod:") || !p.info.connected || out.len() >= row.strips() as usize {
            continue;
        }
        let name = pid.trim_start_matches("mod:");
        let style = CellStyle { emphasis: Emphasis::Tag, source: SourceKind::Mod, source_name: Some(name.to_owned()), after: false };
        let mut cells = vec![HudCell::Text { text: name.to_uppercase(), level: Level::Normal, style: style.clone() }];
        let mut shown = 0;
        for d in store.metrics_of(pid) {
            if shown == STRIP_METRICS {
                break;
            }
            let visible = matches!(d.visibility, None | Some(quiver_perf::descriptor::Visibility::Basic));
            if !visible || !matches!(d.kind, MetricKind::Gauge | MetricKind::State | MetricKind::Text) {
                continue;
            }
            let c = Cell { metric: Some(d.id.clone()), ..Default::default() };
            let label = d.short.clone().filter(|s| !s.is_empty());
            let Some(mut cell) = metric_cell(store, &c, &d.id, now, label, CellStyle::emphasis(Emphasis::Quiet)) else { continue };
            // States read as words in a strip ("FG on"), not pills.
            if let HudCell::Badge { metric, label, value, style } = cell {
                cell = HudCell::Value { metric, label, value, style };
            }
            if shown > 0 {
                cells.push(HudCell::text("·", Level::Dim));
            }
            cells.push(cell);
            shown += 1;
        }
        if shown > 0 {
            out.push(HudRow { cells, divider: row.divider && out.is_empty() });
        }
    }
    out
}

fn is_content(c: &HudCell) -> bool {
    !matches!(c, HudCell::Text { .. } | HudCell::Separator | HudCell::Spacer)
}

/// Builds the model (rev 0: the caller numbers it).
pub fn build_model(inp: &ModelInput, store: &Store, now: i64) -> HudModel {
    let status = store.layer_status.filter(|s| s.extent_w > 0 && s.extent_h > 0);
    let extent = status.map(|s| [s.extent_w, s.extent_h]);
    // 1.6% of one eye's height on a stereo frame (design/SIGHT.md §15).
    let stereo = inp.stereo.as_ref().map(|s| s.stereo()).unwrap_or_default();
    let text_px = extent.map(|[w, h]| stereo.logical_extent((w, h)).1 as f32 * TEXT_FRACTION).unwrap_or(17.0);
    let has_frames = store.frames.frames > 0 || store.producers.get(super::catalog::PRODUCER_LAYER).is_some_and(|p| p.info.connected);
    let mut rows = Vec::new();
    let fg = store.fg_active(now);
    for r in &inp.layout.rows {
        if r.mods {
            rows.extend(mod_strips(store, r, now));
            continue;
        }
        let mut cells: Vec<HudCell> = Vec::new();
        let mut last_label: Option<String> = None;
        // A dropped cell's label goes to the next value in its group ("CPU" stays with the
        // temperature while the load has no sample yet). Labels after values stay theirs.
        let mut carry: Option<String> = None;
        for c in &r.cells {
            let own = c.short.clone().or_else(|| c.label.clone()).filter(|l| !l.is_empty());
            if let Some(w) = &c.when
                && !shown_condition(store, w, now)
            {
                if c.metric.is_some() && carry.is_none() {
                    carry = own;
                }
                continue;
            }
            let emphasis = c.emphasis.unwrap_or_default();
            if c.separator {
                last_label = None;
                carry = None;
                cells.push(HudCell::Separator);
            } else if c.spacer {
                cells.push(HudCell::Spacer);
            } else if c.split.is_some() {
                cells.extend(split_cell(store, c, now));
            } else if let Some(t) = &c.text {
                cells.push(HudCell::Text { text: t.clone(), level: c.level.unwrap_or(Level::Normal), style: CellStyle::emphasis(emphasis) });
            } else if c.graph.is_some() {
                if !has_frames {
                    continue;
                }
                let span_ms = c.span.as_ref().and_then(|s| s.ms()).unwrap_or(inp.cfg.graph_span_ms).clamp(1000, 10_000);
                let since = now - span_ms as i64 * 1_000_000;
                // The base series only while frame generation is on: without it, rendered
                // frames are the presented ones and a second line would only repeat the first.
                let mut series = if c.series.is_empty() { vec!["frametime".to_owned()] } else { c.series.clone() };
                series.retain(|s| s != "frametime.base" || fg);
                let desktop = inp.presenter == Presenter::Desktop;
                let points = if desktop { store.frames.tail(since) } else { Vec::new() };
                let base_points = if desktop && series.iter().any(|s| s == "frametime.base") { store.frames.base_tail(since) } else { Vec::new() };
                cells.push(HudCell::Graph { series, span_ms, rows: c.rows.unwrap_or(2), min_ms: 0.0, max_ms: 0.0, warn_above_ms: 33.4, bad_above_ms: 50.0, points, base_points });
            } else if let Some(id) = &c.metric {
                if let Some(s) = c.suffix.clone().filter(|s| !s.is_empty()) {
                    // "118 FPS": the label is the value's own, after it.
                    let style = CellStyle { emphasis, after: true, ..Default::default() };
                    cells.extend(metric_cell(store, c, id, now, Some(s), style));
                    continue;
                }
                let mut label = own.clone().or_else(|| carry.clone());
                if label.is_some() && label == last_label {
                    label = None;
                }
                match metric_cell(store, c, id, now, label.clone(), CellStyle::emphasis(emphasis)) {
                    Some(cell) => {
                        if label.is_some() {
                            last_label = label;
                        }
                        carry = None;
                        cells.push(cell);
                    }
                    None => {
                        if carry.is_none() {
                            carry = own;
                        }
                    }
                }
            }
        }
        // Tidy: no leading, trailing or doubled separators; drop text-only rows.
        let mut tidy: Vec<HudCell> = Vec::new();
        for c in cells {
            if matches!(c, HudCell::Separator) && tidy.last().is_none_or(|l| matches!(l, HudCell::Separator)) {
                continue;
            }
            tidy.push(c);
        }
        while matches!(tidy.last(), Some(HudCell::Separator | HudCell::Spacer)) {
            tidy.pop();
        }
        // A "·" joins two values: without one on each side it goes.
        let joins = |c: &HudCell| matches!(c, HudCell::Text { text, .. } if text == "·");
        let keep: Vec<bool> = (0..tidy.len())
            .map(|i| !joins(&tidy[i]) || (i > 0 && is_content(&tidy[i - 1]) && tidy.get(i + 1).is_some_and(is_content)))
            .collect();
        let mut keep = keep.into_iter();
        tidy.retain(|_| keep.next().unwrap_or(true));
        // A separator followed only by text (a gated group's leftovers) goes too.
        if tidy.iter().any(is_content) {
            rows.push(HudRow { cells: tidy, divider: r.divider });
        }
    }
    let background = inp.layout.background.unwrap_or(inp.cfg.background);
    HudModel {
        session: inp.session.to_owned(),
        product_id: inp.product_id,
        rev: 0,
        on: inp.on,
        layout: inp.layout.id.clone(),
        name: inp.layout.name.clone(),
        presenter: inp.presenter,
        presenter_reason: inp.reason.to_owned(),
        anchor: inp.layout.anchor.unwrap_or(inp.cfg.anchor),
        offset: inp.cfg.offset,
        scale: inp.cfg.scale,
        text_px,
        background,
        text_opacity: inp.cfg.text_opacity,
        level_colors: inp.cfg.level_colors,
        orientation: inp.layout.orientation(),
        safe_area: inp.cfg.safe_area,
        image: inp.image.map(Path::to_path_buf),
        extent,
        vk_format: status.map(|s| s.vk_format),
        color_space: status.map(|s| s.color_space),
        monitor: inp.monitor.map(str::to_owned),
        captures: inp.cfg.captures,
        size: fit_size(inp.layout, &rows, hud_card::panel(background)),
        rows,
        backdrop: inp.backdrop.clone().map(Box::new),
        game_focus: inp.game_focus,
        stereo: inp.stereo.clone(),
    }
}

/// The card's size for the rows a model shows (SIGHT-HUD §1.4, as built 2026-11-19): the
/// layout's width, and its height when it sets one, else the height of these rows' slots. A
/// Full card without frame generation or a mod is as tall as what it holds, not the 380 px
/// its every row would need. [`CardHold`] keeps it from shrinking while a session runs.
pub fn fit_size(layout: &Layout, rows: &[HudRow], panel: bool) -> [f32; 2] {
    let [w, h] = layout.size(panel);
    if layout.height.is_some() {
        return [w, h];
    }
    let slots: Vec<f32> = rows.iter().map(hud_card::row_height).collect();
    [w, hud_card::height(layout.orientation(), &slots, panel)]
}

/// The tallest card a session's layout has needed: the card grows when a row shows up (frame
/// generation turns on, a mod connects, the latency reports start) and keeps that height
/// while it runs, so a row that comes and goes (a gated tag's row, a latency that pauses on
/// a loading screen) never makes the card jump. Values changing never change the size: rows
/// with stale values keep their place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardHold {
    /// The layout (its id and rows), its width and whether it has a panel.
    key: String,
    height: f32,
}

impl CardHold {
    /// `m.size` at least as tall as before for the same layout; a new layout starts over.
    pub fn apply(&mut self, layout: &Layout, m: &mut HudModel) {
        let key = format!("{}|{}|{}|{:?}", layout.id, m.size[0], hud_card::panel(m.background), layout.rows);
        if self.key != key {
            *self = CardHold { key, height: m.size[1] };
            return;
        }
        self.height = self.height.max(m.size[1]);
        m.size[1] = self.height;
    }
}

/// The model with `rev` zeroed, to tell whether anything changed.
pub fn same(a: &HudModel, b: &HudModel) -> bool {
    let mut b = b.clone();
    b.rev = a.rev;
    *a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::catalog;
    use crate::perf::layout::builtin;
    use chrono::Utc;
    use quiver_perf::descriptor::{MetricDescriptor, RoleClaim, StateValue};
    use quiver_perf::layout::LayerStatus;

    const MS: i64 = 1_000_000;

    fn text_of(m: &HudModel) -> String {
        m.rows
            .iter()
            .map(|r| {
                let rule = if r.divider { "— " } else { "" };
                let cells: Vec<String> = r.cells.iter().map(|c| if matches!(c, HudCell::Spacer) { "~".into() } else { c.plain() }).collect();
                format!("{rule}{}", cells.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A session at 160 presented fps, with system metrics and, optionally, the layer
    /// counting 80 game frames a second (2× frame generation) with Reflex, PC latency 21 ms
    /// and NGX's log saying DLSS K. A mod claims the same roles with other numbers: they
    /// never show as the generic metrics.
    fn session(with_nv: bool) -> (Store, i64) {
        let mut s = Store::new(0, Utc::now());
        s.producer("layer", "Sight layer", true);
        s.producer("system", "System", true);
        for d in catalog::system() {
            s.declare("system", d);
        }
        if with_nv {
            super::super::nvidia::ensure(&mut s);
            s.producer("mod:aco-dlss", "aco-dlss", true);
            let mut c = MetricDescriptor::new("aco-dlss.frames.real", MetricKind::Counter);
            c.roles = vec![RoleClaim { role: catalog::ROLE_BASE_COUNT.into(), flags: vec![], on: None }];
            s.declare("mod:aco-dlss", c);
            let mut fg = MetricDescriptor::new("aco-dlss.fg.state", MetricKind::State);
            fg.values = vec![StateValue { value: "off".into(), label: None, level: Some(Level::Dim) }, StateValue { value: "on".into(), label: None, level: None }];
            fg.roles = vec![RoleClaim { role: "fg.state".into(), flags: vec![], on: None }];
            s.declare("mod:aco-dlss", fg);
            let mut up = MetricDescriptor::new("aco-dlss.upscaler", MetricKind::Text);
            up.roles = vec![RoleClaim { role: "upscaler".into(), flags: vec![], on: None }];
            s.declare("mod:aco-dlss", up);
            let mut lat = MetricDescriptor::new("aco-dlss.latency.mean", MetricKind::Gauge);
            lat.unit = Unit::Ms;
            lat.roles = vec![RoleClaim { role: catalog::ROLE_LATENCY_RENDER.into(), flags: vec![], on: None }];
            s.declare("mod:aco-dlss", lat);
        }
        let mut st = LayerStatus::ZERO;
        st.extent_w = 2560;
        st.extent_h = 1600;
        st.swapchain = 7;
        s.set_layer_status(st);
        let mut t = 0;
        for i in 0..2 * 160 {
            t = 1_000 * MS + i * 6_250_000;
            s.present(7, t, t);
            if with_nv && i % 2 == 0 {
                s.base_frame(7, t);
            }
            if with_nv && i % 16 == 0 {
                s.push_counter("nv.frames.app", t, (i / 2) as u64);
                s.push_counter("nv.frames.generated", t, (i / 2) as u64);
                s.push_counter("aco-dlss.frames.real", t, (i / 4) as u64);
            }
        }
        s.push_num("gpu.load", t, 97.0);
        s.push_num("gpu.temp", t, 71.0);
        s.push_num("gpu.power", t, 92.0);
        s.push_num("cpu.load", t, 38.0);
        s.push_num("cpu.temp", t, 64.0);
        if with_nv {
            s.push_state_value("aco-dlss.fg.state", t, "off");
            s.push_text("aco-dlss.upscaler", t, "TAA".into());
            s.push_num("aco-dlss.latency.mean", t, 99.0);
            s.push_text("dlss.sr", t, "DLSS K".into());
            s.push_num("nv.latency.pc", t, 21.0);
        }
        (s, t)
    }

    fn model(store: &Store, layout: &str, now: i64) -> HudModel {
        let cfg = HudConfig::default();
        let l = builtin(layout).unwrap();
        let inp = ModelInput { session: "s-1", product_id: 1, cfg: &cfg, on: true, layout: &l, presenter: Presenter::Game, reason: "", image: None, monitor: None, backdrop: None, game_focus: None, stereo: None };
        build_model(&inp, store, now)
    }

    #[test]
    fn compact_line_with_and_without_frame_generation() {
        let (s, now) = session(false);
        let m = model(&s, "compact", now);
        assert_eq!(text_of(&m), "160 FPS 6.2 ms 160 1% LOW │ GPU 97% 71° 92 W │ CPU 38% 64°");
        assert_eq!(m.text_px, 1600.0 * 0.016);
        let (s, now) = session(true);
        let m = model(&s, "compact", now);
        assert_eq!(text_of(&m), "160 FPS 80 BASE 6.2 ms 160 1% LOW 21.0 ms LAT │ GPU 97% 71° 92 W │ CPU 38% 64° │ DLSS K FG 2.0×");
    }

    #[test]
    fn fps_only_and_standard_drop_what_nobody_provides() {
        let (s, now) = session(false);
        assert_eq!(text_of(&model(&s, "fps", now)), "160 FPS");
        let m = model(&s, "standard", now);
        // The mock-up's "Standard, Vanilla profile": no base FPS, no latency, one graph line.
        assert_eq!(text_of(&m), "160 FPS\n[graph frametime]\n6.2 ms · 1% low 160\nGPU 97% 71° 92 W CPU 38% 64°");
        let (s, now) = session(true);
        assert_eq!(text_of(&model(&s, "fps", now)), "160 FPS 80 BASE");
        let t = text_of(&model(&s, "standard", now));
        assert_eq!(t, "160 FPS ~ 80 BASE\n[graph frametime+frametime.base]\n6.2 ms · 1% low 160 ~ FG 2.0×\n21.0 ms PC LATENCY\nGPU 97% 71° 92 W CPU 38% 64°");
        // A mod's numbers for generic roles never show as the generic metrics.
        assert!(!t.contains("99") && !t.contains("TAA"), "{t}");
        // Stale frames show —, and the model says so.
        let m = model(&s, "fps", now + 5_000 * MS);
        assert!(text_of(&m).contains('—'));
    }

    #[test]
    fn hierarchy_and_producers_reach_the_model() {
        let (s, now) = session(true);
        let m = model(&s, "standard", now);
        let cell = |id: &str| m.rows.iter().flat_map(|r| &r.cells).find(|c| c.metric() == Some(id)).unwrap().clone();
        let style = |id: &str| cell(id).style().unwrap().clone();
        assert_eq!((style("fps").emphasis, style("fps").after, style("fps").source), (Emphasis::Hero, true, SourceKind::Layer));
        assert_eq!((style("fps.base").emphasis, style("fps.base").source), (Emphasis::Secondary, SourceKind::Layer), "base FPS is the layer's Reflex counting");
        assert_eq!(style("gpu.load").source, SourceKind::System);
        assert_eq!((style("gpu.temp").emphasis, style("gpu.load").after), (Emphasis::Quiet, false));
        assert_eq!(style("fg.ratio").emphasis, Emphasis::Tag);
        assert!(matches!(cell("fg.ratio"), HudCell::Value { .. }), "a tag is a word, not a pill");
        // The hero's row is the taller one.
        assert_eq!(hud_card::row_height(&m.rows[0]), hud_card::HERO_LINE);
        // frametime.base: 1000 ÷ base FPS.
        assert_eq!(s.formatted("frametime.base", Stat::Mean, WINDOW_1S, now).unwrap().text, "12.5");
    }

    #[test]
    fn full_has_the_split_the_flags_and_a_strip_per_mod() {
        let (mut s, now) = session(true);
        // A mod with Reflex markers of its own fills the latency stages (not generic roles).
        for (role, ..) in catalog::LATENCY_SPLIT {
            let id = format!("aco-dlss.{}", role.trim_start_matches("latency.split."));
            let mut d = MetricDescriptor::new(&id, MetricKind::Gauge);
            d.unit = Unit::Ms;
            d.visibility = Some(quiver_perf::descriptor::Visibility::Advanced);
            d.roles = vec![RoleClaim { role: role.into(), flags: vec![], on: None }];
            s.declare("mod:aco-dlss", d);
            s.push_num(&id, now, if role.ends_with("render") { 9.0 } else { 6.0 });
        }
        s.declare("display", catalog::display().into_iter().find(|d| d.id == "display.vrr").unwrap());
        s.producer("display", "Display", true);
        s.push_state_value("display.vrr", now, "on");
        let m = model(&s, "full", now);
        let t = text_of(&m);
        let split = m.rows.iter().flat_map(|r| &r.cells).find_map(|c| if let HudCell::Split { parts, style, .. } = c { Some((parts.clone(), style.clone())) } else { None }).unwrap();
        assert_eq!(split.0.iter().map(|p| (p.label.as_str(), p.value)).collect::<Vec<_>>(), [("SIM", 6.0), ("RENDER", 9.0), ("QUEUE", 6.0), ("GPU", 6.0)]);
        assert_eq!((split.1.source, split.1.source_name.as_deref()), (SourceKind::Mod, Some("aco-dlss")));
        assert!(t.contains("[SIM 6 · RENDER 9 · QUEUE 6 · GPU 6]\n[SIM 6 · RENDER 9 · QUEUE 6 · GPU 6]"), "the bar and its numbers: {t}");
        assert!(t.contains("VRR on"), "{t}");
        // The mod's strip: its name and its basic metrics (not the counter, not the advanced
        // split parts), under a rule.
        let strip = m.rows.last().unwrap();
        assert!(strip.divider);
        assert_eq!(text_of(&HudModel { rows: vec![strip.clone()], ..m.clone() }), "— ACO-DLSS off · 99.0 ms · TAA");
        assert!(strip.cells.iter().filter(|c| c.metric().is_some()).all(|c| c.style().is_some_and(|s| s.source == SourceKind::Mod)));
        // Diagnostics has what Full dropped.
        let d = text_of(&model(&s, "diagnostics", now));
        assert!(d.starts_with("160 FPS 6.2 ms ~ TIME"), "{d}");
        assert!(!text_of(&model(&s, "full", now)).contains("TIME"));
    }

    #[test]
    fn the_base_series_follows_frame_generation() {
        let (s, now) = session(true);
        let cfg = HudConfig::default();
        let l = builtin("standard").unwrap();
        let inp = ModelInput { session: "s-1", product_id: 1, cfg: &cfg, on: true, layout: &l, presenter: Presenter::Desktop, reason: "", image: None, monitor: None, backdrop: None, game_focus: None, stereo: None };
        let m = build_model(&inp, &s, now);
        let Some(HudCell::Graph { series, points, base_points, .. }) = m.rows.iter().flat_map(|r| &r.cells).find(|c| matches!(c, HudCell::Graph { .. })) else { panic!() };
        assert_eq!(series, &["frametime", "frametime.base"]);
        assert!(points.len() > 300 && base_points.len() > 150, "{} {}", points.len(), base_points.len());
        assert!(base_points.iter().all(|ms| (ms - 12.5).abs() < 0.01) && points.iter().all(|ms| (ms - 6.25).abs() < 0.01));
        // FG off: one line.
        let (plain, now) = session(false);
        let m = build_model(&inp, &plain, now);
        let Some(HudCell::Graph { series, base_points, .. }) = m.rows.iter().flat_map(|r| &r.cells).find(|c| matches!(c, HudCell::Graph { .. })) else { panic!() };
        assert_eq!((series.as_slice(), base_points.len()), (&["frametime".to_owned()][..], 0));
    }

    #[test]
    fn a_dropped_cells_label_moves_to_the_next() {
        let (mut s, now) = session(false);
        // A fresh session: CPU load needs two readings, the temperature is there.
        let mut fresh = Store::new(0, Utc::now());
        fresh.producer("system", "System", true);
        for d in catalog::system() {
            fresh.declare("system", d);
        }
        fresh.push_num("cpu.temp", now, 64.0);
        assert_eq!(text_of(&model(&fresh, "compact", now)), "CPU 64°");
        // Not across a separator.
        s.push_num("gpu.load", now, 50.0);
        assert!(text_of(&model(&s, "compact", now)).contains("GPU 50%"));
    }

    #[test]
    fn the_card_fits_the_rows_it_shows() {
        let (plain, now) = session(false);
        let (modded, now2) = session(true);
        let fits = |m: &HudModel| {
            let slots: Vec<f32> = m.rows.iter().map(hud_card::row_height).collect();
            hud_card::height(m.orientation, &slots, hud_card::panel(m.background))
        };
        for id in crate::perf::layout::PRESETS {
            let a = model(&plain, id, now);
            let b = model(&modded, id, now2);
            assert_eq!(a.size[0], hud_card::preset_width(id).unwrap(), "{id}");
            assert_eq!(a.size[0], b.size[0], "{id}: the width is the layout's");
            assert_eq!(a.size[1], fits(&a), "{id}: as tall as its rows");
            assert_eq!(b.size[1], fits(&b), "{id}");
            // Stale values (— everywhere) keep their rows, so the size holds.
            assert_eq!(model(&modded, id, now2 + 5_000 * MS).size, b.size, "{id}");
        }
        // Full without frame generation or a mod (Alan Wake Remastered): no empty half.
        let vanilla = model(&plain, "full", now);
        let every_row = builtin("full").unwrap().size(true)[1];
        assert!(vanilla.size[1] < every_row * 0.6, "{} of {every_row}", vanilla.size[1]);
        // Standard with FG: the hero's line, a two-row graph and three lines, in a panel.
        let m = model(&modded, "standard", now2);
        assert_eq!(m.size, [284.0, 30.0 + 30.0 + 3.0 * 20.0 + 4.0 * 4.0 + 2.0 * (6.0 + 1.0)]);
        assert_eq!(model(&plain, "fps", now).size, [150.0, 20.0 + 2.0 * (6.0 + 1.0)], "a panel by default, as in the mock-up");
        // Without a panel the padding goes, the slots stay.
        let cfg = HudConfig { background: 0.0, ..HudConfig::default() };
        let l = builtin("standard").unwrap();
        let inp = ModelInput { session: "s-1", product_id: 1, cfg: &cfg, on: true, layout: &l, presenter: Presenter::Game, reason: "", image: None, monitor: None, backdrop: None, game_focus: None, stereo: None };
        assert_eq!(build_model(&inp, &modded, now2).size, [284.0, 30.0 + 30.0 + 3.0 * 20.0 + 4.0 * 4.0]);
        // A layout that sets its height keeps it.
        let sized = crate::perf::layout::parse("x", "height = 200\n[[row]]\ncells = [{ metric = \"fps\" }]\n[[row]]\ncells = [{ metric = \"fps.base\" }]").unwrap();
        let cfg = HudConfig::default();
        let inp = ModelInput { layout: &sized, cfg: &cfg, ..inp };
        assert_eq!(build_model(&inp, &plain, now).size[1], 200.0);
    }

    #[test]
    fn the_card_grows_with_a_row_and_never_jumps_back() {
        let (plain, now) = session(false);
        let (modded, now2) = session(true);
        let l = builtin("full").unwrap();
        let mut hold = CardHold::default();
        let sized = |m: HudModel, hold: &mut CardHold| {
            let mut m = m;
            hold.apply(&l, &mut m);
            m.size
        };
        let small = sized(model(&plain, "full", now), &mut hold);
        assert_eq!(small, model(&plain, "full", now).size, "the first model sets it");
        // FG turns on and the latency comes: the card grows to fit.
        let big = sized(model(&modded, "full", now2), &mut hold);
        assert!(big[1] > small[1]);
        // FG off again (a loading screen): the card keeps its height, no jump.
        assert_eq!(sized(model(&plain, "full", now), &mut hold), big);
        // Values moving never change it.
        assert_eq!(sized(model(&modded, "full", now2 + 5_000 * MS), &mut hold), big);
        // Another layout starts over.
        let s = builtin("standard").unwrap();
        let mut m = model(&plain, "standard", now);
        let want = m.size;
        hold.apply(&s, &mut m);
        assert_eq!(m.size, want);
    }

    #[test]
    fn model_change_detection_ignores_rev() {
        let (s, now) = session(false);
        let a = model(&s, "compact", now);
        let mut b = a.clone();
        b.rev = 9;
        assert!(same(&a, &b));
        b.on = false;
        assert!(!same(&a, &b));
    }

    fn layer(status: Option<LayerStatus>) -> LayerRing {
        LayerRing { pid: 1, alive: true, heartbeat: 0, status, ring: "ok".into(), ..Default::default() }
    }

    #[test]
    fn presenter_choice() {
        let auto = PresenterSetting::Auto;
        let now = 100_000 * MS;
        assert_eq!(choose_presenter(auto, false, None, None, 1_000 * MS, now).0, Presenter::Game, "grace while the game loads");
        assert_eq!(choose_presenter(auto, false, None, None, 20_000 * MS, now).0, Presenter::Desktop);
        assert_eq!(choose_presenter(PresenterSetting::Desktop, false, Some(&layer(None)), None, 0, now).0, Presenter::Desktop);
        assert_eq!(choose_presenter(auto, true, Some(&layer(None)), None, 0, now).0, Presenter::Desktop);
        let mut st = LayerStatus::ZERO;
        st.draw_error = DRAW_ERR_NONE;
        assert_eq!(choose_presenter(auto, false, Some(&layer(Some(st))), None, 0, now).0, Presenter::Game);
        // HDR before phase 2.
        let mut hdr = st;
        hdr.color_space = 1_000_104_008;
        assert_eq!(choose_presenter(auto, false, Some(&layer(Some(hdr))), None, 0, now).0, Presenter::Desktop);
        // A format the compositor can't do; a panic.
        let mut bad = st;
        bad.draw_error = DRAW_ERR_FORMAT;
        let (p, why) = choose_presenter(auto, false, Some(&layer(Some(bad))), None, 0, now);
        assert_eq!((p, why.as_str()), (Presenter::Desktop, "the layer can't draw (format)"));
        let mut pan = st;
        pan.flags = LAYER_PANICKED;
        assert_eq!(choose_presenter(auto, false, Some(&layer(Some(pan))), None, 0, now).0, Presenter::Desktop);
        // Waiting for an image is fine; an image it won't take for 2 s isn't.
        let mut wait = st;
        wait.draw_error = DRAW_ERR_NO_IMAGE;
        let img = ImageView { published: 3, heartbeat: now - 100 * MS, changed_at: now - 500 * MS, ..Default::default() };
        assert_eq!(choose_presenter(auto, false, Some(&layer(Some(wait))), Some(&img), 0, now).0, Presenter::Game);
        let old = ImageView { changed_at: now - 3_000 * MS, ..img };
        assert_eq!(choose_presenter(auto, false, Some(&layer(Some(wait))), Some(&old), 0, now).0, Presenter::Desktop);
        // A dead or silent layer.
        let dead = LayerRing { alive: false, ..layer(Some(st)) };
        assert_eq!(choose_presenter(auto, false, Some(&dead), None, 0, now).0, Presenter::Desktop);
        let silent = LayerRing { heartbeat: now - 5_000 * MS, ..layer(Some(st)) };
        assert_eq!(choose_presenter(auto, false, Some(&silent), None, 0, now).0, Presenter::Desktop);
    }

    #[test]
    fn layer_backdrop_state() {
        let now = 100_000 * MS;
        let img = Path::new("/dev/shm/quiver-1000/s-1/hud.image");
        // No hud.image: nowhere to ask.
        assert_eq!(layer_backdrop(None, Some(&layer(None)), now), None);
        let no = |l: Option<&LayerRing>| layer_backdrop(Some(img), l, now).map(|b| (b.ready, b.reason));
        assert_eq!(no(None), Some((false, "no Sight layer in the game".into())));
        assert_eq!(no(Some(&layer(None))), Some((false, "waiting for the layer's first swapchain".into())));
        // A layer from before the blur: zero flags, zero error.
        let old = LayerStatus::ZERO;
        assert_eq!(no(Some(&layer(Some(old)))).unwrap().1, "the layer can't blur this swapchain (not reported (a layer from before the backdrop blur))");
        let mut usage = old;
        usage.backdrop_error = BACKDROP_ERR_USAGE as u16;
        assert_eq!(no(Some(&layer(Some(usage)))).unwrap().1, "the layer can't blur this swapchain (usage)");
        assert_eq!(backdrop_status(&usage), "unavailable: usage");
        // Ready, then blurring (HDR swapchains too: the HUD's presenter doesn't matter).
        let mut ready = old;
        ready.flags = LAYER_BLUR_READY;
        ready.color_space = 1_000_104_008;
        let b = layer_backdrop(Some(img), Some(&layer(Some(ready))), now).unwrap();
        assert_eq!((b.ready, b.blurring, b.image.as_path()), (true, false, img));
        assert_eq!(backdrop_status(&ready), "ready");
        let mut blurring = ready;
        blurring.flags |= LAYER_BLURRING;
        blurring.backdrop_gpu_ns = 420_000;
        assert!(layer_backdrop(Some(img), Some(&layer(Some(blurring))), now).unwrap().blurring);
        assert_eq!(backdrop_status(&blurring), "blurring, 0.42 ms GPU");
        // A panicked or silent layer isn't ready whatever its flags say.
        let pan = LayerStatus { flags: LAYER_BLUR_READY | LAYER_PANICKED, ..old };
        assert!(!layer_backdrop(Some(img), Some(&layer(Some(pan))), now).unwrap().ready);
        let silent = LayerRing { heartbeat: now - 5_000 * MS, ..layer(Some(ready)) };
        assert!(!layer_backdrop(Some(img), Some(&silent), now).unwrap().ready);
    }

    #[test]
    fn game_focus_from_the_layer() {
        let now = 100_000 * MS;
        assert_eq!(game_focus(None, now), None);
        assert_eq!(game_focus(Some(&layer(None)), now), None, "no status yet");
        // A layer from before focus reporting (or one that can't tell): unknown.
        assert_eq!(game_focus(Some(&layer(Some(LayerStatus::ZERO))), now), None);
        let away = LayerStatus { flags: LAYER_FOCUS_KNOWN, ..LayerStatus::ZERO };
        assert_eq!(game_focus(Some(&layer(Some(away))), now), Some(false));
        let front = LayerStatus { flags: LAYER_FOCUS_KNOWN | LAYER_FOCUSED, ..LayerStatus::ZERO };
        assert_eq!(game_focus(Some(&layer(Some(front))), now), Some(true));
        // FOCUSED without KNOWN means nothing; a silent layer is unknown.
        let odd = LayerStatus { flags: LAYER_FOCUSED, ..LayerStatus::ZERO };
        assert_eq!(game_focus(Some(&layer(Some(odd))), now), None);
        let silent = LayerRing { heartbeat: now - 5_000 * MS, ..layer(Some(front)) };
        assert_eq!(game_focus(Some(&silent), now), None);
    }
}
