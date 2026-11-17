//! Quiver's mod SDK protocol: what `sdk/quiver_mod.h` (a mod DLL inside the game) speaks
//! to `quiver-bridge` over the pipe named by `QUIVER_MODS_PIPE`, and what the bridge relays to
//! quiverd (design: MOD-CONFIG §5 "SDK", SIGHT-HUD §3.6).
//!
//! **Framing on the pipe:** a 4-byte little-endian length, then one UTF-8 JSON object with a
//! `t` field naming the message. **Between the bridge and quiverd:** the same objects, one
//! per line, on a broker connection that began with `BrokerRequest::ModChannel`.
//!
//! Unknown `t` values and unknown fields are ignored both ways. `proto` only grows; each side
//! speaks `min(theirs, mine)`. This is protocol [`PROTOCOL`] = 2: the options messages here
//! (`hello`/`welcome`, `options`, `set`/`ack`, `changed`, `readout`, `action`, `log`, `bye`)
//! plus the perf messages, which are `quiver_perf::control::PerfMessage` (`metrics`,
//! `metrics_ok`, `m`, `mark`; the ring ones are SIGHT-HUD phase 2). [`is_perf_message`] tells
//! which enum a message belongs to.
//!
//! Everything a mod sends is untrusted: [`check_frame`] is the bridge's structural check, the
//! typed messages are what quiverd parses, and `quiver_perf::descriptor` validates metrics.
#![warn(clippy::all)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use quiver_perf::control::PerfWelcome;
use quiver_perf::descriptor::limits as perf_limits;

/// Message types that parse as `quiver_perf::control::PerfMessage` rather than [`FromMod`] /
/// [`ToMod`].
pub fn is_perf_message(t: &str) -> bool {
    matches!(t, "metrics" | "metrics_ok" | "m" | "mark" | "ring_req" | "ring" | "perf_ctl")
}

/// The protocol this side speaks.
pub const PROTOCOL: u32 = 2;
/// Largest frame a mod may send (the bridge closes the pipe on a larger one). A schema too
/// big for this ships as `quiver-options.toml` instead of inline in `hello`.
pub const MAX_FRAME: usize = 64 * 1024;
/// Largest line quiverd sends the bridge (a `welcome` snapshot can be big).
pub const MAX_LINE: usize = 256 * 1024;
/// Largest frame the bridge forwards to a mod (= what the SDK accepts).
pub const MAX_FRAME_TO_MOD: usize = 256 * 1024;
/// `t` values are short identifiers.
pub const MAX_TYPE_LEN: usize = 32;
/// Option and action ids: `world.npc_density`.
pub const MAX_OPTION_ID: usize = 64;
/// Options a mod may declare it handles live.
pub const MAX_OPTIONS: usize = 256;
/// An option value as its file would hold it.
pub const MAX_OPTION_TEXT: usize = 4096;
/// A `log` line.
pub const MAX_LOG_TEXT: usize = 1024;
/// Capabilities quiverd knows. A mod lists what it wants in `hello.caps`; `welcome.caps` is
/// the intersection.
pub const CAPS: &[&str] = &["options", "metrics", "actions", "log"];

/// The pipe name for a session, as the bridge creates it and `QUIVER_MODS_PIPE` names it.
pub fn pipe_name(session: Option<&str>) -> String {
    match session.filter(|s| is_session_id(s)) {
        Some(s) => format!(r"\\.\pipe\quiver-mods-{s}"),
        None => r"\\.\pipe\quiver-mods".to_owned(),
    }
}

/// `s-4kq2` style ids: short, `[A-Za-z0-9_-]`, safe inside a pipe name and a path.
pub fn is_session_id(s: &str) -> bool {
    (1..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A surface id (the schema's `surface`, the metric namespace): `aco-dlss`.
pub fn is_surface_id(s: &str) -> bool {
    (1..=48).contains(&s.len())
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// An option or action id: `world.npc_density`, `fg.enabled`.
pub fn is_option_id(s: &str) -> bool {
    (1..=MAX_OPTION_ID).contains(&s.len())
        && s.split('.').all(|seg| {
            !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

/// Frame bytes for the pipe: little-endian length, then the body.
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Why the bridge dropped a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameReject {
    NotJson(String),
    NotObject,
    NoType,
    BadType,
}

impl std::fmt::Display for FrameReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson(e) => write!(f, "not UTF-8 JSON: {e}"),
            Self::NotObject => f.write_str("not a JSON object"),
            Self::NoType => f.write_str("no `t` string"),
            Self::BadType => f.write_str("`t` is not a short identifier"),
        }
    }
}

/// The bridge's structural check of one frame body: UTF-8 JSON, an object, a short
/// `[a-z_0-9]` `t`. Returns the type and the parsed object, re-serialized compactly by the
/// caller, so what quiverd reads is always one well-formed line. (serde_json limits nesting
/// to 128 levels; size is limited by the frame.)
pub fn check_frame(body: &[u8]) -> Result<(String, serde_json::Value), FrameReject> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| FrameReject::NotJson(e.to_string()))?;
    let obj = v.as_object().ok_or(FrameReject::NotObject)?;
    let t = obj.get("t").and_then(|t| t.as_str()).ok_or(FrameReject::NoType)?;
    if t.is_empty() || t.len() > MAX_TYPE_LEN || !t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(FrameReject::BadType);
    }
    Ok((t.to_owned(), v))
}

/// One option's value: `text` exactly as the mod's file would hold it (`"1"`, `"d3d12"`,
/// `"F9"`), `json` the typed value (`true`, `4`, `"d3d12"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionValue {
    pub text: String,
    #[serde(default)]
    pub json: serde_json::Value,
}

pub type Values = BTreeMap<String, OptionValue>;

// ---------------------------------------------------------------------------------------
// mod -> quiverd

/// A message from a mod.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromMod {
    Hello(Hello),
    /// The live options this connection applies (`quiver_handle_option`). Replaces the
    /// previous set. Options not listed are saved and apply on the mod's next start (or via
    /// the file channel).
    Options {
        #[serde(default)]
        ids: Vec<String>,
    },
    /// The mod applied (or rejected) a `set`.
    Ack {
        seq: u64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        rejected: BTreeMap<String, String>,
    },
    /// The mod changed values itself: its own menu (`persist`) or runtime state such as a
    /// hotkey flip (not persisted).
    Changed {
        values: BTreeMap<String, ChangedValue>,
        #[serde(default)]
        persist: bool,
    },
    /// Protocol 1: display values, mapped to `text` metrics named after their ids.
    Readout { values: BTreeMap<String, String> },
    Log {
        #[serde(default = "info_level")]
        level: i32,
        text: String,
    },
    Bye,
    /// A message from a newer SDK: ignored.
    #[serde(other)]
    Unknown,
}

fn info_level() -> i32 {
    2
}

/// `changed` values: the text, and the typed value if the mod has one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChangedValue {
    Text(String),
    Full(OptionValue),
}

impl ChangedValue {
    pub fn text(&self) -> &str {
        match self {
            Self::Text(t) => t,
            Self::Full(v) => &v.text,
        }
    }
}

/// The first message on a connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    /// The SDK's own version string (`quiver_mod.h 2.0`).
    #[serde(default)]
    pub sdk: String,
    /// The schema's surface id, which is also the metric namespace.
    #[serde(rename = "mod")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The mod's own version, free text (`v5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The module the SDK lives in (`d3d11.dll`), for status lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Inline schema (TOML or JSON); `None`: the version's shipped `quiver-options.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// What the mod wants: [`CAPS`] (protocol 2; protocol 1 SDKs send none and get options).
    #[serde(default)]
    pub caps: Vec<String>,
}

// ---------------------------------------------------------------------------------------
// quiverd -> mod

/// A message to a mod.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToMod {
    Welcome(Welcome),
    /// Someone changed values: apply them, then `ack` with the same `seq`.
    Set { seq: u64, values: Values },
    /// Run an action the mod declared (`[action.<id>]`).
    Action {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arg: Option<String>,
    },
    Bye {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    /// `min(hello.proto, PROTOCOL)`.
    pub proto: u32,
    /// `QUIVER_SESSION`, when the launch has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The mod profile the game runs (`None`: unknown or vanilla).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The surface's effective option values (full snapshot).
    #[serde(default)]
    pub values: Values,
    /// The capabilities granted: hello's `caps` that quiverd supports.
    #[serde(default)]
    pub caps: Vec<String>,
    /// Where rings live and the clock (SIGHT-HUD §3.6). Absent while Quiver offers no rings
    /// (phase 1): metrics then travel as JSON only, timestamps in QPC ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<PerfWelcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
}

/// What quiverd enforces, so an SDK can stay under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub metrics: u32,
    pub m_per_s: u32,
    pub marks_per_s: u32,
    pub text_bytes: u32,
    pub frame_bytes: u32,
}

impl Limits {
    pub fn current() -> Self {
        Self {
            metrics: perf_limits::MAX_METRICS_PER_PRODUCER as u32,
            m_per_s: perf_limits::MAX_JSON_HZ,
            marks_per_s: perf_limits::MAX_MARKS_PER_SEC,
            text_bytes: perf_limits::MAX_TEXT_BYTES as u32,
            frame_bytes: MAX_FRAME as u32,
        }
    }
}

/// Make a mod's string safe to show and store: valid UTF-8 already (serde), with control
/// characters and bidi overrides removed and at most `max` bytes, cut on a char boundary.
pub fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    for c in s.chars() {
        let bidi = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}');
        if c.is_control() || bidi {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_are_checked() {
        assert_eq!(encode_frame(b"{}")[..4], [2, 0, 0, 0]);
        let (t, _) = check_frame(br#"{"t":"hello","proto":2,"mod":"x"}"#).unwrap();
        assert_eq!(t, "hello");
        assert!(matches!(check_frame(b"\xff\xfe"), Err(FrameReject::NotJson(_))));
        assert_eq!(check_frame(b"[1]"), Err(FrameReject::NotObject));
        assert_eq!(check_frame(br#"{"x":1}"#), Err(FrameReject::NoType));
        assert_eq!(check_frame(br#"{"t":"He llo"}"#), Err(FrameReject::BadType));
        assert_eq!(check_frame(format!(r#"{{"t":"{}"}}"#, "a".repeat(33)).as_bytes()), Err(FrameReject::BadType));
        // Deep nesting is refused by serde_json's recursion limit, not a stack overflow.
        let deep = format!(r#"{{"t":"m","v":{}{}}}"#, "[".repeat(10_000), "]".repeat(10_000));
        assert!(matches!(check_frame(deep.as_bytes()), Err(FrameReject::NotJson(_))));
    }

    #[test]
    fn messages_parse() {
        let h: FromMod = serde_json::from_value(json!({"t":"hello","proto":2,"sdk":"quiver_mod.h 2.0","mod":"aco-dlss","pid":42,"caps":["metrics","options"],"extra":1})).unwrap();
        let FromMod::Hello(h) = h else { panic!() };
        assert_eq!((h.proto, h.surface.as_str(), h.pid), (2, "aco-dlss", Some(42)));
        // Unknown messages from newer SDKs are ignored, not errors.
        assert_eq!(serde_json::from_value::<FromMod>(json!({"t":"ring_req","bytes":1024})).unwrap(), FromMod::Unknown);
        // Perf messages are quiver-perf's; this enum sees them as unknown.
        assert!(is_perf_message("m") && is_perf_message("metrics") && !is_perf_message("hello"));
        assert_eq!(serde_json::from_value::<FromMod>(json!({"t":"m","ts":5,"v":{"0":1.5}})).unwrap(), FromMod::Unknown);
        let c: FromMod = serde_json::from_value(json!({"t":"changed","values":{"fg.enabled":"0","x":{"text":"1","json":true}},"persist":false})).unwrap();
        let FromMod::Changed { values, persist: false } = c else { panic!() };
        assert_eq!(values["fg.enabled"].text(), "0");
        assert_eq!(values["x"].text(), "1");
        let w = serde_json::to_value(ToMod::Set { seq: 3, values: Values::new() }).unwrap();
        assert_eq!(w, json!({"t":"set","seq":3,"values":{}}));
        assert_eq!(serde_json::from_value::<FromMod>(json!({"t":"bye"})).unwrap(), FromMod::Bye);
    }

    #[test]
    fn ids_and_strings() {
        assert_eq!(pipe_name(Some("s-4kq2")), r"\\.\pipe\quiver-mods-s-4kq2");
        assert_eq!(pipe_name(Some(r"..\x")), r"\\.\pipe\quiver-mods");
        assert!(is_surface_id("aco-dlss") && !is_surface_id("Aco") && !is_surface_id("1x") && !is_surface_id(""));
        assert!(is_option_id("world.npc_density") && !is_option_id("a..b") && !is_option_id("a b"));
        assert_eq!(sanitize("a\u{202e}b\nc\u{7}", 48), "abc");
        assert_eq!(sanitize("ééé", 5), "éé");
    }
}
