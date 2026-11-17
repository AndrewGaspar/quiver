//! One session's perf registry (SIGHT-HUD §3.11): producers, the catalog (built-in and
//! declared descriptors, role bindings, canonical metrics), and an in-memory store of
//! recent values with session aggregates. Every value any consumer sees is computed here.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use quiver_api::perf::{FpsStats, MeanMax, MetricInfo, MetricState, Percentiles, PerfSummary, PerfValue, Presenter, ProducerInfo, Stat};
use quiver_perf::descriptor::{MetricDescriptor, SampleRate};
use quiver_perf::layout::LayerStatus;

use super::catalog::{self, Canonical, FrameStat, Source};
use super::format::{self, Raw};
use super::stats::{CounterSeries, FrameStore, NumSeries, WindowStats};

pub const WINDOW_1S: i64 = 1_000_000_000;
#[cfg(test)]
pub const WINDOW_10S: i64 = 10_000_000_000;
/// Window 0 means the whole session.
pub const WINDOW_SESSION: i64 = 0;
/// Without presents for this long, the frame metrics are stale.
const FRAMES_STALE_NS: i64 = 1_500_000_000;
/// Events kept per event metric.
const MAX_EVENTS: usize = 256;

#[derive(Debug, Default)]
enum Values {
    #[default]
    Empty,
    Num(NumSeries),
    Counter(CounterSeries),
    Text(String),
    State(usize),
    Events(Vec<(i64, Option<i64>, Option<String>)>),
}

#[derive(Debug)]
struct Metric {
    desc: MetricDescriptor,
    producer: String,
    values: Values,
    last_ns: Option<i64>,
    /// Roles this metric fills, and roles it claimed but another producer had first.
    roles: Vec<String>,
    conflicts: Vec<String>,
}

/// A producer's state.
#[derive(Debug, Clone, Default)]
pub struct Producer {
    pub info: ProducerInfo,
}

pub struct Store {
    pub started_ns: i64,
    pub started_at: DateTime<Utc>,
    metrics: BTreeMap<String, Metric>,
    canonical: Vec<Canonical>,
    /// role → metric id that fills it.
    roles: BTreeMap<String, String>,
    pub producers: BTreeMap<String, Producer>,
    pub frames: FrameStore,
    /// The layer's latest status block.
    pub layer_status: Option<LayerStatus>,
    /// Catalog revision: bumps when producers or descriptors change.
    pub rev: u64,
    /// Presenter time shares (ns) for the summary.
    pub presenter_ns: [i64; 2],
    presenter_mark: Option<(Presenter, i64)>,
    /// Ring timestamps rejected for being outside the session's window.
    pub out_of_window: u64,
}

fn period_ns(d: &MetricDescriptor) -> Option<i64> {
    match d.sample {
        Some(SampleRate::Hz(hz)) if hz > 0.0 => Some((1e9 / hz) as i64),
        Some(SampleRate::PerFrame) => Some(100_000_000),
        _ => None,
    }
}

impl Store {
    pub fn new(started_ns: i64, started_at: DateTime<Utc>) -> Store {
        let mut s = Store {
            started_ns,
            started_at,
            metrics: BTreeMap::new(),
            canonical: catalog::canonical(),
            roles: BTreeMap::new(),
            producers: BTreeMap::new(),
            frames: FrameStore::default(),
            layer_status: None,
            rev: 1,
            presenter_ns: [0, 0],
            presenter_mark: None,
            out_of_window: 0,
        };
        s.producer(catalog::PRODUCER_QUIVERD, "Quiver", true);
        s
    }

    /// Adds or updates a producer; returns whether anything changed.
    pub fn producer(&mut self, id: &str, label: &str, connected: bool) -> &mut Producer {
        let p = self.producers.entry(id.to_owned()).or_default();
        if p.info.id.is_empty() || p.info.connected != connected {
            self.rev += 1;
        }
        p.info.id = id.to_owned();
        p.info.label = label.to_owned();
        p.info.connected = connected;
        p
    }

    pub fn producer_mut(&mut self, id: &str) -> Option<&mut Producer> {
        self.producers.get_mut(id)
    }

    /// A producer went away: its metrics become `gone` (kept for the summary), its roles
    /// are released.
    pub fn disconnect(&mut self, producer: &str) {
        if let Some(p) = self.producers.get_mut(producer) {
            if p.info.connected {
                self.rev += 1;
            }
            p.info.connected = false;
        }
        self.roles.retain(|_, m| self.metrics.get(m).is_none_or(|x| x.producer != producer));
    }

    /// Declares (or re-declares) a metric for `producer`. Role claims bind if the role is
    /// free; otherwise they're recorded as conflicts (first wins, §3.4).
    pub fn declare(&mut self, producer: &str, desc: MetricDescriptor) {
        let id = desc.id.clone();
        let claims: Vec<String> = desc.roles.iter().map(|r| r.role.clone()).collect();
        let m = self.metrics.entry(id.clone()).or_insert_with(|| Metric {
            desc: desc.clone(),
            producer: producer.to_owned(),
            values: Values::Empty,
            last_ns: None,
            roles: Vec::new(),
            conflicts: Vec::new(),
        });
        if m.desc.kind != desc.kind {
            m.values = Values::Empty;
        }
        m.desc = desc;
        m.producer = producer.to_owned();
        m.roles.clear();
        m.conflicts.clear();
        for role in claims {
            match self.roles.get(&role) {
                Some(holder) if holder != &id && self.metrics.get(holder).is_some_and(|h| self.producer_connected(&h.producer)) => {
                    self.metrics.get_mut(&id).unwrap().conflicts.push(role);
                }
                _ => {
                    self.roles.insert(role.clone(), id.clone());
                    self.metrics.get_mut(&id).unwrap().roles.push(role);
                }
            }
        }
        self.rev += 1;
    }

    fn producer_connected(&self, p: &str) -> bool {
        self.producers.get(p).is_some_and(|p| p.info.connected)
    }

    pub fn has_metric(&self, id: &str) -> bool {
        self.metrics.contains_key(id)
    }

    pub fn descriptor(&self, id: &str) -> Option<&MetricDescriptor> {
        self.metrics.get(id).map(|m| &m.desc).or_else(|| self.canonical.iter().find(|c| c.desc.id == id).map(|c| &c.desc))
    }

    /// The metric filling `role`, if its producer is connected.
    pub fn role(&self, role: &str) -> Option<&str> {
        let id = self.roles.get(role)?;
        let m = self.metrics.get(id)?;
        self.producer_connected(&m.producer).then_some(id.as_str())
    }

    fn in_window(&mut self, t: i64, now: i64) -> bool {
        let ok = t >= self.started_ns - 1_000_000_000 && t <= now + 1_000_000_000;
        if !ok {
            self.out_of_window += 1;
        }
        ok
    }

    // ---- values in

    pub fn push_num(&mut self, id: &str, t: i64, v: f64) {
        let Some(m) = self.metrics.get_mut(id) else { return };
        if !v.is_finite() {
            return;
        }
        if !matches!(m.values, Values::Num(_)) {
            m.values = Values::Num(NumSeries::default());
        }
        if let Values::Num(s) = &mut m.values {
            s.push(t, v);
        }
        m.last_ns = Some(t);
    }

    pub fn push_counter(&mut self, id: &str, t: i64, total: u64) {
        let Some(m) = self.metrics.get_mut(id) else { return };
        if !matches!(m.values, Values::Counter(_)) {
            m.values = Values::Counter(CounterSeries::default());
        }
        if let Values::Counter(c) = &mut m.values {
            c.push(t, total);
        }
        m.last_ns = Some(t);
    }

    pub fn push_text(&mut self, id: &str, t: i64, s: String) {
        let Some(m) = self.metrics.get_mut(id) else { return };
        m.values = Values::Text(s);
        m.last_ns = Some(t);
    }

    pub fn push_state(&mut self, id: &str, t: i64, idx: usize) {
        let Some(m) = self.metrics.get_mut(id) else { return };
        if idx < m.desc.values.len() {
            m.values = Values::State(idx);
            m.last_ns = Some(t);
        }
    }

    /// A state by its value string.
    pub fn push_state_value(&mut self, id: &str, t: i64, value: &str) {
        let idx = self.metrics.get(id).and_then(|m| m.desc.values.iter().position(|v| v.value == value));
        if let Some(i) = idx {
            self.push_state(id, t, i);
        }
    }

    pub fn push_mark(&mut self, id: &str, t: i64, now: i64, dur: Option<i64>, label: Option<String>) -> bool {
        if !self.in_window(t, now) {
            return false;
        }
        let Some(m) = self.metrics.get_mut(id) else { return false };
        if !matches!(m.values, Values::Events(_)) {
            m.values = Values::Events(Vec::new());
        }
        if let Values::Events(v) = &mut m.values {
            if v.len() >= MAX_EVENTS {
                v.remove(0);
            }
            v.push((t, dur, label));
        }
        m.last_ns = Some(t);
        true
    }

    /// One layer present (entry time in CLOCK_MONOTONIC_RAW ns). False if its time is
    /// outside the session's window.
    pub fn present(&mut self, swapchain: u32, t: i64, now: i64) -> bool {
        if !self.in_window(t, now) {
            return false;
        }
        self.frames.present(swapchain, t);
        true
    }

    pub fn set_layer_status(&mut self, s: LayerStatus) {
        if s.swapchain != 0 {
            self.frames.set_primary(s.swapchain);
        }
        self.layer_status = Some(s);
    }

    /// Accounts presenter time for the summary.
    pub fn note_presenter(&mut self, p: Option<Presenter>, now: i64) {
        if let Some((prev, since)) = self.presenter_mark.take() {
            self.presenter_ns[prev as usize] += now - since;
        }
        self.presenter_mark = p.map(|p| (p, now));
    }

    // ---- values out

    fn frames_live(&self, now: i64) -> bool {
        self.frames.last_ns > 0 && now - self.frames.last_ns < FRAMES_STALE_NS
    }

    fn frame_stats(&self, now: i64, window_ns: i64) -> Option<WindowStats> {
        if window_ns == WINDOW_SESSION {
            let h = &self.frames.hist;
            let total = h.total();
            if total == 0 {
                return None;
            }
            let mean = self.frames.covered_ns as f64 / 1e6 / total as f64;
            return Some(WindowStats {
                count: total as usize,
                mean,
                min: h.percentile(0.0).unwrap_or(mean),
                max: h.percentile(100.0).unwrap_or(mean),
                p50: h.percentile(50.0)?,
                p95: h.percentile(95.0)?,
                p99: h.percentile(99.0)?,
                p999: h.percentile(99.9)?,
                top1_mean: h.top_mean(1.0)?,
                sum: self.frames.covered_ns as f64 / 1e6,
            });
        }
        WindowStats::of(&mut self.frames.window(now, window_ns))
    }

    /// A metric's value for `stat` over `window_ns` (0: the session). `None` without data.
    pub fn value(&self, id: &str, stat: Stat, window_ns: i64, now: i64) -> Option<Raw> {
        if let Some(c) = self.canonical.iter().find(|c| c.desc.id == id) {
            return self.canonical_value(c, stat, window_ns, now);
        }
        let m = self.metrics.get(id)?;
        match &m.values {
            Values::Empty => None,
            Values::Num(s) => num_stat(s, stat, window_ns, now).map(Raw::Num),
            Values::Counter(c) => match stat {
                Stat::Total | Stat::Last => c.total().map(|v| Raw::Num(v as f64)),
                _ if window_ns == WINDOW_SESSION => c.session_rate().map(Raw::Num),
                _ => c.rate(window_ns.max(WINDOW_1S)).map(Raw::Num),
            },
            Values::Text(s) => Some(Raw::Text(s.clone())),
            Values::State(i) => Some(Raw::State(*i)),
            Values::Events(v) => v.last().map(|(_, _, l)| Raw::Text(l.clone().unwrap_or_else(|| "mark".into()))),
        }
    }

    fn canonical_value(&self, c: &Canonical, stat: Stat, window_ns: i64, now: i64) -> Option<Raw> {
        let window_ns = if window_ns == WINDOW_SESSION { WINDOW_SESSION } else { window_ns.max(WINDOW_1S / 4) };
        match c.source {
            Source::Frames => {
                if window_ns != WINDOW_SESSION && !self.frames_live(now) {
                    return None;
                }
                let s = self.frame_stats(now, window_ns)?;
                let fps = |ms: f64| if ms > 0.0 { 1000.0 / ms } else { f64::NAN };
                let v = match (c.frame_stat, stat) {
                    (FrameStat::Fps, Stat::Min) => fps(s.max),
                    (FrameStat::Fps, Stat::Max) => fps(s.min),
                    (FrameStat::Fps, Stat::P1low) | (FrameStat::P1Low, _) => fps(s.p99),
                    (FrameStat::Fps, Stat::P01low) | (FrameStat::P01Low, _) => fps(s.p999),
                    (FrameStat::Fps, _) => fps(s.mean),
                    (FrameStat::P1LowAvg, _) => fps(s.top1_mean),
                    (FrameStat::FrameTime, Stat::Min) => s.min,
                    (FrameStat::FrameTime, Stat::Max) => s.max,
                    (FrameStat::FrameTime, Stat::P50) => s.p50,
                    (FrameStat::FrameTime, Stat::P95) => s.p95,
                    (FrameStat::FrameTime, Stat::P99) | (FrameStat::FrameTimeP99, _) => s.p99,
                    (FrameStat::FrameTime, _) => s.mean,
                    (FrameStat::None, _) => return None,
                };
                v.is_finite().then_some(Raw::Num(v))
            }
            Source::CounterRate(role) => match self.role(role) {
                Some(src) => self.value(src, Stat::Rate, window_ns, now),
                // No base provider: base = presented (§3.4).
                None if c.desc.id == "fps.base" => self.value("fps", stat, window_ns, now),
                None => None,
            },
            Source::FgRatio => {
                self.role(catalog::ROLE_BASE_COUNT)?;
                let p = self.value("fps", Stat::Mean, window_ns, now)?.num()?;
                let b = self.value("fps.base", Stat::Mean, window_ns, now)?.num()?;
                (b > 0.0).then(|| Raw::Num(p / b))
            }
            Source::FgActive => Some(Raw::State(self.fg_active(now) as usize)),
            Source::Alias(role) => {
                let src = self.role(role)?;
                self.value(src, stat, window_ns, now)
            }
            Source::OutputResolution => {
                if let Some(src) = self.role(catalog::ROLE_OUTPUT_RES) {
                    return self.value(src, stat, window_ns, now);
                }
                let s = self.layer_status.as_ref().filter(|s| s.extent_w > 0)?;
                Some(Raw::Text(format!("{}×{}", s.extent_w, s.extent_h)))
            }
            Source::SessionTime => Some(Raw::Num((now - self.started_ns).max(0) as f64 / 1e9)),
            Source::Clock => Some(Raw::Text(chrono::Local::now().format("%H:%M").to_string())),
        }
    }

    /// Frame generation is on: a base provider exists and its FG state (if any) isn't off.
    pub fn fg_active(&self, now: i64) -> bool {
        if self.role(catalog::ROLE_BASE_COUNT).is_none() {
            return false;
        }
        let state = self.role(catalog::ROLE_FG_STATE).and_then(|id| self.metrics.get(id)).and_then(|m| match &m.values {
            Values::State(i) => m.desc.values.get(*i),
            _ => None,
        });
        match state {
            Some(v) => v.value != "off" && !matches!(v.level, Some(quiver_api::perf::Level::Dim | quiver_api::perf::Level::Bad)),
            // No state (yet): on when presents outrun real frames clearly.
            None => {
                let p = self.value("fps", Stat::Mean, WINDOW_1S, now).and_then(|r| r.num());
                let b = self.value("fps.base", Stat::Mean, WINDOW_1S, now).and_then(|r| r.num());
                matches!((p, b), (Some(p), Some(b)) if b > 0.0 && p > b * 1.3)
            }
        }
    }

    pub fn state(&self, id: &str, now: i64) -> MetricState {
        if let Some(c) = self.canonical.iter().find(|c| c.desc.id == id) {
            let live = match c.source {
                Source::Frames => self.frames_live(now),
                Source::SessionTime | Source::Clock | Source::FgActive => true,
                _ => self.value(id, Stat::Last, WINDOW_1S, now).is_some(),
            };
            return if live { MetricState::Live } else if self.frames.frames > 0 && c.source == Source::Frames { MetricState::Stale } else { MetricState::Declared };
        }
        let Some(m) = self.metrics.get(id) else { return MetricState::Declared };
        if !self.producer_connected(&m.producer) {
            return MetricState::Gone;
        }
        match (m.last_ns, period_ns(&m.desc)) {
            (None, _) => MetricState::Declared,
            (Some(t), Some(p)) if now - t > (2 * p).max(2_000_000_000) => MetricState::Stale,
            _ => MetricState::Live,
        }
    }

    /// Formatted, with staleness: `—` for no data or stale.
    pub fn formatted(&self, id: &str, stat: Stat, window_ns: i64, now: i64) -> Option<PerfValue> {
        let desc = self.descriptor(id)?;
        let st = self.state(id, now);
        if st == MetricState::Stale || st == MetricState::Gone {
            return Some(format::stale());
        }
        let desc = desc.clone();
        Some(format::format(&desc, self.value(id, stat, window_ns, now).as_ref()))
    }

    /// Recent values of a numeric metric (sparklines), oldest first.
    pub fn tail(&self, id: &str, n: usize) -> Vec<f64> {
        match self.metrics.get(id).map(|m| &m.values) {
            Some(Values::Num(s)) => s.tail(n),
            _ => Vec::new(),
        }
    }

    /// Every metric id: canonical first, then producers' in id order.
    pub fn ids(&self) -> Vec<String> {
        self.canonical.iter().map(|c| c.desc.id.clone()).chain(self.metrics.keys().cloned()).collect()
    }

    /// Ids matching a selector: an exact id, a role (`frame.base.count`), or a glob with
    /// a trailing `*` (`gpu.*`).
    pub fn select(&self, sel: &str) -> Vec<String> {
        if let Some(prefix) = sel.strip_suffix('*') {
            return self.ids().into_iter().filter(|id| id.starts_with(prefix)).collect();
        }
        if self.descriptor(sel).is_some() {
            return vec![sel.to_owned()];
        }
        self.role(sel).map(|id| vec![id.to_owned()]).unwrap_or_default()
    }

    pub fn catalog(&self, now: i64) -> Vec<MetricInfo> {
        let mut out = Vec::new();
        for c in &self.canonical {
            let source = match c.source {
                Source::Frames => self.producers.contains_key(catalog::PRODUCER_LAYER).then(|| "present.frame".to_owned()),
                Source::CounterRate(r) | Source::Alias(r) => self.role(r).map(str::to_owned).or_else(|| (c.desc.id == "fps.base").then(|| "fps".into())),
                Source::OutputResolution => self.role(catalog::ROLE_OUTPUT_RES).map(str::to_owned).or_else(|| self.layer_status.map(|_| "present.frame".into())),
                _ => None,
            };
            let stat = format::default_stat(&c.desc);
            out.push(MetricInfo {
                descriptor: c.desc.clone(),
                producer: catalog::PRODUCER_QUIVERD.into(),
                producer_label: "Quiver".into(),
                canonical: true,
                source,
                roles: Vec::new(),
                conflicts: Vec::new(),
                state: self.state(&c.desc.id, now),
                last: self.formatted(&c.desc.id, stat, WINDOW_1S, now),
            });
        }
        for (id, m) in &self.metrics {
            let stat = format::default_stat(&m.desc);
            out.push(MetricInfo {
                descriptor: m.desc.clone(),
                producer: m.producer.clone(),
                producer_label: self.producers.get(&m.producer).map(|p| p.info.label.clone()).unwrap_or_default(),
                canonical: false,
                source: None,
                roles: m.roles.clone(),
                conflicts: m.conflicts.clone(),
                state: self.state(id, now),
                last: self.formatted(id, stat, WINDOW_1S, now),
            });
        }
        out
    }

    pub fn producer_infos(&self) -> Vec<ProducerInfo> {
        self.producers
            .values()
            .map(|p| {
                let mut i = p.info.clone();
                i.metrics = match i.id.as_str() {
                    catalog::PRODUCER_QUIVERD => self.canonical.len(),
                    id => self.metrics.values().filter(|m| m.producer == id).count(),
                };
                i
            })
            .collect()
    }

    /// The session's summary (decision 13).
    pub fn summary(&self, now: i64) -> PerfSummary {
        let f = &self.frames;
        let mut s = PerfSummary { frames: f.frames, frame_seconds: f.covered_ns as f64 / 1e9, ..Default::default() };
        if let Some(st) = self.frame_stats(now, WINDOW_SESSION) {
            s.fps = Some(FpsStats { mean: 1000.0 / st.mean, p1low: Some(1000.0 / st.p99), p01low: Some(1000.0 / st.p999) });
            s.frametime_ms = Some(Percentiles { p50: st.p50, p95: st.p95, p99: st.p99 });
            s.hitches = f.hist.count_above(4.0 * st.p50);
        }
        if let Some(src) = self.role(catalog::ROLE_BASE_COUNT).or_else(|| {
            // A mod that disconnected still counts: find the counter by its claim.
            self.metrics.iter().find(|(_, m)| m.roles.iter().any(|r| r == catalog::ROLE_BASE_COUNT)).map(|(id, _)| id.as_str())
        }) && let Some(Values::Counter(c)) = self.metrics.get(src).map(|m| &m.values)
            && let Some(rate) = c.session_rate()
        {
            s.fps_base = Some(FpsStats { mean: rate, p1low: None, p01low: None });
            if let Some(p) = &s.fps
                && rate > 0.0
            {
                s.fg_ratio = Some(p.mean / rate);
            }
        }
        let mm = |id: &str| -> Option<MeanMax> {
            let m = self.metrics.get(id)?;
            match &m.values {
                Values::Num(n) if n.count > 0 => Some(MeanMax { mean: n.mean()?, max: n.max }),
                _ => None,
            }
        };
        let latency = self.metrics.iter().find(|(_, m)| m.roles.iter().any(|r| r == catalog::ROLE_LATENCY_RENDER)).map(|(id, _)| id.clone());
        s.latency_ms = latency.and_then(|id| mm(&id));
        s.gpu_load = mm("gpu.load");
        s.gpu_temp = mm("gpu.temp");
        s.gpu_power = mm("gpu.power");
        s.cpu_load = mm("cpu.load");
        s.cpu_temp = mm("cpu.temp");
        s.vram_peak = mm("gpu.vram.used").map(|m| m.max as u64);
        s.gpu = match self.metrics.get("gpu.name").map(|m| &m.values) {
            Some(Values::Text(n)) => Some(n.clone()),
            _ => None,
        };
        s.sensor_seconds = self.metrics.get("cpu.load").and_then(|m| match &m.values {
            Values::Num(n) => Some(n.count as f64),
            _ => None,
        }).unwrap_or(0.0);
        let mut shares = self.presenter_ns;
        if let Some((p, since)) = self.presenter_mark {
            shares[p as usize] += now - since;
        }
        s.presenter = match shares {
            [0, 0] => None,
            [g, d] if g >= d => Some(Presenter::Game),
            _ => Some(Presenter::Desktop),
        };
        s
    }
}

fn num_stat(s: &NumSeries, stat: Stat, window_ns: i64, now: i64) -> Option<f64> {
    if stat == Stat::Last {
        return s.last().map(|(_, v)| v);
    }
    if window_ns == WINDOW_SESSION {
        return match stat {
            Stat::Min => (s.count > 0).then_some(s.min),
            Stat::Max => (s.count > 0).then_some(s.max),
            Stat::P50 | Stat::P95 | Stat::P99 => {
                let p = match stat {
                    Stat::P50 => 50.0,
                    Stat::P95 => 95.0,
                    _ => 99.0,
                };
                let mut v = s.session();
                v.sort_by(|a, b| a.total_cmp(b));
                (!v.is_empty()).then(|| super::stats::percentile_sorted(&v, p))
            }
            _ => s.mean(),
        };
    }
    // A slow gauge (1 Hz) has nothing in a 1 s window between samples: use the latest.
    let mut w = s.window(now, window_ns);
    if w.is_empty() {
        return s.last().filter(|(t, _)| now - t < 3 * WINDOW_1S).map(|(_, v)| v);
    }
    let st = WindowStats::of(&mut w)?;
    Some(match stat {
        Stat::Min => st.min,
        Stat::Max => st.max,
        Stat::P50 => st.p50,
        Stat::P95 => st.p95,
        Stat::P99 => st.p99,
        _ => st.mean,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_perf::descriptor::{MetricKind, RoleClaim, StateValue};

    const MS: i64 = 1_000_000;

    fn store() -> Store {
        Store::new(0, Utc::now())
    }

    fn counter(id: &str, role: &str) -> MetricDescriptor {
        let mut d = MetricDescriptor::new(id, MetricKind::Counter);
        d.roles = vec![RoleClaim { role: role.into(), flags: vec![], on: None }];
        d
    }

    fn fg_state(id: &str) -> MetricDescriptor {
        let mut d = MetricDescriptor::new(id, MetricKind::State);
        d.values = vec![
            StateValue { value: "off".into(), label: None, level: Some(quiver_api::perf::Level::Dim) },
            StateValue { value: "on".into(), label: None, level: None },
        ];
        d.roles = vec![RoleClaim { role: "fg.state".into(), flags: vec![], on: None }];
        d
    }

    /// 160 presents a second for `secs`, and a mod counting 80 real frames a second.
    fn feed(s: &mut Store, secs: i64, with_mod: bool) -> i64 {
        s.producer("layer", "Sight layer", true);
        let mut t = 0;
        for i in 0..secs * 160 {
            t = 1_000 * MS + i * 6_250_000;
            s.present(7, t, t);
            if with_mod && i % 16 == 0 {
                s.push_counter("aco-dlss.frames.real", t, (i / 2) as u64);
            }
        }
        t
    }

    #[test]
    fn presented_fps_and_lows() {
        let mut s = store();
        let now = feed(&mut s, 3, false);
        let v = |id, stat| s.value(id, stat, WINDOW_1S, now).and_then(|r| r.num()).unwrap();
        assert!((v("fps", Stat::Mean) - 160.0).abs() < 0.01);
        assert!((v("frametime", Stat::Mean) - 6.25).abs() < 1e-9);
        assert!((v("fps.p1low", Stat::Last) - 160.0).abs() < 0.01);
        // Without a base provider, base = presented.
        assert!((v("fps.base", Stat::Mean) - 160.0).abs() < 0.01);
        assert!(s.value("fg.ratio", Stat::Mean, WINDOW_1S, now).is_none());
        assert_eq!(s.formatted("fps", Stat::Mean, WINDOW_1S, now).unwrap().text, "160");
        assert_eq!(s.state("fps", now), MetricState::Live);
        // Two seconds later, nothing presented: stale, shown as —.
        assert_eq!(s.state("fps", now + 2_000 * MS), MetricState::Stale);
        assert_eq!(s.formatted("fps", Stat::Mean, WINDOW_1S, now + 2_000 * MS).unwrap().text, "—");
        // The session's numbers survive.
        let sum = s.summary(now);
        assert!((sum.fps.unwrap().mean - 160.0).abs() < 0.5);
        assert_eq!(sum.frames, 3 * 160 - 1);
        assert_eq!(sum.hitches, 0);
    }

    #[test]
    fn base_fps_from_a_mod_counter_and_fg_state() {
        let mut s = store();
        s.producer("mod:aco-dlss", "aco-dlss", true);
        s.declare("mod:aco-dlss", counter("aco-dlss.frames.real", catalog::ROLE_BASE_COUNT));
        s.declare("mod:aco-dlss", fg_state("aco-dlss.fg.state"));
        let now = feed(&mut s, 3, true);
        let v = |s: &Store, id| s.value(id, Stat::Mean, WINDOW_1S, now).and_then(|r| r.num()).unwrap();
        assert!((v(&s, "fps.base") - 80.0).abs() < 1.0, "{}", v(&s, "fps.base"));
        assert!((v(&s, "fg.ratio") - 2.0).abs() < 0.05);
        // No FG state yet: inferred from the ratio.
        assert!(s.fg_active(now));
        s.push_state_value("aco-dlss.fg.state", now, "off");
        assert!(!s.fg_active(now));
        assert_eq!(s.formatted("fg.active", Stat::Last, WINDOW_1S, now).unwrap().text, "off");
        s.push_state_value("aco-dlss.fg.state", now, "on");
        assert!(s.fg_active(now));
        let sum = s.summary(now);
        assert!((sum.fps_base.unwrap().mean - 80.0).abs() < 1.0);
        assert!((sum.fg_ratio.unwrap() - 2.0).abs() < 0.05);
        // The mod goes away: its roles are released, base falls back to presented.
        s.disconnect("mod:aco-dlss");
        assert!(!s.fg_active(now));
        assert!((v(&s, "fps.base") - 160.0).abs() < 0.1);
        assert_eq!(s.state("aco-dlss.fg.state", now), MetricState::Gone);
        assert!(s.summary(now).fps_base.is_some(), "a finished mod still counts in the summary");
    }

    #[test]
    fn roles_first_claim_wins() {
        let mut s = store();
        s.producer("mod:a", "a", true);
        s.producer("mod:b", "b", true);
        s.declare("mod:a", counter("a.real", catalog::ROLE_BASE_COUNT));
        s.declare("mod:b", counter("b.real", catalog::ROLE_BASE_COUNT));
        assert_eq!(s.role(catalog::ROLE_BASE_COUNT), Some("a.real"));
        let cat = s.catalog(0);
        let b = cat.iter().find(|m| m.descriptor.id == "b.real").unwrap();
        assert_eq!(b.conflicts, vec![catalog::ROLE_BASE_COUNT.to_string()]);
        let base = cat.iter().find(|m| m.descriptor.id == "fps.base").unwrap();
        assert_eq!(base.source.as_deref(), Some("a.real"));
    }

    #[test]
    fn timestamps_outside_the_session_are_dropped() {
        let mut s = Store::new(10_000 * MS, Utc::now());
        assert!(!s.present(1, 1_000 * MS, 20_000 * MS));
        assert!(!s.present(1, 50_000 * MS, 20_000 * MS));
        assert!(s.present(1, 19_000 * MS, 20_000 * MS));
        assert_eq!(s.out_of_window, 2);
    }

    #[test]
    fn gauges_select_and_catalog() {
        let mut s = store();
        s.producer("system", "System", true);
        for d in catalog::system() {
            s.declare("system", d);
        }
        s.push_num("gpu.load", 1_000 * MS, 97.0);
        s.push_num("gpu.load", 2_000 * MS, 95.0);
        s.push_text("gpu.name", 1_000 * MS, "RTX".into());
        // A 1 Hz gauge: the 1 s window holds one sample; still a value in between.
        assert_eq!(s.value("gpu.load", Stat::Mean, WINDOW_1S, 2_500 * MS), Some(Raw::Num(95.0)));
        assert_eq!(s.value("gpu.load", Stat::Mean, WINDOW_10S, 2_500 * MS), Some(Raw::Num(96.0)));
        assert_eq!(s.value("gpu.load", Stat::Max, WINDOW_SESSION, 2_500 * MS), Some(Raw::Num(97.0)));
        assert_eq!(s.state("gpu.load", 3_000 * MS), MetricState::Live);
        assert_eq!(s.state("gpu.load", 9_000 * MS), MetricState::Stale);
        assert_eq!(s.state("gpu.temp", 9_000 * MS), MetricState::Declared);
        let mut g = s.select("gpu.*");
        g.sort();
        assert!(g.contains(&"gpu.load".to_string()) && g.contains(&"gpu.name".to_string()));
        assert_eq!(s.select("fps"), vec!["fps"]);
        assert!(s.select("nope").is_empty());
        let sum = s.summary(3_000 * MS);
        assert_eq!(sum.gpu_load.unwrap().max, 97.0);
        assert_eq!(sum.gpu.as_deref(), Some("RTX"));
        assert!(s.catalog(0).iter().any(|m| m.canonical && m.descriptor.id == "fps"));
    }
}
