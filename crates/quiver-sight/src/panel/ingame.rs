//! The panel drawn in the game (design/SIGHT.md §14, "In the game"): the same [`PanelView`]
//! in an off-screen GPUI window at the game's swapchain size, published into the session's
//! `panel.image` for the Sight layer to composite, and driven by the game's own keyboard and
//! mouse, which the input preload (`libquiver_sight_input.so`) keeps from the game and
//! forwards through `panel.input` while the panel is open. The panel never takes keyboard
//! focus, so the game's window stays active (exclusive-fullscreen games don't minimise).
//!
//! - **When**: the session's layer says it can draw the panel on the game's swapchain and the
//!   preload is in the game ([`available`]); otherwise the layer-shell panel opens as before.
//! - **Rendering**: the view at `extent / factor` logical px, `factor` device px per logical
//!   px (the game's monitor's logical size mapped onto the swapchain, so it looks as the
//!   desktop panel would), rendered when it changes (at most 60 times a second); each frame
//!   goes into `panel.image` with the rows that changed since the last one, so the layer
//!   copies only those.
//! - **Input**: a thread drains the event ring every 4 ms and hands batches to the main
//!   thread ([`Msg::GameInput`]); keys become GPUI key events on the view (Shift+F2 excepted:
//!   the layer's own key reader toggles the panel), the pointer a cursor the layer draws
//!   (absolute X11 positions as they are, Wayland and relative motion as deltas), clicks and
//!   wheels GPUI mouse events at the cursor.
//! - **Stereo** (design/SIGHT.md §15): on a packed frame the view is laid out for one eye's
//!   logical size (`sbs`: half the width; `hsbs`: the whole width, squeezed by the layer), the
//!   image says so, and the layer draws it in each eye. X11 pointer positions map into the eye
//!   they're over (`x mod` the eye's width, ×2 within the half for `hsbs`; the same vertically
//!   for `tab`/`htab`), so a click lands where the panel is in that eye; the cursor is in the
//!   image's (logical) pixels and drawn in both eyes.
//! - **Keeping input**: [`InGame::renew`] every tick (250 ms) renews the capture deadline and
//!   the image's heartbeat; a quiver-sight that hangs or dies gives the game its input back
//!   within a second (quiver-perf `panel::capture`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, FocusHandle, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    OffscreenWindowOptions, Pixels, PlatformInput, ScrollDelta, ScrollEvent, TouchPhase, point, px, size,
};
use quiver_perf::HudBufferHeader;
use quiver_perf::layout::{HUD_MAX_LIFTS, HudLift};
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::{HudWriter, PANEL_LIMITS, buffer_bytes_within};
use quiver_perf::layout::HUD_FORMAT_RGBA8_PREMUL_SRGB;
use quiver_perf::panel::{
    EV_BUTTON, EV_KEY, EV_MOTION, EV_POINTER, EV_RESET, EV_SCROLL, EVF_REPEAT, EVF_WAYLAND, EventReader, PANEL_HOLD_NS, PANEL_IMAGE_FILE, PanelEvent, PanelInput,
    create_panel_image,
};
use quiver_perf::shm::Mapping;
use quiver_perf::stereo::Stereo;

use super::view::PanelView;
use crate::link::Msg;

/// The parts of a view at their own depth in its last frame, back to front: window logical
/// px and rung (thousandths, `quiver_perf::stereo::rung`).
pub type DepthParts = Rc<dyn Fn() -> Vec<(Bounds<Pixels>, u16)>>;

/// What the in-game presenter needs from a view: the panel, the quick menu, the overlay with
/// the toasts (design/SIGHT.md §14, §16).
pub trait InGameView: gpui::Render + Sized + 'static {
    /// Where its parts at their own depth are (read in the frame callback).
    fn depth_parts(&self) -> DepthParts;
    /// What takes the keyboard in its window (`None`: nothing, it takes no input).
    fn focus_handle(&self) -> Option<FocusHandle>;
}

impl InGameView for PanelView {
    fn depth_parts(&self) -> DepthParts {
        let m = self.marks.clone();
        Rc::new(move || m.borrow().parts().collect())
    }
    fn focus_handle(&self) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

/// The layer must have composited the panel within this of opening, or the desktop panel
/// takes over (the game isn't presenting, or the layer can't after all).
pub const CONFIRM_WITHIN: Duration = Duration::from_secs(2);

/// Whether the panel can be drawn in this session's game now: the layer can draw it on the
/// game's swapchain, and the preload has hooked the game's input. `Err`: why not.
pub fn available(p: &PanelInput<Mapping>, now: i64) -> Result<(u32, u32), String> {
    if std::env::var("QUIVER_SIGHT_PANEL").is_ok_and(|v| v == "desktop") {
        return Err("QUIVER_SIGHT_PANEL=desktop".into());
    }
    let layer = p.layer();
    if !layer.can_draw(now) {
        return Err(if layer.seen_ns == 0 {
            "the game has no Sight layer drawing (not loaded, turned off, or not Vulkan)".into()
        } else if layer.extent.0 == 0 {
            "the game has no swapchain".into()
        } else {
            "the Sight layer can't draw on this swapchain (format, or drawing off)".into()
        });
    }
    if !p.input().ready(now) {
        return Err("the input preload isn't in the game (launched without it, or an older launch)".into());
    }
    Ok(layer.extent)
}

/// Device px per logical px: the game's monitor's logical height onto the swapchain's, so
/// the panel looks as it would on the desktop; without the monitor, 1080 logical px tall.
pub fn factor(extent: (u32, u32), monitor_logical: Option<(f32, f32)>) -> f32 {
    let h = extent.1 as f32;
    let f = match monitor_logical {
        Some((_, mh)) if mh > 0.0 => h / mh,
        _ => (h / 1080.0).max(1.0),
    };
    if f.is_finite() { f.clamp(0.5, 4.0) } else { 1.0 }
}

/// The frame publisher (the off-screen window's frame callback).
struct Publish {
    writer: HudWriter<Mapping>,
    /// The image's size (one eye's logical size for a stereo frame).
    extent: (u32, u32),
    /// The swapchain's, and how it packs its eyes.
    swapchain: (u32, u32),
    stereo: Stereo,
    /// Where the view's parts at their own depth are ([`DepthParts`]), and the lifts last
    /// published from them.
    marks: DepthParts,
    prev_lifts: Vec<HudLift>,
    prev: Vec<u8>,
    prev_dims: (u32, u32, u32),
    last: u64,
    frames: u64,
    rows: u64,
}

impl Publish {
    fn frame(&mut self, fr: &gpui::OffscreenFrame<'_>, scale: f32) {
        let (w, h) = (fr.width.min(self.extent.0), fr.height.min(self.extent.1));
        if w == 0 || h == 0 {
            return;
        }
        let len = (fr.stride as usize * (h as usize - 1) + w as usize * 4).min(fr.data.len());
        let data = &fr.data[..len];
        let dims = (w, h, fr.stride);
        let lifts = if self.stereo.is_stereo() { lifts(&(self.marks)(), scale, (w, h)) } else { Vec::new() };
        let same_lifts = lifts == self.prev_lifts;
        // The rows that differ from the last image (whole rows of `w` pixels).
        let dirty = if self.last != 0 && self.prev_dims == dims && self.prev.len() == data.len() {
            let row = |y: u32| {
                let o = y as usize * fr.stride as usize;
                o..o + w as usize * 4
            };
            let first = (0..h).find(|&y| data[row(y)] != self.prev[row(y)]);
            match first {
                None if same_lifts => return, // nothing changed
                None => None,                 // only the depths: the whole image again
                Some(y0) => {
                    let y1 = (y0..h).rev().find(|&y| data[row(y)] != self.prev[row(y)]).unwrap_or(y0) + 1;
                    Some((y0, y1))
                }
            }
        } else {
            None
        };
        let mut d = HudBufferHeader::ZERO;
        (d.width, d.height, d.stride, d.format, d.scale) = (w, h, fr.stride, HUD_FORMAT_RGBA8_PREMUL_SRGB, scale);
        (d.extent_w, d.extent_h, d.opacity, d.stereo) = (self.swapchain.0, self.swapchain.1, 1.0, self.stereo.code());
        // The scrim and the blur stay at the game's plane (depth 0); the card, the focused row
        // and a dialog float at their rungs (design/STEREO-UI.md).
        d.nlifts = lifts.len() as u16;
        d.lifts[..lifts.len()].copy_from_slice(&lifts);
        if let Some((y0, y1)) = dirty {
            (d.dirty_y0, d.dirty_y1, d.dirty_base) = (y0, y1, self.last);
        }
        // `publish` wants stride × height bytes; the frame's last row may be short.
        let need = fr.stride as usize * h as usize;
        let res = if data.len() >= need {
            self.writer.publish(&d, &data[..need])
        } else {
            let mut full = data.to_vec();
            full.resize(need, 0);
            self.writer.publish(&d, &full)
        };
        match res {
            Ok(n) => {
                self.last = n;
                self.frames += 1;
                self.rows += dirty.map_or(h, |(a, b)| b - a) as u64;
                self.prev.clear();
                self.prev.extend_from_slice(data);
                self.prev_dims = dims;
                self.prev_lifts = lifts;
            }
            Err(e) => tracing::warn!("panel.image: {e}"),
        }
    }
}

/// The image's lifts from the view's marks: logical px × `scale` image px, rounded out and
/// kept inside the image (`size`), back to front, at most [`HUD_MAX_LIFTS`].
pub fn lifts(parts: &[(Bounds<Pixels>, u16)], scale: f32, size: (u32, u32)) -> Vec<HudLift> {
    parts
        .iter()
        .copied()
        .filter_map(|(b, depth)| {
            let x0 = (f32::from(b.origin.x) * scale).floor().max(0.0) as u32;
            let y0 = (f32::from(b.origin.y) * scale).floor().max(0.0) as u32;
            let x1 = ((f32::from(b.origin.x + b.size.width) * scale).ceil().max(0.0) as u32).min(size.0).min(u16::MAX as u32);
            let y1 = ((f32::from(b.origin.y + b.size.height) * scale).ceil().max(0.0) as u32).min(size.1).min(u16::MAX as u32);
            (x1 > x0 && y1 > y0).then(|| HudLift { x: x0 as u16, y: y0 as u16, w: (x1 - x0) as u16, h: (y1 - y0) as u16, depth, _pad: 0 })
        })
        .take(HUD_MAX_LIFTS)
        .collect()
}

/// Held keys and buttons, and the modifiers they make.
#[derive(Default)]
struct Keys {
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    meta: [bool; 2],
    buttons: u8,
}

impl Keys {
    fn modifiers(&self) -> Modifiers {
        Modifiers {
            control: self.ctrl.iter().any(|&b| b),
            alt: self.alt.iter().any(|&b| b),
            shift: self.shift.iter().any(|&b| b),
            platform: self.meta.iter().any(|&b| b),
            function: false,
        }
    }

    /// Notes a modifier key; true if `code` is one.
    fn modifier(&mut self, code: u32, down: bool) -> bool {
        let slot = match code {
            42 => &mut self.shift[0],
            54 => &mut self.shift[1],
            29 => &mut self.ctrl[0],
            97 => &mut self.ctrl[1],
            56 => &mut self.alt[0],
            100 => &mut self.alt[1],
            125 => &mut self.meta[0],
            126 => &mut self.meta[1],
            _ => return false,
        };
        *slot = down;
        true
    }
}

/// GPUI's name for a Linux evdev key code (US layout), and the character it types.
pub fn key_name(code: u32, shift: bool) -> Option<(&'static str, Option<char>)> {
    const ROW: [(u32, &str); 47] = [
        (2, "1"), (3, "2"), (4, "3"), (5, "4"), (6, "5"), (7, "6"), (8, "7"), (9, "8"), (10, "9"), (11, "0"), (12, "-"), (13, "="),
        (16, "q"), (17, "w"), (18, "e"), (19, "r"), (20, "t"), (21, "y"), (22, "u"), (23, "i"), (24, "o"), (25, "p"), (26, "["), (27, "]"),
        (30, "a"), (31, "s"), (32, "d"), (33, "f"), (34, "g"), (35, "h"), (36, "j"), (37, "k"), (38, "l"), (39, ";"), (40, "'"), (41, "`"),
        (43, "\\"), (44, "z"), (45, "x"), (46, "c"), (47, "v"), (48, "b"), (49, "n"), (50, "m"), (51, ","), (52, "."), (53, "/"),
    ];
    if let Some((_, k)) = ROW.iter().find(|(c, _)| *c == code) {
        let ch = k.chars().next().map(|c| if shift { shifted(c) } else { c });
        return Some((k, ch));
    }
    let k = match code {
        1 => "escape",
        14 => "backspace",
        15 => "tab",
        28 | 96 => "enter",
        57 => return Some(("space", Some(' '))),
        59 => "f1",
        60 => "f2",
        61 => "f3",
        62 => "f4",
        63 => "f5",
        64 => "f6",
        65 => "f7",
        66 => "f8",
        67 => "f9",
        68 => "f10",
        87 => "f11",
        88 => "f12",
        102 => "home",
        103 => "up",
        104 => "pageup",
        105 => "left",
        106 => "right",
        107 => "end",
        108 => "down",
        109 => "pagedown",
        110 => "insert",
        111 => "delete",
        _ => return None,
    };
    Some((k, None))
}

fn shifted(c: char) -> char {
    match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        ';' => ':',
        '\'' => '"',
        '`' => '~',
        '\\' => '|',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        c => c,
    }
}

fn mouse_button(code: u32) -> Option<MouseButton> {
    Some(match code {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        _ => return None,
    })
}

/// The cursor and key state, separate from GPUI so the translation is testable.
pub struct Translate {
    /// In the image's pixels (one eye's logical pixels on a stereo frame).
    pub cursor: (f64, f64),
    pub extent: (u32, u32),
    /// The swapchain (= the game's window, which X11 positions are in) and its packing.
    pub swapchain: (u32, u32),
    pub stereo: Stereo,
    pub factor: f32,
    /// The cursor is drawn (the mouse was used since the last key).
    pub cursor_shown: bool,
    keys: Keys,
    wl_last: Option<(f64, f64)>,
    clicks: (Option<(Instant, MouseButton)>, usize),
}

impl Translate {
    pub fn new(extent: (u32, u32), factor: f32) -> Translate {
        Self::stereo(extent, Stereo::Mono, factor)
    }

    /// For a swapchain of `swapchain` packed as `stereo`: the image is one eye's logical size.
    pub fn stereo(swapchain: (u32, u32), stereo: Stereo, factor: f32) -> Translate {
        let extent = stereo.logical_extent(swapchain);
        Translate { cursor: (extent.0 as f64 / 2.0, extent.1 as f64 / 2.0), extent, swapchain, stereo, factor, cursor_shown: false, keys: Keys::default(), wl_last: None, clicks: (None, 0) }
    }

    fn position(&self) -> gpui::Point<gpui::Pixels> {
        let f = self.factor as f64;
        point(px((self.cursor.0 / f) as f32), px((self.cursor.1 / f) as f32))
    }

    fn move_to(&mut self, x: f64, y: f64) -> PlatformInput {
        self.cursor = (x.clamp(0.0, self.extent.0.saturating_sub(1) as f64), y.clamp(0.0, self.extent.1.saturating_sub(1) as f64));
        self.cursor_shown = true;
        let pressed = if self.keys.buttons & 1 != 0 { Some(MouseButton::Left) } else { None };
        PlatformInput::MouseMove(MouseMoveEvent { position: self.position(), pressed_button: pressed, modifiers: self.keys.modifiers() })
    }

    /// The GPUI input for one forwarded event, if any.
    pub fn event(&mut self, e: &PanelEvent) -> Option<PlatformInput> {
        match e.kind {
            EV_KEY => {
                let down = e.down();
                if self.keys.modifier(e.code, down) {
                    return Some(PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent { modifiers: self.keys.modifiers(), capslock: Default::default() }));
                }
                let m = self.keys.modifiers();
                let (key, ch) = key_name(e.code, m.shift)?;
                // Shift+F2 is the layer's: its own key reader toggles the panel through
                // quiverd (handling it here too would close and reopen it).
                if key == "f2" && m.shift {
                    return None;
                }
                if down {
                    self.cursor_shown = false;
                }
                let key_char = ch.filter(|_| !m.control && !m.alt && !m.platform).map(String::from);
                let keystroke = Keystroke { modifiers: m, key: key.into(), key_char };
                Some(if down {
                    PlatformInput::KeyDown(KeyDownEvent { keystroke, is_held: e.flags & EVF_REPEAT != 0, prefer_character_input: false })
                } else {
                    PlatformInput::KeyUp(KeyUpEvent { keystroke })
                })
            }
            EV_BUTTON => {
                let b = mouse_button(e.code)?;
                let bit = match b {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    _ => 4,
                };
                self.cursor_shown = true;
                let position = self.position();
                let modifiers = self.keys.modifiers();
                if e.down() {
                    self.keys.buttons |= bit;
                    let now = Instant::now();
                    let n = match self.clicks.0 {
                        Some((t, prev)) if prev == b && now.duration_since(t) < Duration::from_millis(400) => self.clicks.1 + 1,
                        _ => 1,
                    };
                    self.clicks = (Some((now, b)), n);
                    Some(PlatformInput::MouseDown(MouseDownEvent { button: b, position, modifiers, click_count: n, first_mouse: false }))
                } else {
                    self.keys.buttons &= !bit;
                    Some(PlatformInput::MouseUp(MouseUpEvent { button: b, position, modifiers, click_count: self.clicks.1.max(1) }))
                }
            }
            EV_MOTION => {
                let (dx, dy) = e.xy();
                Some(self.move_to(self.cursor.0 + dx, self.cursor.1 + dy))
            }
            EV_POINTER if e.flags & EVF_WAYLAND != 0 => {
                // Surface coordinates (logical, scale unknown here): deltas only.
                let p = e.xy();
                let last = self.wl_last.replace(p);
                let (dx, dy) = last.map_or((0.0, 0.0), |l| (p.0 - l.0, p.1 - l.1));
                Some(self.move_to(self.cursor.0 + dx, self.cursor.1 + dy))
            }
            // X11: the pointer's position in the game's window, which is the swapchain's size;
            // on a stereo frame, in the eye it's over.
            EV_POINTER => {
                let (_, (x, y)) = self.stereo.to_logical(self.swapchain, (e.a as f64, e.b as f64));
                Some(self.move_to(x, y))
            }
            EV_SCROLL => {
                let (dx, dy) = e.xy();
                self.cursor_shown = true;
                Some(PlatformInput::Scroll(ScrollEvent {
                    position: self.position(),
                    delta: ScrollDelta::Lines(point(-(dx as f32) * 3.0, -(dy as f32) * 3.0)),
                    modifiers: self.keys.modifiers(),
                    touch_phase: TouchPhase::Moved,
                    source: Default::default(),
                }))
            }
            EV_RESET => {
                self.keys = Keys::default();
                Some(PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent { modifiers: Modifiers::default(), capslock: Default::default() }))
            }
            _ => None,
        }
    }
}

/// A view drawn in the game: the panel (input captured), or an image shown over the game
/// that takes none (`capture` false: toasts, the status chip, the first-launch hint).
pub struct InGame {
    pub session: String,
    input: Arc<PanelInput<Mapping>>,
    window: AnyWindowHandle,
    /// Input is the view's while it's up (else the game keeps it).
    pub capture: bool,
    publish: Rc<RefCell<Publish>>,
    /// The swapchain's extent (the image is `tr.extent`).
    pub extent: (u32, u32),
    pub stereo: Stereo,
    pub factor: f32,
    pub epoch: u64,
    stop: Arc<AtomicBool>,
    pub opened: Instant,
    /// The layer has composited it.
    pub confirmed: bool,
    tr: Translate,
}

impl InGame {
    /// Opens it: `panel.image` at the swapchain's size, the off-screen window around `view`,
    /// the panel open in `panel.input`, and the thread that drains the input ring.
    #[allow(clippy::too_many_arguments)]
    pub fn open<V: InGameView>(session: &str, input: Arc<PanelInput<Mapping>>, dir: &Path, swapchain: (u32, u32), stereo: Stereo, factor: f32, view: Entity<V>, ui: UnboundedSender<Msg>, cx: &mut App) -> Result<InGame> {
        Self::open_as(session, input, dir, swapchain, stereo, factor, view, Some(ui), cx)
    }

    /// Shows `view` over the game without taking its input ([`PanelInput::show`]).
    #[allow(clippy::too_many_arguments)]
    pub fn show<V: InGameView>(session: &str, input: Arc<PanelInput<Mapping>>, dir: &Path, swapchain: (u32, u32), stereo: Stereo, factor: f32, view: Entity<V>, cx: &mut App) -> Result<InGame> {
        Self::open_as(session, input, dir, swapchain, stereo, factor, view, None, cx)
    }

    /// `ui`: capture input and send it there; `None`: show only.
    #[allow(clippy::too_many_arguments)]
    fn open_as<V: InGameView>(session: &str, input: Arc<PanelInput<Mapping>>, dir: &Path, swapchain: (u32, u32), stereo: Stereo, factor: f32, view: Entity<V>, ui: Option<UnboundedSender<Msg>>, cx: &mut App) -> Result<InGame> {
        let extent = stereo.logical_extent(swapchain);
        let bb = buffer_bytes_within(extent.0, extent.1, &PANEL_LIMITS).with_context(|| format!("a {}x{} panel is larger than panel.image allows", extent.0, extent.1))?;
        let writer = create_panel_image(&dir.join(PANEL_IMAGE_FILE), bb).context("creating panel.image")?;
        writer.set_writer_pid(std::process::id());
        writer.heartbeat(mono_raw_ns());
        let marks = view.read(cx).depth_parts();
        let publish = Rc::new(RefCell::new(Publish { writer, extent, swapchain, stereo, marks, prev_lifts: Vec::new(), prev: Vec::new(), prev_dims: (0, 0, 0), last: 0, frames: 0, rows: 0 }));
        let sink = publish.clone();
        let logical = (extent.0 as f32 / factor, extent.1 as f32 / factor);
        let root = view.clone();
        let window = cx.open_offscreen_window(
            OffscreenWindowOptions {
                size: size(px(logical.0), px(logical.1)),
                scale_factor: factor,
                prefer_low_power_gpu: true,
                min_frame_interval: Some(Duration::from_millis(16)),
                ..Default::default()
            },
            move |fr| sink.borrow_mut().frame(fr, factor),
            move |window, cx| {
                if let Some(focus) = root.read(cx).focus_handle() {
                    window.focus(&focus, cx);
                }
                root
            },
        )?;
        let window: AnyWindowHandle = window.into();
        // The first frame now, so the layer has something to draw as the panel opens.
        // Not `window.update`: that holds the view while rendering it.
        let _ = cx.update_window(window, |_, w, cx| {
            w.refresh();
            w.render_now(cx);
        });
        let epoch = (mono_raw_ns() as u64 / 1_000_000).max(input.control().epoch() + 1);
        let now = mono_raw_ns();
        input.set_sight_pid(std::process::id());
        let tr = Translate::stereo(swapchain, stereo, factor);
        input.set_cursor(tr.cursor.0 as i32, tr.cursor.1 as i32, extent);
        let stop = Arc::new(AtomicBool::new(false));
        let capture = ui.is_some();
        match ui {
            Some(ui) => {
                let mut reader = EventReader::at_end(&input);
                input.open(epoch, now, now + PANEL_HOLD_NS as i64, false);
                let (inp, st, s) = (input.clone(), stop.clone(), session.to_owned());
                std::thread::Builder::new().name("quiver-sight-input".into()).spawn(move || {
                    while !st.load(Ordering::Relaxed) {
                        let mut batch = Vec::new();
                        reader.poll(&inp, |e| batch.push(e));
                        if !batch.is_empty() && ui.unbounded_send(Msg::GameInput { session: s.clone(), events: batch }).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(4));
                    }
                })?;
            }
            None => input.show(epoch),
        }
        tracing::info!(session, swapchain = ?swapchain, stereo = stereo.as_str(), extent = ?extent, factor, epoch, capture, "drawn in the game");
        Ok(InGame { session: session.to_owned(), input, window, capture, publish, extent: swapchain, stereo, factor, epoch, stop, opened: Instant::now(), confirmed: false, tr })
    }

    /// The session's `panel.input` (to open it again).
    pub fn input_handle(&self) -> Arc<PanelInput<Mapping>> {
        self.input.clone()
    }

    /// Renews the capture deadline and the image's heartbeat (every tick); returns whether the
    /// layer has drawn it (once it has, this stays true).
    pub fn renew(&mut self) -> bool {
        let now = mono_raw_ns();
        if self.capture {
            self.input.renew(now + PANEL_HOLD_NS as i64);
        }
        self.publish.borrow().writer.heartbeat(now);
        if !self.confirmed && self.input.layer().drawn_epoch == self.epoch {
            self.confirmed = true;
            let p = self.publish.borrow();
            tracing::info!(session = %self.session, after = ?self.opened.elapsed(), frames = p.frames, "the Sight layer draws the panel in the game");
        }
        self.confirmed
    }

    /// Forwarded input: GPUI events on the view, the cursor for the layer.
    pub fn input(&mut self, events: &[PanelEvent], cx: &mut App) {
        let mut out = Vec::new();
        for e in events {
            if let Some(i) = self.tr.event(e) {
                out.push(i);
            }
        }
        if !out.is_empty() {
            let _ = cx.update_window(self.window, |_, w, cx| {
                for i in out {
                    w.dispatch_event(i, cx);
                }
            });
        }
        self.input.set_cursor(self.tr.cursor.0.round() as i32, self.tr.cursor.1.round() as i32, self.tr.extent);
        self.input.set_cursor_shown(self.tr.cursor_shown);
    }

    /// A line for the log and `quiver sight status`.
    pub fn describe(&self) -> String {
        let p = self.publish.borrow();
        let i = self.input.input();
        let eyes = if self.stereo.is_stereo() { format!(", {} {}x{} per eye", self.stereo.as_str(), self.tr.extent.0, self.tr.extent.1) } else { String::new() };
        format!(
            "in the game ({}x{}{eyes} at {:.2}×): {} images ({} rows), input {} forwarded, {} lost, {} held keys released to the game",
            self.extent.0, self.extent.1, self.factor, p.frames, p.rows, i.forwarded, i.lost, i.releases
        )
    }

    /// The image was replaced by another view's drawn in the game since (its epoch moved on).
    pub fn superseded(&self) -> bool {
        self.input.control().epoch() != self.epoch
    }

    /// Closes it: input goes back to the game at once, the window goes.
    pub fn close(self, cx: &mut App) {
        // Only its own state: another view may have taken the image over since.
        if !self.superseded() {
            self.input.close();
        }
        self.stop.store(true, Ordering::Relaxed);
        let _ = cx.update_window(self.window, |_, w, _| w.remove_window());
        tracing::info!(session = %self.session, "panel closed in the game: {}", self.describe());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_perf::panel::EV_POINTER;

    fn key(code: u32, down: bool) -> PanelEvent {
        PanelEvent::key(0, code, down, 0, 0)
    }

    #[test]
    fn keys_become_keystrokes() {
        let mut t = Translate::new((2560, 1600), 1.25);
        let Some(PlatformInput::KeyDown(k)) = t.event(&key(103, true)) else { panic!() };
        assert_eq!(k.keystroke.key, "up");
        assert!(matches!(t.event(&key(42, true)), Some(PlatformInput::ModifiersChanged(m)) if m.modifiers.shift));
        let Some(PlatformInput::KeyDown(k)) = t.event(&key(30, true)) else { panic!() };
        assert_eq!((k.keystroke.key.as_str(), k.keystroke.key_char.as_deref(), k.keystroke.modifiers.shift), ("a", Some("A"), true));
        // Shift+F2 is the layer's.
        assert!(t.event(&key(60, true)).is_none());
        assert!(matches!(t.event(&key(42, false)), Some(PlatformInput::ModifiersChanged(m)) if !m.modifiers.shift));
        let Some(PlatformInput::KeyDown(k)) = t.event(&key(60, true)) else { panic!() };
        assert_eq!(k.keystroke.key, "f2", "F2 alone isn't the chord");
        let Some(PlatformInput::KeyUp(k)) = t.event(&key(1, false)) else { panic!() };
        assert_eq!(k.keystroke.key, "escape");
        assert!(t.event(&key(0x2fe, true)).is_none(), "unknown keys are dropped");
        let mut rep = key(108, true);
        rep.flags |= EVF_REPEAT;
        assert!(matches!(t.event(&rep), Some(PlatformInput::KeyDown(k)) if k.is_held));
    }

    #[test]
    fn pointer_and_buttons() {
        let mut t = Translate::new((2000, 1000), 2.0);
        assert!(!t.cursor_shown);
        // X11: the window position, in swapchain px; GPUI gets logical px.
        let Some(PlatformInput::MouseMove(m)) = t.event(&PanelEvent { kind: EV_POINTER, a: 400, b: 300, ..Default::default() }) else { panic!() };
        assert_eq!((f32::from(m.position.x), f32::from(m.position.y)), (200.0, 150.0));
        assert!(t.cursor_shown);
        // Relative motion, clamped to the swapchain.
        t.event(&PanelEvent::motion(0, -1000.0, 10.0));
        assert_eq!(t.cursor, (0.0, 310.0));
        // Wayland positions: deltas only (the first sets the reference).
        let wl = |x: i32, y: i32| PanelEvent { kind: EV_POINTER, flags: EVF_WAYLAND, a: x * 256, b: y * 256, ..Default::default() };
        t.event(&wl(50, 50));
        assert_eq!(t.cursor, (0.0, 310.0));
        t.event(&wl(60, 45));
        assert_eq!(t.cursor, (10.0, 305.0));
        let Some(PlatformInput::MouseDown(d)) = t.event(&PanelEvent::button(0, 0x110, true)) else { panic!() };
        assert_eq!((d.button, d.click_count, f32::from(d.position.x)), (MouseButton::Left, 1, 5.0));
        assert!(matches!(t.event(&PanelEvent::button(0, 0x110, false)), Some(PlatformInput::MouseUp(_))));
        assert!(matches!(t.event(&PanelEvent::button(0, 0x110, true)), Some(PlatformInput::MouseDown(d)) if d.click_count == 2));
        let Some(PlatformInput::Scroll(s)) = t.event(&PanelEvent::scroll(0, 0.0, 1.0)) else { panic!() };
        assert!(matches!(s.delta, ScrollDelta::Lines(p) if p.y == -3.0 && p.x == 0.0));
        // A key hides the cursor until the mouse moves again.
        t.event(&key(103, true));
        assert!(!t.cursor_shown);
    }

    #[test]
    fn stereo_pointer() {
        let x11 = |x: i32, y: i32| PanelEvent { kind: EV_POINTER, a: x, b: y, ..Default::default() };
        // sbs, a 3840x1080 window: the panel is 1920x1080 and a click in either half lands at
        // the same place in it.
        let mut t = Translate::stereo((3840, 1080), Stereo::Sbs, 1.0);
        assert_eq!(t.extent, (1920, 1080));
        t.event(&x11(400, 300));
        assert_eq!(t.cursor, (400.0, 300.0));
        t.event(&x11(1920 + 400, 300));
        assert_eq!(t.cursor, (400.0, 300.0));
        // hsbs, a 1920x1080 window: each half is 960 wide, ×2 into the 1920 layout.
        let mut t = Translate::stereo((1920, 1080), Stereo::Hsbs, 1.0);
        assert_eq!(t.extent, (1920, 1080));
        let Some(PlatformInput::MouseMove(m)) = t.event(&x11(200, 100)) else { panic!() };
        assert_eq!((f32::from(m.position.x), f32::from(m.position.y)), (400.0, 100.0));
        t.event(&x11(960 + 200, 100));
        assert_eq!(t.cursor, (400.0, 100.0));
        // tab / htab: the same vertically.
        let mut t = Translate::stereo((1920, 2160), Stereo::Tab, 1.0);
        t.event(&x11(50, 1080 + 70));
        assert_eq!((t.extent, t.cursor), ((1920, 1080), (50.0, 70.0)));
        let mut t = Translate::stereo((1920, 1080), Stereo::Htab, 2.0);
        let Some(PlatformInput::MouseMove(m)) = t.event(&x11(50, 540 + 70)) else { panic!() };
        assert_eq!((t.cursor, f32::from(m.position.y)), ((50.0, 140.0), 70.0));
        // Relative motion stays 1:1 in the layout's pixels, clamped to it.
        let mut t = Translate::stereo((3840, 1080), Stereo::Sbs, 1.0);
        t.event(&PanelEvent::motion(0, 5000.0, 0.0));
        assert_eq!(t.cursor.0, 1919.0);
    }

    #[test]
    fn factors() {
        // 2560x1600 on a 2048x1280 logical monitor: 1.25, as on the desktop.
        assert!((factor((2560, 1600), Some((2048.0, 1280.0))) - 1.25).abs() < 1e-6);
        // The game renders 1080p on it: the panel keeps its apparent size.
        assert!((factor((1920, 1080), Some((2048.0, 1280.0))) - 0.84375).abs() < 1e-6);
        assert_eq!(factor((3840, 2160), None), 2.0);
        assert_eq!(factor((1280, 720), None), 1.0);
        assert_eq!(factor((100, 100), Some((0.0, 0.0))), 1.0);
    }
}
