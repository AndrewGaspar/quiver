//! Epic games' cloud saves, play time and achievements in quiverd (design/STORE-EPIC.md
//! §9.5): `EpicRequest::Game`, the session hooks an Epic launch runs, and the play-time
//! queue's retries. The work is `epic_core`'s (`saves`, `playtime`, `achievements`,
//! `session`).
//!
//! **For the launch (stage 2).** Around the game's process:
//!
//! ```ignore
//! let s = epicgame::begin(&daemon, app, &prefix, Some(&install_dir), session_id).await;
//! job.status = s.note().to_owned();         // "saves in sync", "2 save file(s) downloaded", …
//! // … run the game …
//! let note = epicgame::end(&daemon, s).await; // play time queued and sent, saves pushed
//! ```
//!
//! `begin` downloads what's newer in the cloud (backing up this machine's copies first)
//! and starts polling achievements; `end` checks them once more, queues the session for
//! Epic (`report_playtime`) and uploads the saves. Neither fails a launch.
//!
//! **Who may do what** (STORES.md §7 item 10): reading is open to everyone. The first
//! upload of saves and the first play-time report are writes to the person's Epic account,
//! so they need the person (`confirmed` from the app or a terminal); after that they happen
//! on their own. Settling a conflict or restoring a backup needs a person's OK for agents.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use chrono::Utc;
use epic_core::playtime::Played;
use epic_core::saves::Trigger;
use epic_core::session::{POLL_EVERY, Poller, Prepared};
use quiver_api::epicgame::{EpicGameRequest, EpicPlaytime};
use quiver_api::{Event, GameRef, StoreId};
use serde_json::{Value, json};

use crate::Daemon;
use crate::info::needs_approval;

/// How often queued play sessions are retried.
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn epic_app(game: &GameRef) -> Result<&str> {
    if game.store != StoreId::Epic {
        bail!("{game} is not an Epic game");
    }
    Ok(game.id.as_str())
}

fn person(source: &str) -> bool {
    quiver_api::mods::is_human_source(source)
}

/// At start: backups go where Ubisoft's do; queued play sessions are retried now and then.
pub(crate) fn start(d: &Arc<Daemon>) {
    d.epic.set_save_backups(d.cfg.save_backups.clone());
    let d = d.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        loop {
            if d.epic.accounts().is_ok_and(|a| !a.is_empty()) {
                match d.epic.send_queue(d.cfg.report_playtime, false).await {
                    Ok(sent) if !sent.is_empty() => tracing::info!("Epic play-time reports: {} sent or tried", sent.len()),
                    Ok(_) => {}
                    Err(e) => tracing::info!("Epic play-time reports: {e:#}"),
                }
            }
            tokio::time::sleep(RETRY_EVERY).await;
        }
    });
}

pub(crate) async fn handle(d: &Arc<Daemon>, source: &str, r: EpicGameRequest) -> Result<Value> {
    Ok(match r {
        EpicGameRequest::Saves { game, cloud } => json!(d.epic.saves_report(epic_app(&game)?, cloud).await?),
        EpicGameRequest::SavesSync { game, resolve, confirmed } => {
            let app = epic_app(&game)?;
            if d.running_epic(app) {
                bail!("{game} is running: its saves sync when it exits");
            }
            if resolve.is_some() && needs_approval(source) && !confirmed {
                bail!("choosing which saves to keep needs your approval: run `quiver epic saves {game} sync --keep local|cloud` yourself");
            }
            let first_ok = !d.epic.writes()?.saves.open() && confirmed && person(source);
            if !d.epic.writes()?.saves.open() && !first_ok && needs_upload(d, app).await {
                bail!(
                    "the first upload of saves to your Epic account needs your yes: run `quiver epic saves {game} sync --yes` yourself, or press Sync in the game's Saves (after that, uploads happen after each session)"
                );
            }
            let report = epic_core::session::sync_now(&d.epic, app, Trigger::Manual { resolve, first_upload_ok: first_ok }).await?;
            let name = d.game_name(game).await;
            if !report.uploaded.is_empty() || !report.downloaded.is_empty() {
                d.log_activity_game(source, format!("synced {name}'s Epic cloud saves: {}", report.summary()), Some(game));
            }
            json!(report)
        }
        EpicGameRequest::SavesRestore { game, backup, confirmed } => {
            let app = epic_app(&game)?;
            if needs_approval(source) && !confirmed {
                bail!("restoring saves needs your approval: run `quiver epic saves {game} restore {backup}` yourself");
            }
            if d.running_epic(app) {
                bail!("{game} is running: stop it before restoring its saves");
            }
            let (b, previous) = d.epic.restore_saves(app, &backup).await?;
            let name = d.game_name(game).await;
            d.log_activity_game(source, format!("restored {name}'s saves from backup {}", b.id), Some(game));
            json!({ "restored": b, "previous": previous })
        }
        EpicGameRequest::Playtime { game, refresh } => {
            if let Some(g) = &game {
                epic_app(g)?;
            }
            json!(d.epic.playtime_view(game.as_ref(), refresh, d.cfg.report_playtime).await?)
        }
        EpicGameRequest::PlaytimeSend { confirmed } => {
            if !d.cfg.report_playtime {
                bail!("play-time reporting is off (report_playtime in Quiver's config)");
            }
            let first_ok = !d.epic.writes()?.playtime.open() && confirmed && person(source);
            if !d.epic.writes()?.playtime.open() && !first_ok {
                bail!("the first play-time report to your Epic account needs your yes: run `quiver epic playtime send --yes` yourself (after that, sessions are reported when they end)");
            }
            let sent = d.epic.send_queue(true, first_ok).await?;
            if !sent.is_empty() {
                d.log_activity_game(source, format!("sent {} Epic play session(s)", sent.len()), None);
            }
            let mut v: EpicPlaytime = d.epic.playtime_view(None, true, true).await?;
            v.queue.retain(|q| sent.iter().any(|s| s.id == q.id));
            json!(v)
        }
        EpicGameRequest::Achievements { game, refresh } => json!(d.epic.achievements(epic_app(&game)?, refresh).await?),
    })
}

/// Whether a sync would upload anything (so the first-yes gate applies).
async fn needs_upload(d: &Daemon, app: &str) -> bool {
    match d.epic.saves_report(app, true).await {
        Ok(s) => s.files.iter().any(|f| matches!(f.state, quiver_api::epicgame::FileState::LocalNewer | quiver_api::epicgame::FileState::Conflict)),
        Err(_) => true,
    }
}

// Called by the Epic launch (stage 2's install-and-play branch), as the module docs show.
#[allow(dead_code)]
/// An Epic game's session as quiverd runs it: from [`begin`] to [`end`].
pub(crate) struct EpicSession {
    prepared: Prepared,
    poller: Option<Poller>,
    id: String,
    started: chrono::DateTime<Utc>,
}

// Called by the Epic launch (stage 2's install-and-play branch), as the module docs show.
#[allow(dead_code)]
impl EpicSession {
    /// What happened to the saves before the launch, for the job's status.
    pub(crate) fn note(&self) -> &str {
        &self.prepared.note
    }
}

// Called by the Epic launch (stage 2's install-and-play branch), as the module docs show.
#[allow(dead_code)]
/// Before an Epic game starts in `prefix`: pull saves, start watching achievements.
/// `session`: the launch's session id (the play-time report's id), when it has one.
pub(crate) async fn begin(d: &Arc<Daemon>, app: &str, prefix: &Path, install_dir: Option<&Path>, session: Option<String>) -> EpicSession {
    let prepared = d.epic.before_launch(app, prefix, install_dir).await;
    let game = epic_core::provider::game_ref(app);
    if !prepared.saves.downloaded.is_empty()
        && let Some(g) = game
    {
        d.log_activity_game("quiver", format!("downloaded {} Epic cloud save file(s) before launch", prepared.saves.downloaded.len()), Some(g));
    }
    let poller = prepared.account_id.clone().map(|account| {
        let d2 = d.clone();
        Poller::start(d.epic.clone(), app.to_owned(), account, POLL_EVERY, move |u| unlocked(&d2, u))
    });
    let started = Utc::now();
    let id = session.unwrap_or_else(|| format!("epic-{app}-{}", started.timestamp_millis()));
    EpicSession { prepared, poller, id, started }
}

// Called by the Epic launch (stage 2's install-and-play branch), as the module docs show.
#[allow(dead_code)]
/// After the game exited: last unlock check, play time to Epic, saves up. Returns a line
/// for the job's status.
pub(crate) async fn end(d: &Arc<Daemon>, mut s: EpicSession) -> String {
    if let Some(p) = s.poller.take() {
        p.stop();
    }
    let played = s.prepared.account_id.clone().map(|account_id| Played { id: s.id.clone(), app: s.prepared.app.clone(), account_id, started: s.started, ended: Utc::now() });
    let f = d.epic.after_exit(&s.prepared, played, d.cfg.report_playtime).await;
    for u in f.unlocks {
        unlocked(d, u);
    }
    f.note
}

// Called by the Epic launch (stage 2's install-and-play branch), as the module docs show.
#[allow(dead_code)]
/// A new unlock: a Sight toast (and the open page reloads), the activity log.
fn unlocked(d: &Arc<Daemon>, u: epic_core::achievements::Unlock) {
    let Some(game) = epic_core::provider::game_ref(&u.app) else { return };
    let d = d.clone();
    tokio::spawn(async move {
        let image = match &u.icon_url {
            Some(url) => d.epic.icon_file(url).await.map(|p| p.to_string_lossy().into_owned()),
            None => None,
        };
        d.log_activity_game("quiver", format!("achievement unlocked: {}", u.name), Some(game));
        d.emit(Event::AchievementUnlocked {
            product_id: 0,
            account: u.account_id.clone(),
            achievement_id: 0,
            name: Some(u.name.clone()),
            description: u.description.clone(),
            image,
            unlocked_at: u.unlocked_at,
            report: quiver_api::achievements::ReportState::ByGame,
            game: Some(game),
            key: Some(u.id.clone()),
        });
    });
}

impl Daemon {
    /// Whether an Epic game is being played (its session is in the local sessions).
    pub(crate) fn running_epic(&self, app: &str) -> bool {
        self.local.list().iter().any(|s| s.game.is_some_and(|g| g.store == StoreId::Epic && g.id.as_str() == app))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_epic_games() {
        assert!(epic_app(&GameRef::ubisoft(3539)).unwrap_err().to_string().contains("not an Epic game"));
        assert_eq!(epic_app(&GameRef::parse_ref("epic:Wren").unwrap()).unwrap(), "Wren");
        assert!(person("cli") && person("gui") && !person("agent:claude"));
    }
}
