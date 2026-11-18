//! The HUD model builder (§2.7) and the presenter choice (§2.6).
//!
//! quiverd resolves the session's layout, formats every value (decision 3) and sends
//! `quiver-sight` a [`HudModel`]: rows of cells with text and theme levels, no pixels.

use std::path::{Path, PathBuf};

use quiver_api::perf::{HudCell, HudConfig, HudImageStatus, HudModel, HudRow, Level, MetricState, PerfValue, Presenter, PresenterSetting, Stat, hud_card};
use quiver_perf::descriptor::{Display, MetricKind, Range, Unit};
use quiver_perf::layout::{DRAW_ERR_DEVICE_LOST, DRAW_ERR_FORMAT, DRAW_ERR_INTERNAL, DRAW_ERR_NO_IMAGE, DRAW_ERR_NONE, DRAW_ERR_QUEUE, DRAW_ERR_USAGE, LAYER_PANICKED};

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

fn metric_cell(store: &Store, c: &Cell, id: &str, now: i64, label: Option<String>) -> Option<HudCell> {
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
    let display = c.display.or(desc.display).unwrap_or(if desc.kind == MetricKind::State { Display::Badge } else { Display::Number });
    let metric = id.to_owned();
    Some(match display {
        Display::Badge => HudCell::Badge { metric, label, value },
        Display::Bar => {
            let fraction = fraction(&desc, value.value);
            HudCell::Bar { metric, label, value, fraction }
        }
        Display::Sparkline | Display::Graph => {
            let tail = store.tail(id, SPARK_POINTS);
            let (lo, hi) = tail.iter().fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
            let points = tail.iter().map(|v| if hi > lo { ((v - lo) / (hi - lo)) as f32 } else { 0.5 }).collect();
            HudCell::Sparkline { metric, label, value, points }
        }
        Display::Number | Display::Stack => HudCell::Value { metric, label, value },
    })
}

fn is_content(c: &HudCell) -> bool {
    !matches!(c, HudCell::Text { .. } | HudCell::Separator)
}

/// Builds the model (rev 0: the caller numbers it).
pub fn build_model(inp: &ModelInput, store: &Store, now: i64) -> HudModel {
    let status = store.layer_status.filter(|s| s.extent_w > 0 && s.extent_h > 0);
    let extent = status.map(|s| [s.extent_w, s.extent_h]);
    let text_px = extent.map(|[_, h]| h as f32 * TEXT_FRACTION).unwrap_or(17.0);
    let has_frames = store.frames.frames > 0 || store.producers.get(super::catalog::PRODUCER_LAYER).is_some_and(|p| p.info.connected);
    let mut rows = Vec::new();
    for r in &inp.layout.rows {
        let mut cells: Vec<HudCell> = Vec::new();
        let mut last_label: Option<String> = None;
        // A dropped cell's label goes to the next value in its group ("CPU" stays with the
        // temperature while the load has no sample yet).
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
            if c.separator {
                last_label = None;
                carry = None;
                cells.push(HudCell::Separator);
            } else if let Some(t) = &c.text {
                cells.push(HudCell::Text { text: t.clone(), level: c.level.unwrap_or(Level::Normal) });
            } else if c.graph.is_some() {
                if !has_frames {
                    continue;
                }
                let span_ms = c.span.as_ref().and_then(|s| s.ms()).unwrap_or(inp.cfg.graph_span_ms).clamp(1000, 10_000);
                let points = if inp.presenter == Presenter::Desktop { store.frames.tail(now - span_ms as i64 * 1_000_000) } else { Vec::new() };
                let series = if c.series.is_empty() { vec!["frametime".to_owned()] } else { c.series.clone() };
                cells.push(HudCell::Graph { series, span_ms, rows: c.rows.unwrap_or(2), min_ms: 0.0, max_ms: 0.0, warn_above_ms: 33.4, bad_above_ms: 50.0, points });
            } else if let Some(id) = &c.metric {
                let mut label = own.clone().or_else(|| carry.clone());
                if label.is_some() && label == last_label {
                    label = None;
                }
                match metric_cell(store, c, id, now, label.clone()) {
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
        while matches!(tidy.last(), Some(HudCell::Separator)) {
            tidy.pop();
        }
        // A separator followed only by text (a gated group's leftovers) goes too.
        if tidy.iter().any(is_content) {
            rows.push(HudRow { cells: tidy });
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
        size: inp.layout.size(hud_card::panel(background)),
        rows,
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
                r.cells
                    .iter()
                    .map(|c| match c {
                        HudCell::Value { label, value, .. } | HudCell::Badge { label, value, .. } | HudCell::Bar { label, value, .. } | HudCell::Sparkline { label, value, .. } => {
                            format!("{}{}", label.as_ref().map(|l| format!("{l} ")).unwrap_or_default(), value.display())
                        }
                        HudCell::Text { text, .. } => text.clone(),
                        HudCell::Separator => "│".into(),
                        HudCell::Graph { .. } => "[graph]".into(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
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
        let inp = ModelInput { session: "s-1", product_id: 1, cfg: &cfg, on: true, layout: &l, presenter: Presenter::Game, reason: "", image: None, monitor: None };
        build_model(&inp, store, now)
    }

    #[test]
    fn compact_line_with_and_without_frame_generation() {
        let (s, now) = session(false);
        let m = model(&s, "compact", now);
        assert_eq!(text_of(&m), "FPS 160 6.2 ms 1%↓ 160 │ GPU 97% 71° 92 W │ CPU 38% 64°");
        assert_eq!(m.text_px, 1600.0 * 0.016);
        let (s, now) = session(true);
        let m = model(&s, "compact", now);
        assert_eq!(text_of(&m), "FPS 80 → 160 6.2 ms 1%↓ 160 │ GPU 97% 71° 92 W │ CPU 38% 64° │ DLSS K FG 2.0× PC 21.0 ms");
    }

    #[test]
    fn fps_only_and_standard_drop_what_nobody_provides() {
        let (s, now) = session(false);
        assert_eq!(text_of(&model(&s, "fps", now)), "160");
        let m = model(&s, "standard", now);
        let t = text_of(&m);
        assert!(t.starts_with("FPS 160 1% low 160\nFT 6.2 ms\n[graph]\nGPU 97% 71° 92 W"), "{t}");
        assert!(!t.contains("LAT"), "no latency provider: the row is gone");
        let (s, now) = session(true);
        assert_eq!(text_of(&model(&s, "fps", now)), "80 → 160");
        let t = text_of(&model(&s, "standard", now));
        assert!(t.contains("PC 21.0 ms") && !t.contains("99") && !t.contains("TAA"), "{t}");
        // Stale frames show —, and the model says so.
        let m = model(&s, "fps", now + 5_000 * MS);
        assert!(text_of(&m).contains('—'));
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
    fn the_card_keeps_the_layouts_size_whatever_it_holds() {
        let (plain, now) = session(false);
        let (modded, now2) = session(true);
        for id in ["fps", "compact", "standard", "full", "clock"] {
            let a = model(&plain, id, now);
            let b = model(&modded, id, now2);
            // FG on and an upscaler (cells and a whole row appear) or not: the same card.
            if id != "clock" {
                assert_ne!(text_of(&a), text_of(&b), "{id}");
            }
            assert_eq!(a.size, b.size, "{id}");
            assert_eq!(a.size[0], hud_card::preset_width(id).unwrap(), "{id}");
            // Stale values (— everywhere) too.
            assert_eq!(model(&modded, id, now2 + 5_000 * MS).size, b.size, "{id}");
        }
        // Standard: six lines and a two-row graph, in a panel.
        let m = model(&modded, "standard", now2);
        assert_eq!(m.size, [260.0, 6.0 * 20.0 + 30.0 + 6.0 * 4.0 + 2.0 * (6.0 + 1.0)]);
        assert_eq!(model(&plain, "fps", now).size, [100.0, 20.0], "bare: no padding");
        // Without a panel the padding goes, the slots stay.
        let cfg = HudConfig { background: 0.0, ..HudConfig::default() };
        let l = builtin("standard").unwrap();
        let inp = ModelInput { session: "s-1", product_id: 1, cfg: &cfg, on: true, layout: &l, presenter: Presenter::Game, reason: "", image: None, monitor: None };
        assert_eq!(build_model(&inp, &modded, now2).size, [260.0, 6.0 * 20.0 + 30.0 + 6.0 * 4.0]);
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
}
