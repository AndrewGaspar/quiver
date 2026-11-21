//! The audio jitter buffer: packets in, one decode instruction per output frame out.
//!
//! The buffer holds encoded packets by `seq` and plays them back at the device's pace: the
//! audio callback calls [`JitterBuffer::pull`] once per frame of output it needs. A missing
//! packet is recovered from the next one's in-band FEC when that one is here, and concealed
//! (Opus PLC) otherwise; a packet that turns up after its turn is dropped.
//!
//! The depth adapts. Each packet's transit time (arrival on this clock minus capture on the
//! host's; the clock offset cancels out) goes into a two-second window, and the target depth
//! is the spread between the window's fastest and its 95th-percentile packet, plus a frame
//! and a little margin, clamped to `[min_target, max_target]` (10–80 ms by default: a LAN
//! sits at the bottom). [`JitterBuffer::set_floor`] raises the target so audio waits for
//! video (A/V sync). When the depth stays a couple of frames off the target the buffer
//! drops a frame or conceals one, which also absorbs the drift between the host's and the
//! client's audio clocks.

use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    pub frame_us: u32,
    pub min_target_us: u32,
    pub max_target_us: u32,
    /// Concealed frames in a row before playback stops and the buffer refills.
    pub max_conceal: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self { frame_us: 5_000, min_target_us: 10_000, max_target_us: 80_000, max_conceal: 4 }
    }
}

/// What to do for the next frame of output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pull {
    /// Decode this packet.
    Packet(Vec<u8>),
    /// The packet for this frame is missing: decode the FEC data in this packet (the next
    /// one), which describes it.
    Fec(Vec<u8>),
    /// Missing, and no FEC: let the decoder conceal it.
    Conceal,
    /// Not playing (filling up): output silence.
    Silence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub played: u64,
    pub fec: u64,
    pub concealed: u64,
    pub late: u64,
    pub dropped: u64,
    pub underruns: u64,
}

const WINDOW_US: u64 = 2_000_000;
const MARGIN_US: u32 = 2_000;

pub struct JitterBuffer {
    cfg: JitterConfig,
    packets: BTreeMap<u64, Vec<u8>>,
    /// The seq the next pull plays.
    next: Option<u64>,
    playing: bool,
    concealed_run: u32,
    /// (arrival, transit) for the window.
    transit: VecDeque<(u64, i64)>,
    target_us: u32,
    floor_us: u32,
    /// Depth, smoothed over about 50 frames.
    depth_avg: f32,
    pub stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            cfg,
            packets: BTreeMap::new(),
            next: None,
            playing: false,
            concealed_run: 0,
            transit: VecDeque::new(),
            target_us: cfg.min_target_us,
            floor_us: 0,
            depth_avg: 0.0,
            stats: JitterStats::default(),
        }
    }

    pub fn target_us(&self) -> u32 {
        self.target_us
    }

    /// The audio that is here and not yet played, in microseconds.
    pub fn depth_us(&self) -> u32 {
        let frames = match (self.next, self.packets.last_key_value()) {
            (Some(n), Some((&last, _))) if self.playing && last >= n => last - n + 1,
            _ => self.packets.len() as u64,
        };
        (frames.min(10_000) as u32) * self.cfg.frame_us
    }

    /// The least depth to keep, for A/V sync: audio shouldn't come out before its video.
    pub fn set_floor(&mut self, floor_us: u32) {
        self.floor_us = floor_us.min(self.cfg.max_target_us);
        self.retarget();
    }

    fn retarget(&mut self) {
        let jitter = if self.transit.len() >= 2 {
            let mut v: Vec<i64> = self.transit.iter().map(|&(_, t)| t).collect();
            v.sort_unstable();
            let p95 = v[(v.len() - 1) * 95 / 100];
            (p95 - v[0]).clamp(0, u32::MAX as i64) as u32
        } else {
            0
        };
        let want = jitter.saturating_add(self.cfg.frame_us + MARGIN_US).max(self.floor_us);
        self.target_us = want.clamp(self.cfg.min_target_us, self.cfg.max_target_us);
    }

    /// A packet arrived at `arrival_us` (this clock); it was captured at `captured_us` (the
    /// host's clock).
    pub fn push(&mut self, seq: u64, captured_us: u64, arrival_us: u64, data: Vec<u8>) {
        // A late packet still counts towards the jitter: it is what the target is for.
        self.transit.push_back((arrival_us, arrival_us as i64 - captured_us as i64));
        while self.transit.front().is_some_and(|&(t, _)| t + WINDOW_US < arrival_us) {
            self.transit.pop_front();
        }
        self.retarget();
        if self.next.is_some_and(|n| seq < n) || self.packets.contains_key(&seq) {
            self.stats.late += 1;
            return;
        }
        self.packets.insert(seq, data);
    }

    /// The transport says these packets won't come. Nothing to do until their turn, but the
    /// buffer stops waiting for them when it refills.
    pub fn lost(&mut self, _first: u64, _last: u64) {}

    /// The instruction for the next frame of output.
    pub fn pull(&mut self) -> Pull {
        if !self.playing {
            if self.depth_us() < self.target_us || self.packets.is_empty() {
                return Pull::Silence;
            }
            self.playing = true;
            self.concealed_run = 0;
            self.next = self.packets.first_key_value().map(|(&k, _)| k);
            self.depth_avg = self.depth_us() as f32;
        }
        let depth = self.depth_us() as f32;
        self.depth_avg += (depth - self.depth_avg) * 0.02;
        let frame = self.cfg.frame_us as f32;
        let n = self.next.expect("playing has a next seq");
        // Too deep for a while: drop the oldest frame.
        if self.depth_avg > self.target_us as f32 + 2.5 * frame && self.packets.contains_key(&n) {
            self.packets.remove(&n);
            self.stats.dropped += 1;
            self.depth_avg -= frame;
            self.next = Some(n + 1);
            return self.pull_at(n + 1);
        }
        // Too shallow for a while (but not empty): stretch by a concealed frame.
        if self.depth_avg + 2.5 * frame < self.target_us as f32 && !self.packets.is_empty() {
            self.depth_avg += frame;
            self.stats.concealed += 1;
            return Pull::Conceal;
        }
        self.pull_at(n)
    }

    fn pull_at(&mut self, n: u64) -> Pull {
        if let Some(p) = self.packets.remove(&n) {
            self.next = Some(n + 1);
            self.concealed_run = 0;
            self.stats.played += 1;
            return Pull::Packet(p);
        }
        if self.packets.is_empty() {
            // Nothing at all: conceal a little, then stop and refill.
            self.concealed_run += 1;
            if self.concealed_run > self.cfg.max_conceal {
                self.playing = false;
                self.stats.underruns += 1;
                return Pull::Silence;
            }
            self.next = Some(n + 1);
            self.stats.concealed += 1;
            return Pull::Conceal;
        }
        // This one is missing but later ones are here.
        self.next = Some(n + 1);
        self.concealed_run = 0;
        if let Some(p) = self.packets.get(&(n + 1)) {
            self.stats.fec += 1;
            return Pull::Fec(p.clone());
        }
        self.stats.concealed += 1;
        Pull::Conceal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u64) -> Vec<u8> {
        vec![seq as u8]
    }

    /// Feed `n` packets 5 ms apart, each arriving `delay(seq)` after capture, pulling a frame
    /// every 5 ms; returns the pulls.
    fn run(jb: &mut JitterBuffer, n: u64, delay: impl Fn(u64) -> u64, skip: &[u64]) -> Vec<Pull> {
        let mut arrivals: Vec<(u64, u64)> = (0..n).filter(|s| !skip.contains(s)).map(|s| (s * 5_000 + delay(s), s)).collect();
        arrivals.sort();
        let mut out = Vec::new();
        let mut i = 0;
        let end = n * 5_000 + 200_000;
        let mut t = 0;
        while t < end {
            while i < arrivals.len() && arrivals[i].0 <= t {
                let s = arrivals[i].1;
                jb.push(s, s * 5_000, arrivals[i].0, pkt(s));
                i += 1;
            }
            out.push(jb.pull());
            t += 5_000;
        }
        out
    }

    #[test]
    fn steady_stream_plays_every_packet_in_order() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let pulls = run(&mut jb, 200, |_| 1_000, &[]);
        let played: Vec<u8> = pulls.iter().filter_map(|p| if let Pull::Packet(d) = p { Some(d[0]) } else { None }).collect();
        assert_eq!(played, (0..200u64).map(|s| s as u8).collect::<Vec<_>>());
        assert_eq!(jb.target_us(), 10_000);
        // Nothing concealed until the stream ends (then a few frames, then silence).
        let last = pulls.iter().rposition(|p| matches!(p, Pull::Packet(_))).unwrap();
        assert!(!pulls[..last].contains(&Pull::Conceal));
        assert_eq!(pulls.last(), Some(&Pull::Silence));
        assert_eq!(jb.stats.dropped, 0);
    }

    #[test]
    fn a_lost_packet_uses_the_next_ones_fec() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let pulls = run(&mut jb, 100, |_| 1_000, &[40]);
        let i = pulls.iter().position(|p| *p == Pull::Fec(pkt(41))).expect("fec for 40");
        assert_eq!(pulls[i + 1], Pull::Packet(pkt(41)));
        assert_eq!(pulls[i - 1], Pull::Packet(pkt(39)));
        assert_eq!(jb.stats.fec, 1);
    }

    #[test]
    fn two_lost_in_a_row_conceal_then_fec() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let pulls = run(&mut jb, 100, |_| 1_000, &[40, 41]);
        let i = pulls.iter().position(|p| *p == Pull::Packet(pkt(39))).unwrap();
        assert_eq!(pulls[i + 1], Pull::Conceal);
        assert_eq!(pulls[i + 2], Pull::Fec(pkt(42)));
        assert_eq!(pulls[i + 3], Pull::Packet(pkt(42)));
    }

    #[test]
    fn late_packets_are_dropped() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // Packet 50 arrives 60 ms late: past its turn at a 10 ms target.
        let pulls = run(&mut jb, 100, |s| if s == 50 { 60_000 } else { 1_000 }, &[]);
        assert!(!pulls.contains(&Pull::Packet(pkt(50))));
        assert_eq!(jb.stats.late, 1);
    }

    #[test]
    fn jitter_raises_the_target_within_bounds() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // Up to 18 ms of jitter, spread evenly.
        run(&mut jb, 400, |s| (s * 7919 % 19) * 1_000, &[]);
        let t = jb.target_us();
        assert!((20_000..=30_000).contains(&t), "target {t}");
        // With 18 ms of jitter at that target, nothing is concealed for lateness once
        // started, apart from the first refill.
        let mut jb2 = JitterBuffer::new(JitterConfig::default());
        run(&mut jb2, 400, |_| 0, &[]);
        assert_eq!(jb2.target_us(), 10_000);
        // A huge spread is capped.
        let mut jb3 = JitterBuffer::new(JitterConfig::default());
        run(&mut jb3, 400, |s| (s % 2) * 300_000, &[]);
        assert_eq!(jb3.target_us(), 80_000);
    }

    #[test]
    fn floor_holds_audio_back_for_video() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        jb.set_floor(25_000);
        assert_eq!(jb.target_us(), 25_000);
        let pulls = run(&mut jb, 50, |_| 0, &[]);
        // Starts once 25 ms is buffered: five frames of silence first.
        let first = pulls.iter().position(|p| matches!(p, Pull::Packet(_))).unwrap();
        assert!(first >= 4, "started at pull {first}");
    }

    #[test]
    fn underrun_conceals_then_refills() {
        let cfg = JitterConfig::default();
        let mut jb = JitterBuffer::new(cfg);
        for s in 0..4 {
            jb.push(s, s * 5_000, s * 5_000, pkt(s));
        }
        let mut pulls = Vec::new();
        for _ in 0..12 {
            pulls.push(jb.pull());
        }
        assert_eq!(&pulls[..4], &[Pull::Packet(pkt(0)), Pull::Packet(pkt(1)), Pull::Packet(pkt(2)), Pull::Packet(pkt(3))]);
        assert_eq!(pulls[4..4 + cfg.max_conceal as usize].iter().filter(|p| **p == Pull::Conceal).count(), cfg.max_conceal as usize);
        assert_eq!(pulls[11], Pull::Silence);
        assert_eq!(jb.stats.underruns, 1);
        // Refills, then resumes from what's there.
        for s in 20..23 {
            jb.push(s, s * 5_000, s * 5_000, pkt(s));
        }
        assert_eq!(jb.pull(), Pull::Packet(pkt(20)));
    }

    #[test]
    fn a_growing_buffer_drops_frames() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // The host's clock runs 0.5% fast (real clocks are far closer, but this keeps the
        // test short): 201 packets arrive per 200 pulls.
        let mut seq = 0u64;
        let mut t = 0u64;
        for i in 0..20_000u64 {
            let due = i * 201 / 200;
            while seq <= due {
                jb.push(seq, seq * 5_000, t, pkt(seq));
                seq += 1;
            }
            jb.pull();
            t += 5_000;
        }
        assert!(jb.stats.dropped > 60, "dropped {}", jb.stats.dropped);
        assert!(jb.depth_us() <= jb.target_us() + 20_000, "depth {}", jb.depth_us());
    }
}
