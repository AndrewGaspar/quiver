//! Per-second statistics for [`HostStats`](quiver_stream_proto::HostStats) and the CLI, and
//! the percentiles a benchmark prints.

use std::time::{Duration, Instant};

use quiver_stream_proto::HostStats;

/// Samples of one stage, in µs.
#[derive(Debug, Default, Clone)]
pub struct Samples(pub Vec<u32>);

impl Samples {
    pub fn push(&mut self, v: u32) {
        self.0.push(v);
    }

    /// The `p`th percentile (0..=100), nearest rank. None when empty.
    pub fn percentile(&self, p: f64) -> Option<u32> {
        if self.0.is_empty() {
            return None;
        }
        let mut v = self.0.clone();
        v.sort_unstable();
        let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
        Some(v[rank.clamp(1, v.len()) - 1])
    }

    pub fn median(&self) -> u32 {
        self.percentile(50.0).unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// "p50 1234 p95 … p99 … max …" in µs.
    pub fn summary(&self) -> String {
        if self.0.is_empty() {
            return "no samples".into();
        }
        format!(
            "p50 {} p95 {} p99 {} max {} µs (n {})",
            self.median(),
            self.percentile(95.0).unwrap(),
            self.percentile(99.0).unwrap(),
            self.0.iter().max().unwrap(),
            self.0.len()
        )
    }
}

/// One second's worth of frames.
#[derive(Debug)]
pub struct Window {
    start: Instant,
    frames: u32,
    bytes: u64,
    capture: Samples,
    encode: Samples,
}

impl Default for Window {
    fn default() -> Self {
        Window { start: Instant::now(), frames: 0, bytes: 0, capture: Samples::default(), encode: Samples::default() }
    }
}

impl Window {
    pub fn frame(&mut self, bytes: usize, capture_us: u32, encode_us: u32) {
        self.frames += 1;
        self.bytes += bytes as u64;
        self.capture.push(capture_us);
        self.encode.push(encode_us);
    }

    /// The stats once a second has passed (and a fresh window started), else None.
    pub fn tick(&mut self) -> Option<HostStats> {
        let dt = self.start.elapsed();
        if dt < Duration::from_secs(1) {
            return None;
        }
        let s = dt.as_secs_f64();
        let stats = HostStats {
            fps: (self.frames as f64 / s) as f32,
            capture_us: self.capture.median(),
            encode_us: self.encode.median(),
            bitrate_kbps: (self.bytes as f64 * 8.0 / s / 1000.0) as u32,
            gpu_percent: None,
        };
        *self = Window::default();
        Some(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_by_nearest_rank() {
        let s = Samples((1..=100).collect());
        assert_eq!(s.percentile(50.0), Some(50));
        assert_eq!(s.percentile(99.0), Some(99));
        assert_eq!(s.percentile(100.0), Some(100));
        assert_eq!(s.percentile(0.0), Some(1));
        assert_eq!(Samples::default().percentile(50.0), None);
        assert_eq!(Samples(vec![7]).median(), 7);
    }
}
