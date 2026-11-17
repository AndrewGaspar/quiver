//! Metric descriptors and values as the mod SDK sends them (SIGHT-HUD §3.3–3.5).
//!
//! **Interim home.** These types belong to the `ubi-perf` crate (descriptor types and their
//! validation, SIGHT-HUD §4.2 phase 0). Until it merges they live here, and everything else
//! refers to them only as `ubi_proto::modsdk::metrics::*`, so switching is a re-export:
//! `pub use ubi_perf::descriptor::*;` in this file.

use serde::{Deserialize, Serialize};

/// Metrics one producer (a mod connection) may declare.
pub const MAX_METRICS: usize = 64;
/// `m` messages per second per connection, after the SDK's coalescing.
pub const M_PER_S: u32 = 10;
/// `mark` messages per second per connection.
pub const MARKS_PER_S: u32 = 20;
/// `text` and `state` values, and event labels.
pub const MAX_TEXT: usize = 48;
/// `state` values per metric.
pub const MAX_STATE_VALUES: usize = 16;
/// A full metric id (`aco-dlss.fg.state`).
pub const MAX_ID: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Gauge,
    Counter,
    Series,
    Frame,
    State,
    Text,
    Event,
    Histogram,
}

impl MetricKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Counter => "counter",
            Self::Series => "series",
            Self::Frame => "frame",
            Self::State => "state",
            Self::Text => "text",
            Self::Event => "event",
            Self::Histogram => "histogram",
        }
    }

    /// Kinds whose values travel as JSON (`m`, `mark`) in phase 1. The rest need a ring.
    pub fn json_transport(self) -> bool {
        matches!(self, Self::Gauge | Self::Counter | Self::State | Self::Text | Self::Event)
    }
}

pub const UNITS: &[&str] = &["", "ms", "us", "ns", "s", "fps", "Hz", "%", "C", "W", "MHz", "B", "B/s", "x", "px", "count"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Good,
    Warn,
    Bad,
    Dim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Higher,
    Lower,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    Number,
    Badge,
    Bar,
    Sparkline,
    Graph,
    Stack,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Format {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u8>,
    #[serde(default)]
    pub si: bool,
    #[serde(default)]
    pub sign: bool,
    #[serde(default)]
    pub percent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Range {
    Fixed { min: f64, max: f64 },
    /// `"auto"`
    Auto(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Threshold {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    pub level: Level,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateValue {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
}

/// A role claim: `"fg.state"`, or `{ role, flags, on }` for frame metrics (phase 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Role {
    Name(String),
    Filtered {
        role: String,
        #[serde(default)]
        flags: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<String>,
    },
}

impl Role {
    pub fn name(&self) -> &str {
        match self {
            Self::Name(n) | Self::Filtered { role: n, .. } => n,
        }
    }
}

/// A metric descriptor as a mod declares it at runtime. `name` is relative to the mod's
/// surface (`fg.state` becomes `aco-dlss.fg.state`). Ring-only fields (`fields`, `clock`,
/// `key`, `derive`) are phase 2 and ignored for now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDef {
    pub name: String,
    pub kind: MetricKind,
    #[serde(default)]
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thresholds: Vec<Threshold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub better: Option<Better>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<StateValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_from: Vec<String>,
}

/// `[a-z0-9_-]` segments joined by dots, `segments` long.
fn dotted(s: &str, segments: std::ops::RangeInclusive<usize>) -> bool {
    let n = s.split('.').count();
    segments.contains(&n)
        && s.split('.').all(|seg| {
            !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}

fn short_text(field: &str, v: &Option<String>, max: usize) -> Result<(), String> {
    match v {
        Some(s) if s.chars().count() > max => Err(format!("{field} is longer than {max} characters")),
        _ => Ok(()),
    }
}

impl MetricDef {
    /// The full id under a surface: `aco-dlss` + `fg.state`.
    pub fn id(&self, surface: &str) -> String {
        format!("{surface}.{}", self.name)
    }

    /// Validate a runtime declaration for `surface`. `json_only`: refuse kinds that need a
    /// ring (phase 1). The reason is what `metrics_ok.rejected` reports.
    pub fn validate(&self, surface: &str, json_only: bool) -> Result<(), String> {
        // 2–5 segments in all, the namespace being the first.
        if !dotted(&self.name, 1..=4) {
            return Err("name must be 1-4 dot-separated [a-z0-9_-] segments".into());
        }
        if self.id(surface).len() > MAX_ID {
            return Err(format!("id is longer than {MAX_ID} bytes"));
        }
        if json_only && !self.kind.json_transport() {
            return Err(format!("{} metrics need a shared-memory ring, which this Quiver doesn't offer yet", self.kind.as_str()));
        }
        if !UNITS.contains(&self.unit.as_str()) {
            return Err(format!("unknown unit '{}'", super::sanitize(&self.unit, 16)));
        }
        short_text("label", &self.label, 24)?;
        short_text("short", &self.short, 6)?;
        short_text("help", &self.help, 200)?;
        short_text("group", &self.group, 24)?;
        if let Some(Range::Fixed { min, max }) = self.range {
            if !(min.is_finite() && max.is_finite() && min < max) {
                return Err("range needs finite min < max".into());
            }
        }
        if let Some(Range::Auto(s)) = &self.range
            && s != "auto"
        {
            return Err("range is { min, max } or \"auto\"".into());
        }
        if self.thresholds.len() > 8 {
            return Err("at most 8 thresholds".into());
        }
        for t in &self.thresholds {
            let finite = |v: Option<f64>| v.is_none_or(f64::is_finite);
            if !(finite(t.above) && finite(t.below)) || t.above.is_some() == t.below.is_some() {
                return Err("a threshold has exactly one finite `above` or `below`".into());
            }
        }
        if self.format.as_ref().and_then(|f| f.precision).is_some_and(|p| p > 6) {
            return Err("precision is at most 6".into());
        }
        if self.kind == MetricKind::State {
            if self.values.len() > MAX_STATE_VALUES {
                return Err(format!("at most {MAX_STATE_VALUES} state values"));
            }
            for v in &self.values {
                if v.value.is_empty() || v.value.len() > MAX_TEXT || super::sanitize(&v.value, MAX_TEXT) != v.value {
                    return Err("state values are 1-48 bytes of plain text".into());
                }
                short_text("a state label", &v.label, 24)?;
            }
        } else if !self.values.is_empty() {
            return Err("only state metrics have values".into());
        }
        if self.roles.len() > 8 || self.roles.iter().any(|r| !dotted(r.name(), 1..=3)) {
            return Err("roles are up to 8 canonical role names".into());
        }
        if self.renamed_from.len() > 8 || self.renamed_from.iter().any(|r| !dotted(r, 1..=5)) {
            return Err("renamed_from lists up to 8 ids".into());
        }
        if let Some(l) = &self.level
            && !matches!(l.as_str(), "basic" | "advanced" | "debug")
        {
            return Err("level is basic, advanced or debug".into());
        }
        Ok(())
    }

    /// Display strings, sanitized (control characters and bidi overrides removed).
    pub fn sanitized(mut self) -> Self {
        let clean = |s: &mut Option<String>, max| {
            if let Some(v) = s {
                *v = super::sanitize(v, max);
            }
        };
        clean(&mut self.label, 96);
        clean(&mut self.short, 24);
        clean(&mut self.help, 800);
        clean(&mut self.group, 96);
        for v in &mut self.values {
            clean(&mut v.label, 96);
        }
        self
    }
}

/// A validated low-rate value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetricValue {
    Number(f64),
    Total(u64),
    Text(String),
}

impl MetricValue {
    /// Interpret a JSON value from `m` for a metric of `kind`; `None` if it doesn't fit
    /// (wrong type, NaN/∞, a state that isn't declared, text too long is cut).
    pub fn from_json(kind: MetricKind, v: &serde_json::Value, states: &[StateValue]) -> Option<Self> {
        match kind {
            MetricKind::Gauge => v.as_f64().filter(|x| x.is_finite()).map(Self::Number),
            MetricKind::Counter => v.as_u64().map(Self::Total),
            MetricKind::Text => v.as_str().map(|s| Self::Text(super::sanitize(s, MAX_TEXT))),
            MetricKind::State => {
                let s = v.as_str()?;
                // Without declared values (a static descriptor quiverd doesn't have yet) any
                // short plain string is accepted.
                (states.is_empty() || states.iter().any(|x| x.value == s)).then(|| Self::Text(super::sanitize(s, MAX_TEXT)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(v: serde_json::Value) -> MetricDef {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn descriptors_validate() {
        let fg = def(json!({"name":"fg.state","kind":"state","label":"Frame Generation","short":"FG",
            "values":[{"value":"off","level":"dim"},{"value":"on"},{"value":"failed","level":"bad"}],"roles":["fg.state"]}));
        assert_eq!(fg.validate("aco-dlss", true), Ok(()));
        assert_eq!(fg.id("aco-dlss"), "aco-dlss.fg.state");
        let lat = def(json!({"name":"latency.render","kind":"gauge","unit":"ms","thresholds":[{"above":30,"level":"warn"}],"range":{"min":0,"max":100}}));
        assert_eq!(lat.validate("aco-dlss", true), Ok(()));
        let frame = def(json!({"name":"frame","kind":"frame"}));
        assert!(frame.validate("aco-dlss", true).unwrap_err().contains("ring"));
        assert_eq!(frame.validate("aco-dlss", false), Ok(()));
        for bad in [
            json!({"name":"Fg","kind":"gauge"}),
            json!({"name":"a.b.c.d.e","kind":"gauge"}),
            json!({"name":"x","kind":"gauge","unit":"parsecs"}),
            json!({"name":"x","kind":"gauge","label":"a label that is far too long for a cell"}),
            json!({"name":"x","kind":"gauge","range":{"min":5,"max":1}}),
            json!({"name":"x","kind":"gauge","thresholds":[{"level":"bad"}]}),
            json!({"name":"x","kind":"gauge","values":[{"value":"a"}]}),
            json!({"name":"x","kind":"state","values":[{"value":"a\u{202e}"}]}),
        ] {
            assert!(def(bad.clone()).validate("m", true).is_err(), "{bad}");
        }
        assert!(def(json!({"name":&"x".repeat(70),"kind":"gauge"})).validate("m", true).is_err());
        // Unknown descriptor fields (phase 2's `fields`, a newer SDK's) are ignored.
        assert!(serde_json::from_value::<MetricDef>(json!({"name":"x","kind":"gauge","fields":["a"],"new":1})).is_ok());
        assert!(serde_json::from_value::<MetricDef>(json!({"name":"x","kind":"sparkle"})).is_err());
    }

    #[test]
    fn values_fit_their_kind() {
        let states = [StateValue { value: "on".into(), label: None, level: None }];
        assert_eq!(MetricValue::from_json(MetricKind::Gauge, &json!(1.5), &[]), Some(MetricValue::Number(1.5)));
        assert_eq!(MetricValue::from_json(MetricKind::Gauge, &json!("1.5"), &[]), None);
        assert_eq!(MetricValue::from_json(MetricKind::Counter, &json!(7), &[]), Some(MetricValue::Total(7)));
        assert_eq!(MetricValue::from_json(MetricKind::Counter, &json!(-1), &[]), None);
        assert_eq!(MetricValue::from_json(MetricKind::State, &json!("on"), &states), Some(MetricValue::Text("on".into())));
        assert_eq!(MetricValue::from_json(MetricKind::State, &json!("maybe"), &states), None);
        let long = MetricValue::from_json(MetricKind::Text, &json!("y".repeat(100)), &[]);
        assert_eq!(long, Some(MetricValue::Text("y".repeat(48))));
    }
}
