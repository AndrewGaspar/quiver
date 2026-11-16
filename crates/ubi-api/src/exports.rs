//! Launcher exports: .desktop entries and Steam non-Steam shortcuts.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    Desktop,
    Steam,
}

/// One export Quiver wrote (or staged), as recorded in `<data>/exports.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRecord {
    /// Stable export key: `quiver:<product_id>[/<profile_id>]`.
    pub key: String,
    pub kind: ExportKind,
    pub product_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Name shown in the launcher or Steam.
    pub name: String,
    /// The .desktop file, or the Steam user's `shortcuts.vdf`.
    pub path: PathBuf,
    /// Steam shortcut appid (`crc32(key) | 0x80000000`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appid: Option<u32>,
    /// Steam user (account id, the `userdata/<id>` directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_user: Option<String>,
    pub written_at: DateTime<Utc>,
    /// Hash of what Quiver wrote, to notice edits made outside Quiver.
    pub hash: String,
    /// Steam was running: the shortcut waits in the staging file for a Steam restart.
    #[serde(default)]
    pub staged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportState {
    /// Present and as Quiver wrote it.
    Ok,
    /// Waiting for Steam to restart (or close) before shortcuts.vdf is written.
    WaitingForSteam,
    /// Deleted outside Quiver.
    Missing,
    /// Edited outside Quiver.
    Changed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportStatus {
    #[serde(flatten)]
    pub record: ExportRecord,
    pub state: ExportState,
}

/// A Steam account on this machine (a `userdata/<id>` directory).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SteamUser {
    /// Account id (32-bit), the directory name under `userdata/`.
    pub id: String,
    /// 64-bit SteamID, as keyed in `config/loginusers.vdf`.
    pub steam_id64: u64,
    #[serde(default)]
    pub persona_name: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
    /// Steam's most recent login on this machine.
    #[serde(default)]
    pub most_recent: bool,
}

/// Reply to `Request::SteamUsers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteamUsers {
    /// Steam's root directory, if Steam was found.
    pub root: Option<PathBuf>,
    pub users: Vec<SteamUser>,
    /// The user shortcuts go to without asking: the remembered choice, or the only user.
    pub selected: Option<String>,
    pub running: bool,
    /// Shortcut changes waiting for Steam to close.
    pub staged: Vec<String>,
}

/// Reply to `Request::ExportSteam`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteamExportResult {
    pub key: String,
    pub appid: u32,
    pub steam_user: String,
    /// Steam is running: the change is staged and shows after Steam restarts.
    pub staged: bool,
    /// Earlier staged changes written along with this one.
    #[serde(default)]
    pub applied_staged: usize,
}
