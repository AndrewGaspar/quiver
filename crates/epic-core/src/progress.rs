//! Phase 3's shared pieces (design/STORE-EPIC.md §6): the state [`Epic`] keeps for cloud
//! saves ([`crate::saves`]), play time ([`crate::playtime`]) and achievements
//! ([`crate::achievements`]); which account a game's progress belongs to; the catalog
//! attributes those read; the one-time "yes" for writes to the person's Epic account; and
//! filling an Epic game's page with all three.
//!
//! Files, under `<data>/epic/`:
//!
//! ```text
//! writes.json                      the one-time yes for uploads and play-time reports
//! machine-id                       this machine's id for play-time reports (random, stable)
//! saves/<app>.json                 where the game's saves are, and the last sync per account
//! playtime-queue.json              sessions to report, and those reported (never sent twice)
//! achievements/<account>/<app>.json  unlocks already seen (toasts only for new ones)
//! cache/achievements-<ns>.json     definitions (a day)
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::epicgame::WriteGate;
use quiver_api::stores::{CloudState, GameDetail, StorePlaytime};
use serde::{Deserialize, Serialize};

use crate::Epic;
use crate::api::CatalogItem;
use crate::library::Title;

/// Phase 3's state inside [`Epic`].
pub struct Phase3 {
    /// Where save backups go (`launcher::Config::save_backups`, under `epic-<app>/`).
    pub(crate) backups: std::sync::Mutex<PathBuf>,
    /// One save sync per game at a time.
    pub(crate) syncing: std::sync::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// The play-time queue and achievement state are read and written under these.
    pub(crate) queue: tokio::sync::Mutex<()>,
    pub(crate) unlocks: tokio::sync::Mutex<()>,
    /// Epic's play-time totals per account, and when they were read.
    pub(crate) totals: tokio::sync::Mutex<HashMap<String, (DateTime<Utc>, HashMap<String, u64>)>>,
    /// The player's achievement records (`<account>/<sandbox>`), and when they were read.
    pub(crate) players: tokio::sync::Mutex<HashMap<String, (DateTime<Utc>, crate::achievements::PlayerRecord)>>,
}

impl Phase3 {
    pub(crate) fn new(epic_dir: &Path) -> Self {
        Phase3 {
            backups: std::sync::Mutex::new(epic_dir.join("save-backups")),
            syncing: Default::default(),
            queue: Default::default(),
            unlocks: Default::default(),
            totals: Default::default(),
            players: Default::default(),
        }
    }
}

/// The two kinds of account write that wait for the person's first yes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Writes {
    pub saves: WriteGate,
    pub playtime: WriteGate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Saves,
    Playtime,
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("corrupt {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Write through a temporary file, so a crash leaves the old file or the new one.
pub(crate) fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no parent")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn systime(t: std::time::SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// A catalog attribute, trimmed (`None` when empty).
pub fn attribute(item: &CatalogItem, key: &str) -> Option<String> {
    item.custom_attributes.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, a)| a.value.trim().to_owned()).filter(|v| !v.is_empty())
}

impl Epic {
    /// Where save backups go (Quiver's `save_backups` folder; quiverd sets it at start).
    pub fn set_save_backups(&self, dir: PathBuf) {
        *self.p3.backups.lock().unwrap() = dir;
    }

    pub(crate) fn epic_dir(&self) -> &Path {
        &self.dir
    }

    pub fn writes(&self) -> Result<Writes> {
        read_json(&self.dir.join("writes.json"))
    }

    /// Record the person's yes for a kind of write (from the app or a terminal only;
    /// quiverd checks the source).
    pub fn allow_writes(&self, kind: WriteKind) -> Result<()> {
        let mut w = self.writes()?;
        let g = match kind {
            WriteKind::Saves => &mut w.saves,
            WriteKind::Playtime => &mut w.playtime,
        };
        if g.allowed_at.is_none() {
            g.allowed_at = Some(Utc::now());
            write_json(&self.dir.join("writes.json"), &w)?;
        }
        Ok(())
    }

    pub(crate) fn mark_verified(&self, kind: WriteKind) -> Result<()> {
        let mut w = self.writes()?;
        let g = match kind {
            WriteKind::Saves => &mut w.saves,
            WriteKind::Playtime => &mut w.playtime,
        };
        if g.verified_at.is_none() {
            g.verified_at = Some(Utc::now());
            write_json(&self.dir.join("writes.json"), &w)?;
        }
        Ok(())
    }

    /// This machine's id for play-time reports: random once, then kept (STORES.md §7
    /// item 4: true and stable).
    pub fn machine_id(&self) -> Result<String> {
        let path = self.dir.join("machine-id");
        if let Ok(s) = std::fs::read_to_string(&path) {
            let s = s.trim();
            if s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(s.to_owned());
            }
        }
        let id = crate::savefmt::Guid::random()?.hex().to_lowercase();
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&path, &id)?;
        Ok(id)
    }

    /// A game in the library (from the last read when there is one).
    pub(crate) async fn title_of(&self, app: &str) -> Result<Title> {
        let lib = match self.cached_library().await {
            Some(l) if l.title(app).is_some() => l,
            _ => self.library(false).await?,
        };
        match lib.title(app) {
            Some(t) => Ok(t.clone()),
            None if self.accounts()?.is_empty() => bail!("no Epic account is signed in (quiver epic login)"),
            None => bail!("epic:{app} is not in your Epic library"),
        }
    }

    /// The game's catalog item, from the catalog cache.
    pub(crate) fn catalog_item(&self, t: &Title) -> Option<CatalogItem> {
        self.cache.catalog().remove(&crate::library::catalog_key(&t.namespace, &t.catalog_item_id)).map(|c| c.item)
    }

    /// The account a game's saves, play time and achievements are for: the default
    /// account when it owns the game, else the first that does.
    pub(crate) fn owner_of(&self, t: &Title) -> Result<(String, String)> {
        let accounts = self.accounts()?;
        let pick = accounts
            .iter()
            .find(|a| a.default && t.owners.contains(&a.account_id))
            .or_else(|| accounts.iter().find(|a| t.owners.contains(&a.account_id)))
            .or_else(|| t.owners.is_empty().then(|| accounts.iter().find(|a| a.default)).flatten());
        match pick {
            Some(a) => Ok((a.account_id.clone(), a.display_name.clone())),
            None => bail!("no signed-in Epic account owns {}", t.title),
        }
    }

    /// Fill an Epic game's page with its play time, achievements and cloud state, each as
    /// far as it can be read. What can't be read goes into `unavailable`.
    pub async fn fill_detail(&self, d: &mut GameDetail, refresh: bool) {
        let app = d.game.id.as_str().to_owned();
        d.unavailable.retain(|u| !matches!(u.as_str(), "play time" | "achievements" | "cloud saves"));
        match self.playtime_total(&app, refresh).await {
            Ok(Some(s)) => d.playtime = Some(StorePlaytime { total_seconds: s, two_weeks_seconds: None, last_played: self.last_played(&app) }),
            Ok(None) => d.playtime = Some(StorePlaytime { total_seconds: 0, two_weeks_seconds: None, last_played: self.last_played(&app) }),
            Err(e) => {
                tracing::info!(app, "Epic play time: {e:#}");
                d.unavailable.push("play time".into());
            }
        }
        match self.achievements(&app, refresh).await {
            Ok(r) => d.achievements = r.achievements,
            Err(e) => {
                tracing::info!(app, "Epic achievements: {e:#}");
                d.unavailable.push("achievements".into());
            }
        }
        match self.saves_report(&app, true).await {
            Ok(s) if s.mode == quiver_api::epicgame::SaveMode::Launcher => {
                let files = s.files.iter().filter(|f| f.cloud.is_some()).count();
                let bytes = s.files.iter().filter_map(|f| f.cloud.as_ref()).map(|c| c.size).sum();
                let unsynced = s.files.iter().filter(|f| f.state != quiver_api::epicgame::FileState::InSync).count();
                d.cloud = Some(CloudState { files, bytes, last_upload: s.cloud.as_ref().and_then(|c| c.at), unsynced });
            }
            Ok(_) => {}
            Err(e) => tracing::info!(app, "Epic cloud saves: {e:#}"),
        }
    }

    /// When the game last ended a session Quiver saw (its queue).
    fn last_played(&self, app: &str) -> Option<DateTime<Utc>> {
        crate::playtime::Queue::load(&self.dir).ok()?.sessions.iter().filter(|s| s.game.id.as_str() == app).map(|s| s.ended).max()
    }
}
