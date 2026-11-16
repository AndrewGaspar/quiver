//! The `ubid` daemon's protocol and a client for it.
//!
//! Transport: a Unix socket at `$XDG_RUNTIME_DIR/ubisoft-linux/ubid.sock`, one JSON
//! object per line. Requests carry an `id`, the caller's `source` (gui/cli/agent/…, kept
//! in the settings history and activity log) and a typed [`Request`]. The daemon answers
//! each with a [`Reply`]; after [`Request::Subscribe`] the connection also receives
//! [`Event`]s.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod client;
pub use client::Client;

// ---------------------------------------------------------------------------------------
// Model

/// An installed game managed by the launcher.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Game {
    pub product_id: u32,
    pub name: String,
    pub install_dir: PathBuf,
    pub prefix: PathBuf,
    /// Executable relative to `install_dir`, and its default arguments (product config).
    pub exe: String,
    #[serde(default)]
    pub args: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

/// Per-game launch settings. Every change creates a new revision in the game's history.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GameSettings {
    /// Proton build (directory name under compatibilitytools.d); `None` = global default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proton: Option<String>,
    /// Extra arguments appended after the game's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Commands the game is wrapped in, outermost first (e.g. `gamemoderun`, `mangohud`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrappers: Vec<String>,
    /// Wine DLL overrides (`d3d11` -> `n,b`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dll_overrides: BTreeMap<String, String>,
    /// Install/audio language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl GameSettings {
    /// Apply one edit. Keys: `proton`, `language`, `args` (whitespace-separated),
    /// `wrappers` (comma/space-separated), `env.NAME`, `dll.NAME`.
    pub fn apply(&mut self, op: &SettingOp) -> Result<(), String> {
        let (key, value) = match op {
            SettingOp::Set { key, value } => (key.as_str(), Some(value.as_str())),
            SettingOp::Unset { key } => (key.as_str(), None),
        };
        let opt = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        match key {
            "proton" => self.proton = opt(value),
            "language" => self.language = opt(value),
            "args" => self.args = value.map(|v| v.split_whitespace().map(str::to_owned).collect()).unwrap_or_default(),
            "wrappers" => {
                self.wrappers = value
                    .map(|v| v.split([',', ' ']).filter(|s| !s.is_empty()).map(str::to_owned).collect())
                    .unwrap_or_default()
            }
            k => {
                let (map, name) = if let Some(n) = k.strip_prefix("env.") {
                    (&mut self.env, n)
                } else if let Some(n) = k.strip_prefix("dll.") {
                    (&mut self.dll_overrides, n)
                } else {
                    return Err(format!("unknown setting '{k}' (proton, language, args, wrappers, env.NAME, dll.NAME)"));
                };
                if name.is_empty() {
                    return Err(format!("missing name in '{k}'"));
                }
                match value {
                    Some(v) => map.insert(name.to_owned(), v.to_owned()),
                    None => map.remove(name),
                };
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum SettingOp {
    Set { key: String, value: String },
    Unset { key: String },
}

impl SettingOp {
    /// Parse CLI syntax: `key=value` sets, `key=` unsets.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.split_once('=') {
            Some((k, "")) => Ok(Self::Unset { key: k.to_owned() }),
            Some((k, v)) => Ok(Self::Set { key: k.to_owned(), value: v.to_owned() }),
            None => Err(format!("expected key=value, got '{s}'")),
        }
    }
}

/// One entry in a game's settings history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub revision: u32,
    pub at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub message: Option<String>,
    pub settings: GameSettings,
}

/// A product in the library, with its local install state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub product_id: u32,
    pub name: String,
    pub installed: Option<Game>,
    pub update_available: bool,
    pub running: bool,
    /// Art file names from the product config (served by Ubisoft's asset CDN).
    #[serde(default)]
    pub art: Art,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Art {
    pub thumb: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub signed_in: Option<String>,
    pub awaiting_2fa: bool,
    pub running: Vec<u32>,
    pub jobs: Vec<Job>,
    pub proton_default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Install,
    Import,
    Play,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub product_id: u32,
    pub name: String,
    pub status: String,
    pub downloaded: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
    /// Extra result lines (e.g. files an import didn't copy).
    #[serde(default)]
    pub detail: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginResult {
    SignedIn { name: String },
    TwoFactorRequired,
}

/// One play session recorded by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaySession {
    pub started: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub seconds: u64,
    #[serde(default)]
    pub settings_revision: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Playtime {
    pub total_seconds: u64,
    pub last_played: Option<DateTime<Utc>>,
    pub sessions: Vec<PlaySession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub at: DateTime<Utc>,
    pub source: String,
    pub action: String,
    #[serde(default)]
    pub product_id: Option<u32>,
}

// ---------------------------------------------------------------------------------------
// Wire

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Status,
    Login { email: String, password: String },
    Login2fa { code: String },
    Logout,
    /// Owned, installable games with install state.
    Library { refresh: bool },
    Install { product_id: u32, language: Option<String> },
    /// Adopt an existing install (Lutris/Connect) by reflink-copying its manifest files.
    Import { product_id: u32, path: PathBuf, language: Option<String> },
    Uninstall { product_id: u32, remove_prefix: bool },
    Play { product_id: u32 },
    Stop { product_id: u32 },
    SettingsGet { product_id: u32 },
    SettingsSet { product_id: u32, ops: Vec<SettingOp>, message: Option<String> },
    SettingsHistory { product_id: u32 },
    SettingsRestore { product_id: u32, revision: u32 },
    /// Installed Proton builds.
    ProtonList,
    /// Play sessions recorded by this launcher.
    Playtime { product_id: u32 },
    Activity { limit: usize },
    Subscribe,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    pub source: String,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok { id: u64, result: serde_json::Value },
    Err { id: u64, error: String },
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Job(Job),
    GameStarted { product_id: u32 },
    GameStopped { product_id: u32 },
    SettingsChanged { product_id: u32, revision: u32, source: String },
    SessionChanged { signed_in: Option<String> },
    LibraryChanged,
}

/// Socket path for the current user.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("ubisoft-linux").join("ubid.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("ubisoft-linux-{user}")).join("ubid.sock")
        }
    }
}
