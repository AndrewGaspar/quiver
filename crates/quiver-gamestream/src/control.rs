//! The control stream's messages: what goes inside the ENet packets on UDP 47999.
//!
//! Every message is encrypted with the launch key (Moonlight's "control stream encryption
//! v2", which this server always asks for):
//!
//! ```text
//! u16 LE 0x0001 | u16 LE length (4 + 16 + inner) | u32 LE seq | tag[16] | AES-GCM(inner)
//! inner = u16 LE type | u16 LE payload length | payload
//! ```
//!
//! The GCM IV is the sequence number (little-endian) in bytes 0..4, then zeros, then `C` `C`
//! for client messages or `H` `C` for host messages. Each side counts its own sequence.

use anyhow::{Result, bail};
use quiver_stream_proto::HdrMetadata;

use crate::crypto::{Key, gcm_open, gcm_seal, marked_iv};

/// Message types, client → host.
pub mod from_client {
    pub const REQUEST_IDR: u16 = 0x0302;
    pub const START_B: u16 = 0x0307;
    pub const INVALIDATE_REF_FRAMES: u16 = 0x0301;
    pub const LOSS_STATS: u16 = 0x0201;
    pub const FRAME_STATS: u16 = 0x0204;
    pub const INPUT: u16 = 0x0206;
    pub const PERIODIC_PING: u16 = 0x0200;
    pub const FRAME_FEC_STATUS: u16 = 0x5502;
    pub const LTR_ACK: u16 = 0x0350;
}

/// Message types, host → client.
pub mod to_client {
    pub const RUMBLE: u16 = 0x010b;
    pub const TERMINATION: u16 = 0x0109;
    pub const HDR_MODE: u16 = 0x010e;
    pub const RUMBLE_TRIGGERS: u16 = 0x5500;
    pub const SET_MOTION_EVENT: u16 = 0x5501;
}

/// The termination code for "the host ended the session normally".
pub const TERMINATION_GRACEFUL: u32 = 0x8003_0023;

/// One message from the client, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    RequestIdr,
    /// Frames `first..=last` (frame indexes) were lost.
    InvalidateRefFrames { first: u32, last: u32 },
    Input(Vec<u8>),
    /// Statistics and keep-alives the server needn't act on.
    Ignored(u16),
    Unknown(u16),
}

impl ClientMessage {
    pub fn parse(ptype: u16, payload: &[u8]) -> Self {
        use from_client::*;
        match ptype {
            REQUEST_IDR => Self::RequestIdr,
            INVALIDATE_REF_FRAMES if payload.len() >= 12 => {
                let first = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let last = u32::from_le_bytes(payload[8..12].try_into().unwrap());
                Self::InvalidateRefFrames { first: first.min(last), last: last.max(first) }
            }
            INPUT => Self::Input(payload.to_vec()),
            START_B | LOSS_STATS | FRAME_STATS | PERIODIC_PING | FRAME_FEC_STATUS | LTR_ACK => Self::Ignored(ptype),
            other => Self::Unknown(other),
        }
    }
}

/// Encrypts host messages and decrypts client ones for one session.
pub struct ControlCipher {
    key: Key,
    send_seq: u32,
    /// The highest client sequence seen, and which of the 64 before it arrived (a replay
    /// window: messages on different ENet channels can arrive out of order).
    recv_top: Option<u32>,
    recv_window: u64,
}

impl ControlCipher {
    pub fn new(key: Key) -> Self {
        Self { key, send_seq: 0, recv_top: None, recv_window: 0 }
    }

    /// A host message, ready for an ENet packet.
    pub fn seal(&mut self, ptype: u16, payload: &[u8]) -> Vec<u8> {
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        let mut inner = Vec::with_capacity(4 + payload.len());
        inner.extend_from_slice(&ptype.to_le_bytes());
        inner.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        inner.extend_from_slice(payload);
        let tag = gcm_seal(&self.key, &marked_iv(seq, b'H', b'C'), &mut inner);
        let mut out = Vec::with_capacity(8 + 16 + inner.len());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&((4 + 16 + inner.len()) as u16).to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&inner);
        out
    }

    /// A client message's type and payload.
    pub fn open(&mut self, packet: &[u8]) -> Result<(u16, Vec<u8>)> {
        if packet.len() < 8 + 16 + 4 {
            bail!("control packet of {} bytes is too short", packet.len());
        }
        let kind = u16::from_le_bytes([packet[0], packet[1]]);
        if kind != 1 {
            bail!("control packet type {kind:#x} isn't encrypted");
        }
        let len = usize::from(u16::from_le_bytes([packet[2], packet[3]]));
        if len + 4 != packet.len() {
            bail!("control packet says {len} bytes, has {}", packet.len() - 4);
        }
        let seq = u32::from_le_bytes(packet[4..8].try_into().unwrap());
        let tag: [u8; 16] = packet[8..24].try_into().unwrap();
        let mut inner = packet[24..].to_vec();
        gcm_open(&self.key, &marked_iv(seq, b'C', b'C'), &mut inner, &tag)?;
        self.check_replay(seq)?;
        let ptype = u16::from_le_bytes([inner[0], inner[1]]);
        let plen = usize::from(u16::from_le_bytes([inner[2], inner[3]]));
        if plen + 4 > inner.len() {
            bail!("control message says {plen} payload bytes, has {}", inner.len() - 4);
        }
        Ok((ptype, inner[4..4 + plen].to_vec()))
    }

    fn check_replay(&mut self, seq: u32) -> Result<()> {
        let Some(top) = self.recv_top else {
            self.recv_top = Some(seq);
            self.recv_window = 1;
            return Ok(());
        };
        if seq > top {
            let shift = seq - top;
            self.recv_window = if shift >= 64 { 1 } else { (self.recv_window << shift) | 1 };
            self.recv_top = Some(seq);
            return Ok(());
        }
        let back = top - seq;
        if back >= 64 || self.recv_window & (1 << back) != 0 {
            bail!("replayed control message {seq}");
        }
        self.recv_window |= 1 << back;
        Ok(())
    }
}

/// Host message payloads.
pub fn rumble(slot: u16, low: u16, high: u16) -> Vec<u8> {
    let mut p = vec![0u8; 4];
    p.extend_from_slice(&slot.to_le_bytes());
    p.extend_from_slice(&low.to_le_bytes());
    p.extend_from_slice(&high.to_le_bytes());
    p
}

pub fn rumble_triggers(slot: u16, left: u16, right: u16) -> Vec<u8> {
    let mut p = slot.to_le_bytes().to_vec();
    p.extend_from_slice(&left.to_le_bytes());
    p.extend_from_slice(&right.to_le_bytes());
    p
}

/// Ask the client to send a pad's motion sensors at `rate_hz` (0 stops them).
pub fn set_motion_event(slot: u16, rate_hz: u16, motion_type: u8) -> Vec<u8> {
    let mut p = slot.to_le_bytes().to_vec();
    p.extend_from_slice(&rate_hz.to_le_bytes());
    p.push(motion_type);
    p
}

/// The session ended; `code` is big-endian, and [`TERMINATION_GRACEFUL`] is a normal end.
pub fn termination(code: u32) -> Vec<u8> {
    code.to_be_bytes().to_vec()
}

/// HDR on or off, with the mastering metadata (Sunshine's extended form).
pub fn hdr_mode(enabled: bool, meta: Option<&HdrMetadata>) -> Vec<u8> {
    let mut p = vec![u8::from(enabled)];
    let put = |p: &mut Vec<u8>, v: u16| p.extend_from_slice(&v.to_le_bytes());
    match meta {
        Some(m) => {
            for [x, y] in m.primaries {
                put(&mut p, x);
                put(&mut p, y);
            }
            put(&mut p, m.white_point[0]);
            put(&mut p, m.white_point[1]);
            // Moonlight wants the max in nits and the min in 1/10000 nit; ours are both
            // 1/10000 nit.
            put(&mut p, (m.max_luminance / 10_000).min(u32::from(u16::MAX)) as u16);
            put(&mut p, m.min_luminance.min(u32::from(u16::MAX)) as u16);
            put(&mut p, m.max_cll);
            put(&mut p, m.max_fall);
            put(&mut p, 0);
        }
        None => p.extend_from_slice(&[0u8; 2 * 13]),
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::gcm_seal;

    /// A client message, sealed the way Moonlight seals it.
    pub fn client_seal(key: &Key, seq: u32, ptype: u16, payload: &[u8]) -> Vec<u8> {
        let mut inner = ptype.to_le_bytes().to_vec();
        inner.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        inner.extend_from_slice(payload);
        let tag = gcm_seal(key, &marked_iv(seq, b'C', b'C'), &mut inner);
        let mut out = 1u16.to_le_bytes().to_vec();
        out.extend_from_slice(&((20 + inner.len()) as u16).to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&inner);
        out
    }

    #[test]
    fn open_client_messages() {
        let key = [9u8; 16];
        let mut c = ControlCipher::new(key);
        let (t, p) = c.open(&client_seal(&key, 0, from_client::REQUEST_IDR, &[0, 0])).unwrap();
        assert_eq!(ClientMessage::parse(t, &p), ClientMessage::RequestIdr);
        let mut rfi = Vec::new();
        for v in [10u32, 0, 14, 0, 0, 0] {
            rfi.extend_from_slice(&v.to_le_bytes());
        }
        let (t, p) = c.open(&client_seal(&key, 2, from_client::INVALIDATE_REF_FRAMES, &rfi)).unwrap();
        assert_eq!(ClientMessage::parse(t, &p), ClientMessage::InvalidateRefFrames { first: 10, last: 14 });
        // Out of order within the window is fine; a replay isn't.
        c.open(&client_seal(&key, 1, from_client::PERIODIC_PING, &[0; 8])).unwrap();
        assert!(c.open(&client_seal(&key, 1, from_client::PERIODIC_PING, &[0; 8])).is_err());
        assert!(c.open(&client_seal(&key, 200, from_client::PERIODIC_PING, &[0; 8])).is_ok());
        assert!(c.open(&client_seal(&key, 100, from_client::PERIODIC_PING, &[0; 8])).is_err(), "too old");
        // The host's IV marker isn't the client's.
        let mut bad = client_seal(&key, 300, from_client::INPUT, &[1, 2, 3]);
        bad[30] ^= 1;
        assert!(c.open(&bad).is_err());
    }

    #[test]
    fn seal_host_messages() {
        let key = [4u8; 16];
        let mut c = ControlCipher::new(key);
        let a = c.seal(to_client::RUMBLE, &rumble(1, 0x1000, 0xFFFF));
        let b = c.seal(to_client::TERMINATION, &termination(TERMINATION_GRACEFUL));
        assert_eq!(&a[..2], &[1, 0]);
        assert_eq!(u16::from_le_bytes([a[2], a[3]]) as usize, a.len() - 4);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 1);
        // Decrypt as Moonlight does (host IV marker).
        let tag: [u8; 16] = a[8..24].try_into().unwrap();
        let mut inner = a[24..].to_vec();
        gcm_open(&key, &marked_iv(0, b'H', b'C'), &mut inner, &tag).unwrap();
        assert_eq!(&inner[..4], &[0x0b, 0x01, 10, 0]);
        assert_eq!(&inner[4..], &[0, 0, 0, 0, 1, 0, 0x00, 0x10, 0xFF, 0xFF]);
    }

    #[test]
    fn payload_layouts() {
        assert_eq!(termination(TERMINATION_GRACEFUL), [0x80, 0x03, 0x00, 0x23]);
        assert_eq!(rumble_triggers(2, 1, 0x100), [2, 0, 1, 0, 0, 1]);
        assert_eq!(set_motion_event(0, 100, 2), [0, 0, 100, 0, 2]);
        let m = HdrMetadata {
            primaries: [[34000, 16000], [13250, 34500], [7500, 3000]],
            white_point: [15635, 16450],
            max_luminance: 10_000_000,
            min_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        };
        let p = hdr_mode(true, Some(&m));
        assert_eq!(p.len(), 1 + 26);
        assert_eq!(p[0], 1);
        assert_eq!(u16::from_le_bytes([p[17], p[18]]), 1000, "max display luminance in nits");
        assert_eq!(u16::from_le_bytes([p[19], p[20]]), 50);
        assert_eq!(hdr_mode(false, None), [0u8; 27]);
    }
}
