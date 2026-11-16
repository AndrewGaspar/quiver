//! Components shared by the screens. They take the resolved [`Theme`] and never mix colors
//! themselves (PROPOSAL.md, "Design system for GPUI").

pub mod game;
pub mod hero;
pub mod library;
pub mod overlay;
pub mod pages;
pub mod topbar;

use gpui::{
    AnyElement, BoxShadow, Div, ElementId, FontWeight, Hsla, IntoElement, ObjectFit, ParentElement, Pixels, SharedString,
    Stateful, Styled, StyledImage, Svg, div, img, point, prelude::*, px, svg,
};

use crate::art::{ArtImage, ArtState};
use crate::assets::{DISPLAY_FONT, MONO_FONT};
use crate::theme::{Theme, on_art};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        /// The icon catalog (Lucide). Not every icon is in use yet.
        #[allow(dead_code)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Icon { $($variant),* }
        #[allow(dead_code)]
        impl Icon {
            pub const ALL: &'static [Icon] = &[$(Icon::$variant),*];
            pub fn path(self) -> &'static str {
                match self { $(Icon::$variant => concat!("icons/", $file, ".svg")),* }
            }
        }
    };
}

icons! {
    Activity => "activity",
    AppWindow => "app-window",
    ArchiveRestore => "archive-restore",
    ArrowDownToLine => "arrow-down-to-line",
    ArrowDownUp => "arrow-down-up",
    ArrowRight => "arrow-right",
    Award => "award",
    Bell => "bell",
    BookOpen => "book-open",
    Bot => "bot",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronUp => "chevron-up",
    CircleCheck => "circle-check",
    CircleDot => "circle-dot",
    CircleX => "circle-x",
    Clock => "clock",
    Cloud => "cloud",
    CloudDownload => "cloud-download",
    CloudUpload => "cloud-upload",
    Cog => "cog",
    Coins => "coins",
    Command => "command",
    Copy => "copy",
    CornerDownRight => "corner-down-right",
    Cpu => "cpu",
    Download => "download",
    Ellipsis => "ellipsis",
    ExternalLink => "external-link",
    FileText => "file-text",
    FolderInput => "folder-input",
    FolderOpen => "folder-open",
    FolderPlus => "folder-plus",
    Gamepad => "gamepad-2",
    GitCompare => "git-compare",
    HardDrive => "hard-drive",
    Globe => "globe",
    GripVertical => "grip-vertical",
    History => "history",
    Image => "image",
    ImageOff => "image-off",
    Info => "info",
    Keyboard => "keyboard",
    Layers => "layers",
    LayoutGrid => "layout-grid",
    LibraryBig => "library-big",
    LifeBuoy => "life-buoy",
    List => "list",
    ListFilter => "list-filter",
    Lock => "lock",
    MessagesSquare => "messages-square",
    Minus => "minus",
    MousePointer => "mouse-pointer-2",
    Package => "package",
    Pause => "pause",
    Pencil => "pencil",
    Pin => "pin",
    PinOff => "pin-off",
    Play => "play-fill",
    PlugZap => "plug-zap",
    Plus => "plus",
    RefreshCw => "refresh-cw",
    RotateCcw => "rotate-ccw",
    ScrollText => "scroll-text",
    Search => "search",
    Settings => "settings",
    ShieldCheck => "shield-check",
    SlidersHorizontal => "sliders-horizontal",
    Square => "square",
    SquareTerminal => "square-terminal",
    Star => "star",
    Sparkles => "sparkles",
    TriangleAlert => "triangle-alert",
    Trash => "trash-2",
    Trophy => "trophy",
    Undo => "undo-2",
    User => "user",
    Users => "users",
    X => "x",
}

pub fn icon(i: Icon, size: f32, color: Hsla) -> Svg {
    svg().path(i.path()).size(px(size)).flex_none().text_color(color)
}

// ---------------------------------------------------------------------------------------------
// Text

/// Archivo SemiExpanded, for page titles and the hero game title only.
pub fn display(size: f32, weight: FontWeight, color: Hsla) -> Div {
    div().font_family(DISPLAY_FONT).font_weight(weight).text_size(px(size)).line_height(px(size * 1.06)).text_color(color)
}

pub fn mono(size: f32, color: Hsla) -> Div {
    div().font_family(MONO_FONT).text_size(px(size)).text_color(color)
}

/// 10.5 / 600 uppercase label ("NEEDS ATTENTION", table headers).
pub fn caption(label: &str, color: Hsla) -> Div {
    div().text_size(px(10.5)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(label.to_uppercase())
}

pub const MEDIUM: FontWeight = FontWeight::MEDIUM;
pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;
pub const BOLD: FontWeight = FontWeight::BOLD;

// ---------------------------------------------------------------------------------------------
// Focus

/// The `focus` token as a ring around an element with corner radius `radius`: a 2 px gap
/// in `ink-1`, a 3 px ring, and a soft halo. Drawn as bordered overlays (GPUI doesn't paint
/// spread-only box shadows), so the element must not clip its children.
pub fn focus_overlay(th: &Theme, radius: f32) -> [AnyElement; 2] {
    let ring = |inset: f32, width: f32, color: Hsla| {
        div()
            .absolute()
            .top(px(-inset))
            .left(px(-inset))
            .right(px(-inset))
            .bottom(px(-inset))
            .rounded(px(radius + inset))
            .border(px(width))
            .border_color(color)
    };
    [ring(2., 2., th.ink1).into_any_element(), ring(5., 3., th.focus).into_any_element()]
}

/// The soft halo around a focused element (drawn behind it).
pub fn focus_halo(th: &Theme) -> BoxShadow {
    BoxShadow { color: th.focus_halo, offset: point(px(0.), px(0.)), blur_radius: px(26.), spread_radius: px(0.) }
}

pub trait FocusExt: ParentElement + Styled + Sized {
    /// Draw the focus ring when `on`. `radius` is the element's corner radius.
    fn focus_ring(self, on: bool, th: &Theme, radius: f32) -> Self {
        if on { self.relative().children(focus_overlay(th, radius)) } else { self }
    }

    /// The ring plus the soft halo. Only for elements with an opaque background: GPUI
    /// paints a shadow's full rectangle, so a transparent element would be tinted.
    fn focus_glow(self, on: bool, th: &Theme, radius: f32) -> Self {
        if on { self.shadow(vec![focus_halo(th)]).focus_ring(true, th, radius) } else { self }
    }
}

impl<T: ParentElement + Styled + Sized> FocusExt for T {}

pub fn soft_shadow(color: Hsla, y: f32, blur: f32) -> BoxShadow {
    BoxShadow { color, offset: point(px(0.), px(y)), blur_radius: px(blur), spread_radius: px(0.) }
}

// ---------------------------------------------------------------------------------------------
// Controls

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Ghost,
    /// Translucent, for use over art.
    OnArt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Small,
    Medium,
    Large,
}

/// A button: label with an optional leading icon.
pub fn button(
    id: impl Into<ElementId>,
    th: &Theme,
    kind: Kind,
    size: Size,
    icon_: Option<Icon>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let (h, px_, fs, r, is) = match size {
        Size::Small => (26., 9., 12., 6., 14.),
        Size::Medium => (32., 13., 13., 8., 16.),
        Size::Large => (44., 20., 15., 10., 18.),
    };
    let (bg, fg, border, hover) = match kind {
        Kind::Primary => (th.acc, th.acc_ink, gpui::transparent_black(), th.acc_hi),
        Kind::Secondary => (th.ink4, th.tx1, th.line2, th.ink5),
        Kind::Ghost => (gpui::transparent_black(), th.tx2, gpui::transparent_black(), th.ink3),
        Kind::OnArt => (crate::theme::rgba(0x0e1016, 0.5), on_art::tx1(), on_art::line(), crate::theme::rgba(0x0e1016, 0.7)),
    };
    let label: SharedString = label.into();
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(7.))
        .h(px(h))
        .px(px(px_))
        .rounded(px(r))
        .bg(bg)
        .border_1()
        .border_color(border)
        .text_size(px(fs))
        .font_weight(MEDIUM)
        .text_color(fg)
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .when_some(icon_, |d, i| d.child(icon(i, is, fg)))
        .when(!label.is_empty(), |d| d.child(label))
}

/// A square icon-only button.
pub fn icon_button(id: impl Into<ElementId>, th: &Theme, i: Icon, bordered: bool) -> Stateful<Div> {
    let hover_bg = th.ink3;
    let hover_fg = th.tx1;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(32.))
        .rounded(px(8.))
        .text_color(th.tx2)
        .when(bordered, |d| d.border_1().border_color(th.line2))
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .child(svg().path(i.path()).size(px(16.)).text_color(th.tx2))
}

/// Filter chip / setting summary.
pub fn chip(th: &Theme, i: Option<Icon>, label: impl Into<SharedString>, on: bool) -> Div {
    let (bg, border, fg) = if on { (th.acc_soft, th.acc_line, th.acc_hi) } else { (th.ink3, th.line2, th.tx2) };
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(26.))
        .px(px(10.))
        .rounded_full()
        .bg(bg)
        .border_1()
        .border_color(border)
        .text_size(px(12.))
        .text_color(fg)
        .whitespace_nowrap()
        .when_some(i, |d, i| d.child(icon(i, 12., fg)))
        .child(label.into())
}

/// A count inside a chip (`All 23`).
pub fn chip_count(th: &Theme, n: usize, on: bool) -> Div {
    div().text_color(if on { th.acc } else { th.tx3 }).child(n.to_string())
}

/// Chip drawn over art.
pub fn art_chip(i: Option<Icon>, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(26.))
        .px(px(10.))
        .rounded_full()
        .bg(crate::theme::rgba(0x0e1016, 0.62))
        .border_1()
        .border_color(on_art::line())
        .text_size(px(12.))
        .text_color(on_art::tx2())
        .whitespace_nowrap()
        .when_some(i, |d, i| d.child(icon(i, 12., on_art::tx2())))
        .child(label.into())
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum TagKind {
    Plain,
    Accent,
    Warn,
    Agent,
}

/// A small label on art (tile corners).
pub fn tag(th: &Theme, kind: TagKind, i: Option<Icon>, label: impl Into<SharedString>) -> Div {
    let (bg, fg) = match kind {
        TagKind::Plain => (on_art::tag(), on_art::tx1()),
        TagKind::Accent => (th.acc, th.acc_ink),
        TagKind::Warn => (th.warn, crate::theme::rgba(0x241600, 1.0)),
        TagKind::Agent => (crate::theme::rgba(0xc38bff, 0.9), crate::theme::rgba(0x1b0a33, 1.0)),
    };
    let label: SharedString = label.into();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .px(px(7.))
        .py(px(3.))
        .rounded(px(6.))
        .bg(bg)
        .text_size(px(10.5))
        .font_weight(BOLD)
        .text_color(fg)
        .when_some(i, |d, i| d.child(icon(i, 12., fg)))
        .when(!label.is_empty(), |d| d.child(label))
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum PillKind {
    Ok,
    Warn,
    Bad,
    Accent,
    Muted,
    /// "Not wired yet": dashed, warn-colored.
    Future,
}

pub fn pill(th: &Theme, kind: PillKind, i: Option<Icon>, label: impl Into<SharedString>) -> Div {
    let (fg, bg) = match kind {
        PillKind::Ok => (th.ok, th.ok_soft),
        PillKind::Warn => (th.warn, th.warn_soft),
        PillKind::Bad => (th.bad, th.bad_soft),
        PillKind::Accent => (th.acc_hi, th.acc_soft),
        PillKind::Muted => (th.tx2, th.ink4),
        PillKind::Future => (th.warn, gpui::transparent_black()),
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .px(px(8.))
        .py(px(2.))
        .rounded_full()
        .bg(bg)
        .text_size(px(11.))
        .font_weight(if matches!(kind, PillKind::Future) { MEDIUM } else { SEMIBOLD })
        .text_color(fg)
        .whitespace_nowrap()
        .when(matches!(kind, PillKind::Future), |d| d.border_1().border_dashed().border_color(th.warn.opacity(0.5)))
        .when_some(i, |d, i| d.child(icon(i, 12., fg)))
        .child(label.into())
}

/// Small count badge (`10`, `64%`).
pub fn badge(th: &Theme, label: impl Into<SharedString>, accent: bool) -> Div {
    let (bg, fg) = if accent { (th.acc_soft, th.acc_hi) } else { (th.ink4, th.tx2) };
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded_full()
        .bg(bg)
        .text_size(px(10.5))
        .font_weight(SEMIBOLD)
        .text_color(fg)
        .child(label.into())
}

/// Keyboard key cap.
pub fn kbd(th: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .h(px(18.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_b_2()
        .border_color(th.line2)
        .bg(th.ink3)
        .font_family(MONO_FONT)
        .text_size(px(10.5))
        .text_color(th.tx3)
        .child(label.into())
}

/// Gamepad button glyph. Face buttons are round; bumpers, triggers and Start are rounded
/// squares. `ghost` is the outlined variant used next to the things it switches.
pub fn glyph(th: &Theme, label: &'static str, ghost: bool) -> Div {
    let square = label.len() > 1 || label == "≡";
    let d = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(20.))
        .min_w(px(if square { 26. } else { 20. }))
        .px(px(5.))
        .rounded(px(if square { 5. } else { 10. }))
        .text_size(px(10.5))
        .font_weight(FontWeight::EXTRA_BOLD)
        .child(label);
    if ghost {
        d.text_color(th.tx2).border(px(1.5)).border_color(th.tx3)
    } else {
        d.bg(th.tx1).text_color(th.ink1)
    }
}

pub fn progress(th: &Theme, fraction: f32, width: Option<f32>, height: f32, color: Option<Hsla>) -> Div {
    let track = div().h(px(height)).rounded(px(height)).bg(th.track).overflow_hidden();
    let track = match width {
        Some(w) => track.w(px(w)),
        None => track.w_full(),
    };
    track.child(
        div()
            .h_full()
            .rounded(px(height))
            .bg(color.unwrap_or(th.acc))
            .w(gpui::relative(fraction.clamp(0.0, 1.0))),
    )
}

pub fn avatar(label: &str, from: Hsla, to: Hsla, ring: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(30.))
        .rounded_full()
        .bg(gpui::linear_gradient(135., gpui::linear_color_stop(from, 0.), gpui::linear_color_stop(to, 1.)))
        .border_2()
        .border_color(ring)
        .text_size(px(12.))
        .font_weight(BOLD)
        .text_color(crate::theme::rgba(0xe8ebff, 1.0))
        .child(label.to_owned())
}

pub fn card(th: &Theme) -> Div {
    div().flex().flex_col().bg(th.ink2).border_1().border_color(th.line1).rounded(px(12.))
}

pub fn card_header(th: &Theme, title: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(16.))
        .py(px(12.))
        .border_b_1()
        .border_color(th.line1)
        .child(div().text_size(px(13.5)).font_weight(SEMIBOLD).child(title.into()))
}

/// `$ quiver …` with the exact command.
pub fn command_line(th: &Theme, cmd: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(30.))
        .pl(px(10.))
        .pr(px(6.))
        .rounded(px(8.))
        .bg(th.ink0)
        .border_1()
        .border_color(th.line1)
        .font_family(MONO_FONT)
        .text_size(px(11.5))
        .text_color(th.tx2)
        .child(div().text_color(th.who_cli).child("$"))
        .child(div().flex_1().min_w_0().truncate().child(cmd.into()))
        .child(icon(Icon::Copy, 12., th.tx3))
}

// ---------------------------------------------------------------------------------------------
// Art

/// An image drawn to cover a `w`×`h` box, anchored at `pos` (0..1 on each axis, like CSS
/// `object-position`). The parent must clip (`overflow_hidden`).
pub fn cover(image: &ArtImage, w: f32, h: f32, pos: (f32, f32)) -> AnyElement {
    let (iw, ih) = (image.width.max(1) as f32, image.height.max(1) as f32);
    let s = (w / iw).max(h / ih);
    let (dw, dh) = (iw * s, ih * s);
    img(image.path.clone())
        .absolute()
        .left(px((w - dw) * pos.0))
        .top(px((h - dh) * pos.1))
        .w(px(dw))
        .h(px(dh))
        .object_fit(ObjectFit::Fill)
        .into_any_element()
}

/// Shown while art loads, and in its place if it fails.
pub fn art_placeholder(th: &Theme, state: &ArtState, label: &str, radius: Pixels) -> Div {
    let failed = matches!(state, ArtState::Failed);
    div()
        .size_full()
        .rounded(radius)
        .bg(gpui::linear_gradient(
            160.,
            gpui::linear_color_stop(th.ink4, 0.),
            gpui::linear_color_stop(th.ink2, 1.),
        ))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .p(px(10.))
        .when(failed, |d| d.child(icon(Icon::ImageOff, 18., th.tx4)))
        .when(!label.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(12.))
                    .font_weight(SEMIBOLD)
                    .text_color(th.tx3)
                    .text_center()
                    .child(label.to_owned()),
            )
        })
}

/// A rounded art image filling its parent, with the placeholder until it's ready.
pub fn art_fill(th: &Theme, state: &ArtState, label: &str, radius: f32, dim: bool) -> AnyElement {
    match state {
        ArtState::Ready(image) => {
            let (th1, th2) = (th.clone(), th.clone());
            let label = label.to_owned();
            img(image.path.clone())
                .size_full()
                .rounded(px(radius))
                .object_fit(ObjectFit::Cover)
                .grayscale(dim)
                .with_loading(move || art_placeholder(&th1, &ArtState::Loading, "", px(radius)).into_any_element())
                .with_fallback(move || art_placeholder(&th2, &ArtState::Failed, &label, px(radius)).into_any_element())
                .into_any_element()
        }
        _ => art_placeholder(th, state, label, px(radius)).into_any_element(),
    }
}
