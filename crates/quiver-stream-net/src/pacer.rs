//! The pacer: spreads a frame's datagrams over part of the frame interval instead of
//! sending them back to back at line rate.
//!
//! A burst of a whole frame at 10 Gbit/s into a 1 Gbit/s switch port or a Wi-Fi AP fills
//! its queue and drops the tail; spread out, the same frame passes untouched. Each frame
//! is given a rate when it is queued (see `HostCore`), and its datagrams leave at that
//! rate, a few at a time ([`Pacer::BURST_US`] worth) so GSO can still batch them. Resends
//! for NACKs jump the queue. Only video is paced: input, audio and control are small and
//! go at once.

use std::collections::VecDeque;
use std::sync::Arc;

use quiver_stream_proto::Micros;

use crate::video::ShardedFrame;

#[derive(Debug, Clone)]
pub struct Paced {
    pub frame: Arc<ShardedFrame>,
    pub index: u16,
    pub resend: bool,
}

#[derive(Debug)]
struct Entry {
    item: Paced,
    bytes: usize,
    rate: f64,
    queued: Micros,
}

#[derive(Debug, Default)]
pub struct Pacer {
    queue: VecDeque<Entry>,
    next_at: Micros,
    queued_bytes: usize,
}

impl Pacer {
    /// Up to this much sending time goes out in one go.
    pub const BURST_US: Micros = 250;

    /// Queues every shard of a frame, data first, to leave at `rate` bps. `datagram` is
    /// the size of each on the wire.
    pub fn push_frame(&mut self, frame: &Arc<ShardedFrame>, datagram: usize, rate: f64, now: Micros) {
        for index in 0..frame.total() {
            self.queue.push_back(Entry {
                item: Paced { frame: frame.clone(), index: index as u16, resend: false },
                bytes: datagram,
                rate,
                queued: now,
            });
            self.queued_bytes += datagram;
        }
    }

    /// Queues resends ahead of everything else.
    pub fn push_resends(&mut self, frame: &Arc<ShardedFrame>, indices: &[u16], datagram: usize, rate: f64, now: Micros) {
        for &index in indices.iter().rev() {
            self.queue.push_front(Entry {
                item: Paced { frame: frame.clone(), index, resend: true },
                bytes: datagram,
                rate,
                queued: now,
            });
            self.queued_bytes += datagram;
        }
    }

    /// The next datagram, if one may leave now.
    pub fn poll(&mut self, now: Micros) -> Option<Paced> {
        self.queue.front()?;
        if now < self.next_at {
            return None;
        }
        let e = self.queue.pop_front().expect("head exists");
        self.queued_bytes -= e.bytes;
        let base = self.next_at.max(now.saturating_sub(Self::BURST_US));
        self.next_at = base + (e.bytes as f64 * 8e6 / e.rate.max(1e5)) as Micros;
        Some(e.item)
    }

    /// When the next datagram may leave, if any is queued.
    pub fn next_at(&self) -> Option<Micros> {
        (!self.queue.is_empty()).then_some(self.next_at)
    }

    /// How long the oldest queued datagram has waited.
    pub fn delay(&self, now: Micros) -> Micros {
        self.queue.front().map_or(0, |e| now.saturating_sub(e.queued))
    }

    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::FecPlanner;
    use crate::video::VideoSender;
    use quiver_stream_proto::EncodedFrame;

    fn frame(len: usize) -> Arc<ShardedFrame> {
        let f = EncodedFrame { seq: 1, captured: 0, encoded: 0, keyframe: false, refs_from_seq: 0, width: 1, height: 1, data: vec![1; len] };
        VideoSender::new(FecPlanner::new(1e-4, 0.5, 0)).shard(f, 1184, 0).unwrap()
    }

    #[test]
    fn a_frame_leaves_at_its_rate() {
        let mut p = Pacer::default();
        let f = frame(1184 * 10 - 33);
        assert_eq!(f.total(), 10);
        // 1200-byte datagrams at 96 Mbit/s: one every 100 µs.
        p.push_frame(&f, 1200, 96e6, 0);
        let mut times = Vec::new();
        let mut now = 0;
        while times.len() < 10 {
            if p.poll(now).is_some() {
                times.push(now);
            } else {
                now = p.next_at().unwrap().max(now + 1);
            }
        }
        assert_eq!(times.last().copied(), Some(900));
        assert!(p.poll(now).is_none());
    }

    #[test]
    fn resends_jump_the_queue() {
        let mut p = Pacer::default();
        let f = frame(5000);
        p.push_frame(&f, 1200, 1e9, 0);
        p.push_resends(&f, &[3, 4], 1200, 1e9, 0);
        let first = p.poll(0).unwrap();
        assert!(first.resend && first.index == 3);
        assert_eq!(p.queued_bytes(), (f.total() + 1) * 1200);
    }

    #[test]
    fn idle_time_gives_only_a_small_burst() {
        let mut p = Pacer::default();
        let f = frame(1184 * 20 - 33);
        p.push_frame(&f, 1200, 9.6e6, 1_000_000);
        // 1 ms per datagram; after a long idle only BURST_US of credit.
        assert!(p.poll(1_000_000).is_some());
        assert!(p.poll(1_000_000).is_none());
    }
}
