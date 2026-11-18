//! The HUD view: a [`HudModel`]'s rows and cells drawn with `quiver-ui`'s theme tokens and
//! fonts (Geist for labels, Geist Mono for numbers, SIGHT-HUD §1.2). The same view renders
//! off-screen into `hud.image` (game presenter) and into a layer-shell surface (desktop
//! presenter); only the graph differs: a placeholder rectangle the layer fills, recorded in
//! [`Measure`] for the image descriptor, or a line drawn here from the model's points.
//!
//! Sizes are logical px times `unit`. Off-screen, `unit` is 1 and the window's scale factor
//! carries the swapchain-derived size; on the desktop, the output's scale is the window's and
//! `unit` is the model's `scale`.
//!
//! The card has the model's fixed size (`HudModel::size`, [`hud_card`]); its contents never
//! resize it. Rows sit in fixed-height slots packed from the top; within a row cells go left
//! to right and what doesn't fit is cut: text values and badges shrink first with an
//! ellipsis, numbers never shrink. A number keeps a reserved width (tabular digits, right
//! aligned, at least the unit's usual digit count and the widest it has been for this
//! layout), so values ticking don't move the cells after them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Context, Div, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Render, Styled, Window, canvas,
    div, point, prelude::*, px, size,
};
use quiver_api::perf::{Anchor, HudCell, HudModel, Level, Orientation, PerfValue, hud_card};
use quiver_ui::assets::{MONO_FONT, UI_FONT};
use quiver_ui::components::{MEDIUM, SEMIBOLD, mono};
use quiver_ui::theme::Theme;

/// Base value text size (logical px at unit 1). The image's scale factor maps it to the
/// model's `text_px` (1.6% of the swapchain height) times its `scale`.
pub const BASE_TEXT: f32 = 12.0;

/// A graph placeholder as laid out (window logical px), with what the layer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphRect {
    pub bounds: Bounds<Pixels>,
    pub span_ms: u32,
    pub min_ms: f32,
    pub max_ms: f32,
    pub warn_above_ms: f32,
    pub bad_above_ms: f32,
}

/// What the last frame laid out, read by the owner after rendering.
#[derive(Debug, Default, Clone)]
pub struct Measure {
    /// The HUD card (window logical px; its origin is the window's).
    pub card: Option<Bounds<Pixels>>,
    pub graphs: Vec<GraphRect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Off-screen into `hud.image`: graphs are placeholders the layer draws into.
    Image,
    /// On a layer-shell surface: graphs drawn here; the window follows the card's size.
    Desktop,
}

pub struct HudView {
    pub model: HudModel,
    pub unit: f32,
    pub mode: Mode,
    pub measure: Rc<RefCell<Measure>>,
    /// The widest each value has been (chars of text, chars of unit), by metric and label,
    /// for the layout in `reserved_for`.
    reserved: HashMap<String, (usize, usize)>,
    reserved_for: String,
}

impl HudView {
    pub fn new(model: HudModel, unit: f32, mode: Mode) -> Self {
        Self { model, unit, mode, measure: Rc::default(), reserved: HashMap::new(), reserved_for: String::new() }
    }

    /// Folds the model's values into the reserved widths (reset when the layout changes).
    fn reserve(&mut self) {
        if self.reserved_for != self.model.layout {
            self.reserved.clear();
            self.reserved_for = self.model.layout.clone();
        }
        for c in self.model.rows.iter().flat_map(|r| &r.cells) {
            if let Some((key, v)) = value_of(c) {
                let e = self.reserved.entry(key).or_default();
                let numeric = v.value.is_some();
                let text = v.text.chars().count().max(if numeric { usual_digits(&v.unit) } else { 0 });
                *e = (e.0.max(text), e.1.max(v.unit.chars().count()));
            }
        }
    }
}

/// A value cell's reservation key and value.
fn value_of(c: &HudCell) -> Option<(String, &PerfValue)> {
    match c {
        HudCell::Value { metric, label, value } | HudCell::Bar { metric, label, value, .. } | HudCell::Sparkline { metric, label, value, .. } => {
            Some((format!("{metric}/{}", label.as_deref().unwrap_or("")), value))
        }
        _ => None,
    }
}

/// How many characters a number in `unit` usually needs ("158", "12.7 ms", "97%", "71°"),
/// reserved from the first frame so the common case never moves.
pub fn usual_digits(unit: &str) -> usize {
    match unit {
        "°" => 2,
        "" | "%" | "W" | "×" | "Hz" | "px" => 3,
        _ => 4,
    }
}

/// The card's size (logical px at unit 1): the model's, or for a model without one the
/// default width and the height of its rows.
pub fn model_size(m: &HudModel) -> (f32, f32) {
    let panel = hud_card::panel(m.background);
    let [w, h] = m.size;
    let w = if w > 0.0 { hud_card::clamp_width(w, hud_card::default_width(m.orientation)) } else { hud_card::default_width(m.orientation) };
    let fallback = || {
        let slots: Vec<f32> = m.rows.iter().map(|r| hud_card::row_slot(&r.cells)).collect();
        hud_card::height(m.orientation, &slots, panel)
    };
    let h = if h > 0.0 { hud_card::clamp_height(h, fallback()) } else { fallback() };
    (w, h)
}

/// Theme colours for a level (§1.2: levels, never raw colours). With level colours off,
/// only `dim` keeps its colour.
pub fn level_color(th: &Theme, level: Level, level_colors: bool) -> Hsla {
    match (level, level_colors) {
        (Level::Dim, _) => th.tx3,
        (Level::Normal, _) | (_, false) => th.tx1,
        (Level::Good, true) => th.ok,
        (Level::Warn, true) => th.warn,
        (Level::Bad, true) => th.bad,
        (Level::Accent, true) => th.acc_hi,
    }
}

fn soft_color(th: &Theme, level: Level, level_colors: bool) -> (Hsla, Hsla) {
    match (level, level_colors) {
        (Level::Good, true) => (th.ok, th.ok_soft),
        (Level::Warn, true) => (th.warn, th.warn_soft),
        (Level::Bad, true) => (th.bad, th.bad_soft),
        (Level::Accent, true) => (th.acc_hi, th.acc_soft),
        (Level::Dim, _) => (th.tx3, th.ink4),
        _ => (th.tx1, th.ink4),
    }
}

struct Ctx<'a> {
    th: &'a Theme,
    u: f32,
    text_alpha: f32,
    level_colors: bool,
    /// The value font's advance (tabular digits) per px of font size.
    advance: f32,
    reserved: &'a HashMap<String, (usize, usize)>,
}

impl Ctx<'_> {
    fn px(&self, v: f32) -> Pixels {
        px(v * self.u)
    }

    fn fade(&self, c: Hsla) -> Hsla {
        c.opacity(self.text_alpha)
    }

    fn label(&self, text: &str) -> Div {
        div()
            .font_family(UI_FONT)
            .text_size(self.px(BASE_TEXT * 0.78))
            .font_weight(SEMIBOLD)
            .text_color(self.fade(self.th.tx3))
            .whitespace_nowrap()
            .child(text.to_owned())
    }

    /// A value: a number in its reserved width (right aligned, never shrinks), or text that
    /// shrinks with an ellipsis when the row runs out of room.
    fn value(&self, key: &str, v: &PerfValue) -> Div {
        let color = if v.stale { self.th.tx3 } else { level_color(self.th, v.level, self.level_colors) };
        let (chars, unit_chars) = self.reserved.get(key).copied().unwrap_or((v.text.chars().count(), v.unit.chars().count()));
        let fixed = !shrinks(v);
        // Numbers line up on their last digit; text starts where it starts.
        let right = v.value.is_some() || v.stale;
        // Short text keeps at most a short text's width (it was long once: that doesn't
        // make it unshrinkable now).
        let chars = if right { chars } else { chars.min(SHORT_TEXT).max(v.text.chars().count()) };
        // A little slack over the advance so rounding at odd scales never truncates.
        let w = |n: usize, size: f32| px((n as f32 * self.advance * BASE_TEXT * size + 0.5) * self.u);
        let text = mono(BASE_TEXT * self.u, self.fade(color)).font_weight(SEMIBOLD).whitespace_nowrap();
        let text = if fixed {
            text.flex_none().w(w(chars, 1.0)).when(right, |d| d.text_right()).child(v.text.clone())
        } else {
            text.flex_shrink(1.).min_w_0().truncate().child(v.text.clone())
        };
        let d = div().flex().items_baseline().gap(self.px(1.5)).whitespace_nowrap().child(text);
        let d = if fixed { d.flex_none() } else { d.flex_shrink(1.).min_w_0().overflow_hidden() };
        if unit_chars == 0 {
            d
        } else {
            d.child(mono(BASE_TEXT * 0.78 * self.u, self.fade(self.th.tx2)).font_weight(MEDIUM).flex_none().min_w(w(unit_chars, 0.78)).child(v.unit.clone()))
        }
    }

    fn group(&self, shrink: bool) -> Div {
        let g = div().flex().items_center().gap(self.px(4.));
        if shrink { g.flex_shrink(1.).min_w_0().overflow_hidden() } else { g.flex_none() }
    }

    fn with_label(&self, label: &Option<String>, shrink: bool) -> Div {
        let g = self.group(shrink);
        match label {
            Some(l) => g.child(self.label(l).flex_none()),
            None => g,
        }
    }
}

/// Text up to this long (states: "on", "direct", "DLSS K"; resolutions: "2560×1600") keeps
/// its width like a number.
const SHORT_TEXT: usize = 9;

/// Longer text values shrink (with an ellipsis); numbers, short text, and stale values keep
/// their width.
fn shrinks(v: &PerfValue) -> bool {
    v.value.is_none() && !v.stale && v.text.chars().count() > SHORT_TEXT
}

impl HudView {
    fn cell(&self, cx: &Ctx, c: &HudCell) -> AnyElement {
        match c {
            HudCell::Value { label, value, .. } => {
                let key = value_of(c).map(|(k, _)| k).unwrap_or_default();
                cx.with_label(label, shrinks(value)).child(cx.value(&key, value)).into_any_element()
            }
            HudCell::Badge { label, value, .. } => {
                let (fg, bg) = if value.stale { (cx.th.tx3, cx.th.ink4) } else { soft_color(cx.th, value.level, cx.level_colors) };
                let text = match label {
                    Some(l) => format!("{l} {}", value.display()),
                    None => value.display(),
                };
                // Long badges give way first (an upscaler's full name before the resolutions).
                let long = text.chars().count() > SHORT_TEXT;
                div()
                    .when(long, |d| d.flex_shrink(3.).min_w_0())
                    .when(!long, |d| d.flex_none())
                    .truncate()
                    .px(cx.px(6.))
                    .py(cx.px(1.))
                    .rounded_full()
                    .bg(bg.opacity(bg.a * cx.text_alpha))
                    .font_family(UI_FONT)
                    .text_size(cx.px(BASE_TEXT * 0.82))
                    .font_weight(SEMIBOLD)
                    .text_color(cx.fade(fg))
                    .whitespace_nowrap()
                    .child(text)
                    .into_any_element()
            }
            HudCell::Bar { label, value, fraction, .. } => {
                let fill = match value.level {
                    Level::Normal | Level::Dim => cx.th.acc,
                    l => level_color(cx.th, l, cx.level_colors),
                };
                let bar = div()
                    .w(cx.px(40.))
                    .h(cx.px(4.))
                    .rounded(cx.px(2.))
                    .bg(cx.th.track)
                    .overflow_hidden()
                    .child(div().h_full().rounded(cx.px(2.)).bg(cx.fade(fill)).w(gpui::relative(fraction.clamp(0.0, 1.0))));
                let key = value_of(c).map(|(k, _)| k).unwrap_or_default();
                cx.with_label(label, false).child(bar.flex_none()).child(cx.value(&key, value)).into_any_element()
            }
            HudCell::Sparkline { label, value, points, .. } => {
                let h = 11.0;
                let spark = div().flex().flex_none().items_end().gap(cx.px(1.)).h(cx.px(h)).children(points.iter().map(|p| {
                    div().w(cx.px(2.)).h(cx.px((p.clamp(0.0, 1.0) * h).max(1.0))).rounded(cx.px(1.)).bg(cx.fade(cx.th.acc.opacity(0.85)))
                }));
                let key = value_of(c).map(|(k, _)| k).unwrap_or_default();
                cx.with_label(label, false).child(spark).child(cx.value(&key, value)).into_any_element()
            }
            HudCell::Graph { span_ms, rows, min_ms, max_ms, warn_above_ms, bad_above_ms, points, .. } => {
                let rect = GraphRect {
                    bounds: Bounds::default(),
                    span_ms: *span_ms,
                    min_ms: *min_ms,
                    max_ms: *max_ms,
                    warn_above_ms: *warn_above_ms,
                    bad_above_ms: *bad_above_ms,
                };
                self.graph(cx, rect, (*rows).clamp(1, 6), points.clone())
            }
            HudCell::Text { text, level } => div()
                .flex_none()
                .font_family(UI_FONT)
                .text_size(cx.px(BASE_TEXT * 0.9))
                .text_color(cx.fade(level_color(cx.th, *level, cx.level_colors)))
                .whitespace_nowrap()
                .child(text.clone())
                .into_any_element(),
            HudCell::Separator => div().flex_none().w(px(1.0f32.max(cx.u))).h(cx.px(BASE_TEXT * 0.9)).bg(cx.fade(cx.th.line2)).into_any_element(),
        }
    }

    /// The graph: a quiet track the layer draws the live frame times into (image), or the
    /// frame times drawn here (desktop). Its laid-out bounds go into the descriptor.
    fn graph(&self, cx: &Ctx, rect: GraphRect, rows: u32, points: Vec<f32>) -> AnyElement {
        let measure = self.measure.clone();
        let desktop = self.mode == Mode::Desktop;
        let line = cx.fade(cx.th.acc);
        let warn = cx.fade(cx.th.warn);
        let u = cx.u;
        let height = hud_card::slot(Some(rows));
        let d = div().relative();
        // Its own width on one line; the rest of the row in a block.
        let d = if self.model.orientation == Orientation::Row { d.flex_none().w(cx.px(110.)) } else { d.flex_1().min_w_0() };
        d.h(cx.px(height))
            .rounded(cx.px(3.))
            .bg(cx.th.tx1.opacity(0.05 * cx.text_alpha))
            .child(
                canvas(
                    move |bounds, _, _| {
                        measure.borrow_mut().graphs.push(GraphRect { bounds, ..rect.clone() });
                        (bounds, rect)
                    },
                    move |_, (bounds, rect): (Bounds<Pixels>, GraphRect), window, _| {
                        if desktop && points.len() > 1 {
                            paint_frametimes(window, bounds, &rect, &points, u, line, warn);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }
}

/// The desktop presenter's graph: frame times (ms, oldest first) as a line.
fn paint_frametimes(window: &mut Window, b: Bounds<Pixels>, rect: &GraphRect, points: &[f32], u: f32, line: Hsla, warn: Hsla) {
    let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    if w < 2.0 || h < 2.0 {
        return;
    }
    let mut sorted: Vec<f32> = points.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f32::total_cmp);
    let p99 = sorted.get(((sorted.len() as f32 - 1.0) * 0.99) as usize).copied().unwrap_or(16.7);
    let lo = rect.min_ms.max(0.0);
    let hi = if rect.max_ms > lo { rect.max_ms } else { (p99 * 1.3).max(lo + 4.0) };
    let pad = 1.5 * u;
    let y_of = |v: f32| y0 + h - pad - ((v.clamp(lo, hi) - lo) / (hi - lo)) * (h - 2.0 * pad);
    let n = points.len();
    let mut path = PathBuilder::stroke(px(1.25 * u));
    for (i, v) in points.iter().enumerate() {
        let p = point(px(x0 + w * i as f32 / (n - 1) as f32), px(y_of(*v)));
        if i == 0 { path.move_to(p) } else { path.line_to(p) }
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, line);
    }
    // Spikes over the warn threshold get a tick.
    if rect.warn_above_ms > 0.0 {
        for (i, v) in points.iter().enumerate().filter(|(_, v)| **v > rect.warn_above_ms) {
            let x = x0 + w * i as f32 / (n - 1) as f32;
            window.paint_quad(gpui::fill(Bounds::new(point(px(x - 0.75 * u), px(y_of(*v))), size(px(1.5 * u), px(y0 + h - y_of(*v)))), warn));
        }
    }
}

impl Render for HudView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        *self.measure.borrow_mut() = Measure::default();
        self.reserve();
        let u = self.unit;
        let font = gpui::Font { weight: SEMIBOLD, ..gpui::font(MONO_FONT) };
        let ts = window.text_system();
        let advance = ts.ch_advance(ts.resolve_font(&font), px(BASE_TEXT)).map(|a| f32::from(a) / BASE_TEXT).unwrap_or(0.6);
        let m = &self.model;
        let ctx = Ctx { th: &th, u, text_alpha: m.text_opacity.clamp(0.5, 1.0), level_colors: m.level_colors, advance, reserved: &self.reserved };
        let panel = hud_card::panel(m.background);
        let (cw, ch) = model_size(m);
        // Rows lean towards the anchor's side (panel or not): a right-anchored card's
        // contents are flush right, a centred card's centred; the card keeps its fixed size.
        let justify = |d: Div| match m.anchor {
            Anchor::TopRight | Anchor::Right | Anchor::BottomRight => d.justify_end(),
            Anchor::Top | Anchor::Bottom => d.justify_center(),
            _ => d,
        };
        let row = |cells: &[HudCell], slot: f32| {
            justify(div().flex().flex_none().w_full().h(ctx.px(slot)).items_center().gap(ctx.px(hud_card::CELL_GAP)).overflow_hidden())
                .children(cells.iter().map(|c| self.cell(&ctx, c)))
        };
        let body: Div = match m.orientation {
            Orientation::Row => {
                let mut all: Vec<HudCell> = Vec::new();
                for (i, r) in m.rows.iter().enumerate() {
                    if i > 0 {
                        all.push(HudCell::Separator);
                    }
                    all.extend(r.cells.iter().cloned());
                }
                let slot = hud_card::row_slot(&all);
                div().flex().flex_col().size_full().child(row(&all, slot))
            }
            Orientation::Block => {
                div().flex().flex_col().size_full().gap(ctx.px(hud_card::ROW_GAP)).children(m.rows.iter().map(|r| row(&r.cells, hud_card::row_slot(&r.cells))))
            }
        };
        let measure = self.measure.clone();
        let desktop = self.mode == Mode::Desktop;
        let (pad, border) = if panel { (hud_card::PAD, hud_card::BORDER) } else { (hud_card::BARE_PAD, 0.0) };
        let card = div()
            .relative()
            .flex_none()
            .w(ctx.px(cw))
            .h(ctx.px(ch))
            .overflow_hidden()
            .font_family(UI_FONT)
            .text_color(th.tx1)
            .px(ctx.px(pad[0]))
            .py(ctx.px(pad[1]))
            .when(panel, |d| {
                d.rounded(ctx.px(8.))
                    .bg(th.ink0.opacity(m.background.clamp(0.0, 1.0)))
                    .border(ctx.px(border))
                    .border_color(th.tx1.opacity(0.07 * m.background.clamp(0.0, 1.0) / 0.45))
            })
            .child(body)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        measure.borrow_mut().card = Some(bounds);
                        if desktop {
                            follow_card(window, cx, &measure);
                        }
                        bounds
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );
        div().size_full().flex().flex_col().items_start().child(card)
    }
}

/// The window size that holds the card: its far edges in whole logical px, plus one for
/// the border's anti-aliasing.
pub fn card_size(m: &Measure) -> Option<gpui::Size<Pixels>> {
    let (w, h) = card_extent(m)?;
    Some(size(px(w.ceil().max(1.0) + 1.0), px(h.ceil().max(1.0) + 1.0)))
}

/// The card's outer size (logical px). It sits at the window's origin; the measuring canvas
/// sees its padding box, inset by the border on every side.
pub fn card_extent(m: &Measure) -> Option<(f32, f32)> {
    let b = m.card?;
    Some((f32::from(b.right() + b.origin.x), f32::from(b.bottom() + b.origin.y)))
}

/// The view's measure after a desktop frame: resize the layer surface to the card (deferred,
/// outside the frame).
pub fn follow_card(window: &mut Window, cx: &mut gpui::App, measure: &Rc<RefCell<Measure>>) {
    let Some(want) = card_size(&measure.borrow()) else { return };
    let have = window.viewport_size();
    if (f32::from(have.width) - f32::from(want.width)).abs() > 0.5 || (f32::from(have.height) - f32::from(want.height)).abs() > 0.5 {
        window.defer(cx, move |w, _| w.resize(want));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{VARIANTS, sample_model, vary};
    use quiver_api::perf::Presenter;

    #[test]
    fn the_card_size_comes_from_the_model_not_its_rows() {
        for layout in ["fps", "compact", "standard", "full"] {
            let base = sample_model(layout, 0, Presenter::Game, Some([2560, 1600]));
            let want = model_size(&base);
            assert_eq!(want, (base.size[0], base.size[1]), "{layout}");
            for v in VARIANTS {
                assert_eq!(model_size(&vary(base.clone(), v)), want, "{layout}/{v}");
            }
        }
        // A model from an older quiverd (no size): the default width and its rows' height.
        let mut m = sample_model("standard", 0, Presenter::Game, None);
        m.size = [0.0, 0.0];
        let (w, h) = model_size(&m);
        assert_eq!(w, hud_card::DEFAULT_WIDTH_BLOCK);
        let slots: Vec<f32> = m.rows.iter().map(|r| hud_card::row_slot(&r.cells)).collect();
        assert_eq!(h, hud_card::height(Orientation::Block, &slots, true));
        // Silly sizes are clamped.
        m.size = [1e6, f32::NAN];
        assert_eq!(model_size(&m), (hud_card::MAX_WIDTH, h));
    }

    #[test]
    fn numbers_keep_their_width_and_long_text_gives_way() {
        let num = |t: &str, u: &str| PerfValue { value: t.parse().ok(), text: t.into(), unit: u.into(), ..Default::default() };
        let text = |t: &str| PerfValue { text: t.into(), ..Default::default() };
        assert!(!shrinks(&num("158", "")));
        assert!(!shrinks(&text("on")) && !shrinks(&text("DLSS K")));
        assert!(shrinks(&text("composited (blocked: overlays)")));
        assert!(!shrinks(&PerfValue { stale: true, ..text("—") }));
        assert_eq!((usual_digits(""), usual_digits("ms"), usual_digits("°"), usual_digits("GB")), (3, 4, 2, 4));
        // The reservation is the widest seen for this layout, never less than usual.
        let mut v = HudView::new(sample_model("compact", 0, Presenter::Game, None), 1.0, Mode::Image);
        v.reserve();
        assert_eq!(v.reserved["frametime/"], (4, 2), "6.3 ms reserves 4 digits");
        v.model = vary(v.model.clone(), "long");
        v.reserve();
        assert_eq!(v.reserved["frametime/"], (5, 2));
        v.model = vary(sample_model("compact", 0, Presenter::Game, None), "short");
        v.reserve();
        assert_eq!(v.reserved["frametime/"], (5, 2), "it doesn't shrink back");
        v.model = sample_model("standard", 0, Presenter::Game, None);
        v.reserve();
        assert_eq!(v.reserved.get("frametime/"), None, "another layout starts over");
    }
}
