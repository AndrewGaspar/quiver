//! Lost frames: what to decode after a gap, and what to tell the host.
//!
//! A frame is decodable when it is a keyframe, or when nothing it may depend on is missing:
//! every frame from its `refs_from_seq` up to it was decoded. The tracker keeps one number
//! for that, `broken_through`: the newest frame that wasn't decoded (lost, skipped or
//! failed). A frame with `refs_from_seq > broken_through` is clean. A skipped frame is
//! broken in turn, so a later frame that refers to it is skipped too.
//!
//! What the host hears ([`Control`]):
//! - `LostFrames { since_seq, through_seq }` once per loss (a notice, an unannounced jump in
//!   `seq`, or a decode error), so it can invalidate the reference or code from an older one.
//! - `Keyframe` before the first keyframe arrives, and when decoding has stayed broken for
//!   [`KEYFRAME_AFTER`] despite the LostFrames (the host couldn't recover by reference);
//!   then at most every [`KEYFRAME_EVERY`], so a bad link doesn't become a keyframe storm.

use std::time::{Duration, Instant};

use quiver_stream_proto::Control;

/// How long decoding may stay broken after a loss before the client asks for a keyframe.
pub const KEYFRAME_AFTER: Duration = Duration::from_millis(100);
/// The least time between two keyframe requests.
pub const KEYFRAME_EVERY: Duration = Duration::from_millis(500);

/// What a frame needs from the tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub seq: u64,
    pub keyframe: bool,
    pub refs_from_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Decode,
    /// It depends on a frame that wasn't decoded.
    Skip,
    /// Older than what was already handled (a late duplicate): ignore it.
    Stale,
}

#[derive(Debug, Default)]
pub struct GapTracker {
    /// The seq the next frame should have.
    next: Option<u64>,
    /// A keyframe has been decoded since the start (or since a decoder reset).
    started: bool,
    broken_through: Option<u64>,
    /// When decoding went broken (None: it's clean).
    broken_since: Option<Instant>,
    last_keyframe_ask: Option<Instant>,
    /// Counters for the stats.
    pub lost: u32,
    pub skipped: u32,
}

impl GapTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start over, waiting for a keyframe (the decoder was recreated). Nothing is asked for
    /// yet: the host starts a stream (and a codec change) on a keyframe, and a frame that
    /// isn't one asks ([`GapTracker::on_frame`]).
    pub fn reset(&mut self, now: Instant) {
        self.started = false;
        self.broken_through = None;
        self.broken_since = Some(now);
    }

    fn mark_broken(&mut self, through: u64, now: Instant) {
        self.broken_through = Some(self.broken_through.map_or(through, |b| b.max(through)));
        self.broken_since.get_or_insert(now);
    }

    fn ask_keyframe(&mut self, now: Instant) -> Option<Control> {
        if self.last_keyframe_ask.is_some_and(|t| now.duration_since(t) < KEYFRAME_EVERY) {
            return None;
        }
        self.last_keyframe_ask = Some(now);
        Some(Control::Keyframe)
    }

    /// The transport says frames `first..=last` won't arrive.
    pub fn on_lost(&mut self, first: u64, last: u64, now: Instant) -> Vec<Control> {
        let first = self.next.map_or(first, |n| first.max(n));
        if last < first {
            return Vec::new();
        }
        self.next = Some(last + 1);
        self.lost += (last - first + 1).min(u32::MAX as u64) as u32;
        self.mark_broken(last, now);
        if !self.started {
            return Vec::new();
        }
        vec![Control::LostFrames { since_seq: first, through_seq: last }]
    }

    /// A frame arrived: decode it or not, and what to tell the host.
    pub fn on_frame(&mut self, f: FrameMeta, now: Instant) -> (Verdict, Vec<Control>) {
        let mut out = Vec::new();
        if let Some(next) = self.next {
            if f.seq < next {
                return (Verdict::Stale, out);
            }
            if f.seq > next {
                // A jump nobody announced.
                out.extend(self.on_lost(next, f.seq - 1, now));
            }
        }
        self.next = Some(f.seq + 1);
        let clean = self.broken_through.is_none_or(|b| f.refs_from_seq > b);
        let verdict = if f.keyframe || (self.started && clean) {
            if f.keyframe {
                self.started = true;
                self.broken_through = None;
            }
            self.broken_since = None;
            Verdict::Decode
        } else {
            self.skipped += 1;
            self.mark_broken(f.seq, now);
            if !self.started {
                out.extend(self.ask_keyframe(now));
            }
            Verdict::Skip
        };
        out.extend(self.tick(now));
        (verdict, out)
    }

    /// The decoder rejected a frame.
    pub fn on_decode_error(&mut self, seq: u64, now: Instant) -> Vec<Control> {
        self.mark_broken(seq, now);
        vec![Control::LostFrames { since_seq: seq, through_seq: seq }]
    }

    /// Called now and then (each frame, and on a timer): ask for a keyframe once decoding
    /// has been broken for too long.
    pub fn tick(&mut self, now: Instant) -> Option<Control> {
        let since = self.broken_since?;
        if now.duration_since(since) < KEYFRAME_AFTER && self.started {
            return None;
        }
        self.ask_keyframe(now)
    }

    /// Decoding is waiting for a frame it can use.
    pub fn is_broken(&self) -> bool {
        self.broken_since.is_some() || !self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seq: u64) -> FrameMeta {
        FrameMeta { seq, keyframe: true, refs_from_seq: seq }
    }
    /// A P-frame that depends on everything since `refs`.
    fn p(seq: u64, refs: u64) -> FrameMeta {
        FrameMeta { seq, keyframe: false, refs_from_seq: refs }
    }

    #[test]
    fn waits_for_the_first_keyframe() {
        let t0 = Instant::now();
        let mut g = GapTracker::new();
        let (v, c) = g.on_frame(p(5, 0), t0);
        assert_eq!(v, Verdict::Skip);
        assert_eq!(c, vec![Control::Keyframe]);
        // Not again right away.
        assert_eq!(g.on_frame(p(6, 0), t0 + Duration::from_millis(10)), (Verdict::Skip, vec![]));
        assert_eq!(g.on_frame(key(7), t0 + Duration::from_millis(20)).0, Verdict::Decode);
        assert_eq!(g.on_frame(p(8, 7), t0 + Duration::from_millis(30)), (Verdict::Decode, vec![]));
        assert!(!g.is_broken());
    }

    #[test]
    fn a_loss_skips_dependents_until_a_frame_refs_past_it() {
        let t0 = Instant::now();
        let mut g = GapTracker::new();
        g.on_frame(key(0), t0);
        g.on_frame(p(1, 0), t0);
        let c = g.on_lost(2, 3, t0);
        assert_eq!(c, vec![Control::LostFrames { since_seq: 2, through_seq: 3 }]);
        // Still a chain from the keyframe: depends on 2 and 3.
        assert_eq!(g.on_frame(p(4, 0), t0).0, Verdict::Skip);
        // Depends on the skipped 4: skip.
        assert_eq!(g.on_frame(p(5, 4), t0).0, Verdict::Skip);
        // The host invalidated: refs from 1 on would include the lost frames... still broken.
        assert_eq!(g.on_frame(p(6, 1), t0).0, Verdict::Skip);
        // Coded against nothing at or before 6: clean.
        assert_eq!(g.on_frame(p(7, 7), t0).0, Verdict::Decode);
        assert_eq!(g.on_frame(p(8, 7), t0).0, Verdict::Decode);
        assert_eq!(g.lost, 2);
        assert_eq!(g.skipped, 3);
        assert!(!g.is_broken());
    }

    #[test]
    fn an_unannounced_jump_is_a_loss() {
        let t0 = Instant::now();
        let mut g = GapTracker::new();
        g.on_frame(key(10), t0);
        let (v, c) = g.on_frame(p(13, 10), t0);
        assert_eq!(v, Verdict::Skip);
        assert_eq!(c, vec![Control::LostFrames { since_seq: 11, through_seq: 12 }]);
        // A late duplicate is ignored, not decoded.
        assert_eq!(g.on_frame(p(12, 10), t0).0, Verdict::Stale);
    }

    #[test]
    fn a_notice_overlapping_what_arrived_is_trimmed() {
        let t0 = Instant::now();
        let mut g = GapTracker::new();
        g.on_frame(key(0), t0);
        g.on_frame(p(1, 0), t0);
        assert_eq!(g.on_lost(0, 1, t0), vec![]);
        assert_eq!(g.on_lost(1, 2, t0), vec![Control::LostFrames { since_seq: 2, through_seq: 2 }]);
    }

    #[test]
    fn keyframe_only_after_staying_broken_and_rate_limited() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut g = GapTracker::new();
        g.on_frame(key(0), t0);
        g.on_lost(1, 1, t0);
        assert_eq!(g.on_frame(p(2, 0), t0 + ms(50)), (Verdict::Skip, vec![]));
        let (_, c) = g.on_frame(p(3, 0), t0 + ms(120));
        assert_eq!(c, vec![Control::Keyframe]);
        assert_eq!(g.tick(t0 + ms(300)), None);
        assert_eq!(g.tick(t0 + ms(700)), Some(Control::Keyframe));
        assert_eq!(g.on_frame(key(4), t0 + ms(710)).0, Verdict::Decode);
        assert_eq!(g.tick(t0 + ms(2000)), None);
    }

    #[test]
    fn decode_errors_are_losses() {
        let t0 = Instant::now();
        let mut g = GapTracker::new();
        g.on_frame(key(0), t0);
        assert_eq!(g.on_frame(p(1, 0), t0).0, Verdict::Decode);
        assert_eq!(g.on_decode_error(1, t0), vec![Control::LostFrames { since_seq: 1, through_seq: 1 }]);
        assert_eq!(g.on_frame(p(2, 0), t0).0, Verdict::Skip);
        assert_eq!(g.on_frame(p(3, 2), t0).0, Verdict::Skip);
        assert_eq!(g.on_frame(p(4, 4), t0).0, Verdict::Decode);
    }

    #[test]
    fn reset_waits_for_a_keyframe_again() {
        let t0 = Instant::now();
        let mut g = GapTracker::new();
        g.on_frame(key(0), t0);
        g.reset(t0);
        assert_eq!(g.on_frame(p(1, 1), t0), (Verdict::Skip, vec![Control::Keyframe]));
        assert_eq!(g.on_frame(key(2), t0).0, Verdict::Decode);
    }
}
