//! Game detail, progression and saves: the data behind the game page's Overview, Add-ons
//! and "Saves & progress" tabs.
//!
//! Every field is optional or defaulted so front ends tolerate older and newer daemons.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------
// Game info (product config + ownership + local install)

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameInfo {
    pub product_id: u32,
    pub name: String,
    /// User id of the account whose ownership (and saves) this reflects: the game's
    /// launch account.
    #[serde(default)]
    pub account: Option<String>,
    /// User ids of the signed-in accounts that own the game, default first.
    #[serde(default)]
    pub owned_by: Vec<String>,
    /// UbiServices space and application of the game (ownership data, else product config).
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    /// Franchise sort key, e.g. "Assassin's Creed 16" (`sort_string`).
    #[serde(default)]
    pub sort_string: Option<String>,
    /// `installer.publisher`.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Not in product configs; `None` until a source turns up.
    #[serde(default)]
    pub developer: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    /// Homepage, support, forum, ... from the product config.
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub features: Features,
    /// Present when the game is installed by Quiver.
    #[serde(default)]
    pub install: Option<InstallInfo>,
    /// Associated products (DLC, season passes, bundles, ...), owned or not.
    #[serde(default)]
    pub addons: Vec<AddOn>,
    /// Detail fields that no source provides (shown as "not available", not invented).
    #[serde(default)]
    pub unavailable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// `homepage`, `support`, `forum`, `facebook`, `about`.
    pub kind: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Features {
    #[serde(default)]
    pub cloud_saves: bool,
    /// The game has Ubisoft achievements (`uplay.achievements` names their definition set).
    #[serde(default)]
    pub achievements: bool,
    /// Ubisoft Connect challenges (`club.challenges_enabled`).
    #[serde(default)]
    pub challenges: bool,
    #[serde(default)]
    pub denuvo: bool,
    /// Stat ids shown on Connect's game page (`TimePlayed`, `PlayerLevel`, ...).
    #[serde(default)]
    pub player_stats: Vec<String>,
    /// The game's own options tool (`game_options.name`), e.g. "Animus Control Panel".
    #[serde(default)]
    pub game_options: Option<String>,
    #[serde(default)]
    pub change_language: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstallInfo {
    pub dir: PathBuf,
    /// The installed manifest's `game_version`, if it has one.
    #[serde(default)]
    pub version: Option<String>,
    /// Installed language (install record).
    #[serde(default)]
    pub language: Option<String>,
    /// Languages the installed build offers (manifest).
    #[serde(default)]
    pub languages: Vec<String>,
    /// Bytes the installed language's files take (manifest).
    #[serde(default)]
    pub size_bytes: u64,
    /// SHA-1 of the installed manifest (matches ownership's `latest_manifest` when current).
    #[serde(default)]
    pub manifest: Option<String>,
}

/// Ownership product type (`OwnedGame.ProductType`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductKind {
    Game,
    AddOn,
    PreOrderGame,
    PreOrderAddOn,
    Trial,
    Template,
    Bundle,
    SeasonPass,
    Consumable,
    Preorder,
    ConsumablePacks,
    BundlePackage,
    #[default]
    #[serde(other)]
    Unknown,
}

impl ProductKind {
    pub fn from_type(t: u32) -> Self {
        use ProductKind::*;
        match t {
            0 => Game,
            1 => AddOn,
            2 => PreOrderGame,
            3 => PreOrderAddOn,
            4 => Trial,
            5 => Template,
            6 => Bundle,
            7 => SeasonPass,
            8 => Consumable,
            9 => Preorder,
            10 => ConsumablePacks,
            11 => BundlePackage,
            _ => Unknown,
        }
    }

    /// Something a player would call DLC (shown in Add-ons by default).
    pub fn is_dlc(self) -> bool {
        matches!(self, Self::AddOn | Self::PreOrderAddOn | Self::SeasonPass)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AddOn {
    pub product_id: u32,
    /// From the add-on's own product config or the game's `addons` list.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: ProductKind,
    /// Owned by the game's launch account.
    #[serde(default)]
    pub owned: bool,
    /// Connect lists it (the game's `addons[].is_visible`, else: DLC-like kinds are).
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub description: Option<String>,
    /// Art file name on Ubisoft's asset CDN.
    #[serde(default)]
    pub thumb: Option<String>,
    /// For add-ons you don't own: a Ubisoft Store search for the name.
    #[serde(default)]
    pub store_url: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Progression (UbiServices)

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub product_id: u32,
    /// User id of the account the numbers belong to (the game's launch account).
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    /// When this was fetched from Ubisoft (the daemon caches it for a few minutes).
    pub fetched: DateTime<Utc>,
    /// Ubisoft's play-time record (seconds), when available: the headline number.
    #[serde(default)]
    pub playtime_seconds: Option<u64>,
    /// Player stats (`statsCard`).
    #[serde(default)]
    pub stats: Vec<Stat>,
    /// Ubisoft Connect challenges for this game.
    #[serde(default)]
    pub challenges: Vec<Challenge>,
    /// Ubisoft Connect rewards for this game.
    #[serde(default)]
    pub rewards: Vec<Reward>,
    #[serde(default)]
    pub achievements: Option<Achievements>,
    /// Account-wide Ubisoft Connect level and units.
    #[serde(default)]
    pub connect: Option<ConnectProfile>,
    /// Parts that could not be fetched, with why (`"challenges: HTTP 404"`).
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    /// Stat id, e.g. `TimePlayed`.
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Display value as Ubisoft formats it.
    #[serde(default)]
    pub value: Option<String>,
    /// Numeric value, when the stat is a number.
    #[serde(default)]
    pub number: Option<f64>,
    /// `Time`, `Number`, `Percent`, ... as reported.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub target: Option<f64>,
    /// Units or XP the challenge awards.
    #[serde(default)]
    pub reward: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reward {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// Redeemed by this account.
    #[serde(default)]
    pub owned: bool,
    #[serde(default)]
    pub redeemed_at: Option<DateTime<Utc>>,
    /// Price in Ubisoft Connect units (0: free).
    #[serde(default)]
    pub cost: Option<u64>,
    /// `Legendary`, `Epic`, ...
    #[serde(default)]
    pub rarity: Option<String>,
    /// Ubisoft's status for this account: `LimitReached` (redeemed), `Unlocked` (can be
    /// redeemed), `Locked`, ...
    #[serde(default)]
    pub status: Option<String>,
}

/// A game's achievements. Definitions (names, descriptions, icons) come from the asset the
/// product config names; which ones this account unlocked has no known REST source yet, so
/// `unlocked_known` is false and `unlocked` 0 until one is wired.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Achievements {
    #[serde(default)]
    pub unlocked_known: bool,
    #[serde(default)]
    pub unlocked: u32,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub items: Vec<Achievement>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Achievement {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub unlocked: bool,
    #[serde(default)]
    pub unlocked_at: Option<DateTime<Utc>>,
}

/// The account's Ubisoft Connect meta-progression.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectProfile {
    #[serde(default)]
    pub level: Option<u32>,
    /// XP earned in total.
    #[serde(default)]
    pub xp: Option<u64>,
    /// XP at which the current level starts and the next one starts.
    #[serde(default)]
    pub level_start_xp: Option<u64>,
    #[serde(default)]
    pub next_level_xp: Option<u64>,
    /// Ubisoft Connect units balance.
    #[serde(default)]
    pub units: Option<u64>,
}

// ---------------------------------------------------------------------------------------
// Saves

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavesReport {
    pub product_id: u32,
    /// User id whose saves these are (the game's launch account).
    #[serde(default)]
    pub account: Option<String>,
    /// The local save folder (`savegames/<userId>/<productId>` in the game's prefix).
    #[serde(default)]
    pub dir: Option<PathBuf>,
    #[serde(default)]
    pub slots: Vec<SaveSlot>,
    /// Where the slots' cloud columns come from: `cloudsave` (the cloud objects themselves,
    /// compared byte for byte), `connect_record` (Ubisoft Connect's record of its last upload;
    /// sizes and times only), or `None` (not checked).
    #[serde(default)]
    pub cloud_source: Option<String>,
    /// Why the cloud objects couldn't be read, when they couldn't.
    #[serde(default)]
    pub cloud_error: Option<String>,
    /// Ubisoft Connect's record of the cloud slots (UbiServices entity
    /// `upcCloudSaveUplayId_<id>`), written by Connect after each upload. Quiver's own uploads
    /// don't update it, so it can lag behind the cloud.
    #[serde(default)]
    pub connect_record: Option<CloudRecord>,
    #[serde(default)]
    pub backups: Vec<SaveBackup>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SaveSlot {
    pub slot: u32,
    #[serde(default)]
    pub local: Option<SlotFile>,
    #[serde(default)]
    pub cloud: Option<CloudSlot>,
    /// The game wrote this slot and it hasn't been uploaded yet (`<slot>.save.upload`).
    #[serde(default)]
    pub pending_upload: bool,
    #[serde(default)]
    pub state: SlotState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotFile {
    pub size: u64,
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CloudSlot {
    /// Size once inflated (comparable with the local file).
    #[serde(default)]
    pub size: Option<u64>,
    /// Stored (zlib-compressed) size.
    #[serde(default)]
    pub stored_size: Option<u64>,
    /// `Last-Modified` of the cloud object.
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    /// Same bytes locally and in the cloud.
    InSync,
    /// Only on this machine.
    LocalOnly,
    /// Only in the cloud (the next launch downloads it).
    CloudOnly,
    /// Different, and the local copy is newer (or has a pending upload).
    LocalNewer,
    /// Different, and the cloud copy is newer (the next launch downloads it, backing up
    /// the local one first).
    CloudNewer,
    /// Different, and which is newer is unknown.
    Differs,
    /// Not compared with the cloud.
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CloudRecord {
    /// Machine that last uploaded (`pc_name`).
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
    #[serde(default)]
    pub slots: Vec<RecordSlot>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordSlot {
    pub slot: u32,
    /// Inflated size, as the local file.
    #[serde(default)]
    pub size: Option<u64>,
    /// The local file's modification time when it was uploaded.
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
}

/// A copy of a game's save folder that Quiver made before overwriting it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SaveBackup {
    /// Directory name under the backup root, e.g. `3539-20261116-101500`; pass it to restore.
    pub id: String,
    pub path: PathBuf,
    pub created: Option<DateTime<Utc>>,
    /// Slots in the backup and their sizes.
    #[serde(default)]
    pub slots: Vec<(u32, u64)>,
    /// Why it was made (`before cloud download`, `before restoring …`), when recorded.
    #[serde(default)]
    pub reason: Option<String>,
    /// User id the saves belong to, when recorded (older backups don't say).
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreResult {
    pub restored: Vec<u32>,
    /// The backup of the saves that were there before.
    #[serde(default)]
    pub previous: Option<SaveBackup>,
}
