//! Throughput history: every [`INTERVAL`], each queue item that has run since quiverd
//! started gets a sample: the bytes its run received from the network (or hashed, while
//! verifying) in that interval, from the installer's counters rather than the smoothed
//! `speed`. Items that aren't running get gap samples (paused, waiting), so a pause shows
//! as a hole rather than as zeros. The last [`THROUGHPUT_KEEP`] samples are kept per item
//! and go when it leaves the queue.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use quiver_api::GameRef;
use quiver_api::downloads::{SampleState, THROUGHPUT_KEEP, Throughput, ThroughputSample};
use quiver_core::install::Progress;

pub(crate) const INTERVAL: Duration = Duration::from_secs(1);

/// One queue item's samples.
pub(crate) struct History {
    samples: VecDeque<ThroughputSample>,
    /// When the newest sample ended.
    at: DateTime<Utc>,
    last_tick: Instant,
    /// The run sampled last, with its counters as of then.
    run: Option<Base>,
}

struct Base {
    progress: Arc<Progress>,
    received: u64,
    hashed: u64,
    retries: u64,
}

impl History {
    /// A tick for a running item. A new run's counters start from zero, so its first
    /// sample holds what came in since the previous tick.
    pub(crate) fn running(this: &mut Option<History>, progress: &Arc<Progress>, verifying: bool, now: Instant, wall: DateTime<Utc>) {
        let h = this.get_or_insert_with(|| History { samples: VecDeque::new(), at: wall, last_tick: now.checked_sub(INTERVAL).unwrap_or(now), run: None });
        let (received, hashed, retries) =
            (progress.received.load(Ordering::Relaxed), progress.hashed.load(Ordering::Relaxed), progress.retries.load(Ordering::Relaxed));
        let (r0, h0, t0) = match &h.run {
            Some(b) if Arc::ptr_eq(&b.progress, progress) => (b.received, b.hashed, b.retries),
            _ => (0, 0, 0),
        };
        let moved = if verifying { hashed.saturating_sub(h0) } else { received.saturating_sub(r0) };
        // Scaled to one interval when a tick came late.
        let elapsed = now.saturating_duration_since(h.last_tick).max(Duration::from_millis(1));
        let bytes = (moved as u128 * INTERVAL.as_nanos() / elapsed.as_nanos()) as u64;
        let state = if verifying {
            SampleState::Verifying
        } else if progress.retrying.load(Ordering::Relaxed) > 0 {
            SampleState::Retrying
        } else if bytes == 0 {
            SampleState::Stalled
        } else {
            SampleState::Downloading
        };
        h.push(ThroughputSample { bytes, state, retries: retries.saturating_sub(t0) }, now, wall);
        h.run = Some(Base { progress: progress.clone(), received, hashed, retries });
    }

    /// A tick for an item that isn't running: a gap.
    pub(crate) fn idle(&mut self, state: SampleState, now: Instant, wall: DateTime<Utc>) {
        self.run = None;
        self.push(ThroughputSample { bytes: 0, state, retries: 0 }, now, wall);
    }

    fn push(&mut self, s: ThroughputSample, now: Instant, wall: DateTime<Utc>) {
        self.samples.push_back(s);
        while self.samples.len() > THROUGHPUT_KEEP {
            self.samples.pop_front();
        }
        self.at = wall;
        self.last_tick = now;
    }

    /// The newest `last` samples (all of them by default).
    pub(crate) fn snapshot(&self, game: GameRef, last: Option<usize>) -> Throughput {
        let skip = last.map_or(0, |n| self.samples.len().saturating_sub(n));
        Throughput { game, interval_ms: INTERVAL.as_millis() as u64, at: self.at, samples: self.samples.iter().skip(skip).copied().collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    fn states(h: &History) -> Vec<(u64, SampleState)> {
        h.samples.iter().map(|s| (s.bytes, s.state)).collect()
    }

    #[test]
    fn samples_come_from_received_bytes() {
        let (t0, wall) = (Instant::now(), Utc::now());
        let p = Arc::new(Progress::default());
        let mut h = None;
        // What an earlier run left on disk counts as downloaded, never as throughput.
        p.downloaded.store(5_000_000, Ordering::Relaxed);
        p.received.store(1000, Ordering::Relaxed);
        History::running(&mut h, &p, false, at(t0, 1), wall);
        p.received.store(3000, Ordering::Relaxed);
        History::running(&mut h, &p, false, at(t0, 2), wall);
        // Nothing this interval: stalled, not a gap.
        History::running(&mut h, &p, false, at(t0, 3), wall);
        // Two seconds late: scaled back to one interval.
        p.received.store(7000, Ordering::Relaxed);
        History::running(&mut h, &p, false, at(t0, 5), wall);
        // Slices waiting to retry.
        p.retrying.store(1, Ordering::Relaxed);
        p.retries.store(3, Ordering::Relaxed);
        History::running(&mut h, &p, false, at(t0, 6), wall);
        let h = h.unwrap();
        assert_eq!(
            states(&h),
            [
                (1000, SampleState::Downloading),
                (2000, SampleState::Downloading),
                (0, SampleState::Stalled),
                (2000, SampleState::Downloading),
                (0, SampleState::Retrying)
            ]
        );
        assert_eq!(h.samples.back().unwrap().retries, 3);
        assert_eq!(h.samples[0].retries, 0);
    }

    #[test]
    fn pauses_are_gaps_and_a_new_run_starts_from_zero() {
        let (t0, wall) = (Instant::now(), Utc::now());
        let p = Arc::new(Progress::default());
        let mut h = None;
        p.received.store(4000, Ordering::Relaxed);
        History::running(&mut h, &p, false, at(t0, 1), wall);
        let hist = h.as_mut().unwrap();
        hist.idle(SampleState::Paused, at(t0, 2), wall);
        hist.idle(SampleState::Waiting, at(t0, 3), wall);
        // Resumed: a new run, new counters (from zero).
        let q = Arc::new(Progress::default());
        q.received.store(500, Ordering::Relaxed);
        History::running(&mut h, &q, false, at(t0, 4), wall);
        // A repair: hashing on the same run's counters, then downloading.
        q.hashed.store(9000, Ordering::Relaxed);
        History::running(&mut h, &q, true, at(t0, 5), wall);
        q.received.store(1500, Ordering::Relaxed);
        History::running(&mut h, &q, false, at(t0, 6), wall);
        let h = h.unwrap();
        assert_eq!(
            states(&h),
            [
                (4000, SampleState::Downloading),
                (0, SampleState::Paused),
                (0, SampleState::Waiting),
                (500, SampleState::Downloading),
                (9000, SampleState::Verifying),
                (1000, SampleState::Downloading)
            ]
        );
        assert!(h.samples[1].state.is_gap() && h.samples[2].state.is_gap() && !h.samples[0].state.is_gap());
    }

    #[test]
    fn the_ring_is_bounded() {
        let (t0, wall) = (Instant::now(), Utc::now());
        let p = Arc::new(Progress::default());
        let mut h = None;
        for s in 1..=THROUGHPUT_KEEP as u64 + 100 {
            p.received.store(s * 10, Ordering::Relaxed);
            History::running(&mut h, &p, false, at(t0, s), wall + chrono::Duration::seconds(s as i64));
        }
        let h = h.unwrap();
        assert_eq!(h.samples.len(), THROUGHPUT_KEEP);
        let g = GameRef::ubisoft(1);
        let all = h.snapshot(g, None);
        assert_eq!((all.samples.len(), all.interval_ms, all.game), (THROUGHPUT_KEEP, 1000, g));
        assert_eq!(all.at, wall + chrono::Duration::seconds(THROUGHPUT_KEEP as i64 + 100));
        let recent = h.snapshot(g, Some(60));
        assert_eq!(recent.samples.len(), 60);
        assert_eq!(recent.samples[..], all.samples[THROUGHPUT_KEEP - 60..]);
        assert_eq!(h.snapshot(g, Some(10_000)).samples.len(), THROUGHPUT_KEEP);
    }
}
