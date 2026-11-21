//! The input lane: client → host, reliable without waiting.
//!
//! Every event gets a sequence number and goes out at once. Until the host acks it, it also
//! rides in each of the client's next [`InputOpts::redundancy`] datagrams, whatever they
//! carry, so a lost datagram is usually covered by the next one with no round trip. If the
//! ack still hasn't come one RTO later, the event goes again. The host applies events once,
//! in order, and acks the highest number it has applied.
//!
//! Each pad's whole state is also re-sent every [`InputOpts::pad_refresh_us`] while the pad
//! is plugged in (proto `InputEvent::Pad`), so the host's virtual pad can't drift from the
//! real one for longer than that.
//!
//! Chunk bodies: `Input` is `count: u8` then that many [`codec::put_stamped`] events;
//! `InputAck` is `through: u64`.

use std::collections::{BTreeMap, VecDeque};

use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{InputEvent, Micros, PadState};

use crate::codec;
use crate::wire::{Malformed, Reader, Writer};

#[derive(Debug, Clone)]
pub struct InputOpts {
    /// How many datagrams in a row carry each event before the sender waits for its ack.
    pub redundancy: u8,
    /// How often a plugged-in pad's state is sent again even if it didn't change. 0 turns
    /// the refresh off.
    pub pad_refresh_us: Micros,
}

impl Default for InputOpts {
    fn default() -> Self {
        Self { redundancy: 3, pad_refresh_us: 100_000 }
    }
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    wire: Vec<u8>,
    sends: u8,
    last_sent: Option<Micros>,
}

#[derive(Debug)]
pub struct InputSender {
    opts: InputOpts,
    next_seq: u64,
    unacked: VecDeque<Pending>,
    /// Plugged-in pads: their last state and when it was last queued.
    pads: BTreeMap<u8, (PadState, Micros)>,
    /// An event was queued since the last datagram went out.
    fresh: bool,
    pub sent_events: u64,
    pub resends: u64,
}

impl InputSender {
    pub fn new(opts: InputOpts) -> Self {
        Self { opts, next_seq: 1, unacked: VecDeque::new(), pads: BTreeMap::new(), fresh: false, sent_events: 0, resends: 0 }
    }

    /// Queues an event captured at `at`; returns its sequence number.
    pub fn push(&mut self, event: InputEvent, at: Micros, now: Micros) -> u64 {
        match &event {
            InputEvent::Pad { slot, state } => {
                self.pads.insert(*slot, (*state, now));
            }
            InputEvent::PadArrive { slot, .. } => {
                self.pads.insert(*slot, (PadState::default(), now));
            }
            InputEvent::PadLeave { slot } => {
                self.pads.remove(slot);
            }
            _ => {}
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut wire = Vec::new();
        codec::put_stamped(&mut Writer::new(&mut wire), &Stamped { seq, at, event });
        self.unacked.push_back(Pending { seq, wire, sends: 0, last_sent: None });
        self.fresh = true;
        seq
    }

    pub fn on_ack(&mut self, through: u64) {
        while self.unacked.front().is_some_and(|p| p.seq <= through) {
            self.unacked.pop_front();
        }
    }

    /// Queues a refresh of every pad whose state hasn't gone out for a refresh period.
    pub fn refresh_pads(&mut self, now: Micros) {
        if self.opts.pad_refresh_us == 0 {
            return;
        }
        let due: Vec<(u8, PadState)> = self
            .pads
            .iter()
            .filter(|(_, (_, t))| now >= t + self.opts.pad_refresh_us)
            .map(|(slot, (state, _))| (*slot, *state))
            .collect();
        for (slot, state) in due {
            self.push(InputEvent::Pad { slot, state }, now, now);
        }
    }

    /// Whether a datagram should go out now just for input: a new event, or an unacked one
    /// whose RTO passed.
    pub fn wants_send(&self, now: Micros, rto: Micros) -> bool {
        self.fresh || self.unacked.iter().any(|p| p.last_sent.is_some_and(|t| now >= t + rto) && p.sends > 0)
    }

    /// When an unacked event's RTO or a pad refresh comes due.
    pub fn next_timeout(&self, rto: Micros) -> Option<Micros> {
        let resend = self.unacked.iter().filter_map(|p| p.last_sent).min().map(|t| t + rto);
        let refresh =
            (self.opts.pad_refresh_us > 0).then(|| self.pads.values().map(|(_, t)| t + self.opts.pad_refresh_us).min()).flatten();
        [resend, refresh].into_iter().flatten().min()
    }

    /// Writes the events this datagram should carry, at most `budget` bytes of them, as an
    /// `Input` chunk body. Returns false (writing nothing) if there are none.
    pub fn write_chunk(&mut self, out: &mut Vec<u8>, now: Micros, rto: Micros, budget: usize) -> bool {
        let redundancy = self.opts.redundancy.max(1);
        let start = out.len();
        out.push(0);
        let mut count = 0u8;
        for p in &mut self.unacked {
            let timed_out = p.last_sent.is_some_and(|t| now >= t + rto);
            if p.sends >= redundancy && !timed_out {
                continue;
            }
            if out.len() - start + p.wire.len() > budget || count == u8::MAX {
                break;
            }
            out.extend_from_slice(&p.wire);
            count += 1;
            if p.sends == 0 {
                self.sent_events += 1;
            } else if timed_out {
                self.resends += 1;
                p.sends = 0;
            }
            p.sends = p.sends.saturating_add(1);
            p.last_sent = Some(now);
        }
        self.fresh = false;
        if count == 0 {
            out.truncate(start);
            return false;
        }
        out[start] = count;
        true
    }

    pub fn unacked(&self) -> usize {
        self.unacked.len()
    }
}

/// The host's side: applies each event once and in order.
#[derive(Debug)]
pub struct InputReceiver {
    next: u64,
    held: BTreeMap<u64, Stamped>,
    /// The ack should go out: something arrived since the last one.
    pub ack_due: bool,
    pub duplicates: u64,
}

impl Default for InputReceiver {
    fn default() -> Self {
        Self { next: 1, held: BTreeMap::new(), ack_due: false, duplicates: 0 }
    }
}

/// Events further ahead of the next one wanted than this are dropped (the sender resends).
const MAX_HELD: u64 = 4096;

impl InputReceiver {
    /// Takes an `Input` chunk body; returns the events now ready to apply, in order.
    pub fn on_chunk(&mut self, body: &[u8]) -> Result<Vec<Stamped>, Malformed> {
        let mut r = Reader::new(body);
        let count = r.u8()?;
        let mut ready = Vec::new();
        for _ in 0..count {
            let s = codec::get_stamped(&mut r)?;
            self.ack_due = true;
            if s.seq < self.next || self.held.contains_key(&s.seq) {
                self.duplicates += 1;
                continue;
            }
            if s.seq - self.next >= MAX_HELD {
                continue;
            }
            self.held.insert(s.seq, s);
        }
        while let Some(s) = self.held.remove(&self.next) {
            self.next += 1;
            ready.push(s);
        }
        Ok(ready)
    }

    /// The highest sequence number applied.
    pub fn acked_through(&mut self) -> u64 {
        self.ack_due = false;
        self.next - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16) -> InputEvent {
        InputEvent::Key { code, down: true }
    }

    fn chunk(tx: &mut InputSender, now: Micros) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        tx.write_chunk(&mut out, now, 20_000, 1100).then_some(out)
    }

    #[test]
    fn each_event_rides_in_the_next_datagrams_until_acked() {
        let mut tx = InputSender::new(InputOpts { redundancy: 3, pad_refresh_us: 0 });
        let mut rx = InputReceiver::default();
        tx.push(key(1), 0, 0);
        let first = chunk(&mut tx, 0).unwrap();
        tx.push(key(2), 0, 1);
        let second = chunk(&mut tx, 1).unwrap();
        assert_eq!(second[0], 2, "the second datagram carries both");
        // The first datagram is lost; the second alone delivers both, in order.
        drop(first);
        let got = rx.on_chunk(&second).unwrap();
        assert_eq!(got.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![1, 2]);
        tx.on_ack(rx.acked_through());
        assert_eq!(tx.unacked(), 0);
        assert!(chunk(&mut tx, 2).is_none());
    }

    #[test]
    fn redundancy_runs_out_then_the_rto_resends() {
        let mut tx = InputSender::new(InputOpts { redundancy: 2, pad_refresh_us: 0 });
        tx.push(key(1), 0, 0);
        assert!(chunk(&mut tx, 0).is_some());
        assert!(chunk(&mut tx, 1).is_some());
        assert!(chunk(&mut tx, 2).is_none(), "sent twice, now waiting for the ack");
        assert!(!tx.wants_send(10_000, 20_000));
        assert!(tx.wants_send(20_001, 20_000));
        assert!(chunk(&mut tx, 20_001).is_some());
        assert_eq!(tx.resends, 1);
    }

    #[test]
    fn receiver_applies_once_in_order() {
        let mut tx = InputSender::new(InputOpts { redundancy: 1, pad_refresh_us: 0 });
        let mut rx = InputReceiver::default();
        let chunks: Vec<_> = (1..=3)
            .map(|i| {
                tx.push(key(i), 0, 0);
                chunk(&mut tx, 0).unwrap()
            })
            .collect();
        assert!(rx.on_chunk(&chunks[2]).unwrap().is_empty(), "held until 1 and 2 arrive");
        assert_eq!(rx.on_chunk(&chunks[0]).unwrap().len(), 1);
        assert_eq!(rx.on_chunk(&chunks[1]).unwrap().iter().map(|s| s.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert!(rx.on_chunk(&chunks[1]).unwrap().is_empty());
        assert_eq!(rx.duplicates, 1);
        assert_eq!(rx.acked_through(), 3);
    }

    #[test]
    fn pads_are_refreshed_until_they_leave() {
        let mut tx = InputSender::new(InputOpts { redundancy: 1, pad_refresh_us: 100 });
        let state = PadState { lx: 900, ..Default::default() };
        tx.push(InputEvent::Pad { slot: 0, state }, 0, 0);
        assert_eq!(tx.next_timeout(1_000_000), Some(100));
        tx.refresh_pads(50);
        assert_eq!(tx.unacked(), 1);
        tx.refresh_pads(100);
        assert_eq!(tx.unacked(), 2);
        tx.push(InputEvent::PadLeave { slot: 0 }, 0, 150);
        tx.refresh_pads(1_000);
        assert_eq!(tx.unacked(), 3);
    }
}
