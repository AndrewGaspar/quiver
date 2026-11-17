//! Where the HUD image goes in the swapchain (game presenter) and how the desktop surface is
//! anchored (desktop presenter), from the model's anchor, offset and safe area (§1.2).

use quiver_api::perf::Anchor;

/// The image's position in swapchain pixels, and the part of it that fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    /// The image cropped to the swapchain (normally the whole image).
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Start,
    Center,
    End,
}

fn sides(a: Anchor) -> (Side, Side) {
    use Side::*;
    match a {
        Anchor::TopLeft => (Start, Start),
        Anchor::Top => (Center, Start),
        Anchor::TopRight => (End, Start),
        Anchor::Left => (Start, Center),
        Anchor::Right => (End, Center),
        Anchor::BottomLeft => (Start, End),
        Anchor::Bottom => (Center, End),
        Anchor::BottomRight => (End, End),
    }
}

fn axis(side: Side, extent: u32, size: u32, margin: f32) -> i32 {
    let free = extent.saturating_sub(size) as f32;
    let v = match side {
        Side::Start => margin,
        Side::Center => free / 2.0,
        Side::End => free - margin,
    };
    v.round().clamp(0.0, free) as i32
}

/// `offset` is in image pixels (logical px × the image's scale); `safe_area` a percentage of
/// the frame added to the margin on every side. The result is always inside the swapchain,
/// as the layer's validation requires (§3.16).
pub fn place(anchor: Anchor, offset: [f32; 2], safe_area: f32, extent: (u32, u32), size: (u32, u32)) -> Placement {
    let finite = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
    let safe = finite(safe_area).min(25.0) / 100.0;
    let (mx, my) = (finite(offset[0]) + extent.0 as f32 * safe, finite(offset[1]) + extent.1 as f32 * safe);
    let (w, h) = (size.0.min(extent.0), size.1.min(extent.1));
    let (sx, sy) = sides(anchor);
    Placement { x: axis(sx, extent.0, w, mx), y: axis(sy, extent.1, h, my), w, h }
}

/// The toast lane's corner: top-right (SIGHT §9), unless a HUD owns that corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastCorner {
    #[default]
    TopRight,
    BottomRight,
}

pub fn toast_corner<'a>(hud_anchors: impl IntoIterator<Item = &'a Anchor>) -> ToastCorner {
    if hud_anchors.into_iter().any(|a| *a == Anchor::TopRight) { ToastCorner::BottomRight } else { ToastCorner::TopRight }
}

/// Layer-shell anchor edges and margins (top, right, bottom, left) for the desktop HUD.
pub fn layer_anchor(a: Anchor, offset: [f32; 2]) -> (gpui::layer_shell::Anchor, (f32, f32, f32, f32)) {
    use gpui::layer_shell::Anchor as L;
    let (sx, sy) = sides(a);
    let (ox, oy) = (offset[0].max(0.0), offset[1].max(0.0));
    let mut edges = L::empty();
    let (mut t, mut r, mut b, mut l) = (0.0, 0.0, 0.0, 0.0);
    match sx {
        Side::Start => {
            edges |= L::LEFT;
            l = ox;
        }
        Side::End => {
            edges |= L::RIGHT;
            r = ox;
        }
        Side::Center => {}
    }
    match sy {
        Side::Start => {
            edges |= L::TOP;
            t = oy;
        }
        Side::End => {
            edges |= L::BOTTOM;
            b = oy;
        }
        Side::Center => {}
    }
    (edges, (t, r, b, l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_and_centres() {
        let e = (2560, 1600);
        assert_eq!(place(Anchor::TopLeft, [24.0, 24.0], 0.0, e, (400, 100)), Placement { x: 24, y: 24, w: 400, h: 100 });
        assert_eq!(place(Anchor::TopRight, [24.0, 24.0], 0.0, e, (400, 100)), Placement { x: 2560 - 400 - 24, y: 24, w: 400, h: 100 });
        assert_eq!(place(Anchor::BottomRight, [10.0, 20.0], 0.0, e, (400, 100)), Placement { x: 2150, y: 1480, w: 400, h: 100 });
        assert_eq!(place(Anchor::Top, [99.0, 5.0], 0.0, e, (400, 100)).x, 1080);
        assert_eq!(place(Anchor::Left, [5.0, 99.0], 0.0, e, (400, 100)).y, 750);
        // Safe area: 1.5% of the frame on top of the offset.
        assert_eq!(place(Anchor::TopLeft, [0.0, 0.0], 1.5, e, (400, 100)), Placement { x: 38, y: 24, w: 400, h: 100 });
    }

    #[test]
    fn always_inside_the_swapchain() {
        // Bigger than the frame: cropped, at the origin.
        assert_eq!(place(Anchor::BottomRight, [12.0, 12.0], 0.0, (320, 200), (400, 100)), Placement { x: 0, y: 88, w: 320, h: 100 });
        // A margin that would push it out.
        let p = place(Anchor::TopLeft, [5000.0, f32::NAN], 0.0, (1280, 720), (300, 80));
        assert_eq!((p.x, p.y), (980, 0));
        let p = place(Anchor::BottomRight, [-50.0, 5000.0], 0.0, (1280, 720), (300, 80));
        assert_eq!((p.x, p.y), (980, 0));
    }

    #[test]
    fn toasts_avoid_the_hud_corner() {
        assert_eq!(toast_corner(&[Anchor::TopLeft]), ToastCorner::TopRight);
        assert_eq!(toast_corner(&[Anchor::TopLeft, Anchor::TopRight]), ToastCorner::BottomRight);
        assert_eq!(toast_corner(&[]), ToastCorner::TopRight);
    }

    #[test]
    fn layer_anchors() {
        use gpui::layer_shell::Anchor as L;
        assert_eq!(layer_anchor(Anchor::TopLeft, [12.0, 8.0]), (L::TOP | L::LEFT, (8.0, 0.0, 0.0, 12.0)));
        assert_eq!(layer_anchor(Anchor::BottomRight, [12.0, 8.0]), (L::BOTTOM | L::RIGHT, (0.0, 12.0, 8.0, 0.0)));
        assert_eq!(layer_anchor(Anchor::Top, [12.0, 8.0]).0, L::TOP);
    }
}
