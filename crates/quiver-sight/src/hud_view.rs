//! The HUD view: a [`HudModel`]'s rows and cells drawn with `quiver-ui`'s theme tokens and
//! fonts (Geist for labels, Geist Mono for numbers, SIGHT-HUD §1.2). The same view renders
//! off-screen into `hud.image` (game presenter) and into a layer-shell surface (desktop
//! presenter); only the graph differs: a placeholder rectangle the layer fills, recorded in
//! [`Measure`] for the image descriptor, or a line drawn here from the model's points.
//!
//! Sizes are logical px times `unit`. Off-screen, `unit` is 1 and the window's scale factor
//! carries the swapchain-derived size; on the desktop, the output's scale is the window's and
//! `unit` is the model's `scale`.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Context, Div, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Render, Styled, Window, canvas,
    div, point, prelude::*, px, size,
};
use quiver_api::perf::{HudCell, HudModel, Level, Orientation, PerfValue};
use quiver_ui::assets::UI_FONT;
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
}

impl HudView {
    pub fn new(model: HudModel, unit: f32, mode: Mode) -> Self {
        Self { model, unit, mode, measure: Rc::default() }
    }
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

    fn value(&self, v: &PerfValue) -> Div {
        let color = if v.stale { self.th.tx3 } else { level_color(self.th, v.level, self.level_colors) };
        let d = div().flex().flex_none().items_baseline().gap(self.px(1.5)).whitespace_nowrap().child(
            mono(BASE_TEXT * self.u, self.fade(color)).font_weight(SEMIBOLD).child(v.text.clone()),
        );
        if v.unit.is_empty() {
            d
        } else {
            d.child(mono(BASE_TEXT * 0.78 * self.u, self.fade(self.th.tx2)).font_weight(MEDIUM).child(v.unit.clone()))
        }
    }

    fn group(&self) -> Div {
        div().flex().flex_none().items_center().gap(self.px(4.))
    }

    fn with_label(&self, label: &Option<String>) -> Div {
        let g = self.group();
        match label {
            Some(l) => g.child(self.label(l)),
            None => g,
        }
    }
}

impl HudView {
    fn cell(&self, cx: &Ctx, c: &HudCell) -> AnyElement {
        match c {
            HudCell::Value { label, value, .. } => cx.with_label(label).child(cx.value(value)).into_any_element(),
            HudCell::Badge { label, value, .. } => {
                let (fg, bg) = if value.stale { (cx.th.tx3, cx.th.ink4) } else { soft_color(cx.th, value.level, cx.level_colors) };
                let text = match label {
                    Some(l) => format!("{l} {}", value.display()),
                    None => value.display(),
                };
                div()
                    .flex_none()
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
                cx.with_label(label).child(bar).child(cx.value(value)).into_any_element()
            }
            HudCell::Sparkline { label, value, points, .. } => {
                let h = 11.0;
                let spark = div().flex().flex_none().items_end().gap(cx.px(1.)).h(cx.px(h)).children(points.iter().map(|p| {
                    div().w(cx.px(2.)).h(cx.px((p.clamp(0.0, 1.0) * h).max(1.0))).rounded(cx.px(1.)).bg(cx.fade(cx.th.acc.opacity(0.85)))
                }));
                cx.with_label(label).child(spark).child(cx.value(value)).into_any_element()
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
        let height = rows as f32 * BASE_TEXT * 1.25;
        div()
            .relative()
            .w_full()
            .min_w(cx.px(if self.model.orientation == Orientation::Row { 110. } else { 200. }))
            .h(cx.px(height))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        *self.measure.borrow_mut() = Measure::default();
        let m = &self.model;
        let ctx = Ctx { th: &th, u: self.unit, text_alpha: m.text_opacity.clamp(0.5, 1.0), level_colors: m.level_colors };
        let gap = ctx.px(8.);
        let row = |cells: &[HudCell]| div().flex().flex_none().items_center().gap(gap).children(cells.iter().map(|c| self.cell(&ctx, c)));
        let body: Div = match m.orientation {
            Orientation::Row => {
                let mut all: Vec<HudCell> = Vec::new();
                for (i, r) in m.rows.iter().enumerate() {
                    if i > 0 {
                        all.push(HudCell::Separator);
                    }
                    all.extend(r.cells.iter().cloned());
                }
                div().flex().child(row(&all))
            }
            Orientation::Block => div().flex().flex_col().gap(ctx.px(4.)).children(m.rows.iter().map(|r| row(&r.cells))),
        };
        let panel = m.background > 0.01;
        let measure = self.measure.clone();
        let desktop = self.mode == Mode::Desktop;
        let card = div()
            .relative()
            .flex_none()
            .font_family(UI_FONT)
            .text_color(th.tx1)
            .when(panel, |d| {
                d.px(ctx.px(9.))
                    .py(ctx.px(6.))
                    .rounded(ctx.px(8.))
                    .bg(th.ink0.opacity(m.background.clamp(0.0, 1.0)))
                    .border_1()
                    .border_color(th.tx1.opacity(0.07 * m.background.clamp(0.0, 1.0) / 0.45))
            })
            .when(!panel, |d| d.px(ctx.px(2.)))
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
