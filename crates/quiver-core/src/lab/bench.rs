//! A/B runs (generalised from the Pharos harness's `bench` and `abtest`): config variants,
//! each applied live (or with a relaunch), a settle phase, N samples, aggregated. Samples
//! are screenshot metrics over a crop and/or numbers taken from a log with a regex; the
//! host's state is recorded with each variant. Rounds interleave the variants (A B A B …)
//! so drift (heat, streaming) spreads over all of them.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::configset::Assign;
use super::host::HostState;

/// `LABEL=ASSIGN[;ASSIGN…]` (an empty list is the baseline as it stands).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub label: String,
    pub assigns: Vec<Assign>,
}

impl Variant {
    pub fn parse(s: &str) -> Result<Variant> {
        let (label, rest) = s.split_once('=').with_context(|| format!("variant '{s}': expected LABEL=FILE:[section]key=value[;…]"))?;
        if label.is_empty() || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') {
            bail!("variant label '{label}': letters, digits, - _ .");
        }
        let assigns = rest.split(';').map(str::trim).filter(|a| !a.is_empty()).map(Assign::parse).collect::<Result<_>>()?;
        Ok(Variant { label: label.into(), assigns })
    }
}

/// A number from a log: the first capture group of `pattern` in lines written after the
/// variant settled (`perf: ([0-9.]+) fps`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogMetric {
    pub name: String,
    pub pattern: String,
}

impl LogMetric {
    /// `NAME=REGEX`.
    pub fn parse(s: &str) -> Result<LogMetric> {
        let (name, pattern) = s.split_once('=').with_context(|| format!("log metric '{s}': expected NAME=REGEX with one capture group"))?;
        let re = regex::Regex::new(pattern).with_context(|| format!("log metric '{name}'"))?;
        if re.captures_len() < 2 {
            bail!("log metric '{name}': the regex needs a capture group around the number");
        }
        Ok(LogMetric { name: name.into(), pattern: pattern.into() })
    }

    /// Every match in `text`, in order.
    pub fn values(&self, text: &str) -> Vec<f64> {
        let Ok(re) = regex::Regex::new(&self.pattern) else { return Vec::new() };
        text.lines().filter_map(|l| re.captures(l)?.get(1)?.as_str().trim().parse::<f64>().ok()).collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub n: usize,
    pub mean: f64,
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
}

impl Stats {
    pub fn of(v: &[f64]) -> Option<Stats> {
        if v.is_empty() {
            return None;
        }
        let n = v.len() as f64;
        let mean = v.iter().sum::<f64>() / n;
        let var = if v.len() > 1 { v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0) } else { 0.0 };
        Some(Stats {
            n: v.len(),
            mean,
            stddev: var.sqrt(),
            min: v.iter().copied().fold(f64::INFINITY, f64::min),
            max: v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        })
    }
}

/// One variant's samples (every round's), by metric name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VariantResult {
    pub label: String,
    pub samples: BTreeMap<String, Vec<f64>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<HostState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl VariantResult {
    pub fn stats(&self) -> BTreeMap<String, Stats> {
        self.samples.iter().filter_map(|(k, v)| Some((k.clone(), Stats::of(v)?))).collect()
    }
}

/// The plan's order: each round runs every variant once.
pub fn schedule(variants: usize, rounds: usize) -> Vec<usize> {
    (0..rounds).flat_map(|_| 0..variants).collect()
}

/// A table for the terminal: one row per variant, `mean ± sd` per metric.
pub fn table(results: &[VariantResult]) -> String {
    let metrics: Vec<String> = {
        let mut m: Vec<String> = results.iter().flat_map(|r| r.samples.keys().cloned()).collect();
        m.sort();
        m.dedup();
        m
    };
    let w = results.iter().map(|r| r.label.len()).max().unwrap_or(5).max(7);
    let mut out = format!("{:<w$}", "variant");
    for m in &metrics {
        out.push_str(&format!("  {m:>22}"));
    }
    out.push('\n');
    for r in results {
        out.push_str(&format!("{:<w$}", r.label));
        let st = r.stats();
        for m in &metrics {
            let cell = st.get(m).map_or("-".into(), |s| format!("{:.3} ± {:.3} (n={})", s.mean, s.stddev, s.n));
            out.push_str(&format!("  {cell:>22}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_and_metrics() {
        let v = Variant::parse("dlaa=pharos:[upscaler]mode=dlaa; pharos:[upscaler]sharpen=0").unwrap();
        assert_eq!((v.label.as_str(), v.assigns.len()), ("dlaa", 2));
        assert!(Variant::parse("base=").unwrap().assigns.is_empty());
        assert!(Variant::parse("bad label=x:a.b=1").is_err());
        let m = LogMetric::parse(r"fps=perf: ([0-9.]+) fps").unwrap();
        assert_eq!(m.values("x\nperf: 143.5 fps render 1\nperf: 140 fps\n"), [143.5, 140.0]);
        assert!(LogMetric::parse("fps=perf").is_err());
    }

    #[test]
    fn stats_and_schedule() {
        let s = Stats::of(&[1.0, 2.0, 3.0]).unwrap();
        assert_eq!((s.n, s.mean, s.stddev, s.min, s.max), (3, 2.0, 1.0, 1.0, 3.0));
        assert!(Stats::of(&[]).is_none());
        assert_eq!(schedule(2, 3), [0, 1, 0, 1, 0, 1]);
        let r = VariantResult { label: "a".into(), samples: BTreeMap::from([("fps".into(), vec![1.0, 3.0])]), ..Default::default() };
        assert!(table(&[r]).contains("2.000 ± 1.414 (n=2)"));
    }
}
