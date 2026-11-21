//! Scenarios: a base profile and a timeline of events that change it.
//!
//! ```toml
//! name = "step-down"
//! summary = "..."
//! base = "wifi6-good"     # a profile; [both]/[to_client]/[to_server] patch it
//! duration_s = 40
//!
//! [[event]]
//! at_s = 10
//! kind = "set"            # set | restore | outage | roam | ramp | flood
//! to_client = { rate_mbit = 20 }
//! for_s = 5               # optional: put the touched fields back after this long
//! ```
//!
//! Event kinds:
//! - `set`: patch `both`, `to_client` and/or `to_server`; with `for_s`, revert afterwards.
//! - `restore`: back to the base.
//! - `outage`: 100% loss both ways for `ms`.
//! - `roam`: the client's address changes ([`Lab::roam`](crate::Lab::roam)), with an outage
//!   of `gap_ms` around it (0 for none).
//! - `ramp`: move `field` (`delay_ms`, `jitter_ms`, `rate_mbit` or `loss_pct`) on `dir`
//!   (`both`, `to_client`, `to_server`) linearly `to` a value `over_s`, in `step_ms` steps.
//! - `flood`: a competing bulk flow on `dir` for `for_s`: `proto = "tcp"` (as fast as TCP
//!   goes) or `"udp"` at `rate_mbit`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::ns::Dir;
use crate::profile::{self, Profile};
use crate::shape::{LinkPatch, LinkShape, Loss};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Set,
    Restore,
    Outage,
    Roam,
    Ramp,
    Flood,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DirSel {
    #[default]
    Both,
    ToClient,
    ToServer,
}

impl DirSel {
    fn dirs(self) -> &'static [Dir] {
        match self {
            DirSel::Both => &Dir::BOTH,
            DirSel::ToClient => &[Dir::ToClient],
            DirSel::ToServer => &[Dir::ToServer],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RampField {
    DelayMs,
    JitterMs,
    RateMbit,
    LossPct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloodProto {
    Tcp,
    Udp,
}

/// One event as written. Which fields apply depends on `kind` (see the module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub at_s: f64,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub both: Option<LinkPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_client: Option<LinkPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_server: Option<LinkPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<RampField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<DirSel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<FloodProto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_mbit: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioFile {
    name: String,
    summary: String,
    base: String,
    duration_s: f64,
    #[serde(default)]
    both: LinkPatch,
    #[serde(default)]
    to_client: LinkPatch,
    #[serde(default)]
    to_server: LinkPatch,
    #[serde(default, rename = "event")]
    events: Vec<Event>,
}

/// A scenario, its base resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub summary: String,
    /// The base profile with the scenario's own patches applied.
    pub base: Profile,
    pub duration_s: f64,
    pub events: Vec<Event>,
}

/// What the runner does at a point in the timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
// A timeline has tens to hundreds of steps; boxing the shapes would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum Op {
    /// Shape both directions like this from now on.
    Shape { to_client: LinkShape, to_server: LinkShape },
    Roam,
    FloodStart { id: usize, dir: Dir, proto: FloodProto, rate_mbit: Option<f64> },
    FloodStop { id: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub t_ms: u64,
    pub op: Op,
}

/// An event as the report marks it: a point, or a span when it has an end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub t_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    pub kind: EventKind,
    pub label: String,
    /// The event lowers the rate to the client: the matrix measures recovery after it.
    #[serde(default)]
    pub step_down: bool,
}

macro_rules! embedded {
    ($($name:literal),* $(,)?) => {
        /// The built-in scenarios, by name.
        pub const BUILTIN: &[(&str, &str)] = &[
            $( ($name, include_str!(concat!("../scenarios/", $name, ".toml"))), )*
        ];
    };
}

embedded!("step-down", "microwave", "outage", "roam", "latency-creep", "competing-tcp", "competing-udp");

impl Scenario {
    pub fn parse(text: &str) -> Result<Scenario> {
        let f: ScenarioFile = toml::from_str(text)?;
        let mut base = profile::load(&f.base).with_context(|| format!("base profile {}", f.base))?;
        base.to_client = base.to_client.patched(&f.both).patched(&f.to_client);
        base.to_server = base.to_server.patched(&f.both).patched(&f.to_server);
        let s = Scenario { name: f.name, summary: f.summary, base, duration_s: f.duration_s, events: f.events };
        s.timeline()?;
        Ok(s)
    }

    /// A profile held steady for `duration_s`.
    pub fn steady(p: &Profile, duration_s: f64) -> Scenario {
        Scenario {
            name: p.name.clone(),
            summary: p.summary.clone(),
            base: p.clone(),
            duration_s,
            events: vec![],
        }
    }

    pub fn duration_ms(&self) -> u64 {
        (self.duration_s * 1000.0) as u64
    }

    /// The steps to run, in time order (the first, at 0, applies the base), and the markers
    /// for the report.
    pub fn timeline(&self) -> Result<(Vec<Step>, Vec<Marker>)> {
        let mut steps = vec![Step {
            t_ms: 0,
            op: Op::Shape { to_client: self.base.to_client.clone(), to_server: self.base.to_server.clone() },
        }];
        let mut markers = Vec::new();
        let ms = |s: f64| (s * 1000.0).round() as u64;

        // Pending work, by time then by the order it was queued.
        let mut queue: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
        let mut items: Vec<Pending> = Vec::new();
        let push = |queue: &mut BinaryHeap<Reverse<(u64, usize)>>, items: &mut Vec<Pending>, t: u64, p: Pending| {
            items.push(p);
            queue.push(Reverse((t, items.len() - 1)));
        };

        let mut flood_id = 0;
        for (i, e) in self.events.iter().enumerate() {
            let t = ms(e.at_s);
            let label = e.label.clone().unwrap_or_else(|| default_label(e));
            let mut marker = Marker { t_ms: t, end_ms: None, kind: e.kind, label, step_down: false };
            match e.kind {
                EventKind::Set => {
                    if e.both.is_none() && e.to_client.is_none() && e.to_server.is_none() {
                        bail!("event {i}: `set` needs both, to_client or to_server");
                    }
                    let both = e.both.clone().unwrap_or_default();
                    let pc = merge(&both, e.to_client.as_ref());
                    let ps = merge(&both, e.to_server.as_ref());
                    let revert = e.for_s.map(ms);
                    marker.end_ms = revert.map(|r| t + r);
                    push(&mut queue, &mut items, t, Pending::Patch { pc, ps, revert });
                }
                EventKind::Restore => push(&mut queue, &mut items, t, Pending::Restore),
                EventKind::Outage => {
                    let len = e.ms.ok_or_else(|| anyhow!("event {i}: `outage` needs ms"))?;
                    marker.end_ms = Some(t + len);
                    let p = outage_patch();
                    push(&mut queue, &mut items, t, Pending::Patch { pc: p.clone(), ps: p, revert: Some(len) });
                }
                EventKind::Roam => {
                    let gap = e.gap_ms.unwrap_or(0);
                    if gap > 0 {
                        marker.end_ms = Some(t + gap);
                        let p = outage_patch();
                        push(&mut queue, &mut items, t, Pending::Patch { pc: p.clone(), ps: p, revert: Some(gap) });
                    }
                    push(&mut queue, &mut items, t + gap / 2, Pending::Roam);
                }
                EventKind::Ramp => {
                    let field = e.field.ok_or_else(|| anyhow!("event {i}: `ramp` needs field"))?;
                    let to = e.to.ok_or_else(|| anyhow!("event {i}: `ramp` needs to"))?;
                    let over = ms(e.over_s.ok_or_else(|| anyhow!("event {i}: `ramp` needs over_s"))?);
                    marker.end_ms = Some(t + over);
                    let step = e.step_ms.unwrap_or(250).max(10);
                    push(&mut queue, &mut items, t, Pending::Ramp { field, to, over, step, dir: e.dir.unwrap_or_default() });
                }
                EventKind::Flood => {
                    let proto = e.proto.ok_or_else(|| anyhow!("event {i}: `flood` needs proto"))?;
                    let dir = match e.dir.unwrap_or(DirSel::ToClient) {
                        DirSel::ToClient => Dir::ToClient,
                        DirSel::ToServer => Dir::ToServer,
                        DirSel::Both => bail!("event {i}: a flood goes one way (to_client or to_server)"),
                    };
                    if proto == FloodProto::Udp && e.rate_mbit.is_none() {
                        bail!("event {i}: a UDP flood needs rate_mbit");
                    }
                    let end = e.for_s.map(|f| t + ms(f)).unwrap_or(self.duration_ms());
                    marker.end_ms = Some(end);
                    push(&mut queue, &mut items, t, Pending::Flood(Op::FloodStart { id: flood_id, dir, proto, rate_mbit: e.rate_mbit }));
                    push(&mut queue, &mut items, end, Pending::Flood(Op::FloodStop { id: flood_id }));
                    flood_id += 1;
                }
            }
            markers.push(marker);
        }

        let mut cur = (self.base.to_client.clone(), self.base.to_server.clone());
        while let Some(Reverse((t, idx))) = queue.pop() {
            let p = items[idx].clone();
            let shape_before = cur.clone();
            match p {
                Pending::Patch { pc, ps, revert } => {
                    if let Some(r) = revert {
                        let back = Pending::Patch { pc: revert_of(&cur.0, &pc), ps: revert_of(&cur.1, &ps), revert: None };
                        push(&mut queue, &mut items, t + r, back);
                    }
                    cur = (cur.0.patched(&pc), cur.1.patched(&ps));
                }
                Pending::Restore => cur = (self.base.to_client.clone(), self.base.to_server.clone()),
                Pending::Roam => steps.push(Step { t_ms: t, op: Op::Roam }),
                Pending::Ramp { field, to, over, step, dir } => {
                    let n = (over / step).max(1);
                    let from: Vec<(Dir, f64)> =
                        dir.dirs().iter().map(|&d| (d, field_value(if d == Dir::ToClient { &cur.0 } else { &cur.1 }, field))).collect();
                    for k in 1..=n {
                        let frac = k as f64 / n as f64;
                        let mut pc = LinkPatch::default();
                        let mut ps = LinkPatch::default();
                        for &(d, f0) in &from {
                            let v = f0 + (to - f0) * frac;
                            set_field(if d == Dir::ToClient { &mut pc } else { &mut ps }, field, v);
                        }
                        push(&mut queue, &mut items, t + k * step, Pending::Patch { pc, ps, revert: None });
                    }
                }
                Pending::Flood(op) => steps.push(Step { t_ms: t, op }),
            }
            if cur != shape_before {
                steps.push(Step { t_ms: t, op: Op::Shape { to_client: cur.0.clone(), to_server: cur.1.clone() } });
            }
        }
        steps.sort_by_key(|s| s.t_ms);
        // A marker lowers the rate to the client if the shape it starts has a lower rate.
        for m in &mut markers {
            let before = rate_at(&steps, m.t_ms.saturating_sub(1), &self.base);
            let after = rate_at(&steps, m.t_ms, &self.base);
            m.step_down = after > 0.0 && (before == 0.0 || after < before * 0.8);
        }
        Ok((steps, markers))
    }
}

/// The rate to the client in force at `t_ms`.
fn rate_at(steps: &[Step], t_ms: u64, base: &Profile) -> f64 {
    let mut rate = base.to_client.rate_mbit;
    for s in steps {
        if s.t_ms > t_ms {
            break;
        }
        if let Op::Shape { to_client, .. } = &s.op {
            rate = to_client.rate_mbit;
        }
    }
    rate
}

#[derive(Debug, Clone)]
enum Pending {
    Patch { pc: LinkPatch, ps: LinkPatch, revert: Option<u64> },
    Restore,
    Roam,
    Ramp { field: RampField, to: f64, over: u64, step: u64, dir: DirSel },
    Flood(Op),
}

fn outage_patch() -> LinkPatch {
    LinkPatch { loss: Some(Loss { pct: 100.0, burst: None }), ..Default::default() }
}

fn default_label(e: &Event) -> String {
    match e.kind {
        EventKind::Set => "set".into(),
        EventKind::Restore => "restore".into(),
        EventKind::Outage => format!("{} ms outage", e.ms.unwrap_or(0)),
        EventKind::Roam => "roam".into(),
        EventKind::Ramp => "ramp".into(),
        EventKind::Flood => "competing flow".into(),
    }
}

/// `over` on top of `base`, field by field.
fn merge(base: &LinkPatch, over: Option<&LinkPatch>) -> LinkPatch {
    let Some(o) = over else { return base.clone() };
    macro_rules! pick {
        ($($f:ident),*) => { LinkPatch { $( $f: o.$f.clone().or(base.$f.clone()), )* } };
    }
    pick!(delay_ms, jitter_ms, distribution, loss, duplicate_pct, reorder_pct, corrupt_pct, rate_mbit, queue_ms, queue_packets, aqm, wander, slot)
}

/// The patch that puts back what `p` changes, from the shape before it.
fn revert_of(cur: &LinkShape, p: &LinkPatch) -> LinkPatch {
    let mut r = LinkPatch::default();
    macro_rules! back {
        ($($f:ident),*) => { $( if p.$f.is_some() { r.$f = Some(cur.$f.clone()); } )* };
    }
    back!(delay_ms, jitter_ms, distribution, loss, duplicate_pct, reorder_pct, corrupt_pct, rate_mbit, queue_ms, aqm);
    if p.queue_ms.is_some() || p.queue_packets.is_some() {
        r.queue_ms = Some(cur.queue_ms);
        r.queue_packets = cur.queue_packets;
    }
    if p.rate_mbit.is_some() || p.wander.is_some() {
        r.wander = cur.wander;
    }
    if p.slot.is_some() {
        r.slot = cur.slot;
    }
    r
}

fn field_value(s: &LinkShape, f: RampField) -> f64 {
    match f {
        RampField::DelayMs => s.delay_ms,
        RampField::JitterMs => s.jitter_ms,
        RampField::RateMbit => s.rate_mbit,
        RampField::LossPct => s.loss.pct,
    }
}

fn set_field(p: &mut LinkPatch, f: RampField, v: f64) {
    match f {
        RampField::DelayMs => p.delay_ms = Some(v),
        RampField::JitterMs => p.jitter_ms = Some(v),
        RampField::RateMbit => p.rate_mbit = Some(v),
        RampField::LossPct => p.loss = Some(Loss { pct: v, burst: None }),
    }
}

/// Every built-in scenario, parsed.
pub fn builtin() -> Vec<Scenario> {
    BUILTIN.iter().map(|(n, t)| Scenario::parse(t).unwrap_or_else(|e| panic!("scenario {n}: {e:#}"))).collect()
}

/// A scenario by built-in name, or from a `.toml` path.
pub fn load(name_or_path: &str) -> Result<Scenario> {
    if let Some((_, text)) = BUILTIN.iter().find(|(n, _)| *n == name_or_path) {
        return Scenario::parse(text);
    }
    if name_or_path.ends_with(".toml") || std::path::Path::new(name_or_path).exists() {
        let text = std::fs::read_to_string(name_or_path).with_context(|| format!("reading {name_or_path}"))?;
        return Scenario::parse(&text).with_context(|| format!("parsing {name_or_path}"));
    }
    Err(anyhow!(
        "no scenario `{name_or_path}`; built in: {}",
        BUILTIN.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes(steps: &[Step]) -> Vec<(u64, f64, f64)> {
        steps
            .iter()
            .filter_map(|s| match &s.op {
                Op::Shape { to_client, to_server } => Some((s.t_ms, to_client.rate_mbit, to_server.loss.pct)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_builtin_compiles() {
        for (file, text) in BUILTIN {
            let s = Scenario::parse(text).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            assert_eq!(&s.name, file);
            let (steps, markers) = s.timeline().unwrap();
            assert!(!markers.is_empty(), "{file} has no events");
            assert!(steps.iter().all(|st| st.t_ms <= s.duration_ms()), "{file} runs past its end");
        }
    }

    #[test]
    fn step_down_goes_down_and_back() {
        let s = load("step-down").unwrap();
        let (steps, markers) = s.timeline().unwrap();
        let r = shapes(&steps);
        assert_eq!(r[0].1, 200.0);
        assert!(r.iter().any(|&(t, rate, _)| t == 10_000 && rate == 20.0), "{r:?}");
        assert!(r.iter().any(|&(t, rate, _)| t == 25_000 && rate == 200.0), "{r:?}");
        assert!(markers[0].step_down && !markers[1].step_down);
    }

    #[test]
    fn outage_reverts_only_what_it_touched() {
        let s = load("outage").unwrap();
        let (steps, _) = s.timeline().unwrap();
        let r = shapes(&steps);
        let base_loss = s.base.to_server.loss.pct;
        assert_eq!(r, vec![(0, s.base.to_client.rate_mbit, base_loss), (10_000, s.base.to_client.rate_mbit, 100.0), (10_500, s.base.to_client.rate_mbit, base_loss)]);
    }

    #[test]
    fn ramp_moves_linearly_to_its_target() {
        let s = load("latency-creep").unwrap();
        let (steps, _) = s.timeline().unwrap();
        let delays: Vec<f64> = steps
            .iter()
            .filter_map(|st| match &st.op {
                Op::Shape { to_client, .. } => Some(to_client.delay_ms),
                _ => None,
            })
            .collect();
        assert!(delays.windows(2).all(|w| w[1] >= w[0]), "{delays:?}");
        assert!((delays.last().unwrap() - 120.0).abs() < 1e-6);
    }

    #[test]
    fn roam_happens_inside_its_gap() {
        let s = load("roam").unwrap();
        let (steps, _) = s.timeline().unwrap();
        let roams: Vec<u64> = steps.iter().filter(|s| s.op == Op::Roam).map(|s| s.t_ms).collect();
        assert_eq!(roams, vec![10_075, 20_000]);
    }
}
