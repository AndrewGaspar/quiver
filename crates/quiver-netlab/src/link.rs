//! Applying [`LinkShape`]s to the lab's router, and reading back what the qdiscs did.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ns::{Dir, Lab, Ns};
use crate::profile::Profile;
use crate::shape::LinkShape;

/// The shaping on both directions, with the shapes applied now so a change can be made in
/// place (keeping queued packets) when the qdisc tree's structure allows.
pub struct Shaper<'a> {
    lab: &'a Lab,
    current: [Option<LinkShape>; 2],
}

fn idx(dir: Dir) -> usize {
    match dir {
        Dir::ToClient => 0,
        Dir::ToServer => 1,
    }
}

impl<'a> Shaper<'a> {
    pub fn new(lab: &'a Lab) -> Self {
        Shaper { lab, current: [None, None] }
    }

    pub fn lab(&self) -> &'a Lab {
        self.lab
    }

    pub fn current(&self, dir: Dir) -> Option<&LinkShape> {
        self.current[idx(dir)].as_ref()
    }

    /// Shapes one direction as `shape`.
    pub fn apply(&mut self, dir: Dir, shape: &LinkShape) -> Result<()> {
        let dev = dir.router_dev();
        let change = self.current[idx(dir)].as_ref().and_then(|prev| shape.tc_change(prev, dev));
        match change {
            Some(script) if script.is_empty() => {}
            Some(script) => self.lab.tc_batch(Ns::Router, &script)?,
            None => {
                // Nothing there yet (veths start with noqueue) or a different tree: rebuild.
                let _ = self.lab.run(Ns::Router, &["tc", "qdisc", "del", "dev", dev, "root"]);
                self.lab.tc_batch(Ns::Router, &shape.tc_build(dev)).with_context(|| format!("shaping {}", dir.name()))?;
            }
        }
        self.current[idx(dir)] = Some(shape.clone());
        Ok(())
    }

    pub fn apply_profile(&mut self, p: &Profile) -> Result<()> {
        self.apply(Dir::ToClient, &p.to_client)?;
        self.apply(Dir::ToServer, &p.to_server)
    }

    /// The qdiscs' counters for one direction.
    pub fn counters(&self, dir: Dir) -> Result<QdiscCounters> {
        let out = self.lab.run(Ns::Router, &["tc", "-s", "-j", "qdisc", "show", "dev", dir.router_dev()])?;
        QdiscCounters::parse(&out)
    }
}

/// The rate random walk for shapes that have a [`Wander`](crate::shape::Wander).
pub struct WanderState {
    rng: fastrand::Rng,
    next: [Option<std::time::Instant>; 2],
}

impl WanderState {
    pub fn new(seed: u64) -> Self {
        WanderState { rng: fastrand::Rng::with_seed(seed), next: [None, None] }
    }

    /// Takes the walk's steps that are due on the shaper's current shapes, and returns the
    /// rates it applied.
    pub fn tick(&mut self, shaper: &mut Shaper) -> Result<Vec<(Dir, f64)>> {
        let now = std::time::Instant::now();
        let mut changed = Vec::new();
        for dir in Dir::BOTH {
            let Some(shape) = shaper.current(dir).cloned() else { continue };
            let Some(w) = shape.wander else {
                self.next[idx(dir)] = None;
                continue;
            };
            let every = std::time::Duration::from_millis(w.every_ms.max(20));
            match self.next[idx(dir)] {
                None => self.next[idx(dir)] = Some(now + every),
                Some(t) if now >= t => {
                    let mut s = shape.clone();
                    s.rate_mbit = w.step(shape.rate_mbit, self.rng.f64());
                    shaper.apply(dir, &s)?;
                    changed.push((dir, s.rate_mbit));
                    self.next[idx(dir)] = Some(t + every);
                }
                Some(_) => {}
            }
        }
        Ok(changed)
    }
}

/// What the qdiscs on one direction have done since they were built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct QdiscCounters {
    /// Bytes and packets that left the root (onto the wire).
    pub sent_bytes: u64,
    pub sent_packets: u64,
    /// Drops anywhere in the tree: netem's loss plus the bottleneck's overflow.
    pub drops: u64,
    /// Drops at the bottleneck queue (tbf, fq_codel or cake): congestion, not random loss.
    pub queue_drops: u64,
    /// Bytes waiting at the bottleneck now.
    pub queue_bytes: u64,
}

#[derive(Deserialize)]
struct TcQdisc {
    kind: String,
    #[serde(default)]
    root: bool,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    packets: u64,
    #[serde(default)]
    drops: u64,
    #[serde(default)]
    backlog: u64,
}

impl QdiscCounters {
    /// Parses `tc -s -j qdisc show dev <dev>`.
    pub fn parse(json: &str) -> Result<QdiscCounters> {
        let qdiscs: Vec<TcQdisc> = serde_json::from_str(json).context("parsing tc -j output")?;
        let mut c = QdiscCounters::default();
        for q in &qdiscs {
            if q.root {
                c.sent_bytes = q.bytes;
                c.sent_packets = q.packets;
                c.drops = q.drops;
            } else if q.kind != "netem" {
                c.queue_drops += q.drops;
                // fq_codel sits under tbf and both report the same backlog; take the larger.
                c.queue_bytes = c.queue_bytes.max(q.backlog);
            }
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tc_json() {
        let json = r#"[{"kind":"netem","handle":"1:","root":true,"bytes":1000,"packets":10,"drops":3,"backlog":180},
                       {"kind":"tbf","handle":"2:","parent":"1:","bytes":900,"packets":9,"drops":2,"backlog":3000}]"#;
        let c = QdiscCounters::parse(json).unwrap();
        assert_eq!(c, QdiscCounters { sent_bytes: 1000, sent_packets: 10, drops: 3, queue_drops: 2, queue_bytes: 3000 });
    }
}
