//! Round-trip time and the offset between the two sides' clocks.
//!
//! Each side pings the other every [`PING_INTERVAL`]. A ping carries its send time `t1`; the
//! pong carries `t1` back with the peer's receive and send times `t2` and `t3`; the pinger
//! notes `t4` when the pong arrives. As in NTP:
//!
//! ```text
//! rtt    = (t4 - t1) - (t3 - t2)
//! offset = ((t2 - t1) + (t3 - t4)) / 2      (peer clock minus ours)
//! ```
//!
//! The offset is only as good as the path is symmetric, and queueing makes it worse, so the
//! estimate is the offset of the sample with the smallest RTT among the recent ones. Clocks
//! drift a few tens of ppm apart, so the window is kept short.

use std::collections::VecDeque;

use quiver_stream_proto::Micros;

/// How often each side pings.
pub const PING_INTERVAL: Micros = 100_000;
/// Samples kept for the offset (about 3 s of pings).
const WINDOW: usize = 32;

#[derive(Debug, Clone, Copy)]
struct Sample {
    rtt: u64,
    offset: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ClockSync {
    samples: VecDeque<Sample>,
    srtt: Option<f64>,
    rttvar: f64,
    latest: Option<u64>,
}

impl ClockSync {
    /// Takes one ping's four timestamps: ours at send and receive, the peer's at receive and
    /// send.
    pub fn on_pong(&mut self, t1: Micros, t2: Micros, t3: Micros, t4: Micros) {
        let local = t4.saturating_sub(t1) as i64;
        let remote = t3.saturating_sub(t2) as i64;
        let rtt = (local - remote).max(0) as u64;
        let offset = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
        self.samples.push_back(Sample { rtt, offset });
        if self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
        self.latest = Some(rtt);
        self.update_srtt(rtt as f64);
    }

    /// RFC 6298 smoothing.
    fn update_srtt(&mut self, rtt: f64) {
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.rttvar = rtt / 2.0;
            }
            Some(s) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (s - rtt).abs();
                self.srtt = Some(0.875 * s + 0.125 * rtt);
            }
        }
    }

    /// The smoothed RTT in µs, once a pong has arrived.
    pub fn rtt(&self) -> Option<u64> {
        self.srtt.map(|s| s as u64)
    }

    /// The smallest RTT in the window.
    pub fn min_rtt(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.rtt).min()
    }

    pub fn latest_rtt(&self) -> Option<u64> {
        self.latest
    }

    /// The RTT's mean deviation, in µs.
    pub fn rtt_var(&self) -> u64 {
        self.rttvar as u64
    }

    /// The peer's clock minus ours, in µs.
    pub fn offset(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|s| s.rtt).map(|s| s.offset)
    }

    /// How long to wait for an ack before resending: SRTT + 4·RTTVAR, at least `floor`.
    /// Before any RTT sample, 50 ms.
    pub fn rto(&self, floor: Micros) -> Micros {
        match self.srtt {
            Some(s) => ((s + 4.0 * self.rttvar) as Micros).max(floor),
            None => 50_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_path_gives_exact_offset() {
        let mut c = ClockSync::default();
        // Peer clock is 5 s ahead; 300 µs each way; the peer takes 10 µs to answer.
        let t1 = 1_000_000;
        let t2 = t1 + 300 + 5_000_000;
        let t3 = t2 + 10;
        let t4 = t1 + 610;
        c.on_pong(t1, t2, t3, t4);
        assert_eq!(c.rtt(), Some(600));
        assert_eq!(c.offset(), Some(5_000_000));
    }

    #[test]
    fn offset_comes_from_the_least_queued_sample() {
        let mut c = ClockSync::default();
        let ahead: u64 = 1_000;
        // A queued sample: 5 ms extra on the way there skews its offset by 2.5 ms.
        c.on_pong(0, 5_300 + ahead, 5_300 + ahead, 5_600);
        // A clean one.
        c.on_pong(10_000, 10_300 + ahead, 10_300 + ahead, 10_600);
        assert_eq!(c.offset(), Some(ahead as i64));
        assert_eq!(c.min_rtt(), Some(600));
        assert!(c.rtt().unwrap() > 600, "smoothed includes the queued one");
    }

    #[test]
    fn rto_has_a_floor_and_a_default() {
        let mut c = ClockSync::default();
        assert_eq!(c.rto(1_000), 50_000);
        c.on_pong(0, 100, 100, 200);
        assert_eq!(c.rto(5_000), 5_000);
    }
}
