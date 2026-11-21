//! The control lane: reliable, ordered messages, at a low rate.
//!
//! A message (a [`Control`](quiver_stream_proto::Control) as JSON) is cut into segments of
//! at most [`SEGMENT_BYTES`]; the last has `fin` set. Segments are numbered on the lane. The
//! receiver acks with the next number it wants plus a bitmap of the 64 after it, and the
//! sender resends a segment when its RTO passes or when a later one is acked (a hole). Up to
//! [`WINDOW`] segments are in flight, which lets a 256×256 cursor image through in a burst.
//!
//! Chunk bodies: `Control` is `seq: u32, fin: u8, data`; `ControlAck` is `next: u32,
//! sack: u64`.

use std::collections::{BTreeMap, VecDeque};

use quiver_stream_proto::Micros;

use crate::wire::{Malformed, Reader, Writer};

pub const SEGMENT_BYTES: usize = 1000;
pub const WINDOW: u32 = 256;
/// Bigger messages are refused (a cursor is at most 256 KiB of pixels, as JSON about 1 MiB).
pub const MAX_MESSAGE: usize = 4 << 20;

#[derive(Debug)]
struct OutSeg {
    seq: u32,
    fin: bool,
    data: Vec<u8>,
    sent_at: Option<Micros>,
    acked: bool,
    /// A later segment was acked while this one wasn't: resend without waiting for the RTO.
    hole: bool,
}

#[derive(Debug, Default)]
pub struct ControlSender {
    next_seq: u32,
    /// Every segment not yet acked cumulatively, in order.
    segs: VecDeque<OutSeg>,
    pub retransmits: u64,
}

impl ControlSender {
    pub fn push(&mut self, msg: &[u8]) {
        let mut parts = msg.chunks(SEGMENT_BYTES).peekable();
        if parts.peek().is_none() {
            self.push_seg(Vec::new(), true);
        }
        while let Some(p) = parts.next() {
            self.push_seg(p.to_vec(), parts.peek().is_none());
        }
    }

    fn push_seg(&mut self, data: Vec<u8>, fin: bool) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.segs.push_back(OutSeg { seq, fin, data, sent_at: None, acked: false, hole: false });
    }

    pub fn on_ack(&mut self, next: u32, sack: u64) {
        while self.segs.front().is_some_and(|s| seq_lt(s.seq, next)) {
            self.segs.pop_front();
        }
        let mut highest_acked = None;
        for s in &mut self.segs {
            let off = s.seq.wrapping_sub(next);
            if (1..=64).contains(&off) && sack & (1 << (off - 1)) != 0 {
                s.acked = true;
                highest_acked = Some(s.seq);
            }
        }
        if let Some(h) = highest_acked {
            for s in self.segs.iter_mut().take_while(|s| s.seq != h) {
                if !s.acked && s.sent_at.is_some() {
                    s.hole = true;
                }
            }
        }
    }

    /// The next segment to put on the wire, if any is due: a resend first, then a new one
    /// inside the window.
    pub fn poll(&mut self, now: Micros, rto: Micros) -> Option<(u32, bool, &[u8])> {
        let base = self.segs.front()?.seq;
        let idx = self
            .segs
            .iter()
            .position(|s| !s.acked && s.sent_at.is_some_and(|t| s.hole || now >= t + rto))
            .or_else(|| self.segs.iter().position(|s| s.sent_at.is_none() && s.seq.wrapping_sub(base) < WINDOW))?;
        let s = &mut self.segs[idx];
        if s.sent_at.is_some() {
            self.retransmits += 1;
        }
        s.sent_at = Some(now);
        s.hole = false;
        Some((s.seq, s.fin, &s.data))
    }

    /// When the earliest resend comes due, if anything is in flight.
    pub fn next_timeout(&self, rto: Micros) -> Option<Micros> {
        self.segs.iter().filter(|s| !s.acked).filter_map(|s| s.sent_at).min().map(|t| t + rto)
    }

    /// Whether a segment is waiting to go out now.
    pub fn has_unsent(&self) -> bool {
        let Some(base) = self.segs.front().map(|s| s.seq) else { return false };
        self.segs.iter().any(|s| (s.sent_at.is_none() && s.seq.wrapping_sub(base) < WINDOW) || s.hole)
    }

    pub fn in_flight(&self) -> usize {
        self.segs.len()
    }
}

/// `a < b` in wrapping sequence space.
fn seq_lt(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) > 0
}

#[derive(Debug, Default)]
pub struct ControlReceiver {
    next: u32,
    ooo: BTreeMap<u32, (bool, Vec<u8>)>,
    partial: Vec<u8>,
    /// Something arrived since the last ack went out.
    pub ack_due: bool,
    oversized: bool,
}

impl ControlReceiver {
    /// Takes one segment; returns the messages it completes, in order.
    pub fn on_segment(&mut self, seq: u32, fin: bool, data: &[u8]) -> Vec<Vec<u8>> {
        self.ack_due = true;
        let off = seq.wrapping_sub(self.next);
        if off >= WINDOW {
            return Vec::new();
        }
        self.ooo.entry(seq).or_insert_with(|| (fin, data.to_vec()));
        let mut done = Vec::new();
        while let Some((fin, data)) = self.ooo.remove(&self.next) {
            self.next = self.next.wrapping_add(1);
            if self.partial.len() + data.len() > MAX_MESSAGE {
                self.oversized = true;
                self.partial.clear();
            } else if !self.oversized {
                self.partial.extend_from_slice(&data);
            }
            if fin {
                if !std::mem::take(&mut self.oversized) {
                    done.push(std::mem::take(&mut self.partial));
                }
                self.partial.clear();
            }
        }
        done
    }

    /// `(next, sack)`: the next segment wanted, and which of the 64 after it are held.
    pub fn ack(&mut self) -> (u32, u64) {
        self.ack_due = false;
        let mut sack = 0u64;
        for &seq in self.ooo.keys() {
            let off = seq.wrapping_sub(self.next);
            if (1..=64).contains(&off) {
                sack |= 1 << (off - 1);
            }
        }
        (self.next, sack)
    }
}

pub fn put_segment(w: &mut Writer<'_>, seq: u32, fin: bool, data: &[u8]) {
    w.u32(seq).u8(fin as u8).bytes(data);
}

pub fn get_segment(body: &[u8]) -> Result<(u32, bool, &[u8]), Malformed> {
    let mut r = Reader::new(body);
    Ok((r.u32()?, r.u8()? & 1 != 0, r.rest()))
}

pub fn put_ack(w: &mut Writer<'_>, next: u32, sack: u64) {
    w.u32(next).u64(sack);
}

pub fn get_ack(body: &[u8]) -> Result<(u32, u64), Malformed> {
    let mut r = Reader::new(body);
    Ok((r.u32()?, r.u64()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moves segments from `tx` to `rx`, dropping those `drop` picks, then acks back.
    fn pump(tx: &mut ControlSender, rx: &mut ControlReceiver, now: Micros, mut drop: impl FnMut(u32) -> bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut sent = Vec::new();
        while let Some((seq, fin, data)) = tx.poll(now, 10_000) {
            sent.push((seq, fin, data.to_vec()));
        }
        for (seq, fin, data) in sent {
            if !drop(seq) {
                out.extend(rx.on_segment(seq, fin, &data));
            }
        }
        if rx.ack_due {
            let (n, s) = rx.ack();
            tx.on_ack(n, s);
        }
        out
    }

    #[test]
    fn messages_arrive_whole_and_in_order_despite_loss() {
        let mut tx = ControlSender::default();
        let mut rx = ControlReceiver::default();
        let big: Vec<u8> = (0..5_500u32).map(|i| i as u8).collect();
        tx.push(b"one");
        tx.push(&big);
        tx.push(b"");
        tx.push(b"three");
        let mut got = pump(&mut tx, &mut rx, 0, |seq| seq == 2 || seq == 5);
        assert_eq!(got, vec![b"one".to_vec()], "held back behind the hole");
        // The holes are known from the sack, so they go again at once, before the RTO.
        got.extend(pump(&mut tx, &mut rx, 1, |_| false));
        assert_eq!(got, vec![b"one".to_vec(), big, Vec::new(), b"three".to_vec()]);
        assert_eq!(tx.in_flight(), 0);
        assert_eq!(tx.retransmits, 2);
    }

    #[test]
    fn a_lost_tail_is_resent_after_the_rto() {
        let mut tx = ControlSender::default();
        let mut rx = ControlReceiver::default();
        tx.push(b"x");
        assert!(pump(&mut tx, &mut rx, 0, |_| true).is_empty());
        assert_eq!(tx.next_timeout(10_000), Some(10_000));
        assert!(pump(&mut tx, &mut rx, 5_000, |_| false).is_empty(), "not yet due");
        assert_eq!(pump(&mut tx, &mut rx, 10_000, |_| false), vec![b"x".to_vec()]);
    }

    #[test]
    fn duplicates_are_ignored() {
        let mut rx = ControlReceiver::default();
        assert_eq!(rx.on_segment(0, true, b"a").len(), 1);
        assert!(rx.on_segment(0, true, b"a").is_empty());
        assert_eq!(rx.ack(), (1, 0));
    }

    #[test]
    fn sequence_comparison_wraps() {
        assert!(seq_lt(u32::MAX, 0));
        assert!(!seq_lt(0, u32::MAX));
    }
}
