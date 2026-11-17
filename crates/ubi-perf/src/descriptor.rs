//! Metric descriptors (SIGHT-HUD §3.3–3.5): the static description of one metric, as a mod
//! ships it in `quiver-options.toml` (`[metric.<name>]`) or declares it at runtime over the
//! SDK (`metrics` message), and as quiverd's catalog serves it. serde types plus validation.
//!
//! Every descriptor from a producer is untrusted: [`MetricDescriptor::sanitize`] cleans its
//! display strings, [`MetricDescriptor::validate`] checks everything else, and
//! [`validate_set`] applies the per-producer rules (namespace, count, duplicates). Invalid
//! descriptors are rejected whole with a reason, never half-applied.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// The descriptor format (`format = 1`).
pub const DESCRIPTOR_FORMAT: u32 = 1;

/// Limits from §3.3 / §3.16.
pub mod limits {
    pub const MAX_ID_LEN: usize = 64;
    pub const MIN_ID_SEGMENTS: usize = 2;
    pub const MAX_ID_SEGMENTS: usize = 5;
    /// Characters.
    pub const MAX_LABEL: usize = 24;
    pub const MAX_SHORT: usize = 6;
    pub const MAX_HELP: usize = 200;
    pub const MAX_GROUP: usize = 24;
    /// Bytes of UTF-8: `text` values and event labels.
    pub const MAX_TEXT_BYTES: usize = 48;
    pub const MAX_STATE_VALUES: usize = 16;
    pub const MAX_FIELDS: usize = 12;
    pub const MAX_FIELD_NAME: usize = 24;
    pub const MAX_THRESHOLDS: usize = 8;
    pub const MAX_ROLES: usize = 4;
    pub const MAX_DERIVE: usize = 8;
    pub const MAX_RENAMED: usize = 8;
    pub const MAX_PRECISION: u8 = 6;
    pub const MAX_HISTOGRAM_BUCKETS: usize = 64;
    pub const MAX_METRICS_PER_PRODUCER: usize = 64;
    pub const MAX_METRICS_PER_SESSION: usize = 512;
    /// JSON samples per producer per second, after coalescing.
    pub const MAX_JSON_HZ: u32 = 10;
    pub const MAX_MARKS_PER_SEC: u32 = 20;
    pub const MAX_RING_RECORDS_PER_SEC: u32 = 20_000;
    /// Bytes a mod may ask for in `ring_req`.
    pub const MAX_RING_REQ_BYTES: u64 = 1 << 20;
}
use limits::*;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// Kinds whose values travel in rings (events may also come as JSON marks).
    pub fn uses_ring(self) -> bool {
        matches!(self, MetricKind::Series | MetricKind::Frame | MetricKind::Event)
    }

    /// `quiver_kind` in the SDK header (histograms are JSON-only and have none).
    pub fn sdk_code(self) -> Option<u32> {
        Some(match self {
            MetricKind::Gauge => 1,
            MetricKind::Counter => 2,
            MetricKind::Series => 3,
            MetricKind::Frame => 4,
            MetricKind::State => 5,
            MetricKind::Text => 6,
            MetricKind::Event => 7,
            MetricKind::Histogram => return None,
        })
    }

    pub fn from_sdk_code(code: u32) -> Option<MetricKind> {
        Some(match code {
            1 => MetricKind::Gauge,
            2 => MetricKind::Counter,
            3 => MetricKind::Series,
            4 => MetricKind::Frame,
            5 => MetricKind::State,
            6 => MetricKind::Text,
            7 => MetricKind::Event,
            _ => return None,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Unit {
    #[default]
    #[serde(rename = "")]
    None,
    #[serde(rename = "ms")]
    Ms,
    #[serde(rename = "us")]
    Us,
    #[serde(rename = "ns")]
    Ns,
    #[serde(rename = "s")]
    S,
    #[serde(rename = "fps")]
    Fps,
    #[serde(rename = "Hz")]
    Hz,
    #[serde(rename = "%")]
    Percent,
    #[serde(rename = "C")]
    Celsius,
    #[serde(rename = "W")]
    Watt,
    #[serde(rename = "MHz")]
    MHz,
    #[serde(rename = "B")]
    Bytes,
    #[serde(rename = "B/s")]
    BytesPerSec,
    #[serde(rename = "x")]
    Ratio,
    #[serde(rename = "px")]
    Px,
    #[serde(rename = "count")]
    Count,
}

impl Unit {
    pub fn is_none(&self) -> bool {
        *self == Unit::None
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Format {
    /// Decimals, ≤ 6.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precision: Option<u8>,
    /// SI prefixes (B → GB).
    #[serde(skip_serializing_if = "is_false")]
    pub si: bool,
    /// Always show the sign.
    #[serde(skip_serializing_if = "is_false")]
    pub sign: bool,
    /// The value is a 0–1 fraction shown as a percentage.
    #[serde(skip_serializing_if = "is_false")]
    pub percent: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `range`: `"auto"` or `{ min, max }`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Range {
    Auto,
    Fixed { min: f64, max: f64 },
}

impl Serialize for Range {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            Range::Auto => s.serialize_str("auto"),
            Range::Fixed { min, max } => {
                let mut st = s.serialize_struct("Range", 2)?;
                st.serialize_field("min", min)?;
                st.serialize_field("max", max)?;
                st.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Range {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Range;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("\"auto\" or { min, max }")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Range, E> {
                if v == "auto" { Ok(Range::Auto) } else { Err(E::invalid_value(de::Unexpected::Str(v), &self)) }
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Range, A::Error> {
                let (mut min, mut max) = (None, None);
                while let Some(k) = m.next_key::<String>()? {
                    match k.as_str() {
                        "min" => min = Some(m.next_value()?),
                        "max" => max = Some(m.next_value()?),
                        _ => {
                            m.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(Range::Fixed { min: min.ok_or_else(|| de::Error::missing_field("min"))?, max: max.ok_or_else(|| de::Error::missing_field("max"))? })
            }
        }
        d.deserialize_any(V)
    }
}

/// Theme levels (§1.2). Thresholds use `good`, `warn`, `bad`, `dim`; state values any.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Normal,
    Dim,
    Good,
    Warn,
    Bad,
    Accent,
}

/// `{ above = 30, level = "warn" }` or `{ below = …, level = … }`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Threshold {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    pub level: Level,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Higher,
    Lower,
    None,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    Number,
    Badge,
    Bar,
    Sparkline,
    Graph,
    Stack,
}

/// `sample`: `"per-frame"`, `"per-event"`, or a rate in Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleRate {
    PerFrame,
    PerEvent,
    Hz(f64),
}

impl Serialize for SampleRate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SampleRate::PerFrame => s.serialize_str("per-frame"),
            SampleRate::PerEvent => s.serialize_str("per-event"),
            SampleRate::Hz(hz) => s.serialize_f64(*hz),
        }
    }
}

impl<'de> Deserialize<'de> for SampleRate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = SampleRate;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("\"per-frame\", \"per-event\" or a rate in Hz")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<SampleRate, E> {
                match v {
                    "per-frame" => Ok(SampleRate::PerFrame),
                    "per-event" => Ok(SampleRate::PerEvent),
                    _ => Err(E::invalid_value(de::Unexpected::Str(v), &self)),
                }
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<SampleRate, E> {
                Ok(SampleRate::Hz(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<SampleRate, E> {
                Ok(SampleRate::Hz(v as f64))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<SampleRate, E> {
                Ok(SampleRate::Hz(v as f64))
            }
        }
        d.deserialize_any(V)
    }
}

/// One of a `state` metric's values.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StateValue {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
}

/// Clock domain of a ring metric's timestamps (§3.9).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Clock {
    Qpc,
    MonoRawNs,
    ReflexUs,
}

/// What a `frame` record's `key` means, for correlation (§3.10).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyMeaning {
    /// The producer's own per-swapchain present sequence (1, 2, 3, …).
    PresentSeq,
    /// A Reflex / game frame id.
    FrameId,
    /// `VkPresentIdKHR` (DXVK's frame id), else the layer's count: the layer's records.
    PresentId,
}

/// Frame record flags by name, for role filters.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FrameFlag {
    Real,
    Generated,
    Nofg,
}

impl FrameFlag {
    pub fn bit(self) -> u8 {
        match self {
            FrameFlag::Real => crate::layout::FLAG_REAL,
            FrameFlag::Generated => crate::layout::FLAG_GENERATED,
            FrameFlag::Nofg => crate::layout::FLAG_NOFG,
        }
    }
}

/// Canonical roles (§3.4) and the kinds that may fill them.
pub const ROLES: &[(&str, MetricKind)] = &[
    ("frame.presented", MetricKind::Frame),
    ("frame.base", MetricKind::Frame),
    ("frame.generated", MetricKind::Frame),
    ("fg.state", MetricKind::State),
    ("latency.frame", MetricKind::Frame),
    ("render.resolution", MetricKind::Text),
    ("output.resolution", MetricKind::Text),
    ("upscaler", MetricKind::Text),
];

/// The kind a known role needs; `None` for a role this version doesn't know (accepted, so a
/// newer mod still validates, but it binds nothing).
pub fn role_kind(role: &str) -> Option<MetricKind> {
    ROLES.iter().find(|(r, _)| *r == role).map(|(_, k)| *k)
}

/// `roles`: `"fg.state"` or `{ role = "frame.base", flags = ["real", "nofg"], on = "game_present" }`.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct RoleClaim {
    pub role: String,
    /// Only records with one of these flags fill the role (frame metrics).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<FrameFlag>,
    /// The field whose timestamps give intervals (frame metrics); default the record's `t`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
}

impl<'de> Deserialize<'de> for RoleClaim {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Full {
            role: String,
            #[serde(default)]
            flags: Vec<FrameFlag>,
            #[serde(default)]
            on: Option<String>,
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RoleClaim;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a role name or { role, flags, on }")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<RoleClaim, E> {
                Ok(RoleClaim { role: v.to_string(), flags: Vec::new(), on: None })
            }
            fn visit_map<A: MapAccess<'de>>(self, m: A) -> Result<RoleClaim, A::Error> {
                let f = Full::deserialize(de::value::MapAccessDeserializer::new(m))?;
                Ok(RoleClaim { role: f.role, flags: f.flags, on: f.on })
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeriveFn {
    /// Time between consecutive records (optionally filtered by flags), on a field.
    Interval,
    /// `to − from` within one record.
    Span,
    /// A counter per second.
    Rate,
    /// One metric over another (`of = [x, y]`).
    Ratio,
    /// Consecutive spans between `segments` as a stacked bar.
    Stack,
}

/// A derived metric (§3.10), computed only in quiverd. Its id is `<metric id>.<id>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Derive {
    pub id: String,
    #[serde(rename = "fn")]
    pub func: Option<DeriveFn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<FrameFlag>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<String>,
    /// `ratio`: the two metric ids, numerator first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub of: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub thresholds: Vec<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub better: Option<Better>,
}

/// Catalog visibility, as MOD-CONFIG's options (`level` in the descriptor).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Basic,
    Advanced,
    Debug,
}

/// One metric's descriptor. Unknown fields are ignored (forward compatibility). In
/// `quiver-options.toml` the table key is the name and `id` is absent: set it with
/// [`qualify`] before validating.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MetricDescriptor {
    #[serde(default)]
    pub id: String,
    pub kind: MetricKind,
    #[serde(default, skip_serializing_if = "Unit::is_none")]
    pub unit: Unit,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<SampleRate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<StateValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyMeaning>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<RoleClaim>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derive: Vec<Derive>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_from: Vec<String>,
    #[serde(rename = "level", default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

/// Why a descriptor was rejected: the field and a reason, for `metrics_ok.rejected` and
/// `options lint`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorError {
    pub field: &'static str,
    pub reason: String,
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

fn err(field: &'static str, reason: impl Into<String>) -> DescriptorError {
    DescriptorError { field, reason: reason.into() }
}

/// `<namespace>.<name>` for a name relative to a producer's namespace (the SDK's and the
/// `[metric.<name>]` table's form).
pub fn qualify(namespace: &str, name: &str) -> String {
    format!("{namespace}.{name}")
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A producer metric id: 2–5 `[a-z0-9_-]` segments separated by dots, ≤ 64 bytes.
pub fn validate_metric_id(id: &str) -> Result<(), String> {
    if id.len() > MAX_ID_LEN {
        return Err(format!("longer than {MAX_ID_LEN} bytes"));
    }
    let n = id.split('.').count();
    if !(MIN_ID_SEGMENTS..=MAX_ID_SEGMENTS).contains(&n) {
        return Err(format!("{n} segments (need {MIN_ID_SEGMENTS}–{MAX_ID_SEGMENTS})"));
    }
    if !id.split('.').all(valid_segment) {
        return Err("segments must be [a-z0-9_-]+".into());
    }
    Ok(())
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Turns whitespace runs (newlines, tabs, separators) into single spaces, removes control
/// characters (C0, C1, DEL), bidi marks/overrides/isolates and BOMs, trims, and cuts at a char boundary
/// to `max_bytes`. (NFC normalization is left to the display side.)
pub fn sanitize_text(s: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_bytes));
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_bidi_control(c) || c == '\u{FEFF}' {
            continue;
        }
        let extra = if space { 1 } else { 0 };
        if out.len() + extra + c.len_utf8() > max_bytes {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
}

/// [`sanitize_text`] for raw bytes from a ring (lossy UTF-8 first).
pub fn sanitize_bytes(b: &[u8], max_bytes: usize) -> String {
    sanitize_text(&String::from_utf8_lossy(b), max_bytes)
}

fn check_chars(field: &'static str, s: &Option<String>, max: usize) -> Result<(), DescriptorError> {
    match s {
        Some(s) if s.chars().count() > max => Err(err(field, format!("longer than {max} characters"))),
        Some(s) if s.is_empty() => Err(err(field, "empty")),
        _ => Ok(()),
    }
}

fn check_thresholds(field: &'static str, t: &[Threshold]) -> Result<(), DescriptorError> {
    if t.len() > MAX_THRESHOLDS {
        return Err(err(field, format!("more than {MAX_THRESHOLDS}")));
    }
    for th in t {
        let v = match (th.above, th.below) {
            (Some(v), None) | (None, Some(v)) => v,
            _ => return Err(err(field, "each needs exactly one of above / below")),
        };
        if !v.is_finite() {
            return Err(err(field, "not finite"));
        }
        if !matches!(th.level, Level::Good | Level::Warn | Level::Bad | Level::Dim) {
            return Err(err(field, "level must be good, warn, bad or dim"));
        }
    }
    Ok(())
}

fn check_format(field: &'static str, f: &Option<Format>) -> Result<(), DescriptorError> {
    match f {
        Some(Format { precision: Some(p), .. }) if *p > MAX_PRECISION => Err(err(field, format!("precision above {MAX_PRECISION}"))),
        _ => Ok(()),
    }
}

fn valid_field_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_FIELD_NAME && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl MetricDescriptor {
    /// A minimal descriptor.
    pub fn new(id: impl Into<String>, kind: MetricKind) -> MetricDescriptor {
        MetricDescriptor {
            id: id.into(),
            kind,
            unit: Unit::None,
            label: None,
            short: None,
            help: None,
            format: None,
            range: None,
            thresholds: Vec::new(),
            better: None,
            display: None,
            group: None,
            order: None,
            sample: None,
            values: Vec::new(),
            fields: Vec::new(),
            clock: None,
            key: None,
            roles: Vec::new(),
            derive: Vec::new(),
            renamed_from: Vec::new(),
            visibility: None,
        }
    }

    /// Cleans every display string (labels, help, group, state labels, derived labels) with
    /// [`sanitize_text`]. Length limits are then enforced by [`validate`](Self::validate),
    /// so an over-long label is reported rather than silently cut.
    pub fn sanitize(&mut self) {
        fn clean(s: &mut Option<String>) {
            if let Some(v) = s {
                *v = sanitize_text(v, MAX_HELP * 4);
            }
        }
        clean(&mut self.label);
        clean(&mut self.short);
        clean(&mut self.help);
        clean(&mut self.group);
        for v in &mut self.values {
            clean(&mut v.label);
        }
        for d in &mut self.derive {
            clean(&mut d.label);
            clean(&mut d.short);
            clean(&mut d.help);
        }
    }

    /// Validates the descriptor. With `namespace`, the id must be under it (a producer can
    /// only declare its own metrics).
    pub fn validate(&self, namespace: Option<&str>) -> Result<(), DescriptorError> {
        validate_metric_id(&self.id).map_err(|r| err("id", r))?;
        if let Some(ns) = namespace
            && !self.id.strip_prefix(ns).is_some_and(|rest| rest.starts_with('.'))
        {
            return Err(err("id", format!("not in namespace {ns}")));
        }
        check_chars("label", &self.label, MAX_LABEL)?;
        check_chars("short", &self.short, MAX_SHORT)?;
        check_chars("help", &self.help, MAX_HELP)?;
        check_chars("group", &self.group, MAX_GROUP)?;
        check_format("format", &self.format)?;
        if let Some(Range::Fixed { min, max }) = self.range
            && !(min.is_finite() && max.is_finite() && min < max)
        {
            return Err(err("range", "min and max must be finite, min < max"));
        }
        check_thresholds("thresholds", &self.thresholds)?;
        if let Some(SampleRate::Hz(hz)) = self.sample
            && !(hz.is_finite() && hz > 0.0 && hz <= 10_000.0)
        {
            return Err(err("sample", "rate must be in (0, 10000] Hz"));
        }
        // Per-kind fields.
        let kind = self.kind;
        if kind == MetricKind::State {
            if self.values.is_empty() || self.values.len() > MAX_STATE_VALUES {
                return Err(err("values", format!("a state needs 1–{MAX_STATE_VALUES} values")));
            }
            for (i, v) in self.values.iter().enumerate() {
                if v.value.is_empty() || v.value.len() > MAX_TEXT_BYTES || v.value.chars().any(|c| c.is_control() || is_bidi_control(c)) {
                    return Err(err("values", format!("bad value {:?}", v.value)));
                }
                if self.values[..i].iter().any(|o| o.value == v.value) {
                    return Err(err("values", format!("duplicate value {:?}", v.value)));
                }
                check_chars("values", &v.label, MAX_LABEL)?;
            }
        } else if !self.values.is_empty() {
            return Err(err("values", "only for kind state"));
        }
        if kind == MetricKind::Frame {
            if self.fields.len() > MAX_FIELDS {
                return Err(err("fields", format!("more than {MAX_FIELDS}")));
            }
            for (i, f) in self.fields.iter().enumerate() {
                if !valid_field_name(f) {
                    return Err(err("fields", format!("bad field name {f:?}")));
                }
                if self.fields[..i].contains(f) {
                    return Err(err("fields", format!("duplicate field {f:?}")));
                }
            }
        } else if !self.fields.is_empty() {
            return Err(err("fields", "only for kind frame"));
        }
        if self.key.is_some() && kind != MetricKind::Frame {
            return Err(err("key", "only for kind frame"));
        }
        if self.clock.is_some() && !kind.uses_ring() {
            return Err(err("clock", "only for ring kinds (series, frame, event)"));
        }
        let has_field = |f: &str| self.fields.iter().any(|x| x == f);
        // Roles.
        if self.roles.len() > MAX_ROLES {
            return Err(err("roles", format!("more than {MAX_ROLES}")));
        }
        for r in &self.roles {
            if r.role.len() > MAX_ID_LEN || !r.role.split('.').all(valid_segment) {
                return Err(err("roles", format!("bad role name {:?}", r.role)));
            }
            if let Some(need) = role_kind(&r.role)
                && need != kind
            {
                return Err(err("roles", format!("{} needs a {need:?} metric", r.role)));
            }
            if (!r.flags.is_empty() || r.on.is_some()) && kind != MetricKind::Frame {
                return Err(err("roles", "flags / on only for frame metrics"));
            }
            if let Some(on) = &r.on
                && !has_field(on)
            {
                return Err(err("roles", format!("on = {on:?} is not a field")));
            }
        }
        // Derived metrics.
        if self.derive.len() > MAX_DERIVE {
            return Err(err("derive", format!("more than {MAX_DERIVE}")));
        }
        for (i, d) in self.derive.iter().enumerate() {
            if !valid_segment(&d.id) {
                return Err(err("derive", format!("bad id {:?}", d.id)));
            }
            validate_metric_id(&format!("{}.{}", self.id, d.id)).map_err(|r| err("derive", format!("{}: {r}", d.id)))?;
            if self.derive[..i].iter().any(|o| o.id == d.id) {
                return Err(err("derive", format!("duplicate id {:?}", d.id)));
            }
            check_chars("derive", &d.label, MAX_LABEL)?;
            check_chars("derive", &d.short, MAX_SHORT)?;
            check_chars("derive", &d.help, MAX_HELP)?;
            check_format("derive", &d.format)?;
            check_thresholds("derive", &d.thresholds)?;
            let field_ok = |f: &Option<String>| f.as_deref().is_some_and(&has_field);
            match d.func {
                None => return Err(err("derive", format!("{}: missing fn", d.id))),
                Some(DeriveFn::Interval) => {
                    if kind != MetricKind::Frame && kind != MetricKind::Event {
                        return Err(err("derive", "interval needs a frame or event metric"));
                    }
                    if d.on.is_some() && !field_ok(&d.on) {
                        return Err(err("derive", "interval: on is not a field"));
                    }
                }
                Some(DeriveFn::Span) => {
                    if kind != MetricKind::Frame || !field_ok(&d.from) || !field_ok(&d.to) {
                        return Err(err("derive", "span needs a frame metric and from / to fields"));
                    }
                }
                Some(DeriveFn::Stack) => {
                    if kind != MetricKind::Frame || d.segments.len() < 2 || !d.segments.iter().all(|s| has_field(s)) {
                        return Err(err("derive", "stack needs a frame metric and ≥ 2 segment fields"));
                    }
                }
                Some(DeriveFn::Rate) => {
                    if kind != MetricKind::Counter {
                        return Err(err("derive", "rate needs a counter"));
                    }
                }
                Some(DeriveFn::Ratio) => {
                    if d.of.len() != 2 || d.of.iter().any(|m| m.len() > MAX_ID_LEN || !m.split('.').all(valid_segment)) {
                        return Err(err("derive", "ratio needs of = [x, y] metric ids"));
                    }
                }
            }
            if !d.flags.is_empty() && kind != MetricKind::Frame {
                return Err(err("derive", "flags only for frame metrics"));
            }
        }
        if self.renamed_from.len() > MAX_RENAMED {
            return Err(err("renamed_from", format!("more than {MAX_RENAMED}")));
        }
        for old in &self.renamed_from {
            validate_metric_id(old).map_err(|r| err("renamed_from", format!("{old}: {r}")))?;
        }
        Ok(())
    }

    /// The ids of this metric's derived metrics.
    pub fn derived_ids(&self) -> impl Iterator<Item = String> + '_ {
        self.derive.iter().map(|d| format!("{}.{}", self.id, d.id))
    }

    /// Index of a frame field by name.
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f == name)
    }
}

/// The outcome of [`validate_set`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetOutcome {
    pub accepted: Vec<MetricDescriptor>,
    /// id (or the position, when the id is unusable) → reason.
    pub rejected: BTreeMap<String, String>,
}

/// Validates a producer's declarations: sanitizes and validates each, keeps them in its
/// namespace, rejects duplicates and anything past [`MAX_METRICS_PER_PRODUCER`] (counting
/// `already` metrics the producer declared before). Each is accepted or rejected whole.
pub fn validate_set(defs: Vec<MetricDescriptor>, namespace: &str, already: usize) -> SetOutcome {
    let mut out = SetOutcome::default();
    for (i, mut d) in defs.into_iter().enumerate() {
        d.sanitize();
        let key = if d.id.is_empty() || d.id.len() > MAX_ID_LEN { format!("#{i}") } else { d.id.clone() };
        let res = d.validate(Some(namespace)).map_err(|e| e.to_string()).and_then(|()| {
            if out.accepted.iter().any(|a| a.id == d.id) {
                Err("duplicate id".into())
            } else if already + out.accepted.len() >= MAX_METRICS_PER_PRODUCER {
                Err(format!("more than {MAX_METRICS_PER_PRODUCER} metrics"))
            } else {
                Ok(())
            }
        });
        match res {
            Ok(()) => out.accepted.push(d),
            Err(e) => {
                out.rejected.insert(key, e);
            }
        }
    }
    out
}

/// A low-rate value in an `m` message: a number (gauge, counter), a string (state, text),
/// or a histogram.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum SampleValue {
    Number(f64),
    Text(String),
    Histogram(Histogram),
}

/// Fixed bucket edges and counts (`counts.len() == edges.len() - 1`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Histogram {
    pub edges: Vec<f64>,
    pub counts: Vec<u64>,
}

/// A sample that passed [`check_sample`].
#[derive(Clone, Debug, PartialEq)]
pub enum CheckedValue {
    Gauge(f64),
    Counter(u64),
    /// Index into the descriptor's `values`.
    State(usize),
    /// Sanitized, ≤ 48 bytes.
    Text(String),
    Histogram(Histogram),
}

/// Checks a JSON sample against its metric's descriptor (§3.5, §3.16): gauges finite,
/// counters non-negative integers, states one of the declared values, text sanitized and
/// capped, histograms well-formed. Ring kinds don't take JSON samples.
pub fn check_sample(desc: &MetricDescriptor, v: &SampleValue) -> Result<CheckedValue, &'static str> {
    match (desc.kind, v) {
        (MetricKind::Gauge, SampleValue::Number(x)) if x.is_finite() => Ok(CheckedValue::Gauge(*x)),
        (MetricKind::Counter, SampleValue::Number(x)) if x.is_finite() && *x >= 0.0 && x.fract() == 0.0 && *x <= u64::MAX as f64 => {
            Ok(CheckedValue::Counter(*x as u64))
        }
        (MetricKind::State, SampleValue::Text(s)) => desc.values.iter().position(|v| v.value == *s).map(CheckedValue::State).ok_or("not a declared state value"),
        (MetricKind::Text, SampleValue::Text(s)) => Ok(CheckedValue::Text(sanitize_text(s, MAX_TEXT_BYTES))),
        (MetricKind::Histogram, SampleValue::Histogram(h)) => {
            let n = h.edges.len();
            if n < 2 || n > MAX_HISTOGRAM_BUCKETS + 1 || h.counts.len() != n - 1 {
                return Err("histogram needs 2–65 edges and one count per bucket");
            }
            if !h.edges.iter().all(|e| e.is_finite()) || !h.edges.windows(2).all(|w| w[0] < w[1]) {
                return Err("histogram edges must be finite and increasing");
            }
            Ok(CheckedValue::Histogram(h.clone()))
        }
        (MetricKind::Gauge | MetricKind::Counter, SampleValue::Number(_)) => Err("not a finite / valid number"),
        (k, _) if k.uses_ring() => Err("ring kinds take no JSON samples"),
        _ => Err("wrong value type for the metric's kind"),
    }
}

/// The descriptor of the Sight layer's present record (`present.frame`, metric index
/// [`crate::layout::LAYER_METRIC_PRESENT`]): fills `frame.presented`.
pub fn layer_present_descriptor() -> MetricDescriptor {
    let mut d = MetricDescriptor::new("present.frame", MetricKind::Frame);
    d.label = Some("Presents".into());
    d.help = Some("One record per vkQueuePresentKHR per swapchain, from the Sight layer.".into());
    d.clock = Some(Clock::MonoRawNs);
    d.key = Some(KeyMeaning::PresentId);
    d.sample = Some(SampleRate::PerFrame);
    d.fields = crate::record::PRESENT_FIELD_NAMES.iter().map(|s| s.to_string()).collect();
    d.roles = alloc::vec![RoleClaim { role: "frame.presented".into(), flags: Vec::new(), on: Some("entry".into()) }];
    d.visibility = Some(Visibility::Debug);
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn parse(json: &str) -> MetricDescriptor {
        serde_json::from_str(json).unwrap()
    }

    /// The aco-dlss example from SIGHT-HUD §3.15, as JSON.
    fn aco_dlss() -> Vec<MetricDescriptor> {
        let defs = r#"[
          {"id": "aco-dlss.frame", "kind": "frame", "label": "DLSS frames", "clock": "qpc", "key": "present_seq",
           "fields": ["game_present", "present_call", "present_return"],
           "roles": [{"role": "frame.base", "flags": ["real", "nofg"], "on": "game_present"},
                     {"role": "frame.generated", "flags": ["generated"], "on": "present_call"}]},
          {"id": "aco-dlss.fg.state", "kind": "state", "label": "Frame Generation", "short": "FG",
           "values": [{"value": "off", "label": "off", "level": "dim"}, {"value": "on", "label": "on"},
                      {"value": "failed", "label": "failed", "level": "bad"}], "roles": ["fg.state"]},
          {"id": "aco-dlss.fg.dropped", "kind": "counter", "label": "Generated frames dropped",
           "help": "The generated frame wasn't ready in time; only the real frame was shown.", "better": "lower"},
          {"id": "aco-dlss.upscaler", "kind": "text", "label": "Upscaler", "roles": ["upscaler"]},
          {"id": "aco-dlss.res.internal", "kind": "text", "label": "Internal resolution", "roles": ["render.resolution"]},
          {"id": "aco-dlss.dlss.ms", "kind": "series", "unit": "ms", "label": "DLSS cost", "short": "DLSS",
           "format": {"precision": 1}, "better": "lower"},
          {"id": "aco-dlss.reflex.mode", "kind": "state", "label": "Reflex",
           "values": [{"value": "off", "label": "off", "level": "dim"}, {"value": "on", "label": "on"}, {"value": "boost", "label": "boost"}]},
          {"id": "aco-dlss.latency", "kind": "frame", "label": "Reflex latency", "clock": "reflex_us", "key": "frame_id",
           "fields": ["input", "sim_start", "sim_end", "submit_start", "submit_end", "present_start",
                      "present_end", "driver_end", "queue_end", "gpu_start", "gpu_end"],
           "roles": ["latency.frame"],
           "derive": [
             {"id": "render", "fn": "span", "from": "sim_start", "to": "gpu_end", "label": "Render latency",
              "short": "LAT", "unit": "ms", "thresholds": [{"above": 30, "level": "warn"}, {"above": 50, "level": "bad"}]},
             {"id": "breakdown", "fn": "stack", "label": "Latency breakdown",
              "segments": ["sim_start", "sim_end", "submit_end", "present_end", "gpu_end"]}]},
          {"id": "aco-dlss.bench", "kind": "event", "label": "Benchmark markers", "sample": "per-event"},
          {"id": "aco-dlss.gpu.ms", "kind": "gauge", "unit": "ms", "range": {"min": 0, "max": 50}, "sample": 2,
           "level": "advanced", "display": "bar", "group": "DLSS", "order": 3}
        ]"#;
        serde_json::from_str(defs).unwrap()
    }

    #[test]
    fn worked_example_validates() {
        let defs = aco_dlss();
        let out = validate_set(defs.clone(), "aco-dlss", 0);
        assert!(out.rejected.is_empty(), "{:?}", out.rejected);
        assert_eq!(out.accepted.len(), defs.len());
        let lat = &out.accepted[7];
        assert_eq!(lat.derived_ids().collect::<Vec<_>>(), ["aco-dlss.latency.render", "aco-dlss.latency.breakdown"]);
        assert_eq!(lat.field_index("gpu_end"), Some(10));
        assert_eq!(out.accepted[9].visibility, Some(Visibility::Advanced));
        assert_eq!(out.accepted[9].sample, Some(SampleRate::Hz(2.0)));
        // Round trip through JSON is lossless.
        for d in &out.accepted {
            let j = serde_json::to_string(d).unwrap();
            assert_eq!(&serde_json::from_str::<MetricDescriptor>(&j).unwrap(), d, "{j}");
        }
        assert!(layer_present_descriptor().validate(Some("present")).is_ok());
    }

    #[test]
    fn namespace_and_ids() {
        let d = MetricDescriptor::new("other.frame", MetricKind::Frame);
        assert!(d.validate(Some("aco-dlss")).is_err());
        assert!(MetricDescriptor::new("aco-dlssx.frame", MetricKind::Frame).validate(Some("aco-dlss")).is_err());
        assert!(MetricDescriptor::new("aco-dlss.frame", MetricKind::Frame).validate(Some("aco-dlss")).is_ok());
        for bad in ["fps", "a.b.c.d.e.f", "A.b", "a..b", "a.b ", "a.b/c", "", &format!("a.{}", "b".repeat(63))] {
            assert!(validate_metric_id(bad).is_err(), "{bad}");
        }
        assert_eq!(qualify("aco-dlss", "fg.state"), "aco-dlss.fg.state");
        // A TOML-style def without an id is rejected until qualified.
        let d: MetricDescriptor = parse(r#"{"kind": "text"}"#);
        assert_eq!(validate_set(vec![d.clone()], "m", 0).rejected.keys().collect::<Vec<_>>(), ["#0"]);
        let d = MetricDescriptor { id: qualify("m", "upscaler"), ..d };
        assert!(validate_set(vec![d], "m", 0).rejected.is_empty());
    }

    #[test]
    fn rejections() {
        let cases = [
            (r#"{"id": "m.x", "kind": "gauge", "unit": "furlongs"}"#, true),
            (r#"{"id": "m.x", "kind": "nope"}"#, true),
            (r#"{"id": "m.x", "kind": "gauge", "range": "big"}"#, true),
        ];
        for (j, parse_fails) in cases {
            assert_eq!(serde_json::from_str::<MetricDescriptor>(j).is_err(), parse_fails, "{j}");
        }
        let bad = [
            r#"{"id": "m.x", "kind": "gauge", "label": "this label is much longer than allowed"}"#,
            r#"{"id": "m.x", "kind": "gauge", "short": "TOOLONG"}"#,
            r#"{"id": "m.x", "kind": "gauge", "label": ""}"#,
            r#"{"id": "m.x", "kind": "gauge", "format": {"precision": 9}}"#,
            r#"{"id": "m.x", "kind": "gauge", "range": {"min": 5, "max": 1}}"#,
            r#"{"id": "m.x", "kind": "gauge", "thresholds": [{"above": 1, "below": 2, "level": "bad"}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "thresholds": [{"level": "bad"}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "thresholds": [{"above": 1, "level": "accent"}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "sample": 0}"#,
            r#"{"id": "m.x", "kind": "gauge", "values": [{"value": "a"}]}"#,
            r#"{"id": "m.x", "kind": "state"}"#,
            r#"{"id": "m.x", "kind": "state", "values": [{"value": "a"}, {"value": "a"}]}"#,
            r#"{"id": "m.x", "kind": "state", "values": [{"value": ""}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "fields": ["a"]}"#,
            r#"{"id": "m.x", "kind": "frame", "fields": ["a", "a"]}"#,
            r#"{"id": "m.x", "kind": "frame", "fields": ["Bad"]}"#,
            r#"{"id": "m.x", "kind": "frame", "fields": ["a","b","c","d","e","f","g","h","i","j","k","l","m"]}"#,
            r#"{"id": "m.x", "kind": "gauge", "clock": "qpc"}"#,
            r#"{"id": "m.x", "kind": "gauge", "key": "frame_id"}"#,
            r#"{"id": "m.x", "kind": "gauge", "roles": ["frame.base"]}"#,
            r#"{"id": "m.x", "kind": "frame", "roles": [{"role": "frame.base", "on": "nope"}]}"#,
            r#"{"id": "m.x", "kind": "text", "roles": [{"role": "upscaler", "flags": ["real"]}]}"#,
            r#"{"id": "m.x", "kind": "frame", "roles": ["Bad Role"]}"#,
            r#"{"id": "m.x", "kind": "frame", "fields": ["a"], "derive": [{"id": "s", "fn": "span", "from": "a", "to": "b"}]}"#,
            r#"{"id": "m.x", "kind": "frame", "fields": ["a", "b"], "derive": [{"id": "s", "fn": "stack", "segments": ["a"]}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "derive": [{"id": "r", "fn": "rate"}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "derive": [{"id": "r", "fn": "ratio", "of": ["fps"]}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "derive": [{"id": "r"}]}"#,
            r#"{"id": "m.x", "kind": "counter", "derive": [{"id": "r", "fn": "rate"}, {"id": "r", "fn": "rate"}]}"#,
            r#"{"id": "m.x", "kind": "counter", "derive": [{"id": "R", "fn": "rate"}]}"#,
            r#"{"id": "m.x", "kind": "gauge", "renamed_from": ["old"]}"#,
        ];
        for j in bad {
            let d = parse(j);
            assert!(d.validate(Some("m")).is_err(), "{j}");
        }
        // Unknown roles are accepted (bind nothing); unknown fields ignored.
        let d = parse(r#"{"id": "m.x", "kind": "frame", "roles": ["future.role"], "colour": "hotpink"}"#);
        assert!(d.validate(Some("m")).is_ok());
        assert_eq!(role_kind("future.role"), None);
    }

    #[test]
    fn set_limits() {
        let defs: Vec<_> = (0..70).map(|i| MetricDescriptor::new(format!("m.g{i}"), MetricKind::Gauge)).collect();
        let out = validate_set(defs, "m", 0);
        assert_eq!(out.accepted.len(), MAX_METRICS_PER_PRODUCER);
        assert_eq!(out.rejected.len(), 6);
        let out = validate_set(vec![MetricDescriptor::new("m.a", MetricKind::Gauge)], "m", 64);
        assert_eq!(out.rejected.len(), 1);
        let out = validate_set(vec![MetricDescriptor::new("m.a", MetricKind::Gauge), MetricDescriptor::new("m.a", MetricKind::Text)], "m", 0);
        assert_eq!((out.accepted.len(), out.rejected["m.a"].as_str()), (1, "duplicate id"));
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize_text("ok \u{202E}evil\u{2066} \x07bell\n\tnext", 100), "ok evil bell next");
        assert_eq!(sanitize_text("  lead and trail  ", 100), "lead and trail");
        assert_eq!(sanitize_text("ééééé", 5), "éé");
        assert_eq!(sanitize_bytes(b"fine\xff\xfe", 48), "fine\u{FFFD}\u{FFFD}");
        // Property: output is always within the cap, free of controls and bidi overrides.
        for _ in 0..2000 {
            let s: String = (0..fastrand::usize(0..80))
                .map(|_| char::from_u32(fastrand::u32(0..0x3000)).unwrap_or('x'))
                .collect();
            let cap = fastrand::usize(0..60);
            let out = sanitize_text(&s, cap);
            assert!(out.len() <= cap);
            assert!(!out.chars().any(|c| c.is_control() || is_bidi_control(c)));
        }
        let mut d = parse("{\"id\": \"m.x\", \"kind\": \"gauge\", \"label\": \"GPU\\u202e temp\"}");
        d.sanitize();
        assert_eq!(d.label.as_deref(), Some("GPU temp"));
    }

    #[test]
    fn samples() {
        let defs = aco_dlss();
        let fg = &defs[1];
        let n = |x: f64| SampleValue::Number(x);
        let t = |s: &str| SampleValue::Text(s.into());
        assert_eq!(check_sample(fg, &t("on")), Ok(CheckedValue::State(1)));
        assert!(check_sample(fg, &t("maybe")).is_err());
        assert!(check_sample(fg, &n(1.0)).is_err());
        let gauge = &defs[9];
        assert_eq!(check_sample(gauge, &n(1.5)), Ok(CheckedValue::Gauge(1.5)));
        assert!(check_sample(gauge, &n(f64::NAN)).is_err());
        let counter = &defs[2];
        assert_eq!(check_sample(counter, &n(3.0)), Ok(CheckedValue::Counter(3)));
        assert!(check_sample(counter, &n(-1.0)).is_err());
        assert!(check_sample(counter, &n(1.5)).is_err());
        let text = &defs[3];
        let long = "DLSS K \u{202E}".to_string() + &"x".repeat(60);
        match check_sample(text, &t(&long)).unwrap() {
            CheckedValue::Text(s) => assert!(s.len() <= 48 && s.starts_with("DLSS K x")),
            v => panic!("{v:?}"),
        }
        assert!(check_sample(&defs[0], &n(1.0)).is_err());
        let h = MetricDescriptor::new("m.h", MetricKind::Histogram);
        let hv: SampleValue = serde_json::from_str(r#"{"edges": [0, 1, 2], "counts": [5, 6]}"#).unwrap();
        assert!(check_sample(&h, &hv).is_ok());
        let hv: SampleValue = serde_json::from_str(r#"{"edges": [0, 2, 1], "counts": [5, 6]}"#).unwrap();
        assert!(check_sample(&h, &hv).is_err());
        assert_eq!(MetricKind::from_sdk_code(MetricKind::Event.sdk_code().unwrap()), Some(MetricKind::Event));
        assert_eq!(MetricKind::Histogram.sdk_code(), None);
    }
}
