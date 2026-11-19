//! The data behind a download's throughput bars: quiverd's history
//! (`Request::DownloadThroughput`), read whole when the app connects and then merged in as
//! it grows, so the graph doesn't start empty each time the app opens. From a quiverd
//! without history, the page samples the queue's `speed` itself, as it always did.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use quiver_api::downloads::{Throughput, ThroughputSample};

use super::SAMPLES;

/// One game's bars, oldest first.
#[derive(Default, Clone, Debug)]
pub struct Series {
    /// Bytes per interval; `None`: a gap (paused, waiting its turn).
    pub bars: VecDeque<Option<u64>>,
    /// When quiverd's newest merged sample ended; `None`: none merged since (re)connecting,
    /// so the next history replaces what is here.
    upto: Option<DateTime<Utc>>,
}

impl Series {
    pub fn push(&mut self, v: Option<u64>) {
        self.bars.push_back(v);
        while self.bars.len() > SAMPLES {
            self.bars.pop_front();
        }
    }

    /// The next history replaces the bars (after reconnecting: quiverd may have restarted).
    pub fn reseed(&mut self) {
        self.upto = None;
    }

    /// Add quiverd's samples. The first time they replace the bars (what the page sampled
    /// itself before the answer came); after that only the samples newer than the newest
    /// merged go on, with gaps for any that were missed in between.
    pub fn merge(&mut self, t: &Throughput) {
        let n = t.samples.len();
        if n == 0 {
            return;
        }
        let iv = t.interval_ms.max(1) as i64;
        // When sample `i` ended, in ms before `t.at`.
        let before = |i: usize| (n - 1 - i) as i64 * iv;
        match self.upto {
            None => {
                self.bars.clear();
                for s in &t.samples {
                    self.push(bar(s));
                }
            }
            Some(upto) => {
                let ahead = (t.at - upto).num_milliseconds();
                // Newer than what is here, by more than half an interval (ticks jitter).
                let Some(first) = (0..n).find(|&i| ahead - before(i) > iv / 2) else { return };
                let missed = ((ahead - before(first) + iv / 2) / iv - 1).clamp(0, SAMPLES as i64);
                for _ in 0..missed {
                    self.push(None);
                }
                for s in &t.samples[first..] {
                    self.push(bar(s));
                }
            }
        }
        self.upto = Some(t.at);
    }
}

fn bar(s: &ThroughputSample) -> Option<u64> {
    (!s.state.is_gap()).then_some(s.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::GameRef;
    use quiver_api::downloads::SampleState;

    fn history(at_s: i64, samples: &[(u64, SampleState)]) -> Throughput {
        Throughput {
            game: GameRef::ubisoft(1),
            interval_ms: 1000,
            at: DateTime::from_timestamp(1_800_000_000 + at_s, 0).unwrap(),
            samples: samples.iter().map(|&(bytes, state)| ThroughputSample { bytes, state, retries: 0 }).collect(),
        }
    }

    const D: SampleState = SampleState::Downloading;

    #[test]
    fn seeds_then_appends_only_whats_new() {
        let mut s = Series::default();
        // Sampled by the page before quiverd's answer came: replaced by the history.
        s.push(Some(999));
        s.merge(&history(10, &[(1, D), (0, SampleState::Paused), (0, SampleState::Stalled), (4, D)]));
        assert_eq!(s.bars, [Some(1), None, Some(0), Some(4)]);
        // The next poll overlaps: only the newer samples go on.
        s.merge(&history(12, &[(0, SampleState::Stalled), (4, D), (5, D), (6, D)]));
        assert_eq!(s.bars, [Some(1), None, Some(0), Some(4), Some(5), Some(6)]);
        // Nothing new (the same answer again, or an older one).
        s.merge(&history(12, &[(5, D), (6, D)]));
        s.merge(&history(11, &[(5, D)]));
        assert_eq!(s.bars.len(), 6);
        // A poll that missed samples: gaps for them, rather than closing up the time.
        s.merge(&history(16, &[(9, D)]));
        assert_eq!(s.bars, [Some(1), None, Some(0), Some(4), Some(5), Some(6), None, None, None, Some(9)]);
    }

    #[test]
    fn keeps_the_window_and_reseeds_after_reconnecting() {
        let mut s = Series::default();
        let long: Vec<(u64, SampleState)> = (0..SAMPLES as u64 + 30).map(|b| (b, D)).collect();
        s.merge(&history(100, &long));
        assert_eq!(s.bars.len(), SAMPLES);
        assert_eq!(s.bars.back(), Some(&Some(SAMPLES as u64 + 29)));
        // After a long stall in polling, the gaps never outgrow the window.
        s.merge(&history(100_000, &[(1, D)]));
        assert_eq!(s.bars.len(), SAMPLES);
        assert_eq!(s.bars.back(), Some(&Some(1)));
        assert!(s.bars.iter().take(SAMPLES - 1).all(Option::is_none));
        // Reconnected (quiverd may have restarted and started over): replaced again.
        s.reseed();
        s.merge(&history(5, &[(7, D), (8, D)]));
        assert_eq!(s.bars, [Some(7), Some(8)]);
        // An empty history changes nothing.
        s.merge(&history(9, &[]));
        assert_eq!(s.bars, [Some(7), Some(8)]);
    }
}
