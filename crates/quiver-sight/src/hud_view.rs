//! The HUD view: a [`HudModel`]'s rows and cells drawn with `quiver-ui`'s theme tokens and
//! fonts (Geist for labels, Geist Mono for numbers, SIGHT-HUD §1.2). The same view renders
//! off-screen into `hud.image` (game presenter) and into a layer-shell surface (desktop
//! presenter); only the graph differs: a placeholder rectangle the layer fills, recorded in
//! [`Measure`] for the image descriptor, or the lines drawn here from the model's points.
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
//!
//! **The look** follows the design canvas's HUD mock-ups (design/SIGHT-HUD.md §1.1): one big
//! number (a cell with `hero` emphasis) and a second one beside it (`secondary`, in the
//! graph's base-series colour), the rest smaller; small uppercase labels after values ("118
//! FPS", "8.5 MS"); values tinted by their producer (the Sight layer neutral, a mod teal,
//! the system grey, Quiver Tweaks the accent; a threshold level wins); the frame-time graph
//! with presented frames in blue and rendered ones in amber over a faint reference line.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Context, Div, FontWeight, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Render, Rgba, Styled, Window, canvas, div, point, prelude::*, px,
    relative, size,
};
use quiver_api::perf::{Anchor, CellStyle, Emphasis, HudCell, HudModel, HudRow, Level, Orientation, PerfValue, SourceKind, SplitPart, hud_card};
use quiver_ui::assets::{MONO_FONT, UI_FONT};
use quiver_ui::components::{MEDIUM, SEMIBOLD, mono};
use quiver_ui::theme::Theme;

/// Base value text size (logical px at unit 1). The image's scale factor maps it to the
/// model's `text_px` (1.6% of the swapchain height) times its `scale`.
pub const BASE_TEXT: f32 = 12.0;
/// Labels and small units, relative to [`BASE_TEXT`].
const LABEL: f32 = 0.78;

/// A graph placeholder as laid out (window logical px), with what the layer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphRect {
    pub bounds: Bounds<Pixels>,
    pub span_ms: u32,
    pub min_ms: f32,
    pub max_ms: f32,
    pub warn_above_ms: f32,
    pub bad_above_ms: f32,
    /// The model asks for the base (rendered frames) series too.
    pub base: bool,
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
    let (label, value) = c.labelled()?;
    Some((format!("{}/{}", c.metric()?, label.unwrap_or("")), value))
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

/// A unit shown as a small uppercase label after the number ("8.5 MS", "6.1 GB"), not
/// attached to it ("87%", "72°", "96W", "2.0×").
pub fn unit_is_label(unit: &str) -> bool {
    unit.chars().count() >= 2 && unit.chars().all(char::is_alphabetic)
}

/// A value's size (× [`BASE_TEXT`]) and weight for its emphasis.
pub fn emphasis_size(e: Emphasis) -> (f32, FontWeight) {
    match e {
        Emphasis::Hero => (1.85, SEMIBOLD),
        Emphasis::Secondary => (1.3, SEMIBOLD),
        Emphasis::Strong => (1.0, SEMIBOLD),
        Emphasis::Normal | Emphasis::Quiet => (1.0, MEDIUM),
        Emphasis::Tag => (LABEL, SEMIBOLD),
    }
}

/// The card's size (logical px at unit 1): the model's, or for a model without one the
/// default width and the height of its rows.
pub fn model_size(m: &HudModel) -> (f32, f32) {
    let panel = hud_card::panel(m.background);
    let [w, h] = m.size;
    let w = if w > 0.0 { hud_card::clamp_width(w, hud_card::default_width(m.orientation)) } else { hud_card::default_width(m.orientation) };
    let fallback = || {
        let slots: Vec<f32> = m.rows.iter().map(hud_card::row_height).collect();
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

/// `a` moved `t` of the way to `b` (straight RGB).
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    let l = |x: f32, y: f32| x + (y - x) * t;
    Rgba { r: l(a.r, b.r), g: l(a.g, b.g), b: l(a.b, b.b), a: l(a.a, b.a) }.into()
}

/// A producer's mark colour (the dot, the editor's group): the theme's actor tokens (the
/// built-in palette's `who-gui` blue for the layer, `who-cli` teal for mods, `who-sys` grey).
pub fn source_mark(th: &Theme, s: SourceKind) -> Hsla {
    match s {
        SourceKind::Layer => th.who_gui,
        SourceKind::Mod => th.who_cli,
        SourceKind::System => th.who_sys,
        SourceKind::Tweaks => th.acc_hi,
        SourceKind::Quiver => th.tx2,
    }
}

/// A value's colour from its producer, emphasis, level and freshness. A threshold level
/// (warn, bad, good, accent) wins over the producer's tint; stale and dim values are dim.
pub fn value_color(th: &Theme, style: &CellStyle, level: Level, stale: bool, level_colors: bool) -> Hsla {
    if stale {
        return th.tx3;
    }
    if level != Level::Normal && (level_colors || level == Level::Dim) {
        return level_color(th, level, level_colors);
    }
    let quiet = style.emphasis == Emphasis::Quiet;
    match style.source {
        // A mod's values in light teal (the mark mixed toward the text colour).
        SourceKind::Mod => {
            let c = mix(th.who_cli, th.tx1, 0.45);
            if quiet { c.opacity(0.8) } else { c }
        }
        SourceKind::Tweaks => th.acc_hi,
        // The second number beside the hero is the graph's base series.
        _ if style.emphasis == Emphasis::Secondary => th.warn,
        _ if quiet => th.tx2,
        SourceKind::System => mix(th.tx1, th.who_sys, 0.3),
        SourceKind::Layer | SourceKind::Quiver => th.tx1,
    }
}

/// A tag's colour: the label colour for the layer's and quiverd's, else its producer's tint
/// ("REFLEX BOOST" in teal from a mod).
pub fn tag_color(th: &Theme, style: &CellStyle) -> Hsla {
    match style.source {
        SourceKind::Layer | SourceKind::Quiver => th.tx2,
        _ => value_color(th, &CellStyle { emphasis: Emphasis::Normal, ..style.clone() }, Level::Normal, false, true),
    }
}

/// The split bar's part colours, in order (simulation, render, queue, present + scan-out).
pub fn split_colors(th: &Theme) -> [Hsla; 4] {
    [th.who_gui, th.acc_hi, th.warn, th.tx4]
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

    /// A small uppercase label ("GPU", "FPS", "MS").
    fn label_in(&self, text: &str, color: Hsla) -> Div {
        div()
            .flex_none()
            .font_family(UI_FONT)
            .text_size(self.px(BASE_TEXT * LABEL))
            .font_weight(SEMIBOLD)
            .text_color(self.fade(color))
            .whitespace_nowrap()
            .child(text.to_uppercase())
    }

    fn label(&self, text: &str) -> Div {
        self.label_in(text, self.th.tx3)
    }

    /// A producer's dot.
    fn dot(&self, s: SourceKind) -> Div {
        div().flex_none().size(self.px(6.)).rounded_full().bg(self.fade(source_mark(self.th, s)))
    }

    /// A value: a number in its reserved width (right aligned, never shrinks), or text that
    /// shrinks with an ellipsis when the row runs out of room; its unit after it, attached
    /// ("87%") or as a small label ("8.5 MS").
    fn value(&self, key: &str, v: &PerfValue, style: &CellStyle) -> Div {
        let color = value_color(self.th, style, v.level, v.stale, self.level_colors);
        if style.emphasis == Emphasis::Tag {
            let text = if v.unit.is_empty() || !unit_is_label(&v.unit) { v.display() } else { format!("{} {}", v.text, v.unit) };
            let color = if v.level == Level::Normal && !v.stale { tag_color(self.th, style) } else { color };
            return self.label_in(&text, color);
        }
        let (scale, weight) = emphasis_size(style.emphasis);
        let (chars, unit_chars) = self.reserved.get(key).copied().unwrap_or((v.text.chars().count(), v.unit.chars().count()));
        let fixed = !shrinks(v);
        // Numbers line up on their last digit; text starts where it starts.
        let right = v.value.is_some() || v.stale;
        // Short text keeps at most a short text's width (it was long once: that doesn't
        // make it unshrinkable now).
        let chars = if right { chars } else { chars.min(SHORT_TEXT).max(v.text.chars().count()) };
        // A little slack over the advance so rounding at odd scales never truncates.
        let w = |n: usize, size: f32| px((n as f32 * self.advance * BASE_TEXT * size + 0.5) * self.u);
        let small_unit = unit_is_label(&v.unit);
        let attached = !v.unit.is_empty() && !small_unit;
        // An attached unit sits in the number's box, no space ("96W" as in "87%"), so the
        // number stays right aligned with it.
        let (text, chars) = if attached { (format!("{}{}", v.text, v.unit), chars + unit_chars) } else { (v.text.clone(), chars) };
        let num = mono(BASE_TEXT * scale * self.u, self.fade(color)).font_weight(weight).whitespace_nowrap();
        let num = if fixed {
            num.flex_none().w(w(chars, scale)).when(right, |d| d.text_right()).child(text)
        } else {
            num.flex_shrink(1.).min_w_0().truncate().child(text)
        };
        let d = div().flex().items_baseline().gap(self.px(3.)).whitespace_nowrap().child(num);
        let d = if fixed { d.flex_none() } else { d.flex_shrink(1.).min_w_0().overflow_hidden() };
        if small_unit { d.child(self.unit(&v.unit).min_w(w(unit_chars, LABEL))) } else { d }
    }

    /// A unit as a small label: "MS", "FPS", but "GB", "Hz" and "MHz" as written.
    fn unit(&self, unit: &str) -> Div {
        let text = if unit.chars().all(|c| c.is_lowercase()) { unit.to_uppercase() } else { unit.to_owned() };
        div()
            .flex_none()
            .font_family(UI_FONT)
            .text_size(self.px(BASE_TEXT * LABEL))
            .font_weight(SEMIBOLD)
            .text_color(self.fade(self.th.tx3))
            .whitespace_nowrap()
            .child(text)
    }

    fn group(&self, shrink: bool) -> Div {
        let g = div().flex().items_baseline().gap(self.px(4.));
        if shrink { g.flex_shrink(1.).min_w_0().overflow_hidden() } else { g.flex_none() }
    }

    /// A value with its label: before it ("GPU 87%"), or after it ("118 FPS").
    fn labelled(&self, label: &Option<String>, style: &CellStyle, shrink: bool, value: Div) -> Div {
        let g = self.group(shrink);
        let g = if style.emphasis == Emphasis::Tag && style.source == SourceKind::Mod { g.items_center().child(self.dot(style.source)) } else { g };
        match label {
            Some(l) if style.after => g.child(value).child(self.label(l)),
            Some(l) if style.emphasis == Emphasis::Tag => g.child(self.label_in(l, tag_color(self.th, style))).child(value),
            Some(l) => g.child(self.label(l)).child(value),
            None => g.child(value),
        }
    }
}

/// Text up to this long (states: "on", "direct", "composited", "DLSS K"; resolutions:
/// "2560×1600") keeps its width like a number.
const SHORT_TEXT: usize = 10;

/// Longer text values shrink (with an ellipsis); numbers, short text, and stale values keep
/// their width.
fn shrinks(v: &PerfValue) -> bool {
    v.value.is_none() && !v.stale && v.text.chars().count() > SHORT_TEXT
}

impl HudView {
    fn cell(&self, cx: &Ctx, c: &HudCell) -> AnyElement {
        let key = value_of(c).map(|(k, _)| k).unwrap_or_default();
        match c {
            HudCell::Value { label, value, style, .. } => cx.labelled(label, style, shrinks(value), cx.value(&key, value, style)).into_any_element(),
            HudCell::Badge { label, value, style, .. } => {
                let (fg, bg) = if value.stale { (cx.th.tx3, cx.th.ink4) } else { soft_color(cx.th, value.level, cx.level_colors) };
                let fg = if value.level == Level::Normal && !value.stale { value_color(cx.th, style, value.level, false, cx.level_colors) } else { fg };
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
            HudCell::Bar { label, value, fraction, style, .. } => {
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
                    .child(div().h_full().rounded(cx.px(2.)).bg(cx.fade(fill)).w(relative(fraction.clamp(0.0, 1.0))));
                cx.labelled(label, style, false, div().flex().items_center().gap(cx.px(4.)).child(bar.flex_none()).child(cx.value(&key, value, style))).into_any_element()
            }
            HudCell::Sparkline { label, value, points, style, .. } => {
                let h = 11.0;
                let spark = div().flex().flex_none().items_end().gap(cx.px(1.)).h(cx.px(h)).children(points.iter().map(|p| {
                    div().w(cx.px(2.)).h(cx.px((p.clamp(0.0, 1.0) * h).max(1.0))).rounded(cx.px(1.)).bg(cx.fade(cx.th.acc.opacity(0.85)))
                }));
                cx.labelled(label, style, false, div().flex().items_end().gap(cx.px(4.)).child(spark).child(cx.value(&key, value, style))).into_any_element()
            }
            HudCell::Split { parts, numbers: true, style, .. } => split_numbers(cx, parts, style).into_any_element(),
            HudCell::Split { label, parts, .. } => {
                let bar = split_bar(cx, parts);
                match label {
                    Some(l) => div().flex().flex_1().min_w_0().items_center().gap(cx.px(4.)).child(cx.label(l)).child(bar).into_any_element(),
                    None => bar.into_any_element(),
                }
            }
            HudCell::Graph { span_ms, rows, min_ms, max_ms, warn_above_ms, bad_above_ms, points, base_points, series } => {
                let rect = GraphRect {
                    bounds: Bounds::default(),
                    span_ms: *span_ms,
                    min_ms: *min_ms,
                    max_ms: *max_ms,
                    warn_above_ms: *warn_above_ms,
                    bad_above_ms: *bad_above_ms,
                    base: series.iter().any(|s| s == "frametime.base"),
                };
                self.graph(cx, rect, (*rows).clamp(1, 6), points.clone(), base_points.clone())
            }
            HudCell::Text { text, style, .. } if style.emphasis == Emphasis::Tag => {
                let color = tag_color(cx.th, style);
                let g = cx.group(false).items_center();
                let g = if style.source == SourceKind::Mod { g.child(cx.dot(style.source)) } else { g };
                g.child(cx.label_in(text, color)).into_any_element()
            }
            HudCell::Text { text, level, .. } => div()
                .flex_none()
                .font_family(UI_FONT)
                .text_size(cx.px(BASE_TEXT * 0.9))
                .text_color(cx.fade(level_color(cx.th, *level, cx.level_colors)))
                .whitespace_nowrap()
                .child(text.clone())
                .into_any_element(),
            HudCell::Spacer => div().flex_1().min_w(cx.px(4.)).into_any_element(),
            HudCell::Separator => div().flex_none().w(px(1.0f32.max(cx.u))).h(cx.px(BASE_TEXT * 0.9)).bg(cx.fade(cx.th.line2)).into_any_element(),
        }
    }

    /// The graph: a quiet track the layer draws the live frame times into (image), or the
    /// frame times drawn here (desktop). Its laid-out bounds go into the descriptor.
    fn graph(&self, cx: &Ctx, rect: GraphRect, rows: u32, points: Vec<f32>, base_points: Vec<f32>) -> AnyElement {
        let measure = self.measure.clone();
        let desktop = self.mode == Mode::Desktop;
        let colors = GraphColors { line: cx.fade(cx.th.who_gui), base: cx.fade(cx.th.warn), warn: cx.fade(cx.th.warn), bad: cx.fade(cx.th.bad), reference: cx.fade(cx.th.tx1.opacity(0.10)) };
        let u = cx.u;
        let height = hud_card::slot(Some(rows));
        let d = div().relative();
        // Its own width on one line; the rest of the row in a block.
        let d = if self.model.orientation == Orientation::Row { d.flex_none().w(cx.px(110.)) } else { d.flex_1().min_w_0() };
        d.h(cx.px(height))
            .rounded(cx.px(3.))
            .bg(cx.th.tx1.opacity(0.04 * cx.text_alpha))
            .child(
                canvas(
                    move |bounds, _, _| {
                        measure.borrow_mut().graphs.push(GraphRect { bounds, ..rect.clone() });
                        (bounds, rect)
                    },
                    move |_, (bounds, rect): (Bounds<Pixels>, GraphRect), window, _| {
                        if desktop && points.len() > 1 {
                            paint_frametimes(window, bounds, &rect, &points, &base_points, u, &colors);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }
}

/// The latency split: a thin stacked bar of its parts, in proportion.
fn split_bar(cx: &Ctx, parts: &[SplitPart]) -> Div {
    let total: f32 = parts.iter().map(|p| p.value.max(0.0)).sum();
    let colors = split_colors(cx.th);
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .h(cx.px(5.))
        .rounded(cx.px(2.5))
        .overflow_hidden()
        .gap(px(1.0f32.max(cx.u)))
        .children(parts.iter().enumerate().map(|(i, p)| {
            let share = if total > 0.0 { p.value.max(0.0) / total } else { 0.0 };
            div().h_full().flex_shrink(1.).flex_basis(relative(share)).bg(cx.fade(colors[i % colors.len()]))
        }))
}

/// The split's numbers: "■ SIM 6 · ■ RENDER 9 · …", each name after a mark in its bar colour.
fn split_numbers(cx: &Ctx, parts: &[SplitPart], style: &CellStyle) -> Div {
    let colors = split_colors(cx.th);
    let color = value_color(cx.th, &CellStyle { emphasis: Emphasis::Normal, ..style.clone() }, Level::Normal, false, cx.level_colors);
    let mut d = div().flex().flex_shrink(1.).min_w_0().overflow_hidden().items_center().gap(cx.px(5.));
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            d = d.child(div().flex_none().text_size(cx.px(BASE_TEXT * 0.9)).text_color(cx.fade(cx.th.tx3)).child("·"));
        }
        let mark = div().flex_none().size(cx.px(5.)).rounded(cx.px(1.)).bg(cx.fade(colors[i % colors.len()]));
        let num = mono(BASE_TEXT * cx.u, cx.fade(color)).font_weight(MEDIUM).flex_none().child(format!("{:.0}", p.value));
        d = d.child(div().flex().flex_none().items_center().gap(cx.px(3.)).child(mark).child(cx.label(&p.label)).child(num));
    }
    d
}

/// The desktop graph's colours.
#[derive(Clone, Copy)]
struct GraphColors {
    line: Hsla,
    base: Hsla,
    warn: Hsla,
    bad: Hsla,
    reference: Hsla,
}

/// Frame-rate targets the reference line can mark (ms), as the layer's graph does.
const TARGETS: [f32; 8] = [8.33, 16.67, 33.33, 50.0, 100.0, 200.0, 500.0, 1000.0];

/// The largest target comfortably inside `(lo, hi)`.
pub fn reference_ms(lo: f32, hi: f32) -> Option<f32> {
    TARGETS.iter().rev().copied().find(|&t| t > lo && t < hi * 0.92)
}

/// The desktop presenter's graph: frame times (ms, oldest first) as a line, base frame times
/// under it, a reference line behind.
fn paint_frametimes(window: &mut Window, b: Bounds<Pixels>, rect: &GraphRect, points: &[f32], base: &[f32], u: f32, c: &GraphColors) {
    let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    if w < 2.0 || h < 2.0 {
        return;
    }
    let base = if rect.base { base } else { &[] };
    let mut sorted: Vec<f32> = points.iter().chain(base).copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f32::total_cmp);
    let p99 = sorted.get(((sorted.len() as f32 - 1.0) * 0.99) as usize).copied().unwrap_or(16.7);
    let lo = rect.min_ms.max(0.0);
    let hi = if rect.max_ms > lo { rect.max_ms } else { (p99 * 1.3).max(lo + 4.0) };
    let pad = 1.5 * u;
    let y_of = |v: f32| y0 + h - pad - ((v.clamp(lo, hi) - lo) / (hi - lo)) * (h - 2.0 * pad);
    if let Some(r) = reference_ms(lo, hi) {
        window.paint_quad(gpui::fill(Bounds::new(point(px(x0), px(y_of(r) - 0.5 * u)), size(px(w), px(u.max(1.0)))), c.reference));
    }
    // The base series spans the same time as the presented one: its points are spread over
    // the width by their share of the total time.
    let line = |window: &mut Window, pts: &[f32], color: Hsla| {
        let total: f32 = pts.iter().sum();
        if pts.len() < 2 || total <= 0.0 {
            return;
        }
        let mut path = PathBuilder::stroke(px(1.25 * u));
        let mut t = 0.0;
        for (i, v) in pts.iter().enumerate() {
            t += v;
            let p = point(px(x0 + w * t / total), px(y_of(*v)));
            if i == 0 { path.move_to(p) } else { path.line_to(p) }
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    line(window, base, c.base);
    line(window, points, c.line);
    // Spikes over the warn threshold get a tick.
    if rect.warn_above_ms > 0.0 {
        let total: f32 = points.iter().sum();
        let mut t = 0.0;
        for v in points {
            t += v;
            if *v > rect.warn_above_ms {
                let x = x0 + w * t / total.max(1e-3);
                let color = if rect.bad_above_ms > 0.0 && *v > rect.bad_above_ms { c.bad } else { c.warn };
                window.paint_quad(gpui::fill(Bounds::new(point(px(x - 0.75 * u), px(y_of(*v))), size(px(1.5 * u), px(y0 + h - y_of(*v)))), color));
            }
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
        let row = |cells: &[HudCell], slot: f32, divider: bool| {
            let line = justify(div().flex().flex_none().w_full().h(ctx.px(slot)).items_center().gap(ctx.px(hud_card::CELL_GAP)).overflow_hidden())
                .children(cells.iter().map(|c| self.cell(&ctx, c)));
            if divider {
                // The rule at the top of the slot, the line below it.
                line.border_t(px(1.0f32.max(u))).border_color(ctx.fade(th.tx1.opacity(0.10))).pt(ctx.px(hud_card::DIVIDER - 1.0)).h(ctx.px(slot + hud_card::DIVIDER))
            } else {
                line
            }
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
                div().flex().flex_col().size_full().child(row(&all, slot, false))
            }
            Orientation::Block => div()
                .flex()
                .flex_col()
                .size_full()
                .gap(ctx.px(hud_card::ROW_GAP))
                .children(m.rows.iter().map(|r: &HudRow| row(&r.cells, hud_card::row_slot(&r.cells), r.divider))),
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
                d.rounded(ctx.px(10.))
                    .bg(th.ink0.opacity(m.background.clamp(0.0, 1.0)))
                    .border(ctx.px(border))
                    .border_color(th.tx1.opacity(0.08 * (m.background.clamp(0.0, 1.0) / 0.62).min(1.0)))
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
        for layout in crate::demo::LAYOUTS {
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
        let slots: Vec<f32> = m.rows.iter().map(hud_card::row_height).collect();
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
        assert_eq!(v.reserved.get("frametime/"), Some(&(4, 2)), "another layout starts over");
    }

    #[test]
    fn units_and_emphasis() {
        // "8.5 MS" and "6.1 GB" carry a small label; "87%", "72°", "96W", "2.0×" stay attached.
        for u in ["ms", "GB", "MHz", "fps", "Hz"] {
            assert!(unit_is_label(u), "{u}");
        }
        for u in ["", "%", "°", "W", "×"] {
            assert!(!unit_is_label(u), "{u}");
        }
        // One big number, a second one smaller, the rest at the base size; tags as labels.
        let (hero, _) = emphasis_size(Emphasis::Hero);
        let (sec, _) = emphasis_size(Emphasis::Secondary);
        let (norm, w) = emphasis_size(Emphasis::Normal);
        assert!(hero > sec && sec > norm && w == MEDIUM);
        assert!(BASE_TEXT * hero * 1.2 <= hud_card::HERO_LINE, "the hero fits its taller row");
        assert_eq!(emphasis_size(Emphasis::Tag).0, LABEL);
    }

    #[test]
    fn values_are_tinted_by_their_producer() {
        let th = Theme::builtin();
        let s = |source, emphasis| CellStyle { source, emphasis, ..Default::default() };
        let c = |st: CellStyle, level| value_color(&th, &st, level, false, true);
        // The layer neutral, a mod teal, the system grey, Tweaks the accent.
        assert_eq!(c(s(SourceKind::Layer, Emphasis::Hero), Level::Normal), th.tx1);
        let teal = c(s(SourceKind::Mod, Emphasis::Normal), Level::Normal);
        assert_ne!(teal, th.tx1);
        let rgb = Rgba::from(teal);
        assert!(rgb.g > rgb.r && rgb.b > rgb.r, "teal: {rgb:?}");
        let grey = c(s(SourceKind::System, Emphasis::Normal), Level::Normal);
        assert!(grey != th.tx1 && grey != teal);
        assert_eq!(c(s(SourceKind::Tweaks, Emphasis::Normal), Level::Normal), th.acc_hi);
        // The second number is the base series' colour, unless a mod sends it.
        assert_eq!(c(s(SourceKind::Layer, Emphasis::Secondary), Level::Normal), th.warn);
        assert_eq!(c(s(SourceKind::Mod, Emphasis::Secondary), Level::Normal), teal);
        // Quiet detail is dimmer; a threshold level wins; stale is dim.
        assert_eq!(c(s(SourceKind::System, Emphasis::Quiet), Level::Normal), th.tx2);
        assert_eq!(c(s(SourceKind::Mod, Emphasis::Normal), Level::Bad), th.bad);
        assert_eq!(value_color(&th, &s(SourceKind::Mod, Emphasis::Normal), Level::Normal, true, true), th.tx3);
        // With level colours off a warn is plain again (the producer's tint).
        assert_eq!(value_color(&th, &s(SourceKind::Layer, Emphasis::Normal), Level::Warn, false, false), th.tx1);
        // The marks are the theme's actor tokens.
        assert_eq!(source_mark(&th, SourceKind::Mod), th.who_cli);
        assert_eq!(split_colors(&th)[0], th.who_gui);
    }

    #[test]
    fn the_reference_line_marks_a_frame_rate() {
        assert_eq!(reference_ms(0.0, 20.0), Some(16.67));
        assert_eq!(reference_ms(0.0, 10.0), Some(8.33));
        assert_eq!(reference_ms(0.0, 4.0), None);
    }
}
