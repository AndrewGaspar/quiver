//! An Epic game's cloud saves, play time and achievements (`EpicRequest::Game`;
//! design/STORE-EPIC.md §6 phase 3, STORES.md §3.7–§3.9).
//!
//! - **Cloud saves.** Games whose catalog names a `CloudSaveFolder` keep their saves in
//!   Epic's launcher cloud (EGS savesync), which Quiver syncs: pulled before a launch
//!   (this machine's copies backed up first), pushed after it, with a conflict when both
//!   sides changed since the last sync; between sessions quiverd keeps them fresh,
//!   download only ([`SavesFreshness`]). One cloud store per game, the game's real save
//!   folder in its prefix. Games without one sync their saves in the game, through Epic
//!   Online Services, if at all ([`SaveMode::InGame`]).
//! - **Play time.** Epic's own total is the headline. Sessions are reported to Epic after
//!   they end (the `report_playtime` setting), each once, queued while offline.
//! - **Achievements.** Read from Epic's store (definitions and this account's unlocks); the
//!   game reports unlocks itself through EOS. They are in `GameDetail.achievements`.
//!
//! The first upload of saves and the first play-time report are writes to the person's
//! Epic account that wait for them to say yes once ([`WriteGate`]); after that they happen
//! on their own.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::game::GameRef;
use crate::stores::Achievements;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum EpicGameRequest {
    /// The game's saves here and in Epic's cloud, side by side, and its backups
    /// ([`EpicSaves`]). `cloud: false` reads only this machine.
    Saves {
        game: GameRef,
        #[serde(default = "yes")]
        cloud: bool,
    },
    /// Sync now ([`SyncReport`]): what's newer in the cloud comes down (this machine's
    /// copies backed up first), what's newer here goes up. `resolve` settles a conflict.
    /// The first upload ever, and any `resolve`, need a person (`confirmed`, from the app
    /// or a terminal); agents need `confirmed` for any upload.
    SavesSync {
        game: GameRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolve: Option<Resolve>,
        #[serde(default)]
        confirmed: bool,
    },
    /// Put a backup's files back (the files there now are backed up first). `backup` is a
    /// backup id, a unique part of one, or `latest`. Agents need `confirmed`.
    SavesRestore {
        game: GameRef,
        backup: String,
        #[serde(default)]
        confirmed: bool,
    },
    /// Epic's play-time total for a game (or every game) and the report queue
    /// ([`EpicPlaytime`]).
    Playtime {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        game: Option<GameRef>,
        #[serde(default)]
        refresh: bool,
    },
    /// Send queued play sessions now ([`EpicPlaytime`]). The first report ever needs a
    /// person (`confirmed`, from the app or a terminal).
    PlaytimeSend {
        #[serde(default)]
        confirmed: bool,
    },
    /// Drop sessions waiting to be reported (for a game, or every game) so they are never
    /// sent ([`EpicPlaytime`], its queue the sessions dropped).
    PlaytimeDiscard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        game: Option<GameRef>,
    },
    /// The game's achievements and this account's unlocks ([`EpicAchievements`]).
    Achievements {
        game: GameRef,
        #[serde(default)]
        refresh: bool,
    },
    /// Read (and with `enabled`, change) whether quiverd keeps Epic cloud saves fresh
    /// between sessions ([`SavesFreshness`]): `{ "enabled": bool }` (appended).
    SavesBackground {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
    },
}

fn yes() -> bool {
    true
}

/// How a conflict is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolve {
    /// This machine's files win: they are uploaded as a new cloud version (the older cloud
    /// versions stay in the cloud).
    KeepLocal,
    /// The cloud's files win: they are downloaded over this machine's, which are backed
    /// up first.
    KeepCloud,
}

/// Who syncs a game's saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveMode {
    /// Epic's launcher cloud, synced by Quiver.
    Launcher,
    /// "Saves sync in game": the catalog names no save folder for the launcher, so cloud
    /// saves, if the game has them, go through Epic Online Services inside the game.
    #[default]
    #[serde(other)]
    InGame,
}

/// A one-time "yes" for a kind of write to the person's Epic account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteGate {
    /// When the person first said yes (`None`: not yet; such writes wait).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_at: Option<DateTime<Utc>>,
    /// The first write that went through and was read back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<DateTime<Utc>>,
}

impl WriteGate {
    pub fn open(&self) -> bool {
        self.allowed_at.is_some()
    }
}

/// One side of a save file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveSide {
    pub size: u64,
    /// The local file's modification time; the cloud version's time for the cloud.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<DateTime<Utc>>,
    /// SHA-1, hex.
    pub sha1: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    InSync,
    /// Changed here since the last sync (or only here): the next sync uploads it.
    LocalNewer,
    /// Changed in the cloud since the last sync (or only there): the next sync downloads it.
    CloudNewer,
    /// Changed on both sides since the last sync: [`Resolve`] decides.
    Conflict,
    /// Deleted here, still in the cloud: kept there, not downloaded again (deletions aren't
    /// synced; the next upload leaves it out of the new version).
    DeletedHere,
    /// Not compared with the cloud.
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveFile {
    /// Relative to the save folder, `/`-separated.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<SaveSide>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<SaveSide>,
    pub state: FileState,
}

/// The game's saves as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    InSync,
    /// Uploads are waiting (and nothing new in the cloud).
    LocalNewer,
    /// Downloads are waiting (and nothing new here).
    CloudNewer,
    /// Different files changed on each side: a sync takes both, no question.
    Both,
    /// The same files changed on both sides: [`Resolve`] decides.
    Conflict,
    /// No saves on either side.
    Empty,
    #[default]
    #[serde(other)]
    Unknown,
}

/// A version of the game's saves in Epic's cloud (a save manifest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudVersion {
    /// Its name, the UTC time it was made: `2026.11.18-10.15.00`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub bytes: u64,
}

/// A copy of the save folder Quiver made before changing it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpicBackup {
    /// Pass it to restore: `20261118-101500`.
    pub id: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub bytes: u64,
    /// `before cloud download`, `before restoring 20261117-…`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The last sync of a game's saves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSync {
    pub at: DateTime<Utc>,
    /// The cloud version both sides matched then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `before launch`, `after exit`, `cli`, `gui` …
    #[serde(default)]
    pub by: String,
}

/// `EpicGameRequest::Saves`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpicSaves {
    pub game: GameRef,
    pub mode: SaveMode,
    /// The Epic account whose cloud this is, by name, and its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The catalog's `CloudSaveFolder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Where it is on this machine (in the game's prefix), once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// Why the folder isn't known here yet (not installed, a macOS-only path …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir_note: Option<String>,
    /// The catalog's include and exclude lists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Epic's size limit for the game's saves, when the catalog sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mb: Option<u64>,
    pub state: SyncState,
    #[serde(default)]
    pub files: Vec<SaveFile>,
    /// The newest cloud version, and how many there are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudVersion>,
    #[serde(default)]
    pub cloud_versions: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<LastSync>,
    #[serde(default)]
    pub backups: Vec<EpicBackup>,
    /// Uploads happen on their own once this is open.
    #[serde(default)]
    pub uploads: WriteGate,
    /// What a sync would do, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Between sessions: when the cloud was last checked and what came down (appended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<SavesFreshness>,
}

/// Background freshness of a game's cloud saves: between sessions quiverd checks Epic's
/// cloud now and then and downloads what is newer there (this machine's copies backed up
/// first; a conflict is left for the person), so the saves are ready when the game is
/// played offline. It never uploads, except to finish a session's upload that couldn't go
/// at exit (offline), once uploads are allowed ([`WriteGate`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavesFreshness {
    /// The setting (`epic_saves_background`, on by default). Off: saves sync only around
    /// launches and on request.
    pub enabled: bool,
    /// The last time the cloud was read in the background (successfully).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
    /// The newest cloud version then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_version: Option<String>,
    /// The last background download, and its files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downloaded: Vec<String>,
    /// Files a background check found changed on both sides (nothing synced until the
    /// person chooses).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// Why the last check failed (offline, signed out …), and how many in a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub failures: u32,
    /// When the next check is due (sooner when the network comes back or the library is
    /// read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check: Option<DateTime<Utc>>,
    /// A session's upload that didn't go at exit (since then), retried in the background
    /// once uploads are allowed; and why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_pending: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_error: Option<String>,
    /// Why the game isn't checked (its prefix isn't set up yet, it's running …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl EpicSaves {
    pub fn new(game: GameRef) -> Self {
        EpicSaves {
            game,
            mode: SaveMode::default(),
            account: None,
            account_id: None,
            folder: None,
            dir: None,
            dir_note: None,
            include: Vec::new(),
            exclude: Vec::new(),
            max_mb: None,
            state: SyncState::default(),
            files: Vec::new(),
            cloud: None,
            cloud_versions: 0,
            cloud_error: None,
            last_sync: None,
            backups: Vec::new(),
            uploads: WriteGate::default(),
            next: None,
            background: None,
        }
    }
}

/// `EpicGameRequest::SavesSync`, and what a launch's sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    #[serde(default)]
    pub downloaded: Vec<String>,
    #[serde(default)]
    pub uploaded: Vec<String>,
    /// The cloud version written by the upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Files left alone because both sides changed.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// The backup made before downloading over this machine's files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<EpicBackup>,
    /// Uploads that wait for the person's first yes (or for a conflict to be settled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// Nothing to sync: the catalog names no save folder (saves sync in game).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_game: bool,
}

impl SyncReport {
    /// One line for a launch job's status.
    pub fn summary(&self) -> String {
        if self.in_game {
            return "saves sync in game".into();
        }
        let mut parts = Vec::new();
        if !self.downloaded.is_empty() {
            parts.push(format!("{} save file(s) downloaded", self.downloaded.len()));
        }
        if !self.uploaded.is_empty() {
            parts.push(format!("{} uploaded", self.uploaded.len()));
        }
        if !self.conflicts.is_empty() {
            parts.push(format!("{} in conflict (see Saves)", self.conflicts.len()));
        }
        if let Some(h) = &self.held {
            parts.push(h.clone());
        }
        if parts.is_empty() { "saves in sync".into() } else { parts.join(", ") }
    }
}

/// Where a reported session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionReport {
    /// Waiting to be sent (offline, or not tried yet); retried.
    #[default]
    Queued,
    /// Waiting for the person's first yes ([`WriteGate`]).
    Held,
    /// `report_playtime` is off: kept here only.
    Off,
    /// Epic accepted it.
    Reported,
    /// Epic refused it (an error reply, not a lost connection); not retried.
    Failed,
    /// Sent, but no answer came back, so it may have counted: not sent again on its own,
    /// so it is never counted twice.
    Unconfirmed,
    /// The person dropped it (`quiver epic playtime --discard`): never sent.
    Discarded,
    #[serde(other)]
    Unknown,
}

/// A play session in the report queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedSession {
    /// Quiver's id for it (the launch session's id when there is one).
    pub id: String,
    pub game: GameRef,
    pub account_id: String,
    pub started: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub seconds: u64,
    pub state: SessionReport,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<DateTime<Utc>>,
}

/// `EpicGameRequest::Playtime`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpicPlaytime {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
    /// Epic's total for the game (seconds): the headline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_seconds: Option<u64>,
    /// Every game's total, when no game was asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub games: Vec<(String, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `report_playtime` (covers every store).
    #[serde(default)]
    pub reporting: bool,
    #[serde(default)]
    pub reports: WriteGate,
    /// The game's sessions (or all), newest first.
    #[serde(default)]
    pub queue: Vec<QueuedSession>,
}

/// `EpicGameRequest::Achievements`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpicAchievements {
    pub game: GameRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// `None`: Epic's store has no achievements for the game (or couldn't be read: see
    /// `errors`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievements: Option<Achievements>,
    /// XP: this account's, of the game's total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xp: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched: Option<DateTime<Utc>>,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl EpicAchievements {
    pub fn new(game: GameRef) -> Self {
        EpicAchievements { game, account: None, achievements: None, xp: None, fetched: None, errors: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire() {
        let r: crate::Request =
            serde_json::from_value(serde_json::json!({"method": "epic", "params": {"op": "game", "action": "saves_sync", "game": "epic:Wren", "resolve": "keep_cloud"}})).unwrap();
        let crate::Request::Epic(crate::epic::EpicRequest::Game(EpicGameRequest::SavesSync { game, resolve, confirmed })) = r else { panic!() };
        assert_eq!((game.to_string(), resolve, confirmed), ("epic:Wren".to_owned(), Some(Resolve::KeepCloud), false));
        let s: EpicSaves = serde_json::from_str(r#"{"game":"epic:Wren","mode":"later","state":"newer_kind"}"#).unwrap();
        assert_eq!((s.mode, s.state), (SaveMode::InGame, SyncState::Unknown));
        let q: QueuedSession =
            serde_json::from_str(r#"{"id":"s-1","game":"epic:Wren","account_id":"a","started":"2026-11-18T00:00:00Z","ended":"2026-11-18T01:00:00Z","seconds":3600,"state":"unconfirmed"}"#)
                .unwrap();
        assert_eq!(q.state, SessionReport::Unconfirmed);
        assert_eq!(SyncReport { in_game: true, ..Default::default() }.summary(), "saves sync in game");
        assert_eq!(SyncReport { downloaded: vec!["a".into()], ..Default::default() }.summary(), "1 save file(s) downloaded");
    }
}
