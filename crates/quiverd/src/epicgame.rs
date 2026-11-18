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
//! **Between sessions** ([`fresh_loop`], design/STORE-EPIC.md §9.12): each installed Epic
//! game that isn't running gets `Epic::freshen` every 30 minutes (backing off after
//! failures), and sooner when the network comes back, the library is read, or quiverd
//! starts: the cloud is listed, and what's newer there comes down with the pre-launch
//! rules, so the saves are ready offline. Download only (a session's failed upload is
//! finished once uploads are allowed). `epic_saves_background` in the config turns it off.
//!
//! **Who may do what** (STORES.md §7 item 10): reading is open to everyone. The first
//! upload of saves and the first play-time report are writes to the person's Epic account,
//! so they need the person (`confirmed` from the app or a terminal); after that they happen
//! on their own. Settling a conflict (which can overwrite the cloud copy) needs a person's
//! OK for agents. Restoring a local backup is a game change like any other: every source
//! may (2026-11-18).

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
/// How often the background save check looks at its triggers (the network, library
/// reads, each game's schedule). Each game's own schedule is `epic_core::fresh`'s.
const FRESH_TICK: std::time::Duration = std::time::Duration::from_secs(30);

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
    d.epic.set_background_saves(d.cfg.epic_saves_background);
    tokio::spawn(fresh_loop(d.clone()));
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
        EpicGameRequest::SavesRestore { game, backup } => {
            let app = epic_app(&game)?;
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
                bail!("the first play-time report to your Epic account needs your yes: run `quiver epic playtime --send --yes` yourself (after that, sessions are reported when they end)");
            }
            let sent = d.epic.send_queue(true, first_ok).await?;
            if !sent.is_empty() {
                d.log_activity_game(source, format!("sent {} Epic play session(s)", sent.len()), None);
            }
            let mut v: EpicPlaytime = d.epic.playtime_view(None, true, true).await?;
            v.queue.retain(|q| sent.iter().any(|s| s.id == q.id));
            json!(v)
        }
        EpicGameRequest::PlaytimeDiscard { game } => {
            if let Some(g) = &game {
                epic_app(g)?;
            }
            let dropped = d.epic.discard_sessions(game.as_ref()).await?;
            if !dropped.is_empty() {
                d.log_activity_game(source, format!("dropped {} Epic play session(s) without reporting them", dropped.len()), game);
            }
            let mut v: EpicPlaytime = d.epic.playtime_view(game.as_ref(), false, d.cfg.report_playtime).await?;
            v.queue = dropped;
            json!(v)
        }
        EpicGameRequest::Achievements { game, refresh } => json!(d.epic.achievements(epic_app(&game)?, refresh).await?),
        EpicGameRequest::SavesBackground { enabled } => {
            let mut cfg = quiver_core::launcher::Config::load()?;
            if let Some(on) = enabled
                && on != cfg.epic_saves_background
            {
                cfg.epic_saves_background = on;
                cfg.save()?;
                d.epic.set_background_saves(on);
                d.log_activity(
                    source,
                    if on { "Epic cloud saves are kept fresh between sessions again".to_owned() } else { "stopped keeping Epic cloud saves fresh between sessions (they sync around launches and on request)".to_owned() },
                    None,
                );
            }
            json!({ "enabled": cfg.epic_saves_background })
        }
    })
}

/// Keep installed Epic games' cloud saves fresh between sessions (the module's rules).
async fn fresh_loop(d: Arc<Daemon>) {
    // Let quiverd settle (and the app read the library) first; this first pass is the
    // daemon-start nudge.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    let mut nudge = true;
    let mut online = has_default_route();
    let mut reads = d.epic.library_reads();
    loop {
        let on = quiver_core::launcher::Config::load().map(|c| c.epic_saves_background).unwrap_or(d.cfg.epic_saves_background);
        d.epic.set_background_saves(on);
        let now_online = has_default_route();
        if now_online && !online {
            tracing::info!("network back: checking Epic cloud saves");
            nudge = true;
        }
        online = now_online;
        let r = d.epic.library_reads();
        if r != reads {
            reads = r;
            nudge = true;
        }
        // Offline, nothing is tried (and nothing counts as a failure).
        if on && online && d.epic.accounts().is_ok_and(|a| !a.is_empty()) {
            freshen_all(&d, nudge).await;
        }
        nudge = false;
        tokio::time::sleep(FRESH_TICK).await;
    }
}

/// One pass over the installed Epic games that aren't running.
async fn freshen_all(d: &Arc<Daemon>, nudge: bool) {
    use epic_core::fresh::Outcome;
    for i in d.epic.installed().unwrap_or_default() {
        let Some(game) = epic_core::provider::game_ref(&i.app_name) else { continue };
        if d.running_games.lock().unwrap().contains(&game) || d.running_epic(&i.app_name) {
            continue;
        }
        match d.epic.freshen(&i.app_name, nudge).await {
            Ok(Outcome::Downloaded(r)) => {
                let backup = r.backup.as_ref().map(|b| format!(" (this PC's copies backed up as {})", b.id)).unwrap_or_default();
                d.log_activity_game("quiver", format!("downloaded {} newer Epic cloud save file(s) of {} in the background{backup}", r.downloaded.len(), i.title), Some(game));
            }
            Ok(Outcome::Uploaded(r)) => {
                d.log_activity_game("quiver", format!("uploaded {}'s saves from its last session, which couldn't go at exit ({})", i.title, r.summary()), Some(game));
            }
            Ok(Outcome::Conflict { files, new: true }) => {
                d.log_activity_game(
                    "quiver",
                    format!("{}: {} Epic cloud save file(s) changed both here and in the cloud; nothing synced until you choose which to keep (its Saves tab)", i.title, files.len()),
                    Some(game),
                );
            }
            Ok(Outcome::Skipped(_) | Outcome::UpToDate | Outcome::Conflict { .. }) => {}
            Ok(Outcome::Held(why)) => tracing::info!(app = i.app_name, "Epic cloud saves in the background: {why}"),
            Err(e) => tracing::info!(app = i.app_name, "Epic cloud saves in the background: {e:#}"),
        }
    }
}

/// Whether the machine has a default route (IPv4 or IPv6): "the network is up", read from
/// the kernel without sending anything. Unknown (no `/proc`): assumed up.
fn has_default_route() -> bool {
    let v4 = std::fs::read_to_string("/proc/net/route");
    let v6 = std::fs::read_to_string("/proc/net/ipv6_route");
    if v4.is_err() && v6.is_err() {
        return true;
    }
    v4.is_ok_and(|t| default_route_v4(&t)) || v6.is_ok_and(|t| default_route_v6(&t))
}

/// `/proc/net/route`: a line whose destination and mask are 0 and whose flags say up.
fn default_route_v4(table: &str) -> bool {
    table.lines().skip(1).any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 8 && f[1] == "00000000" && u32::from_str_radix(f[3], 16).is_ok_and(|flags| flags & 1 == 1) && f[7] == "00000000"
    })
}

/// `/proc/net/ipv6_route`: `::/0` through an interface other than loopback.
fn default_route_v6(table: &str) -> bool {
    table.lines().any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 10 && f[0].bytes().all(|b| b == b'0') && f[1] == "00" && f[9] != "lo" && u32::from_str_radix(f[8], 16).is_ok_and(|flags| flags & 1 == 1)
    })
}

/// Whether a sync would upload anything (so the first-yes gate applies).
async fn needs_upload(d: &Daemon, app: &str) -> bool {
    match d.epic.saves_report(app, true).await {
        Ok(s) => s.files.iter().any(|f| matches!(f.state, quiver_api::epicgame::FileState::LocalNewer | quiver_api::epicgame::FileState::Conflict)),
        Err(_) => true,
    }
}

/// An Epic game's session as quiverd runs it: from [`begin`] to [`end`].
pub(crate) struct EpicSession {
    prepared: Prepared,
    poller: Option<Poller>,
    id: String,
    started: chrono::DateTime<Utc>,
}

impl EpicSession {
    /// What happened to the saves before the launch, for the job's status.
    pub(crate) fn note(&self) -> &str {
        &self.prepared.note
    }
}

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

    #[test]
    fn default_routes() {
        let v4 = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                  wlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                  wlan0\t0002A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";
        assert!(default_route_v4(v4));
        let lan_only = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                        wlan0\t0002A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\n";
        assert!(!default_route_v4(lan_only));
        let v6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00000003 wlan0\n\
                  00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200 lo\n";
        assert!(default_route_v6(v6));
        assert!(!default_route_v6(v6.lines().nth(1).unwrap()));
    }
}
