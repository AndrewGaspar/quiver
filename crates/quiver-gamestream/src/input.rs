//! Moonlight's input packets, turned into [`InputEvent`]s.
//!
//! Each input packet rides in a control message of type 0x0206. It starts with an 8-byte
//! header: the length of the rest (big-endian, counting the magic) and a magic number
//! (little-endian) naming the event. Field byte orders differ by event, as noted below;
//! these are the Gen 7 (Sunshine-compatible) forms Moonlight sends to a 7.1.431 host.
//!
//! Keys arrive as Windows virtual-key codes, which [`vk_to_evdev`] maps to Linux key codes by
//! position on a US layout (the host's keymap applies). Pads arrive as XInput-shaped state
//! with Moonlight's button flags, which match [`PadButtons`] except Misc.

use anyhow::{Result, bail};
use quiver_stream_proto::{InputEvent, MouseButton, PadButtons, PadKind, PadState};

pub const KEY_DOWN: u32 = 0x03;
pub const KEY_UP: u32 = 0x04;
pub const MOUSE_ABSOLUTE: u32 = 0x05;
pub const MOUSE_RELATIVE: u32 = 0x07;
pub const MOUSE_BUTTON_DOWN: u32 = 0x08;
pub const MOUSE_BUTTON_UP: u32 = 0x09;
pub const SCROLL: u32 = 0x0A;
pub const MULTI_CONTROLLER: u32 = 0x0C;
pub const HAPTICS: u32 = 0x0D;
pub const UTF8_TEXT: u32 = 0x17;
pub const HSCROLL: u32 = 0x5500_0001;
pub const TOUCH: u32 = 0x5500_0002;
pub const PEN: u32 = 0x5500_0003;
pub const CONTROLLER_ARRIVAL: u32 = 0x5500_0004;
pub const CONTROLLER_TOUCH: u32 = 0x5500_0005;
pub const CONTROLLER_MOTION: u32 = 0x5500_0006;
pub const CONTROLLER_BATTERY: u32 = 0x5500_0007;

/// Moonlight's Misc button (Share, Capture, Mute), which [`PadButtons::MISC`] holds.
const MOONLIGHT_MISC: u32 = 0x20_0000;
/// Pad capability bits in an arrival packet.
pub const CAP_RUMBLE: u16 = 0x02;
pub const CAP_TRIGGER_RUMBLE: u16 = 0x04;
pub const CAP_ACCEL: u16 = 0x10;
pub const CAP_GYRO: u16 = 0x20;

pub const MOTION_ACCEL: u8 = 1;
pub const MOTION_GYRO: u8 = 2;

const TOUCH_DOWN: u8 = 0x01;
const TOUCH_UP: u8 = 0x02;
const TOUCH_MOVE: u8 = 0x03;
const TOUCH_CANCEL: u8 = 0x04;
const TOUCH_HOVER: u8 = 0x00;
const TOUCH_CANCEL_ALL: u8 = 0x07;

/// Something the decoder wants the host told that isn't an input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A pad arrived with motion sensors: ask the client to start sending them.
    EnableMotion { slot: u8, accel: bool, gyro: bool },
}

#[derive(Debug, Default, Clone)]
struct Pad {
    gyro: [i16; 3],
    accel: [i16; 3],
}

/// Turns input packets into events, tracking pads across packets.
#[derive(Debug, Default)]
pub struct InputDecoder {
    pads: [Option<Pad>; 16],
    touches_down: Vec<u32>,
    pen_down: bool,
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.at + n > self.b.len() {
            bail!("input packet ends early ({} of {} bytes)", self.b.len(), self.at + n);
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn le16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn be16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn le32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.at..];
        self.at = self.b.len();
        s
    }
}

/// A fraction 0.0..=1.0 as 0..=65535.
fn unit(f: f32) -> u16 {
    (f.clamp(0.0, 1.0) * 65535.0).round() as u16
}

impl InputDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one input packet (the payload of a 0x0206 control message).
    pub fn decode(&mut self, packet: &[u8], notices: &mut Vec<Notice>) -> Result<Vec<InputEvent>> {
        let mut r = Reader::new(packet);
        let size = u32::from_be_bytes(r.take(4)?.try_into().unwrap()) as usize;
        if size + 4 > packet.len() || size < 4 {
            bail!("input packet says {size} bytes, has {}", packet.len() - 4);
        }
        let mut r = Reader::new(&packet[4..4 + size]);
        let magic = r.le32()?;
        let mut ev = Vec::new();
        match magic {
            KEY_DOWN | KEY_UP => {
                let _flags = r.u8()?;
                let vk = r.le16()?;
                let _modifiers = r.u8()?;
                match vk_to_evdev((vk & 0xFF) as u8) {
                    Some(code) => ev.push(InputEvent::Key { code, down: magic == KEY_DOWN }),
                    None => tracing::debug!(vk, "no evdev key for this virtual-key code"),
                }
            }
            UTF8_TEXT => {
                let text = String::from_utf8_lossy(r.rest()).trim_end_matches('\0').to_owned();
                if !text.is_empty() {
                    ev.push(InputEvent::Text { text });
                }
            }
            MOUSE_RELATIVE => {
                let dx = r.be16()? as i16;
                let dy = r.be16()? as i16;
                ev.push(InputEvent::MouseMotion { dx: dx.into(), dy: dy.into() });
            }
            MOUSE_ABSOLUTE => {
                let x = r.be16()? as i16;
                let y = r.be16()? as i16;
                let _unused = r.be16()?;
                // The reference size minus one: x and y run 0..=width.
                let w = (r.be16()? as i16).max(1);
                let h = (r.be16()? as i16).max(1);
                let fx = f32::from(x.clamp(0, w)) / f32::from(w);
                let fy = f32::from(y.clamp(0, h)) / f32::from(h);
                ev.push(InputEvent::MousePosition { x: unit(fx), y: unit(fy) });
            }
            MOUSE_BUTTON_DOWN | MOUSE_BUTTON_UP => {
                let button = match r.u8()? {
                    1 => MouseButton::Left,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Right,
                    4 => MouseButton::Back,
                    5 => MouseButton::Forward,
                    other => bail!("mouse button {other}"),
                };
                ev.push(InputEvent::MouseButton { button, down: magic == MOUSE_BUTTON_DOWN });
            }
            SCROLL => {
                // 120 per notch, positive away from the user (up), as Windows counts it.
                let amount = r.be16()? as i16;
                ev.push(InputEvent::Wheel { dx: 0, dy: amount.into() });
            }
            HSCROLL => {
                let amount = r.be16()? as i16;
                ev.push(InputEvent::Wheel { dx: amount.into(), dy: 0 });
            }
            MULTI_CONTROLLER => self.multi_controller(&mut r, &mut ev)?,
            CONTROLLER_ARRIVAL => {
                let slot = r.u8()?;
                let kind = match r.u8()? {
                    1 => PadKind::Xbox,
                    2 => PadKind::PlayStation,
                    3 => PadKind::SwitchPro,
                    4 => PadKind::SteamDeck,
                    _ => PadKind::Generic,
                };
                let caps = r.le16()?;
                let _buttons = r.le32()?;
                if usize::from(slot) < self.pads.len() {
                    if self.pads[usize::from(slot)].is_some() {
                        ev.push(InputEvent::PadLeave { slot });
                    }
                    self.pads[usize::from(slot)] = Some(Pad::default());
                    ev.push(InputEvent::PadArrive { slot, kind });
                    let (accel, gyro) = (caps & CAP_ACCEL != 0, caps & CAP_GYRO != 0);
                    if accel || gyro {
                        notices.push(Notice::EnableMotion { slot, accel, gyro });
                    }
                }
            }
            CONTROLLER_MOTION => {
                let slot = r.u8()?;
                let kind = r.u8()?;
                r.take(2)?;
                let (x, y, z) = (r.f32()?, r.f32()?, r.f32()?);
                if let Some(Some(pad)) = self.pads.get_mut(usize::from(slot)) {
                    match kind {
                        // m/s² to g × 4096.
                        MOTION_ACCEL => pad.accel = [x, y, z].map(|v| (v / 9.806_65 * 4096.0).clamp(-32768.0, 32767.0) as i16),
                        // deg/s to deg/s × 16.
                        MOTION_GYRO => pad.gyro = [x, y, z].map(|v| (v * 16.0).clamp(-32768.0, 32767.0) as i16),
                        _ => return Ok(ev),
                    }
                    ev.push(InputEvent::PadMotion { slot, gyro: pad.gyro, accel: pad.accel });
                }
            }
            TOUCH => {
                let kind = r.u8()?;
                r.take(1)?;
                let _rotation = r.le16()?;
                let id = r.le32()?;
                let (x, y) = (r.f32()?, r.f32()?);
                self.touch(kind, id, unit(x), unit(y), &mut ev);
            }
            PEN => {
                // A pen drives the mouse: games rarely take pen input, and every host can
                // move a mouse.
                let kind = r.u8()?;
                let _tool = r.u8()?;
                let _buttons = r.u8()?;
                r.take(1)?;
                let (x, y) = (r.f32()?, r.f32()?);
                if kind != TOUCH_CANCEL_ALL {
                    ev.push(InputEvent::MousePosition { x: unit(x), y: unit(y) });
                }
                let down = match kind {
                    TOUCH_DOWN => Some(true),
                    TOUCH_UP | TOUCH_CANCEL | TOUCH_CANCEL_ALL => Some(false),
                    _ => None,
                };
                if let Some(down) = down
                    && down != self.pen_down
                {
                    self.pen_down = down;
                    ev.push(InputEvent::MouseButton { button: MouseButton::Left, down });
                }
            }
            HAPTICS | CONTROLLER_TOUCH | CONTROLLER_BATTERY => {}
            other => tracing::debug!(magic = format_args!("{other:#x}"), "ignored input packet"),
        }
        Ok(ev)
    }

    fn touch(&mut self, kind: u8, id: u32, x: u16, y: u16, ev: &mut Vec<InputEvent>) {
        match kind {
            TOUCH_DOWN | TOUCH_MOVE => {
                if !self.touches_down.contains(&id) {
                    self.touches_down.push(id);
                }
                ev.push(InputEvent::Touch { id, x, y, down: true });
            }
            TOUCH_UP | TOUCH_CANCEL => {
                self.touches_down.retain(|&t| t != id);
                ev.push(InputEvent::Touch { id, x, y, down: false });
            }
            TOUCH_CANCEL_ALL => {
                for t in std::mem::take(&mut self.touches_down) {
                    ev.push(InputEvent::Touch { id: t, x, y, down: false });
                }
            }
            TOUCH_HOVER => {}
            _ => {}
        }
    }

    fn multi_controller(&mut self, r: &mut Reader<'_>, ev: &mut Vec<InputEvent>) -> Result<()> {
        let _header_b = r.le16()?;
        let slot = r.le16()? as u8 & 0x0F;
        let active = r.le16()?;
        let _mid_b = r.le16()?;
        let low = u32::from(r.le16()?);
        let lt = r.u8()?;
        let rt = r.u8()?;
        let lx = r.le16()? as i16;
        let ly = r.le16()? as i16;
        let rx = r.le16()? as i16;
        let ry = r.le16()? as i16;
        let _tail_a = r.le16()?;
        let high = u32::from(r.le16()?);
        let flags = low | (high << 16);

        // Pads whose bit went clear have gone.
        for s in 0..self.pads.len() {
            if self.pads[s].is_some() && active & (1 << s) == 0 {
                self.pads[s] = None;
                ev.push(InputEvent::PadLeave { slot: s as u8 });
            }
        }
        if active & (1 << slot) == 0 {
            return Ok(());
        }
        if self.pads[usize::from(slot)].is_none() {
            // A client that doesn't send arrivals: assume an Xbox pad.
            self.pads[usize::from(slot)] = Some(Pad::default());
            ev.push(InputEvent::PadArrive { slot, kind: PadKind::Xbox });
        }
        let mut buttons = flags & !(MOONLIGHT_MISC | 0x0800);
        if flags & MOONLIGHT_MISC != 0 {
            buttons |= PadButtons::MISC;
        }
        ev.push(InputEvent::Pad { slot, state: PadState { buttons: PadButtons(buttons), lx, ly, rx, ry, lt, rt } });
        Ok(())
    }

    /// The pads currently plugged in.
    pub fn pad_slots(&self) -> Vec<u8> {
        (0..self.pads.len()).filter(|&s| self.pads[s].is_some()).map(|s| s as u8).collect()
    }
}

/// A Windows virtual-key code to a Linux evdev key code, by US-layout position.
pub fn vk_to_evdev(vk: u8) -> Option<u16> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    Some(match vk {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0D => 28,  // Enter
        0x10 => 42,  // Shift
        0x11 => 29,  // Ctrl
        0x12 => 56,  // Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Caps Lock
        0x1B => 1,   // Escape
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2C => 99,  // Print Screen (SysRq)
        0x2D => 110, // Insert
        0x2E => 111, // Delete
        0x2F => 138, // Help
        0x30 => 11,  // 0
        0x31..=0x39 => 2 + u16::from(vk - 0x31),
        0x41..=0x5A => LETTERS[usize::from(vk - 0x41)],
        0x5B => 125, // Left Super
        0x5C => 126, // Right Super
        0x5D => 127, // Menu (Compose)
        0x5F => 142, // Sleep
        0x60 => 82,  // Keypad 0
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73, // Keypad 9
        0x6A => 55, // Keypad *
        0x6B => 78, // Keypad +
        0x6C => 121, // Keypad ,
        0x6D => 74, // Keypad -
        0x6E => 83, // Keypad .
        0x6F => 98, // Keypad /
        0x70..=0x79 => 59 + u16::from(vk - 0x70), // F1..F10
        0x7A => 87,                               // F11
        0x7B => 88,                               // F12
        0x7C..=0x87 => 183 + u16::from(vk - 0x7C), // F13..F24
        0x90 => 69,                               // Num Lock
        0x91 => 70,                               // Scroll Lock
        0xA0 => 42,                               // Left Shift
        0xA1 => 54,                               // Right Shift
        0xA2 => 29,                               // Left Ctrl
        0xA3 => 97,                               // Right Ctrl
        0xA4 => 56,                               // Left Alt
        0xA5 => 100,                              // Right Alt
        0xA6 => 158,                              // Browser Back
        0xA7 => 159,                              // Browser Forward
        0xA8 => 173,                              // Refresh
        0xA9 => 128,                              // Stop
        0xAA => 217,                              // Search
        0xAB => 156,                              // Bookmarks
        0xAC => 172,                              // Home page
        0xAD => 113,                              // Mute
        0xAE => 114,                              // Volume Down
        0xAF => 115,                              // Volume Up
        0xB0 => 163,                              // Next track
        0xB1 => 165,                              // Previous track
        0xB2 => 166,                              // Stop media
        0xB3 => 164,                              // Play/Pause
        0xB4 => 155,                              // Mail
        0xBA => 39,                               // ; :
        0xBB => 13,                               // = +
        0xBC => 51,                               // , <
        0xBD => 12,                               // - _
        0xBE => 52,                               // . >
        0xBF => 53,                               // / ?
        0xC0 => 41,                               // ` ~
        0xDB => 26,                               // [ {
        0xDC => 43,                               // \ |
        0xDD => 27,                               // ] }
        0xDE => 40,                               // ' "
        0xE2 => 86,                               // the ISO key between Left Shift and Z
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds an input packet: the big-endian length, the little-endian magic, the body.
    pub fn packet(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut p = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        p.extend_from_slice(&magic.to_le_bytes());
        p.extend_from_slice(body);
        p
    }

    fn one(d: &mut InputDecoder, p: &[u8]) -> Vec<InputEvent> {
        d.decode(p, &mut Vec::new()).unwrap()
    }

    #[test]
    fn keyboard() {
        let mut d = InputDecoder::new();
        // Moonlight sends 0x80 in the high byte; 'A' down, Escape up.
        assert_eq!(one(&mut d, &packet(KEY_DOWN, &[0, 0x41, 0x80, 0, 0, 0])), [InputEvent::Key { code: 30, down: true }]);
        assert_eq!(one(&mut d, &packet(KEY_UP, &[0, 0x1B, 0x80, 0, 0, 0])), [InputEvent::Key { code: 1, down: false }]);
        assert_eq!(vk_to_evdev(0x14), Some(58), "Caps Lock");
        assert_eq!(vk_to_evdev(0x39), Some(10), "9");
        assert_eq!(vk_to_evdev(0x5A), Some(44), "Z");
        assert_eq!(vk_to_evdev(0x7B), Some(88), "F12");
        assert_eq!(vk_to_evdev(0x87), Some(194), "F24");
        assert!(one(&mut d, &packet(KEY_DOWN, &[0, 0xFF, 0x80, 0, 0, 0])).is_empty());
    }

    #[test]
    fn mouse() {
        let mut d = InputDecoder::new();
        assert_eq!(one(&mut d, &packet(MOUSE_RELATIVE, &[0xFF, 0xFE, 0x00, 0x05])), [InputEvent::MouseMotion { dx: -2, dy: 5 }]);
        // Absolute: x = 960 of 1919 (width - 1), y = 0.
        let mut body = Vec::new();
        for v in [960i16, 0, 0, 1919, 1079] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(one(&mut d, &packet(MOUSE_ABSOLUTE, &body)), [InputEvent::MousePosition { x: 32785, y: 0 }]);
        assert_eq!(one(&mut d, &packet(MOUSE_BUTTON_DOWN, &[3])), [InputEvent::MouseButton { button: MouseButton::Right, down: true }]);
        assert_eq!(one(&mut d, &packet(MOUSE_BUTTON_UP, &[5])), [InputEvent::MouseButton { button: MouseButton::Forward, down: false }]);
        assert_eq!(one(&mut d, &packet(SCROLL, &[0xFF, 0x88, 0, 0, 0, 0])), [InputEvent::Wheel { dx: 0, dy: -120 }]);
        assert_eq!(one(&mut d, &packet(HSCROLL, &[0x00, 0x3C])), [InputEvent::Wheel { dx: 60, dy: 0 }]);
        assert_eq!(one(&mut d, &packet(UTF8_TEXT, "héllo".as_bytes())), [InputEvent::Text { text: "héllo".into() }]);
    }

    pub fn controller(slot: u16, active: u16, buttons: u32, lt: u8, sticks: [i16; 4]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0x001Au16.to_le_bytes());
        b.extend_from_slice(&slot.to_le_bytes());
        b.extend_from_slice(&active.to_le_bytes());
        b.extend_from_slice(&0x0014u16.to_le_bytes());
        b.extend_from_slice(&(buttons as u16).to_le_bytes());
        b.push(lt);
        b.push(0);
        for s in sticks {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b.extend_from_slice(&0x009Cu16.to_le_bytes());
        b.extend_from_slice(&((buttons >> 16) as u16).to_le_bytes());
        b.extend_from_slice(&0x0055u16.to_le_bytes());
        packet(MULTI_CONTROLLER, &b)
    }

    #[test]
    fn controllers() {
        let mut d = InputDecoder::new();
        let mut notices = Vec::new();
        // A DualSense arrives with gyro and accel.
        let mut arrival = vec![1u8, 2];
        arrival.extend_from_slice(&(CAP_RUMBLE | CAP_ACCEL | CAP_GYRO).to_le_bytes());
        arrival.extend_from_slice(&0u32.to_le_bytes());
        let ev = d.decode(&packet(CONTROLLER_ARRIVAL, &arrival), &mut notices).unwrap();
        assert_eq!(ev, [InputEvent::PadArrive { slot: 1, kind: PadKind::PlayStation }]);
        assert_eq!(notices, [Notice::EnableMotion { slot: 1, accel: true, gyro: true }]);

        // A + Guide + Misc, left trigger, sticks; pad 0 appears without an arrival.
        let ev = one(&mut d, &controller(0, 0b11, 0x1000 | 0x0400 | 0x20_0000, 200, [100, -100, 32767, -32768]));
        assert_eq!(ev[0], InputEvent::PadArrive { slot: 0, kind: PadKind::Xbox });
        let InputEvent::Pad { slot: 0, state } = ev[1] else { panic!("{ev:?}") };
        assert_eq!(state.buttons.0, PadButtons::A | PadButtons::GUIDE | PadButtons::MISC);
        assert_eq!((state.lt, state.lx, state.ly, state.rx, state.ry), (200, 100, -100, 32767, -32768));
        // Paddles carry over.
        let ev = one(&mut d, &controller(1, 0b11, 0x01_0000, 0, [0; 4]));
        let InputEvent::Pad { slot: 1, state } = ev[0] else { panic!("{ev:?}") };
        assert!(state.buttons.has(PadButtons::PADDLE1));

        // Gyro then accel; each update carries both.
        let mut m = vec![1u8, MOTION_GYRO, 0, 0];
        for v in [10.0f32, -20.0, 0.5] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(one(&mut d, &packet(CONTROLLER_MOTION, &m)), [InputEvent::PadMotion { slot: 1, gyro: [160, -320, 8], accel: [0; 3] }]);
        let mut m = vec![1u8, MOTION_ACCEL, 0, 0];
        for v in [0.0f32, 9.806_65, 0.0] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(one(&mut d, &packet(CONTROLLER_MOTION, &m)), [InputEvent::PadMotion { slot: 1, gyro: [160, -320, 8], accel: [0, 4096, 0] }]);

        // Pad 1's bit clears: it leaves.
        let ev = one(&mut d, &controller(0, 0b01, 0, 0, [0; 4]));
        assert_eq!(ev[0], InputEvent::PadLeave { slot: 1 });
        assert_eq!(d.pad_slots(), [0]);
    }

    #[test]
    fn touch_and_pen() {
        let mut d = InputDecoder::new();
        let touch = |kind: u8, id: u32, x: f32, y: f32| {
            let mut b = vec![kind, 0, 0, 0];
            b.extend_from_slice(&id.to_le_bytes());
            for v in [x, y, 1.0, 0.0, 0.0] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            packet(TOUCH, &b)
        };
        assert_eq!(one(&mut d, &touch(TOUCH_DOWN, 7, 0.5, 1.0)), [InputEvent::Touch { id: 7, x: 32768, y: 65535, down: true }]);
        assert_eq!(one(&mut d, &touch(TOUCH_CANCEL_ALL, 0, 0.0, 0.0)), [InputEvent::Touch { id: 7, x: 0, y: 0, down: false }]);
        let pen = |kind: u8| {
            let mut b = vec![kind, 1, 0, 0];
            for v in [0.25f32, 0.75, 1.0] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&[0; 12]);
            packet(PEN, &b)
        };
        assert_eq!(
            one(&mut d, &pen(TOUCH_DOWN)),
            [InputEvent::MousePosition { x: 16384, y: 49151 }, InputEvent::MouseButton { button: MouseButton::Left, down: true }]
        );
        assert_eq!(one(&mut d, &pen(TOUCH_MOVE)), [InputEvent::MousePosition { x: 16384, y: 49151 }]);
    }

    #[test]
    fn malformed() {
        let mut d = InputDecoder::new();
        assert!(d.decode(&[0, 0, 0, 9, 3, 0, 0, 0], &mut Vec::new()).is_err());
        assert!(d.decode(&packet(MOUSE_RELATIVE, &[0]), &mut Vec::new()).is_err());
        assert!(one(&mut d, &packet(0x1234, &[])).is_empty());
    }
}
