//! Packed stereo frames (design/SIGHT.md §15): how a game's presented image carries two eyes,
//! and the geometry Sight needs to draw its HUD and panel once per eye and to map the pointer
//! into the eye it is over.
//!
//! The layouts and their spellings are HypXRland's (`~/code/hypxrland/docs/openxr/
//! 05-configuration.md` §8.6, the `stereo:<layout>` xdg tag), so one string names a layout
//! everywhere: the mod SDK's `quiver_set_stereo_layout`, the `sight.stereo` Tweak, the
//! compositor's window rule.
//!
//! | Layout | The frame is | Each eye is | Overlay laid out at (logical) |
//! |---|---|---|---|
//! | `mono` | one view | the whole frame | the frame |
//! | `sbs` | 2× wide | the left / right half, 1:1 | half the width × the height |
//! | `hsbs` | window width | the left / right half, squeezed 2:1 horizontally | the width × the height |
//! | `tab` | 2× tall | the top / bottom half, 1:1 | the width × half the height |
//! | `htab` | window height | the top / bottom half, squeezed 2:1 vertically | the width × the height |
//!
//! The left eye is the left (or top) half. Physical pixels are the swapchain's; *logical*
//! pixels are one eye's un-squeezed space, which is what the overlay's images are laid out in
//! (for `mono` the two are the same). An image drawn in both eyes at the same logical position
//! is at zero disparity, as the game's own HUD is.

/// `qv_hud_buffer.stereo`, `qv_hud_header.stereo`, `qv_layer_status.stereo_*`: the layout codes.
pub const STEREO_MONO: u32 = 0;
pub const STEREO_SBS: u32 = 1;
pub const STEREO_HSBS: u32 = 2;
pub const STEREO_TAB: u32 = 3;
pub const STEREO_HTAB: u32 = 4;
/// `qv_layer_status.stereo_setting`: set (the code is in the low byte); clear = not set (auto).
pub const STEREO_SETTING_SET: u32 = 0x100;
/// `qv_layer_status.stereo_setting`: the variable is set but isn't a layout.
pub const STEREO_SETTING_INVALID: u32 = 0x200;

/// The game's environment variable for the `sight.stereo` Tweak (the fallback for mods without
/// the SDK call): `auto` (or unset), `mono`, `sbs`, `hsbs`, `tab`, `htab`.
pub const ENV_STEREO: &str = "QUIVER_SIGHT_STEREO";

/// How a frame packs its eyes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Stereo {
    #[default]
    Mono,
    Sbs,
    Hsbs,
    Tab,
    Htab,
}

/// A rectangle in swapchain pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Stereo {
    pub const ALL: [Stereo; 5] = [Stereo::Mono, Stereo::Sbs, Stereo::Hsbs, Stereo::Tab, Stereo::Htab];

    /// `mono`, `sbs`, `hsbs`, `tab`, `htab`, also with HypXRland's tag prefix (`stereo:sbs`)
    /// and `off` for `mono`. Exact, lowercase: the string is a compatibility contract.
    pub fn parse(s: &str) -> Option<Stereo> {
        let s = s.strip_prefix("stereo:").unwrap_or(s);
        Some(match s {
            "mono" | "off" => Stereo::Mono,
            "sbs" => Stereo::Sbs,
            "hsbs" => Stereo::Hsbs,
            "tab" => Stereo::Tab,
            "htab" => Stereo::Htab,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Stereo::Mono => "mono",
            Stereo::Sbs => "sbs",
            Stereo::Hsbs => "hsbs",
            Stereo::Tab => "tab",
            Stereo::Htab => "htab",
        }
    }

    pub fn code(self) -> u32 {
        match self {
            Stereo::Mono => STEREO_MONO,
            Stereo::Sbs => STEREO_SBS,
            Stereo::Hsbs => STEREO_HSBS,
            Stereo::Tab => STEREO_TAB,
            Stereo::Htab => STEREO_HTAB,
        }
    }

    /// `None` for a code no layout has (an image that says so isn't drawn).
    pub fn from_code(c: u32) -> Option<Stereo> {
        Self::ALL.into_iter().find(|s| s.code() == c)
    }

    pub fn is_stereo(self) -> bool {
        self != Stereo::Mono
    }

    /// The eyes are side by side (`sbs`, `hsbs`) rather than over-under.
    pub fn side_by_side(self) -> bool {
        matches!(self, Stereo::Sbs | Stereo::Hsbs)
    }

    pub fn eyes(self) -> u32 {
        if self.is_stereo() { 2 } else { 1 }
    }

    /// Physical pixels per logical pixel along x and y: ½ along the squeezed axis.
    pub fn squeeze(self) -> (f32, f32) {
        match self {
            Stereo::Hsbs => (0.5, 1.0),
            Stereo::Htab => (1.0, 0.5),
            _ => (1.0, 1.0),
        }
    }

    /// One eye's rectangle in a frame of `extent` (eye 0 left or top). An odd split leaves
    /// the last column (row) to neither eye.
    pub fn eye_rect(self, extent: (u32, u32), eye: u32) -> Rect {
        let (w, h) = extent;
        let e = eye.min(self.eyes() - 1);
        match self {
            Stereo::Mono => Rect { x: 0, y: 0, w, h },
            Stereo::Sbs | Stereo::Hsbs => Rect { x: e * (w / 2), y: 0, w: w / 2, h },
            Stereo::Tab | Stereo::Htab => Rect { x: 0, y: e * (h / 2), w, h: h / 2 },
        }
    }

    /// The size the overlay is laid out at for a frame of `extent`: one eye, un-squeezed.
    pub fn logical_extent(self, extent: (u32, u32)) -> (u32, u32) {
        let r = self.eye_rect(extent, 0);
        match self {
            Stereo::Hsbs => (r.w * 2, r.h),
            Stereo::Htab => (r.w, r.h * 2),
            _ => (r.w, r.h),
        }
    }

    /// The layout fits this frame: each eye at least 2 × 2 physical pixels.
    pub fn fits(self, extent: (u32, u32)) -> bool {
        let r = self.eye_rect(extent, 0);
        r.w >= 2 && r.h >= 2
    }

    /// A position in the frame (swapchain pixels, as an X11 pointer in a window of the
    /// swapchain's size) → which eye it is over and the logical position in that eye: `x mod`
    /// the eye's width for `sbs`, then ×2 for `hsbs`; the same vertically for `tab`/`htab`.
    /// Positions past the frame stay with the nearest eye.
    pub fn to_logical(self, extent: (u32, u32), p: (f64, f64)) -> (u32, (f64, f64)) {
        if !self.is_stereo() {
            return (0, p);
        }
        let r = self.eye_rect(extent, 0);
        let (sx, sy) = self.squeeze();
        let split = |v: f64, size: u32| -> (u32, f64) {
            let s = size.max(1) as f64;
            if v >= s { (1, v - s) } else { (0, v) }
        };
        let (eye, lx, ly) = if self.side_by_side() {
            let (e, x) = split(p.0, r.w);
            (e, x, p.1)
        } else {
            let (e, y) = split(p.1, r.h);
            (e, p.0, y)
        };
        (eye, (lx / sx as f64, ly / sy as f64))
    }

    /// A logical position in `eye` → swapchain pixels.
    pub fn to_physical(self, extent: (u32, u32), eye: u32, p: (f64, f64)) -> (f64, f64) {
        let r = self.eye_rect(extent, eye);
        let (sx, sy) = self.squeeze();
        (r.x as f64 + p.0 * sx as f64, r.y as f64 + p.1 * sy as f64)
    }

    /// A logical rectangle drawn in `eye`, as the swapchain pixels it covers (rounded out) and
    /// clipped to that eye: the scissor for drawing it there.
    pub fn eye_scissor(self, extent: (u32, u32), eye: u32, x: i32, y: i32, w: u32, h: u32) -> Rect {
        let r = self.eye_rect(extent, eye);
        // Logical px per physical px along each axis (integers, so this works without std).
        let (dx, dy): (i64, i64) = match self {
            Stereo::Hsbs => (2, 1),
            Stereo::Htab => (1, 2),
            _ => (1, 1),
        };
        let floor = |v: i64, d: i64| v.div_euclid(d);
        let ceil = |v: i64, d: i64| -(-v).div_euclid(d);
        let (rx0, ry0, rx1, ry1) = (r.x as i64, r.y as i64, (r.x + r.w) as i64, (r.y + r.h) as i64);
        let x0 = (rx0 + floor(x as i64, dx)).clamp(rx0, rx1);
        let y0 = (ry0 + floor(y as i64, dy)).clamp(ry0, ry1);
        let x1 = (rx0 + ceil(x as i64 + w as i64, dx)).clamp(x0, rx1);
        let y1 = (ry0 + ceil(y as i64 + h as i64, dy)).clamp(y0, ry1);
        Rect { x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 }
    }
}

/// The `sight.stereo` setting (Tweak) as the game's environment has it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Setting {
    /// Unset or `auto`: the mod's SDK call decides, else mono.
    #[default]
    Auto,
    Fixed(Stereo),
    /// Set to something that isn't a layout (ignored, reported).
    Invalid,
}

impl Setting {
    pub fn parse(v: Option<&str>) -> Setting {
        match v.map(str::trim) {
            None | Some("") | Some("auto") => Setting::Auto,
            Some(s) => Stereo::parse(s).map_or(Setting::Invalid, Setting::Fixed),
        }
    }

    /// `qv_layer_status.stereo_setting`.
    pub fn code(self) -> u32 {
        match self {
            Setting::Auto => 0,
            Setting::Fixed(s) => STEREO_SETTING_SET | s.code(),
            Setting::Invalid => STEREO_SETTING_INVALID,
        }
    }

    pub fn from_code(c: u32) -> Setting {
        if c & STEREO_SETTING_INVALID != 0 {
            Setting::Invalid
        } else if c & STEREO_SETTING_SET != 0 {
            Stereo::from_code(c & 0xff).map_or(Setting::Invalid, Setting::Fixed)
        } else {
            Setting::Auto
        }
    }
}

/// Who decided the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A mod said so through the SDK (`quiver_set_stereo_layout`), and its channel is live.
    Mod,
    /// The `sight.stereo` setting (`QUIVER_SIGHT_STEREO` in the game's environment).
    Setting,
    /// Neither: mono.
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Mod => "mod",
            Source::Setting => "setting",
            Source::Default => "default",
        }
    }
}

/// The precedence (design/SIGHT.md §15): a live mod's SDK declaration, else the setting, else
/// mono. The mod wins while its channel is up because it knows what it presents frame by frame
/// (Torchbearer switches packing at runtime); the setting is for games whose mod can't say.
pub fn resolve(sdk: Option<Stereo>, setting: Setting) -> (Stereo, Source) {
    match (sdk, setting) {
        (Some(s), _) => (s, Source::Mod),
        (None, Setting::Fixed(s)) => (s, Source::Setting),
        _ => (Stereo::Mono, Source::Default),
    }
}

// ---------------------------------------------------------------------------------- depth

/// The game's environment variable for the `sight.depth` Tweak: how strongly Sight's HUD and
/// panel float in front of the game on a packed stereo frame, in percent of the shared ladder
/// (design/STEREO-UI.md): `0` or `off` draws everything at the game's own screen plane (zero
/// disparity, as before depth), unset is 100.
pub const ENV_DEPTH: &str = "QUIVER_SIGHT_DEPTH";

/// The strength's ceiling, percent: at 200 the nearest rung is ≈13 px of total disparity on a
/// 1920-wide eye, ≈0.3° on an XReal (46° per eye), still well inside HypXRland's 1° cap.
pub const DEPTH_MAX_PERCENT: u32 = 200;

/// `qv_layer_status.depth_setting`: set (the percent is in the low 16 bits); clear = not set
/// (100).
pub const DEPTH_SETTING_SET: u32 = 0x1_0000;
/// The variable is set but isn't `off` or a percent.
pub const DEPTH_SETTING_INVALID: u32 = 0x2_0000;

/// A depth is HypXRland's unitless rung (`windowrule = depth`, its 05-configuration.md §8.7):
/// 0 is the screen plane (the game, the wallpaper), 1 is the top of the ladder. Descriptors
/// carry it in thousandths ([`HudLift::depth`](crate::layout::HudLift),
/// `qv_hud_buffer.depth`).
pub const DEPTH_ONE: u16 = 1000;

/// The rungs Sight draws at (design/STEREO-UI.md "The ladder"). Total disparity at 100% on a
/// 1920-wide eye in the comment: every step between two things seen together is 2 px
/// (≈0.05° on an XReal), under the ≈0.1° foveal fusion limit, and the nearest is 7 px
/// (≈0.17°), a sixth of the 1° comfort budget.
pub mod rung {
    /// The perf HUD (2 px): just in front of the game's own HUD, which is at zero.
    pub const HUD: u16 = 300;
    /// The panel's card (3 px). Its scrim and the blur stay at the game's plane.
    pub const PANEL: u16 = 450;
    /// The focused row or section (5 px).
    pub const FOCUS: u16 = 750;
    /// A dialog's box (5 px: over the card's scrim, which is at the card's depth).
    pub const DIALOG: u16 = 750;
    /// A dialog's focused button (7 px).
    pub const DIALOG_FOCUS: u16 = 1000;
}

/// The app's ladder (quiver-app, through `quiver_ui::depth`; design/STEREO-UI.md §4), in the
/// same thousandths as [`rung`]. Two kinds of number: *absolute* rungs a layer sits on
/// whatever it is drawn over, and *lifts* added to the depth of the surface something is
/// drawn on, so a button on a raised card pops from the card. Every lift is one step of 150
/// (≈1 px of total disparity at 100% on a 1920 eye) except focus, which is two; resolved
/// depths are clamped to [`ui::MAX`]. Total pixels at 100%, 1920 eye, in the comments.
pub mod ui {
    /// Backgrounds, hero art, the page itself: the window's plane (0 px).
    pub const PLANE: u16 = 0;
    /// The top bar and the controller hint bar (1 px).
    pub const CHROME: u16 = 150;
    /// Lift: a content surface (card, panel, list, tile grid) over what it is on (+1 px).
    pub const SURFACE: u16 = 150;
    /// Lift: a control (button, tab, chip, row, input, link…) over its surface (+1 px).
    pub const CONTROL: u16 = 150;
    /// Lift: a hovered control's extra (+1 px). Never more than [`FOCUS`].
    pub const HOVER: u16 = 150;
    /// Lift: a focused control's extra (keyboard or controller focus, +2 px).
    pub const FOCUS: u16 = 300;
    /// Menus, popovers, dialogs, the palette, toasts: above the whole page (4 px).
    pub const OVERLAY: u16 = 600;
    /// Tooltips (5 px).
    pub const TOOLTIP: u16 = 750;
    /// The ceiling (7 px): a focused control on a dialog, clamped from 1050.
    pub const MAX: u16 = 1000;
}

/// HypXRland's depth geometry (05-configuration.md §8.7 and research/24 §8.1), shared so a
/// rung means the same disparity in Sight, the app and the compositor's depth desktop:
/// a rung of 1 rises `depth_scale` (0.12 m) out of a virtual screen 1.6 m wide at 1.5 m,
/// seen with a 63 mm interocular distance.
pub mod geometry {
    pub const DEPTH_SCALE_M: f64 = 0.12;
    pub const DISTANCE_M: f64 = 1.5;
    pub const SCREEN_WIDTH_M: f64 = 1.6;
    pub const IOD_M: f64 = 0.063;
}

/// The `sight.depth` setting as the game's environment has it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DepthSetting {
    /// Unset: 100%.
    #[default]
    Default,
    /// A percent of the ladder, 0 (off) ..= [`DEPTH_MAX_PERCENT`].
    Percent(u16),
    /// Set to something else (ignored: 100%, reported).
    Invalid,
}

impl DepthSetting {
    /// `off`, or a percent (`0`..`200`, a decimal like `150.0` too, as a float Tweak writes
    /// it). Over the ceiling is clamped to it.
    pub fn parse(v: Option<&str>) -> DepthSetting {
        let Some(s) = v.map(str::trim) else { return DepthSetting::Default };
        match s {
            "" => DepthSetting::Default,
            "off" => DepthSetting::Percent(0),
            _ => {
                let int = s.split_once('.').map_or(s, |(i, f)| if f.bytes().all(|b| b == b'0') { i } else { "" });
                match int.parse::<u32>() {
                    Ok(p) => DepthSetting::Percent(p.min(DEPTH_MAX_PERCENT) as u16),
                    Err(_) => DepthSetting::Invalid,
                }
            }
        }
    }

    /// The strength it gives, percent.
    pub fn percent(self) -> u32 {
        match self {
            DepthSetting::Percent(p) => p as u32,
            _ => 100,
        }
    }

    /// `qv_layer_status.depth_setting`.
    pub fn code(self) -> u32 {
        match self {
            DepthSetting::Default => 0,
            DepthSetting::Percent(p) => DEPTH_SETTING_SET | p as u32,
            DepthSetting::Invalid => DEPTH_SETTING_INVALID,
        }
    }

    pub fn from_code(c: u32) -> DepthSetting {
        if c & DEPTH_SETTING_INVALID != 0 {
            DepthSetting::Invalid
        } else if c & DEPTH_SETTING_SET != 0 {
            DepthSetting::Percent((c & 0xffff).min(DEPTH_MAX_PERCENT) as u16)
        } else {
            DepthSetting::Default
        }
    }
}

/// The total disparity, in whole logical pixels of an eye `eye_width` wide, of something at
/// `depth` (thousandths of a rung) at `percent` strength: HypXRland's formula,
/// `p = b·h / (D − h)` metres of crossed parallax for a rise `h`, times `P / W` pixels per
/// metre, rounded to the nearest pixel. Whole pixels keep text crisp in each eye (no
/// resampling); a pixel of total disparity is ≈1.4′ on an XReal, finer than the ladder's steps.
pub fn disparity_px(depth: u16, percent: u32, eye_width: u32) -> u32 {
    use geometry::*;
    let d = depth.min(DEPTH_ONE) as f64 / DEPTH_ONE as f64;
    let h = d * DEPTH_SCALE_M * percent.min(DEPTH_MAX_PERCENT) as f64 / 100.0;
    if h <= 0.0 {
        return 0;
    }
    let p = IOD_M * h / (DISTANCE_M - h);
    let px = p * eye_width as f64 / SCREEN_WIDTH_M;
    (px + 0.5) as u32
}

/// One eye's share of a total disparity, logical px along +x: content in front of the screen
/// moves right in the left eye (eye 0) and left in the right eye. An odd total gives the left
/// eye the extra pixel; the half-pixel that moves the fused position is invisible.
pub fn eye_shift(total: u32, eye: u32) -> i32 {
    let t = total as i32;
    if eye == 0 { (t + 1) / 2 } else { -(t / 2) }
}

/// An eye's shift for something `margin_left` / `margin_right` logical px from the eye's
/// edges, limited so it never slides past the edge: a sliver one eye sees and the other
/// doesn't is the worst stereo artifact (frame violation). HypXRland's rule: a shift may not
/// exceed the greater of the margin on the side it moves toward and a 2 px slack.
pub fn clamp_to_frame(shift: i32, margin_left: i32, margin_right: i32) -> i32 {
    const SLACK: i32 = 2;
    if shift > 0 {
        shift.min(margin_right.max(SLACK))
    } else {
        shift.max(-margin_left.max(SLACK))
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    #[test]
    fn the_ladder_in_pixels() {
        // HypXRland's own table (05 §8.7): 0.2 → 1.2, 0.6 → 3.8, 0.8 → 5.2, 1.0 → 6.6 px total
        // on a 1920 pane; whole pixels here.
        assert_eq!(disparity_px(200, 100, 1920), 1);
        assert_eq!(disparity_px(600, 100, 1920), 4);
        assert_eq!(disparity_px(800, 100, 1920), 5);
        assert_eq!(disparity_px(1000, 100, 1920), 7);
        // Sight's rungs: 2, 3, 5, 7.
        assert_eq!(disparity_px(rung::HUD, 100, 1920), 2);
        assert_eq!(disparity_px(rung::PANEL, 100, 1920), 3);
        assert_eq!(disparity_px(rung::FOCUS, 100, 1920), 5);
        assert_eq!(disparity_px(rung::DIALOG_FOCUS, 100, 1920), 7);
        // Off, and the screen plane.
        assert_eq!(disparity_px(rung::DIALOG_FOCUS, 0, 1920), 0);
        assert_eq!(disparity_px(0, 200, 1920), 0);
        // It scales with the eye's pixels (the eye spans the same screen) and the strength.
        assert_eq!(disparity_px(rung::FOCUS, 100, 960), 2);
        assert_eq!(disparity_px(1000, 200, 1920), 14);
        assert_eq!(disparity_px(1000, 500, 1920), 14, "clamped to 200%");
        assert_eq!(disparity_px(5000, 100, 1920), 7, "clamped to a rung of 1");
    }

    #[test]
    fn the_apps_ladder() {
        let px = |d: u16| disparity_px(d, 100, 1920);
        // Absolute rungs.
        assert_eq!(px(ui::PLANE), 0);
        assert_eq!(px(ui::CHROME), 1);
        assert_eq!(px(ui::OVERLAY), 4);
        assert_eq!(px(ui::TOOLTIP), 5);
        assert_eq!(px(ui::MAX), 7);
        // The page: a card, a button on it, hovered, focused: 1, 2, 3, 4 px.
        let card = ui::PLANE + ui::SURFACE;
        let button = card + ui::CONTROL;
        assert_eq!([card, button, button + ui::HOVER, button + ui::FOCUS].map(px), [1, 2, 3, 4]);
        // A dialog: its buttons at 5 px, hovered 6, focused 7 (clamped at the ceiling).
        let control = ui::OVERLAY + ui::CONTROL;
        assert_eq!([control, control + ui::HOVER, (control + ui::FOCUS).min(ui::MAX)].map(px), [5, 6, 7]);
        // Hover never rises more than focus, and every lift is at most two steps (≈2 px).
        assert!(ui::HOVER <= ui::FOCUS);
        for lift in [ui::SURFACE, ui::CONTROL, ui::HOVER, ui::FOCUS] {
            assert!(disparity_px(lift, 100, 1920) <= 2);
        }
        // At 200% the nearest is 14 px (≈0.34° on an XReal), inside the 1° cap (≈31 px).
        assert_eq!(disparity_px(ui::MAX, 200, 1920), 14);
    }

    #[test]
    fn eyes_split_the_disparity() {
        assert_eq!((eye_shift(5, 0), eye_shift(5, 1)), (3, -2));
        assert_eq!((eye_shift(4, 0), eye_shift(4, 1)), (2, -2));
        assert_eq!((eye_shift(0, 0), eye_shift(0, 1)), (0, 0));
        for t in 0..20 {
            assert_eq!(eye_shift(t, 0) - eye_shift(t, 1), t as i32, "the eyes differ by the total");
        }
    }

    #[test]
    fn nothing_slides_off_the_frame() {
        // Plenty of room: unchanged.
        assert_eq!(clamp_to_frame(3, 12, 12), 3);
        assert_eq!(clamp_to_frame(-2, 12, 12), -2);
        // Against the right edge, moving right: the slack only.
        assert_eq!(clamp_to_frame(3, 12, 0), 2);
        assert_eq!(clamp_to_frame(-3, 0, 40), -2);
        assert_eq!(clamp_to_frame(-1, 0, 40), -1);
    }

    #[test]
    fn the_setting() {
        assert_eq!(DepthSetting::parse(None), DepthSetting::Default);
        assert_eq!(DepthSetting::parse(Some("")), DepthSetting::Default);
        assert_eq!(DepthSetting::parse(Some("off")), DepthSetting::Percent(0));
        assert_eq!(DepthSetting::parse(Some("0")), DepthSetting::Percent(0));
        assert_eq!(DepthSetting::parse(Some(" 150 ")), DepthSetting::Percent(150));
        assert_eq!(DepthSetting::parse(Some("50.0")), DepthSetting::Percent(50));
        assert_eq!(DepthSetting::parse(Some("900")), DepthSetting::Percent(200));
        assert_eq!(DepthSetting::parse(Some("12.5")), DepthSetting::Invalid);
        assert_eq!(DepthSetting::parse(Some("deep")), DepthSetting::Invalid);
        assert_eq!(DepthSetting::parse(Some("-5")), DepthSetting::Invalid);
        assert_eq!(DepthSetting::Default.percent(), 100);
        assert_eq!(DepthSetting::Invalid.percent(), 100);
        for s in [DepthSetting::Default, DepthSetting::Invalid, DepthSetting::Percent(0), DepthSetting::Percent(175)] {
            assert_eq!(DepthSetting::from_code(s.code()), s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_are_hypxrlands() {
        for s in Stereo::ALL {
            assert_eq!(Stereo::parse(s.as_str()), Some(s));
            assert_eq!(Stereo::from_code(s.code()), Some(s));
        }
        assert_eq!(Stereo::parse("stereo:hsbs"), Some(Stereo::Hsbs));
        assert_eq!(Stereo::parse("off"), Some(Stereo::Mono));
        assert_eq!(Stereo::parse("SBS"), None, "exact and lowercase, like the xdg tag");
        assert_eq!(Stereo::parse("auto"), None, "auto is a setting, not a layout");
        assert_eq!(Stereo::from_code(9), None);
    }

    #[test]
    fn eyes_and_logical_sizes() {
        // Torchbearer's full SBS: a 3840x1080 window, 1920x1080 per eye.
        assert_eq!(Stereo::Sbs.eye_rect((3840, 1080), 0), Rect { x: 0, y: 0, w: 1920, h: 1080 });
        assert_eq!(Stereo::Sbs.eye_rect((3840, 1080), 1), Rect { x: 1920, y: 0, w: 1920, h: 1080 });
        assert_eq!(Stereo::Sbs.logical_extent((3840, 1080)), (1920, 1080));
        // Half SBS: a 1920x1080 window, each eye 960x1080 squeezed; laid out at 1920x1080.
        assert_eq!(Stereo::Hsbs.eye_rect((1920, 1080), 1), Rect { x: 960, y: 0, w: 960, h: 1080 });
        assert_eq!(Stereo::Hsbs.logical_extent((1920, 1080)), (1920, 1080));
        assert_eq!(Stereo::Tab.eye_rect((1920, 2160), 1), Rect { x: 0, y: 1080, w: 1920, h: 1080 });
        assert_eq!(Stereo::Tab.logical_extent((1920, 2160)), (1920, 1080));
        assert_eq!(Stereo::Htab.logical_extent((1920, 1080)), (1920, 1080));
        assert_eq!(Stereo::Htab.eye_rect((1920, 1080), 1), Rect { x: 0, y: 540, w: 1920, h: 540 });
        assert_eq!(Stereo::Mono.logical_extent((2560, 1600)), (2560, 1600));
        assert_eq!(Stereo::Mono.eyes(), 1);
        // Odd: the last column belongs to neither eye.
        assert_eq!(Stereo::Sbs.eye_rect((1921, 1080), 1), Rect { x: 960, y: 0, w: 960, h: 1080 });
        assert!(!Stereo::Sbs.fits((3, 1080)) && Stereo::Sbs.fits((4, 2)));
        assert_eq!(Stereo::Hsbs.squeeze(), (0.5, 1.0));
        assert_eq!(Stereo::Htab.squeeze(), (1.0, 0.5));
    }

    #[test]
    fn pointer_lands_in_the_eye_it_is_over() {
        // sbs: x mod the eye width.
        assert_eq!(Stereo::Sbs.to_logical((3840, 1080), (100.0, 50.0)), (0, (100.0, 50.0)));
        assert_eq!(Stereo::Sbs.to_logical((3840, 1080), (2020.0, 50.0)), (1, (100.0, 50.0)));
        // hsbs: ×2 within the half.
        assert_eq!(Stereo::Hsbs.to_logical((1920, 1080), (50.0, 30.0)), (0, (100.0, 30.0)));
        assert_eq!(Stereo::Hsbs.to_logical((1920, 1080), (1010.0, 30.0)), (1, (100.0, 30.0)));
        // tab / htab: the same vertically.
        assert_eq!(Stereo::Tab.to_logical((1920, 2160), (10.0, 1090.0)), (1, (10.0, 10.0)));
        assert_eq!(Stereo::Htab.to_logical((1920, 1080), (10.0, 545.0)), (1, (10.0, 10.0)));
        assert_eq!(Stereo::Htab.to_logical((1920, 1080), (10.0, 5.0)), (0, (10.0, 10.0)));
        // mono: unchanged.
        assert_eq!(Stereo::Mono.to_logical((1920, 1080), (1500.0, 9.0)), (0, (1500.0, 9.0)));
        // And back.
        for s in Stereo::ALL {
            let ext = (3840, 2160);
            for p in [(5.0, 7.0), (3000.0, 2000.0), (1919.0, 1079.0)] {
                let (eye, l) = s.to_logical(ext, p);
                let q = s.to_physical(ext, eye, l);
                assert!((q.0 - p.0).abs() < 1e-9 && (q.1 - p.1).abs() < 1e-9, "{s:?} {p:?} -> {eye} {l:?} -> {q:?}");
            }
        }
    }

    #[test]
    fn scissors_stay_in_their_eye() {
        // A 200x100 image at logical (10, 20), hsbs on 1920x1080: x squeezed, both eyes.
        assert_eq!(Stereo::Hsbs.eye_scissor((1920, 1080), 0, 10, 20, 200, 100), Rect { x: 5, y: 20, w: 100, h: 100 });
        assert_eq!(Stereo::Hsbs.eye_scissor((1920, 1080), 1, 10, 20, 200, 100), Rect { x: 965, y: 20, w: 100, h: 100 });
        // An odd logical x rounds out.
        assert_eq!(Stereo::Hsbs.eye_scissor((1920, 1080), 0, 11, 0, 3, 10), Rect { x: 5, y: 0, w: 2, h: 10 });
        // A full-eye panel is clipped to its eye, never across the seam.
        assert_eq!(Stereo::Sbs.eye_scissor((3840, 1080), 0, 0, 0, 1920, 1080), Rect { x: 0, y: 0, w: 1920, h: 1080 });
        assert_eq!(Stereo::Sbs.eye_scissor((3840, 1080), 1, 0, 0, 4000, 1080), Rect { x: 1920, y: 0, w: 1920, h: 1080 });
        assert_eq!(Stereo::Htab.eye_scissor((1920, 1080), 1, 0, 1070, 100, 20), Rect { x: 0, y: 1075, w: 100, h: 5 });
        assert_eq!(Stereo::Mono.eye_scissor((1920, 1080), 0, 10, 10, 20, 20), Rect { x: 10, y: 10, w: 20, h: 20 });
    }

    #[test]
    fn settings_and_precedence() {
        assert_eq!(Setting::parse(None), Setting::Auto);
        assert_eq!(Setting::parse(Some("auto")), Setting::Auto);
        assert_eq!(Setting::parse(Some(" hsbs ")), Setting::Fixed(Stereo::Hsbs));
        assert_eq!(Setting::parse(Some("3d")), Setting::Invalid);
        for s in [Setting::Auto, Setting::Invalid, Setting::Fixed(Stereo::Mono), Setting::Fixed(Stereo::Htab)] {
            assert_eq!(Setting::from_code(s.code()), s);
        }
        // A live mod wins; else the setting; else mono.
        assert_eq!(resolve(Some(Stereo::Sbs), Setting::Fixed(Stereo::Hsbs)), (Stereo::Sbs, Source::Mod));
        assert_eq!(resolve(Some(Stereo::Mono), Setting::Fixed(Stereo::Hsbs)), (Stereo::Mono, Source::Mod));
        assert_eq!(resolve(None, Setting::Fixed(Stereo::Hsbs)), (Stereo::Hsbs, Source::Setting));
        assert_eq!(resolve(None, Setting::Auto), (Stereo::Mono, Source::Default));
        assert_eq!(resolve(None, Setting::Invalid), (Stereo::Mono, Source::Default));
    }
}
