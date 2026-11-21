//! Transport-wide receive feedback, client → host, for the bandwidth estimator.
//!
//! The client notes the arrival time of every host datagram and, every
//! [`FEEDBACK_INTERVAL`], sends the list: packet numbers and arrival times. The host matches
//! them with its send times to see the queue grow or drain (delay-based control). Each
//! report also carries two counters, the packets received so far and the highest number
//! seen, so the loss rate stays right even when a report itself is lost.
//!
//! Chunk body: `received: u64, highest: u64, n: u16`, then if `n > 0` the first entry as
//! `pn: u64, at: u64`, then `n - 1` entries as `pn delta: varint, at delta: svarint (µs)`,
//! sorted by packet number.

use quiver_stream_proto::Micros;

use crate::wire::{Malformed, Reader, Writer};

/// How often the client reports.
pub const FEEDBACK_INTERVAL: Micros = 10_000;
/// The most entries in one chunk; more go in the next chunk.
pub const MAX_ENTRIES: usize = 300;

/// One report, decoded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Feedback {
    pub received: u64,
    pub highest: u64,
    /// `(packet number, arrival time on the client's clock)`, sorted by packet number.
    pub entries: Vec<(u64, Micros)>,
}

#[derive(Debug, Default)]
pub struct FeedbackBuilder {
    received: u64,
    highest: u64,
    pending: Vec<(u64, Micros)>,
    last_sent: Micros,
}

impl FeedbackBuilder {
    pub fn on_packet(&mut self, pn: u64, now: Micros) {
        self.received += 1;
        self.highest = self.highest.max(pn);
        self.pending.push((pn, now));
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn highest(&self) -> u64 {
        self.highest
    }

    pub fn due(&self, now: Micros) -> bool {
        !self.pending.is_empty() && (now >= self.last_sent + FEEDBACK_INTERVAL || self.pending.len() >= MAX_ENTRIES)
    }

    pub fn next_timeout(&self) -> Option<Micros> {
        (!self.pending.is_empty()).then_some(self.last_sent + FEEDBACK_INTERVAL)
    }

    /// Writes one chunk body with up to [`MAX_ENTRIES`] of the pending entries.
    pub fn write_chunk(&mut self, out: &mut Vec<u8>, now: Micros) {
        self.last_sent = now;
        self.pending.sort_unstable_by_key(|e| e.0);
        let n = self.pending.len().min(MAX_ENTRIES);
        let mut w = Writer::new(out);
        w.u64(self.received).u64(self.highest).u16(n as u16);
        let mut prev: Option<(u64, Micros)> = None;
        for &(pn, at) in &self.pending[..n] {
            match prev {
                None => w.u64(pn).u64(at),
                Some((ppn, pat)) => w.varint(pn - ppn).svarint(at as i64 - pat as i64),
            };
            prev = Some((pn, at));
        }
        self.pending.drain(..n);
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

pub fn parse(body: &[u8]) -> Result<Feedback, Malformed> {
    let mut r = Reader::new(body);
    let received = r.u64()?;
    let highest = r.u64()?;
    let n = r.u16()? as usize;
    let mut entries = Vec::with_capacity(n);
    if n > 0 {
        let mut pn = r.u64()?;
        let mut at = r.u64()?;
        entries.push((pn, at));
        for _ in 1..n {
            pn = pn.checked_add(r.varint()?).ok_or(Malformed)?;
            at = at.checked_add_signed(r.svarint()?).ok_or(Malformed)?;
            entries.push((pn, at));
        }
    }
    Ok(Feedback { received, highest, entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_round_trip_sorted() {
        let mut b = FeedbackBuilder::default();
        b.on_packet(3, 1_000);
        b.on_packet(1, 1_200);
        b.on_packet(7, 900);
        assert!(!b.due(5_000));
        assert!(b.due(FEEDBACK_INTERVAL));
        let mut out = Vec::new();
        b.write_chunk(&mut out, FEEDBACK_INTERVAL);
        let f = parse(&out).unwrap();
        assert_eq!(f, Feedback { received: 3, highest: 7, entries: vec![(1, 1_200), (3, 1_000), (7, 900)] });
        assert_eq!(b.pending(), 0);
        assert!(out.len() <= 40, "{} bytes", out.len());
    }

    #[test]
    fn long_lists_split_across_chunks() {
        let mut b = FeedbackBuilder::default();
        for pn in 1..=(MAX_ENTRIES as u64 + 5) {
            b.on_packet(pn, pn * 10);
        }
        assert!(b.due(0));
        let mut out = Vec::new();
        b.write_chunk(&mut out, 0);
        assert_eq!(parse(&out).unwrap().entries.len(), MAX_ENTRIES);
        assert_eq!(b.pending(), 5);
        assert!(out.len() < 1100, "{} bytes", out.len());
    }
}
