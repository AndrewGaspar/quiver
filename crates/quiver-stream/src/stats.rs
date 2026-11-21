//! Measurements: sample windows, the host clock offset, and the per-second stats the host
//! (ClientStats) and the stats chip both get.

use std::collections::VecDeque;
use std::sync::Mutex;

use quiver_stream_proto::{ClientStats, HostStats, Micros};

/// A bag of microsecond samples.
#[derive(Debug, Default, Clone)]
pub struct Samples {
    v: Vec<u64>,
}

impl Samples {
    pub fn push(&mut self, x: u64) {
        self.v.push(x);
    }
    pub fn len(&self) -> usize {
        self.v.len()
    }
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }
    pub fn clear(&mut self) {
        self.v.clear();
    }
    /// The p-th percentile (0–100, nearest rank); 0 when empty.
    pub fn percentile(&self, p: u32) -> u64 {
        if self.v.is_empty() {
            return 0;
        }
        let mut s = self.v.clone();
        s.sort_unstable();
        let i = ((s.len() - 1) * p.min(100) as usize).div_ceil(100);
        s[i]
    }
    pub fn median(&self) -> u64 {
        self.percentile(50)
    }
}

/// The host's clock relative to ours, from Ping/Pong: the sample with the shortest round trip
/// among the last [`ClockSync::WINDOW`] is the most trustworthy (the least queueing).
#[derive(Debug, Default)]
pub struct ClockSync {
    samples: VecDeque<(u32, i64)>,
    /// Set by the transport when it keeps the sync itself.
    pub external: Option<i64>,
}

impl ClockSync {
    pub const WINDOW: usize = 16;

    /// A Pong: we sent at `sent`, the host's clock read `peer_now`, it came back at `recv`.
    pub fn on_pong(&mut self, sent: Micros, peer_now: Micros, recv: Micros) {
        let rtt = recv.saturating_sub(sent).min(u32::MAX as u64) as u32;
        let offset = peer_now as i64 - (sent as i64 + rtt as i64 / 2);
        self.samples.push_back((rtt, offset));
        while self.samples.len() > Self::WINDOW {
            self.samples.pop_front();
        }
    }

    /// The host's clock minus ours.
    pub fn offset(&self) -> Option<i64> {
        self.external.or_else(|| self.samples.iter().min_by_key(|s| s.0).map(|s| s.1))
    }

    /// The latest round trip.
    pub fn rtt_us(&self) -> Option<u32> {
        self.samples.back().map(|s| s.0)
    }

    /// A host timestamp on our clock.
    pub fn to_local(&self, host: Micros) -> Option<Micros> {
        self.offset().map(|o| (host as i64 - o).max(0) as u64)
    }
}

/// What the stages record during a period.
#[derive(Debug, Default)]
pub struct Window {
    pub frames_received: u32,
    pub frames_lost: u32,
    pub frames_dropped: u32,
    pub frames_shown: u32,
    pub video_bytes: u64,
    /// Capture (host clock, made local) to arrival here.
    pub network: Samples,
    /// Arrival to the picture ready on the GPU.
    pub decode: Samples,
    /// Picture ready to on screen (present wait), or to the present call.
    pub present: Samples,
    pub capture_to_present: Samples,
    pub packets_received: u32,
    pub packets_lost: u32,
    pub packets_recovered: u32,
    pub jitter_us: Option<u32>,
    pub estimated_kbps: u32,
}

/// What the stats chip shows: the last period's numbers.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub client: ClientStats,
    pub host: Option<HostStats>,
    pub fps: f32,
    pub kbps: u32,
    pub loss_percent: f32,
    pub decoder: String,
    pub present_mode: String,
    pub size: (u32, u32),
    pub codec: String,
    pub hdr: bool,
    pub audio_ms: f32,
}

#[derive(Default)]
pub struct Stats {
    pub window: Mutex<Window>,
}

impl Stats {
    /// Close the period of `secs` seconds: the ClientStats for the host and the chip's numbers.
    pub fn take(&self, secs: f32, rtt_us: Option<u32>) -> (ClientStats, f32, u32, f32) {
        let w = std::mem::take(&mut *self.window.lock().unwrap());
        let c = ClientStats {
            frames_received: w.frames_received,
            frames_lost: w.frames_lost,
            frames_dropped: w.frames_dropped,
            packets_received: w.packets_received,
            packets_lost: w.packets_lost,
            packets_recovered: w.packets_recovered,
            rtt_us: rtt_us.unwrap_or(0),
            jitter_us: w.jitter_us.unwrap_or(0),
            network_us: w.network.median() as u32,
            decode_us: w.decode.median() as u32,
            present_us: w.present.median() as u32,
            capture_to_present_us: w.capture_to_present.median() as u32,
            estimated_kbps: w.estimated_kbps,
        };
        let secs = secs.max(1e-3);
        let fps = w.frames_shown as f32 / secs;
        let kbps = (w.video_bytes as f32 * 8.0 / 1000.0 / secs) as u32;
        let total = w.frames_received + w.frames_lost;
        let loss = if total > 0 { w.frames_lost as f32 * 100.0 / total as f32 } else { 0.0 };
        (c, fps, kbps, loss)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut s = Samples::default();
        assert_eq!(s.median(), 0);
        for x in 1..=100 {
            s.push(x);
        }
        assert_eq!(s.percentile(0), 1);
        assert_eq!(s.median(), 51);
        assert_eq!(s.percentile(99), 100);
        assert_eq!(s.percentile(100), 100);
    }

    #[test]
    fn clock_offset_prefers_the_shortest_round_trip() {
        let mut c = ClockSync::default();
        // Host clock 1_000_000 ahead. A 2 ms round trip, then a 20 ms one with queueing
        // on the way back (asymmetric), which would skew the offset.
        c.on_pong(10_000, 1_011_000, 12_000);
        c.on_pong(20_000, 1_021_000, 40_000);
        assert_eq!(c.offset(), Some(1_000_000));
        assert_eq!(c.rtt_us(), Some(20_000));
        assert_eq!(c.to_local(1_050_000), Some(50_000));
        c.external = Some(5);
        assert_eq!(c.offset(), Some(5));
    }
}
