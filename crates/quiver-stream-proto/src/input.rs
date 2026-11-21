//! Input from the client to the game.
//!
//! The client sends events as they happen. The transport makes them reliable without
//! waiting: each event goes out in several consecutive datagrams, and a pad's whole state is
//! re-sent periodically. The host drops any event it has already applied (`seq`).

use serde::{Deserialize, Serialize};

use crate::Micros;

/// One input event, with the client's sequence number and the time it was captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub seq: u64,
    pub at: Micros,
    pub event: InputEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum InputEvent {
    /// A key, as a Linux evdev key code (`KEY_*` in linux/input-event-codes.h). This is the
    /// key's position, not its symbol: the host's keymap applies. The exception is keys that
    /// type no text (Escape, Caps Lock, the modifiers, Enter, Tab, Backspace): the client
    /// sends those by what the client's keymap makes them, because an XKB option such as
    /// `caps:swapescape` remaps symbols, not key codes, and the person's swap has to carry
    /// over. A key's release always carries the code its press did.
    Key { code: u16, down: bool },
    /// Text the client's input method composed, for chat boxes. Hosts that can't type it
    /// ignore it.
    Text { text: String },
    /// Relative motion while the mouse is captured, in device units (unaccelerated where the
    /// client can get them).
    MouseMotion { dx: i32, dy: i32 },
    /// Where the pointer is, while it isn't captured, as a fraction of the stream's size
    /// (0..=65535 on each axis).
    MousePosition { x: u16, y: u16 },
    MouseButton { button: MouseButton, down: bool },
    /// The wheel, in 120ths of a notch (high-resolution scrolling).
    Wheel { dx: i32, dy: i32 },
    /// A pad was plugged in on the client. `slot` stays its slot until PadLeave.
    PadArrive { slot: u8, kind: PadKind },
    PadLeave { slot: u8 },
    /// The pad's whole state. Sent on every change, and re-sent periodically, so a lost
    /// datagram costs at most one period.
    Pad { slot: u8, state: PadState },
    /// A pad's motion sensors, for games that use gyro aiming. The gyro is in deg/s × 16 and
    /// the accelerometer in g × 4096.
    PadMotion { slot: u8, gyro: [i16; 3], accel: [i16; 3] },
    /// A touch on the client's screen, as a fraction of the stream's size.
    Touch { id: u32, x: u16, y: u16, down: bool },
    /// Release every key, button and stick. The client sends it when it loses focus or stops
    /// capturing input.
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// The kind of pad the host should make. Every kind is offered to the game as a wired Xbox
/// 360 or Xbox One pad. The kind decides the glyphs Sight shows, and whether there is gyro.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadKind {
    Xbox,
    PlayStation,
    SwitchPro,
    SteamDeck,
    Generic,
}

/// A pad's state, XInput-shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PadState {
    pub buttons: PadButtons,
    /// -32768..=32767; +y is up (the XInput convention).
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    /// 0..=255.
    pub lt: u8,
    pub rt: u8,
}

/// Bit flags, in XInput's layout, plus Guide and Misc (Share/Capture).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PadButtons(pub u32);

impl PadButtons {
    pub const DPAD_UP: u32 = 0x0001;
    pub const DPAD_DOWN: u32 = 0x0002;
    pub const DPAD_LEFT: u32 = 0x0004;
    pub const DPAD_RIGHT: u32 = 0x0008;
    pub const START: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LEFT_THUMB: u32 = 0x0040;
    pub const RIGHT_THUMB: u32 = 0x0080;
    pub const LEFT_SHOULDER: u32 = 0x0100;
    pub const RIGHT_SHOULDER: u32 = 0x0200;
    pub const GUIDE: u32 = 0x0400;
    pub const MISC: u32 = 0x0800;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;
    /// Steam Deck and DualSense extras: back paddles and the touchpad click.
    pub const PADDLE1: u32 = 0x1_0000;
    pub const PADDLE2: u32 = 0x2_0000;
    pub const PADDLE3: u32 = 0x4_0000;
    pub const PADDLE4: u32 = 0x8_0000;
    pub const TOUCHPAD: u32 = 0x10_0000;

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}
