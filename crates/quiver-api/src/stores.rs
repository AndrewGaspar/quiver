//! Stores other than Ubisoft, as front ends see them (design/STORES.md §3).
//!
//! Each store is a provider in quiverd. A provider says what it does ([`Capabilities`]),
//! so a front end can show what isn't wired instead of inventing it. Steam is the first:
//! in *local mode* it reads the Steam client's own files (libraries, app manifests,
//! `appinfo.vdf`, art, play time, achievements, cloud state) and plays through the client
//! (`steam://rungameid/<appid>`); Steam keeps licensing, installs, updates, cloud sync and
//! play-time records. No Steam sign-in happens in Quiver.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::game::{self, GameRef, StoreId};

/// How a store does one thing in Quiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Quiver does it.
    Quiver,
    /// The store's own client does it; Quiver starts it or shows what the client did.
    Delegated,
    /// Quiver only reads what the store recorded.
    ReadOnly,
    /// Not available (not built, or the store doesn't allow it).
    #[default]
    #[serde(other)]
    No,
}

impl Support {
    pub fn available(self) -> bool {
        self != Support::No
    }
}

/// What a store's provider implements (STORES.md §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub library: Support,
    pub install: Support,
    pub launch: Support,
    /// Quiver's per-game settings (environment, wrappers, …) apply to launches.
    pub settings: Support,
    pub mods: Support,
    pub cloud_saves: Support,
    pub achievements: Support,
    pub playtime: Support,
}

/// Whose account a store's games come from (STORES.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    SignedIn,
    SignedInElsewhere,
    /// The store's own client is signed in, and Quiver follows it (Steam local mode).
    ClientManaged,
    #[default]
    #[serde(other)]
    NeedsSignIn,
}

/// One store in the daemon (`Request::Stores`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreInfo {
    pub store: StoreId,
    pub account: AccountState,
    /// The account the store's games are read for, by name, when known (Steam: the
    /// client's most recent user).
    #[serde(default)]
    pub account_name: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Games in the library from this store.
    #[serde(default)]
    pub games: usize,
    /// Why the store has no games, or anything else to know ("Steam not found").
    #[serde(default)]
    pub note: Option<String>,
}

/// How another store's record relates to the entry carrying it (design/STORES.md §2.4).
/// Records are linked only by an id the vendor declared, never by a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// The same game bought in another store, folded into this entry, which installs,
    /// launches and keys its data (an Epic copy of a Ubisoft game in its Ubisoft entry).
    CopyOf,
    /// On an entry that wasn't folded: the game the vendor says it is, which isn't in the
    /// library (an Epic copy of a Ubisoft game not on a signed-in Ubisoft account yet).
    Linked,
    /// A relation a newer daemon knows.
    #[serde(other)]
    Other,
}

/// Another store's record of a library entry's game.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreRecord {
    pub game: GameRef,
    pub relation: Relation,
    /// That store's accounts that own it (Epic account ids), when known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_by: Vec<String>,
    /// For people: "Bought on Epic", "Managed by Ubisoft Connect".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A game installed and run by its store's own client (a Steam game in local mode), as the
/// library lists it (`LibraryEntry::client`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientInstall {
    /// The game's folder.
    pub dir: PathBuf,
    /// The store library folder it is in (a Steam library).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<PathBuf>,
    /// Bytes on disk, as the store counts them (Steam's `SizeOnDisk`).
    #[serde(default)]
    pub size_bytes: u64,
    /// Fully installed, not still downloading.
    #[serde(default)]
    pub complete: bool,
    /// The store has an update waiting or in progress.
    #[serde(default)]
    pub update_pending: bool,
    /// What the store's client is doing about that update, when it says (Steam's app
    /// manifest: state, bytes, target build, last error). The client updates the game;
    /// Quiver only shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<ClientUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    /// Platforms the store's build supports (`windows`, `linux`, `macos`), when known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// The store's own art, from its local cache.
    #[serde(default, skip_serializing_if = "crate::art::SteamArt::is_empty")]
    pub art: crate::art::SteamArt,
    /// Play time the store recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playtime: Option<StorePlaytime>,
    /// Achievements unlocked, of the total, from the store's local cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievements: Option<AchievementCount>,
    /// What Play opens: `steam://rungameid/620`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_url: Option<String>,
}

/// Where a store client's update of a game is (`ClientInstall::update`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientUpdateState {
    /// Waiting in the client's queue (or for the client to start, or the game to launch).
    #[default]
    Queued,
    Downloading,
    Paused,
    /// Writing the downloaded data into the game's folder (Steam's staging and committing).
    Installing,
    /// Checking the game's files.
    Validating,
}

/// A store client's update of a game, as far as its files say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientUpdate {
    pub state: ClientUpdateState,
    #[serde(default)]
    pub bytes_to_download: u64,
    #[serde(default)]
    pub bytes_downloaded: u64,
    #[serde(default)]
    pub bytes_to_stage: u64,
    #[serde(default)]
    pub bytes_staged: u64,
    /// The build it updates to (Steam's `TargetBuildID`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_build_id: Option<String>,
    /// The client's code for why its last update attempt failed (Steam's `UpdateResult`),
    /// when it isn't 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<u32>,
    /// The client updates it only when the game is started (Steam's per-game "only update
    /// this game when I launch it").
    #[serde(default)]
    pub on_launch: bool,
    /// What opens the client's own view of its downloads (`steam://open/downloads`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_url: Option<String>,
}

impl ClientUpdate {
    /// Done of the download, 0.0–1.0, when the client says how much there is.
    pub fn fraction(&self) -> Option<f32> {
        let (done, total) = match self.state {
            ClientUpdateState::Installing if self.bytes_to_stage > 0 => (self.bytes_staged, self.bytes_to_stage),
            _ if self.bytes_to_download > 0 => (self.bytes_downloaded, self.bytes_to_download),
            _ => return None,
        };
        Some((done as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }

    /// One line for people: "Steam is updating it · 42% of 3.1 GB", "Update queued in Steam".
    pub fn describe(&self, client: &str) -> String {
        let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
        let pct = self.fraction().map(|f| format!(" · {}%", (f * 100.0).round() as u32)).unwrap_or_default();
        let size = if self.bytes_to_download > 0 { format!(" of {}", gb(self.bytes_to_download)) } else { String::new() };
        let mut line = match self.state {
            ClientUpdateState::Downloading => format!("{client} is updating it{pct}{size}"),
            ClientUpdateState::Installing => format!("{client} is installing the update{pct}"),
            ClientUpdateState::Paused => format!("Update paused in {client}{pct}{size}"),
            ClientUpdateState::Validating => format!("{client} is checking its files"),
            ClientUpdateState::Queued if self.on_launch => format!("Update waiting: {client} updates it when you play"),
            ClientUpdateState::Queued => format!("Update queued in {client}{size}"),
        };
        if let Some(e) = self.error {
            line.push_str(&format!(" (last attempt failed, {client} error {e})"));
        }
        line
    }
}

/// Play time as a store recorded it (Steam: `localconfig.vdf`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorePlaytime {
    #[serde(default)]
    pub total_seconds: u64,
    /// The last two weeks, when the store keeps that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_weeks_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AchievementCount {
    pub unlocked: usize,
    pub total: usize,
}

/// A game's page for stores other than Ubisoft (`Request::GameDetail`): facts, install,
/// play time, achievements and cloud state, each only as far as the store's files say.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameDetail {
    #[serde(flatten, with = "game::wire")]
    pub game: GameRef,
    pub name: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// `game`, `application`, `demo`, … as the store types it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<DateTime<Utc>>,
    /// The store's page for the game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<ClientInstall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playtime: Option<StorePlaytime>,
    /// `None`: the store's cache has nothing for the game (unknown, not "none").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievements: Option<Achievements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudState>,
    /// DLC the store lists for the game, with whether it is installed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dlc: Vec<Dlc>,
    /// The account the facts are for (Steam: the client's user), by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Detail no source provides (shown as "not available", not invented).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
    /// Steam: whether the game starts through `quiver steam-run`, so Quiver's tweaks, HUD
    /// and launch settings reach it (appended; design/STORE-STEAM.md §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_hook: Option<crate::exports::SteamHook>,
    /// The store's description of the game (appended; Epic's catalog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The game this store record links to (an Epic copy of a Ubisoft game:
    /// `ubisoft:3539`), which installs and plays it (appended; STORES.md §2.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked: Option<GameRef>,
    /// Digital extras that come with the game (a soundtrack, an art book, wallpapers): their
    /// own downloads, never part of the game's folder (appended; Epic).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<Extra>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Achievements {
    pub unlocked: usize,
    pub total: usize,
    #[serde(default)]
    pub list: Vec<Achievement>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Achievement {
    /// The store's id for it (Steam's API name).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlocked_at: Option<DateTime<Utc>>,
    /// Unlocked, even when the store kept no time for it.
    #[serde(default)]
    pub unlocked: bool,
    /// Icon URL (unlocked, and greyed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_gray: Option<String>,
}

/// What the store's client last knew of the game's cloud files (Steam: `remotecache.vdf`).
/// Sync is the client's; Quiver only shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CloudState {
    pub files: usize,
    pub bytes: u64,
    /// Newest remote time of any file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_upload: Option<DateTime<Utc>>,
    /// Files the client hasn't synced (sync state other than "in sync").
    #[serde(default)]
    pub unsynced: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dlc {
    #[serde(flatten, with = "game::wire")]
    pub game: GameRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub installed: bool,
    /// Whether the account owns it, when the store says (appended; Epic's library).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned: Option<bool>,
}

/// A digital extra of a game (`GameDetail.extras`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extra {
    /// The store's id for it (Epic: its app name); pass it to install it.
    pub id: String,
    pub name: String,
    /// Its folder on this machine (next to the game's): where it goes, or where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// Downloaded (its build version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    /// Quiver can download it (`EpicRequest::ExtraInstall`).
    #[serde(default)]
    pub installable: bool,
    /// For people: why it can't be downloaded, or what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_and_newer_replies_decode() {
        let d: GameDetail = serde_json::from_str(r#"{"game":"steam:620","name":"Portal 2","later":1}"#).unwrap();
        assert_eq!(d.game, GameRef::steam(620));
        assert!(d.achievements.is_none() && d.install.is_none());
        let c: Capabilities = serde_json::from_str(r#"{"launch":"delegated","install":"teleport"}"#).unwrap();
        assert_eq!((c.launch, c.install, c.mods), (Support::Delegated, Support::No, Support::No));
        let s: StoreInfo = serde_json::from_str(r#"{"store":"steam","account":"client_managed"}"#).unwrap();
        assert_eq!(s.account, AccountState::ClientManaged);
        let i: ClientInstall = serde_json::from_str(r#"{"dir":"/s/common/Portal 2"}"#).unwrap();
        assert!(!i.complete && i.art.is_empty());
        let v = serde_json::to_value(Dlc { game: GameRef::steam(1), name: None, installed: true, owned: None }).unwrap();
        assert_eq!(v, serde_json::json!({"game": "steam:1", "installed": true}));
        let r: StoreRecord = serde_json::from_str(r#"{"game":"epic:Camellia","relation":"merged_somehow"}"#).unwrap();
        assert_eq!((r.game.to_string(), r.relation), ("epic:Camellia".to_owned(), Relation::Other));
    }
}
