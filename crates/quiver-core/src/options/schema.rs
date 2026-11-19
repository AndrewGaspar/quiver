//! Reading and checking `quiver-options.toml` (design/MOD-CONFIG.md §1).
//!
//! [`parse`] turns the text into a [`Schema`] (unknown fields fail loudly, except under
//! `x-*` keys; a newer `format` is refused; `[readout.x]` becomes `[metric.x] kind = "text"`;
//! metric ids get their surface namespace). [`lint`] checks it on its own: types and their
//! fields, ranges, pages, `when`, presets, bindings, metric descriptors (SIGHT-HUD §3.3).
//! [`lint_files`] checks it against the files a version ships.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use quiver_api::options::{
    Apply, FORMAT, FileFormat, FileOwner, FileRoot, FormatSpec, LintFinding, MetricDecl, MetricKind, OptionDef, OptionType, Reload, Schema, Severity,
    WhenCond, scalar_text,
};

use crate::confpatch::Ini;

/// Parse a schema (TOML, or JSON with the same fields).
pub fn parse(text: &str, json: bool) -> Result<Schema> {
    parse_noted(text, json).map(|(s, _)| s)
}

/// [`parse`], plus a warning for each default written as the wrong type that was converted
/// (`default = "1"` on a bool option becomes `true`). Every reader then sees the typed value:
/// the app's switch used to show a text "1" as off while the resolver used it as on.
pub fn parse_noted(text: &str, json: bool) -> Result<(Schema, Vec<LintFinding>)> {
    let mut schema = parse_raw_schema(text, json)?;
    let notes = normalize_defaults(&mut schema);
    Ok((schema, notes))
}

/// Convert defaults to their option's type where they parse as it (`"1"`, `"true"`, `"on"` for
/// a bool; `"60"` for a number). A default that doesn't parse is left as written, for `lint`.
pub fn normalize_defaults(schema: &mut Schema) -> Vec<LintFinding> {
    let mut notes = Vec::new();
    for (id, d) in schema.options.iter_mut() {
        let Some(v) = &d.default else { continue };
        if d.special_of(v).is_some() {
            continue;
        }
        if let Ok(typed) = d.normalize(v)
            && typed != *v
        {
            notes.push(LintFinding {
                severity: Severity::Warning,
                at: format!("option {id}"),
                message: format!("default {v} is {}; write {typed} (Quiver reads it as {typed})", json_kind(v)),
            });
            d.default = Some(typed);
        }
    }
    notes
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "text",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        Value::Array(_) => "a list",
        Value::Object(_) => "a table",
        Value::Null => "nothing",
    }
}

fn parse_raw_schema(text: &str, json: bool) -> Result<Schema> {
    let mut value: Value = if json {
        serde_json::from_str(text).context("not valid JSON")?
    } else {
        let t: toml::Value = toml::from_str(text).map_err(|e| anyhow::anyhow!("not valid TOML: {}", e.to_string().trim()))?;
        serde_json::to_value(t)?
    };
    let format = value.get("format").and_then(Value::as_u64);
    match format {
        None => bail!("`format = {FORMAT}` is missing (the schema format version)"),
        Some(f) if f > FORMAT as u64 => bail!("schema format {f} is newer than this Quiver understands (format {FORMAT}); update Quiver"),
        Some(0) => bail!("format 0 doesn't exist; use format = {FORMAT}"),
        _ => {}
    }
    strip_experiments(&mut value);
    // Deserialize from the TOML text when it has no experiments, for line numbers.
    let mut schema: Schema = if !json && !has_experiments(text) {
        toml::from_str(text).map_err(|e| anyhow::anyhow!("{}", e.to_string().trim()))?
    } else {
        serde_json::from_value(value).map_err(|e| anyhow::anyhow!("{e}"))?
    };
    for (name, mut r) in std::mem::take(&mut schema.readouts) {
        r.kind = MetricKind::Text;
        schema.metrics.entry(name).or_insert(r);
    }
    let surface = schema.surface.clone();
    for (name, m) in schema.metrics.iter_mut() {
        m.id = format!("{surface}.{name}");
    }
    Ok(schema)
}

fn has_experiments(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start().trim_start_matches('[').trim_start_matches('"');
        t.starts_with("x-")
    }) || text.contains(".x-") || text.contains("\"x-")
}

/// Drop every `x-*` key (reserved for experiments), at any depth.
fn strip_experiments(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| !k.starts_with("x-"));
            m.values_mut().for_each(strip_experiments);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_experiments),
        _ => {}
    }
}

/// Where an option lives in an INI file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binding {
    pub file: String,
    pub section: String,
    pub key: String,
}

impl Binding {
    /// `file:section/key`, as raw values are stored.
    pub fn raw_key(&self) -> String {
        format!("{}:{}/{}", self.file, self.section, self.key)
    }

    /// Parse `file:section/key` or `section/key` (in `default_file`).
    pub fn parse_raw(s: &str, default_file: Option<&str>) -> Result<Binding> {
        let (file, rest) = match s.split_once(':') {
            Some((f, r)) => (f.to_owned(), r),
            None => (default_file.context("name the file: file:section/key")?.to_owned(), s),
        };
        let (section, key) = rest.rsplit_once('/').unwrap_or(("", rest));
        if key.trim().is_empty() {
            bail!("{s}: give section/key");
        }
        Ok(Binding { file, section: section.to_owned(), key: key.to_owned() })
    }
}

/// The file an option is in: its own `file`, else the schema's only file.
pub fn option_file(schema: &Schema, def: &OptionDef) -> Option<String> {
    match &def.file {
        Some(f) => Some(f.clone()),
        None if schema.files.len() == 1 => schema.files.keys().next().cloned(),
        None => None,
    }
}

/// Where an option is bound (INI files: `section.key` from the id unless given).
pub fn binding(schema: &Schema, id: &str, def: &OptionDef) -> Result<Binding> {
    let file = option_file(schema, def).with_context(|| format!("option {id}: name its `file` (the schema has {} files)", schema.files.len()))?;
    let (section, key) = match (&def.section, &def.key) {
        (Some(s), Some(k)) => (s.clone(), k.clone()),
        (Some(s), None) => (s.clone(), id.rsplit('.').next().unwrap_or(id).to_owned()),
        (None, Some(k)) => (id.rsplit_once('.').map(|(s, _)| s.to_owned()).unwrap_or_default(), k.clone()),
        (None, None) => match id.split_once('.') {
            Some((s, k)) => (s.to_owned(), k.to_owned()),
            None => (String::new(), id.to_owned()),
        },
    };
    Ok(Binding { file, section, key })
}

/// Sentences in a help text: boundaries are ". ", "! " or "? " followed by a capital
/// ("e.g. 160" and "0.5" don't end one).
fn sentences(s: &str) -> usize {
    let b = s.as_bytes();
    1 + (0..b.len().saturating_sub(2)).filter(|&i| matches!(b[i], b'.' | b'!' | b'?') && b[i + 1] == b' ' && b[i + 2].is_ascii_uppercase()).count()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// Fields that only make sense for some types: (field, set?, types that take it).
fn type_fields(d: &OptionDef) -> Vec<(&'static str, bool, &'static [OptionType])> {
    use OptionType::*;
    vec![
        ("values", !d.values.is_empty(), &[Enum]),
        ("open", d.open, &[Enum]),
        ("min", d.min.is_some(), &[Int, Float]),
        ("max", d.max.is_some(), &[Int, Float]),
        ("step", d.step.is_some(), &[Int, Float]),
        ("unit", d.unit.is_some(), &[Int, Float]),
        ("scale", d.scale.is_some(), &[Int, Float]),
        ("marks", !d.marks.is_empty(), &[Int, Float]),
        ("precision", d.precision.is_some(), &[Float]),
        ("pattern", d.pattern.is_some(), &[String]),
        ("max_len", d.max_len.is_some(), &[String]),
        ("multiline", d.multiline, &[String]),
        ("relative_to", d.relative_to.is_some(), &[Path]),
        ("style", d.style.is_some(), &[Path]),
        ("filter", d.filter.is_some(), &[Path]),
        ("allowed", d.allowed.is_some(), &[Key]),
        ("none", d.none.is_some(), &[Key]),
        ("item", d.item.is_some(), &[List]),
        ("sep", d.sep.is_some(), &[List]),
        ("min_items", d.min_items.is_some(), &[List]),
        ("max_items", d.max_items.is_some(), &[List]),
        ("format", d.format.is_some(), &[Bool, Key, Color]),
    ]
}

fn type_name(t: OptionType) -> String {
    serde_json::to_value(t).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

fn lint_def(id: &str, d: &OptionDef, out: &mut Vec<LintFinding>, at: &str) {
    let mut push = |severity, message: String| out.push(LintFinding { severity, at: at.to_owned(), message });
    for (field, set, types) in type_fields(d) {
        if set && !types.contains(&d.ty) {
            let names: Vec<String> = types.iter().map(|t| type_name(*t)).collect();
            push(Severity::Error, format!("`{field}` is for {} options, not {}", names.join("/"), type_name(d.ty)));
        }
    }
    if let Some(l) = &d.label
        && l.chars().count() > 40
    {
        push(Severity::Warning, format!("label is {} characters; keep it to 40 so it fits the in-game panel", l.chars().count()));
    }
    if let Some(h) = &d.help
        && sentences(h) > 2
    {
        push(Severity::Warning, "help has more than two sentences; keep it short".into());
    }
    if d.label.is_none() && !id.is_empty() {
        push(Severity::Note, "no label (the id is shown)".into());
    }
    match (d.apply, &d.at) {
        (Apply::At, None) => push(Severity::Error, "`apply = \"at\"` needs `at` (the moment it's read)".into()),
        (a, Some(_)) if a != Apply::At => push(Severity::Warning, "`at` is only used with apply = \"at\"".into()),
        _ => {}
    }
    if d.latency.is_some() && d.apply != Apply::Live {
        push(Severity::Warning, "`latency` is only used with apply = \"live\"".into());
    }
    match d.ty {
        OptionType::Enum => {
            if d.values.is_empty() && !d.open {
                push(Severity::Error, "an enum needs `values` (or `open = true`)".into());
            }
            let mut seen = BTreeSet::new();
            for v in &d.values {
                if !seen.insert(scalar_text(&v.value).to_lowercase()) {
                    push(Severity::Error, format!("enum value {} is listed twice", scalar_text(&v.value)));
                }
                if v.value.is_object() || v.value.is_array() {
                    push(Severity::Error, format!("enum value {} must be text or a number", v.value));
                }
            }
        }
        OptionType::Int | OptionType::Float => {
            if let (Some(a), Some(b)) = (d.min, d.max)
                && a > b
            {
                push(Severity::Error, format!("min {a} is above max {b}"));
            }
            if d.step.is_some_and(|s| s <= 0.0) {
                push(Severity::Error, "step must be positive".into());
            }
            if let Some(s) = &d.scale
                && s != "log"
            {
                push(Severity::Error, format!("scale {s:?}: only \"log\" is known"));
            }
            for m in &d.marks {
                if let Err(e) = d.validate(&m.value) {
                    push(Severity::Warning, format!("mark {}: {e}", scalar_text(&m.value)));
                }
            }
            for s in &d.special {
                if !(s.value.is_string() || s.value.is_number()) {
                    push(Severity::Error, format!("special value {} must be a number or text", s.value));
                }
            }
        }
        OptionType::Key => {
            if let Some(FormatSpec::Name(f)) = &d.format
                && !quiver_api::options::KEY_FORMATS.contains(&f.as_str())
            {
                push(Severity::Error, format!("key format {f:?}: one of {}", quiver_api::options::KEY_FORMATS.join(", ")));
            }
        }
        OptionType::Bool => match &d.format {
            Some(FormatSpec::Name(f)) if !quiver_api::options::BOOL_FORMATS.contains(&f.as_str()) => {
                push(Severity::Error, format!("bool format {f:?}: one of {} or {{ on, off }}", quiver_api::options::BOOL_FORMATS.join(", ")))
            }
            Some(FormatSpec::Pair { on, off }) if on.eq_ignore_ascii_case(off) => push(Severity::Error, "bool format: on and off are the same".into()),
            _ => {}
        },
        OptionType::Color => {
            if let Some(FormatSpec::Name(f)) = &d.format
                && !quiver_api::options::COLOR_FORMATS.contains(&f.as_str())
            {
                push(Severity::Error, format!("color format {f:?}: one of {}", quiver_api::options::COLOR_FORMATS.join(", ")));
            }
        }
        OptionType::String => {
            if let Some(p) = &d.pattern
                && let Err(e) = regex::Regex::new(p)
            {
                push(Severity::Error, format!("pattern doesn't compile: {e}"));
            }
        }
        OptionType::List => match &d.item {
            None => push(Severity::Error, "a list needs `item = { type = … }`".into()),
            Some(item) if item.ty == OptionType::List => push(Severity::Error, "a list's items can't be lists".into()),
            Some(item) => {
                let mut inner = Vec::new();
                lint_def("", item, &mut inner, at);
                for f in inner {
                    out.push(LintFinding { message: format!("item: {}", f.message), ..f });
                }
            }
        },
        OptionType::Path => {}
    }
    if let Some(v) = &d.default
        && let Err(e) = d.normalize(v)
    {
        out.push(LintFinding { severity: Severity::Error, at: at.to_owned(), message: format!("default {}: {e}", scalar_text(v)) });
    }
}

fn lint_when(schema: &Schema, w: &BTreeMap<String, WhenCond>, at: &str, out: &mut Vec<LintFinding>) {
    for (dep, c) in w {
        let Some(d) = schema.options.get(dep) else {
            out.push(LintFinding { severity: Severity::Error, at: at.to_owned(), message: format!("`when` names {dep}, which isn't an option") });
            continue;
        };
        let values: Vec<&Value> = match c {
            WhenCond::One(v) => vec![v],
            WhenCond::Any(l) => l.iter().collect(),
            WhenCond::Range { .. } => {
                if !matches!(d.ty, OptionType::Int | OptionType::Float) {
                    out.push(LintFinding { severity: Severity::Error, at: at.to_owned(), message: format!("`when` gives {dep} a range, but it isn't a number") });
                }
                vec![]
            }
        };
        for v in values {
            if d.normalize(v).is_err() {
                out.push(LintFinding { severity: Severity::Warning, at: at.to_owned(), message: format!("`when`: {dep} can never be {}", scalar_text(v)) });
            }
        }
    }
}

const METRIC_UNITS: &[&str] = &["ms", "us", "ns", "s", "fps", "Hz", "%", "C", "W", "MHz", "B", "B/s", "x", "px", "count", ""];
const METRIC_LEVELS: &[&str] = &["good", "warn", "bad", "dim"];

/// Check a metric descriptor against SIGHT-HUD §3.3.
pub fn lint_metric(m: &MetricDecl, out: &mut Vec<LintFinding>) {
    let at = format!("metric {}", m.id);
    let mut push = |severity, message: String| out.push(LintFinding { severity, at: at.clone(), message });
    let segs: Vec<&str> = m.id.split('.').collect();
    if !(2..=5).contains(&segs.len()) || m.id.len() > 64 || !segs.iter().all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')) {
        push(Severity::Error, "ids are 2–5 segments of [a-z0-9_-] (with the surface), at most 64 characters".into());
    }
    if let Some(u) = &m.unit
        && !METRIC_UNITS.contains(&u.as_str())
    {
        push(Severity::Error, format!("unit {u:?}: one of {}", METRIC_UNITS.join(" ")));
    }
    for (field, v, max) in [("label", &m.label, 24), ("short", &m.short, 6), ("help", &m.help, 200)] {
        if let Some(s) = v
            && s.chars().count() > max
        {
            push(Severity::Error, format!("{field} is longer than {max} characters"));
        }
    }
    if m.kind == MetricKind::State && m.values.is_empty() {
        push(Severity::Error, "a state metric lists its `values`".into());
    }
    if m.kind != MetricKind::State && !m.values.is_empty() {
        push(Severity::Error, "`values` is for state metrics".into());
    }
    if m.values.len() > 16 {
        push(Severity::Error, "at most 16 values".into());
    }
    for v in &m.values {
        if let Some(l) = &v.level
            && !METRIC_LEVELS.contains(&l.as_str())
        {
            push(Severity::Error, format!("value {}: level {l:?} is one of {}", v.value, METRIC_LEVELS.join(", ")));
        }
    }
    for t in &m.thresholds {
        if !METRIC_LEVELS.contains(&t.level.as_str()) {
            push(Severity::Error, format!("threshold level {:?} is one of {}", t.level, METRIC_LEVELS.join(", ")));
        }
        if t.above.is_some() == t.below.is_some() {
            push(Severity::Error, "a threshold gives `above` or `below`".into());
        }
    }
    if m.kind == MetricKind::Frame && m.fields.is_empty() {
        push(Severity::Warning, "a frame metric names its `fields`".into());
    }
    if m.fields.len() > 12 {
        push(Severity::Error, "at most 12 fields".into());
    }
    if !m.fields.is_empty() && m.kind != MetricKind::Frame {
        push(Severity::Error, "`fields` is for frame metrics".into());
    }
    if let Some(c) = &m.clock
        && !["qpc", "mono_raw_ns", "reflex_us"].contains(&c.as_str())
    {
        push(Severity::Error, format!("clock {c:?}: qpc, mono_raw_ns or reflex_us"));
    }
    if let Some(b) = &m.better
        && !["higher", "lower", "none"].contains(&b.as_str())
    {
        push(Severity::Error, format!("better {b:?}: higher, lower or none"));
    }
    if let Some(d) = &m.display
        && !["number", "badge", "bar", "sparkline", "graph", "stack"].contains(&d.as_str())
    {
        push(Severity::Error, format!("display {d:?}: number, badge, bar, sparkline, graph or stack"));
    }
    for r in &m.roles {
        if !(r.is_string() || r.get("role").is_some_and(Value::is_string)) {
            push(Severity::Error, format!("role {r}: a role name, or {{ role, … }}"));
        }
    }
}

/// Check a schema on its own. Errors make it unusable; warnings and notes are advice.
pub fn lint(schema: &Schema) -> Vec<LintFinding> {
    let mut out = Vec::new();
    let mut top = |severity, at: &str, message: String| out.push(LintFinding { severity, at: at.to_owned(), message });
    if schema.surface.is_empty() || !schema.surface.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        top(Severity::Error, "surface", format!("{:?}: a stable id of lowercase letters, digits, - and _", schema.surface));
    }
    let platform = schema.kind == Some(quiver_api::options::SurfaceKind::Platform);
    if schema.files.is_empty() && !schema.options.is_empty() && !platform {
        top(Severity::Error, "file", "the schema binds no files (`[file.<id>]`)".into());
    }
    for (fid, f) in &schema.files {
        let at = format!("file {fid}");
        for p in std::iter::once(&f.path).chain(&f.formerly) {
            if p.is_empty() || p.starts_with('/') || p.contains('\\') || p.split('/').any(|c| c.is_empty() || c == ".." || c == ".") {
                top(Severity::Error, &at, format!("path {p:?}: relative to its root, with '/' and no '..'"));
            }
        }
        if !f.formerly.is_empty() && f.root != FileRoot::Install {
            top(Severity::Warning, &at, "`formerly` is only used for install files".into());
        }
        if f.format != FileFormat::Ini {
            top(Severity::Warning, &at, format!("the {:?} adapter isn't built yet (phase 2); values for it are kept but not written", f.format));
        }
        if f.reload != Reload::None && f.owner != FileOwner::Quiver {
            top(Severity::Warning, &at, "`reload` is only used for files Quiver renders (owner = \"quiver\")".into());
        }
        if f.owner == FileOwner::Quiver && f.root != FileRoot::Install {
            top(Severity::Warning, &at, "owner = \"quiver\" files outside the install are patched, not rendered".into());
        }
    }
    let mut bound: BTreeMap<Binding, String> = BTreeMap::new();
    for (id, d) in &schema.options {
        let at = format!("option {id}");
        if !valid_id(id) {
            out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: "ids are dotted names of letters, digits, _ and -".into() });
        }
        lint_def(id, d, &mut out, &at);
        if let Some(f) = &d.file
            && !schema.files.contains_key(f)
        {
            out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("file {f} isn't declared") });
        }
        match binding(schema, id, d) {
            _ if platform => {}
            Ok(b) => {
                if let Some(other) = bound.insert(b.clone(), id.clone()) {
                    out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("bound to the same key as {other} ({})", b.raw_key()) });
                }
            }
            Err(e) => out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("{e:#}") }),
        }
        if let Some(w) = &d.when {
            lint_when(schema, w, &at, &mut out);
            if w.contains_key(id) {
                out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: "`when` names the option itself".into() });
            }
        }
        for w in &d.when_any {
            lint_when(schema, w, &at, &mut out);
        }
        for old in &d.renamed_from {
            if schema.options.contains_key(old) {
                out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("renamed_from {old}, which is still an option") });
            }
        }
    }
    let mut placed: BTreeMap<&str, &str> = BTreeMap::new();
    let mut pages = BTreeSet::new();
    for p in &schema.pages {
        let at = format!("page {}", p.id);
        if !pages.insert(p.id.as_str()) {
            out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: "page id used twice".into() });
        }
        for id in p.options.iter().chain(p.groups.iter().flat_map(|g| g.options.iter())) {
            if !schema.options.contains_key(id) {
                out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("lists {id}, which isn't an option") });
            } else if let Some(other) = placed.insert(id, &p.id) {
                out.push(LintFinding { severity: Severity::Warning, at: at.clone(), message: format!("{id} is also on page {other}") });
            }
        }
    }
    if !schema.pages.is_empty() {
        let other = schema.options.keys().filter(|id| !placed.contains_key(id.as_str())).count();
        if other > 0 {
            out.push(LintFinding { severity: Severity::Note, at: "page".into(), message: format!("{other} option(s) on no page land on \"Other\"") });
        }
    }
    let mut presets = BTreeSet::new();
    for p in &schema.presets {
        let at = format!("preset {}", p.id);
        if !presets.insert(p.id.as_str()) {
            out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: "preset id used twice".into() });
        }
        if p.set.is_empty() {
            out.push(LintFinding { severity: Severity::Warning, at: at.clone(), message: "sets nothing".into() });
        }
        for (id, v) in &p.set {
            match schema.options.get(id) {
                None => out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("sets {id}, which isn't an option") }),
                Some(d) => {
                    if let Err(e) = d.normalize(v) {
                        out.push(LintFinding { severity: Severity::Error, at: at.clone(), message: format!("{id} = {}: {e}", scalar_text(v)) });
                    }
                }
            }
        }
    }
    for (aid, a) in &schema.actions {
        if let Some(k) = &a.hotkey_option {
            match schema.options.get(k) {
                Some(d) if d.ty == OptionType::Key => {}
                Some(_) => out.push(LintFinding { severity: Severity::Error, at: format!("action {aid}"), message: format!("hotkey_option {k} isn't a key option") }),
                None => out.push(LintFinding { severity: Severity::Error, at: format!("action {aid}"), message: format!("hotkey_option {k} isn't an option") }),
            }
        }
    }
    for m in schema.metrics.values() {
        lint_metric(m, &mut out);
    }
    lint_tweaks(schema, platform, &mut out);
    if schema.matches.is_some() && schema.audited.is_none() {
        out.push(LintFinding { severity: Severity::Warning, at: "match".into(), message: "a catalog schema says who audited it (`audited`)".into() });
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    out
}

/// Quiver Tweaks fields (design/TWEAKS.md §3.2, §7.1): `via` bindings on platform surfaces,
/// `provides`/`conflicts`/`tweak_page` on mod and game surfaces.
fn lint_tweaks(schema: &Schema, platform: bool, out: &mut Vec<LintFinding>) {
    use crate::tweaks::compile::{BUILT, KNOWN, PHASE};
    let mut f = |severity, at: String, message: String| out.push(LintFinding { severity, at, message });
    for (id, d) in &schema.options {
        let at = format!("option {id}");
        if !d.via.is_empty() && !platform {
            f(Severity::Warning, at.clone(), "`via` is only read on platform surfaces (kind = \"platform\")".into());
        }
        for (n, b) in d.via.iter().enumerate() {
            let at = format!("{at} via[{n}]");
            if !KNOWN.contains(&b.mech.as_str()) {
                f(Severity::Error, at.clone(), format!("unknown mechanism {:?} (known: {})", b.mech, KNOWN.join(", ")));
            } else if b.phase.unwrap_or(1) > PHASE || !BUILT.contains(&b.mech.as_str()) {
                f(Severity::Note, at.clone(), format!("mechanism {} (phase {}) isn't built in this Quiver: ignored", b.mech, b.phase.unwrap_or(2)));
            }
            if b.mech == "mod" {
                f(Severity::Error, at.clone(), "providers declare `[provides]` in their own schema; `mech = \"mod\"` isn't a binding".into());
            }
            for name in b.requires.has.iter().chain(&b.requires.lacks) {
                if !matches!(name.split_once(':').map(|x| x.0), Some("dxvk" | "nvapi" | "vkd3d" | "proton")) {
                    f(Severity::Error, at.clone(), format!("`has` names are component:name (dxvk:, nvapi:, vkd3d:, proton:), got {name:?}"));
                }
            }
            if b.set.is_empty() && b.map.values().all(|v| matches!(v, quiver_api::options::ViaValue::Text(_))) && BUILT.contains(&b.mech.as_str()) && b.phase.unwrap_or(1) <= PHASE && b.flags.is_empty() {
                f(Severity::Error, at.clone(), "the binding writes nothing (no `set`, no per-value tables)".into());
            }
            for (k, v) in &b.map {
                if let quiver_api::options::ViaValue::Text(t) = v
                    && t.starts_with('@')
                    && t != "@unset"
                    && t != "@skip"
                {
                    f(Severity::Error, at.clone(), format!("map {k} = {t}: only @unset and @skip are known"));
                }
                if d.ty == OptionType::Enum && !d.open && !d.values.iter().any(|e| scalar_text(&e.value).eq_ignore_ascii_case(k)) {
                    f(Severity::Warning, at.clone(), format!("map key {k} isn't one of the tweak's values"));
                }
            }
        }
        if let Some(p) = &d.tweak_page
            && !crate::tweaks::schema().pages.iter().any(|x| &x.id == p)
        {
            f(Severity::Warning, at.clone(), format!("tweak_page {p} isn't a Quiver Tweaks page"));
        }
    }
    let tweaks = if platform { schema } else { crate::tweaks::schema() };
    let mut whens = Vec::new();
    for (tweak, p) in &schema.provides {
        let at = format!("provides {tweak}");
        if !tweaks.options.contains_key(tweak) {
            f(Severity::Warning, at.clone(), format!("{tweak} isn't a tweak this Quiver knows (fine if a newer one adds it)"));
        }
        match schema.options.get(&p.option) {
            None => f(Severity::Error, at.clone(), format!("option {} isn't an option of this surface", p.option)),
            Some(def) => {
                let off = p.off.iter().map(|o| ("off".to_owned(), o.clone()));
                for (k, v) in p.map.iter().map(|(k, v)| (k.clone(), v.clone())).chain(off) {
                    let skip = v.as_str().is_some_and(|s| s == "@keep" || s.starts_with('{'));
                    if !skip && let Err(e) = def.normalize(&v) {
                        f(Severity::Error, at.clone(), format!("{k} → {}: {e}", scalar_text(&v)));
                    }
                }
            }
        }
        if let Some(w) = &p.when {
            whens.push((w, at.clone()));
        }
    }
    for (n, c) in schema.conflicts.iter().enumerate() {
        let at = format!("conflicts[{n}]");
        match (&c.tweak, &c.mech) {
            (None, None) => f(Severity::Error, at.clone(), "name a `tweak` or a `mech`".into()),
            (Some(t), _) if !tweaks.options.contains_key(t) => f(Severity::Warning, at.clone(), format!("{t} isn't a tweak this Quiver knows")),
            (_, Some(m)) if !KNOWN.contains(&m.split(':').next().unwrap_or("")) => f(Severity::Error, at.clone(), format!("unknown mechanism in {m:?}")),
            _ => {}
        }
        if c.reason.trim().is_empty() {
            f(Severity::Error, at.clone(), "give the reason people see".into());
        }
        if let Some(w) = &c.when
            && !platform
        {
            whens.push((w, at.clone()));
        }
    }
    for (w, at) in whens {
        lint_when(schema, w, &at, out);
    }
}

/// The first error of a lint, as one message.
pub fn check(schema: &Schema) -> Result<()> {
    let errors: Vec<String> = lint(schema).into_iter().filter(|f| f.severity == Severity::Error).map(|f| f.to_string()).collect();
    if !errors.is_empty() {
        bail!("{} problem(s) in the options schema:\n  {}", errors.len(), errors.join("\n  "));
    }
    Ok(())
}

/// Every `(section, key)` of an INI file, in order (each once).
pub fn ini_keys(ini: &Ini) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut section = String::new();
    for line in ini.render().lines() {
        let t = line.trim();
        if t.starts_with('[') && t.contains(']') {
            section = t[1..t.rfind(']').unwrap()].trim().to_owned();
            continue;
        }
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
            continue;
        }
        if let Some((k, _)) = t.split_once('=') {
            let k = k.trim().to_owned();
            if !k.is_empty() && !out.iter().any(|(s, x)| s.eq_ignore_ascii_case(&section) && x.eq_ignore_ascii_case(&k)) {
                out.push((section.clone(), k));
            }
        }
    }
    out
}

/// Check a schema against the files a version ships: keys that vanished, keys without a
/// description, file values that don't fit. `files`: file id → its text (`None`: not there).
/// Returns (undescribed keys as `file:section/key`, findings).
pub fn lint_files(schema: &Schema, files: &BTreeMap<String, Option<String>>) -> (Vec<String>, Vec<LintFinding>) {
    let mut out = Vec::new();
    let mut undescribed = Vec::new();
    for (fid, text) in files {
        let Some(f) = schema.files.get(fid) else { continue };
        let Some(text) = text else {
            if f.owner == FileOwner::Quiver {
                out.push(LintFinding { severity: Severity::Warning, at: format!("file {fid}"), message: format!("{} isn't in the version's files", f.path) });
            }
            continue;
        };
        if f.format != FileFormat::Ini {
            continue;
        }
        let ini = Ini::parse(text);
        let keys = ini_keys(&ini);
        let mut described = BTreeSet::new();
        for (id, d) in &schema.options {
            let Ok(b) = binding(schema, id, d) else { continue };
            if &b.file != fid {
                continue;
            }
            described.insert((b.section.to_lowercase(), b.key.to_lowercase()));
            match ini.get(&b.section, &b.key).last() {
                None if d.default.is_none() && f.owner == FileOwner::Quiver => out.push(LintFinding {
                    severity: Severity::Warning,
                    at: format!("option {id}"),
                    message: format!("[{}] {} isn't in {} and has no `default`", b.section, b.key, f.path),
                }),
                None => {}
                Some(text) => {
                    if let Err(e) = d.parse_text(text).and_then(|v| d.validate(&v)) {
                        out.push(LintFinding { severity: Severity::Warning, at: format!("option {id}"), message: format!("the file's value {text:?} doesn't fit: {e}") });
                    }
                }
            }
        }
        for (s, k) in keys {
            if !described.contains(&(s.to_lowercase(), k.to_lowercase())) {
                undescribed.push(format!("{fid}:{s}/{k}"));
            }
        }
    }
    if !undescribed.is_empty() {
        out.push(LintFinding {
            severity: Severity::Note,
            at: String::new(),
            message: format!("{} key(s) without a description (shown under Other keys): {}", undescribed.len(), undescribed.iter().take(12).cloned().collect::<Vec<_>>().join(", ")),
        });
    }
    (undescribed, out)
}

#[cfg(test)]
mod tests {

    /// Torchbearer 0.4.0 had `type = "bool"` with `default = "1"`: lint passed it, the resolver
    /// read it as on and the app's switch showed it as off. Parsing now converts it and warns.
    #[test]
    fn text_defaults_become_typed_with_a_warning() {
        let text = "format = 1\nsurface = \"t\"\n[option.blur]\ntype = \"bool\"\ndefault = \"1\"\n[option.grain]\ntype = \"bool\"\ndefault = \"0\"\n[option.fps]\ntype = \"int\"\ndefault = \"60\"\n[option.ok]\ntype = \"bool\"\ndefault = true\n";
        let (s, notes) = parse_noted(text, false).unwrap();
        assert_eq!(s.options["blur"].default, Some(Value::Bool(true)));
        assert_eq!(s.options["grain"].default, Some(Value::Bool(false)));
        assert_eq!(s.options["fps"].default, Some(Value::from(60)));
        assert_eq!(s.options["ok"].default, Some(Value::Bool(true)));
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().all(|n| n.severity == Severity::Warning));
        assert!(notes.iter().any(|n| n.at == "option blur" && n.message.contains("\"1\"") && n.message.contains("true")), "{notes:?}");
        assert_eq!(parse(text, false).unwrap().options["blur"].default, Some(Value::Bool(true)));
    }

    use super::*;

    pub(crate) const DOC_EXAMPLE: &str = include_str!("../../../../options/aco-dlss.toml");

    #[test]
    fn parses_the_catalog_example_cleanly() {
        let s = parse(DOC_EXAMPLE, false).unwrap();
        assert_eq!(s.surface, "aco-dlss");
        let errors: Vec<_> = lint(&s).into_iter().filter(|f| f.severity == Severity::Error).collect();
        assert!(errors.is_empty(), "{errors:#?}");
        assert!(s.metrics.contains_key("fg.state"));
        assert_eq!(s.metrics["fg.state"].id, "aco-dlss.fg.state");
        let b = binding(&s, "world.npc_density", &s.options["world.npc_density"]).unwrap();
        assert_eq!((b.file.as_str(), b.section.as_str(), b.key.as_str()), ("ini", "world", "npc_density"));
    }

    #[test]
    fn errors_are_loud_and_clear() {
        let base = "format = 1\nsurface = \"s\"\n[file.ini]\npath = \"a.ini\"\n";
        // Unknown fields fail, with the line.
        let e = parse(&format!("{base}[option.\"a.b\"]\ntype = \"bool\"\nlabl = \"x\"\n"), false).unwrap_err().to_string();
        assert!(e.contains("labl") && e.contains("line"), "{e}");
        // …except experiments.
        let s = parse(&format!("{base}x-note = 1\n[option.\"a.b\"]\ntype = \"bool\"\nx-ui = \"fancy\"\n"), false).unwrap();
        assert!(s.options.contains_key("a.b"));
        // A newer format is refused.
        assert!(parse("format = 2\nsurface = \"s\"\n", false).unwrap_err().to_string().contains("newer than this Quiver"));
        assert!(parse("surface = \"s\"\n", false).unwrap_err().to_string().contains("format = 1"));
        assert!(parse("format = 1\nsurface = ", false).unwrap_err().to_string().contains("not valid TOML"));
        // Lint: type fields, ranges, when, presets, pages, metrics.
        let s = parse(
            &format!(
                "{base}[option.\"a.b\"]\ntype = \"bool\"\nmin = 1\n[option.\"a.c\"]\ntype = \"float\"\nmin = 5\nmax = 1\nwhen = {{ \"a.x\" = 1 }}\n\
                 [option.\"a.d\"]\ntype = \"enum\"\napply = \"at\"\n\
                 [[page]]\nid = \"p\"\ntitle = \"P\"\noptions = [\"a.b\", \"nope\"]\n\
                 [[preset]]\nid = \"q\"\nlabel = \"Q\"\nset = {{ \"a.b\" = \"maybe\" }}\n\
                 [metric.\"Bad Id\"]\nkind = \"state\"\nlabel = \"a label that is far too long for a hud\"\n\
                 [readout.\"old\"]\nlabel = \"Old\"\n"
            ),
            false,
        )
        .unwrap();
        let msgs: Vec<String> = lint(&s).iter().map(|f| f.to_string()).collect();
        let has = |s: &str| msgs.iter().any(|m| m.contains(s));
        assert!(has("option a.b: `min` is for int/float options, not bool"), "{msgs:#?}");
        assert!(has("min 5 is above max 1"));
        assert!(has("`when` names a.x, which isn't an option"));
        assert!(has("an enum needs `values`") && has("needs `at`"));
        assert!(has("lists nope, which isn't an option"));
        assert!(has("a.b = maybe: expected 1 or 0"));
        assert!(has("ids are 2–5 segments") && has("label is longer than 24") && has("lists its `values`"));
        assert_eq!(s.metrics["old"].kind, MetricKind::Text, "readouts are text metrics");
        assert!(check(&s).unwrap_err().to_string().contains("problem(s)"));
    }

    #[test]
    fn bindings() {
        let s = parse("format = 1\nsurface = \"s\"\n[file.a]\npath = \"a.ini\"\n[file.b]\npath = \"b.ini\"\n[option.\"x.y\"]\ntype = \"int\"\nfile = \"b\"\n[option.\"top\"]\ntype = \"int\"\n", false).unwrap();
        let b = binding(&s, "x.y", &s.options["x.y"]).unwrap();
        assert_eq!(b.raw_key(), "b:x/y");
        assert!(binding(&s, "top", &s.options["top"]).unwrap_err().to_string().contains("name its `file`"));
        assert_eq!(Binding::parse_raw("fg/skip", Some("ini")).unwrap().raw_key(), "ini:fg/skip");
        assert_eq!(Binding::parse_raw("ini:/top", None).unwrap().section, "");
    }

    #[test]
    fn files_lint() {
        let s = parse(DOC_EXAMPLE, false).unwrap();
        let ini = "[upscaler]\nenabled=1\nnew_key=3\n[fg]\nmode=vulkan2\n";
        let (undescribed, f) = lint_files(&s, &[("ini".to_owned(), Some(ini.to_owned()))].into());
        assert_eq!(undescribed, ["ini:upscaler/new_key"]);
        let msgs: Vec<String> = f.iter().map(|x| x.to_string()).collect();
        assert!(msgs.iter().any(|m| m.contains("fg.mode") && m.contains("doesn't fit")), "{msgs:#?}");
        assert!(msgs.iter().any(|m| m.contains("isn't in pharos.ini")), "{msgs:#?}");
    }
}
