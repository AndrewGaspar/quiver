//! A run's report: `report.json` (everything recorded, on the run's own clock: ms from its
//! start) and `report.html` (a self-contained page of time series with the scenario's events
//! marked; inline SVG and a few lines of inline script for the hover readout, nothing
//! external).

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::ns::Dir;
use crate::probe::Dist;
use crate::profile::Profile;
use crate::run::{Applied, FloodRecord, LinkSample, Recording, RoamRecord, RunSpec, WorkloadEnd};
use crate::scenario::{Event, EventKind, Marker};
use crate::stats::StatsLine;

pub const REPORT_VERSION: u32 = 1;

/// A workload's stats line, placed on the run's clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timed {
    pub t_ms: f64,
    #[serde(flatten)]
    pub line: StatsLine,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioInfo {
    pub name: String,
    pub summary: String,
    pub base: Profile,
    pub duration_s: f64,
    pub events: Vec<Event>,
}

/// How long the stream took to get back to its old latency after an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recovery {
    pub label: String,
    pub t_ms: u64,
    pub step_down: bool,
    /// The latency before the event (median of the 5 s before), and the threshold that counts
    /// as recovered: max(1.5 × baseline, baseline + 10 ms).
    pub baseline_ms: f64,
    pub threshold_ms: f64,
    /// Seconds from the event (or the end of its span) to three samples in a row under the
    /// threshold; 0 if it never went over; None if it never came back.
    pub recovered_after_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Capture → present, from the client's stats: the frame-weighted median of its period
    /// medians, and the frame-weighted p99 of its period p99s (`latency_p99_us`, or the
    /// medians when a workload doesn't report it).
    pub latency_p50_ms: Option<f64>,
    pub latency_p99_ms: Option<f64>,
    /// Which series the latency and recovery figures come from: "capture_to_present" or
    /// "path_rtt" (no client stats).
    pub latency_source: String,
    pub gaps_per_min: Option<f64>,
    pub frames_lost: u64,
    /// The stream's bitrate (host stats, else the client's `recv_kbps`), and the mean rate
    /// to the client the link allowed (time-weighted).
    pub mean_bitrate_mbit: Option<f64>,
    pub mean_capacity_mbit: f64,
    pub utilisation_pct: Option<f64>,
    pub stream_loss_pct: Option<f64>,
    pub path_rtt_p50_ms: Option<f64>,
    pub path_rtt_p99_ms: Option<f64>,
    pub path_loss_pct: Option<f64>,
    /// Packets the bottleneck queue to the client dropped (congestion, not random loss).
    pub queue_drops_to_client: u64,
    pub recoveries: Vec<Recovery>,
    /// The slowest recovery after a step-down in rate (None without one; f64::INFINITY,
    /// written as null, never happens: an unrecovered step-down sets `recovered: false`).
    pub recovery_s: Option<f64>,
    pub recovered: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub version: u32,
    pub label: String,
    pub scenario: ScenarioInfo,
    /// CLOCK_MONOTONIC at the run's start, in µs: t_ms = 0.
    pub start_us: u64,
    pub duration_s: f64,
    pub server: Option<WorkloadEnd>,
    pub client: Option<WorkloadEnd>,
    pub markers: Vec<Marker>,
    pub applied: Vec<Applied>,
    pub samples: Vec<LinkSample>,
    pub floods: Vec<FloodRecord>,
    pub roams: Vec<RoamRecord>,
    /// The netlab's own probe: (t_ms, round trip ms) per answered packet.
    pub path_rtt: Vec<(f64, f64)>,
    pub path_sent: u64,
    pub client_stats: Vec<Timed>,
    pub host_stats: Vec<Timed>,
    pub bad_stats_lines: usize,
    pub summary: Summary,
}

impl Report {
    pub fn build(
        spec: &RunSpec,
        markers: &[Marker],
        rec: Recording,
        server: Vec<StatsLine>,
        client: Vec<StatsLine>,
        bad: usize,
    ) -> Report {
        let place = |lines: Vec<StatsLine>| -> Vec<Timed> {
            lines
                .into_iter()
                .map(|l| Timed { t_ms: (l.t_us as f64 - rec.start_us as f64) / 1000.0, line: l })
                .filter(|t| t.t_ms >= -1000.0)
                .collect()
        };
        let path_rtt: Vec<(f64, f64)> =
            rec.path.samples.iter().map(|&(t, r)| ((t as f64 - rec.start_us as f64) / 1000.0, r)).collect();
        let mut r = Report {
            version: REPORT_VERSION,
            label: spec.label.clone(),
            scenario: ScenarioInfo {
                name: spec.scenario.name.clone(),
                summary: spec.scenario.summary.clone(),
                base: spec.scenario.base.clone(),
                duration_s: spec.scenario.duration_s,
                events: spec.scenario.events.clone(),
            },
            start_us: rec.start_us,
            duration_s: rec.end_ms / 1000.0,
            server: rec.server,
            client: rec.client,
            markers: markers.to_vec(),
            applied: rec.applied,
            samples: rec.samples,
            floods: rec.floods,
            roams: rec.roams,
            path_sent: rec.path.sent,
            path_rtt,
            client_stats: place(client),
            host_stats: place(server),
            bad_stats_lines: bad,
            summary: Summary::default(),
        };
        r.summary = summarise(&r);
        r
    }

    /// The rate to the client in force at `t_ms`.
    pub fn capacity_at(&self, t_ms: f64) -> f64 {
        let mut rate = self.scenario.base.to_client.rate_mbit;
        for a in self.applied.iter().filter(|a| a.dir == Dir::ToClient) {
            if a.t_ms > t_ms {
                break;
            }
            rate = a.rate_mbit;
        }
        rate
    }

    /// The stream's bitrate over time, in Mbit/s.
    pub fn bitrate_series(&self) -> Vec<(f64, f64)> {
        let host: Vec<(f64, f64)> = self
            .host_stats
            .iter()
            .filter_map(|t| t.line.host.as_ref().map(|h| (t.t_ms, h.bitrate_kbps as f64 / 1000.0)))
            .collect();
        if !host.is_empty() {
            return host;
        }
        self.client_stats.iter().filter_map(|t| t.line.extra("recv_kbps").map(|k| (t.t_ms, k / 1000.0))).collect()
    }

    /// Capture → present medians over time, in ms (periods with frames only).
    pub fn latency_series(&self) -> Vec<(f64, f64)> {
        self.client_stats
            .iter()
            .filter_map(|t| {
                let c = t.line.client.as_ref()?;
                (c.frames_received > 0 && c.capture_to_present_us > 0).then(|| (t.t_ms, c.capture_to_present_us as f64 / 1000.0))
            })
            .collect()
    }

    /// The link's carried rate in one direction, from the router's counters, in Mbit/s.
    pub fn carried_series(&self, dir: Dir) -> Vec<(f64, f64)> {
        let s: Vec<&LinkSample> = self.samples.iter().filter(|s| s.dir == dir).collect();
        s.windows(2)
            .filter(|w| w[1].t_ms > w[0].t_ms && w[1].sent_bytes >= w[0].sent_bytes)
            .map(|w| (w[1].t_ms, (w[1].sent_bytes - w[0].sent_bytes) as f64 * 8.0 / ((w[1].t_ms - w[0].t_ms) / 1000.0) / 1e6))
            .collect()
    }

    /// The queueing delay at the bottleneck to the client: its backlog at the rate then.
    pub fn queue_series(&self) -> Vec<(f64, f64)> {
        self.samples
            .iter()
            .filter(|s| s.dir == Dir::ToClient)
            .map(|s| {
                let rate = self.capacity_at(s.t_ms);
                (s.t_ms, if rate > 0.0 { s.queue_bytes as f64 * 8.0 / (rate * 1e6) * 1000.0 } else { 0.0 })
            })
            .collect()
    }

    pub fn write(&self, json: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        std::fs::write(json, serde_json::to_string_pretty(self)?)?;
        let html = json.with_extension("html");
        std::fs::write(&html, render_html(self))?;
        Ok(html)
    }
}

/// The value at quantile `q` of `values` weighted by `weights`.
fn weighted_quantile(pairs: &[(f64, f64)], q: f64) -> Option<f64> {
    let mut v: Vec<(f64, f64)> = pairs.iter().copied().filter(|p| p.1 > 0.0).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = v.iter().map(|p| p.1).sum();
    let mut acc = 0.0;
    for (x, w) in &v {
        acc += w;
        if acc >= q * total {
            return Some(*x);
        }
    }
    v.last().map(|p| p.0)
}

fn summarise(r: &Report) -> Summary {
    let mut s = Summary::default();
    let dur_min = (r.duration_s / 60.0).max(1e-9);
    let clients: Vec<_> = r.client_stats.iter().filter_map(|t| t.line.client.as_ref().map(|c| (t, c))).collect();
    if !clients.is_empty() {
        let p50: Vec<(f64, f64)> =
            clients.iter().map(|(_, c)| (c.capture_to_present_us as f64 / 1000.0, c.frames_received as f64)).collect();
        let p99: Vec<(f64, f64)> = clients
            .iter()
            .map(|(t, c)| {
                (t.line.extra("latency_p99_us").unwrap_or(c.capture_to_present_us as f64) / 1000.0, c.frames_received as f64)
            })
            .collect();
        s.latency_p50_ms = weighted_quantile(&p50, 0.5);
        s.latency_p99_ms = weighted_quantile(&p99, 0.99);
        s.frames_lost = clients.iter().map(|(_, c)| c.frames_lost as u64).sum();
        let gaps: Option<f64> = clients.iter().map(|(t, _)| t.line.extra("frame_gaps")).sum();
        s.gaps_per_min = Some(gaps.unwrap_or(s.frames_lost as f64) / dur_min);
        let (rx, lost): (u64, u64) =
            clients.iter().fold((0, 0), |a, (_, c)| (a.0 + c.packets_received as u64, a.1 + c.packets_lost as u64));
        if rx + lost > 0 {
            s.stream_loss_pct = Some(100.0 * lost as f64 / (rx + lost) as f64);
        }
    }
    let br = r.bitrate_series();
    if !br.is_empty() {
        s.mean_bitrate_mbit = Some(br.iter().map(|p| p.1).sum::<f64>() / br.len() as f64);
    }
    // Time-weighted capacity over the run.
    let end = r.duration_s * 1000.0;
    let mut cap = 0.0;
    let mut steps: Vec<(f64, f64)> = r.applied.iter().filter(|a| a.dir == Dir::ToClient).map(|a| (a.t_ms, a.rate_mbit)).collect();
    if steps.is_empty() {
        steps.push((0.0, r.scenario.base.to_client.rate_mbit));
    }
    for (i, &(t, rate)) in steps.iter().enumerate() {
        let next = steps.get(i + 1).map_or(end, |n| n.0).min(end);
        cap += rate * (next - t).max(0.0);
    }
    s.mean_capacity_mbit = if end > 0.0 { cap / end } else { 0.0 };
    if let Some(b) = s.mean_bitrate_mbit
        && s.mean_capacity_mbit > 0.0
    {
        s.utilisation_pct = Some(100.0 * b / s.mean_capacity_mbit);
    }
    let rtts: Vec<f64> = r.path_rtt.iter().map(|p| p.1).collect();
    if !rtts.is_empty() {
        let d = Dist::of(&rtts);
        s.path_rtt_p50_ms = Some(d.p50);
        s.path_rtt_p99_ms = Some(d.p99);
    }
    if r.path_sent > 0 {
        s.path_loss_pct = Some(100.0 * (1.0 - r.path_rtt.len() as f64 / r.path_sent as f64).max(0.0));
    }
    let to_client: Vec<&LinkSample> = r.samples.iter().filter(|x| x.dir == Dir::ToClient).collect();
    s.queue_drops_to_client = to_client.last().map_or(0, |l| l.queue_drops);

    // Recovery, on the stream's latency, else the path's.
    let mut lat = r.latency_series();
    s.latency_source = "capture_to_present".into();
    if lat.is_empty() {
        lat = smooth(&r.path_rtt, 250.0);
        s.latency_source = "path_rtt".into();
    }
    for m in &r.markers {
        let t = m.t_ms as f64;
        let before: Vec<f64> = lat.iter().filter(|p| p.0 >= t - 5000.0 && p.0 < t).map(|p| p.1).collect();
        if before.is_empty() {
            continue;
        }
        let base = Dist::of(&before).p50;
        let threshold = (base * 1.5).max(base + 10.0);
        let from = m.end_ms.map_or(t, |e| e as f64).max(t);
        let after: Vec<&(f64, f64)> = lat.iter().filter(|p| p.0 >= t).collect();
        let disturbed = after.iter().position(|p| p.1 > threshold && p.0 <= from + 5000.0);
        let rec = match disturbed {
            None => Some(0.0),
            Some(i) => {
                let mut found = None;
                for j in i..after.len() {
                    if after[j..].iter().take(3).count() == 3 && after[j..j + 3].iter().all(|p| p.1 <= threshold) {
                        found = Some(((after[j].0 - from) / 1000.0).max(0.0));
                        break;
                    }
                }
                found
            }
        };
        s.recoveries.push(Recovery {
            label: m.label.clone(),
            t_ms: m.t_ms,
            // A competing flow steps down what is left for the stream: count it as one.
            step_down: m.step_down || m.kind == EventKind::Flood,
            baseline_ms: base,
            threshold_ms: threshold,
            recovered_after_s: rec,
        });
    }
    let downs: Vec<&Recovery> = s.recoveries.iter().filter(|r| r.step_down).collect();
    if !downs.is_empty() {
        s.recovered = Some(downs.iter().all(|r| r.recovered_after_s.is_some()));
        s.recovery_s = downs.iter().filter_map(|r| r.recovered_after_s).reduce(f64::max);
    }
    s
}

/// The median of `pts` in windows of `window_ms`.
fn smooth(pts: &[(f64, f64)], window_ms: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < pts.len() {
        let start = pts[i].0;
        let mut j = i;
        while j < pts.len() && pts[j].0 < start + window_ms {
            j += 1;
        }
        let v: Vec<f64> = pts[i..j].iter().map(|p| p.1).collect();
        out.push((pts[j - 1].0, Dist::of(&v).p50));
        i = j;
    }
    out
}

// --- HTML ------------------------------------------------------------------------------------

pub const CSS: &str = r#"
:root { --bg:#0E0F13; --panel:#15171D; --line:#262A33; --text:#E6E8EE; --muted:#9AA0AE;
        --cyan:#3FC8E4; --red:#FF6A55; --amber:#E8B44A; --violet:#A98BFF; --grey:#8C93A3; }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--text);
       font:14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width:1040px; margin:0 auto; padding:24px 16px 64px; }
h1 { font-size:22px; margin:0 0 4px; } h2 { font-size:16px; margin:28px 0 8px; }
p.lede { color:var(--muted); margin:0 0 16px; }
.tiles { display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:8px; }
.tile { background:var(--panel); border:1px solid var(--line); border-radius:8px; padding:10px 12px; }
.tile b { display:block; font-size:20px; font-weight:600; font-variant-numeric:tabular-nums; }
.tile span { color:var(--muted); font-size:12px; }
.chart { background:var(--panel); border:1px solid var(--line); border-radius:8px; padding:10px 12px 4px;
         margin:10px 0; position:relative; }
.chart h3 { font-size:13px; font-weight:600; margin:0; }
.legend { display:flex; flex-wrap:wrap; gap:4px 14px; font-size:12px; color:var(--muted); margin:2px 0 4px; }
.legend i { display:inline-block; width:14px; height:3px; border-radius:2px; vertical-align:middle; margin-right:5px; }
svg { width:100%; height:auto; display:block; }
svg text { fill:var(--muted); font-size:11px; font-family:inherit; }
.tip { position:absolute; pointer-events:none; background:#0B0C10; border:1px solid var(--line); border-radius:6px;
       padding:6px 8px; font-size:12px; display:none; white-space:nowrap; z-index:2; }
table { border-collapse:collapse; width:100%; font-size:13px; font-variant-numeric:tabular-nums; }
th, td { text-align:left; padding:5px 8px; border-bottom:1px solid var(--line); vertical-align:top; }
th { color:var(--muted); font-weight:500; }
td.n, th.n { text-align:right; }
.bad { color:var(--red); } .ok { color:var(--cyan); }
details { margin-top:8px; } summary { cursor:pointer; color:var(--muted); }
.scroll { overflow-x:auto; }
code { font-size:12px; color:var(--muted); word-break:break-all; }
a { color:var(--cyan); }
"#;

/// The hover readout: a crosshair and the nearest value of each series.
const SCRIPT: &str = r#"
document.querySelectorAll('.chart').forEach(function (c) {
  var svg = c.querySelector('svg'), tip = c.querySelector('.tip'), cross = c.querySelector('.cross');
  var d = JSON.parse(c.dataset.series), g = JSON.parse(c.dataset.geom);
  svg.addEventListener('mousemove', function (e) {
    var r = svg.getBoundingClientRect(), sx = (e.clientX - r.left) * g.w / r.width;
    if (sx < g.l || sx > g.w - g.r) { tip.style.display = 'none'; cross.setAttribute('visibility', 'hidden'); return; }
    var x = (sx - g.l) / (g.w - g.l - g.r) * g.xmax, rows = ['<b>' + x.toFixed(1) + ' s</b>'];
    d.forEach(function (s) {
      var best = null;
      s.p.forEach(function (p) { if (best === null || Math.abs(p[0] - x) < Math.abs(best[0] - x)) best = p; });
      if (best && Math.abs(best[0] - x) < g.xmax / 40)
        rows.push('<i style="display:inline-block;width:10px;height:3px;background:' + s.c + ';margin-right:5px;vertical-align:middle"></i>'
                  + s.n + ': ' + best[1].toFixed(best[1] < 10 ? 2 : 1) + ' ' + g.u);
    });
    cross.setAttribute('x1', sx); cross.setAttribute('x2', sx); cross.setAttribute('visibility', 'visible');
    tip.innerHTML = rows.join('<br>'); tip.style.display = 'block';
    var px = e.clientX - c.getBoundingClientRect().left;
    tip.style.left = (px > c.clientWidth / 2 ? px - tip.offsetWidth - 12 : px + 12) + 'px'; tip.style.top = '34px';
  });
  svg.addEventListener('mouseleave', function () { tip.style.display = 'none'; cross.setAttribute('visibility', 'hidden'); });
});
"#;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Line,
    Step,
    Bars,
}

pub struct Series<'a> {
    pub name: &'a str,
    pub color: &'a str,
    pub dash: bool,
    pub kind: Kind,
    /// (seconds, value)
    pub pts: Vec<(f64, f64)>,
}

fn nice_ceil(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if v <= m * p {
            return m * p;
        }
    }
    10.0 * p
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// One chart: a single y axis, series as thin lines, steps or bars, events marked.
pub fn chart(title: &str, unit: &str, series: &[Series], x_max: f64, markers: &[Marker], labels: bool) -> String {
    let (w, h, l, r, t, b) = (960.0, 210.0, 52.0, 14.0, if labels { 26.0 } else { 10.0 }, 24.0);
    let pw = w - l - r;
    let ph = h - t - b;
    let ymax_raw = series.iter().flat_map(|s| s.pts.iter().map(|p| p.1)).fold(0.0, f64::max);
    // Four gridline steps of a round size.
    let ymax = nice_ceil(ymax_raw * 1.08 / 4.0) * 4.0;
    let x = |v: f64| l + v.clamp(0.0, x_max) / x_max * pw;
    let y = |v: f64| t + ph - (v / ymax).clamp(0.0, 1.0) * ph;
    let mut svg = String::new();
    let _ = write!(svg, r#"<svg viewBox="0 0 {w} {h}" role="img" aria-label="{}">"#, esc(title));
    // Grid and y ticks.
    for i in 0..=4 {
        let v = ymax * i as f64 / 4.0;
        let yy = y(v);
        let _ = write!(
            svg,
            r##"<line x1="{l}" x2="{}" y1="{yy:.1}" y2="{yy:.1}" stroke="#262A33" stroke-width="1"/><text x="{}" y="{:.1}" text-anchor="end">{}</text>"##,
            w - r,
            l - 6.0,
            yy + 4.0,
            fmt_tick(v)
        );
    }
    // x ticks.
    let step = nice_ceil(x_max / 8.0);
    let mut xv = 0.0;
    while xv <= x_max + 1e-9 {
        let _ = write!(svg, r#"<text x="{:.1}" y="{}" text-anchor="middle">{}s</text>"#, x(xv), h - 6.0, fmt_tick(xv));
        xv += step;
    }
    // Event spans and lines.
    for m in markers {
        let mx = x(m.t_ms as f64 / 1000.0);
        if let Some(e) = m.end_ms {
            let ex = x(e as f64 / 1000.0);
            let _ = write!(
                svg,
                r##"<rect x="{mx:.1}" y="{t}" width="{:.1}" height="{ph}" fill="#FF6A55" fill-opacity="0.10"/>"##,
                (ex - mx).max(1.5)
            );
        }
        let _ = write!(
            svg,
            r##"<line x1="{mx:.1}" x2="{mx:.1}" y1="{t}" y2="{}" stroke="#FF6A55" stroke-width="1" stroke-dasharray="3 3" stroke-opacity="0.8"/>"##,
            t + ph
        );
        if labels {
            let anchor = if mx > w * 0.8 { "end" } else { "start" };
            let _ = write!(svg, r#"<text x="{:.1}" y="{}" text-anchor="{anchor}">{}</text>"#, mx + 3.0, t - 8.0, esc(&m.label));
        }
    }
    // Series.
    for s in series {
        if s.pts.is_empty() {
            continue;
        }
        match s.kind {
            Kind::Bars => {
                let bw = (pw / (x_max * 2.0)).clamp(1.0, 8.0);
                for &(px, py) in &s.pts {
                    if py <= 0.0 {
                        continue;
                    }
                    let _ = write!(
                        svg,
                        r#"<rect x="{:.1}" y="{:.1}" width="{bw:.1}" height="{:.1}" rx="1" fill="{}"/>"#,
                        x(px) - bw / 2.0,
                        y(py),
                        (t + ph - y(py)).max(1.0),
                        s.color
                    );
                }
            }
            Kind::Line | Kind::Step => {
                let mut pts = String::new();
                let mut prev: Option<(f64, f64)> = None;
                for &(px, py) in &s.pts {
                    if s.kind == Kind::Step
                        && let Some((_, qy)) = prev
                    {
                        let _ = write!(pts, "{:.1},{:.1} ", x(px), y(qy));
                    }
                    let _ = write!(pts, "{:.1},{:.1} ", x(px), y(py));
                    prev = Some((px, py));
                }
                if s.kind == Kind::Step
                    && let Some((_, qy)) = prev
                {
                    let _ = write!(pts, "{:.1},{:.1} ", x(x_max), y(qy));
                }
                let dash = if s.dash { r#" stroke-dasharray="6 4""# } else { "" };
                let _ = write!(
                    svg,
                    r#"<polyline points="{pts}" fill="none" stroke="{}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"{dash}/>"#,
                    s.color
                );
            }
        }
    }
    let _ = write!(
        svg,
        r##"<line class="cross" x1="0" x2="0" y1="{t}" y2="{}" stroke="#9AA0AE" stroke-width="1" visibility="hidden"/></svg>"##,
        t + ph
    );
    let data: Vec<serde_json::Value> = series
        .iter()
        .map(|s| {
            serde_json::json!({
                "n": s.name, "c": s.color,
                "p": s.pts.iter().map(|p| [(p.0 * 100.0).round() / 100.0, (p.1 * 1000.0).round() / 1000.0]).collect::<Vec<_>>()
            })
        })
        .collect();
    let geom = serde_json::json!({ "w": w, "l": l, "r": r, "xmax": x_max, "u": unit });
    let mut legend = String::new();
    if series.len() > 1 {
        legend.push_str(r#"<div class="legend">"#);
        for s in series {
            let style = if s.dash {
                format!("background:repeating-linear-gradient(90deg,{c} 0 5px,transparent 5px 8px)", c = s.color)
            } else {
                format!("background:{}", s.color)
            };
            let _ = write!(legend, r#"<span><i style="{style}"></i>{}</span>"#, esc(s.name));
        }
        legend.push_str("</div>");
    }
    format!(
        r#"<div class="chart" data-series="{}" data-geom="{}"><h3>{} <span style="color:var(--muted);font-weight:400">({})</span></h3>{legend}{svg}<div class="tip"></div></div>"#,
        esc(&serde_json::to_string(&data).unwrap()),
        esc(&geom.to_string()),
        esc(title),
        esc(unit)
    )
}

fn fmt_tick(v: f64) -> String {
    if v == 0.0 {
        "0".into()
    } else if v.abs() >= 100.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else if v.abs() >= 1.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

fn opt(v: Option<f64>, digits: usize, unit: &str) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.digits$}{unit}"),
        _ => "—".into(),
    }
}

pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{CSS}</style></head><body><main>{body}</main><script>{SCRIPT}</script></body></html>",
        esc(title)
    )
}

fn shape_row(name: &str, s: &crate::shape::LinkShape) -> String {
    let loss = match s.loss.burst {
        Some(b) if b > 1.0 => format!("{:.2}% (bursts of {b:.0})", s.loss.pct),
        _ => format!("{:.2}%", s.loss.pct),
    };
    let wander = s.wander.map(|w| format!(" (wanders {:.0}–{:.0})", w.min_mbit, w.max_mbit)).unwrap_or_default();
    format!(
        "<tr><td>{name}</td><td class=n>{:.2}</td><td class=n>{:.2} {:?}</td><td class=n>{loss}</td><td class=n>{:.0}{wander}</td><td class=n>{:.0} ms {:?}</td><td class=n>{:.1}/{:.1}/{:.1}</td></tr>",
        s.delay_ms,
        s.jitter_ms,
        s.distribution,
        s.rate_mbit,
        s.full_queue_ms(),
        s.aqm,
        s.duplicate_pct,
        s.reorder_pct,
        s.corrupt_pct
    )
}

pub fn render_html(r: &Report) -> String {
    let xs = r.duration_s.max(1.0);
    let sec = |v: Vec<(f64, f64)>| -> Vec<(f64, f64)> { v.into_iter().map(|(t, y)| (t / 1000.0, y)).collect() };
    let cap: Vec<(f64, f64)> = r.applied.iter().filter(|a| a.dir == Dir::ToClient).map(|a| (a.t_ms / 1000.0, a.rate_mbit)).collect();
    let mut body = String::new();
    let s = &r.summary;
    let _ = write!(
        body,
        "<h1>{}</h1><p class=lede>{}</p>",
        esc(&r.label),
        esc(&r.scenario.summary)
    );
    let recovery = match (s.recovered, s.recovery_s) {
        (Some(false), _) => "did not recover".to_string(),
        (_, Some(v)) => format!("{v:.1} s"),
        _ => "—".into(),
    };
    let tiles = [
        (opt(s.latency_p50_ms, 1, " ms"), "capture → present, median"),
        (opt(s.latency_p99_ms, 1, " ms"), "capture → present, p99"),
        (opt(s.gaps_per_min, 1, ""), "frame gaps per minute"),
        (
            format!("{} / {:.0}", opt(s.mean_bitrate_mbit, 1, ""), s.mean_capacity_mbit),
            "mean bitrate / capacity (Mbit/s)",
        ),
        (opt(s.stream_loss_pct, 2, "%"), "stream packet loss"),
        (opt(s.path_rtt_p50_ms, 1, " ms"), "path RTT, median (netlab probe)"),
        (recovery, "recovery after a step-down"),
    ];
    body.push_str("<div class=tiles>");
    for (v, k) in tiles {
        let _ = write!(body, "<div class=tile><b>{}</b><span>{}</span></div>", esc(&v), k);
    }
    body.push_str("</div>");

    let carried = sec(r.carried_series(Dir::ToClient));
    let bitrate = sec(r.bitrate_series());
    let mut br = vec![
        Series { name: "capacity", color: "#8C93A3", dash: true, kind: Kind::Step, pts: cap },
        Series { name: "link carried", color: "#A98BFF", dash: false, kind: Kind::Line, pts: carried },
    ];
    if !bitrate.is_empty() {
        br.push(Series { name: "stream bitrate", color: "#3FC8E4", dash: false, kind: Kind::Line, pts: bitrate });
    }
    body.push_str("<h2>To the client</h2>");
    body.push_str(&chart("Bitrate", "Mbit/s", &br, xs, &r.markers, true));

    let mut lat = Vec::new();
    let l = sec(r.latency_series());
    if !l.is_empty() {
        lat.push(Series { name: "capture → present", color: "#3FC8E4", dash: false, kind: Kind::Line, pts: l });
    }
    let rtt: Vec<(f64, f64)> = r
        .client_stats
        .iter()
        .filter_map(|t| t.line.client.as_ref().filter(|c| c.rtt_us > 0).map(|c| (t.t_ms / 1000.0, c.rtt_us as f64 / 1000.0)))
        .collect();
    if !rtt.is_empty() {
        lat.push(Series { name: "stream RTT", color: "#E8B44A", dash: false, kind: Kind::Line, pts: rtt });
    }
    lat.push(Series { name: "path RTT (probe)", color: "#A98BFF", dash: false, kind: Kind::Line, pts: sec(smooth(&r.path_rtt, 250.0)) });
    body.push_str(&chart("Latency", "ms", &lat, xs, &r.markers, false));
    body.push_str(&chart(
        "Queue at the bottleneck",
        "ms",
        &[Series { name: "queueing delay", color: "#3FC8E4", dash: false, kind: Kind::Line, pts: sec(r.queue_series()) }],
        xs,
        &r.markers,
        false,
    ));
    let loss: Vec<(f64, f64)> = r
        .client_stats
        .iter()
        .filter_map(|t| {
            let c = t.line.client.as_ref()?;
            let n = c.packets_received + c.packets_lost;
            (n > 0).then(|| (t.t_ms / 1000.0, 100.0 * c.packets_lost as f64 / n as f64))
        })
        .collect();
    if !loss.is_empty() {
        body.push_str(&chart(
            "Stream packet loss",
            "%",
            &[Series { name: "packets lost", color: "#FF6A55", dash: false, kind: Kind::Line, pts: loss }],
            xs,
            &r.markers,
            false,
        ));
    }
    let gaps: Vec<(f64, f64)> = r
        .client_stats
        .iter()
        .filter_map(|t| t.line.extra("frame_gaps").map(|g| (t.t_ms / 1000.0, g)))
        .collect();
    let lost: Vec<(f64, f64)> = r
        .client_stats
        .iter()
        .filter_map(|t| t.line.client.as_ref().map(|c| (t.t_ms / 1000.0, c.frames_lost as f64)))
        .collect();
    if !gaps.is_empty() || lost.iter().any(|p| p.1 > 0.0) {
        body.push_str(&chart(
            "Frame gaps and lost frames",
            "per period",
            &[
                Series { name: "gaps", color: "#FF6A55", dash: false, kind: Kind::Bars, pts: gaps },
                Series { name: "frames lost", color: "#E8B44A", dash: false, kind: Kind::Line, pts: lost },
            ],
            xs,
            &r.markers,
            false,
        ));
    }

    body.push_str("<h2>Events</h2><div class=scroll><table><tr><th>at</th><th>event</th><th>until</th><th class=n>recovered after</th></tr>");
    for m in &r.markers {
        let rec = s.recoveries.iter().find(|x| x.t_ms == m.t_ms && x.label == m.label);
        let after = match rec {
            Some(x) => match x.recovered_after_s {
                Some(v) => format!("{v:.1} s"),
                None => "<span class=bad>never</span>".into(),
            },
            None => "—".into(),
        };
        let _ = write!(
            body,
            "<tr><td>{:.1} s</td><td>{}</td><td>{}</td><td class=n>{after}</td></tr>",
            m.t_ms as f64 / 1000.0,
            esc(&m.label),
            m.end_ms.map(|e| format!("{:.1} s", e as f64 / 1000.0)).unwrap_or_default()
        );
    }
    for ro in &r.roams {
        let _ = write!(body, "<tr><td>{:.1} s</td><td>client now {} / {}</td><td></td><td></td></tr>", ro.t_ms / 1000.0, ro.to.v4, ro.to.v6);
    }
    for f in &r.floods {
        let secs = ((f.stop_ms - f.start_ms) / 1000.0).max(1e-3);
        let _ = write!(
            body,
            "<tr><td>{:.1} s</td><td>{:?} flow {}: {:.1} Mbit/s delivered</td><td>{:.1} s</td><td></td></tr>",
            f.start_ms / 1000.0,
            f.proto,
            f.dir.name(),
            f.delivered_bytes as f64 * 8.0 / secs / 1e6,
            f.stop_ms / 1000.0
        );
    }
    body.push_str("</table></div>");

    body.push_str("<h2>Base link</h2><div class=scroll><table><tr><th></th><th class=n>delay ms</th><th class=n>jitter ms</th><th class=n>loss</th><th class=n>rate Mbit/s</th><th class=n>full queue</th><th class=n>dup/reorder/corrupt %</th></tr>");
    body.push_str(&shape_row("to client", &r.scenario.base.to_client));
    body.push_str(&shape_row("to server", &r.scenario.base.to_server));
    body.push_str("</table></div>");
    if !r.scenario.base.sources.is_empty() {
        body.push_str("<details><summary>Where the numbers come from</summary><ul>");
        for src in &r.scenario.base.sources {
            let _ = write!(body, "<li>{}</li>", esc(src));
        }
        body.push_str("</ul></details>");
    }

    body.push_str("<h2>Workloads</h2><div class=scroll><table><tr><th></th><th>command</th><th class=n>exit</th></tr>");
    for (name, w) in [("server", &r.server), ("client", &r.client)] {
        if let Some(w) = w {
            let code = match (w.code, w.exited_early) {
                (Some(c), true) => format!("{c} (early)"),
                (Some(c), false) => c.to_string(),
                (None, _) => "signal".into(),
            };
            let _ = write!(body, "<tr><td>{name}</td><td><code>{}</code></td><td class=n>{code}</td></tr>", esc(&w.command));
        }
    }
    body.push_str("</table></div>");
    if r.bad_stats_lines > 0 {
        let _ = write!(body, "<p class=bad>{} stats lines didn't parse and were skipped.</p>", r.bad_stats_lines);
    }

    body.push_str("<details><summary>Client stats, per period</summary><div class=scroll><table><tr><th class=n>t s</th><th class=n>frames</th><th class=n>lost</th><th class=n>gaps</th><th class=n>pkts</th><th class=n>pkts lost</th><th class=n>rtt ms</th><th class=n>jitter ms</th><th class=n>latency ms</th><th class=n>recv Mbit/s</th></tr>");
    for t in &r.client_stats {
        if let Some(c) = &t.line.client {
            let _ = write!(
                body,
                "<tr><td class=n>{:.1}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{:.1}</td><td class=n>{:.1}</td><td class=n>{:.1}</td><td class=n>{:.1}</td></tr>",
                t.t_ms / 1000.0,
                c.frames_received,
                c.frames_lost,
                t.line.extra("frame_gaps").unwrap_or(0.0),
                c.packets_received,
                c.packets_lost,
                c.rtt_us as f64 / 1000.0,
                c.jitter_us as f64 / 1000.0,
                c.capture_to_present_us as f64 / 1000.0,
                t.line.extra("recv_kbps").unwrap_or(0.0) / 1000.0
            );
        }
    }
    body.push_str("</table></div></details>");
    page(&format!("netlab: {}", r.label), &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_quantiles() {
        let v = [(10.0, 1.0), (20.0, 1.0), (30.0, 98.0)];
        assert_eq!(weighted_quantile(&v, 0.5), Some(30.0));
        assert_eq!(weighted_quantile(&v, 0.01), Some(10.0));
        assert_eq!(weighted_quantile(&[], 0.5), None);
    }

    #[test]
    fn charts_are_svg_with_one_axis_and_marked_events() {
        let m = Marker { t_ms: 5000, end_ms: Some(7000), kind: EventKind::Outage, label: "out <age>".into(), step_down: false };
        let c = chart(
            "Bitrate",
            "Mbit/s",
            &[
                Series { name: "a", color: "#3FC8E4", dash: false, kind: Kind::Line, pts: vec![(0.0, 1.0), (10.0, 3.0)] },
                Series { name: "b", color: "#8C93A3", dash: true, kind: Kind::Step, pts: vec![(0.0, 2.0)] },
            ],
            10.0,
            &[m],
            true,
        );
        assert!(c.contains("<svg") && c.contains("polyline") && c.contains("out &lt;age&gt;"));
        assert_eq!(c.matches("<svg").count(), 1);
        assert!(c.contains(r#"class="legend""#));
    }
}
