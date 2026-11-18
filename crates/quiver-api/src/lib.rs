//! The `quiverd` daemon's protocol and a client for it.
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

pub mod achievements;
pub mod art;
pub mod client;
pub mod daemon;
pub mod downloads;
pub mod epic;
pub mod epicgame;
pub mod exports;
pub mod game;
pub mod mods;
pub mod info;
pub mod options;
pub mod perf;
pub mod savestores;
pub mod setup;
pub mod sight;
pub mod steamlib;
pub mod tweaks;
pub mod stores;
pub use client::Client;
pub use game::{GameId, GameRef, StoreId};
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
    /// The game when it isn't a Ubisoft product (`steam:620`: a Steam game as mods see it,
    /// with Steam's install folder and, for a Proton game, Steam's prefix; `product_id` is
    /// then 0). Never written for Ubisoft games, so `games.json` is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
}

impl Game {
    /// The game's ref: [`Game::game`], else the Ubisoft product.
    pub fn game_ref(&self) -> GameRef {
        self.game.unwrap_or(GameRef::ubisoft(self.product_id))
    }

    /// The Wine prefix, if the game has one (a native Steam game has none: `prefix` is
    /// empty).
    pub fn prefix(&self) -> Option<&std::path::Path> {
        (!self.prefix.as_os_str().is_empty()).then_some(self.prefix.as_path())
    }
}

/// Reply to `Request::Uninstall`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UninstallResult {
    pub name: String,
    /// The install folder (its files are gone).
    pub dir: PathBuf,
    /// The game's compatibility folder (Wine prefix).
    #[serde(default)]
    pub prefix: PathBuf,
    /// The prefix was deleted too.
    #[serde(default)]
    pub prefix_removed: bool,
    /// Quiver's exports of the game, removed with it: "the .desktop entry", "the Steam
    /// shortcut", "the Steam shortcut (when Steam restarts)".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports_removed: Vec<String>,
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
    /// The mod profile Play uses by default (a profile id; the daemon resolves names);
    /// `None`: vanilla. A launch that names no profile uses it ([`Request::Play`]), so the
    /// app's Play button, `quiver play`, .desktop entries and Steam shortcuts follow it; one
    /// that doesn't exist any more (deleted) launches vanilla, with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Whose art the app shows for the game: `steam` puts Steam's art (for games with a
    /// Steam install) ahead of Ubisoft's; `None`/`ubisoft`: Ubisoft's first (see [`art`]).
    /// Display only; launches ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    /// Reporting the achievements the game earns to Ubisoft (see [`achievements`]):
    /// `None` (on, the default), `off` (journaled locally only) or `dry-run` (journaled,
    /// and the write Quiver would send is logged instead of sent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievements: Option<String>,
    /// The mod profile a launch from Steam runs (`quiver steam-run`; a profile id, the daemon
    /// resolves names); `None`: vanilla. Steam's Play has no profile picker, so this is what
    /// it runs; `steam-run --profile` overrides it for one launch (design/STEAM-COPIES.md §4,
    /// STORE-STEAM.md §10). For a Steam game, `profile` sets this too (its only launch is
    /// Steam's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_profile: Option<String>,
}

impl GameSettings {
    /// Apply one edit. Keys: `proton`, `language`, `account` (a user id; the daemon
    /// resolves names), `profile` (the default mod profile's id; `vanilla` or empty: none;
    /// the daemon resolves names), `steam_profile` (the same for launches from Steam), `art` (`steam` or `ubisoft`), `achievements` (`on`,
    /// `off` or `dry-run`), `args` (whitespace-separated), `wrappers` (comma/space-separated),
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
            "profile" => self.profile = opt(value).filter(|v| !v.trim().eq_ignore_ascii_case(mods::VANILLA)),
            "steam_profile" => self.steam_profile = opt(value).filter(|v| !v.trim().eq_ignore_ascii_case(mods::VANILLA)),
            "art" => {
                self.art = match opt(value).map(|v| v.trim().to_ascii_lowercase()) {
                    None => None,
                    Some(v) if v == "ubisoft" => None,
                    Some(v) if v == "steam" => Some(v),
                    Some(v) => return Err(format!("art: expected steam or ubisoft, got '{v}'")),
                }
            }
            "achievements" => {
                self.achievements = match opt(value).map(|v| v.trim().to_ascii_lowercase().replace('_', "-")) {
                    None => None,
                    Some(v) if v == "on" => None,
                    Some(v) if v == "off" || v == "dry-run" => Some(v),
                    Some(v) => return Err(format!("achievements: expected on, off or dry-run, got '{v}'")),
                }
            }
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
                    return Err(format!("unknown setting '{k}' (proton, language, account, profile, art, achievements, args, wrappers, env.NAME, dll.NAME)"));
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
    /// `wrappers` replace the game's. `profile` (per game) is kept from `self`.
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

/// A game in the library, with its local install state.
///
/// `game` is its home record: what installs, launches and keys its data. Ubisoft entries
/// also carry the old `product_id` on the wire ([`game::wire`]). A Ubisoft game bought on
/// Steam is one Ubisoft entry with its Steam install in `steam`; other stores' games have
/// their store's install in `client`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    #[serde(flatten, with = "game::wire")]
    pub game: GameRef,
    pub name: String,
    pub installed: Option<Game>,
    pub update_available: bool,
    pub running: bool,
    /// Ubisoft's art: file names from the product config (served by Ubisoft's asset CDN)
    /// and URLs from the games API. Steam's art is in `steam` (see [`art::candidates`]).
    #[serde(default)]
    pub art: Art,
    /// User ids of the signed-in accounts that own it, default account first. Empty for a
    /// Steam install no signed-in account has (its Steam account isn't linked).
    #[serde(default)]
    pub owned_by: Vec<String>,
    /// Ubisoft has a PC build Quiver can install. `false` for copies sold on Steam: the
    /// account owns the product, but Steam installs and updates it.
    #[serde(default = "yes")]
    pub installable: bool,
    /// Installed through Steam (launched with `steam://rungameid/<app_id>`). Steam
    /// installs, updates and uninstalls it; Quiver shows it and can make it start through
    /// Quiver instead of Ubisoft Connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam: Option<exports::SteamGame>,
    /// The game's UbiServices space (ownership data, else its product config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// A game of another store, installed and run by that store's client (a Steam game in
    /// local mode): its install, art, play time and achievement counts as the client keeps
    /// them. `None` for Ubisoft games.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<stores::ClientInstall>,
    /// Other stores' records of this game, linked by an id the vendor declared (an Epic
    /// copy of a Ubisoft game, folded into its Ubisoft entry; or, on an entry not folded
    /// yet, the game it links to). See `stores::StoreRecord` and design/STORES.md §2.4.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<stores::StoreRecord>,
}

impl LibraryEntry {
    /// The Ubisoft product id, for a Ubisoft game.
    pub fn ubisoft_id(&self) -> Option<u32> {
        self.game.as_ubisoft()
    }

    pub fn store(&self) -> StoreId {
        self.game.store
    }

    /// An update is waiting, whoever installs the game: Quiver's install is behind the
    /// store's build (Ubisoft, Epic: `update_available`), or the store's client has one
    /// waiting or under way (Steam: `client.update_pending`). The library's Updates filter
    /// and badge, and `quiver library --updates`.
    pub fn has_update(&self) -> bool {
        (self.installed.is_some() && self.update_available) || self.client.as_ref().is_some_and(|c| c.update_pending)
    }

    /// Installed only through Steam (no Quiver install of its own): a Ubisoft game's Steam
    /// copy, or a Steam game.
    pub fn steam_only(&self) -> bool {
        self.installed.is_none() && (self.steam.is_some() || self.client.is_some() && self.game.store == StoreId::Steam)
    }

    /// Installed by Quiver, by Steam, or by another store's client.
    pub fn on_disk(&self) -> bool {
        self.installed.is_some() || self.steam.is_some() || self.client.is_some()
    }

    /// Every store the entry has a record in, home store first (for store badges and
    /// filters): a Ubisoft game bought on Steam is in both.
    pub fn stores(&self) -> Vec<StoreId> {
        let mut out = vec![self.game.store];
        if self.steam.is_some() && !out.contains(&StoreId::Steam) {
            out.push(StoreId::Steam);
        }
        for r in self.records.iter().filter(|r| r.relation == stores::Relation::CopyOf) {
            if !out.contains(&r.game.store) {
                out.push(r.game.store);
            }
        }
        out
    }

    /// The entry is this game's home record, or carries `game` as a folded copy.
    pub fn is_or_has(&self, game: &GameRef) -> bool {
        self.game == *game || self.records.iter().any(|r| r.relation == stores::Relation::CopyOf && r.game == *game)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Art {
    pub thumb: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
    /// `icon_image` (an .ico), used for .desktop entries and Steam shortcuts.
    #[serde(default)]
    pub icon: Option<String>,
    /// From UbiServices' games API, for slots the config leaves empty (games bought on
    /// Steam have no config art).
    #[serde(default, skip_serializing_if = "art::SpaceArt::is_empty")]
    pub space: art::SpaceArt,
    /// Epic's catalog art (`keyImages`: `DieselGameBoxTall`, `DieselGameBox`,
    /// `DieselGameBoxLogo`), for Epic games and Epic copies of Ubisoft games.
    #[serde(default, skip_serializing_if = "art::SpaceArt::is_empty")]
    pub epic: art::SpaceArt,
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
    /// The game (with the old `product_id` too, for Ubisoft games).
    #[serde(flatten, with = "game::wire")]
    pub game: GameRef,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveSession {
    pub product_id: u32,
    /// User id of the account playing.
    #[serde(default)]
    pub account: String,
    pub started: DateTime<Utc>,
    /// Who holds the session open, each once: `launcher` (games quiverd started), `steam`
    /// (a Steam copy started through `quiver steam-run`) and/or `bridge` (any other game
    /// talking to a Quiver bridge, e.g. containers).
    pub holders: Vec<String>,
    /// Whether Ubisoft accepted the session (play time is being recorded upstream).
    pub reported: bool,
    /// The Steam app it was started from (`quiver steam-run`), if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_app_id: Option<u32>,
    // ---- Addressing a running game (design/MOD-CONFIG.md C1; appended). Filled in by
    // `Request::Sessions` and `OptionsRequest::Sessions` for games quiverd launched.
    /// `s-…`, also `QUIVER_SESSION` in the game's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Mod profile id (`vanilla` for vanilla).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The options revision the game launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_revision: Option<u32>,
    /// The game's processes, once known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<u32>,
    // ---- Other stores (appended; design/STORE-STEAM.md §9). A Steam game started through
    // `quiver steam-run` (no Ubisoft product) has `product_id` 0 and its ref here; Ubisoft
    // sessions leave it out, so their JSON is as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
}

impl ActiveSession {
    /// The game being played: `game` when set, else the Ubisoft product.
    pub fn game_ref(&self) -> GameRef {
        self.game.unwrap_or(GameRef::ubisoft(self.product_id))
    }
}

/// One play session recorded by the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Mod versions: the mods the launch ran, bottom to top, each with the version it
    /// resolved to ("DLSS v3 (current)"), so the session can be reproduced even after the
    /// mod's current version moves on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<mods::ResolvedMod>,
    // ---- appended (design/MOD-CONFIG.md §3, SIGHT-HUD.md §3.12). Sessions recorded before
    // these were fields kept them under the same keys in `playtime.jsonl`, so old lines read
    // into them unchanged.
    /// The launch's session id (`s-…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The options revision the session launched with (MOD-CONFIG steer 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_revision: Option<u32>,
    /// The session's performance summary (also read with `PerfRequest::Summaries`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<perf::PerfSummary>,
    /// The Quiver Tweaks revisions (global, game, profile) the session launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tweaks: Option<tweaks::TweakRevisions>,
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
    /// The game it was about, if any: `product_id` for Ubisoft games, as always (so older
    /// daemons read `activity.jsonl`), `game` for others.
    #[serde(flatten, with = "game::persisted::option")]
    pub game: Option<GameRef>,
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
    /// Owned, installable games with install state. `all_stores`: also every other store's
    /// games (Steam's); clients from before other stores leave it out and get Ubisoft's.
    Library {
        refresh: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all_stores: bool,
    },
    /// Install a game, or update it if installed (any store that installs: Ubisoft, Epic).
    Install {
        #[serde(flatten)]
        game: game::GameField,
        language: Option<String>,
    },
    /// Adopt an existing install (Lutris/Connect) by reflink-copying its manifest files.
    Import {
        #[serde(flatten)]
        game: game::GameField,
        path: PathBuf,
        language: Option<String>,
    },
    Uninstall {
        #[serde(flatten)]
        game: game::GameField,
        remove_prefix: bool,
    },
    /// Launch a game. `profile`: a mod profile's name or id, or `vanilla` for the unmodded
    /// game; `None` (what older clients send): the game's default profile (its `profile`
    /// setting; vanilla when unset, or with a warning when that profile was deleted).
    /// A game of another store asks its client to start it (Steam: `steam://rungameid`).
    Play {
        #[serde(flatten)]
        game: game::GameField,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    Stop {
        #[serde(flatten)]
        game: game::GameField,
    },
    /// Cancel a job. Installs stop and can be resumed by installing again; `discard`
    /// also deletes the partial files (only for games that weren't installed before).
    /// Launches stop the game.
    Cancel { job_id: u64, discard: bool },
    /// A game's launch settings. Ubisoft games go out as `product_id` (as always); a Steam
    /// game's (`game: "steam:620"`) are what `quiver steam-run` applies to its Steam launch.
    SettingsGet {
        #[serde(flatten)]
        game: game::GameField,
    },
    SettingsSet {
        #[serde(flatten)]
        game: game::GameField,
        ops: Vec<SettingOp>,
        message: Option<String>,
    },
    SettingsHistory {
        #[serde(flatten)]
        game: game::GameField,
    },
    SettingsRestore {
        #[serde(flatten)]
        game: game::GameField,
        revision: u32,
    },
    /// A launch context for running a game's bridge outside this launcher (e.g. in a
    /// container): account, fresh ticket, owned ids, CD keys, broker address.
    /// `account` (user id or name) overrides the game's launch account.
    LaunchContext {
        product_id: u32,
        savegame_path: Option<String>,
        install_dir: Option<PathBuf>,
        #[serde(default)]
        account: Option<String>,
        /// The Steam app this launch is for (`quiver steam-run`): its play session shows
        /// as started from Steam.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steam_app_id: Option<u32>,
    },
    /// Installed Proton builds.
    ProtonList,
    /// Play sessions recorded by this launcher; for another store's game, the total and last
    /// played time its client recorded (no sessions).
    Playtime {
        #[serde(flatten)]
        game: game::GameField,
    },
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
    /// Write (or with `remove`, delete) a game's .desktop entry: a game Quiver installs
    /// (Ubisoft, Epic). A Ubisoft game goes out as `product_id`, as before; others as `game`.
    ExportDesktop {
        #[serde(flatten)]
        game: game::GameField,
        remove: bool,
    },
    /// Add (or remove) a game's Steam shortcut. Staged if Steam is running. `steam_user`
    /// (id or name) is required when there are several Steam users and none was chosen
    /// before; a given user is remembered. Not for Steam's own games (Steam lists them).
    ExportSteam {
        #[serde(flatten)]
        game: game::GameField,
        remove: bool,
        steam_user: Option<String>,
    },
    /// Steam's users on this machine (`exports::SteamUsers`).
    SteamUsers,
    /// Write staged shortcut changes now (fails while Steam is running). Never restarts Steam.
    SteamApplyStaged,
    /// Mod layers and profiles (see [`mods`]).
    Mods(ModsRequest),
    /// The download queue in order (`Vec<downloads::QueueEntry>`).
    Queue,
    /// Move a game's queue item up, down or to the top.
    QueueMove {
        #[serde(flatten)]
        game: game::GameField,
        to: downloads::QueueMove,
    },
    /// Pause a game's download (it stops at a slice boundary and resumes where it was).
    QueuePause {
        #[serde(flatten)]
        game: game::GameField,
    },
    /// Resume a paused item, or retry a failed one.
    QueueResume {
        #[serde(flatten)]
        game: game::GameField,
    },
    /// Take a game out of the queue, stopping it if it is running. Downloaded files stay,
    /// so installing again resumes.
    QueueRemove {
        #[serde(flatten)]
        game: game::GameField,
    },
    /// Sizes, free space and languages for installing (or updating) a game, without
    /// starting it (`downloads::InstallPreview`).
    InstallPreview {
        #[serde(flatten)]
        game: game::GameField,
        language: Option<String>,
    },
    /// What updating an installed game would change (`downloads::UpdatePreview`).
    UpdatePreview {
        #[serde(flatten)]
        game: game::GameField,
    },
    /// Queue an update for an installed game. Returns the job id.
    Update {
        #[serde(flatten)]
        game: game::GameField,
    },
    /// Hash a game's installed files against its manifest. With `repair`, queue
    /// re-downloading only what is missing or corrupt. Returns the job id.
    Verify {
        #[serde(flatten)]
        game: game::GameField,
        repair: bool,
    },
    /// The last verify report for a game (`Option<downloads::VerifyReport>`).
    VerifyResult {
        #[serde(flatten)]
        game: game::GameField,
    },
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
    // ---- Steam copies of Ubisoft games: "use Quiver instead of Ubisoft Connect"
    /// Ubisoft games installed through Steam, with whether each starts through Quiver
    /// (`exports::SteamGames`). `steam_user` (id or name) reads that user's launch options
    /// instead of each game's installer's.
    SteamGames {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steam_user: Option<String>,
    },
    /// Make a Steam game start through Quiver: its launch options become
    /// `<quiver> --source steam steam-run --product <id> %command%`, keeping the user's own
    /// options (an older form without `--product` counts as done already). Staged
    /// while Steam runs (it rewrites the file on exit). Returns `exports::SteamPatchResult`.
    SteamPatch {
        app_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steam_user: Option<String>,
    },
    /// Undo [`Request::SteamPatch`] (or a hand-made one): the launch options go back to what
    /// they were. Staged while Steam runs.
    SteamUnpatch {
        app_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steam_user: Option<String>,
    },
    /// "Not now" on the offer for a newly installed game: it isn't offered again.
    SteamDismissOffer { app_id: u32 },
    // ---- Any Steam game through `quiver steam-run` (appended; design/STORE-STEAM.md §9)
    /// Whether a Steam game starts through `quiver steam-run` (`exports::SteamHook`): its
    /// launch options for the Steam user who plays it, and what `SteamHook` would write.
    /// Read-only.
    SteamHookStatus {
        app_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steam_user: Option<String>,
    },
    /// Start a Steam game through Quiver (`on`) or not: its launch options get
    /// `<quiver> --source steam steam-run --app <appid>` in front of `%command%`, keeping the
    /// user's own, or go back to exactly what they were. A Ubisoft game's Steam copy gets
    /// `SteamPatch`/`SteamUnpatch` instead (with `--product`). Staged while Steam runs;
    /// opt-in per game (STORE-STEAM steer 3b). Returns `exports::SteamHook`.
    SteamHook {
        app_id: u32,
        on: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steam_user: Option<String>,
    },
    /// `quiver steam-run` for a Steam game: hold a local play session for Steam app `app_id`
    /// while process `pid` lives (`steam-run` registers itself, then execs Steam's command,
    /// so the pid becomes Steam's own command). quiverd watches the pid; when it exits, the
    /// session goes into the game's play history with its perf summary. Returns the
    /// `ActiveSession`.
    SteamSession {
        app_id: u32,
        pid: u32,
        /// The perf session `SteamLaunchPlan { perf: true }` began for this launch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        perf_session: Option<String>,
        /// The settings revision the plan came from.
        #[serde(default)]
        settings_revision: u32,
        /// The mod launch the plan prepared (`SteamLaunchPlan { mods: true }`,
        /// `SteamMods::launch`): its profile, mods and options go into the session's record,
        /// and its options session ends with it (appended).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mods_launch: Option<String>,
    },
    /// Read (and with `enabled`, change) whether quiverd offers the switch for Ubisoft
    /// games newly installed in Steam. Returns `{ "enabled": bool }`.
    SteamOffers {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
    },
    /// Close Steam cleanly, write every staged change (shortcuts and launch options) and
    /// start Steam again; with Steam closed, only writes them. Takes up to ~90 s. Returns
    /// `exports::RestartReport`; fails if anything is still staged afterwards.
    ///
    /// **Only send this after the person confirmed it, every time** (a dialog they answered
    /// just now, or a terminal prompt). Refused unless `confirmed` is true, and from sources
    /// other than "gui" and "cli"; quiverd never restarts Steam on its own.
    SteamRestart {
        #[serde(default)]
        confirmed: bool,
    },
    // ---- Steam copies in the library (feat/steam-library)
    /// What Ubisoft's ownership service says about products, raw (`Vec<OwnedProduct>`):
    /// for finding out why a product is or isn't in the library. `product_id: None`: every
    /// product. Read-only; uses the account's existing session.
    Ownership {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<u32>,
        /// Account (user id or name); default: the product's launch account.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// The launch settings `quiver steam-run` applies to a Steam copy
    /// (`SteamLaunchPlan`): the game's environment, DLL overrides, arguments and wrappers.
    /// With `perf`, quiverd also starts the launch's perf session (design/SIGHT-HUD.md):
    /// its environment is in the plan's `env` and its id in `perf_session`; it ends with the
    /// Steam copy's play session (or, if the game never starts, after a while).
    ///
    /// The game is a Ubisoft product (a Steam copy; `product_id` on the wire, as always) or,
    /// for any other Steam game, `game: "steam:<appid>"`: the same plan from that game's
    /// settings, its game and global tweaks, and its HUD (design/STORE-STEAM.md §9).
    SteamLaunchPlan {
        #[serde(flatten)]
        game: game::GameField,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        perf: bool,
        /// The launch's environment as Steam set it (what the person typed before
        /// `%command%`, Steam's own variables), limited to the keys tweaks and the HUD look
        /// at (`DXVK_*`, `VK_*`, `PROTON_*`, …): tweaks yield to the person's values per key
        /// and the Sight layer goes below their vkBasalt or lsfg-vk. Sent for every Steam
        /// launch, Ubisoft copies included (STORE-STEAM §9, G4).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        /// Prepare the launch's mods too (appended; design/STEAM-COPIES.md §4): the profile's
        /// config patches and options are applied to Steam's prefix and install view, and the
        /// plan's `mods` says what to mount. Sent by a `steam-run` that can mount them.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        mods: bool,
        /// The profile to run (a name or id, `vanilla`); `None`: the game's `steam_profile`
        /// setting (`steam-run --profile`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    // ---- Save stores (see [`savestores`]); appended
    /// A game's save stores, who uses each, slot counts (`savestores::SaveStoresReport`).
    /// Reads only the prefix; no Ubisoft request.
    SaveStores { product_id: u32 },
    /// Create, rename, retire or restore stores, move the cloud flag, or pick vanilla's
    /// store. Records a revision; returns `savestores::SaveStoresRevision`. Agents need
    /// `confirmed` to move the cloud flag or restore a revision (which saves sync changes).
    SaveStoreEdit {
        product_id: u32,
        op: savestores::SaveStoreOp,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        confirmed: bool,
    },
    /// The save stores' revisions (`Vec<savestores::SaveStoresRevision>`).
    SaveStoreHistory { product_id: u32 },
    /// Merge one store's slots into another (`into` `None`: Default), slot by slot, the
    /// newer file (by modification time) winning. The target's saves are backed up before
    /// any is replaced; the source is left as it was. Refused while the game runs. Agents
    /// need `confirmed`. Returns `savestores::MergeReport`.
    SavesMerge {
        product_id: u32,
        from: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        into: Option<String>,
        #[serde(default)]
        confirmed: bool,
    },
    // ---- Mod options (see [`options`]); appended
    /// Mod options: schemas, values per profile, live changes (design/MOD-CONFIG.md).
    Options(options::OptionsRequest),
    // ---- Mod SDK (appended; build/mod-sdk)
    /// Mods connected through the mod SDK (`quiver_mod.h`) in running games.
    ModSdk(mods::ModSdkRequest),
    // ---- Performance metrics and the HUD (see [`perf`]); appended
    /// Perf catalog, subscriptions, the HUD's settings, toggle and renderer attachment
    /// (design/SIGHT-HUD.md §3.11).
    Perf(perf::PerfRequest),
    // ---- Achievements (appended; build/achievements)
    /// Unlock status, the report journal, manual resends (see [`achievements`]).
    Achievements(achievements::AchievementsRequest),
    // ---- Stores (appended; design/STORES.md)
    /// Every store provider, its account state, capabilities and game count
    /// (`Vec<stores::StoreInfo>`).
    Stores,
    /// A game's page for any store (`stores::GameDetail`): facts, install, play time,
    /// achievements, cloud state, as far as the store's files say.
    GameDetail {
        #[serde(alias = "product_id")]
        game: GameRef,
        #[serde(default)]
        refresh: bool,
    },
    // ---- The daemon's lifecycle (appended; build/daemon-auto-restart)
    /// Build, staged update, what a restart waits on, holds (see [`daemon`]).
    Daemon(daemon::DaemonRequest),
    // ---- Quiver Tweaks (appended; design/TWEAKS.md)
    /// Platform-level overrides per scope (global, game, profile), the plan of the next
    /// launch and sessions' effects (see [`tweaks`]).
    Tweaks(tweaks::TweaksRequest),
    // ---- Epic Games Store (appended; design/STORE-EPIC.md)
    /// Epic accounts: sign in with a pasted code, sign out, list (see [`epic`]).
    Epic(epic::EpicRequest),
    // ---- The Sight panel (appended; design/SIGHT.md §14)
    /// Open, close or toggle the in-game panel; its status (see [`sight`]).
    Sight(sight::SightRequest),
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
    /// `code`: [`daemon::RESTARTING`] when the request was refused, not run, because the
    /// daemon is restarting (send it again); none otherwise (and from older daemons).
    Err {
        id: u64,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    Event(Event),
    /// Mod options events as daemons from before the fold into [`Event`] sent them
    /// (`{"options": {...}}`). Still read; quiverd now sends [`Event::OptionsChanged`] and its
    /// siblings (see [`Reply::folded`]).
    Options(options::OptionsEvent),
    /// Perf events as daemons from before the fold sent them (`{"perf": {...}}`). Still
    /// read; quiverd now sends [`Event::PerfUpdate`] and its siblings.
    Perf(perf::PerfEvent),
    // ---- appended (build/daemon-auto-restart); older clients skip lines they can't read
    /// The daemon's last message on a connection before it restarts onto a new build:
    /// it answers nothing more on it, and requests it hasn't answered did not run (send
    /// them again; the new daemon picks up new connections at once).
    Closing { reason: String },
}

impl Reply {
    /// The reply as it goes on the wire: options and perf events as [`Event`]s. (quiverd's
    /// perf and options code still hands out the `Options`/`Perf` forms internally.)
    pub fn folded(self) -> Reply {
        match self {
            Reply::Options(e) => Reply::Event(e.into()),
            Reply::Perf(e) => Reply::Event(e.into()),
            r => r,
        }
    }
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
    /// A game's mod layers or profiles changed. Another store's game (appended): `game`,
    /// with `product_id` 0.
    ModsChanged {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        game: Option<GameRef>,
    },
    /// Not sent by quiverd: [`Client`] reports that the daemon went away (it is
    /// reconnecting; events are missed until [`Event::Reconnected`]).
    Disconnected,
    /// Not sent by quiverd: [`Client`] is connected and subscribed again after
    /// [`Event::Disconnected`]. The daemon may have restarted; reload state.
    Reconnected,
    /// Ubisoft games installed through Steam, or whether they start through Quiver, changed
    /// (see `Request::SteamGames`). `offer`: app ids newly installed and offered the switch.
    SteamGamesChanged {
        #[serde(default)]
        offer: Vec<u32>,
    },
    // ---- appended (build/achievements)
    /// A game earned an achievement this account hadn't (as far as Ubisoft and the journal
    /// know); `report` is its journal state (`pending`, `reported`, `off`, `dry_run`, …).
    /// Sight shows these as toasts.
    AchievementUnlocked {
        product_id: u32,
        account: String,
        achievement_id: u32,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        /// Icon file on this machine, if the set is cached.
        #[serde(default)]
        image: Option<String>,
        unlocked_at: DateTime<Utc>,
        report: achievements::ReportState,
        /// Another store's game (appended; Epic): `product_id` is 0 and `achievement_id` 0,
        /// the store's own id for it is `key`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        game: Option<GameRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    // ---- appended: mod options (design/MOD-CONFIG.md §6, C5). Daemons from before these
    // sent them as `Reply::Options`; `Client` delivers both forms the same way.
    /// A new revision of a profile's option values.
    OptionsChanged {
        product_id: u32,
        profile: String,
        revision: u32,
        source: String,
        #[serde(default)]
        changes: Vec<String>,
    },
    /// Values changed for a running session (by anyone; saved or not): option id → value.
    OptionsLive {
        session: String,
        product_id: u32,
        surface: String,
        values: std::collections::BTreeMap<String, serde_json::Value>,
        by: String,
        #[serde(default)]
        saved: bool,
    },
    /// The game exited abnormally soon after a live change: offer `RevertSession` (never
    /// done automatically, MOD-CONFIG §8).
    OptionsCrash { product_id: u32, profile: String, note: options::SessionOptions },
    /// What a change made while the game runs did, one outcome per operation, whoever made
    /// it (Sight shows it as a toast).
    OptionsOutcome {
        session: String,
        product_id: u32,
        profile: String,
        by: String,
        outcomes: Vec<options::OpOutcome>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<String>,
    },
    /// A mod's SDK channel connected or went away in a running session.
    ModChannel {
        session: String,
        #[serde(default)]
        product_id: u32,
        surface: String,
        connected: bool,
    },
    // ---- appended: performance metrics (design/SIGHT-HUD.md §3.11). Daemons from before
    // these sent them as `Reply::Perf`. `PerfUpdate` and `HudModel` go only to the
    // connection that subscribed or attached; the others to every `Subscribe`r.
    PerfUpdate(perf::PerfUpdate),
    HudModel(perf::HudModel),
    /// A session's catalog changed (a producer came or went, a mod declared metrics).
    PerfCatalogChanged { session: String, product_id: u32 },
    /// HUD settings or a running HUD's state changed.
    HudChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// A perf session started or ended (with its summary when it ended).
    PerfSession {
        session: String,
        product_id: u32,
        ended: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<Box<perf::PerfSummary>>,
        /// Another store's game (`product_id` is then 0).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        game: Option<GameRef>,
    },
    // ---- appended (build/daemon-auto-restart)
    /// quiverd is restarting onto a newly installed build (`to`: its version, when the
    /// install manifest says). Subscribers see `Disconnected`, then `Reconnected` once the
    /// new build is up.
    DaemonRestarting {
        from: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    // ---- appended: Quiver Tweaks (design/TWEAKS.md §8.1)
    /// A new revision of a tweak scope's values (global, a game, a profile).
    TweaksChanged {
        scope: tweaks::Scope,
        revision: u32,
        source: String,
        #[serde(default)]
        changes: Vec<String>,
    },
    /// A tweak's effect in a running session changed (confirmed by evidence, …).
    TweaksEffect {
        session: String,
        #[serde(flatten, with = "game::wire")]
        game: GameRef,
        effect: tweaks::TweakEffect,
    },
    // ---- appended: stores (design/STORES.md §3.2)
    /// A store's accounts changed (signed in, signed out, default, or Epic ended a
    /// session): reload `Request::Stores` and that store's account list.
    StoreAccountsChanged { store: StoreId },
    // ---- appended: the Sight panel (design/SIGHT.md §14). Comes as a perf event
    // (`PerfEvent::Panel`) to every subscriber; only `quiver-sight` acts on it.
    SightPanel(sight::PanelCommand),
}

impl From<options::OptionsEvent> for Event {
    fn from(e: options::OptionsEvent) -> Event {
        use options::OptionsEvent as O;
        match e {
            O::Changed { product_id, profile, revision, source, changes } => Event::OptionsChanged { product_id, profile, revision, source, changes },
            O::Live { session, product_id, surface, values, by, saved } => Event::OptionsLive { session, product_id, surface, values, by, saved },
            O::Crash { product_id, profile, note } => Event::OptionsCrash { product_id, profile, note },
            O::Outcome { session, product_id, profile, by, outcomes, changes } => Event::OptionsOutcome { session, product_id, profile, by, outcomes, changes },
        }
    }
}

impl From<perf::PerfEvent> for Event {
    fn from(e: perf::PerfEvent) -> Event {
        use perf::PerfEvent as P;
        match e {
            P::Update(u) => Event::PerfUpdate(u),
            P::HudModel(m) => Event::HudModel(m),
            P::CatalogChanged { session, product_id } => Event::PerfCatalogChanged { session, product_id },
            P::HudChanged { product_id, session } => Event::HudChanged { product_id, session },
            P::Session { session, product_id, ended, summary, game } => Event::PerfSession { session, product_id, ended, summary, game },
            P::Panel(c) => Event::SightPanel(c),
        }
    }
}

impl Event {
    /// This event in `OptionsEvent` form, if it is one (for `Client::subscribe_options`).
    pub fn options(&self) -> Option<options::OptionsEvent> {
        use options::OptionsEvent as O;
        Some(match self {
            Event::OptionsChanged { product_id, profile, revision, source, changes } => {
                O::Changed { product_id: *product_id, profile: profile.clone(), revision: *revision, source: source.clone(), changes: changes.clone() }
            }
            Event::OptionsLive { session, product_id, surface, values, by, saved } => O::Live {
                session: session.clone(),
                product_id: *product_id,
                surface: surface.clone(),
                values: values.clone(),
                by: by.clone(),
                saved: *saved,
            },
            Event::OptionsCrash { product_id, profile, note } => O::Crash { product_id: *product_id, profile: profile.clone(), note: note.clone() },
            Event::OptionsOutcome { session, product_id, profile, by, outcomes, changes } => O::Outcome {
                session: session.clone(),
                product_id: *product_id,
                profile: profile.clone(),
                by: by.clone(),
                outcomes: outcomes.clone(),
                changes: changes.clone(),
            },
            _ => return None,
        })
    }

    /// This event in `PerfEvent` form, if it is one (for `Client::subscribe_perf`).
    pub fn perf(&self) -> Option<perf::PerfEvent> {
        use perf::PerfEvent as P;
        Some(match self {
            Event::PerfUpdate(u) => P::Update(u.clone()),
            Event::HudModel(m) => P::HudModel(m.clone()),
            Event::PerfCatalogChanged { session, product_id } => P::CatalogChanged { session: session.clone(), product_id: *product_id },
            Event::HudChanged { product_id, session } => P::HudChanged { product_id: *product_id, session: session.clone() },
            Event::PerfSession { session, product_id, ended, summary, game } => {
                P::Session { session: session.clone(), product_id: *product_id, ended: *ended, summary: summary.clone(), game: *game }
            }
            Event::SightPanel(c) => P::Panel(c.clone()),
            _ => return None,
        })
    }
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

/// `new`, or `old` while only `old` exists. Quiver's own files and folders were named `ubi-*`
/// before the rename to Quiver; existing ones are read under the old name until something
/// moves them (read old, write new).
pub fn renamed(new: PathBuf, old: &std::path::Path) -> PathBuf {
    if new.symlink_metadata().is_err() && old.symlink_metadata().is_ok() { old.to_path_buf() } else { new }
}

/// The per-launch logs in a game's prefix: `<prefix>/quiver-run`, or a prefix's `ubi-run`
/// from before the rename until its next launch moves it.
pub fn launch_logs_dir(prefix: &std::path::Path) -> PathBuf {
    renamed(prefix.join("quiver-run"), &prefix.join("ubi-run"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_reads_old_until_new_exists() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(launch_logs_dir(t.path()), t.path().join("quiver-run"));
        std::fs::create_dir(t.path().join("ubi-run")).unwrap();
        assert_eq!(launch_logs_dir(t.path()), t.path().join("ubi-run"));
        std::fs::create_dir(t.path().join("quiver-run")).unwrap();
        assert_eq!(launch_logs_dir(t.path()), t.path().join("quiver-run"));
    }

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
    fn profile_setting() {
        let mut s = GameSettings::default();
        apply(&mut s, "profile=ptkzxpv").unwrap();
        assert_eq!(s.profile.as_deref(), Some("ptkzxpv"));
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::json!({ "profile": "ptkzxpv" }));
        // Vanilla (any case) and empty both mean "no default profile".
        apply(&mut s, "profile=Vanilla").unwrap();
        assert_eq!(s.profile, None);
        apply(&mut s, "profile=p1").unwrap();
        apply(&mut s, "profile=").unwrap();
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::json!({}));
        // The default is per game: a profile's overrides never change it.
        let mut game = GameSettings::default();
        apply(&mut game, "profile=p1").unwrap();
        let over = GameSettings { profile: Some("p2".into()), ..Default::default() };
        assert_eq!(game.merged(&over).profile.as_deref(), Some("p1"));
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
    fn export_requests_take_a_game() {
        // What clients from before game refs send still parses, and Ubisoft games still go
        // out that way (older daemons read it).
        let r: Request = serde_json::from_str(r#"{"method":"export_desktop","params":{"product_id":1651,"remove":false}}"#).unwrap();
        assert!(matches!(r, Request::ExportDesktop { game, remove: false } if game == GameRef::ubisoft(1651)));
        let r: Request =
            serde_json::from_str(r#"{"method":"export_steam","params":{"product_id":1651,"remove":true,"steam_user":null}}"#).unwrap();
        assert!(matches!(r, Request::ExportSteam { game, remove: true, steam_user: None } if game == GameRef::ubisoft(1651)));
        let v = serde_json::to_value(Request::ExportDesktop { game: 1651.into(), remove: false }).unwrap();
        assert_eq!(v, serde_json::json!({"method": "export_desktop", "params": {"product_id": 1651, "remove": false}}));
        // Other stores' games by ref.
        let epic = GameRef::new(StoreId::Epic, "Hornbill").unwrap();
        let v = serde_json::to_value(Request::ExportSteam { game: epic.into(), remove: false, steam_user: Some("1001".into()) }).unwrap();
        assert_eq!(v, serde_json::json!({"method": "export_steam", "params": {"game": "epic:Hornbill", "remove": false, "steam_user": "1001"}}));
        let back: Request = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Request::ExportSteam { game, .. } if game == epic));
        let r: Request = serde_json::from_str(r#"{"method":"export_desktop","params":{"game":"epic:Hornbill","remove":true}}"#).unwrap();
        assert!(matches!(r, Request::ExportDesktop { game, remove: true } if game == epic));
    }

    #[test]
    fn uninstall_reply() {
        // Epic's reply from before `prefix_removed` and the exports.
        let r: UninstallResult = serde_json::from_str(r#"{"name":"Alto","dir":"/g/Alto","prefix":"/p/epic-Hornbill"}"#).unwrap();
        assert_eq!((r.name.as_str(), r.prefix_removed, r.exports_removed.len()), ("Alto", false, 0));
        // Ubisoft's was null before; the CLI reads it as a Value either way.
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
    fn steam_games_wire() {
        let r: Request = serde_json::from_str(r#"{"method":"steam_games","params":{}}"#).unwrap();
        assert!(matches!(r, Request::SteamGames { steam_user: None }));
        let r: Request = serde_json::from_str(r#"{"method":"steam_patch","params":{"app_id":3751950}}"#).unwrap();
        assert!(matches!(r, Request::SteamPatch { app_id: 3751950, steam_user: None }));
        let v = serde_json::to_value(Request::SteamOffers { enabled: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "steam_offers", "params": {} }));
        let v = serde_json::to_value(Request::SteamRestart { confirmed: true }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "steam_restart", "params": { "confirmed": true } }));
        let r: Request = serde_json::from_str(r#"{"method":"steam_restart","params":{}}"#).unwrap();
        assert!(matches!(r, Request::SteamRestart { confirmed: false }), "unconfirmed unless it says so");
        let e: Event = serde_json::from_str(r#"{"type":"steam_games_changed"}"#).unwrap();
        assert!(matches!(e, Event::SteamGamesChanged { offer } if offer.is_empty()));
        // Minimal and newer replies decode.
        let g: exports::SteamGames = serde_json::from_str(r#"{"games":[{"app_id":1,"name":"x","install_dir":"/g","state":"later_state"}]}"#).unwrap();
        assert!(g.offers && g.root.is_none());
        assert_eq!(g.games[0].state, exports::PatchState::Other);
        assert!(!g.games[0].patchable && !g.games[0].state.uses_quiver());
    }

    #[test]
    fn options_and_perf_events_fold_into_event() {
        // A daemon from before the fold.
        let old = r#"{"options":{"type":"changed","product_id":3539,"profile":"p1","revision":4,"source":"gui","changes":["DLSS fg.enabled on → off"]}}"#;
        let r: Reply = serde_json::from_str(old).unwrap();
        let Reply::Event(e) = r.folded() else { panic!("not folded") };
        assert!(matches!(&e, Event::OptionsChanged { revision: 4, profile, .. } if profile == "p1"));
        assert!(matches!(e.options(), Some(options::OptionsEvent::Changed { revision: 4, .. })));
        assert!(e.perf().is_none());
        // Today's wire form, and back.
        let line = serde_json::to_string(&Reply::Event(e)).unwrap();
        assert!(line.starts_with(r#"{"event":{"type":"options_changed""#), "{line}");
        let live = Reply::Options(options::OptionsEvent::Live {
            session: "s-1".into(),
            product_id: 1,
            surface: "m/aco-dlss".into(),
            values: [("fg.enabled".to_owned(), serde_json::json!(false))].into(),
            by: "in-game".into(),
            saved: false,
        });
        let v = serde_json::to_value(live.folded()).unwrap();
        assert_eq!(v["event"]["type"], "options_live");
        assert_eq!(v["event"]["values"]["fg.enabled"], false);
        let o = options::OptionsEvent::Outcome {
            session: "s-1".into(),
            product_id: 1,
            profile: "p1".into(),
            by: "sight".into(),
            outcomes: vec![options::OpOutcome { surface: "m/aco-dlss".into(), id: "fg.enabled".into(), outcome: options::Outcome::Unchanged, before: None, after: None }],
            changes: Vec::new(),
        };
        let line = serde_json::to_string(&Reply::Options(o.clone()).folded()).unwrap();
        assert!(line.contains(r#""type":"options_outcome""#), "{line}");
        let Reply::Event(back) = serde_json::from_str(&line).unwrap() else { panic!() };
        assert_eq!(back.options(), Some(o));
        // Perf: per-subscription updates (map keys are stats) survive the tagged form.
        let u = perf::PerfUpdate {
            sub: 7,
            session: "s-1".into(),
            product_id: 1,
            at: Utc::now(),
            values: [("fps".to_owned(), [(perf::Stat::P1low, perf::PerfValue { value: Some(61.0), text: "61".into(), ..Default::default() })].into())].into(),
            game: None,
        };
        let line = serde_json::to_string(&Reply::Perf(perf::PerfEvent::Update(u.clone())).folded()).unwrap();
        assert!(line.contains(r#""type":"perf_update""#), "{line}");
        let Reply::Event(back) = serde_json::from_str(&line).unwrap() else { panic!() };
        assert!(matches!(back.perf(), Some(perf::PerfEvent::Update(b)) if b == u));
        let r: Reply = serde_json::from_str(r#"{"perf":{"type":"catalog_changed","session":"s-1","product_id":5}}"#).unwrap();
        assert!(matches!(r.folded(), Reply::Event(Event::PerfCatalogChanged { product_id: 5, .. })));
        // Other events are neither.
        assert!(Event::LibraryChanged.options().is_none() && Event::LibraryChanged.perf().is_none());
    }

    #[test]
    fn sessions_carry_their_addressing() {
        // A daemon from before the fields, and one from when they came flattened beside it.
        let s: ActiveSession = serde_json::from_str(r#"{"product_id":1,"started":"2026-11-16T20:00:00Z","holders":["launcher"],"reported":true}"#).unwrap();
        assert!(s.session.is_none() && s.pids.is_empty());
        let s: options::SessionDetail = serde_json::from_str(
            r#"{"product_id":1,"started":"2026-11-16T20:00:00Z","holders":["launcher"],"reported":true,"session":"s-4kq2","profile":"p1","options_revision":7,"pids":[42]}"#,
        )
        .unwrap();
        assert_eq!((s.session.as_deref(), s.profile.as_deref(), s.options_revision, s.pids.as_slice()), (Some("s-4kq2"), Some("p1"), Some(7), &[42][..]));
        let v = serde_json::to_value(ActiveSession { product_id: 1, ..Default::default() }).unwrap();
        assert!(v.get("session").is_none() && v.get("pids").is_none(), "{v}");
        // playtime.jsonl lines written with the side keys read into the fields.
        let p: PlaySession = serde_json::from_str(
            r#"{"started":"2026-11-16T20:00:00Z","ended":"2026-11-16T21:00:00Z","seconds":3600,"session":"s-1","options_revision":3,"perf":{"frames":10,"hitches":1}}"#,
        )
        .unwrap();
        assert_eq!((p.session.as_deref(), p.options_revision), (Some("s-1"), Some(3)));
        assert_eq!(p.perf.as_ref().map(|x| x.frames), Some(10));
        let v = serde_json::to_value(PlaySession::default()).unwrap();
        assert!(v.get("perf").is_none() && v.get("session").is_none());
    }

    /// Requests and records from before game refs (STORES.md §2.3) still read, and Ubisoft
    /// records still carry `product_id`.
    #[test]
    fn game_refs_on_the_wire() {
        let r: Request = serde_json::from_str(r#"{"method":"play","params":{"product_id":3539}}"#).unwrap();
        assert!(matches!(r, Request::Play { game, profile: None } if game == GameRef::ubisoft(3539)));
        let r: Request = serde_json::from_str(r#"{"method":"play","params":{"game":"steam:620"}}"#).unwrap();
        assert!(matches!(r, Request::Play { game, .. } if game == GameRef::steam(620)));
        let r: Request = serde_json::from_str(r#"{"method":"play","params":{"game":3539,"profile":"p1"}}"#).unwrap();
        assert!(matches!(r, Request::Play { game, profile: Some(_) } if game == GameRef::ubisoft(3539)));
        let r: Request = serde_json::from_str(r#"{"method":"stop","params":{"product_id":1}}"#).unwrap();
        assert!(matches!(r, Request::Stop { game } if game == GameRef::ubisoft(1)));
        let r: Request = serde_json::from_str(r#"{"method":"playtime","params":{"product_id":1}}"#).unwrap();
        assert!(matches!(r, Request::Playtime { game } if game == GameRef::ubisoft(1)));
        // Install, verify and the queue take any store's game; Ubisoft's still go out as
        // `product_id`, so a daemon from before reads them.
        let v = serde_json::to_value(Request::Install { game: GameRef::ubisoft(3539).into(), language: None }).unwrap();
        assert_eq!(v, serde_json::json!({"method": "install", "params": {"product_id": 3539, "language": null}}));
        let epic = GameRef::new(StoreId::Epic, "Kestrel").unwrap();
        let v = serde_json::to_value(Request::QueuePause { game: epic.into() }).unwrap();
        assert_eq!(v, serde_json::json!({"method": "queue_pause", "params": {"game": "epic:Kestrel"}}));
        let r: Request = serde_json::from_str(r#"{"method":"verify","params":{"product_id":420,"repair":true}}"#).unwrap();
        assert!(matches!(r, Request::Verify { game, repair: true } if game == GameRef::ubisoft(420)));
        let q: downloads::QueueEntry = serde_json::from_value(serde_json::json!({"product_id": 0, "game": "epic:Kestrel", "name": "AW", "kind": "install", "state": "queued", "source": "cli", "added": "2026-11-18T00:00:00Z"})).unwrap();
        assert_eq!(q.game_ref(), epic);
        // Older clients ask for the library without `all_stores`, and get Ubisoft's.
        let r: Request = serde_json::from_str(r#"{"method":"library","params":{"refresh":false}}"#).unwrap();
        assert!(matches!(r, Request::Library { refresh: false, all_stores: false }));
        // Ubisoft games go out as `product_id` numbers: a daemon from before game refs reads them.
        let v = serde_json::to_value(Request::Playtime { game: GameRef::ubisoft(3539).into() }).unwrap();
        assert_eq!(v["params"], serde_json::json!({ "product_id": 3539 }));
        let v = serde_json::to_value(Request::Playtime { game: GameRef::steam(620).into() }).unwrap();
        assert_eq!(v["params"], serde_json::json!({ "game": "steam:620" }));
        let v = serde_json::to_value(Request::Library { refresh: true, all_stores: false }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "library", "params": { "refresh": true } }));
        let v = serde_json::to_value(Request::Play { game: GameRef::steam(620).into(), profile: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "play", "params": { "game": "steam:620" } }));

        // Settings and Steam launch plans: Ubisoft games on the wire exactly as before.
        let v = serde_json::to_value(Request::SettingsGet { game: 3539.into() }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "settings_get", "params": { "product_id": 3539 } }));
        let v = serde_json::to_value(Request::SteamLaunchPlan { game: 66088.into(), perf: true, env: BTreeMap::new(), mods: false, profile: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "steam_launch_plan", "params": { "product_id": 66088, "perf": true } }));
        let v = serde_json::to_value(Request::SteamLaunchPlan { game: GameRef::steam(292030).into(), perf: true, env: BTreeMap::new(), mods: true, profile: Some("Next-gen".into()) }).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "steam_launch_plan", "params": { "game": "steam:292030", "perf": true, "mods": true, "profile": "Next-gen" } }));

        // Mods and options requests: Ubisoft games exactly as before, other stores as `game`.
        let v = serde_json::to_value(Request::Mods(ModsRequest::List { game: 3539.into() })).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "mods", "params": { "op": "list", "product_id": 3539 } }));
        let r: Request = serde_json::from_value(v).unwrap();
        assert!(matches!(r, Request::Mods(ModsRequest::List { game }) if game == GameRef::ubisoft(3539)));
        let v = serde_json::to_value(Request::Mods(ModsRequest::Plan { game: GameRef::steam(292030).into(), profile: Some("p".into()) })).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "mods", "params": { "op": "plan", "game": "steam:292030", "profile": "p" } }));
        let r: Request = serde_json::from_value(v).unwrap();
        assert!(matches!(r, Request::Mods(ModsRequest::Plan { game, .. }) if game == GameRef::steam(292030)));
        let r: Request = serde_json::from_str(r#"{"method":"options","params":{"op":"list","game":"steam:292030"}}"#).unwrap();
        assert!(matches!(r, Request::Options(options::OptionsRequest::List { game, .. }) if game == GameRef::steam(292030)));
        let v = serde_json::to_value(Event::ModsChanged { product_id: 3539, game: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "type": "mods_changed", "product_id": 3539 }));
        let r: Request = serde_json::from_str(r#"{"method":"steam_launch_plan","params":{"product_id":66088}}"#).unwrap();
        assert!(matches!(r, Request::SteamLaunchPlan { game, perf: false, .. } if game == GameRef::ubisoft(66088)));
        let r: Request = serde_json::from_str(r#"{"method":"settings_set","params":{"game":"steam:870780","ops":[],"message":null}}"#).unwrap();
        assert!(matches!(r, Request::SettingsSet { game, .. } if game == GameRef::steam(870780)));
        let s = ActiveSession { game: Some(GameRef::steam(870780)), ..Default::default() };
        assert_eq!((s.game_ref(), ActiveSession { product_id: 3539, ..Default::default() }.game_ref()), (GameRef::steam(870780), GameRef::ubisoft(3539)));
        let v = serde_json::to_value(ActiveSession { product_id: 3539, ..Default::default() }).unwrap();
        assert!(v.get("game").is_none(), "Ubisoft sessions as before");

        // Library entries: Ubisoft ones keep `product_id` beside `game`.
        let old = r#"{"product_id":3539,"name":"Origins","installed":null,"update_available":false,"running":false,"art":{"thumb":null,"background":null,"logo":null,"icon":null},"owned_by":["u"],"installable":true}"#;
        let e: LibraryEntry = serde_json::from_str(old).unwrap();
        assert_eq!((e.game, e.ubisoft_id()), (GameRef::ubisoft(3539), Some(3539)));
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!((v["product_id"].as_u64(), v["game"].as_str()), (Some(3539), Some("ubisoft:3539")));
        let steam = LibraryEntry { game: GameRef::steam(620), client: Some(Default::default()), ..e.clone() };
        let v = serde_json::to_value(&steam).unwrap();
        assert!(v.get("product_id").is_none() && v["game"] == "steam:620", "{v}");
        let back: LibraryEntry = serde_json::from_value(v).unwrap();
        assert!(back.steam_only() && back.on_disk() && back.stores() == [StoreId::Steam]);

        // Jobs carry both for Ubisoft.
        let j: Job = serde_json::from_str(r#"{"id":1,"kind":"play","product_id":5,"name":"x","status":"","downloaded":0,"total":0,"done":true,"error":null}"#).unwrap();
        assert_eq!(j.game, GameRef::ubisoft(5));
        assert_eq!(serde_json::to_value(&j).unwrap()["product_id"], 5);

        // activity.jsonl lines: Ubisoft ones are written exactly as before.
        let line = r#"{"at":"2026-11-16T20:00:00Z","source":"cli","action":"play x","product_id":3539}"#;
        let a: ActivityEntry = serde_json::from_str(line).unwrap();
        assert_eq!(a.game, Some(GameRef::ubisoft(3539)));
        assert_eq!(serde_json::to_string(&a).unwrap(), line);
        let a: ActivityEntry = serde_json::from_str(r#"{"at":"2026-11-16T20:00:00Z","source":"cli","action":"signed out x","product_id":null}"#).unwrap();
        assert_eq!(a.game, None);
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"at":"2026-11-16T20:00:00Z","source":"cli","action":"signed out x"}"#);
        let s = ActivityEntry { game: Some(GameRef::steam(620)), ..a };
        assert!(serde_json::to_string(&s).unwrap().ends_with(r#""game":"steam:620"}"#));
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
