//! Aggregation (SIGHT-HUD §3.10): frame times from present records, windowed statistics,
//! and a session-long log histogram for percentiles.
//!
//! "1% low" is 1000 ÷ the 99th-percentile frame time in the window (CapFrameX P1,
//! decision 11); `p1low_avg` is the mean of the slowest 1% for MangoHud-style numbers.

use std::collections::{HashMap, VecDeque};

/// How long raw samples are kept (the 10 s window, plus slack).
pub const KEEP_NS: i64 = 32_000_000_000;
/// Intervals longer than this are gaps (a loading screen, alt-tab, a breakpoint), not frames.
pub const MAX_FRAME_NS: i64 = 1_000_000_000;
/// Samples per numeric series kept for the session (decimated beyond).
const SESSION_SAMPLES: usize = 1 << 16;

/// Statistics over one window of values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowStats {
    pub count: usize,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub p999: f64,
    /// Mean of the slowest 1% (at least one value).
    pub top1_mean: f64,
    pub sum: f64,
}

impl WindowStats {
    /// From unsorted values (sorted in place). `None` when empty.
    pub fn of(v: &mut [f64]) -> Option<WindowStats> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let n = v.len();
        let sum: f64 = v.iter().sum();
        let top = (n / 100).max(1);
        Some(WindowStats {
            count: n,
            mean: sum / n as f64,
            min: v[0],
            max: v[n - 1],
            p50: percentile_sorted(v, 50.0),
            p95: percentile_sorted(v, 95.0),
            p99: percentile_sorted(v, 99.0),
            p999: percentile_sorted(v, 99.9),
            top1_mean: v[n - top..].iter().sum::<f64>() / top as f64,
            sum,
        })
    }
}

/// Nearest-rank percentile of sorted values.
pub fn percentile_sorted(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

/// Frame times in ms, 1% relative resolution from 0.05 ms to ~10 s.
#[derive(Clone, Debug)]
pub struct LogHistogram {
    counts: Vec<u32>,
    total: u64,
}

const H_MIN: f64 = 0.05;
const H_STEP: f64 = 1.01;
const H_BUCKETS: usize = 1240;

impl Default for LogHistogram {
    fn default() -> Self {
        Self { counts: vec![0; H_BUCKETS], total: 0 }
    }
}

impl LogHistogram {
    fn bucket(v: f64) -> usize {
        if v <= H_MIN {
            return 0;
        }
        ((v / H_MIN).ln() / H_STEP.ln()).floor().clamp(0.0, (H_BUCKETS - 1) as f64) as usize
    }

    fn value(b: usize) -> f64 {
        // Geometric middle of the bucket.
        H_MIN * H_STEP.powf(b as f64 + 0.5)
    }

    pub fn add(&mut self, v: f64) {
        if v.is_finite() && v >= 0.0 {
            self.counts[Self::bucket(v)] += 1;
            self.total += 1;
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        let rank = ((p / 100.0) * self.total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (b, &c) in self.counts.iter().enumerate() {
            seen += c as u64;
            if seen >= rank {
                return Some(Self::value(b));
            }
        }
        Some(Self::value(H_BUCKETS - 1))
    }

    /// Values above `v`.
    pub fn count_above(&self, v: f64) -> u64 {
        let b = Self::bucket(v);
        self.counts[b + 1..].iter().map(|&c| c as u64).sum()
    }

    /// Mean of the slowest `pct` percent.
    pub fn top_mean(&self, pct: f64) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        let want = ((pct / 100.0) * self.total as f64).ceil().max(1.0) as u64;
        let (mut got, mut sum) = (0u64, 0.0);
        for (b, &c) in self.counts.iter().enumerate().rev() {
            let take = (c as u64).min(want - got);
            sum += take as f64 * Self::value(b);
            got += take;
            if got >= want {
                break;
            }
        }
        Some(sum / got as f64)
    }
}

/// Presented frames from the layer's present records (role `frame.presented`).
#[derive(Debug, Default)]
pub struct FrameStore {
    /// The swapchain the numbers follow: the one the layer's status names, else the busiest.
    primary: Option<u32>,
    /// Last present entry per swapchain.
    last: HashMap<u32, i64>,
    /// Presents per swapchain since the last pick, to find the busiest.
    counts: HashMap<u32, u32>,
    /// (time of the present ns, frame time ms), the last [`KEEP_NS`].
    recent: VecDeque<(i64, f64)>,
    /// The same for application-rendered frames on the primary swapchain (the presents the
    /// layer's Reflex counting says start one): the graph's base series.
    recent_base: VecDeque<(i64, f64)>,
    last_base: Option<i64>,
    pub hist: LogHistogram,
    pub frames: u64,
    /// Sum of frame times counted, ns (the time the summary covers).
    pub covered_ns: i64,
    pub last_ns: i64,
}

impl FrameStore {
    /// The swapchain the layer says it draws on (its status block).
    pub fn set_primary(&mut self, swapchain: u32) {
        if swapchain != 0 && self.primary != Some(swapchain) {
            self.primary = Some(swapchain);
        }
    }

    /// One present (entry time, CLOCK_MONOTONIC_RAW ns). Returns the frame time in ms if
    /// it counted.
    pub fn present(&mut self, swapchain: u32, t: i64) -> Option<f64> {
        *self.counts.entry(swapchain).or_default() += 1;
        if self.primary.is_none() {
            self.primary = Some(swapchain);
        } else if self.counts.values().sum::<u32>() >= 240 {
            // Re-pick the busiest swapchain now and then (a launcher window, then the game).
            if let Some((&busiest, _)) = self.counts.iter().max_by_key(|(_, c)| **c) {
                self.primary = Some(busiest);
            }
            self.counts.clear();
        }
        let prev = self.last.insert(swapchain, t);
        if self.primary != Some(swapchain) {
            return None;
        }
        let dt = t - prev?;
        if dt <= 0 || dt > MAX_FRAME_NS {
            return None;
        }
        let ms = dt as f64 / 1e6;
        self.recent.push_back((t, ms));
        self.hist.add(ms);
        self.frames += 1;
        self.covered_ns += dt;
        self.last_ns = t;
        while self.recent.front().is_some_and(|(ft, _)| t - ft > KEEP_NS) {
            self.recent.pop_front();
        }
        Some(ms)
    }

    /// Frame times (ms) presented in `(now - window, now]`.
    pub fn window(&self, now: i64, window_ns: i64) -> Vec<f64> {
        self.recent.iter().rev().take_while(|(t, _)| now - t < window_ns).map(|(_, ms)| *ms).collect()
    }

    /// The most recent `n` frame times, oldest first (the desktop presenter's graph).
    pub fn tail(&self, since: i64) -> Vec<f32> {
        self.recent.iter().filter(|(t, _)| *t > since).map(|(_, ms)| *ms as f32).collect()
    }

    /// A present that starts an application-rendered frame (entry time). Only the primary
    /// swapchain's count; returns the base frame time in ms if it counted.
    pub fn base(&mut self, swapchain: u32, t: i64) -> Option<f64> {
        if self.primary != Some(swapchain) {
            return None;
        }
        let prev = self.last_base.replace(t);
        let dt = t - prev?;
        if dt <= 0 || dt > MAX_FRAME_NS {
            return None;
        }
        let ms = dt as f64 / 1e6;
        self.recent_base.push_back((t, ms));
        while self.recent_base.front().is_some_and(|(ft, _)| t - ft > KEEP_NS) {
            self.recent_base.pop_front();
        }
        Some(ms)
    }

    /// (present time ns, frame time ms) since `since`, oldest first: presented frames, and the
    /// application-rendered ones (the Sight panel's 30 s graph and the performance log).
    pub fn timed(&self, since: i64) -> (Vec<(i64, f64)>, Vec<(i64, f64)>) {
        let f = |q: &VecDeque<(i64, f64)>| q.iter().filter(|(t, _)| *t > since).copied().collect();
        (f(&self.recent), f(&self.recent_base))
    }

    /// Base frame times since `since`, oldest first (the desktop graph's base series).
    pub fn base_tail(&self, since: i64) -> Vec<f32> {
        self.recent_base.iter().filter(|(t, _)| *t > since).map(|(_, ms)| *ms as f32).collect()
    }
}

/// A numeric series: gauges, derived values. Recent samples for windows, session aggregates.
#[derive(Debug, Default, Clone)]
pub struct NumSeries {
    recent: VecDeque<(i64, f64)>,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    session: Vec<f64>,
    stride: u64,
}

impl NumSeries {
    pub fn push(&mut self, t: i64, v: f64) {
        if !v.is_finite() {
            return;
        }
        self.recent.push_back((t, v));
        while self.recent.front().is_some_and(|(ft, _)| t - ft > KEEP_NS) {
            self.recent.pop_front();
        }
        if self.count == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.count += 1;
        self.sum += v;
        // Keep a decimated copy for session percentiles.
        let stride = self.stride.max(1);
        if self.count.is_multiple_of(stride) {
            self.session.push(v);
            if self.session.len() >= SESSION_SAMPLES {
                self.session = self.session.iter().step_by(2).copied().collect();
                self.stride = stride * 2;
            }
        }
    }

    pub fn last(&self) -> Option<(i64, f64)> {
        self.recent.back().copied()
    }

    pub fn window(&self, now: i64, window_ns: i64) -> Vec<f64> {
        self.recent.iter().rev().take_while(|(t, _)| now - t < window_ns).map(|(_, v)| *v).collect()
    }

    /// Values for the session (decimated).
    pub fn session(&self) -> Vec<f64> {
        self.session.clone()
    }

    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// (time ns, value) since `since`, oldest first.
    pub fn since(&self, since: i64) -> Vec<(i64, f64)> {
        self.recent.iter().filter(|(t, _)| *t > since).copied().collect()
    }

    /// The last `n` values, oldest first (sparklines).
    pub fn tail(&self, n: usize) -> Vec<f64> {
        let skip = self.recent.len().saturating_sub(n);
        self.recent.iter().skip(skip).map(|(_, v)| *v).collect()
    }
}

/// A monotonic counter (mod counters): totals over time; consumers see a rate.
#[derive(Debug, Default, Clone)]
pub struct CounterSeries {
    samples: VecDeque<(i64, u64)>,
    /// Added across resets (a total going backwards starts a new baseline).
    offset: u64,
    first: Option<(i64, u64)>,
}

impl CounterSeries {
    pub fn push(&mut self, t: i64, total: u64) {
        if let Some(&(_, prev)) = self.samples.back()
            && total + self.offset < prev
        {
            self.offset = prev;
        }
        let v = total + self.offset;
        self.first.get_or_insert((t, v));
        self.samples.push_back((t, v));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(ft, _)| t - ft > KEEP_NS) {
            self.samples.pop_front();
        }
    }

    pub fn total(&self) -> Option<u64> {
        self.samples.back().map(|(_, v)| *v)
    }

    /// Per second over the window ending at the latest sample: from the newest sample at or
    /// before the window's start (or the oldest kept) to the latest.
    pub fn rate(&self, window_ns: i64) -> Option<f64> {
        let &(t1, v1) = self.samples.back()?;
        let start = t1 - window_ns;
        let &(t0, v0) = self.samples.iter().rev().find(|(t, _)| *t <= start).or_else(|| self.samples.front())?;
        (t1 > t0).then(|| (v1 - v0) as f64 * 1e9 / (t1 - t0) as f64)
    }

    /// Per second over the whole session.
    pub fn session_rate(&self) -> Option<f64> {
        let (t0, v0) = self.first?;
        let &(t1, v1) = self.samples.back()?;
        (t1 > t0).then(|| (v1 - v0) as f64 * 1e9 / (t1 - t0) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn window_stats_and_percentiles() {
        let mut v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        let s = WindowStats::of(&mut v).unwrap();
        assert_eq!((s.count, s.min, s.max, s.mean), (100, 1.0, 100.0, 50.5));
        assert_eq!((s.p50, s.p95, s.p99, s.p999), (50.0, 95.0, 99.0, 100.0));
        assert_eq!(s.top1_mean, 100.0);
        assert!(WindowStats::of(&mut []).is_none());
    }

    #[test]
    fn p1low_is_1000_over_p99() {
        // 99 frames at 10 ms, one at 50 ms: p99 frame time is 10 ms (nearest rank 99), the
        // 1% low by CapFrameX P1 is 100 fps; the slowest-1% mean is 50 ms (20 fps).
        let mut fs = FrameStore::default();
        let mut t = 0;
        fs.present(1, t);
        for i in 0..100 {
            t += if i == 50 { 50 * MS } else { 10 * MS };
            fs.present(1, t);
        }
        let mut w = fs.window(t, 2_000 * MS);
        assert_eq!(w.len(), 100);
        let s = WindowStats::of(&mut w).unwrap();
        assert_eq!(1000.0 / s.p99, 100.0);
        assert_eq!(1000.0 / s.top1_mean, 20.0);
        // 1000 / mean frame time = presented fps.
        assert!((1000.0 / s.mean - 1000.0 / 10.4).abs() < 1e-9);
        // The histogram agrees within its 1% resolution.
        let p99 = fs.hist.percentile(99.0).unwrap();
        assert!((p99 - 10.0).abs() / 10.0 < 0.011, "{p99}");
        assert_eq!(fs.hist.count_above(40.0), 1);
        assert_eq!(fs.frames, 100);
    }

    #[test]
    fn gaps_and_other_swapchains_dont_count() {
        let mut fs = FrameStore::default();
        fs.present(1, 0);
        assert_eq!(fs.present(1, 16 * MS), Some(16.0));
        // A second swapchain (a launcher overlay) doesn't make frames.
        assert_eq!(fs.present(2, 17 * MS), None);
        // A 3 s loading screen is a gap.
        assert_eq!(fs.present(1, 3_016 * MS), None);
        assert_eq!(fs.present(1, 3_032 * MS), Some(16.0));
        // Time going backwards (a reused ring) isn't a frame.
        assert_eq!(fs.present(1, 3_000 * MS), None);
        // The layer's status can move the primary.
        fs.set_primary(2);
        // (its last present was long ago: a gap)
        assert_eq!(fs.present(2, 3_040 * MS), None);
        assert_eq!(fs.present(2, 3_050 * MS), Some(10.0));
    }

    #[test]
    fn window_excludes_old_frames() {
        let mut fs = FrameStore::default();
        for i in 0..=300 {
            fs.present(1, i * 10 * MS);
        }
        let now = 300 * 10 * MS;
        assert_eq!(fs.window(now, 1_000 * MS).len(), 100);
        assert_eq!(fs.window(now, 10_000 * MS).len(), 300);
    }

    #[test]
    fn counter_rates() {
        let mut c = CounterSeries::default();
        for i in 0..=20 {
            c.push(i * 100 * MS, (i * 8) as u64); // 80 per second
        }
        assert!((c.rate(1_000 * MS).unwrap() - 80.0).abs() < 1e-9);
        assert!((c.session_rate().unwrap() - 80.0).abs() < 1e-9);
        // A reset (the mod restarted) doesn't produce a negative rate.
        c.push(2_100 * MS, 0);
        c.push(2_200 * MS, 8);
        assert!(c.rate(100 * MS).unwrap() >= 0.0);
        assert_eq!(c.total(), Some(168));
    }

    #[test]
    fn num_series() {
        let mut s = NumSeries::default();
        for i in 0..20 {
            s.push(i * 1_000 * MS, i as f64);
        }
        s.push(20_000 * MS, f64::NAN);
        assert_eq!(s.count, 20);
        assert_eq!((s.min, s.max, s.mean()), (0.0, 19.0, Some(9.5)));
        assert_eq!(s.window(19_000 * MS, 1_000 * MS), vec![19.0]);
        assert_eq!(s.tail(3), vec![17.0, 18.0, 19.0]);
        assert_eq!(s.session().len(), 20);
    }

    #[test]
    fn histogram_top_mean_and_bounds() {
        let mut h = LogHistogram::default();
        assert_eq!(h.percentile(50.0), None);
        for _ in 0..99 {
            h.add(10.0);
        }
        h.add(100.0);
        h.add(f64::NAN);
        h.add(1e9);
        assert_eq!(h.total(), 101);
        let top = h.top_mean(1.0).unwrap();
        assert!(top > 1000.0, "{top}");
    }
}
