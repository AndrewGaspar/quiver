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
    /// Presentation gaps: new frames shown more than 2.5 frame intervals apart.
    pub frame_gaps: u32,
    pub max_gap_us: u64,
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

/// One period's numbers: the host's [`ClientStats`], the chip's, and the extra ones a netlab
/// run reports.
#[derive(Debug, Clone, Default)]
pub struct Period {
    pub client: ClientStats,
    pub fps: f32,
    pub kbps: u32,
    pub loss_percent: f32,
    pub latency_p99_us: u64,
    pub frame_gaps: u32,
    pub max_gap_us: u64,
}

impl Period {
    /// The `extra` numbers of a netlab stats line (`frame_gaps`, `max_gap_ms`, `recv_kbps`,
    /// `latency_p99_us`).
    pub fn netlab_extra(&self) -> std::collections::BTreeMap<String, f64> {
        let mut m = std::collections::BTreeMap::new();
        m.insert("frame_gaps".into(), self.frame_gaps as f64);
        m.insert("max_gap_ms".into(), self.max_gap_us as f64 / 1000.0);
        m.insert("recv_kbps".into(), self.kbps as f64);
        if self.latency_p99_us > 0 {
            m.insert("latency_p99_us".into(), self.latency_p99_us as f64);
        }
        m
    }
}

impl Stats {
    /// Note that a new frame was shown at `at` (our clock), `interval_us` after the stream's
    /// frame interval says the next one is due: a gap when frames are more than 2.5
    /// intervals apart. `last` is the previous one's time, kept by the caller.
    pub fn shown(&self, last: &mut Option<Micros>, at: Micros, interval_us: u64) {
        if let Some(prev) = last.replace(at) {
            let d = at.saturating_sub(prev);
            if interval_us > 0 && d * 2 > interval_us * 5 {
                let mut w = self.window.lock().unwrap();
                w.frame_gaps += 1;
                w.max_gap_us = w.max_gap_us.max(d);
            }
        }
    }

    /// Close the period of `secs` seconds.
    pub fn period(&self, secs: f32, rtt_us: Option<u32>) -> Period {
        let p99 = self.window.lock().unwrap().capture_to_present.percentile(99);
        let (gaps, max_gap) = {
            let w = self.window.lock().unwrap();
            (w.frame_gaps, w.max_gap_us)
        };
        let (client, fps, kbps, loss_percent) = self.take(secs, rtt_us);
        Period { client, fps, kbps, loss_percent, latency_p99_us: p99, frame_gaps: gaps, max_gap_us: max_gap }
    }

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
    fn gaps_are_frames_more_than_two_and_a_half_intervals_apart() {
        let s = Stats::default();
        let mut last = None;
        for at in [0, 8_333, 16_666, 37_000, 45_000, 100_000] {
            s.shown(&mut last, at, 8_333);
        }
        let p = s.period(1.0, None);
        assert_eq!((p.frame_gaps, p.max_gap_us), (1, 55_000), "20.3 ms is under 2.5 intervals; 55 ms isn't");
        assert_eq!(p.netlab_extra()["max_gap_ms"], 55.0);
        assert_eq!(s.period(1.0, None).frame_gaps, 0, "a new period starts empty");
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
