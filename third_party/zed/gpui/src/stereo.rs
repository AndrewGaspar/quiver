//! Quiver: stereo depth for frames packed side by side for two eyes (Quiver's
//! design/STEREO-UI.md). Elements ask for a depth through their style
//! ([`crate::Styled::stereo_depth`], [`crate::Styled::stereo_lift`],
//! [`crate::Styled::stereo_focused`], [`crate::Styled::stereo_control`]); the window resolves
//! it against the depth of the element they are drawn on and its [`StereoPolicy`]. Nothing
//! here does anything unless the window packs stereo frames
//! ([`crate::Window::set_stereo_packing`]).
//!
//! A depth is HypXRland's unitless rung: 0 is the window's plane, 1 the top of the ladder.
//! Nothing goes behind the plane.

/// How a window turns an element's style into a depth. The app sets it once
/// ([`crate::Window::set_stereo_policy`]); the default lifts nothing by itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoPolicy {
    /// How far a control (anything that looks clickable or typeable: a pointer or text
    /// cursor, or [`crate::Styled::stereo_focused`] / [`crate::Styled::stereo_control`])
    /// rises above the surface it is drawn on.
    pub control: f32,
    /// A hovered control's extra rise. Never more than [`StereoPolicy::focus`].
    pub hover: f32,
    /// A focused control's extra rise (keyboard or controller focus).
    pub focus: f32,
    /// The ceiling every resolved depth is clamped to.
    pub max: f32,
    /// The depth tooltips are drawn at.
    pub tooltip: f32,
}

impl Default for StereoPolicy {
    fn default() -> Self {
        Self {
            control: 0.0,
            hover: 0.0,
            focus: 0.0,
            max: 1.0,
            tooltip: 0.0,
        }
    }
}

/// What an element's style asks for (the `stereo_*` fields of [`crate::Style`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StereoRequest {
    /// An absolute depth: the element no longer sits on its parent's depth but on this one.
    pub depth: Option<f32>,
    /// A rise above the base (the parent's depth, or `depth`).
    pub lift: Option<f32>,
    /// The element has the app's keyboard/controller focus (`Some(false)`: it can have it,
    /// so it is a control, but doesn't now).
    pub focused: Option<bool>,
    /// Overrides whether the element is a control: `Some(false)` keeps a clickable thing (a
    /// scrim, a whole-page click target) on its parent's depth.
    pub control: Option<bool>,
}

/// What the window knows about an element while it paints it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StereoState {
    /// It shows a pointer or a text cursor: it looks clickable or typeable.
    pub pointer_control: bool,
    /// The pointer is over it.
    pub hovered: bool,
    /// It holds GPUI's keyboard focus. Counts only on a control (a text input), not on a
    /// container that tracks focus for its key bindings.
    pub focused: bool,
    /// It's drawn inside a pointer control that doesn't take the app's focus itself (a
    /// clickable wrapper around a pill with the focus ring): the two are one control, so
    /// the inner one only adds its focus lift, not a second control lift.
    pub in_control: bool,
}

impl StereoRequest {
    /// Whether this asks for nothing (the element inherits its parent's depth).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The depth an element is painted at, or `None` when it inherits `parent`'s unchanged (no
/// scope of its own). Controls rise [`StereoPolicy::control`] above their base, and a further
/// [`StereoPolicy::focus`] while focused or [`StereoPolicy::hover`] (at most the focus lift)
/// while hovered: the larger of the two, so a focused control under the pointer doesn't
/// rise twice. The result is clamped to `0..=policy.max`.
pub fn resolve_stereo_depth(
    parent: f32,
    request: StereoRequest,
    state: StereoState,
    policy: &StereoPolicy,
) -> Option<f32> {
    let control = request
        .control
        .unwrap_or(state.pointer_control || request.focused.is_some());
    // GPUI's own focus counts on a control (a text input), not on a container that only
    // holds the keyboard focus for its key bindings.
    let focused = request.focused.unwrap_or(false) || (state.focused && control);
    if request.depth.is_none() && request.lift.is_none() && !control {
        return None;
    }
    let base = request.depth.unwrap_or(parent);
    let mut lift = request.lift.unwrap_or(0.0);
    if control {
        let merged = state.in_control && !state.pointer_control && request.control.is_none();
        let hover = if state.hovered {
            policy.hover.min(policy.focus.max(0.0)).max(0.0)
        } else {
            0.0
        };
        let focus = if focused { policy.focus.max(0.0) } else { 0.0 };
        let rise = if merged { 0.0 } else { policy.control.max(0.0) };
        lift += rise + hover.max(focus);
    }
    Some((base + lift).clamp(0.0, policy.max.max(0.0)))
}

/// One eye's shift (device pixels, `+` right) for something `margin_left` / `margin_right`
/// device pixels from the eye's edges, limited so it never slides past the edge it moves
/// toward by more than the greater of its margin there and `slack`: a sliver one eye sees
/// and the other doesn't is the worst stereo artifact (frame violation). HypXRland's
/// `depth_edge_slack` rule, the same as `quiver_perf::stereo::clamp_to_frame`. Margins are
/// floored so whole-pixel shifts stay whole.
pub fn clamp_eye_shift(shift: f32, margin_left: f32, margin_right: f32, slack: f32) -> f32 {
    if shift > 0.0 {
        shift.min(margin_right.floor().max(slack))
    } else if shift < 0.0 {
        shift.max(-margin_left.floor().max(slack))
    } else {
        0.0
    }
}

/// What packing has cost, for profiling ([`crate::Window::stereo_stats`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct StereoStats {
    /// Frames packed.
    pub packed: u64,
    /// Presents that reused the last packed frame (the scene hadn't changed).
    pub reused: u64,
    /// The last pack's CPU time.
    pub last_pack: std::time::Duration,
    /// The total CPU time spent packing.
    pub total_pack: std::time::Duration,
    /// Primitives in the last packed frame (both eyes).
    pub primitives: usize,
    /// Depth scopes in the last frame.
    pub scopes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: StereoPolicy = StereoPolicy {
        control: 0.15,
        hover: 0.15,
        focus: 0.3,
        max: 1.0,
        tooltip: 0.75,
    };

    fn r(depth: Option<f32>, lift: Option<f32>, focused: Option<bool>) -> StereoRequest {
        StereoRequest {
            depth,
            lift,
            focused,
            control: None,
        }
    }

    fn near(a: Option<f32>, b: f32) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-6)
    }

    #[test]
    fn plain_elements_inherit() {
        let s = StereoState::default();
        assert_eq!(resolve_stereo_depth(0.3, StereoRequest::default(), s, &APP), None);
        // A hover on something that isn't a control changes nothing.
        let hovered = StereoState {
            hovered: true,
            ..s
        };
        assert_eq!(resolve_stereo_depth(0.3, StereoRequest::default(), hovered, &APP), None);
    }

    #[test]
    fn nested_surfaces_add_up() {
        let s = StereoState::default();
        // The page (0) → a card (+0.15) → a panel in it (+0.15).
        let card = resolve_stereo_depth(0.0, r(None, Some(0.15), None), s, &APP).unwrap();
        let panel = resolve_stereo_depth(card, r(None, Some(0.15), None), s, &APP).unwrap();
        assert!(near(Some(card), 0.15) && near(Some(panel), 0.3));
        // An absolute depth replaces the parent's: an overlay over a raised card.
        assert!(near(resolve_stereo_depth(panel, r(Some(0.6), None, None), s, &APP), 0.6));
        assert!(near(resolve_stereo_depth(panel, r(Some(0.6), Some(0.15), None), s, &APP), 0.75));
    }

    #[test]
    fn controls_pop_from_their_base() {
        let pointer = StereoState {
            pointer_control: true,
            ..Default::default()
        };
        // A button on the page, and the same button on a card: it pops from the card.
        assert!(near(resolve_stereo_depth(0.0, StereoRequest::default(), pointer, &APP), 0.15));
        assert!(near(resolve_stereo_depth(0.15, StereoRequest::default(), pointer, &APP), 0.3));
        // A row that is itself a control, and a button on that row.
        let row = resolve_stereo_depth(0.15, StereoRequest::default(), pointer, &APP).unwrap();
        assert!(near(resolve_stereo_depth(row, StereoRequest::default(), pointer, &APP), 0.45));
        // Something focusable by the app's navigation is a control even without a pointer.
        let s = StereoState::default();
        assert!(near(resolve_stereo_depth(0.0, r(None, None, Some(false)), s, &APP), 0.15));
        // `stereo_control(false)` keeps a clickable scrim flat.
        let flat = StereoRequest {
            control: Some(false),
            ..Default::default()
        };
        assert_eq!(resolve_stereo_depth(0.6, flat, pointer, &APP), None);
    }

    #[test]
    fn focus_and_hover_lift_more() {
        let pointer = StereoState {
            pointer_control: true,
            ..Default::default()
        };
        let hovered = StereoState {
            hovered: true,
            ..pointer
        };
        let base = 0.15;
        let rest = resolve_stereo_depth(base, StereoRequest::default(), pointer, &APP).unwrap();
        let hover = resolve_stereo_depth(base, StereoRequest::default(), hovered, &APP).unwrap();
        let focus = resolve_stereo_depth(base, r(None, None, Some(true)), pointer, &APP).unwrap();
        let both = resolve_stereo_depth(base, r(None, None, Some(true)), hovered, &APP).unwrap();
        assert!(near(Some(rest), 0.3));
        assert!(near(Some(hover), 0.45));
        assert!(near(Some(focus), 0.6));
        assert!(near(Some(both), 0.6), "focus and hover don't stack");
        assert!(rest < hover && hover <= focus);
        // GPUI's own focus on a text input counts as focus; on a container that only
        // tracks focus for its key bindings it changes nothing.
        let typing = StereoState {
            focused: true,
            pointer_control: true,
            ..StereoState::default()
        };
        assert!(near(resolve_stereo_depth(base, StereoRequest::default(), typing, &APP), 0.6));
        let root = StereoState {
            focused: true,
            ..StereoState::default()
        };
        assert_eq!(resolve_stereo_depth(0.0, StereoRequest::default(), root, &APP), None);
        // Hover never rises more than focus, whatever the policy says.
        let eager = StereoPolicy {
            hover: 0.5,
            ..APP
        };
        assert!(near(resolve_stereo_depth(0.0, StereoRequest::default(), hovered, &eager), 0.45));
    }

    #[test]
    fn a_wrapper_and_its_focus_pill_are_one_control() {
        // A clickable wrapper (pointer, no focus of its own) around a pill with the ring.
        let pointer = StereoState {
            pointer_control: true,
            ..Default::default()
        };
        let wrapper = resolve_stereo_depth(0.15, StereoRequest::default(), pointer, &APP).unwrap();
        let inside = StereoState {
            in_control: true,
            ..Default::default()
        };
        // At rest the pill stays on the wrapper; focused it rises the focus lift only.
        assert!(near(resolve_stereo_depth(wrapper, r(None, None, Some(false)), inside, &APP), 0.3));
        assert!(near(resolve_stereo_depth(wrapper, r(None, None, Some(true)), inside, &APP), 0.6));
        // A real button inside a clickable row is still a control of its own.
        let button = StereoState {
            in_control: true,
            pointer_control: true,
            ..Default::default()
        };
        assert!(near(resolve_stereo_depth(wrapper, StereoRequest::default(), button, &APP), 0.45));
    }

    #[test]
    fn clamped_to_the_ceiling() {
        let pointer = StereoState {
            pointer_control: true,
            ..Default::default()
        };
        // A focused button on a dialog (0.6): 0.6 + 0.15 + 0.3 = 1.05, held at 1.0.
        assert!(near(resolve_stereo_depth(0.6, r(None, None, Some(true)), pointer, &APP), 1.0));
        // A button on a card in a dialog, hovered: 0.75 + 0.15 + 0.15 = 1.05 → 1.0.
        let hovered = StereoState {
            hovered: true,
            ..pointer
        };
        assert!(near(resolve_stereo_depth(0.75, StereoRequest::default(), hovered, &APP), 1.0));
        // Nothing goes behind the plane.
        assert!(near(resolve_stereo_depth(0.0, r(None, Some(-0.5), None), StereoState::default(), &APP), 0.0));
        // The default policy lifts nothing by itself, but explicit depths still apply.
        let none = StereoPolicy::default();
        assert_eq!(resolve_stereo_depth(0.0, StereoRequest::default(), hovered, &none), Some(0.0));
        assert!(near(resolve_stereo_depth(0.0, r(Some(0.8), None, None), StereoState::default(), &none), 0.8));
    }

    #[test]
    fn edge_clamp() {
        // Room to move: unchanged.
        assert_eq!(clamp_eye_shift(3.0, 40.0, 40.0, 2.0), 3.0);
        assert_eq!(clamp_eye_shift(-2.0, 40.0, 40.0, 2.0), -2.0);
        // Flush with the right edge, moving right: the slack only.
        assert_eq!(clamp_eye_shift(4.0, 40.0, 0.0, 2.0), 2.0);
        // Flush with the left edge, moving left.
        assert_eq!(clamp_eye_shift(-4.0, 0.0, 40.0, 2.0), -2.0);
        // Moving away from the edge it is flush with is fine.
        assert_eq!(clamp_eye_shift(4.0, 0.0, 40.0, 2.0), 4.0);
        // A fractional margin is floored: whole pixels stay whole.
        assert_eq!(clamp_eye_shift(5.0, 40.0, 3.6, 2.0), 3.0);
        // Partly off the window: the slack.
        assert_eq!(clamp_eye_shift(-3.0, -10.0, 40.0, 2.0), -2.0);
    }
}
