//! Compact binary forms of the proto's media and input types.
//!
//! Input is the hot path upstream (an 8 kHz mouse), so every [`InputEvent`] has a fixed
//! binary form a few bytes long rather than JSON. The match is exhaustive on purpose: a new
//! event in `quiver-stream-proto` fails to build here until it has a wire form.

use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioPacket, EncodedFrame, InputEvent, MouseButton, PadButtons, PadKind, PadState};

use crate::wire::{Malformed, Reader, Writer};

// --- input -------------------------------------------------------------------------------------

pub fn put_stamped(w: &mut Writer<'_>, s: &Stamped) {
    w.varint(s.seq).varint(s.at);
    put_event(w, &s.event);
}

pub fn get_stamped(r: &mut Reader<'_>) -> Result<Stamped, Malformed> {
    let seq = r.varint()?;
    let at = r.varint()?;
    let event = get_event(r)?;
    Ok(Stamped { seq, at, event })
}

pub fn put_event(w: &mut Writer<'_>, e: &InputEvent) {
    match e {
        InputEvent::Key { code, down } => {
            w.u8(1).u16(*code).u8(*down as u8);
        }
        InputEvent::Text { text } => {
            // At most 1 KiB, cut on a character boundary.
            let mut n = text.len().min(1024);
            while !text.is_char_boundary(n) {
                n -= 1;
            }
            w.u8(2).blob16(&text.as_bytes()[..n]);
        }
        InputEvent::MouseMotion { dx, dy } => {
            w.u8(3).svarint(i64::from(*dx)).svarint(i64::from(*dy));
        }
        InputEvent::MousePosition { x, y } => {
            w.u8(4).u16(*x).u16(*y);
        }
        InputEvent::MouseButton { button, down } => {
            w.u8(5).u8(button_byte(*button)).u8(*down as u8);
        }
        InputEvent::Wheel { dx, dy } => {
            w.u8(6).svarint(i64::from(*dx)).svarint(i64::from(*dy));
        }
        InputEvent::PadArrive { slot, kind } => {
            w.u8(7).u8(*slot).u8(pad_kind_byte(*kind));
        }
        InputEvent::PadLeave { slot } => {
            w.u8(8).u8(*slot);
        }
        InputEvent::Pad { slot, state } => {
            w.u8(9).u8(*slot).u32(state.buttons.0);
            w.i16(state.lx).i16(state.ly).i16(state.rx).i16(state.ry).u8(state.lt).u8(state.rt);
        }
        InputEvent::PadMotion { slot, gyro, accel } => {
            w.u8(10).u8(*slot);
            for v in gyro.iter().chain(accel) {
                w.i16(*v);
            }
        }
        InputEvent::Touch { id, x, y, down } => {
            w.u8(11).varint(u64::from(*id)).u16(*x).u16(*y).u8(*down as u8);
        }
        InputEvent::ReleaseAll => {
            w.u8(12);
        }
    }
}

pub fn get_event(r: &mut Reader<'_>) -> Result<InputEvent, Malformed> {
    Ok(match r.u8()? {
        1 => InputEvent::Key { code: r.u16()?, down: get_bool(r)? },
        2 => InputEvent::Text { text: String::from_utf8(r.blob16()?.to_vec()).map_err(|_| Malformed)? },
        3 => InputEvent::MouseMotion { dx: get_i32(r)?, dy: get_i32(r)? },
        4 => InputEvent::MousePosition { x: r.u16()?, y: r.u16()? },
        5 => InputEvent::MouseButton { button: button_from(r.u8()?)?, down: get_bool(r)? },
        6 => InputEvent::Wheel { dx: get_i32(r)?, dy: get_i32(r)? },
        7 => InputEvent::PadArrive { slot: r.u8()?, kind: pad_kind_from(r.u8()?)? },
        8 => InputEvent::PadLeave { slot: r.u8()? },
        9 => {
            let slot = r.u8()?;
            let buttons = PadButtons(r.u32()?);
            let state = PadState {
                buttons,
                lx: r.i16()?,
                ly: r.i16()?,
                rx: r.i16()?,
                ry: r.i16()?,
                lt: r.u8()?,
                rt: r.u8()?,
            };
            InputEvent::Pad { slot, state }
        }
        10 => {
            let slot = r.u8()?;
            let mut v = [0i16; 6];
            for x in &mut v {
                *x = r.i16()?;
            }
            InputEvent::PadMotion { slot, gyro: [v[0], v[1], v[2]], accel: [v[3], v[4], v[5]] }
        }
        11 => {
            let id = u32::try_from(r.varint()?).map_err(|_| Malformed)?;
            InputEvent::Touch { id, x: r.u16()?, y: r.u16()?, down: get_bool(r)? }
        }
        12 => InputEvent::ReleaseAll,
        _ => return Err(Malformed),
    })
}

fn get_bool(r: &mut Reader<'_>) -> Result<bool, Malformed> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Malformed),
    }
}

fn get_i32(r: &mut Reader<'_>) -> Result<i32, Malformed> {
    i32::try_from(r.svarint()?).map_err(|_| Malformed)
}

fn button_byte(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

fn button_from(b: u8) -> Result<MouseButton, Malformed> {
    Ok(match b {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => return Err(Malformed),
    })
}

fn pad_kind_byte(k: PadKind) -> u8 {
    match k {
        PadKind::Xbox => 0,
        PadKind::PlayStation => 1,
        PadKind::SwitchPro => 2,
        PadKind::SteamDeck => 3,
        PadKind::Generic => 4,
    }
}

fn pad_kind_from(b: u8) -> Result<PadKind, Malformed> {
    Ok(match b {
        0 => PadKind::Xbox,
        1 => PadKind::PlayStation,
        2 => PadKind::SwitchPro,
        3 => PadKind::SteamDeck,
        4 => PadKind::Generic,
        _ => return Err(Malformed),
    })
}

// --- video ---------------------------------------------------------------------------------------

/// Bytes of frame metadata. It rides at the *end* of a frame's byte string, so the receiver
/// can hand the reassembled buffer up as the frame's data by truncating it, with no copy.
pub const FRAME_META_LEN: usize = 33;

/// The frame's bytes as they are cut into shards: its data, then its metadata.
pub fn frame_blob_len(frame: &EncodedFrame) -> usize {
    frame.data.len() + FRAME_META_LEN
}

pub fn put_frame_meta(out: &mut Vec<u8>, f: &EncodedFrame) {
    let mut w = Writer::new(out);
    w.u64(f.captured).u64(f.encoded).u64(f.refs_from_seq).u32(f.width).u32(f.height).u8(f.keyframe as u8);
}

/// Turns a reassembled blob back into a frame, reusing its allocation for the data.
pub fn frame_from_blob(seq: u64, mut blob: Vec<u8>) -> Result<EncodedFrame, Malformed> {
    let data_len = blob.len().checked_sub(FRAME_META_LEN).ok_or(Malformed)?;
    let mut r = Reader::new(&blob[data_len..]);
    let captured = r.u64()?;
    let encoded = r.u64()?;
    let refs_from_seq = r.u64()?;
    let width = r.u32()?;
    let height = r.u32()?;
    let keyframe = r.u8()? & 1 != 0;
    blob.truncate(data_len);
    Ok(EncodedFrame { seq, captured, encoded, keyframe, refs_from_seq, width, height, data: blob })
}

// --- audio ---------------------------------------------------------------------------------------

pub fn put_audio(w: &mut Writer<'_>, p: &AudioPacket) {
    w.u64(p.seq).u64(p.captured).u8(p.mic as u8).blob16(&p.data);
}

pub fn get_audio(r: &mut Reader<'_>) -> Result<AudioPacket, Malformed> {
    Ok(AudioPacket { seq: r.u64()?, captured: r.u64()?, mic: r.u8()? & 1 != 0, data: r.blob16()?.to_vec() })
}

pub fn audio_wire_len(p: &AudioPacket) -> usize {
    19 + p.data.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn every_event() -> Vec<InputEvent> {
        vec![
            InputEvent::Key { code: 58, down: true },
            InputEvent::Text { text: "héllo".into() },
            InputEvent::MouseMotion { dx: -3, dy: i32::MAX },
            InputEvent::MousePosition { x: 1, y: 65535 },
            InputEvent::MouseButton { button: MouseButton::Forward, down: false },
            InputEvent::Wheel { dx: 0, dy: -120 },
            InputEvent::PadArrive { slot: 2, kind: PadKind::SteamDeck },
            InputEvent::PadLeave { slot: 3 },
            InputEvent::Pad {
                slot: 1,
                state: PadState { buttons: PadButtons(PadButtons::A | PadButtons::GUIDE), lx: -32768, ly: 32767, rx: 5, ry: -5, lt: 255, rt: 1 },
            },
            InputEvent::PadMotion { slot: 0, gyro: [1, -2, 3], accel: [4096, 0, -4096] },
            InputEvent::Touch { id: 70000, x: 3, y: 4, down: true },
            InputEvent::ReleaseAll,
        ]
    }

    #[test]
    fn every_input_event_round_trips() {
        let mut buf = Vec::new();
        let events = every_event();
        for (i, e) in events.iter().enumerate() {
            put_stamped(&mut Writer::new(&mut buf), &Stamped { seq: i as u64, at: 1_000_000 + i as u64, event: e.clone() });
        }
        let mut r = Reader::new(&buf);
        for (i, e) in events.iter().enumerate() {
            let s = get_stamped(&mut r).unwrap();
            assert_eq!((s.seq, s.at, &s.event), (i as u64, 1_000_000 + i as u64, e));
        }
        assert!(r.is_empty());
    }

    #[test]
    fn hot_events_are_small() {
        let mut buf = Vec::new();
        put_event(&mut Writer::new(&mut buf), &InputEvent::MouseMotion { dx: 4, dy: -7 });
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn frame_meta_rides_behind_the_data() {
        let f = EncodedFrame {
            seq: 9,
            captured: 1,
            encoded: 2,
            keyframe: true,
            refs_from_seq: 9,
            width: 2560,
            height: 1440,
            data: vec![7; 100],
        };
        let mut blob = f.data.clone();
        put_frame_meta(&mut blob, &f);
        assert_eq!(blob.len(), frame_blob_len(&f));
        assert_eq!(frame_from_blob(9, blob).unwrap(), f);
        assert!(frame_from_blob(1, vec![0; 3]).is_err());
    }

    #[test]
    fn audio_round_trips() {
        let p = AudioPacket { seq: 5, captured: 6, mic: true, data: vec![1, 2, 3] };
        let mut buf = Vec::new();
        put_audio(&mut Writer::new(&mut buf), &p);
        assert_eq!(buf.len(), audio_wire_len(&p));
        assert_eq!(get_audio(&mut Reader::new(&buf)).unwrap(), p);
    }
}
