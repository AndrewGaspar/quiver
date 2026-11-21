//! How one direction of the link is shaped, and the `tc` commands that do it.
//!
//! Each direction is a chain of kernel qdiscs on the router's egress interface:
//!
//! ```text
//! netem (delay, jitter, loss, duplication, reordering, corruption)
//!   └─ tbf (the bottleneck's rate, and its FIFO queue: queue_ms or queue_packets)
//!        └─ fq_codel            (aqm = "fq_codel")
//!   └─ cake (rate and AQM in one) (aqm = "cake")
//! ```
//!
//! netem always gets a 10 Gbit/s `rate`. That doesn't limit anything; it switches netem to
//! scheduling each packet after the one ahead of it, so jitter delays the packets behind a
//! late one (as a Wi-Fi retry or a cellular scheduler does) rather than reordering them.
//! Reordering is its own knob (`reorder_pct`). netem's own queue only holds the delay line
//! (up to 100 000 packets), so the bottleneck's queue size is the tbf's, and a bufferbloated
//! link is a large `queue_ms` with `aqm = "fifo"`.

use serde::{Deserialize, Serialize};

/// Packets in netem's delay line. Enough for 1 Gbit/s × 1 s of 1500-byte packets.
const NETEM_LIMIT: u32 = 100_000;
/// The largest frame on the veths (1500 MTU + Ethernet header).
pub const FRAME: u64 = 1514;

/// The distribution of netem's per-packet delay around `delay_ms` (tc's tables in
/// `/usr/lib/tc`). `uniform` is netem's default: anywhere in `delay ± jitter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Distribution {
    Uniform,
    #[default]
    Normal,
    Pareto,
    /// A normal body with a Pareto tail: mostly small jitter with rare large spikes.
    Paretonormal,
}

/// The queue discipline at the bottleneck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Aqm {
    /// A tail-drop FIFO of `queue_ms`/`queue_packets`: what most home routers, modems and
    /// cellular base stations do, and what makes bufferbloat.
    #[default]
    Fifo,
    /// tbf with fq_codel under it: flow queueing and a 5 ms sojourn target.
    FqCodel,
    /// CAKE: shaping, flow isolation and COBALT AQM in one qdisc.
    Cake,
}

/// Packet loss. `burst` above 1 makes it bursty with a two-state Gilbert model (netem
/// `loss gemodel`): in the bad state every packet is lost, in the good state none, and the
/// mean time in the bad state is `burst` packets.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Loss {
    /// The mean loss, in percent.
    pub pct: f64,
    /// The mean length of a loss burst, in packets. 1 (or absent) means independent loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<f64>,
}

impl Loss {
    /// The Gilbert–Elliott transition probabilities (percent): good → bad `p`, bad → good `r`.
    /// The stationary loss is p / (p + r) and the mean burst 1 / r.
    pub fn gemodel(&self) -> Option<(f64, f64)> {
        let burst = self.burst.filter(|&b| b > 1.0)?;
        let mean = (self.pct / 100.0).clamp(0.0, 0.99);
        let r = 1.0 / burst;
        let p = mean * r / (1.0 - mean);
        Some((p * 100.0, r * 100.0))
    }
}

/// A bounded random walk of the rate, for cellular and busy Wi-Fi links whose capacity moves
/// with the radio. Every `every_ms` the rate moves by up to ±`step_pct`, staying in
/// [`min_mbit`, `max_mbit`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wander {
    pub min_mbit: f64,
    pub max_mbit: f64,
    #[serde(default = "Wander::default_every")]
    pub every_ms: u64,
    #[serde(default = "Wander::default_step")]
    pub step_pct: f64,
}

impl Wander {
    fn default_every() -> u64 {
        500
    }
    fn default_step() -> f64 {
        15.0
    }

    /// The next rate after `now`, from a uniform draw `u` in [0, 1).
    pub fn step(&self, now_mbit: f64, u: f64) -> f64 {
        let factor = 1.0 + (u * 2.0 - 1.0) * self.step_pct / 100.0;
        (now_mbit * factor).clamp(self.min_mbit, self.max_mbit)
    }
}

/// netem `slot`: packets leave in bursts at slot boundaries, like Wi-Fi aggregation or a
/// cellular scheduler's transmission opportunities.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub min_ms: f64,
    pub max_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packets: Option<u32>,
}

/// One direction of the link, fully resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkShape {
    /// One-way delay, in ms.
    pub delay_ms: f64,
    /// Delay variation, in ms (the distribution's spread; σ for `normal`).
    pub jitter_ms: f64,
    pub distribution: Distribution,
    pub loss: Loss,
    pub duplicate_pct: f64,
    /// Packets sent ahead of the delay line (out of order). Needs `delay_ms` > 0.
    pub reorder_pct: f64,
    pub corrupt_pct: f64,
    /// The bottleneck rate, in Mbit/s. 0 means unshaped.
    pub rate_mbit: f64,
    /// The bottleneck's queue as time at `rate_mbit` (for `fifo`). Ignored when
    /// `queue_packets` is set.
    pub queue_ms: f64,
    pub queue_packets: Option<u32>,
    pub aqm: Aqm,
    pub wander: Option<Wander>,
    pub slot: Option<Slot>,
}

impl Default for LinkShape {
    fn default() -> Self {
        LinkShape {
            delay_ms: 0.0,
            jitter_ms: 0.0,
            distribution: Distribution::Normal,
            loss: Loss::default(),
            duplicate_pct: 0.0,
            reorder_pct: 0.0,
            corrupt_pct: 0.0,
            rate_mbit: 0.0,
            queue_ms: 50.0,
            queue_packets: None,
            aqm: Aqm::Fifo,
            wander: None,
            slot: None,
        }
    }
}

/// A partial [`LinkShape`], as profiles and scenario events write them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<Distribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<Loss>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reorder_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrupt_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_mbit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_packets: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aqm: Option<Aqm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wander: Option<Wander>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<Slot>,
}

impl LinkShape {
    /// This shape with a patch's fields over it. Setting `rate_mbit` without `wander` stops
    /// the wander: a fixed rate was asked for.
    pub fn patched(&self, p: &LinkPatch) -> LinkShape {
        let mut s = self.clone();
        macro_rules! take {
            ($($f:ident),*) => { $( if let Some(v) = p.$f { s.$f = v; } )* };
        }
        take!(delay_ms, jitter_ms, distribution, loss, duplicate_pct, reorder_pct, corrupt_pct, rate_mbit, queue_ms, aqm);
        if p.queue_packets.is_some() {
            s.queue_packets = p.queue_packets;
        }
        if p.queue_ms.is_some() && p.queue_packets.is_none() {
            s.queue_packets = None;
        }
        if p.wander.is_some() {
            s.wander = p.wander;
        } else if p.rate_mbit.is_some() {
            s.wander = None;
        }
        if p.slot.is_some() {
            s.slot = p.slot;
        }
        s
    }

    /// The queue at the bottleneck in bytes (tbf's `limit`).
    pub fn queue_bytes(&self) -> u64 {
        let bytes = match self.queue_packets {
            Some(n) => n as u64 * FRAME,
            None => (self.queue_ms / 1000.0 * self.rate_mbit * 1e6 / 8.0) as u64,
        };
        bytes.max(2 * FRAME)
    }

    /// The bucket for tbf: 1 ms at the rate, at least two frames.
    pub fn burst_bytes(&self) -> u64 {
        ((self.rate_mbit * 1e6 / 8.0 / 1000.0) as u64).max(2 * FRAME)
    }

    /// The queueing delay a full bottleneck FIFO adds, in ms.
    pub fn full_queue_ms(&self) -> f64 {
        if self.rate_mbit <= 0.0 {
            return 0.0;
        }
        self.queue_bytes() as f64 * 8.0 / (self.rate_mbit * 1e6) * 1000.0
    }

    /// The expected loss in percent: the Gilbert model's stationary loss, or the random one.
    pub fn mean_loss_pct(&self) -> f64 {
        match self.loss.gemodel() {
            Some((p, r)) => p / (p + r) * 100.0,
            None => self.loss.pct,
        }
    }

    fn structure(&self) -> Structure {
        match (self.rate_mbit > 0.0, self.aqm) {
            (false, Aqm::Fifo) => Structure::NetemOnly,
            (false, Aqm::FqCodel) | (true, Aqm::FqCodel) => Structure::TbfFqCodel,
            (true, Aqm::Fifo) => Structure::Tbf,
            (_, Aqm::Cake) => Structure::Cake,
        }
    }

    /// netem's arguments.
    pub fn netem_args(&self) -> String {
        let mut a = format!("limit {NETEM_LIMIT}");
        let us = |ms: f64| format!("{}us", (ms * 1000.0).round() as u64);
        if self.delay_ms > 0.0 || self.jitter_ms > 0.0 {
            a += &format!(" delay {}", us(self.delay_ms));
            if self.jitter_ms > 0.0 {
                a += &format!(" {}", us(self.jitter_ms));
                if self.distribution != Distribution::Uniform {
                    a += &format!(
                        " distribution {}",
                        match self.distribution {
                            Distribution::Uniform => unreachable!(),
                            Distribution::Normal => "normal",
                            Distribution::Pareto => "pareto",
                            Distribution::Paretonormal => "paretonormal",
                        }
                    );
                }
            }
        }
        if self.reorder_pct > 0.0 && self.delay_ms > 0.0 {
            a += &format!(" reorder {}%", pct(self.reorder_pct));
        }
        if self.loss.pct > 0.0 {
            match self.loss.gemodel() {
                Some((p, r)) => a += &format!(" loss gemodel {}% {}% 100% 0%", pct(p), pct(r)),
                None => a += &format!(" loss random {}%", pct(self.loss.pct)),
            }
        }
        if self.duplicate_pct > 0.0 {
            a += &format!(" duplicate {}%", pct(self.duplicate_pct));
        }
        if self.corrupt_pct > 0.0 {
            a += &format!(" corrupt {}%", pct(self.corrupt_pct));
        }
        a += " rate 10gbit";
        if let Some(slot) = self.slot {
            a += &format!(" slot {} {}", us(slot.min_ms), us(slot.max_ms));
            if let Some(n) = slot.packets {
                a += &format!(" packets {n}");
            }
        }
        a
    }

    fn rate_bits(&self) -> u64 {
        (self.rate_mbit * 1e6).round() as u64
    }

    /// The `tc -batch` lines that build this shape on `dev` from nothing (after
    /// `qdisc del dev <dev> root`).
    pub fn tc_build(&self, dev: &str) -> String {
        let mut s = format!("qdisc add dev {dev} root handle 1: netem {}\n", self.netem_args());
        s += &self.tc_children(dev, "add");
        s
    }

    /// The `tc -batch` lines that turn `prev` into this shape in place, keeping the queued
    /// packets, or None when the qdisc tree's structure differs and it must be rebuilt.
    pub fn tc_change(&self, prev: &LinkShape, dev: &str) -> Option<String> {
        if self.structure() != prev.structure() {
            return None;
        }
        let mut s = String::new();
        if self.netem_args() != prev.netem_args() {
            s += &format!("qdisc change dev {dev} root handle 1: netem {}\n", self.netem_args());
        }
        let children = self.tc_children(dev, "change");
        if children != prev.tc_children(dev, "change") {
            s += &children;
        }
        Some(s)
    }

    fn tc_children(&self, dev: &str, verb: &str) -> String {
        let fq_limit = self.queue_packets.map(|n| format!(" limit {n}")).unwrap_or_default();
        match self.structure() {
            Structure::NetemOnly => String::new(),
            Structure::Tbf => format!(
                "qdisc {verb} dev {dev} parent 1: handle 2: tbf rate {}bit burst {} limit {}\n",
                self.rate_bits(),
                self.burst_bytes(),
                self.queue_bytes()
            ),
            Structure::TbfFqCodel => {
                if self.rate_mbit > 0.0 {
                    format!(
                        "qdisc {verb} dev {dev} parent 1: handle 2: tbf rate {}bit burst {} limit {}\n\
                         qdisc {verb} dev {dev} parent 2:1 handle 3: fq_codel{fq_limit}\n",
                        self.rate_bits(),
                        self.burst_bytes(),
                        self.queue_bytes().max(64 * FRAME)
                    )
                } else {
                    format!("qdisc {verb} dev {dev} parent 1: handle 2: fq_codel{fq_limit}\n")
                }
            }
            Structure::Cake => {
                let bw = if self.rate_mbit > 0.0 { format!("bandwidth {}bit", self.rate_bits()) } else { "unlimited".into() };
                format!("qdisc {verb} dev {dev} parent 1: handle 2: cake {bw} besteffort\n")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Structure {
    NetemOnly,
    Tbf,
    TbfFqCodel,
    Cake,
}

/// Percent with up to four decimals and no trailing zeros, as tc reads it.
fn pct(v: f64) -> String {
    let s = format!("{:.4}", v.clamp(0.0, 100.0));
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemodel_matches_mean_and_burst() {
        let l = Loss { pct: 2.0, burst: Some(4.0) };
        let (p, r) = l.gemodel().unwrap();
        assert!((r - 25.0).abs() < 1e-9);
        assert!((p / (p + r) - 0.02).abs() < 1e-9);
        assert!(Loss { pct: 2.0, burst: Some(1.0) }.gemodel().is_none());
    }

    #[test]
    fn netem_args_cover_every_knob() {
        let s = LinkShape {
            delay_ms: 20.0,
            jitter_ms: 5.5,
            distribution: Distribution::Paretonormal,
            loss: Loss { pct: 1.0, burst: None },
            duplicate_pct: 0.5,
            reorder_pct: 1.0,
            corrupt_pct: 0.1,
            ..Default::default()
        };
        assert_eq!(
            s.netem_args(),
            "limit 100000 delay 20000us 5500us distribution paretonormal reorder 1% loss random 1% \
             duplicate 0.5% corrupt 0.1% rate 10gbit"
        );
    }

    #[test]
    fn fifo_queue_is_time_at_rate() {
        let s = LinkShape { rate_mbit: 20.0, queue_ms: 500.0, ..Default::default() };
        assert_eq!(s.queue_bytes(), 1_250_000);
        assert!((s.full_queue_ms() - 500.0).abs() < 0.01);
        let b = s.tc_build("cli");
        assert!(b.contains("tbf rate 20000000bit burst 3028 limit 1250000"), "{b}");
    }

    #[test]
    fn change_keeps_structure_or_asks_for_rebuild() {
        let a = LinkShape { rate_mbit: 200.0, ..Default::default() };
        let b = a.patched(&LinkPatch { rate_mbit: Some(20.0), ..Default::default() });
        let c = b.tc_change(&a, "cli").unwrap();
        assert!(c.starts_with("qdisc change dev cli parent 1: handle 2: tbf rate 20000000bit"), "{c}");
        assert!(!c.contains("netem"));
        let d = a.patched(&LinkPatch { aqm: Some(Aqm::Cake), ..Default::default() });
        assert!(d.tc_change(&a, "cli").is_none());
        assert_eq!(a.tc_change(&a, "cli").unwrap(), "");
    }

    #[test]
    fn a_fixed_rate_stops_the_wander() {
        let w = Wander { min_mbit: 10.0, max_mbit: 50.0, every_ms: 500, step_pct: 10.0 };
        let a = LinkShape { rate_mbit: 30.0, wander: Some(w), ..Default::default() };
        assert!(a.patched(&LinkPatch { delay_ms: Some(1.0), ..Default::default() }).wander.is_some());
        assert!(a.patched(&LinkPatch { rate_mbit: Some(5.0), ..Default::default() }).wander.is_none());
        for u in [0.0, 0.5, 0.999] {
            let r = w.step(49.0, u);
            assert!((10.0..=50.0).contains(&r));
        }
    }
}
