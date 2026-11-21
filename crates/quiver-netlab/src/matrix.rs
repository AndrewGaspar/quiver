//! `matrix`: the same workloads through many profiles and scenarios, each in a fresh lab,
//! and one summary table across them (`summary.json`, `summary.md`, `summary.html`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::Lab;
use crate::report::{Summary, page};
use crate::run::{RunSpec, run};
use crate::scenario::Scenario;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixRow {
    pub name: String,
    /// "profile" or "scenario".
    pub kind: String,
    pub duration_s: f64,
    /// The run's report, relative to the matrix's directory.
    pub report: Option<PathBuf>,
    pub summary: Option<Summary>,
    pub error: Option<String>,
}

/// One run of the matrix: a profile held steady or a scenario, and the workloads for it.
pub struct MatrixItem {
    /// "profile" or "scenario".
    pub kind: String,
    pub scenario: Scenario,
    pub server: Option<String>,
    pub client: Option<String>,
}

pub struct MatrixSpec {
    pub items: Vec<MatrixItem>,
    pub out_dir: PathBuf,
    pub path_probe_pps: u32,
}

/// Runs every item in its own lab, writing `<out>/<name>/report.{json,html}` and the
/// summaries. A failed item is recorded and the matrix goes on.
pub fn run_matrix(spec: &MatrixSpec, stop: &AtomicBool, mut progress: impl FnMut(&MatrixRow)) -> Result<Vec<MatrixRow>> {
    std::fs::create_dir_all(&spec.out_dir)?;
    let mut rows = Vec::new();
    for item in &spec.items {
        let (kind, sc) = (&item.kind, &item.scenario);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let dir = spec.out_dir.join(&sc.name);
        let result = (|| -> Result<Summary> {
            let lab = Lab::new()?;
            let rs = RunSpec {
                label: sc.name.clone(),
                scenario: sc.clone(),
                server: item.server.clone(),
                client: item.client.clone(),
                out_dir: dir.clone(),
                path_probe_pps: spec.path_probe_pps,
                env: vec![],
            };
            let report = run(&lab, &rs, stop)?;
            report.write(&dir.join("report.json"))?;
            Ok(report.summary)
        })();
        let row = match result {
            Ok(s) => MatrixRow {
                name: sc.name.clone(),
                kind: kind.clone(),
                duration_s: sc.duration_s,
                report: Some(Path::new(&sc.name).join("report.html")),
                summary: Some(s),
                error: None,
            },
            Err(e) => MatrixRow {
                name: sc.name.clone(),
                kind: kind.clone(),
                duration_s: sc.duration_s,
                report: None,
                summary: None,
                error: Some(format!("{e:#}")),
            },
        };
        progress(&row);
        rows.push(row);
    }
    std::fs::write(spec.out_dir.join("summary.json"), serde_json::to_string_pretty(&rows)?)?;
    std::fs::write(spec.out_dir.join("summary.md"), render_md(&rows))?;
    std::fs::write(spec.out_dir.join("summary.html"), render_html(&rows))?;
    Ok(rows)
}

fn f(v: Option<f64>, d: usize) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.d$}"),
        _ => "—".into(),
    }
}

fn cells(s: &Summary) -> [String; 8] {
    let recovery = match (s.recovered, s.recovery_s) {
        (Some(false), _) => "never".into(),
        (_, v) => f(v, 1),
    };
    [
        f(s.latency_p50_ms, 1),
        f(s.latency_p99_ms, 1),
        f(s.gaps_per_min, 1),
        format!("{} / {:.0}", f(s.mean_bitrate_mbit, 1), s.mean_capacity_mbit),
        f(s.utilisation_pct, 0),
        f(s.stream_loss_pct, 2),
        f(s.path_rtt_p50_ms, 1),
        recovery,
    ]
}

const HEAD: [&str; 8] = [
    "latency p50 ms",
    "latency p99 ms",
    "gaps/min",
    "bitrate / capacity Mbit/s",
    "util %",
    "loss %",
    "path RTT p50 ms",
    "recovery s",
];

/// The summary as a Markdown table.
pub fn render_md(rows: &[MatrixRow]) -> String {
    let mut s = format!("| name | kind | {} |\n|---|---|{}\n", HEAD.join(" | "), "---:|".repeat(HEAD.len()));
    for r in rows {
        match &r.summary {
            Some(sum) => {
                let _ = writeln!(s, "| {} | {} | {} |", r.name, r.kind, cells(sum).join(" | "));
            }
            None => {
                let _ = writeln!(s, "| {} | {} | failed: {} |{}", r.name, r.kind, r.error.as_deref().unwrap_or(""), " |".repeat(HEAD.len() - 1));
            }
        }
    }
    s
}

pub fn render_html(rows: &[MatrixRow]) -> String {
    let mut b = String::from("<h1>netlab matrix</h1><p class=lede>Latency is capture → present from the client's stats (the path's RTT when there are none); recovery is the slowest return to the pre-event latency after a step-down in rate.</p><div class=scroll><table><tr><th>name</th><th>kind</th>");
    for h in HEAD {
        let _ = write!(b, "<th class=n>{h}</th>");
    }
    b.push_str("</tr>");
    for r in rows {
        let name = match &r.report {
            Some(p) => format!("<a href=\"{}\">{}</a>", p.display(), r.name),
            None => r.name.clone(),
        };
        let _ = write!(b, "<tr><td>{name}</td><td>{}</td>", r.kind);
        match &r.summary {
            Some(s) => {
                for c in cells(s) {
                    let class = if c == "never" { "n bad" } else { "n" };
                    let _ = write!(b, "<td class=\"{class}\">{c}</td>");
                }
            }
            None => {
                let _ = write!(b, "<td class=bad colspan={}>{}</td>", HEAD.len(), r.error.as_deref().unwrap_or("failed"));
            }
        }
        b.push_str("</tr>");
    }
    b.push_str("</table></div>");
    page("netlab matrix", &b)
}
