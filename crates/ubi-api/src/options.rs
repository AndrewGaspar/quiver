//! Mod **options** (design/MOD-CONFIG.md): a declarative description of a mod's settings
//! (`quiver-options.toml`, a [`Schema`]) from which the app, the CLI, agents and the in-game
//! overlay render the same controls, and the person's values for it, per profile, stored as
//! changes from the defaults with their own revisions.
//!
//! The wire shapes here are the contract the in-game overlay builds on (MOD-CONFIG C1–C10):
//! [`SurfaceState`] for discovery, [`OptionOp`] and [`OptionsChange`] (one [`Outcome`] per
//! operation, never an optimistic "done") for changes, [`KeysReport`] for bindings. Fields
//! are appended, never renamed.
//!
//! Value handling is here too (pure, no files), so front ends can validate, step and format
//! exactly as the daemon does: [`OptionDef::normalize`] (anything a person or a client sends
//! to a typed value), [`OptionDef::parse_text`] (a file's text to a typed value),
//! [`OptionDef::to_text`] (a typed value to what the file holds), [`OptionDef::display`].
//! The daemon is still the authority: it validates every value before writing it.
//!
//! Values a running mod reports (FG state, base FPS) are **metrics** (design/SIGHT-HUD.md
//! §3), declared statically here as `[metric.<name>]` ([`MetricDecl`]) and delivered through
//! the perf catalog (`PerfCatalog`, `PerfSubscribe`), not through options.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The schema format this build reads (`format = 1`).
pub const FORMAT: u32 = 1;

/// The schema file a mod version ships (at a plain folder's root, or next to
/// `quiver-mod.toml` in a package).
pub const SCHEMA_FILE: &str = "quiver-options.toml";
pub const SCHEMA_FILE_JSON: &str = "quiver-options.json";

// -- schema ---------------------------------------------------------------------------------

/// A surface's description: files, pages, options, presets, metrics, actions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub format: u32,
    /// Stable id chosen by the author (`aco-dlss`); values are stored under (mod id, surface).
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// The files the surface binds to, by id (`[file.ini]`).
    #[serde(default, rename = "file", skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, FileDef>,
    #[serde(default, rename = "page", skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageDef>,
    /// Options by dotted id (`[option."fg.mode"]`).
    #[serde(default, rename = "option", skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, OptionDef>,
    #[serde(default, rename = "preset", skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<PresetDef>,
    /// Static metric descriptors (`[metric.<name>]`, SIGHT-HUD §3.3), by name relative to the
    /// surface (`fg.state` is `aco-dlss.fg.state`).
    #[serde(default, rename = "metric", skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics: BTreeMap<String, MetricDecl>,
    /// An older draft's `[readout.<id>]`: read as `[metric.<id>] kind = "text"`.
    #[serde(default, rename = "readout", skip_serializing)]
    pub readouts: BTreeMap<String, MetricDecl>,
    #[serde(default, rename = "action", skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ActionDef>,
    /// Catalog schemas only: which installed mods the schema describes.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub matches: Option<MatchDef>,
    /// Catalog schemas only: who checked it, when, against what.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audited: Option<String>,
    /// Where the information comes from (a link), for catalog schemas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Which installed mod versions a catalog schema is for. Every given test must hold.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MatchDef {
    /// Product ids (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub products: Vec<u32>,
    /// Mod manifest ids; one must match (empty: not checked).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
    /// Install-relative files the version ships; all must be there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// SHA-256 (hex) of shipped files: file → accepted hashes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sha256: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileRoot {
    /// The merged install folder (default).
    #[default]
    Install,
    /// The Wine prefix.
    Prefix,
    /// The prefix user's `Documents` (`drive_c/users/steamuser/Documents`).
    Docs,
}

impl FileRoot {
    /// The prefix-relative folder `docs` stands for.
    pub const DOCS: &'static str = "drive_c/users/steamuser/Documents";
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    #[default]
    Ini,
    /// Phase 2 adapters, accepted in schemas so they lint, refused at use.
    Xml,
    Kv,
    Json,
    Toml,
    Env,
    Args,
    Sdk,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileOwner {
    /// The program writes the file itself (ACO.ini, ReShade.ini): Quiver patches only the
    /// keys with profile values.
    #[default]
    Program,
    /// The mod only reads it: Quiver renders it from the version's shipped file plus the
    /// profile's values at each launch.
    Quiver,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reload {
    /// The program reads the file once.
    #[default]
    None,
    /// It re-reads the file periodically (aco-dlss: about once a second).
    Poll,
    /// It watches the file for changes.
    Watch,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileDef {
    /// Relative to `root`, `/`-separated; `*` in a component matches any one name.
    pub path: String,
    #[serde(default)]
    pub root: FileRoot,
    #[serde(default)]
    pub format: FileFormat,
    #[serde(default)]
    pub owner: FileOwner,
    #[serde(default)]
    pub reload: Reload,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PageDef {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, rename = "group", skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupDef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GroupDef {
    pub title: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetDef {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Option id → value, applied as one change.
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionDef {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// The `key` option holding the mod's own binding for this action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey_option: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum OptionType {
    Bool,
    Enum,
    Int,
    Float,
    #[default]
    String,
    Path,
    Key,
    Color,
    List,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Basic,
    Advanced,
    Debug,
}

/// When a changed value reaches the program.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// The running program picks the change up by itself.
    Live,
    /// Read at a specific moment (`at`).
    At,
    /// Read at start (the default: nothing better is known).
    #[default]
    Restart,
    /// Quiver-side only (env, args): next launch.
    Launch,
}

/// `format` is a name for most types, or `{ on, off }` for a bool with its own spelling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FormatSpec {
    Name(String),
    Pair { on: String, off: String },
}

/// One `when` entry: a value, a list of allowed values, or a numeric range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum WhenCond {
    Range {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
    Any(Vec<Value>),
    One(Value),
}

pub type When = BTreeMap<String, WhenCond>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnumValue {
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
}

/// A labelled value: slider marks (`Quality`) and named sentinels (`0 = no cap`, `auto`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub value: Value,
    pub label: String,
}

/// One option: a typed value bound to one place. Type-specific fields are flat (only the
/// ones for its `type` are allowed; the schema linter says which).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OptionDef {
    #[serde(rename = "type")]
    pub ty: OptionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Only when the file has no value for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default)]
    pub level: Level,
    #[serde(default)]
    pub apply: Apply,
    /// `apply = "at"`: the moment ("swapchain creation").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// `apply = "live"`: how long the program takes ("about a minute").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<String>,
    /// Shown only when every entry holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    /// …or when any of these holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_any: Vec<When>,
    /// Changing it needs a confirmation (the sentence says why).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub experimental: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_from: Vec<String>,
    /// `false`: not shown in game (paths, long strings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<bool>,
    // -- binding (INI: `section.key` from the id unless given) --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    // -- bool, key, color --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FormatSpec>,
    // -- enum --
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<EnumValue>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open: bool,
    // -- int, float --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<Mark>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub special: Vec<Mark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u32>,
    // -- string --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiline: bool,
    // -- path --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    // -- key --
    /// `F1-F24`, or a comma-separated list of names and ranges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<String>,
    /// The text that means "no key" (`none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub none: Option<String>,
    // -- list --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Box<OptionDef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
}

// -- metrics (SIGHT-HUD §3.3) ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Gauge,
    Counter,
    Series,
    Frame,
    State,
    #[default]
    Text,
    Event,
    Histogram,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetricThreshold {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    pub level: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetricValue {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

/// A metric descriptor declared statically in a mod version's `quiver-options.toml`
/// (`[metric.<name>]`): the same fields a mod declares at runtime over the SDK (SIGHT-HUD
/// §3.3), so the perf catalog can merge the two field by field. `id` is filled in as
/// `<surface>.<name>` when the schema is read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetricDecl {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default)]
    pub kind: MetricKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// `{ precision, si, sign, percent }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    /// `{ min, max }` or `"auto"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thresholds: Vec<MetricThreshold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub better: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// `per-frame`, `per-event`, or a rate in Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<MetricValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Canonical roles (`fg.state`, `frame.base`), optionally with a flag filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derive: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_from: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
}

// -- values ---------------------------------------------------------------------------------

/// The numeric value of a JSON value (numbers and numeric strings).
pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Two values are the same choice (numbers compare by value: `5` is `5.0`).
pub fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_value(a, b)),
        _ => a == b,
    }
}

/// A scalar as plain text (strings without quotes).
pub fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

fn float_value(f: f64) -> Value {
    serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

fn bool_words(format: Option<&FormatSpec>) -> (String, String) {
    match format {
        Some(FormatSpec::Pair { on, off }) => (on.clone(), off.clone()),
        Some(FormatSpec::Name(n)) => match n.as_str() {
            "truefalse" => ("true".into(), "false".into()),
            "TrueFalse" => ("True".into(), "False".into()),
            "onoff" => ("on".into(), "off".into()),
            "yesno" => ("yes".into(), "no".into()),
            _ => ("1".into(), "0".into()),
        },
        None => ("1".into(), "0".into()),
    }
}

/// The bool formats a schema may name.
pub const BOOL_FORMATS: &[&str] = &["01", "truefalse", "TrueFalse", "onoff", "yesno"];
/// The key encodings a schema may name.
pub const KEY_FORMATS: &[&str] = &["vk-name", "vk-hex", "dik", "reshade", "sdl"];
/// The colour encodings a schema may name.
pub const COLOR_FORMATS: &[&str] = &["#rrggbb", "#rrggbbaa", "r,g,b", "r,g,b,a", "0xAARRGGBB"];

/// `F1-F24` style ranges and plain names, uppercased.
fn key_allowed(allowed: &str, key: &str) -> bool {
    let k = key.trim().to_ascii_uppercase();
    allowed.split(',').map(str::trim).filter(|s| !s.is_empty()).any(|part| match part.split_once('-') {
        Some((a, b)) => {
            let split = |s: &str| {
                let s = s.trim().to_ascii_uppercase();
                let i = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
                (s[..i].to_owned(), s[i..].parse::<u32>().ok())
            };
            match (split(a), split(b), split(&k)) {
                ((pa, Some(na)), (pb, Some(nb)), (pk, Some(nk))) => pa == pb && pa == pk && (na..=nb).contains(&nk),
                _ => false,
            }
        }
        None => part.eq_ignore_ascii_case(&k),
    })
}

impl OptionDef {
    /// The label, or the id's last part.
    pub fn label_or<'a>(&'a self, id: &'a str) -> &'a str {
        self.label.as_deref().unwrap_or_else(|| id.rsplit('.').next().unwrap_or(id))
    }

    /// Shown in the in-game overlay (C8: `overlay = false`, paths and multi-line strings aren't).
    pub fn in_overlay(&self) -> bool {
        self.overlay.unwrap_or(!(self.ty == OptionType::Path || (self.ty == OptionType::String && self.multiline)))
    }

    /// The special (named sentinel) a value is, if any.
    pub fn special_of(&self, v: &Value) -> Option<&Mark> {
        self.special.iter().find(|s| same_value(&s.value, v) || (s.value.is_string() && v.is_string() && scalar_text(&s.value).eq_ignore_ascii_case(&scalar_text(v))))
    }

    fn special_text(&self, text: &str) -> Option<Value> {
        let t = text.trim();
        self.special.iter().find_map(|s| {
            let st = scalar_text(&s.value);
            let numeric_eq = as_f64(&s.value).zip(t.parse::<f64>().ok()).is_some_and(|(a, b)| a == b);
            (st.eq_ignore_ascii_case(t) || numeric_eq).then(|| s.value.clone())
        })
    }

    /// A file's text as a typed value (lenient about spelling; [`Self::validate`] checks it).
    pub fn parse_text(&self, text: &str) -> Result<Value, String> {
        let t = text.trim();
        if let Some(v) = self.special_text(t) {
            return Ok(v);
        }
        match self.ty {
            OptionType::Bool => {
                let (on, off) = bool_words(self.format.as_ref());
                let l = t.to_ascii_lowercase();
                if t.eq_ignore_ascii_case(&on) || matches!(l.as_str(), "1" | "true" | "on" | "yes") {
                    Ok(Value::Bool(true))
                } else if t.eq_ignore_ascii_case(&off) || matches!(l.as_str(), "0" | "false" | "off" | "no") {
                    Ok(Value::Bool(false))
                } else {
                    Err(format!("expected {on} or {off}, got '{t}'"))
                }
            }
            OptionType::Int => match t.parse::<i64>() {
                Ok(n) => Ok(Value::from(n)),
                Err(_) => match t.parse::<f64>() {
                    Ok(f) if f.fract() == 0.0 => Ok(Value::from(f as i64)),
                    _ => Err(format!("expected a whole number, got '{t}'")),
                },
            },
            OptionType::Float => t.parse::<f64>().ok().filter(|f| f.is_finite()).map(float_value).ok_or_else(|| format!("expected a number, got '{t}'")),
            OptionType::Enum => {
                let found = self.values.iter().find(|e| scalar_text(&e.value).eq_ignore_ascii_case(t));
                match found {
                    Some(e) => Ok(e.value.clone()),
                    None if self.open => Ok(Value::String(t.to_owned())),
                    None => Err(format!("expected one of {}, got '{t}'", self.enum_list())),
                }
            }
            OptionType::Key => {
                let none = self.none.as_deref().unwrap_or("none");
                if t.is_empty() || t.eq_ignore_ascii_case(none) {
                    return Ok(Value::String(none.to_owned()));
                }
                let vk_name = matches!(&self.format, None | Some(FormatSpec::Name(_))) && self.format.as_ref().is_none_or(|f| f == &FormatSpec::Name("vk-name".into()));
                Ok(Value::String(if vk_name { t.to_ascii_uppercase() } else { t.to_owned() }))
            }
            OptionType::List => {
                let item = self.item.as_deref().cloned().unwrap_or_default();
                let sep = self.sep.as_deref().unwrap_or(",");
                if t.is_empty() {
                    return Ok(Value::Array(Vec::new()));
                }
                t.split(sep).map(str::trim).filter(|s| !s.is_empty()).map(|s| item.parse_text(s)).collect::<Result<Vec<_>, _>>().map(Value::Array)
            }
            OptionType::String | OptionType::Path | OptionType::Color => Ok(Value::String(text.trim().to_owned())),
        }
    }

    fn enum_list(&self) -> String {
        self.values.iter().map(|e| scalar_text(&e.value)).collect::<Vec<_>>().join(", ")
    }

    /// What a client sent (typed JSON, or text as a person types it) as a valid typed value.
    pub fn normalize(&self, v: &Value) -> Result<Value, String> {
        if let Some(s) = self.special_of(v) {
            return Ok(s.value.clone());
        }
        let typed = match (self.ty, v) {
            (_, Value::String(s)) => self.parse_text(s)?,
            (OptionType::Bool, Value::Number(_)) => self.parse_text(&scalar_text(v))?,
            (OptionType::Float, Value::Number(n)) => float_value(n.as_f64().unwrap_or_default()),
            (OptionType::Int, Value::Number(n)) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => Value::from(i),
                (None, Some(f)) if f.fract() == 0.0 => Value::from(f as i64),
                _ => return Err(format!("expected a whole number, got {n}")),
            },
            (OptionType::Enum | OptionType::Key, Value::Number(_) | Value::Bool(_)) => self.parse_text(&scalar_text(v))?,
            (OptionType::List, Value::Array(items)) => {
                let item = self.item.as_deref().cloned().unwrap_or_default();
                Value::Array(items.iter().map(|i| item.normalize(i)).collect::<Result<_, _>>()?)
            }
            (_, Value::Null) => return Err("no value given".into()),
            _ => v.clone(),
        };
        self.validate(&typed)?;
        Ok(typed)
    }

    /// Check a typed value: type, range, allowed values, pattern, length.
    pub fn validate(&self, v: &Value) -> Result<(), String> {
        if self.special_of(v).is_some() {
            return Ok(());
        }
        match self.ty {
            OptionType::Bool => if v.is_boolean() { Ok(()) } else { Err(format!("expected on or off, got {v}")) },
            OptionType::Int | OptionType::Float => {
                let Some(f) = as_f64(v).filter(|_| v.is_number()) else { return Err(format!("expected a number, got {v}")) };
                if self.ty == OptionType::Int && f.fract() != 0.0 {
                    return Err(format!("expected a whole number, got {f}"));
                }
                let unit = self.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
                if let Some(min) = self.min
                    && f < min
                {
                    return Err(format!("{} is below the minimum {}{unit}{}", scalar_text(v), scalar_text(&float_value(min)), self.specials_note()));
                }
                if let Some(max) = self.max
                    && f > max
                {
                    return Err(format!("{} is above the maximum {}{unit}{}", scalar_text(v), scalar_text(&float_value(max)), self.specials_note()));
                }
                Ok(())
            }
            OptionType::Enum => {
                if self.values.iter().any(|e| same_value(&e.value, v)) || (self.open && v.is_string()) {
                    Ok(())
                } else {
                    Err(format!("{} is not one of {}", scalar_text(v), self.enum_list()))
                }
            }
            OptionType::Key => {
                let Some(s) = v.as_str() else { return Err(format!("expected a key name, got {v}")) };
                let none = self.none.as_deref().unwrap_or("none");
                if s.eq_ignore_ascii_case(none) {
                    return Ok(());
                }
                match &self.allowed {
                    Some(a) if !key_allowed(a, s) => Err(format!("{s} is not an allowed key ({a}, or {none})")),
                    _ => Ok(()),
                }
            }
            OptionType::List => {
                let Some(items) = v.as_array() else { return Err(format!("expected a list, got {v}")) };
                if let Some(n) = self.min_items
                    && items.len() < n
                {
                    return Err(format!("needs at least {n} item(s)"));
                }
                if let Some(n) = self.max_items
                    && items.len() > n
                {
                    return Err(format!("takes at most {n} item(s)"));
                }
                let item = self.item.as_deref().cloned().unwrap_or_default();
                let sep = self.sep.as_deref().unwrap_or(",");
                for i in items {
                    item.validate(i)?;
                    if scalar_text(i).contains(sep) {
                        return Err(format!("item {} contains the separator '{sep}'", scalar_text(i)));
                    }
                }
                Ok(())
            }
            OptionType::String | OptionType::Path | OptionType::Color => {
                let Some(s) = v.as_str() else { return Err(format!("expected text, got {v}")) };
                if s.contains(['\n', '\r']) {
                    return Err("a value can't contain a line break".into());
                }
                if let Some(n) = self.max_len
                    && s.chars().count() > n
                {
                    return Err(format!("longer than {n} characters"));
                }
                if let Some(p) = &self.pattern {
                    let re = regex::Regex::new(p).map_err(|e| format!("the schema's pattern doesn't compile: {e}"))?;
                    if !re.is_match(s) {
                        return Err(format!("'{s}' doesn't match the pattern {p}"));
                    }
                }
                Ok(())
            }
        }
    }

    fn specials_note(&self) -> String {
        match self.special.as_slice() {
            [] => String::new(),
            list => format!(" (or {})", list.iter().map(|s| format!("{} = {}", scalar_text(&s.value), s.label)).collect::<Vec<_>>().join(", ")),
        }
    }

    /// A typed value as the file holds it (`"1"`, `"d3d12"`, `"F9"`).
    pub fn to_text(&self, v: &Value) -> String {
        if let Some(s) = self.special_of(v) {
            return scalar_text(&s.value);
        }
        match (self.ty, v) {
            (OptionType::Bool, Value::Bool(b)) => {
                let (on, off) = bool_words(self.format.as_ref());
                if *b { on } else { off }
            }
            (OptionType::Float, Value::Number(n)) => match (self.precision, n.as_f64()) {
                (Some(p), Some(f)) => format!("{f:.*}", p as usize),
                _ => scalar_text(v),
            },
            (OptionType::List, Value::Array(items)) => {
                let item = self.item.as_deref().cloned().unwrap_or_default();
                items.iter().map(|i| item.to_text(i)).collect::<Vec<_>>().join(self.sep.as_deref().unwrap_or(","))
            }
            _ => scalar_text(v),
        }
    }

    /// A value for people: `on`, `0.667 Quality`, `4 ×`, `K: DLSS 4 transformer`, `No cap`.
    pub fn display(&self, v: &Value) -> String {
        if v.is_null() {
            return "(not set)".into();
        }
        if let Some(s) = self.special_of(v) {
            return format!("{} ({})", scalar_text(&s.value), s.label);
        }
        match (self.ty, v) {
            (OptionType::Bool, Value::Bool(b)) => (if *b { "on" } else { "off" }).into(),
            (OptionType::Int | OptionType::Float, _) => {
                let mut out = self.to_text(v);
                if let Some(u) = &self.unit {
                    out.push(' ');
                    out.push_str(u);
                }
                if let Some(m) = self.marks.iter().find(|m| same_value(&m.value, v)) {
                    out.push(' ');
                    out.push_str(&m.label);
                }
                out
            }
            (OptionType::Enum, _) => match self.values.iter().find(|e| same_value(&e.value, v)).and_then(|e| e.label.as_deref()) {
                Some(l) if l.starts_with(&format!("{}:", scalar_text(v))) || l.eq_ignore_ascii_case(&scalar_text(v)) => l.to_owned(),
                Some(l) => format!("{}  {l}", scalar_text(v)),
                None => scalar_text(v),
            },
            (OptionType::List, Value::Array(a)) if a.is_empty() => "(none)".into(),
            _ => self.to_text(v),
        }
    }

    /// Whether `when`/`when_any` hold for the current values (`get` gives an option's
    /// effective value). No conditions: visible.
    pub fn visible(&self, get: &dyn Fn(&str) -> Option<Value>) -> bool {
        let holds = |w: &When| w.iter().all(|(id, c)| get(id).is_some_and(|v| cond_holds(c, &v)));
        let all = self.when.as_ref().is_none_or(holds);
        let any = self.when_any.is_empty() || self.when_any.iter().any(holds);
        all && any
    }
}

/// One `when` condition against a value.
pub fn cond_holds(c: &WhenCond, v: &Value) -> bool {
    let eq = |a: &Value| same_value(a, v) || scalar_text(a).eq_ignore_ascii_case(&scalar_text(v));
    match c {
        WhenCond::One(a) => eq(a),
        WhenCond::Any(list) => list.iter().any(eq),
        WhenCond::Range { min, max } => as_f64(v).is_some_and(|f| min.is_none_or(|m| f >= m) && max.is_none_or(|m| f <= m)),
    }
}

// -- what clients see -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    /// Shipped with (or described for) a mod version.
    #[default]
    Mod,
    /// A game's own settings (Quiver's catalog; phase 2).
    Game,
    /// A tool (DXVK, MangoHud, the in-game overlay's own keys).
    Runtime,
}

/// Where a surface's schema came from (precedence: author, catalog, local, inferred).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchemaSource {
    /// `quiver-options.toml` shipped with the version (or carried forward from an earlier one).
    #[default]
    Author,
    /// Quiver's audited catalog (`options/<id>.toml` in the repo).
    Catalog,
    /// Written for an installed mod by a person or an agent (`options schema --set`).
    Local,
    /// Guessed from the file.
    Inferred,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveChannel {
    /// No way to reach the running program: changes are saved for the next launch.
    #[default]
    None,
    /// Quiver rewrites the file inside the running game's namespace; the mod re-reads it.
    File,
    /// The mod's SDK channel (two-way, acknowledged).
    Sdk,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LiveInfo {
    pub channel: LiveChannel,
    /// The running session's SDK channel is connected (SDK surfaces), or the file channel
    /// has a process to write through (file surfaces).
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GroupState {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PageState {
    pub id: String,
    pub title: String,
    pub groups: Vec<GroupState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PresetInfo {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Option id → value.
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ActionInfo {
    pub label: String,
    /// The mod's binding for it right now (normalized), if it names a key option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
}

/// Where an option's effective value comes from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueSource {
    /// The shipped file's value, the schema's `default`, or the file's own value.
    #[default]
    Default,
    /// A mod's config patch sets it (MODS-V2).
    ModPatch {
        #[serde(rename = "mod")]
        mod_id: String,
    },
    /// The profile's value, set in this revision by this source.
    Profile { revision: u32, source: String },
    /// Set for the running session only (`Live::Only`), not saved.
    Session { source: String },
}

/// A saved value the running game hasn't picked up (yet).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    #[default]
    None,
    /// Applies when the game restarts.
    Restart,
    /// Applies at a moment (`at`).
    At { at: String },
    /// A live option the running program can't be reached for (no live channel).
    Reread,
}

impl Pending {
    pub fn is_none(&self) -> bool {
        *self == Pending::None
    }
}

/// One option in a surface, as a client renders it (C2).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OptionState {
    pub def: OptionDef,
    /// The effective value (typed) and what the file holds for it.
    pub value: Value,
    pub text: String,
    /// The value without the profile's (and session's) change; `null`: none known.
    pub default: Value,
    pub from: ValueSource,
    /// `when` already evaluated.
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Pending::is_none")]
    pub pending: Pending,
    /// A stored value that doesn't fit this version's schema: kept, not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dormant: Option<String>,
    /// The file's own text for this key doesn't fit the schema (shown, used as is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
}

/// A key in a surface's files that its schema doesn't describe ("Other keys"), with a
/// guessed type.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RawKey {
    /// `file:section/key` (`ini:fg/skip`).
    pub binding: String,
    /// The effective text, and the file's own.
    pub text: String,
    pub default: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<ValueSource>,
    pub guess: OptionDef,
}

/// A stored value the resolved version doesn't use (gone, renamed without `renamed_from`, or
/// no longer valid).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DormantValue {
    pub id: String,
    pub value: Value,
    pub reason: String,
}

/// One file a surface binds to, and where it really is for this profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SurfaceFile {
    pub id: String,
    pub def: FileDef,
    /// Install- or prefix-relative path (after `*` matching: the first match).
    pub rel: String,
    /// The file that provides the defaults (the shipped copy, or the file the program
    /// wrote), if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<std::path::PathBuf>,
    /// Where Quiver writes it for this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<std::path::PathBuf>,
    /// `install:<profile>`, `prefix` or `profile:<id>` (MODS-V2 `profile_config`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

/// A surface for one profile: schema, values and where they come from (C2).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SurfaceState {
    /// `<mod id>/<surface>` for mod surfaces (`game/<surface>`, `runtime/<surface>` otherwise):
    /// the key values are stored under.
    pub key: String,
    pub surface: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub kind: SurfaceKind,
    /// The mod (store id) and the version (layer id, label) it resolved to.
    #[serde(default, rename = "mod", skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_label: Option<String>,
    pub schema_source: SchemaSource,
    /// In words: "schema shipped with v4", "Quiver catalog", "guessed".
    pub schema_note: String,
    pub files: Vec<SurfaceFile>,
    pub live: LiveInfo,
    pub pages: Vec<PageState>,
    pub options: BTreeMap<String, OptionState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<PresetInfo>,
    /// The surface's metrics (full ids); resolve them with the perf catalog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ActionInfo>,
    /// "Other keys": undescribed keys of the surface's files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw: Vec<RawKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dormant: Vec<DormantValue>,
    /// Lint findings about the schema against this version's files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lint: Vec<String>,
}

// -- changes --------------------------------------------------------------------------------

/// One change in an [`OptionsSet`](crate::mods::ModsRequest::OptionsSet). `surface` is a
/// surface key (`m7d2k1x/aco-dlss`), a surface id (`aco-dlss`) or the mod's name or id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OptionOp {
    /// Set an option: typed JSON, or text as typed (`"on"`, `"0.667"`, `"F9"`).
    Set { surface: String, id: String, value: Value },
    /// Delete the profile's value (back to the default).
    Reset { surface: String, id: String },
    /// Apply a preset as one change.
    Preset { surface: String, id: String },
    /// Set (or with `None`, reset) a key the schema doesn't describe: `binding` is
    /// `section/key` or `file:section/key`.
    Raw {
        surface: String,
        binding: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Reset every value of a surface.
    ResetAll { surface: String },
}

/// Whether a change goes to the running game.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Live {
    /// Saved, and delivered live if the game runs and the option can be.
    #[default]
    IfRunning,
    /// Saved only; the running game isn't touched.
    No,
    /// The running session only, not saved (a "try" UX; reverted at session end).
    Only,
}

/// What happened to one operation. Never optimistic: `applied_live` means delivered (and,
/// for SDK mods, acknowledged).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The running program has it (file rewritten for a mod that re-reads it; SDK acked).
    AppliedLive {
        channel: LiveChannel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latency: Option<String>,
    },
    /// Saved; applies at the next launch (`running`: the game runs now and needs a restart).
    SavedRestart { running: bool },
    /// Saved; the running game reads it at a moment (`at`).
    SavedAt { at: String },
    /// Saved; a live option, but the running program can't be reached (`detail` says why):
    /// it applies when the mod re-reads its file, or at the next launch.
    SavedPending { detail: String },
    /// Kept but not applied: the resolved version's schema doesn't take it.
    Dormant { reason: String },
    Rejected { reason: String },
    /// The value was already that.
    Unchanged,
    /// The SDK channel didn't acknowledge in time (the value is saved).
    TimedOut,
}

impl Outcome {
    /// One phrase for people.
    pub fn describe(&self) -> String {
        match self {
            Outcome::AppliedLive { channel: LiveChannel::Sdk, .. } => "applied live (the mod confirmed)".into(),
            Outcome::AppliedLive { latency: Some(l), .. } => format!("applied live; the mod picks it up within {l}"),
            Outcome::AppliedLive { .. } => "applied live".into(),
            Outcome::SavedRestart { running: true } => "saved; needs a restart".into(),
            Outcome::SavedRestart { running: false } => "saved; applies next launch".into(),
            Outcome::SavedAt { at } => format!("saved; applies at {at}"),
            Outcome::SavedPending { detail } => format!("saved; {detail}"),
            Outcome::Dormant { reason } => format!("kept, not applied: {reason}"),
            Outcome::Rejected { reason } => format!("rejected: {reason}"),
            Outcome::Unchanged => "unchanged".into(),
            Outcome::TimedOut => "saved; the mod didn't confirm in time".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpOutcome {
    /// Surface key and option id (or raw binding); empty for an op that names no option.
    pub surface: String,
    pub id: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

/// The reply to `OptionsSet`, `OptionsRestore` and `OptionsRevertSession`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OptionsChange {
    pub product_id: u32,
    /// Profile id (`vanilla` for vanilla).
    pub profile: String,
    /// The new revision (0: nothing saved).
    pub revision: u32,
    pub outcomes: Vec<OpOutcome>,
    /// In words ("DLSS world.npc_density 4 → 5").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    /// What reached the running game, in words ("rewrote aco_dlss.ini").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Stored values of one profile: surface key → option id → typed value.
pub type Values = BTreeMap<String, BTreeMap<String, Value>>;
/// Stored raw values (keys without a schema): surface key → `file:section/key` → text.
pub type RawValues = BTreeMap<String, BTreeMap<String, String>>;

/// One revision of a profile's option values (`games/<product>/options/<profile>.jsonl`):
/// the whole state from then on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OptionsRevision {
    pub revision: u32,
    pub at: DateTime<Utc>,
    /// `gui`, `cli`, agent names, `in-game`, `game` (imported from a file the program
    /// wrote), `file` (a hand edit), `mod:<id>` (reported over the SDK).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    #[serde(default)]
    pub values: Values,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub raw: RawValues,
}

/// One difference between two sets of values (`OptionsDiff`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OptionDiff {
    pub surface: String,
    pub title: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// This profile's effective value and the other side's, as people read them.
    pub value: String,
    pub against: String,
    /// Typed, when known.
    #[serde(default)]
    pub value_json: Value,
    #[serde(default)]
    pub against_json: Value,
}

/// The reply to `OptionsSchema` / `OptionsSchemaSet`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SchemaInfo {
    pub key: String,
    #[serde(rename = "mod")]
    pub mod_id: String,
    pub version: String,
    pub version_label: String,
    pub source: SchemaSource,
    pub note: String,
    pub schema: Schema,
    /// The schema's text as stored (TOML), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lint: Vec<LintFinding>,
    /// For a local schema: its revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Note,
    Warning,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LintFinding {
    pub severity: Severity,
    /// What it's about (`option fg.mode`, `file ini`, `metric fg.state`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    pub message: String,
}

impl std::fmt::Display for LintFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        };
        if self.at.is_empty() { write!(f, "{sev}: {}", self.message) } else { write!(f, "{sev}: {}: {}", self.at, self.message) }
    }
}

/// The reply to `OptionsLint`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LintReport {
    /// The schema file read, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<std::path::PathBuf>,
    pub surface: String,
    pub options: usize,
    /// Keys in the files the schema doesn't describe.
    pub undescribed: Vec<String>,
    pub findings: Vec<LintFinding>,
}

impl LintReport {
    pub fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::Error).count()
    }
}

/// One key binding in play for a profile (C7), normalized to W3C `KeyboardEvent.code`-style
/// names with modifiers (`F9`, `Shift+Home`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyBinding {
    pub key: String,
    /// As the source spells it (`F9`, `0x78`, `36,0,0,0`).
    pub raw: String,
    /// Who uses it: a surface title, `Quiver HUD`, `Steam`, …
    pub owner: String,
    /// Surface key and option id, for option bindings.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub label: String,
    /// `option`, `runtime`, `overlay`, `known`.
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyConflict {
    pub key: String,
    /// Indexes into `KeysReport::bindings`.
    pub bindings: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KeysReport {
    pub product_id: u32,
    pub profile: String,
    pub bindings: Vec<KeyBinding>,
    /// Never resolved automatically, only shown.
    pub conflicts: Vec<KeyConflict>,
}

/// What `OptionsInvoke` did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InvokeResult {
    pub session: String,
    pub surface: String,
    pub action: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A session's option changes, for "Revert this session" and crash correlation (§8).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionOptions {
    pub session: String,
    pub product_id: u32,
    pub profile: String,
    /// The options revision the session launched with.
    pub launched_revision: u32,
    pub started: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<DateTime<Utc>>,
    /// Live changes: when, which surface and option.
    #[serde(default)]
    pub live_changes: Vec<LiveChange>,
    /// The game exited abnormally soon after a live change (never acted on automatically).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash: Option<CrashNote>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LiveChange {
    pub at: DateTime<Utc>,
    pub surface: String,
    pub id: String,
    pub source: String,
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CrashNote {
    /// Seconds between the last live change and the exit.
    pub seconds_after: i64,
    /// The last live change (`DLSS fg.toggle_key`).
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The sentence to show ("AC Origins crashed 4 s after … Revert this session's changes?").
    pub message: String,
}

// -- requests and events ------------------------------------------------------------------------

/// Requests about mod options (`Request::Options`). Games are product ids; profiles are
/// named by id or (case-insensitive) name, `vanilla` for vanilla, `None` for the game's
/// default profile. Agent policy (MOD-CONFIG steer 13): setting values on profiles is
/// allowed; live changes to a running game, restores and `danger` options are "Ask".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OptionsRequest {
    /// Every options surface of a profile's stack, renderable without file knowledge
    /// (`Vec<options::SurfaceState>`, C2). `profile` `None`: the game's default profile.
    /// `level`: hide options above it (`basic`, `advanced`; default: everything).
    List {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<Level>,
    },
    /// Change option values (`OptionsChange`, one outcome per op, C3). Validated by
    /// the daemon. Agents need `confirmed` for live changes to a running game and for
    /// `danger` options; everyone needs it for `danger` options.
    Set {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        ops: Vec<OptionOp>,
        #[serde(default)]
        live: Live,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        confirmed: bool,
    },
    /// What a profile's values differ in (`Vec<options::OptionDiff>`): against `default`
    /// (the default), another profile (name or id), or a revision (`r12`).
    Diff {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        against: Option<String>,
    },
    /// A profile's options history (`Vec<options::OptionsRevision>`).
    History {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// Make an earlier options revision current again, as a new revision (live where the
    /// game runs). Agents need `confirmed`.
    Restore {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        revision: u32,
        #[serde(default)]
        confirmed: bool,
    },
    /// Put back the values a session launched with (C4; also after a crash), live where
    /// possible. Agents need `confirmed`.
    RevertSession {
        product_id: u32,
        session: String,
        #[serde(default)]
        confirmed: bool,
    },
    /// Run a mod's action in a running session (SDK; `InvokeResult`).
    Invoke {
        product_id: u32,
        surface: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arg: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Every key binding in play for a profile, normalized, with conflicts
    /// (`KeysReport`, C7).
    Keys {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// A mod version's resolved schema, its source and lint findings (`SchemaInfo`).
    Schema {
        product_id: u32,
        #[serde(rename = "mod")]
        mod_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// Write a local schema for an installed mod (revisioned; `SchemaInfo`).
    SchemaSet {
        product_id: u32,
        #[serde(rename = "mod")]
        mod_id: String,
        toml: String,
        #[serde(default)]
        message: Option<String>,
    },
    /// A schema guessed from one of a mod version's files (TOML text).
    Infer {
        product_id: u32,
        #[serde(rename = "mod")]
        mod_id: String,
        file: String,
    },
    /// Check a package or plain folder's schema against its files (`LintReport`).
    Lint { path: PathBuf },
    /// Games being played with their options addressing (`Vec<SessionDetail>`, C1).
    Sessions,
}

/// A running game as the in-game overlay and the CLI address it (C1): the session as
/// `Request::Sessions` lists it, plus the launch's session id, profile, options revision
/// and processes. `Request::Sessions` answers these fields too (clients that don't know
/// them ignore them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub base: crate::ActiveSession,
    /// `s-…`, also `QUIVER_SESSION` in the game's environment (games quiverd launched).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Profile id (`vanilla`), for games quiverd launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_revision: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<u32>,
}

/// Options events, sent to subscribed connections as `Reply::Options` (C5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OptionsEvent {
    /// A new revision of a profile's values.
    Changed {
        product_id: u32,
        profile: String,
        revision: u32,
        source: String,
        #[serde(default)]
        changes: Vec<String>,
    },
    /// Values changed for a running session (by anyone; saved or not): option id → value.
    Live {
        session: String,
        product_id: u32,
        surface: String,
        values: BTreeMap<String, Value>,
        by: String,
        #[serde(default)]
        saved: bool,
    },
    /// The game exited abnormally soon after a live change: offer `RevertSession` (never
    /// done automatically, §8).
    Crash { product_id: u32, profile: String, note: SessionOptions },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(ty: OptionType) -> OptionDef {
        OptionDef { ty, ..Default::default() }
    }

    #[test]
    fn bools() {
        let d = def(OptionType::Bool);
        assert_eq!(d.parse_text("1").unwrap(), json!(true));
        assert_eq!(d.parse_text(" 0 ").unwrap(), json!(false));
        assert_eq!(d.normalize(&json!("on")).unwrap(), json!(true));
        assert_eq!(d.to_text(&json!(true)), "1");
        assert!(d.parse_text("maybe").unwrap_err().contains("expected 1 or 0"));
        let ue = OptionDef { format: Some(FormatSpec::Name("TrueFalse".into())), ..def(OptionType::Bool) };
        assert_eq!(ue.to_text(&json!(false)), "False");
        assert_eq!(ue.parse_text("True").unwrap(), json!(true));
        let pair = OptionDef { format: Some(FormatSpec::Pair { on: "enabled".into(), off: "disabled".into() }), ..def(OptionType::Bool) };
        assert_eq!(pair.parse_text("Enabled").unwrap(), json!(true));
        assert_eq!(pair.to_text(&json!(false)), "disabled");
        assert_eq!(d.display(&json!(true)), "on");
    }

    #[test]
    fn numbers_ranges_and_specials() {
        let d = OptionDef {
            min: Some(0.33),
            max: Some(1.0),
            special: vec![Mark { value: json!(0), label: "Use the game's".into() }],
            marks: vec![Mark { value: json!(0.667), label: "Quality".into() }],
            ..def(OptionType::Float)
        };
        assert_eq!(d.normalize(&json!("0.667")).unwrap(), json!(0.667));
        assert_eq!(d.display(&json!(0.667)), "0.667 Quality");
        // 0 is a named sentinel, allowed below the minimum.
        assert_eq!(d.normalize(&json!(0)).unwrap(), json!(0));
        assert_eq!(d.display(&json!(0)), "0 (Use the game's)");
        let e = d.normalize(&json!(0.1)).unwrap_err();
        assert!(e.contains("below the minimum 0.33") && e.contains("0 = Use the game's"), "{e}");
        assert!(d.normalize(&json!(2)).unwrap_err().contains("above the maximum 1"));
        // A string sentinel on a float.
        let bias = OptionDef { min: Some(-3.0), max: Some(1.0), special: vec![Mark { value: json!("auto"), label: "Automatic".into() }], ..def(OptionType::Float) };
        assert_eq!(bias.parse_text("AUTO").unwrap(), json!("auto"));
        assert_eq!(bias.to_text(&json!("auto")), "auto");
        assert_eq!(bias.parse_text("-0.5").unwrap(), json!(-0.5));
        assert!(bias.normalize(&json!("loud")).is_err());
        let n = OptionDef { min: Some(1.0), max: Some(5.0), unit: Some("×".into()), ..def(OptionType::Float) };
        assert_eq!(n.to_text(&n.normalize(&json!(5)).unwrap()), "5");
        assert_eq!(n.display(&json!(4.0)), "4 ×");
        let i = def(OptionType::Int);
        assert_eq!(i.parse_text("165").unwrap(), json!(165));
        assert_eq!(i.parse_text("3.0").unwrap(), json!(3));
        assert!(i.normalize(&json!(1.5)).is_err());
        let p = OptionDef { precision: Some(6), ..def(OptionType::Float) };
        assert_eq!(p.to_text(&json!(1.0)), "1.000000");
        assert!(same_value(&json!(5), &json!(5.0)));
    }

    #[test]
    fn enums_keys_lists_strings() {
        let e = OptionDef {
            values: vec![
                EnumValue { value: json!("off"), label: Some("Off".into()), ..Default::default() },
                EnumValue { value: json!("d3d12"), label: Some("On (D3D12 route)".into()), ..Default::default() },
            ],
            ..def(OptionType::Enum)
        };
        assert_eq!(e.parse_text("D3D12").unwrap(), json!("d3d12"));
        assert!(e.normalize(&json!("vulkan")).unwrap_err().contains("off, d3d12"));
        assert_eq!(e.display(&json!("d3d12")), "d3d12  On (D3D12 route)");
        let open = OptionDef { open: true, ..e.clone() };
        assert_eq!(open.normalize(&json!("default")).unwrap(), json!("default"));
        let k = OptionDef { allowed: Some("F1-F24".into()), none: Some("none".into()), format: Some(FormatSpec::Name("vk-name".into())), ..def(OptionType::Key) };
        assert_eq!(k.normalize(&json!("f9")).unwrap(), json!("F9"));
        assert_eq!(k.normalize(&json!("none")).unwrap(), json!("none"));
        assert!(k.normalize(&json!("F25")).unwrap_err().contains("not an allowed key"));
        assert!(k.normalize(&json!("Home")).is_err());
        let l = OptionDef { item: Some(Box::new(OptionDef { pattern: Some(r"^DLSSG\.\w+:\d+$".into()), ..def(OptionType::String) })), ..def(OptionType::List) };
        assert_eq!(l.parse_text("DLSSG.A:1, DLSSG.B:2").unwrap(), json!(["DLSSG.A:1", "DLSSG.B:2"]));
        assert_eq!(l.parse_text("").unwrap(), json!([]));
        assert_eq!(l.to_text(&json!(["DLSSG.A:1", "DLSSG.B:2"])), "DLSSG.A:1,DLSSG.B:2");
        assert!(l.normalize(&json!("nope")).unwrap_err().contains("pattern"));
        let s = OptionDef { max_len: Some(3), ..def(OptionType::String) };
        assert!(s.normalize(&json!("abcd")).is_err());
        assert!(s.normalize(&json!("a\nb")).is_err());
    }

    #[test]
    fn when_conditions() {
        let d = OptionDef {
            when: Some([("fg.mode".to_owned(), WhenCond::One(json!("d3d12")))].into()),
            when_any: vec![[("a".to_owned(), WhenCond::Range { min: Some(1.0), max: None })].into(), [("b".to_owned(), WhenCond::Any(vec![json!("x"), json!("y")]))].into()],
            ..def(OptionType::Bool)
        };
        let vals = |m: Value| move |id: &str| m.get(id).cloned();
        assert!(d.visible(&vals(json!({"fg.mode": "d3d12", "a": 2}))));
        assert!(d.visible(&vals(json!({"fg.mode": "d3d12", "a": 0, "b": "y"}))));
        assert!(!d.visible(&vals(json!({"fg.mode": "d3d12", "a": 0, "b": "z"}))));
        assert!(!d.visible(&vals(json!({"fg.mode": "off", "a": 2}))));
        // TOML-shaped conditions read as the right variants.
        let w: When = serde_json::from_value(json!({"x": {"min": 1, "max": 3}, "y": ["a", "b"], "z": "c"})).unwrap();
        assert!(matches!(w["x"], WhenCond::Range { .. }) && matches!(w["y"], WhenCond::Any(_)) && matches!(w["z"], WhenCond::One(_)));
    }

    #[test]
    fn outcomes_on_the_wire() {
        let o = OpOutcome { surface: "m1/aco-dlss".into(), id: "fg.mode".into(), outcome: Outcome::SavedAt { at: "swapchain creation".into() }, before: None, after: Some(json!("off")) };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["outcome"], "saved_at");
        assert_eq!(v["at"], "swapchain creation");
        let back: OpOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back, o);
        let op: OptionOp = serde_json::from_value(json!({"op": "set", "surface": "DLSS", "id": "a.b", "value": 5})).unwrap();
        assert!(matches!(op, OptionOp::Set { .. }));
        assert_eq!(serde_json::to_value(Live::IfRunning).unwrap(), "if_running");
    }
}
