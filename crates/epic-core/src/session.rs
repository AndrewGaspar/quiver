//! What happens around a session of an Epic game (design/STORE-EPIC.md §9.5): the hooks the
//! launch calls, as `launcher::play` syncs Ubisoft saves around its sessions.
//!
//! ```text
//! before the game starts   Epic::before_launch   remember the prefix, pull saves (backups first)
//! while it runs            Poller               new achievement unlocks, every few minutes
//! after it exits           Epic::after_exit     last unlock check, queue and send the play
//!                                               session, push saves
//! ```
//!
//! None of it stops a launch: a cloud that can't be reached, or saves in conflict, are
//! reported in the launch's status and the game starts with the saves on this machine.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use quiver_api::epicgame::{QueuedSession, SyncReport};

use crate::Epic;
use crate::achievements::Unlock;
use crate::playtime::Played;
use crate::saves::{Location, Trigger};

/// How often a running game's achievements are read.
pub const POLL_EVERY: Duration = Duration::from_secs(5 * 60);

/// What [`Epic::before_launch`] found.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub app: String,
    /// The account the game's saves, play time and achievements are for.
    pub account_id: Option<String>,
    /// `None`: the game's saves sync in game (or its folder couldn't be placed: `note`).
    pub location: Option<Location>,
    pub saves: SyncReport,
    /// For the launch's status: "saves in sync", "cloud saves not synced: …".
    pub note: String,
}

/// What [`Epic::after_exit`] did.
#[derive(Debug, Clone, Default)]
pub struct Finished {
    pub saves: Option<std::result::Result<SyncReport, String>>,
    pub playtime: Option<QueuedSession>,
    pub unlocks: Vec<Unlock>,
    pub note: String,
}

impl Epic {
    /// Before an Epic game starts in `prefix`: download what's newer in the cloud.
    pub async fn before_launch(&self, app: &str, prefix: &Path, install_dir: Option<&Path>) -> Prepared {
        // From here to `after_exit`, background syncs leave the game's saves alone.
        self.set_playing(app, true);
        let account_id = match self.title_of(app).await.and_then(|t| self.owner_of(&t)) {
            Ok((id, _)) => Some(id),
            Err(e) => {
                tracing::info!(app, "Epic account for the session: {e:#}");
                None
            }
        };
        let mut p = Prepared { app: app.to_owned(), account_id, location: None, saves: SyncReport::default(), note: String::new() };
        match self.launch_location(app, prefix, install_dir).await {
            Ok(None) => {
                p.saves.in_game = true;
                p.note = "saves sync in game".into();
            }
            Ok(Some(loc)) => {
                match self.sync_saves(&loc, Trigger::BeforeLaunch).await {
                    Ok(r) => {
                        p.note = r.summary();
                        p.saves = r;
                    }
                    Err(e) => {
                        tracing::warn!(app, "Epic cloud saves before launch: {e:#}");
                        p.note = format!("cloud saves not synced: {e:#}");
                    }
                }
                p.location = Some(loc);
            }
            Err(e) => p.note = format!("cloud saves not synced: {e:#}"),
        }
        p
    }

    /// After the game exited: the last achievement check, the play session (queued, then
    /// sent when reporting is on), and saves uploaded.
    pub async fn after_exit(&self, prepared: &Prepared, played: Option<Played>, report_playtime: bool) -> Finished {
        let mut f = Finished::default();
        let mut notes = Vec::new();
        if let Some(account) = &prepared.account_id {
            match self.new_unlocks(&prepared.app, account).await {
                Ok(u) => f.unlocks = u,
                Err(e) => tracing::info!(app = prepared.app, "Epic achievements at exit: {e:#}"),
            }
        }
        if let Some(p) = played {
            match self.queue_session(&p, report_playtime).await {
                Ok(q) => {
                    if let Err(e) = self.send_queue(report_playtime, false).await {
                        tracing::info!("Epic play-time reports: {e:#}");
                    }
                    let q = crate::playtime::Queue::load(self.epic_dir()).ok().and_then(|x| x.sessions.into_iter().find(|s| s.id == q.id)).unwrap_or(q);
                    f.playtime = Some(q);
                }
                Err(e) => tracing::warn!("queue the Epic play session: {e:#}"),
            }
        }
        if let Some(loc) = &prepared.location {
            let r = self.sync_saves(loc, Trigger::AfterExit).await.map_err(|e| format!("{e:#}"));
            notes.push(match &r {
                Ok(r) => r.summary(),
                Err(e) if self.writes().is_ok_and(|w| w.saves.open()) => format!("cloud saves not uploaded: {e} (tried again in the background once Epic answers)"),
                Err(e) => format!("cloud saves not synced: {e} (tried again in the background once Epic answers and you've said yes to uploads)"),
            });
            if let Err(e) = &r
                && let Err(x) = self.mark_upload_pending(&loc.app, &loc.account_id, e)
            {
                tracing::warn!(app = loc.app, "remembering the pending save upload: {x:#}");
            }
            f.saves = Some(r);
        }
        self.set_playing(&prepared.app, false);
        f.note = notes.join("; ");
        f
    }
}

/// Polls a running game's achievements until dropped (or [`Poller::stop`]).
pub struct Poller {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Poller {
    /// Read the game's unlocks now (to record what's there) and every `every`;
    /// `on_unlock` gets each new one once.
    pub fn start(epic: Arc<Epic>, app: String, account: String, every: Duration, on_unlock: impl Fn(Unlock) + Send + 'static) -> Poller {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            loop {
                match epic.new_unlocks(&app, &account).await {
                    Ok(list) => list.into_iter().for_each(&on_unlock),
                    Err(e) => tracing::info!(app, "Epic achievements: {e:#}"),
                }
                tokio::select! {
                    _ = &mut rx => return,
                    _ = tokio::time::sleep(every) => {}
                }
            }
        });
        Poller { stop: Some(tx) }
    }

    pub fn stop(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

/// A sync on request (`EpicGameRequest::SavesSync`).
pub async fn sync_now(epic: &Epic, app: &str, trigger: Trigger) -> Result<SyncReport> {
    let loc = epic.location(app).await?;
    epic.sync_saves(&loc, trigger).await
}
