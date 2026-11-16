//! Downloads: the install queue, pre-flight sizes, update previews, verify/repair and
//! download settings.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What a queue item does.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueueKind {
    Install,
    Update,
    /// Verify the installed files, then re-download only the missing or corrupt ones.
    Repair,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    Queued,
    Downloading,
    Paused,
    Verifying,
    Done,
    Failed,
}

impl QueueState {
    /// Running now (counts against `max_active`).
    pub fn is_active(self) -> bool {
        matches!(self, Self::Downloading | Self::Verifying)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueueMove {
    Up,
    Down,
    Top,
}

impl std::str::FromStr for QueueMove {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "top" => Ok(Self::Top),
            _ => Err(format!("expected up, down or top, got '{s}'")),
        }
    }
}

/// One item in the download queue. The queue is in order: items start from the top,
/// `max_active` at a time; paused and failed items keep their place but are skipped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueEntry {
    pub product_id: u32,
    pub name: String,
    pub kind: QueueKind,
    pub state: QueueState,
    /// 1-based place in the queue.
    #[serde(default)]
    pub position: u32,
    /// The job showing this item's progress (changes when quiverd restarts).
    #[serde(default)]
    pub job_id: Option<u64>,
    #[serde(default)]
    pub language: Option<String>,
    /// Account (user id) to download with; `None`: the game's launch account.
    #[serde(default)]
    pub account: Option<String>,
    /// Who queued it (gui, cli, agent, auto-update…).
    pub source: String,
    pub added: DateTime<Utc>,
    #[serde(default)]
    pub error: Option<String>,
    /// Bytes done and in total for the current phase (download, or hashing while verifying).
    #[serde(default)]
    pub done_bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
    /// Current download speed in bytes per second.
    #[serde(default)]
    pub speed: u64,
    /// Slice downloads tried again so far (connection trouble, expired URLs).
    #[serde(default)]
    pub retries: u64,
}

/// Download settings (`config.json` → `downloads`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadSettings {
    /// Downloads that run at the same time.
    #[serde(default = "one")]
    pub max_active: usize,
    /// Bytes per second across all downloads; `None`: unlimited.
    #[serde(default)]
    pub bandwidth_limit: Option<u64>,
    /// Queue updates for installed games when the library shows one.
    #[serde(default)]
    pub auto_update: bool,
}

fn one() -> usize {
    1
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self { max_active: 1, bandwidth_limit: None, auto_update: false }
    }
}

/// Download and install size of one language's own chunks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LanguageSize {
    pub code: String,
    pub download_bytes: u64,
    pub install_bytes: u64,
}

/// What installing (or updating) a game would take, without starting it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPreview {
    pub product_id: u32,
    pub name: String,
    pub dest: PathBuf,
    pub version: Option<String>,
    /// Language the sizes are for.
    pub language: String,
    /// Languages the build offers, with the size each one's own files add.
    pub languages: Vec<LanguageSize>,
    /// The whole game in this language.
    pub download_bytes: u64,
    pub install_bytes: u64,
    /// Still to download and to write, after what is already on disk (resume, update).
    pub remaining_download_bytes: u64,
    pub required_bytes: u64,
    /// Free space on the library's filesystem, and the safety margin kept on top.
    pub free_bytes: u64,
    pub margin_bytes: u64,
    pub fits: bool,
    /// Set when the game is installed: what an update changes.
    pub update: Option<UpdatePreview>,
}

/// What an update changes between the installed and the latest build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePreview {
    pub product_id: u32,
    pub name: String,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub up_to_date: bool,
    pub download_bytes: u64,
    /// Bytes of files that will be (re)written.
    pub write_bytes: u64,
    pub changed: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorruptFile {
    pub path: String,
    pub size: u64,
    pub expected_size: u64,
    /// Slices whose SHA-1 doesn't match, and their bytes.
    pub bad_slices: usize,
    pub bad_bytes: u64,
}

/// Installed files hashed against the game's manifest. Files that aren't in the
/// manifest (mods, extra DLLs, configs) are neither checked nor touched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VerifyReport {
    pub product_id: u32,
    pub at: Option<DateTime<Utc>>,
    pub version: Option<String>,
    pub files: usize,
    pub bytes: u64,
    pub ok: usize,
    pub missing: Vec<String>,
    pub corrupt: Vec<CorruptFile>,
    /// Files whose slices carry no usable hash.
    #[serde(default)]
    pub unverified: Vec<String>,
    /// Set by a repair: bytes downloaded and files rewritten.
    #[serde(default)]
    pub repaired_files: usize,
    #[serde(default)]
    pub repaired_bytes: u64,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.missing.is_empty() && self.corrupt.is_empty()
    }
}
