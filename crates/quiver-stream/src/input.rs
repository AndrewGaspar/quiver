//! Input capture: SDL keyboard, mouse and gamepad events to [`InputEvent`]s, and the
//! client's own shortcuts.
//!
//! Shortcuts (never forwarded):
//! - **Ctrl+Alt+Shift+Q**: the stream menu (releases capture while it's open).
//! - **Ctrl+Alt+Shift+Z**: release capture. A click in the window captures again.
//! - **Ctrl+Alt+Shift+S**: the stats chip.
//! - **Pad: Back+LB+RB held 1 s**: the stream menu. Guide alone, Guide+Start and Select+Start
//!   stay the game's (Sight's chords, design/SIGHT.md), so they are forwarded untouched.
//!
//! Any shortcut sends `ReleaseAll` first, so the modifiers the host already saw go up.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use quiver_stream_proto::input::{InputEvent, MouseButton, PadButtons, PadKind, PadState};
use sdl3_sys::gamepad::*;
use sdl3_sys::keycode::*;
use sdl3_sys::scancode::*;

use crate::keymap;

/// The client's own actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    Menu,
    Release,
    Stats,
}

/// Ctrl+Alt+Shift plus Q, Z or S, by position (so it works on any layout).
pub fn hotkey(sc: SDL_Scancode, mods: SDL_Keymod) -> Option<Hotkey> {
    let all = mods.0 & SDL_KMOD_CTRL.0 != 0 && mods.0 & SDL_KMOD_ALT.0 != 0 && mods.0 & SDL_KMOD_SHIFT.0 != 0;
    if !all {
        return None;
    }
    match sc {
        SDL_SCANCODE_Q => Some(Hotkey::Menu),
        SDL_SCANCODE_Z => Some(Hotkey::Release),
        SDL_SCANCODE_S => Some(Hotkey::Stats),
        _ => None,
    }
}

/// Keys held, by scancode, with the code their press was sent as.
#[derive(Debug, Default)]
pub struct Keys {
    down: HashMap<i32, u16>,
}

impl Keys {
    /// A key event; returns what to send. Repeats are dropped (the host repeats on its own).
    pub fn event(&mut self, sc: SDL_Scancode, key: SDL_Keycode, down: bool, repeat: bool) -> Option<InputEvent> {
        if repeat {
            return None;
        }
        if down {
            let code = keymap::evdev_for_key(sc, key)?;
            self.down.insert(sc.0, code);
            Some(InputEvent::Key { code, down: true })
        } else {
            let code = self.down.remove(&sc.0)?;
            Some(InputEvent::Key { code, down: false })
        }
    }

    pub fn clear(&mut self) {
        self.down.clear();
    }
}

/// Sub-unit motion and wheel movement carried to the next event.
#[derive(Debug, Default)]
pub struct Accum {
    mx: f32,
    my: f32,
    wx: f32,
    wy: f32,
}

impl Accum {
    /// Relative motion (SDL gives floats; the host wants whole device units).
    pub fn motion(&mut self, dx: f32, dy: f32) -> Option<InputEvent> {
        self.mx += dx;
        self.my += dy;
        let (ix, iy) = (self.mx.trunc(), self.my.trunc());
        self.mx -= ix;
        self.my -= iy;
        (ix != 0.0 || iy != 0.0).then_some(InputEvent::MouseMotion { dx: ix as i32, dy: iy as i32 })
    }

    /// The wheel in notches (fractions on high-resolution wheels and touchpads), as
    /// 120ths. +y is away from the person, +x right.
    pub fn wheel(&mut self, x: f32, y: f32) -> Option<InputEvent> {
        self.wx += x * 120.0;
        self.wy += y * 120.0;
        let (ix, iy) = (self.wx.trunc(), self.wy.trunc());
        self.wx -= ix;
        self.wy -= iy;
        (ix != 0.0 || iy != 0.0).then_some(InputEvent::Wheel { dx: ix as i32, dy: iy as i32 })
    }
}

pub fn mouse_button(b: u8) -> Option<MouseButton> {
    Some(match b {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        4 => MouseButton::Back,
        5 => MouseButton::Forward,
        _ => return None,
    })
}

/// A pointer position in the window to the stream's 0..=65535 range, through the rectangle
/// the picture fills (`rect`: x, y, w, h in window pixels). None outside the picture.
pub fn absolute_position(x: f32, y: f32, rect: (i32, i32, u32, u32)) -> Option<(u16, u16)> {
    let (rx, ry, rw, rh) = (rect.0 as f32, rect.1 as f32, rect.2 as f32, rect.3 as f32);
    if rw <= 0.0 || rh <= 0.0 || x < rx || y < ry || x >= rx + rw || y >= ry + rh {
        return None;
    }
    let fx = ((x - rx) / rw * 65535.0).round().clamp(0.0, 65535.0) as u16;
    let fy = ((y - ry) / rh * 65535.0).round().clamp(0.0, 65535.0) as u16;
    Some((fx, fy))
}

// --- pads ------------------------------------------------------------------------------------

/// SDL's (positional) button to the XInput-shaped bit.
pub fn pad_bit(b: SDL_GamepadButton) -> Option<u32> {
    Some(match b {
        SDL_GAMEPAD_BUTTON_SOUTH => PadButtons::A,
        SDL_GAMEPAD_BUTTON_EAST => PadButtons::B,
        SDL_GAMEPAD_BUTTON_WEST => PadButtons::X,
        SDL_GAMEPAD_BUTTON_NORTH => PadButtons::Y,
        SDL_GAMEPAD_BUTTON_BACK => PadButtons::BACK,
        SDL_GAMEPAD_BUTTON_GUIDE => PadButtons::GUIDE,
        SDL_GAMEPAD_BUTTON_START => PadButtons::START,
        SDL_GAMEPAD_BUTTON_LEFT_STICK => PadButtons::LEFT_THUMB,
        SDL_GAMEPAD_BUTTON_RIGHT_STICK => PadButtons::RIGHT_THUMB,
        SDL_GAMEPAD_BUTTON_LEFT_SHOULDER => PadButtons::LEFT_SHOULDER,
        SDL_GAMEPAD_BUTTON_RIGHT_SHOULDER => PadButtons::RIGHT_SHOULDER,
        SDL_GAMEPAD_BUTTON_DPAD_UP => PadButtons::DPAD_UP,
        SDL_GAMEPAD_BUTTON_DPAD_DOWN => PadButtons::DPAD_DOWN,
        SDL_GAMEPAD_BUTTON_DPAD_LEFT => PadButtons::DPAD_LEFT,
        SDL_GAMEPAD_BUTTON_DPAD_RIGHT => PadButtons::DPAD_RIGHT,
        SDL_GAMEPAD_BUTTON_MISC1 => PadButtons::MISC,
        // Elite numbering: P1/P3 on the right, P2/P4 on the left.
        SDL_GAMEPAD_BUTTON_RIGHT_PADDLE1 => PadButtons::PADDLE1,
        SDL_GAMEPAD_BUTTON_LEFT_PADDLE1 => PadButtons::PADDLE2,
        SDL_GAMEPAD_BUTTON_RIGHT_PADDLE2 => PadButtons::PADDLE3,
        SDL_GAMEPAD_BUTTON_LEFT_PADDLE2 => PadButtons::PADDLE4,
        SDL_GAMEPAD_BUTTON_TOUCHPAD => PadButtons::TOUCHPAD,
        _ => return None,
    })
}

/// Apply an SDL axis value to the state: sticks keep their range with +y up (SDL's +y is
/// down), triggers go from 0..=32767 to 0..=255.
pub fn apply_axis(s: &mut PadState, axis: SDL_GamepadAxis, v: i16) {
    let flip = |v: i16| (-(v as i32)).clamp(-32768, 32767) as i16;
    let trig = |v: i16| ((v.max(0) as u32 * 255 + 16383) / 32767) as u8;
    match axis {
        SDL_GAMEPAD_AXIS_LEFTX => s.lx = v,
        SDL_GAMEPAD_AXIS_LEFTY => s.ly = flip(v),
        SDL_GAMEPAD_AXIS_RIGHTX => s.rx = v,
        SDL_GAMEPAD_AXIS_RIGHTY => s.ry = flip(v),
        SDL_GAMEPAD_AXIS_LEFT_TRIGGER => s.lt = trig(v),
        SDL_GAMEPAD_AXIS_RIGHT_TRIGGER => s.rt = trig(v),
        _ => {}
    }
}

/// Gyro in rad/s (SDL) to deg/s × 16; accelerometer in m/s² to g × 4096. Axis order is
/// SDL's: x pitch, y yaw, z roll.
pub fn motion_units(gyro: [f32; 3], accel: [f32; 3]) -> ([i16; 3], [i16; 3]) {
    let g = |v: f32| (v.to_degrees() * 16.0).round().clamp(-32768.0, 32767.0) as i16;
    let a = |v: f32| (v / 9.80665 * 4096.0).round().clamp(-32768.0, 32767.0) as i16;
    (gyro.map(g), accel.map(a))
}

/// What kind of pad the host should make.
pub fn pad_kind(t: SDL_GamepadType, vendor: u16, product: u16) -> PadKind {
    if vendor == 0x28de && product == 0x1205 {
        return PadKind::SteamDeck;
    }
    match t {
        SDL_GAMEPAD_TYPE_XBOX360 | SDL_GAMEPAD_TYPE_XBOXONE => PadKind::Xbox,
        SDL_GAMEPAD_TYPE_PS3 | SDL_GAMEPAD_TYPE_PS4 | SDL_GAMEPAD_TYPE_PS5 => PadKind::PlayStation,
        SDL_GAMEPAD_TYPE_NINTENDO_SWITCH_PRO
        | SDL_GAMEPAD_TYPE_NINTENDO_SWITCH_JOYCON_PAIR
        | SDL_GAMEPAD_TYPE_NINTENDO_SWITCH_JOYCON_LEFT
        | SDL_GAMEPAD_TYPE_NINTENDO_SWITCH_JOYCON_RIGHT => PadKind::SwitchPro,
        _ => PadKind::Generic,
    }
}

/// The pad chord that opens the stream menu: Back + LB + RB, held.
pub const MENU_CHORD: u32 = PadButtons::BACK | PadButtons::LEFT_SHOULDER | PadButtons::RIGHT_SHOULDER;
pub const MENU_CHORD_HOLD: Duration = Duration::from_millis(1000);

/// Watches one pad's buttons for the menu chord.
#[derive(Debug, Default)]
pub struct ChordWatch {
    since: Option<Instant>,
    fired: bool,
}

impl ChordWatch {
    /// The buttons changed (or time passed): true once when the chord has been held long
    /// enough. Releasing it re-arms it.
    pub fn update(&mut self, buttons: u32, now: Instant) -> bool {
        if buttons & MENU_CHORD == MENU_CHORD && buttons & PadButtons::GUIDE == 0 {
            let since = *self.since.get_or_insert(now);
            if !self.fired && now.duration_since(since) >= MENU_CHORD_HOLD {
                self.fired = true;
                return true;
            }
        } else {
            self.since = None;
            self.fired = false;
        }
        false
    }

    pub fn pending(&self) -> bool {
        self.since.is_some() && !self.fired
    }
}

/// Slots 0–3, first free.
pub fn free_slot(used: impl IntoIterator<Item = u8>) -> Option<u8> {
    let used: Vec<u8> = used.into_iter().collect();
    (0..4).find(|s| !used.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_need_all_three_modifiers() {
        let all = SDL_Keymod(SDL_KMOD_CTRL.0 | SDL_KMOD_ALT.0 | SDL_KMOD_SHIFT.0);
        assert_eq!(hotkey(SDL_SCANCODE_Q, all), Some(Hotkey::Menu));
        assert_eq!(hotkey(SDL_SCANCODE_Z, all), Some(Hotkey::Release));
        assert_eq!(hotkey(SDL_SCANCODE_S, all), Some(Hotkey::Stats));
        assert_eq!(hotkey(SDL_SCANCODE_Q, SDL_Keymod(SDL_KMOD_CTRL.0 | SDL_KMOD_ALT.0)), None);
        assert_eq!(hotkey(SDL_SCANCODE_W, all), None);
        // Right-hand modifiers count too (SDL_KMOD_CTRL covers both).
        let right = SDL_Keymod(SDL_KMOD_RCTRL.0 | SDL_KMOD_RALT.0 | SDL_KMOD_RSHIFT.0);
        assert_eq!(hotkey(SDL_SCANCODE_Q, right), Some(Hotkey::Menu));
    }

    #[test]
    fn release_uses_the_press_code() {
        let mut k = Keys::default();
        // Caps Lock that the keymap makes Escape.
        assert_eq!(k.event(SDL_SCANCODE_CAPSLOCK, SDLK_ESCAPE, true, false), Some(InputEvent::Key { code: keymap::KEY_ESC, down: true }));
        assert_eq!(k.event(SDL_SCANCODE_CAPSLOCK, SDLK_ESCAPE, true, true), None);
        // The symbol changed while held (a layout switch): the release still matches.
        assert_eq!(
            k.event(SDL_SCANCODE_CAPSLOCK, SDLK_CAPSLOCK, false, false),
            Some(InputEvent::Key { code: keymap::KEY_ESC, down: false })
        );
        // A release with no press (pressed before capture) isn't sent.
        assert_eq!(k.event(SDL_SCANCODE_A, SDLK_A, false, false), None);
    }

    #[test]
    fn motion_and_wheel_carry_fractions() {
        let mut a = Accum::default();
        assert_eq!(a.motion(0.4, -0.6), None);
        assert_eq!(a.motion(0.7, -0.6), Some(InputEvent::MouseMotion { dx: 1, dy: -1 }));
        assert_eq!(a.motion(3.0, 0.0), Some(InputEvent::MouseMotion { dx: 3, dy: 0 }));
        // One notch, then a high-resolution quarter notch.
        assert_eq!(a.wheel(0.0, 1.0), Some(InputEvent::Wheel { dx: 0, dy: 120 }));
        assert_eq!(a.wheel(0.0, -0.25), Some(InputEvent::Wheel { dx: 0, dy: -30 }));
        assert_eq!(a.wheel(0.004, 0.0), None);
    }

    #[test]
    fn absolute_positions_map_through_the_picture() {
        let r = (240, 0, 1440, 1080);
        assert_eq!(absolute_position(240.0, 0.0, r), Some((0, 0)));
        assert_eq!(absolute_position(960.0, 540.0, r), Some((32768, 32768)));
        assert_eq!(absolute_position(100.0, 540.0, r), None);
    }

    #[test]
    fn axes_buttons_and_motion() {
        let mut s = PadState::default();
        apply_axis(&mut s, SDL_GAMEPAD_AXIS_LEFTY, -32768);
        assert_eq!(s.ly, 32767);
        apply_axis(&mut s, SDL_GAMEPAD_AXIS_RIGHTY, 32767);
        assert_eq!(s.ry, -32767);
        apply_axis(&mut s, SDL_GAMEPAD_AXIS_RIGHT_TRIGGER, 32767);
        assert_eq!(s.rt, 255);
        apply_axis(&mut s, SDL_GAMEPAD_AXIS_LEFT_TRIGGER, 0);
        assert_eq!(s.lt, 0);
        assert_eq!(pad_bit(SDL_GAMEPAD_BUTTON_SOUTH), Some(PadButtons::A));
        assert_eq!(pad_bit(SDL_GAMEPAD_BUTTON_GUIDE), Some(PadButtons::GUIDE));
        let (g, a) = motion_units([std::f32::consts::PI, 0.0, -1.0], [0.0, -9.80665, 0.0]);
        assert_eq!(g, [2880, 0, -917]);
        assert_eq!(a, [0, -4096, 0]);
        assert_eq!(pad_kind(SDL_GAMEPAD_TYPE_PS5, 0x054c, 0x0ce6), PadKind::PlayStation);
        assert_eq!(pad_kind(SDL_GAMEPAD_TYPE_STANDARD, 0x28de, 0x1205), PadKind::SteamDeck);
        assert_eq!(pad_kind(SDL_GAMEPAD_TYPE_XBOXONE, 0x045e, 0x0b12), PadKind::Xbox);
        assert_eq!(free_slot([0, 2]), Some(1));
        assert_eq!(free_slot([0, 1, 2, 3]), None);
    }

    #[test]
    fn menu_chord_fires_once_after_holding() {
        let t0 = Instant::now();
        let mut c = ChordWatch::default();
        assert!(!c.update(MENU_CHORD, t0));
        assert!(c.pending());
        assert!(!c.update(MENU_CHORD, t0 + Duration::from_millis(500)));
        assert!(c.update(MENU_CHORD | PadButtons::A, t0 + Duration::from_millis(1000)));
        assert!(!c.update(MENU_CHORD, t0 + Duration::from_millis(2000)));
        c.update(0, t0 + Duration::from_millis(2100));
        assert!(!c.pending());
        // With Guide held it's a Sight chord, not ours.
        assert!(!c.update(MENU_CHORD | PadButtons::GUIDE, t0 + Duration::from_millis(3000)));
        assert!(!c.update(MENU_CHORD | PadButtons::GUIDE, t0 + Duration::from_millis(5000)));
    }
}
