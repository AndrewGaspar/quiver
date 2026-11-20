//! Stereo depth for Quiver's GPUI surfaces (design/STEREO-UI.md §4). On a side-by-side stereo
//! output the app draws itself once per eye, and each element sits a little in front of what
//! it is drawn on. Nothing here changes a mono window: depths only matter while the window
//! packs stereo frames (`gpui::Window::set_stereo_packing`).
//!
//! The ladder is `quiver_perf::stereo::ui` (thousandths, shared with Sight's rungs); these are
//! the same numbers as GPUI takes them.
//!
//! **How a screen gets its depth.** Three kinds of thing, back to front:
//! - *the plane*: backgrounds and art ([`DepthExt::depth_plane`], or nothing: the page is at 0);
//! - *surfaces*: cards, panels, lists, a tile grid, each a step above what it's on
//!   ([`DepthExt::depth_surface`]; [`crate::components::card`] already is one);
//! - *controls*: anything that shows a pointer or text cursor, or takes the app's focus ring
//!   ([`crate::components::FocusExt`]), rises a step above its surface by itself, one more while
//!   hovered and two more while focused (the window's [`policy`]). Nothing to add at the call
//!   site: a new button, row or chip pops from whatever it sits on.
//!
//! *Overlays* (menus, popovers, dialogs, the palette, toasts) sit at [`OVERLAY`] whatever is
//! under them ([`DepthExt::depth_overlay`]), and their own controls pop from them. A scrim
//! that closes an overlay on click is clickable but isn't a control: [`DepthExt::depth_flat`].

use gpui::{StereoPolicy, Styled};
use quiver_perf::stereo::ui;

fn d(thousandths: u16) -> f32 {
    thousandths as f32 / 1000.0
}

/// Backgrounds, hero art: the window's plane.
pub const PLANE: f32 = 0.0;
/// The top bar and the controller hint bar.
pub const CHROME: f32 = ui::CHROME as f32 / 1000.0;
/// A surface's lift above what it's on.
pub const SURFACE: f32 = ui::SURFACE as f32 / 1000.0;
/// A control's lift above its surface (applied by the [`policy`], not by call sites).
pub const CONTROL: f32 = ui::CONTROL as f32 / 1000.0;
/// A hovered control's extra lift.
pub const HOVER: f32 = ui::HOVER as f32 / 1000.0;
/// A focused control's extra lift.
pub const FOCUS: f32 = ui::FOCUS as f32 / 1000.0;
/// Menus, popovers, dialogs, the palette, toasts.
pub const OVERLAY: f32 = ui::OVERLAY as f32 / 1000.0;
/// Tooltips.
pub const TOOLTIP: f32 = ui::TOOLTIP as f32 / 1000.0;
/// The ceiling every depth is clamped to.
pub const MAX: f32 = ui::MAX as f32 / 1000.0;

/// The window's rules for turning styles into depths: controls rise [`CONTROL`] above their
/// surface, [`HOVER`] more while hovered and [`FOCUS`] more while focused (not both),
/// clamped to [`MAX`]; tooltips at [`TOOLTIP`].
pub fn policy() -> StereoPolicy {
    StereoPolicy {
        control: d(ui::CONTROL),
        hover: d(ui::HOVER),
        focus: d(ui::FOCUS),
        max: d(ui::MAX),
        tooltip: d(ui::TOOLTIP),
    }
}

/// Depth in thousandths, as `quiver_perf::stereo::disparity_px` takes it.
pub fn thousandths(depth: f32) -> u16 {
    (depth.clamp(0.0, 1.0) * 1000.0).round() as u16
}

/// Each eye's shift in whole device pixels, `[left, right]`, for `depth` on an eye
/// `pane_px` device pixels wide at `percent` strength: HypXRland's formula
/// (`quiver_perf::stereo::disparity_px`), the left eye's content moving right by the larger
/// half.
pub fn eye_shifts(depth: f32, pane_px: f32, percent: u32) -> [i32; 2] {
    use quiver_perf::stereo::{disparity_px, eye_shift};
    let total = disparity_px(thousandths(depth), percent, pane_px.round().max(0.0) as u32);
    [eye_shift(total, 0), eye_shift(total, 1)]
}

/// The packing for a window on a pane `pane_px` device pixels wide at `percent` strength.
pub fn packing(pane_px: f32, percent: u32) -> gpui::StereoPacking {
    gpui::StereoPacking::new(move |depth| {
        let [l, r] = eye_shifts(depth, pane_px, percent);
        [l as f32, r as f32]
    })
}

/// Depth roles for the elements a screen is built from.
pub trait DepthExt: Styled + Sized {
    /// A content surface: a step above what it's drawn on.
    fn depth_surface(self) -> Self {
        self.stereo_lift(SURFACE)
    }

    /// Two steps above what it's drawn on: something raised on a surface that isn't itself a
    /// control (a selected item's highlight, a drag preview).
    fn depth_raised(self) -> Self {
        self.stereo_lift(SURFACE * 2.0)
    }

    /// An overlay (menu, popover, dialog, palette, toast): above the whole page.
    fn depth_overlay(self) -> Self {
        self.stereo_depth(OVERLAY)
    }

    /// The window's plane, whatever it's drawn on (art, backgrounds).
    fn depth_plane(self) -> Self {
        self.stereo_depth(PLANE)
    }

    /// The top bar and other window chrome.
    fn depth_chrome(self) -> Self {
        self.stereo_depth(CHROME)
    }

    /// Clickable but not a control: a scrim, a whole-area click target. Stays on its
    /// parent's depth.
    fn depth_flat(self) -> Self {
        self.stereo_control(false)
    }
}

impl<T: Styled + Sized> DepthExt for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{StereoRequest, StereoState, resolve_stereo_depth};

    fn px(depth: f32) -> i32 {
        let [l, r] = eye_shifts(depth, 1920.0, 100);
        l - r
    }

    const POINTER: StereoState = StereoState { pointer_control: true, hovered: false, focused: false, in_control: false };
    const HOVERED: StereoState = StereoState { pointer_control: true, hovered: true, focused: false, in_control: false };

    fn lift(l: f32) -> StereoRequest {
        StereoRequest { lift: Some(l), ..Default::default() }
    }

    fn focused(on: bool) -> StereoRequest {
        StereoRequest { focused: Some(on), ..Default::default() }
    }

    /// The resolution GPUI does for every element, with the app's policy: nested surfaces,
    /// controls on them, hover and focus, overlays and the ceiling.
    #[test]
    fn depth_resolution() {
        let p = policy();
        let none = StereoState::default();
        let r = |parent: f32, req: StereoRequest, s: StereoState| resolve_stereo_depth(parent, req, s, &p).unwrap();
        // The page (0) → a card → a panel in the card: 1, 2 px.
        let card = r(PLANE, lift(SURFACE), none);
        let panel = r(card, lift(SURFACE), none);
        assert_eq!((px(card), px(panel)), (1, 2));
        // A button on the page, on the card, on the panel: it pops from its own surface.
        assert_eq!(px(r(PLANE, StereoRequest::default(), POINTER)), 1);
        assert_eq!(px(r(card, StereoRequest::default(), POINTER)), 2);
        assert_eq!(px(r(panel, StereoRequest::default(), POINTER)), 3);
        // The card's button: rest 2, hovered 3, focused 4, focused and hovered still 4.
        let rest = r(card, focused(false), POINTER);
        let hover = r(card, focused(false), HOVERED);
        let focus = r(card, focused(true), POINTER);
        let both = r(card, focused(true), HOVERED);
        assert_eq!([rest, hover, focus, both].map(px), [2, 3, 4, 4]);
        // Something only the app's navigation can focus is a control too.
        assert_eq!(px(r(card, focused(false), none)), 2);
        assert_eq!(px(r(card, focused(true), none)), 4);
        // A plain element on the card inherits it (no scope of its own).
        assert_eq!(resolve_stereo_depth(card, StereoRequest::default(), none, &p), None);
        // A scrim with a click handler stays flat.
        let flat = StereoRequest { control: Some(false), ..Default::default() };
        assert_eq!(resolve_stereo_depth(OVERLAY, flat, POINTER, &p), None);
    }

    /// A clickable wrapper around a pill with the focus ring (the top bar's sections) is one
    /// control; a container that only tracks GPUI's keyboard focus (the app's root) isn't one.
    #[test]
    fn compound_controls_and_key_focus() {
        let p = policy();
        let wrapper = resolve_stereo_depth(SURFACE, StereoRequest::default(), POINTER, &p).unwrap();
        let inside = StereoState { in_control: true, ..Default::default() };
        let pill = |on| resolve_stereo_depth(wrapper, focused(on), inside, &p).unwrap();
        assert_eq!([px(wrapper), px(pill(false)), px(pill(true))], [2, 2, 4]);
        // A real button inside a clickable row is still a control of its own.
        let button = StereoState { in_control: true, ..POINTER };
        assert_eq!(px(resolve_stereo_depth(wrapper, StereoRequest::default(), button, &p).unwrap()), 3);
        // The root holds the keyboard focus for its key bindings: nothing pops.
        let root = StereoState { focused: true, ..Default::default() };
        assert_eq!(resolve_stereo_depth(PLANE, StereoRequest::default(), root, &p), None);
        // A focused text input (a text cursor and the keyboard): two steps more.
        let typing = StereoState { focused: true, pointer_control: true, ..Default::default() };
        assert_eq!(px(resolve_stereo_depth(SURFACE, StereoRequest::default(), typing, &p).unwrap()), 4);
    }

    #[test]
    fn overlays_and_the_ceiling() {
        let p = policy();
        let r = |parent: f32, req: StereoRequest, s: StereoState| resolve_stereo_depth(parent, req, s, &p).unwrap();
        // A dialog over a raised card: the dialog's own depth, not the card's plus.
        let dialog = r(0.45, StereoRequest { depth: Some(OVERLAY), ..Default::default() }, StereoState::default());
        assert_eq!(px(dialog), 4);
        // Its buttons: 5 at rest, 6 hovered, 7 focused (0.6 + 0.15 + 0.3 = 1.05, clamped).
        let b = |req, s| px(r(dialog, req, s));
        assert_eq!([b(focused(false), POINTER), b(focused(false), HOVERED), b(focused(true), POINTER)], [5, 6, 7]);
        assert!((r(dialog, focused(true), POINTER) - MAX).abs() < 1e-6, "clamped to the ceiling");
        // A card inside a dialog, its button focused: still the ceiling, never past it.
        let inner = r(dialog, lift(SURFACE), StereoState::default());
        assert!(r(inner, focused(true), HOVERED) <= MAX);
        // Tooltips sit over the page's focused controls (4 px) at 5.
        assert_eq!(px(TOOLTIP), 5);
        assert!(px(TOOLTIP) > px(r(SURFACE, focused(true), POINTER)));
    }

    #[test]
    fn strengths() {
        // The `sight.depth` vocabulary: 50, 100, 150, 200%. The nearest thing (the ceiling).
        let at = |percent| {
            let [l, r] = eye_shifts(MAX, 1920.0, percent);
            l - r
        };
        assert_eq!([at(0), at(50), at(100), at(150), at(200)], [0, 3, 7, 10, 14]);
        // Whole pixels, the left eye taking the odd one.
        assert_eq!(eye_shifts(MAX, 1920.0, 100), [4, -3]);
        assert_eq!(eye_shifts(OVERLAY, 1920.0, 100), [2, -2]);
        // Scaled to the pane's device pixels (a 2560-wide XR monitor at scale 1).
        assert_eq!(eye_shifts(MAX, 2560.0, 100), [5, -4]);
    }
}
