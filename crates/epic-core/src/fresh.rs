//! Keeping Epic cloud saves fresh between sessions (design/STORE-EPIC.md §9.12), so a game
//! is ready to play offline with the newest saves: quiverd calls [`Epic::freshen`] for each
//! installed Epic game now and then, and when the network comes back or the library is
//! read. Each call:
//!
//! - stands aside for games being played (and syncs nobody waits for never run while one
//!   is: `before_launch` to `after_exit`), for games whose saves sync in game, and for
//!   prefixes a launch hasn't set up yet (nothing is written into a prefix before that);
//! - lists the cloud (one cheap call) and stops there when its newest version is the one
//!   the last sync left (the base), or the same as at the last look with this machine's
//!   files unchanged; only otherwise is the newest manifest read and the pre-launch
//!   download run, with the pre-launch rules: this machine's copies backed up first, a
//!   conflict left alone and reported, the base moved only after the download;
//! - never uploads, with one exception: a session's upload that failed at exit (offline)
//!   is retried, and only once the person's first yes for uploads was given;
//! - backs off after failures (doubling from [`EVERY`] up to [`BACKOFF_MAX`]).
//!
//! Its state is per game and account in `saves/<app>.json` ([`Fresh`]), shown in the
//! Saves tab and `quiver epic saves <game>`.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use quiver_api::epicgame::{SavesFreshness, SyncReport};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

use crate::Epic;
use crate::saves::{Location, Trigger, manifest_name, scan};

/// How often each game's cloud is checked.
pub const EVERY: Duration = Duration::from_secs(30 * 60);
/// The longest wait between checks after failures.
pub const BACKOFF_MAX: Duration = Duration::from_secs(6 * 3600);
/// A nudge (network back, a library read, the daemon starting) checks a game again only
/// this long after its last check.
pub const NUDGE_GAP: Duration = Duration::from_secs(5 * 60);

/// What a background check last saw of the cloud and this machine: the same again means
/// the same answer, without reading the manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Looked {
    pub version: String,
    /// SHA-1 over this machine's files (path and hash) then.
    pub local: String,
}

/// A game's background state for one account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fresh {
    /// The last check that ran (successful or not).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempted_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
    /// The newest cloud version at the last check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub looked: Option<Looked>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub downloaded: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub failures: u32,
    /// A session's upload that failed at exit, since then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_pending: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_error: Option<String>,
}

impl Fresh {
    /// When the next scheduled check is due: [`EVERY`] after the last, doubling with each
    /// failure in a row up to [`BACKOFF_MAX`]. `None`: never checked, due now.
    pub fn next_due(&self) -> Option<DateTime<Utc>> {
        let last = self.attempted_at?;
        let wait = EVERY.saturating_mul(1u32 << self.failures.min(8)).min(BACKOFF_MAX.max(EVERY));
        Some(last + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::hours(6)))
    }

    /// Whether a check should run now. A nudge skips the wait (and the backoff: the
    /// network is back, or Epic just answered), not the [`NUDGE_GAP`].
    pub fn due(&self, now: DateTime<Utc>, nudge: bool) -> bool {
        let Some(last) = self.attempted_at else { return true };
        if nudge {
            return now - last >= chrono::Duration::from_std(NUDGE_GAP).unwrap_or_default();
        }
        self.next_due().is_none_or(|d| now >= d)
    }
}

/// What [`Epic::freshen`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not checked, and why (in game, running, prefix not set up yet, not due).
    Skipped(String),
    /// Nothing newer in the cloud.
    UpToDate,
    /// Newer files came down (this machine's copies backed up first: `report.backup`).
    Downloaded(SyncReport),
    /// The same files changed here and in the cloud: nothing moved. `new`: not seen at the
    /// last check.
    Conflict { files: Vec<String>, new: bool },
    /// A session's pending upload went up.
    Uploaded(SyncReport),
    /// Something waits on the person (the first yes, or the game started meanwhile).
    Held(String),
}

/// The newest manifest's version name in a cloud listing.
pub(crate) fn newest_version(files: &std::collections::HashMap<String, crate::saves::CloudFile>) -> Option<String> {
    files
        .iter()
        .filter_map(|(k, f)| manifest_name(k).map(|n| (n, f.last_modified)))
        .max_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)))
        .map(|(n, _)| n.to_owned())
}

/// A fingerprint of the save folder's files.
pub(crate) fn fingerprint(local: &std::collections::BTreeMap<String, crate::saves::LocalFile>) -> String {
    let text: String = local.iter().map(|(p, f)| format!("{p}\0{}\n", f.sha1)).collect();
    hex::encode(crate::savefmt::sha1(text.as_bytes()))
}

impl Epic {
    /// Turn keeping saves fresh between sessions on or off (quiverd's setting).
    pub fn set_background_saves(&self, on: bool) {
        self.p3.background.store(on, Ordering::Relaxed);
    }

    pub fn background_saves(&self) -> bool {
        self.p3.background.load(Ordering::Relaxed)
    }

    /// Library reads that asked Epic, this run.
    pub fn library_reads(&self) -> u64 {
        self.p3.library_reads.load(Ordering::Relaxed)
    }

    pub(crate) fn is_playing(&self, app: &str) -> bool {
        self.p3.playing.lock().unwrap().contains(app)
    }

    pub(crate) fn set_playing(&self, app: &str, on: bool) {
        let mut p = self.p3.playing.lock().unwrap();
        if on {
            p.insert(app.to_owned());
        } else {
            p.remove(app);
        }
    }

    pub(crate) fn fresh_of(&self, app: &str, account: &str) -> Fresh {
        self.game_state(app).ok().and_then(|s| s.fresh.get(account).cloned()).unwrap_or_default()
    }

    fn edit_fresh(&self, app: &str, account: &str, f: impl FnOnce(&mut Fresh)) -> Result<()> {
        self.edit_state(app, |st| f(st.fresh.entry(account.to_owned()).or_default()))
    }

    /// A session's upload failed at exit: retried between sessions (once uploads are
    /// allowed).
    pub(crate) fn mark_upload_pending(&self, app: &str, account: &str, why: &str) -> Result<()> {
        self.edit_fresh(app, account, |f| {
            f.upload_pending.get_or_insert_with(Utc::now);
            f.upload_error = Some(why.to_owned());
        })
    }

    pub(crate) fn clear_upload_pending(&self, app: &str, account: &str) -> Result<()> {
        if self.fresh_of(app, account).upload_pending.is_none() {
            return Ok(());
        }
        self.edit_fresh(app, account, |f| {
            f.upload_pending = None;
            f.upload_error = None;
        })
    }

    /// The Saves tab's view of a game's background state.
    pub(crate) fn freshness(&self, app: &str, account: &str, note: Option<String>) -> SavesFreshness {
        let f = self.fresh_of(app, account);
        SavesFreshness {
            enabled: self.background_saves(),
            checked_at: f.checked_at,
            cloud_version: f.cloud_version.clone(),
            downloaded_at: f.downloaded_at,
            downloaded: f.downloaded.clone(),
            conflicts: f.conflicts.clone(),
            error: f.error.clone(),
            failures: f.failures,
            next_check: self.background_saves().then(|| f.next_due().unwrap_or_else(Utc::now)),
            upload_pending: f.upload_pending,
            upload_error: f.upload_error.clone(),
            note,
        }
    }

    /// Why a game's saves can't be checked between sessions, if they can't.
    pub(crate) fn not_checked(&self, loc: &Location) -> Option<String> {
        if self.is_playing(&loc.app) {
            return Some("the game is being played: its saves sync when it exits".into());
        }
        let place = self.game_state(&loc.app).ok().and_then(|s| s.place)?;
        if !place.prefix.join(quiver_core::launcher::PLAIN_PREFIX_READY).exists() {
            return Some("its prefix isn't set up yet: the first launch downloads the saves".into());
        }
        None
    }

    /// One background check of a game's cloud saves (the rules are the module's). `nudge`:
    /// the network came back, the library was read or the daemon started. Errors are
    /// recorded (and back the next check off) as well as returned.
    pub async fn freshen(&self, app: &str, nudge: bool) -> Result<Outcome> {
        let now = Utc::now();
        // Cheap checks first (this runs often): the game's small state file only.
        let st = self.game_state(app)?;
        if st.place.is_none() {
            return Ok(Outcome::Skipped("not installed or launched through Quiver yet".into()));
        }
        if !st.fresh.is_empty() && st.fresh.values().all(|f| !f.due(now, nudge)) {
            return Ok(Outcome::Skipped("not due".into()));
        }
        let (spec, (account, _), loc) = match self.locate(app).await {
            Ok(x) => x,
            Err(e) => {
                // Not placeable now (signed out, not in the library): asked again later.
                self.edit_fresh(app, "", |f| {
                    f.attempted_at = Some(now);
                    f.failures = f.failures.saturating_add(1);
                })?;
                return Err(e);
            }
        };
        // Skips that won't change soon wait for the next scheduled check.
        let skip = |why: String| -> Result<Outcome> {
            self.edit_fresh(app, &account, |f| f.attempted_at = Some(now))?;
            Ok(Outcome::Skipped(why))
        };
        if spec.template.is_none() {
            return skip("saves sync in game".into());
        }
        let loc = match loc {
            Ok(l) => l,
            Err(why) => return skip(why),
        };
        let mut f = self.fresh_of(app, &account);
        if !f.due(now, nudge) {
            return Ok(Outcome::Skipped("not due".into()));
        }
        if let Some(why) = self.not_checked(&loc) {
            return skip(why);
        }
        f.attempted_at = Some(now);
        let result = self.freshen_now(&loc, &mut f).await;
        match &result {
            Ok(_) => {
                f.checked_at = Some(Utc::now());
                f.failures = 0;
                f.error = None;
            }
            Err(e) => {
                f.failures = f.failures.saturating_add(1);
                f.error = Some(format!("{e:#}"));
            }
        }
        // Whether a session's upload is still pending is the file's (syncs clear it, exits
        // set it); the error is this check's when it has one.
        let disk = self.fresh_of(app, &account);
        f.upload_pending = disk.upload_pending;
        if f.upload_pending.is_none() {
            f.upload_error = None;
        } else if f.upload_error.is_none() {
            f.upload_error = disk.upload_error;
        }
        self.edit_fresh(app, &account, |x| *x = f)?;
        result
    }

    async fn freshen_now(&self, loc: &Location, f: &mut Fresh) -> Result<Outcome> {
        // A session's upload that couldn't go at exit: finished now, once uploads are
        // allowed (the same sync an exit runs: download what's newer, then upload).
        if f.upload_pending.is_some() && self.writes()?.saves.open() {
            match self.sync_saves(loc, Trigger::Retry).await {
                Ok(r) if !r.conflicts.is_empty() => {
                    let new = f.conflicts != r.conflicts;
                    f.conflicts = r.conflicts.clone();
                    return Ok(Outcome::Conflict { files: r.conflicts, new });
                }
                Ok(r) if r.held.is_some() => return Ok(Outcome::Held(r.held.unwrap_or_default())),
                Ok(r) => {
                    f.conflicts.clear();
                    f.upload_pending = None;
                    f.upload_error = None;
                    if !r.downloaded.is_empty() {
                        f.downloaded_at = Some(Utc::now());
                        f.downloaded = r.downloaded.clone();
                    }
                    f.cloud_version = r.version.clone().or(f.cloud_version.take());
                    return Ok(Outcome::Uploaded(r));
                }
                Err(e) => {
                    f.upload_error = Some(format!("{e:#}"));
                    return Err(e);
                }
            }
        }
        let token = self.access_token(&loc.account_id).await?;
        let files = self.api.savesync_list(token.expose(), &loc.account_id, &loc.app).await?;
        let newest = newest_version(&files);
        f.cloud_version = newest.clone();
        let Some(newest) = newest else {
            f.conflicts.clear();
            return Ok(Outcome::UpToDate);
        };
        let base = self.game_state(&loc.app)?.base.get(&loc.account_id).cloned();
        if base.as_ref().and_then(|b| b.version.as_deref()) == Some(newest.as_str()) {
            // The cloud hasn't moved since the last sync: nothing can be newer there.
            f.conflicts.clear();
            return Ok(Outcome::UpToDate);
        }
        let local = fingerprint(&scan(&loc.dir, &loc.filter)?);
        if f.looked.as_ref().is_some_and(|l| l.version == newest && l.local == local) {
            // Same cloud, same files as at the last look: the same answer.
            return Ok(if f.conflicts.is_empty() { Outcome::UpToDate } else { Outcome::Conflict { files: f.conflicts.clone(), new: false } });
        }
        let r = self.sync_saves(loc, Trigger::Background).await?;
        f.looked = Some(Looked { version: newest, local: fingerprint(&scan(&loc.dir, &loc.filter)?) });
        if !r.conflicts.is_empty() {
            let new = f.conflicts != r.conflicts;
            f.conflicts = r.conflicts.clone();
            return Ok(Outcome::Conflict { files: r.conflicts, new });
        }
        f.conflicts.clear();
        if let Some(h) = &r.held {
            return Ok(Outcome::Held(h.clone()));
        }
        if r.downloaded.is_empty() {
            return Ok(Outcome::UpToDate);
        }
        f.downloaded_at = Some(Utc::now());
        f.downloaded = r.downloaded.clone();
        Ok(Outcome::Downloaded(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_and_backoff() {
        let t0 = Utc::now();
        let mut f = Fresh::default();
        assert!(f.due(t0, false) && f.due(t0, true) && f.next_due().is_none());
        f.attempted_at = Some(t0);
        let min = |m: i64| chrono::Duration::minutes(m);
        assert!(!f.due(t0 + min(29), false) && f.due(t0 + min(30), false));
        // A nudge skips the wait, not the gap.
        assert!(!f.due(t0 + min(4), true) && f.due(t0 + min(5), true));
        // Failures double the wait, up to six hours.
        f.failures = 1;
        assert_eq!(f.next_due(), Some(t0 + min(60)));
        f.failures = 3;
        assert_eq!(f.next_due(), Some(t0 + min(240)));
        f.failures = 30;
        assert_eq!(f.next_due(), Some(t0 + min(360)));
        assert!(f.due(t0 + min(6), true), "the network back skips the backoff");
    }

    #[test]
    fn newest_by_name() {
        let mut files = std::collections::HashMap::new();
        for k in ["a/b/app/manifests/2026.11.17-10.00.00.manifest", "a/b/app/manifests/2026.11.18-09.00.00.manifest", "a/b/app/ChunksV4/00/x.chunk"] {
            files.insert(k.to_owned(), crate::saves::CloudFile::default());
        }
        assert_eq!(newest_version(&files).as_deref(), Some("2026.11.18-09.00.00"));
        assert_eq!(newest_version(&Default::default()), None);
    }
}
