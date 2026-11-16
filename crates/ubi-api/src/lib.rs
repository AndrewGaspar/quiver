//! The `ubid` daemon's protocol and a client for it.
//!
//! Transport: a Unix socket at `$XDG_RUNTIME_DIR/quiver/quiverd.sock`, one JSON
//! object per line. Requests carry an `id`, the caller's `source` (gui/cli/agent/…, kept
//! in the settings history and activity log) and a typed [`Request`]. The daemon answers
//! each with a [`Reply`]; after [`Request::Subscribe`] the connection also receives
//! [`Event`]s.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod client;
pub mod downloads;
pub mod exports;
pub mod mods;
pub mod info;
pub mod setup;
pub use client::Client;
pub use mods::{ModsRequest, Profile};

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
    /// Launch account (user id); `None`: the default account if it owns the game, else
    /// the first account that does. Also decides whose cloud saves are used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl GameSettings {
    /// Apply one edit. Keys: `proton`, `language`, `account` (a user id; the daemon
    /// resolves names), `args` (whitespace-separated), `wrappers` (comma/space-separated),
    /// `env.NAME`, `dll.NAME`.
    pub fn apply(&mut self, op: &SettingOp) -> Result<(), String> {
        let (key, value) = match op {
            SettingOp::Set { key, value } => (key.as_str(), Some(value.as_str())),
            SettingOp::Unset { key } => (key.as_str(), None),
        };
        let opt = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        match key {
            "proton" => self.proton = opt(value),
            "language" => self.language = opt(value),
            "account" => self.account = opt(value),
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
                    return Err(format!("unknown setting '{k}' (proton, language, account, args, wrappers, env.NAME, dll.NAME)"));
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

    /// These settings with `over` (a mod profile's overrides) on top: values `over` sets
    /// win, `args` are appended, `env` and `dll` entries are merged, and non-empty
    /// `wrappers` replace the game's.
    pub fn merged(&self, over: &GameSettings) -> GameSettings {
        let mut out = self.clone();
        if over.proton.is_some() {
            out.proton = over.proton.clone();
        }
        if over.language.is_some() {
            out.language = over.language.clone();
        }
        if over.account.is_some() {
            out.account = over.account.clone();
        }
        out.args.extend(over.args.iter().cloned());
        if !over.wrappers.is_empty() {
            out.wrappers = over.wrappers.clone();
        }
        out.env.extend(over.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        out.dll_overrides.extend(over.dll_overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
        out
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
    /// User ids of the signed-in accounts that own it, default account first.
    #[serde(default)]
    pub owned_by: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Art {
    pub thumb: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
    /// `icon_image` (an .ico), used for .desktop entries and Steam shortcuts.
    #[serde(default)]
    pub icon: Option<String>,
}

/// A signed-in Ubisoft account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub user_id: String,
    /// Ubisoft username.
    pub name: String,
    /// The default account (launches games every account owns, unless a game overrides it).
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    /// The default account's name.
    pub signed_in: Option<String>,
    pub awaiting_2fa: bool,
    pub running: Vec<u32>,
    pub jobs: Vec<Job>,
    pub proton_default: Option<String>,
    /// `host:port` of the Denuvo/ticket broker that game bridges connect to.
    #[serde(default)]
    pub broker: String,
    /// Every signed-in account, in sign-in order.
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Install,
    Import,
    Play,
    /// Hashing a game's files against its manifest (repairs run as `Install` jobs).
    Verify,
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

/// A game currently being played, as tracked by the daemon (from any launch path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSession {
    pub product_id: u32,
    /// User id of the account playing.
    #[serde(default)]
    pub account: String,
    pub started: DateTime<Utc>,
    /// Who holds the session open: `launcher` (games quiverd started) and/or `bridge`
    /// (any game talking to a Quiver bridge, e.g. containers).
    pub holders: Vec<String>,
    /// Whether Ubisoft accepted the session (play time is being recorded upstream).
    pub reported: bool,
}

/// One play session recorded by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaySession {
    pub started: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub seconds: u64,
    #[serde(default)]
    pub settings_revision: u32,
    /// Mod profile id (`None`: vanilla) and its revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_revision: Option<u32>,
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
    /// Sign in, adding the account (the first one becomes the default).
    Login { email: String, password: String },
    Login2fa { code: String },
    /// Sign one account out (`None`: the default). The next account becomes the default.
    /// `account` is a user id, username or unique user-id prefix.
    Logout {
        #[serde(default)]
        account: Option<String>,
    },
    /// Owned, installable games with install state.
    Library { refresh: bool },
    Install { product_id: u32, language: Option<String> },
    /// Adopt an existing install (Lutris/Connect) by reflink-copying its manifest files.
    Import { product_id: u32, path: PathBuf, language: Option<String> },
    Uninstall { product_id: u32, remove_prefix: bool },
    /// Launch a game; `profile` (mod profile name or id; `None` or `vanilla`: unmodded).
    Play {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    Stop { product_id: u32 },
    /// Cancel a job. Installs stop and can be resumed by installing again; `discard`
    /// also deletes the partial files (only for games that weren't installed before).
    /// Launches stop the game.
    Cancel { job_id: u64, discard: bool },
    SettingsGet { product_id: u32 },
    SettingsSet { product_id: u32, ops: Vec<SettingOp>, message: Option<String> },
    SettingsHistory { product_id: u32 },
    SettingsRestore { product_id: u32, revision: u32 },
    /// A launch context for running a game's bridge outside this launcher (e.g. in a
    /// container): account, fresh ticket, owned ids, CD keys, broker address.
    /// `account` (user id or name) overrides the game's launch account.
    LaunchContext {
        product_id: u32,
        savegame_path: Option<String>,
        install_dir: Option<PathBuf>,
        #[serde(default)]
        account: Option<String>,
    },
    /// Installed Proton builds.
    ProtonList,
    /// Play sessions recorded by this launcher.
    Playtime { product_id: u32 },
    /// Ubisoft's own play-time record for a game (read-only).
    RemotePlaytime { product_id: u32 },
    Activity { limit: usize },
    /// Games being played right now, from every launch path.
    Sessions,
    Subscribe,
    Shutdown,
    /// Make an account (user id, username or unique user-id prefix) the default.
    AccountDefault { account: String },
    /// Exports Quiver wrote, with whether each is still as written (`Vec<ExportStatus>`).
    ExportList,
    /// Write (or with `remove`, delete) a game's .desktop entry.
    ExportDesktop { product_id: u32, remove: bool },
    /// Add (or remove) a game's Steam shortcut. Staged if Steam is running. `steam_user`
    /// (id or name) is required when there are several Steam users and none was chosen
    /// before; a given user is remembered.
    ExportSteam { product_id: u32, remove: bool, steam_user: Option<String> },
    /// Steam's users on this machine (`exports::SteamUsers`).
    SteamUsers,
    /// Write staged shortcut changes now (fails while Steam is running). Never restarts Steam.
    SteamApplyStaged,
    /// Mod layers and profiles (see [`mods`]).
    Mods(ModsRequest),
    /// The download queue in order (`Vec<downloads::QueueEntry>`).
    Queue,
    /// Move a game's queue item up, down or to the top.
    QueueMove { product_id: u32, to: downloads::QueueMove },
    /// Pause a game's download (it stops at a slice boundary and resumes where it was).
    QueuePause { product_id: u32 },
    /// Resume a paused item, or retry a failed one.
    QueueResume { product_id: u32 },
    /// Take a game out of the queue, stopping it if it is running. Downloaded files stay,
    /// so installing again resumes.
    QueueRemove { product_id: u32 },
    /// Sizes, free space and languages for installing (or updating) a game, without
    /// starting it (`downloads::InstallPreview`).
    InstallPreview { product_id: u32, language: Option<String> },
    /// What updating an installed game would change (`downloads::UpdatePreview`).
    UpdatePreview { product_id: u32 },
    /// Queue an update for an installed game. Returns the job id.
    Update { product_id: u32 },
    /// Hash a game's installed files against its manifest. With `repair`, queue
    /// re-downloading only what is missing or corrupt. Returns the job id.
    Verify { product_id: u32, repair: bool },
    /// The last verify report for a game (`Option<downloads::VerifyReport>`).
    VerifyResult { product_id: u32 },
    /// Read download settings, changing the ones given (`bandwidth_limit: 0` = unlimited).
    /// Returns `downloads::DownloadSettings`.
    DownloadSettings {
        #[serde(default)]
        max_active: Option<usize>,
        #[serde(default)]
        bandwidth_limit: Option<u64>,
        #[serde(default)]
        auto_update: Option<bool>,
    },
    /// A game's detail (`info::GameInfo`): product-config facts, add-ons owned or not (with
    /// store links), install size and languages. `refresh` re-reads ownership from Ubisoft.
    GameInfo {
        product_id: u32,
        #[serde(default)]
        refresh: bool,
    },
    /// A game's Ubisoft progression and the account's Ubisoft Connect level and units
    /// (`info::Progress`), for the game's launch account. Cached for a few minutes.
    GameProgress {
        product_id: u32,
        #[serde(default)]
        refresh: bool,
    },
    /// A game's save slots, local and cloud side by side, and its local backups
    /// (`info::SavesReport`). `cloud: false` lists only what's on this machine.
    Saves {
        product_id: u32,
        #[serde(default = "yes")]
        cloud: bool,
    },
    /// Make a save backup current (`info::RestoreResult`); the saves there now are backed
    /// up first. `backup` is a backup id, a unique part of one, or `latest`. Agents need a
    /// human's approval: only a front end that asked the person sets `confirmed`.
    SavesRestore {
        product_id: u32,
        backup: String,
        #[serde(default)]
        confirmed: bool,
    },
    /// Upload a game's saves that are waiting for the cloud (slots with `.upload` markers),
    /// without launching it: e.g. after an upload failed at exit. `profile` picks a mod
    /// profile's saves (name or id; it must have cloud saves on). Returns the uploaded slots.
    SavesSync {
        product_id: u32,
        #[serde(default)]
        profile: Option<String>,
    },
    /// First-run state and storage paths (`setup::SetupState`). `library` / `prefixes`
    /// change where games and prefixes go; that is refused once anything is installed,
    /// queued or importing, and quiverd restarts to use them (clients reconnect on their
    /// own). `finish` records that setup was done (or skipped).
    Setup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        library: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefixes: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish: Option<setup::FinishSetup>,
    },
    /// Find existing Ubisoft Connect installs in Lutris, Wine and Steam prefixes, matched
    /// to owned games (`Vec<setup::FoundInstall>`). Read-only; import one with `Import`.
    ScanInstalls,
}

fn yes() -> bool {
    true
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
    /// The signed-in accounts or the default changed; `signed_in` is the default's name.
    SessionChanged { signed_in: Option<String> },
    LibraryChanged,
    /// The set of games being played changed (see `Request::Sessions`).
    SessionsChanged { sessions: Vec<ActiveSession> },
    /// A game's mod layers or profiles changed.
    ModsChanged { product_id: u32 },
    /// Not sent by quiverd: [`Client`] reports that the daemon went away (it is
    /// reconnecting; events are missed until [`Event::Reconnected`]).
    Disconnected,
    /// Not sent by quiverd: [`Client`] is connected and subscribed again after
    /// [`Event::Disconnected`]. The daemon may have restarted; reload state.
    Reconnected,
}

/// Socket path for the current user.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("quiver").join("quiverd.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("quiver-{user}")).join("quiverd.sock")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(s: &mut GameSettings, op: &str) -> Result<(), String> {
        s.apply(&SettingOp::parse(op)?)
    }

    #[test]
    fn account_setting() {
        let mut s = GameSettings::default();
        apply(&mut s, "account=123-abc").unwrap();
        assert_eq!(s.account.as_deref(), Some("123-abc"));
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::json!({ "account": "123-abc" }));
        apply(&mut s, "account=").unwrap();
        assert_eq!(s.account, None);
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::json!({}));
        // Older revisions have no account key.
        let old: GameSettings = serde_json::from_str(r#"{"proton":"GE"}"#).unwrap();
        assert_eq!(old.account, None);
    }

    #[test]
    fn merged_settings() {
        let mut game = GameSettings::default();
        for op in ["proton=GE-1", "args=-a", "env.A=1", "env.B=2", "dll.d3d11=n", "wrappers=gamemoderun"] {
            apply(&mut game, op).unwrap();
        }
        assert_eq!(game.merged(&GameSettings::default()), game);
        let mut over = GameSettings::default();
        for op in ["args=-b", "env.B=3", "dll.dxgi=n,b"] {
            apply(&mut over, op).unwrap();
        }
        let m = game.merged(&over);
        assert_eq!(m.proton.as_deref(), Some("GE-1"));
        assert_eq!(m.args, ["-a", "-b"]);
        assert_eq!(m.env["A"], "1");
        assert_eq!(m.env["B"], "3");
        assert_eq!(m.dll_overrides.len(), 2);
        assert_eq!(m.wrappers, ["gamemoderun"]);
        apply(&mut over, "proton=GE-2").unwrap();
        apply(&mut over, "wrappers=mangohud").unwrap();
        let m = game.merged(&over);
        assert_eq!(m.proton.as_deref(), Some("GE-2"));
        assert_eq!(m.wrappers, ["mangohud"]);
    }

    #[test]
    fn setting_parse() {
        assert_eq!(SettingOp::parse("a=b=c").unwrap(), SettingOp::Set { key: "a".into(), value: "b=c".into() });
        assert_eq!(SettingOp::parse("account=").unwrap(), SettingOp::Unset { key: "account".into() });
        assert!(SettingOp::parse("account").is_err());
        let mut s = GameSettings::default();
        assert!(apply(&mut s, "acount=x").unwrap_err().contains("account"));
        apply(&mut s, "wrappers=gamemoderun, mangohud").unwrap();
        assert_eq!(s.wrappers, ["gamemoderun", "mangohud"]);
        apply(&mut s, "env.DXVK_HUD=fps").unwrap();
        apply(&mut s, "env.DXVK_HUD=").unwrap();
        assert!(s.env.is_empty());
        assert!(apply(&mut s, "dll.=n").is_err());
    }

    #[test]
    fn info_requests_wire() {
        let r: Request = serde_json::from_str(r#"{"method":"game_info","params":{"product_id":3539}}"#).unwrap();
        assert!(matches!(r, Request::GameInfo { product_id: 3539, refresh: false }));
        let r: Request = serde_json::from_str(r#"{"method":"game_progress","params":{"product_id":1,"refresh":true}}"#).unwrap();
        assert!(matches!(r, Request::GameProgress { refresh: true, .. }));
        // The cloud is checked unless asked not to.
        let r: Request = serde_json::from_str(r#"{"method":"saves","params":{"product_id":1}}"#).unwrap();
        assert!(matches!(r, Request::Saves { cloud: true, .. }));
        let r: Request = serde_json::from_str(r#"{"method":"saves_restore","params":{"product_id":1,"backup":"latest"}}"#).unwrap();
        assert!(matches!(r, Request::SavesRestore { confirmed: false, .. }));
        // Results decode from minimal objects (older or newer daemons).
        let i: info::GameInfo = serde_json::from_str(r#"{"product_id":1,"name":"x","future":1}"#).unwrap();
        assert!(i.addons.is_empty() && i.install.is_none());
        let a: info::AddOn = serde_json::from_str(r#"{"product_id":2,"kind":"some_new_kind"}"#).unwrap();
        assert_eq!(a.kind, info::ProductKind::Unknown);
        let p: info::Progress = serde_json::from_str(r#"{"product_id":1,"fetched":"2026-11-16T00:00:00Z"}"#).unwrap();
        assert!(p.stats.is_empty() && p.connect.is_none());
        let s: info::SavesReport = serde_json::from_str(r#"{"product_id":1,"slots":[{"slot":1,"state":"brand_new"}]}"#).unwrap();
        assert_eq!(s.slots[0].state, info::SlotState::Unknown);
        assert_eq!(serde_json::to_value(info::SlotState::CloudNewer).unwrap(), "cloud_newer");
        assert_eq!(serde_json::to_value(info::ProductKind::SeasonPass).unwrap(), "season_pass");
    }

    #[test]
    fn setup_requests_wire() {
        let r: Request = serde_json::from_str(r#"{"method":"setup","params":{}}"#).unwrap();
        assert!(matches!(r, Request::Setup { library: None, prefixes: None, finish: None }));
        let r: Request = serde_json::from_str(r#"{"method":"scan_installs"}"#).unwrap();
        assert!(matches!(r, Request::ScanInstalls));
        let v = serde_json::to_value(Request::Setup { library: None, prefixes: None, finish: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "setup", "params": {} }));
    }

    #[test]
    fn wire_compat() {
        let r: Request = serde_json::from_str(r#"{"method":"logout","params":{}}"#).unwrap();
        assert!(matches!(r, Request::Logout { account: None }));
        let r: Request = serde_json::from_str(r#"{"method":"logout","params":{"account":"fam"}}"#).unwrap();
        assert!(matches!(r, Request::Logout { account: Some(a) } if a == "fam"));
        let r: Request = serde_json::from_str(r#"{"method":"account_default","params":{"account":"fam"}}"#).unwrap();
        assert!(matches!(r, Request::AccountDefault { account } if account == "fam"));
        let r: Request = serde_json::from_str(
            r#"{"method":"launch_context","params":{"product_id":1,"savegame_path":null,"install_dir":null}}"#,
        )
        .unwrap();
        assert!(matches!(r, Request::LaunchContext { account: None, .. }));
        // A daemon from before multiple accounts.
        let s: Status = serde_json::from_str(
            r#"{"version":"0","signed_in":"me","awaiting_2fa":false,"running":[],"jobs":[],"proton_default":null}"#,
        )
        .unwrap();
        assert!(s.accounts.is_empty());
        let e: LibraryEntry = serde_json::from_str(
            r#"{"product_id":1,"name":"x","installed":null,"update_available":false,"running":false}"#,
        )
        .unwrap();
        assert!(e.owned_by.is_empty());
    }
}
