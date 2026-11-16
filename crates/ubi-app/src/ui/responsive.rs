//! Breakpoints. Hyprland tiles windows into small or very narrow columns, so every screen
//! adapts to the width it gets. [`Bp`] is computed from a width once per frame: the
//! window's (top bar, pages) or a pane's (the game page beside its side list).
//!
//! | Bp      | width       | what changes                                                   |
//! |---------|-------------|----------------------------------------------------------------|
//! | Wide    | ≥ 1360      | the full design                                                |
//! | Large   | 1140–1360   | top bar: quiverd as a dot, now playing as icon + clock         |
//! | Medium  | 900–1140    | top bar: icon-only sections; game page: side list as a rail    |
//! | Compact | 600–900     | top bar: search as a button, logo mark only; page headers stack|
//! | Narrow  | < 600       | two-row top bar, 16 px gutters, game list in a drawer, one column |

use gpui::{AnyView, App, AppContext, Div, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px};

use crate::app::Quiver;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bp {
    Narrow,
    Compact,
    Medium,
    Large,
    Wide,
}

impl Bp {
    pub const COMPACT: f32 = 600.;
    pub const MEDIUM: f32 = 900.;
    pub const LARGE: f32 = 1140.;
    pub const WIDE: f32 = 1360.;

    pub fn of(width: f32) -> Bp {
        if width >= Self::WIDE {
            Bp::Wide
        } else if width >= Self::LARGE {
            Bp::Large
        } else if width >= Self::MEDIUM {
            Bp::Medium
        } else if width >= Self::COMPACT {
            Bp::Compact
        } else {
            Bp::Narrow
        }
    }

    /// Side padding of a page (library, Downloads, Activity, Settings).
    pub fn gutter(self) -> f32 {
        match self {
            Bp::Narrow => 16.,
            Bp::Compact => 24.,
            _ => 40.,
        }
    }

    /// Side padding inside a pane (the game page, dialogs).
    pub fn pad(self) -> f32 {
        match self {
            Bp::Narrow => 16.,
            Bp::Compact => 20.,
            _ => 28.,
        }
    }

    /// Two columns side by side (content + a fixed side column) fit.
    pub fn two_columns(self) -> bool {
        self >= Bp::Medium
    }

    pub fn narrow(self) -> bool {
        self == Bp::Narrow
    }

    /// Below `Medium`: rows of controls stack under their title.
    pub fn stacked(self) -> bool {
        self <= Bp::Compact
    }
}

/// How the game page shows its list of games.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideMode {
    /// The full list (288 px).
    Full,
    /// Box art only (56 px), names as tooltips.
    Rail,
    /// Hidden; a "Games" button opens it over the page.
    Drawer,
}

pub const RAIL_WIDTH: f32 = 56.;

impl Quiver {
    /// The window's breakpoint, as of the last render.
    pub fn bp(&self) -> Bp {
        Bp::of(self.viewport.0)
    }

    /// Side padding of the current page.
    pub fn gutter(&self) -> f32 {
        self.bp().gutter()
    }

    pub fn side_mode(&self) -> SideMode {
        match self.bp() {
            Bp::Wide | Bp::Large => SideMode::Full,
            Bp::Medium | Bp::Compact => SideMode::Rail,
            Bp::Narrow => SideMode::Drawer,
        }
    }

    /// Width of the game page's pane (the window minus the side list).
    pub fn pane_width(&self) -> f32 {
        let side = match self.side_mode() {
            SideMode::Full => crate::ui::game::SIDE_WIDTH + 1.,
            SideMode::Rail => RAIL_WIDTH + 1.,
            SideMode::Drawer => 0.,
        };
        (self.viewport.0 - side).max(240.)
    }

    /// The breakpoint of the game page's pane.
    pub fn pane_bp(&self) -> Bp {
        Bp::of(self.pane_width())
    }

    /// A dialog's width: `want`, or the window less a margin when that's narrower.
    pub fn dialog_width(&self, want: f32) -> f32 {
        want.min(self.viewport.0 - 2. * self.bp().pad().min(16.)).max(200.)
    }
}

/// A main column beside a fixed-width side column (`side_w`) from `Medium` up; below, one
/// column with the side column under the main one.
pub fn columns(bp: Bp, gap: f32, side_w: f32, main: Div, side: Div) -> Div {
    if bp.two_columns() {
        div().flex().items_start().gap(px(gap)).child(main.flex_1().min_w_0()).child(side.w(px(side_w)).flex_none())
    } else {
        div().flex().flex_col().gap(px(gap)).child(main.w_full()).child(side.w_full())
    }
}

/// A plain text tooltip (icon-only buttons in narrow layouts).
pub struct Tip {
    text: SharedString,
    th: Theme,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(6.))
            .bg(self.th.ink4)
            .border_1()
            .border_color(self.th.line2)
            .text_size(px(12.))
            .text_color(self.th.tx1)
            .font_family(crate::assets::UI_FONT)
            .child(self.text.clone())
    }
}

/// A tooltip builder for `.tooltip(...)`.
pub fn tip(th: &Theme, text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    let th = th.clone();
    move |_, cx| cx.new(|_| Tip { text: text.clone(), th: th.clone() }).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints() {
        assert_eq!(Bp::of(1440.), Bp::Wide);
        assert_eq!(Bp::of(1200.), Bp::Large);
        assert_eq!(Bp::of(1024.), Bp::Medium);
        assert_eq!(Bp::of(800.), Bp::Compact);
        assert_eq!(Bp::of(525.), Bp::Narrow);
        assert_eq!(Bp::of(360.), Bp::Narrow);
        assert!(Bp::Narrow < Bp::Wide);
        assert!(Bp::Medium.two_columns() && !Bp::Compact.two_columns());
    }
}
