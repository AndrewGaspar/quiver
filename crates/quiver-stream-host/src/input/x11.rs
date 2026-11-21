//! Keyboard and mouse into the stream's gamescope, through XTEST on its Xwayland.
//!
//! **Only that display.** The injector connects to the X display gamescope reported for the
//! stream, refuses :0–:9, and before sending anything checks that the server's root window
//! carries `GAMESCOPE_PID` naming the stream's own gamescope. The person's desktop
//! (Hyprland's Xwayland on :0, Hyprland itself, Steam) is never connected to, so stream
//! input can't reach it.
//!
//! - **Keys** by evdev code: X keycode = evdev + 8 (Xwayland's evdev keymap).
//! - **Relative motion** with XTEST's relative motion, which the server also reports as XI2
//!   raw motion (what Wine's raw input and games' mouse-look read).
//! - **Absolute position** as a fraction of the screen.
//! - **Buttons** 1/2/3 and back/forward as 8/9.
//! - **The wheel**: X has no high-resolution wheel through XTEST, so 120ths accumulate and
//!   each whole notch is a click of buttons 4–7. A remainder carries to the next event.
//!
//! Everything pressed is remembered, so `ReleaseAll` (and dropping the injector) lets go of
//! all of it.
//!
//! The same connection watches the cursor (XFIXES): each change of shape becomes a
//! [`Control::Cursor`], with `None` when the game hides it.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use quiver_stream_proto::{Control, CursorImage, InputEvent, MouseButton};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::display::reserved_display;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION: u8 = 6;

/// The X button for a mouse button.
pub fn x_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

/// Whole notches from a wheel accumulator, in 120ths. Returns the clicks (negative is down
/// or left) and leaves the remainder.
pub fn wheel_notches(acc: &mut i32, delta: i32) -> i32 {
    *acc += delta;
    let n = *acc / 120;
    *acc -= n * 120;
    n
}

/// What one input event becomes, as XTEST requests: (type, detail, x, y). Pure, so it can be
/// tested without a server. `size` is the screen, for absolute positions.
pub fn requests(ev: &InputEvent, size: (u16, u16), wheel: &mut (i32, i32), held_keys: &mut BTreeSet<u8>, held_buttons: &mut BTreeSet<u8>) -> Vec<(u8, u8, i16, i16)> {
    let mut out = Vec::new();
    match ev {
        InputEvent::Key { code, down } => {
            let kc = code.saturating_add(8);
            if kc > 255 {
                return out;
            }
            let kc = kc as u8;
            if *down {
                held_keys.insert(kc);
            } else {
                held_keys.remove(&kc);
            }
            out.push((if *down { KEY_PRESS } else { KEY_RELEASE }, kc, 0, 0));
        }
        InputEvent::MouseMotion { dx, dy } => {
            // XTEST motion deltas are 16-bit; split anything bigger.
            let (mut x, mut y) = (*dx, *dy);
            while x != 0 || y != 0 {
                let sx = x.clamp(-32767, 32767);
                let sy = y.clamp(-32767, 32767);
                out.push((MOTION, 1, sx as i16, sy as i16));
                x -= sx;
                y -= sy;
            }
        }
        InputEvent::MousePosition { x, y } => {
            let px = (*x as u32 * size.0.saturating_sub(1) as u32 / 65535) as i16;
            let py = (*y as u32 * size.1.saturating_sub(1) as u32 / 65535) as i16;
            out.push((MOTION, 0, px, py));
        }
        InputEvent::MouseButton { button, down } => {
            let b = x_button(*button);
            if *down {
                held_buttons.insert(b);
            } else {
                held_buttons.remove(&b);
            }
            out.push((if *down { BUTTON_PRESS } else { BUTTON_RELEASE }, b, 0, 0));
        }
        InputEvent::Wheel { dx, dy } => {
            // Positive dy is up (away from the person), as SDL and Wayland's v120 report it.
            let v = wheel_notches(&mut wheel.1, *dy);
            let h = wheel_notches(&mut wheel.0, *dx);
            for (n, pos, neg) in [(v, 4u8, 5u8), (h, 7, 6)] {
                let b = if n > 0 { pos } else { neg };
                for _ in 0..n.unsigned_abs() {
                    out.push((BUTTON_PRESS, b, 0, 0));
                    out.push((BUTTON_RELEASE, b, 0, 0));
                }
            }
        }
        InputEvent::ReleaseAll => {
            for k in std::mem::take(held_keys) {
                out.push((KEY_RELEASE, k, 0, 0));
            }
            for b in std::mem::take(held_buttons) {
                out.push((BUTTON_RELEASE, b, 0, 0));
            }
        }
        _ => {}
    }
    out
}

/// XTEST into one gamescope's Xwayland.
pub struct XInjector {
    conn: RustConnection,
    root: Window,
    size: (u16, u16),
    wheel: (i32, i32),
    held_keys: BTreeSet<u8>,
    held_buttons: BTreeSet<u8>,
    cursor_serial: u32,
}

impl XInjector {
    /// Connect to `display` and check it is gamescope `gamescope_pid`'s.
    pub fn connect(display: &str, gamescope_pid: u32) -> Result<XInjector> {
        if reserved_display(display) {
            bail!("refusing to send input to X display {display}");
        }
        let (conn, screen) = x11rb::connect(Some(display)).with_context(|| format!("connect to X display {display}"))?;
        let s = &conn.setup().roots[screen];
        let (root, size) = (s.root, (s.width_in_pixels, s.height_in_pixels));
        let atom = conn.intern_atom(true, b"GAMESCOPE_PID")?.reply()?.atom;
        let pid = if atom == 0 {
            None
        } else {
            let r = conn.get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)?.reply()?;
            r.value32().and_then(|mut v| v.next())
        };
        if pid != Some(gamescope_pid) {
            bail!("X display {display} isn't this stream's gamescope (GAMESCOPE_PID {pid:?}, expected {gamescope_pid}); not sending input there");
        }
        conn.xtest_get_version(2, 2)?.reply().context("the display has no XTEST")?;
        conn.xfixes_query_version(5, 0)?.reply().context("the display has no XFIXES")?;
        conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?;
        conn.flush()?;
        Ok(XInjector { conn, root, size, wheel: (0, 0), held_keys: BTreeSet::new(), held_buttons: BTreeSet::new(), cursor_serial: 0 })
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Send one event. Events this injector doesn't handle (pads, text, touch) are ignored.
    pub fn send(&mut self, ev: &InputEvent) -> Result<()> {
        for (ty, detail, x, y) in requests(ev, self.size, &mut self.wheel, &mut self.held_keys, &mut self.held_buttons) {
            let root = if ty == MOTION && detail == 0 { self.root } else { x11rb::NONE };
            self.conn.xtest_fake_input(ty, detail, 0, root, x, y, 0)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// The cursor, if it changed since the last call: `Some(Control::Cursor{..})`.
    pub fn poll_cursor(&mut self) -> Result<Option<Control>> {
        let mut changed = false;
        while let Some(ev) = self.conn.poll_for_event()? {
            if let x11rb::protocol::Event::XfixesCursorNotify(n) = ev
                && n.cursor_serial != self.cursor_serial
            {
                changed = true;
            }
        }
        if !changed && self.cursor_serial != 0 {
            return Ok(None);
        }
        let img = self.conn.xfixes_get_cursor_image()?.reply()?;
        if img.cursor_serial == self.cursor_serial {
            return Ok(None);
        }
        self.cursor_serial = img.cursor_serial;
        Ok(Some(Control::Cursor { shape: cursor_shape(img.width, img.height, img.xhot, img.yhot, &img.cursor_image) }))
    }
}

/// XFIXES' premultiplied ARGB words to the protocol's RGBA, or None for a cursor that shows
/// nothing (games hide the pointer with a blank one). Cursors are cut to 256×256.
pub fn cursor_shape(width: u16, height: u16, hot_x: u16, hot_y: u16, argb: &[u32]) -> Option<CursorImage> {
    if width == 0 || height == 0 || argb.iter().all(|p| p >> 24 == 0) {
        return None;
    }
    let (w, h) = (width.min(256), height.min(256));
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let p = argb.get(y * width as usize + x).copied().unwrap_or(0);
            let a = (p >> 24) as u8;
            let un = |c: u32| if a == 0 { 0 } else { ((c & 0xff) * 255 / a as u32).min(255) as u8 };
            rgba.extend_from_slice(&[un(p >> 16), un(p >> 8), un(p), a]);
        }
    }
    Some(CursorImage { width: w, height: h, hot_x: hot_x.min(w - 1), hot_y: hot_y.min(h - 1), rgba, relative: false })
}

impl Drop for XInjector {
    fn drop(&mut self) {
        let _ = self.send(&InputEvent::ReleaseAll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(evs: &[InputEvent]) -> Vec<(u8, u8, i16, i16)> {
        let (mut w, mut k, mut b) = ((0, 0), BTreeSet::new(), BTreeSet::new());
        evs.iter().flat_map(|e| requests(e, (2560, 1440), &mut w, &mut k, &mut b)).collect()
    }

    #[test]
    fn keys_are_evdev_plus_eight() {
        // KEY_A is 30, X keycode 38; KEY_ESC 1 → 9.
        assert_eq!(run(&[InputEvent::Key { code: 30, down: true }, InputEvent::Key { code: 1, down: false }]), vec![(KEY_PRESS, 38, 0, 0), (KEY_RELEASE, 9, 0, 0)]);
        // Codes past X's range are dropped.
        assert!(run(&[InputEvent::Key { code: 300, down: true }]).is_empty());
    }

    #[test]
    fn motion_relative_and_absolute() {
        assert_eq!(run(&[InputEvent::MouseMotion { dx: 5, dy: -3 }]), vec![(MOTION, 1, 5, -3)]);
        assert_eq!(run(&[InputEvent::MouseMotion { dx: 40000, dy: 0 }]), vec![(MOTION, 1, 32767, 0), (MOTION, 1, 7233, 0)]);
        assert_eq!(run(&[InputEvent::MousePosition { x: 65535, y: 0 }]), vec![(MOTION, 0, 2559, 0)]);
        assert_eq!(run(&[InputEvent::MousePosition { x: 32768, y: 32768 }]), vec![(MOTION, 0, 1279, 719)]);
    }

    #[test]
    fn the_wheel_clicks_per_notch_and_keeps_the_remainder() {
        let up = |n| InputEvent::Wheel { dx: 0, dy: n };
        // Half a notch twice is one click up.
        assert_eq!(run(&[up(60)]), vec![]);
        assert_eq!(run(&[up(60), up(60)]), vec![(BUTTON_PRESS, 4, 0, 0), (BUTTON_RELEASE, 4, 0, 0)]);
        assert_eq!(run(&[up(-240)]).len(), 4);
        assert_eq!(run(&[up(-240)])[0], (BUTTON_PRESS, 5, 0, 0));
        assert_eq!(run(&[InputEvent::Wheel { dx: 120, dy: 0 }])[0], (BUTTON_PRESS, 7, 0, 0));
        let mut acc = 0;
        assert_eq!(wheel_notches(&mut acc, -150), -1);
        assert_eq!(acc, -30);
    }

    #[test]
    fn release_all_lets_go_of_everything_held() {
        let r = run(&[
            InputEvent::Key { code: 30, down: true },
            InputEvent::Key { code: 31, down: true },
            InputEvent::Key { code: 31, down: false },
            InputEvent::MouseButton { button: MouseButton::Right, down: true },
            InputEvent::ReleaseAll,
        ]);
        assert_eq!(&r[4..], &[(KEY_RELEASE, 38, 0, 0), (BUTTON_RELEASE, 3, 0, 0)]);
    }

    #[test]
    fn cursors_hidden_and_shown() {
        assert_eq!(cursor_shape(2, 1, 0, 0, &[0, 0]), None);
        assert_eq!(cursor_shape(0, 0, 0, 0, &[]), None);
        // Half-transparent white, premultiplied 0x80808080 → (255, 255, 255, 128).
        let c = cursor_shape(1, 1, 3, 3, &[0x8080_8080]).unwrap();
        assert_eq!((c.rgba.as_slice(), c.hot_x, c.relative), (&[255u8, 255, 255, 128][..], 0, false));
    }
}
