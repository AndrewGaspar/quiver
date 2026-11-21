//! The client's input, into the stream's display only: keyboard and mouse through XTEST on
//! gamescope's Xwayland ([`x11`]), pads as uinput devices ([`pads`]).
//!
//! Events arrive with the client's sequence number; the transport sends each one several
//! times, so any already applied is dropped here ([`Dedup`]).

pub mod pads;
pub mod x11;

use quiver_stream_proto::input::Stamped;

/// Drops events already applied, by the client's `seq`.
#[derive(Debug, Default)]
pub struct Dedup {
    /// The highest seq applied, and which of the 64 before it were.
    high: Option<u64>,
    seen: u64,
}

impl Dedup {
    /// True when `seq` is new (and records it). Events more than 64 behind the newest are
    /// treated as already applied.
    pub fn fresh(&mut self, seq: u64) -> bool {
        let Some(high) = self.high else {
            self.high = Some(seq);
            self.seen = 0;
            return true;
        };
        if seq > high {
            let shift = seq - high;
            self.seen = if shift >= 64 { 0 } else { (self.seen << shift) | (1 << (shift - 1)) };
            self.high = Some(seq);
            return true;
        }
        let back = high - seq;
        if back == 0 || back > 64 {
            return false;
        }
        let bit = 1u64 << (back - 1);
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }

    pub fn filter(&mut self, s: &Stamped) -> bool {
        self.fresh(s.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_and_reordering() {
        let mut d = Dedup::default();
        assert!(d.fresh(5));
        assert!(!d.fresh(5));
        assert!(d.fresh(7));
        assert!(d.fresh(6), "late but new");
        assert!(!d.fresh(6));
        assert!(!d.fresh(7));
        assert!(d.fresh(200));
        assert!(!d.fresh(100), "far behind");
        assert!(d.fresh(199));
    }
}
