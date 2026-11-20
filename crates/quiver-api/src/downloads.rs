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
    /// To the top and running now: the same as `Request::QueueRunNow` (a running download
    /// may be switched out for it).
    Now,
}

impl std::str::FromStr for QueueMove {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "top" => Ok(Self::Top),
            "now" => Ok(Self::Now),
            _ => Err(format!("expected up, down, top or now, got '{s}'")),
        }
    }
}

/// What `Request::QueueRunNow` did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunNow {
    /// A slot was free: it started.
    Started,
    /// It was downloading already.
    AlreadyRunning,
    /// The download that ran is stopping at a safe point (its finished slices and files are
    /// kept) and goes back to the queue right behind this one, which starts once it stopped.
    Switching { from: crate::GameRef, from_name: String },
    /// A download is finishing (registering its install, or already stopping); this one
    /// starts as soon as it's done.
    Waiting { for_name: String },
}

impl RunNow {
    /// In words for people, of the game `name`.
    pub fn message(&self, name: &str) -> String {
        match self {
            Self::Started => format!("Downloading {name} now"),
            Self::AlreadyRunning => format!("{name} is already downloading"),
            Self::Switching { from_name, .. } => format!("Switching to {name}: {from_name} goes back to the queue, keeping its progress"),
            Self::Waiting { for_name } => format!("{name} is next: it starts as soon as {for_name} finishes"),
        }
    }
}

/// "When it finishes": what quiverd does once an install completes (Ubisoft and Epic
/// games). Kept on the queue item, so a restart of the app or of quiverd keeps it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AfterInstall {
    /// Add a Steam shortcut (staged until Steam restarts while Steam runs; quiverd never
    /// restarts Steam).
    #[serde(default)]
    pub steam: bool,
    /// Write a .desktop entry.
    #[serde(default)]
    pub desktop: bool,
}

impl AfterInstall {
    pub fn is_empty(&self) -> bool {
        !self.steam && !self.desktop
    }
}

/// How the lines an install's job carries about what [`AfterInstall`] did (in its
/// `detail`) start, so front ends can tell them from the install's own notes.
pub const AFTER_STEAM: &str = "Steam: ";
pub const AFTER_DESKTOP: &str = ".desktop entry: ";

/// Whether a job's detail line says what [`AfterInstall`] did.
pub fn is_after_note(line: &str) -> bool {
    line.starts_with(AFTER_STEAM) || line.starts_with(AFTER_DESKTOP)
}

/// One item in the download queue. The queue is in order: items start from the top,
/// `max_active` at a time; paused and failed items keep their place but are skipped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueEntry {
    /// The Ubisoft product; 0 for another store's game ([`QueueEntry::game`] names it).
    pub product_id: u32,
    /// The game when it isn't a Ubisoft product (`epic:<app>`). Never written for Ubisoft
    /// games, so their queue lines are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<crate::GameRef>,
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
    /// What quiverd does when the install finishes (left out when nothing).
    #[serde(default, skip_serializing_if = "AfterInstall::is_empty")]
    pub after: AfterInstall,
    /// Set while the item waits to try again after a failure that may pass (the
    /// connection to the store dropped, a server error): it keeps its place and its slot
    /// (`state` stays `downloading`, so the next item doesn't start), and goes again at
    /// `at`. Left out otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryWait>,
}

/// A queue item waiting to try again ([`QueueEntry::retry`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryWait {
    /// When it tries again.
    pub at: DateTime<Utc>,
    /// Tries that failed in a row (1 for the first wait).
    pub attempt: u32,
    /// What failed last.
    pub error: String,
    /// When it gives up (and fails) should it keep failing without progress.
    pub until: DateTime<Utc>,
}

impl RetryWait {
    /// Whole seconds left until the next try (0 once due).
    pub fn secs_left(&self, now: DateTime<Utc>) -> u64 {
        ((self.at - now).num_milliseconds().max(0) as u64).div_ceil(1000)
    }

    /// For people: "Reconnecting… retry in 12s".
    pub fn line(&self, now: DateTime<Utc>) -> String {
        match self.secs_left(now) {
            0 => "Reconnecting…".into(),
            s if s < 120 => format!("Reconnecting… retry in {s}s"),
            s => format!("Reconnecting… retry in {}m {}s", s / 60, s % 60),
        }
    }
}

impl QueueEntry {
    /// The game: [`QueueEntry::game`], else the Ubisoft product.
    pub fn game_ref(&self) -> crate::GameRef {
        self.game.unwrap_or(crate::GameRef::ubisoft(self.product_id))
    }
}

/// A queue item's throughput as quiverd sampled it (`Request::DownloadThroughput`): one
/// sample every `interval_ms`, oldest first, the newest ending at `at`. quiverd keeps the
/// last [`THROUGHPUT_KEEP`] samples per item while it is in the queue (pauses included) and
/// drops them when it leaves; a restart of quiverd starts over.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Throughput {
    pub game: crate::GameRef,
    pub interval_ms: u64,
    /// When the newest sample ended.
    pub at: DateTime<Utc>,
    pub samples: Vec<ThroughputSample>,
}

/// Samples quiverd keeps per queue item: ten minutes at one a second.
pub const THROUGHPUT_KEEP: usize = 600;

/// One interval of a [`Throughput`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThroughputSample {
    /// Bytes received from the network in the interval (hashed, while verifying); what an
    /// earlier run left on disk never counts. 0 in a gap ([`SampleState::is_gap`]).
    pub bytes: u64,
    pub state: SampleState,
    /// Slice downloads tried again during the interval.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retries: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What an item was doing during a [`ThroughputSample`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SampleState {
    /// Bytes came in.
    Downloading,
    /// Running, but nothing came in (a slow server, URLs being fetched, files being set
    /// up or written).
    Stalled,
    /// Slices were waiting to be tried again (connection trouble).
    Retrying,
    /// Hashing installed files (`bytes` hashed).
    Verifying,
    /// Not running because it was paused. A gap, not a zero.
    Paused,
    /// Not running: waiting its turn, for the game to exit, or after a failure. A gap.
    Waiting,
    /// A state from a newer quiverd.
    #[serde(other)]
    Other,
}

impl SampleState {
    /// Nothing ran: draw nothing rather than a zero.
    pub fn is_gap(self) -> bool {
        matches!(self, Self::Paused | Self::Waiting)
    }
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
    /// The game when it isn't a Ubisoft product (`product_id` is then 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<crate::GameRef>,
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
    /// The game when it isn't a Ubisoft product (`product_id` is then 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<crate::GameRef>,
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
    /// How the update keeps the download small, in words for people ("Epic's delta patch
    /// 1.0 → 1.1 is used", "1.2 GiB of the changed files is copied from their installed
    /// copies"), shown under the numbers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
    /// The game when it isn't a Ubisoft product (`product_id` is then 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<crate::GameRef>,
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
