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

// ---------------------------------------------------------------------------------------
// Steam copies of Ubisoft games ("use Quiver instead of Ubisoft Connect")

/// Whether a Steam game starts through Quiver: its launch options run
/// `quiver --source steam steam-run [--product <id>] %command%`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchState {
    /// Starts through Ubisoft Connect, as Steam installed it.
    Off,
    /// Starts through Quiver; Quiver set it up.
    On,
    /// Starts through Quiver; someone set the launch options by hand.
    ByHand,
    /// Steam is running: switching to Quiver waits for Steam to close or restart.
    Waiting,
    /// Steam is running: switching back to Connect waits for Steam to close or restart.
    WaitingUndo,
    /// Which Steam user plays it isn't known (several users, none chosen).
    Unknown,
    #[serde(other)]
    Other,
}

impl PatchState {
    /// Starts (or will start, after Steam restarts) through Quiver.
    pub fn uses_quiver(self) -> bool {
        matches!(self, Self::On | Self::ByHand | Self::Waiting)
    }
}

/// A Ubisoft game installed through Steam (`Request::SteamGames`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SteamGame {
    pub app_id: u32,
    /// As Steam names it.
    pub name: String,
    pub install_dir: PathBuf,
    /// The Ubisoft product it runs as, when Quiver can tell.
    #[serde(default)]
    pub product_id: Option<u32>,
    /// How the product was found: `install_script`, `registry`, `known` or `library_name`.
    #[serde(default)]
    pub product_from: Option<String>,
    /// Owned by a signed-in Ubisoft account (`None`: the library isn't loaded).
    #[serde(default)]
    pub owned: Option<bool>,
    /// The Steam user (account id) whose launch options these are: the one who installed
    /// it, unless asked for another.
    #[serde(default)]
    pub steam_user: Option<String>,
    /// The launch options in Steam now.
    #[serde(default)]
    pub launch_options: String,
    pub state: PatchState,
    /// Can be switched to Quiver (product known, Steam supported).
    #[serde(default)]
    pub patchable: bool,
    /// Installed after Quiver first looked, and not answered yet: offer the switch.
    #[serde(default)]
    pub offer: bool,
    /// Why not patchable, or something to know.
    #[serde(default)]
    pub note: Option<String>,
}

/// Reply to `Request::SteamGames`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SteamGames {
    /// Steam's root directory, if Steam was found.
    #[serde(default)]
    pub root: Option<PathBuf>,
    #[serde(default)]
    pub running: bool,
    pub games: Vec<SteamGame>,
    /// Offer the switch when a Ubisoft game is newly installed in Steam (app setting).
    #[serde(default = "yes")]
    pub offers: bool,
    /// The `quiver` launch options Quiver writes, in front of `%command%`, less the
    /// per-game `--product <id>` it adds.
    #[serde(default)]
    pub command: String,
}

fn yes() -> bool {
    true
}

/// Reply to `Request::SteamPatch` / `Request::SteamUnpatch`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SteamPatchResult {
    pub app_id: u32,
    pub name: String,
    pub steam_user: String,
    /// The launch options now (or once Steam closes, when staged).
    pub launch_options: String,
    /// Steam is running: the change waits for it to close or restart.
    pub staged: bool,
    /// Nothing needed changing (already so).
    #[serde(default)]
    pub unchanged: bool,
    /// A change for this game that was waiting for Steam was dropped instead.
    #[serde(default)]
    pub dropped_staged: bool,
}
