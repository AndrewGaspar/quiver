//! Test hook: `QUIVER_TEST_GAMES=<file.json>` registers non-store test programs (a list of
//! `quiver_api::Game`, each with its `game` ref) that `Play` and `StartStream` launch the way
//! quiverd launches a store game it installed: the launch plan, settings, Options, Tweaks
//! and the HUD, through `launcher::play_store_game`, with no licence. For scratch daemons
//! (streaming tests); a person's quiverd never sets it.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use quiver_api::{Event, Game, GameRef, JobKind};
use quiver_core::launcher::{LaunchEvent, StoreLaunch};

use crate::Daemon;

/// The test games, if the hook is set.
pub(crate) fn all() -> Vec<Game> {
    let Some(path) = std::env::var_os("QUIVER_TEST_GAMES") else { return Vec::new() };
    match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| serde_json::from_slice::<Vec<Game>>(&b).map_err(Into::into)) {
        Ok(v) => v.into_iter().filter(|g| g.game.is_some()).collect(),
        Err(e) => {
            tracing::warn!("QUIVER_TEST_GAMES: {e:#}");
            Vec::new()
        }
    }
}

pub(crate) fn find(game: GameRef) -> Option<Game> {
    all().into_iter().find(|g| g.game == Some(game))
}

/// `Request::Play` for a test game: a Play job, as for an Epic game Quiver installed.
pub(crate) async fn play(d: &Arc<Daemon>, source: &str, g: Game, profile: Option<String>) -> Result<u64> {
    let game = g.game.context("a test game needs its ref")?;
    let (mut plan, _) = crate::mods::launch_plan(&g, profile.as_deref(), source)?;
    if !plan.api.problems.is_empty() {
        bail!("can't launch {}: {}", g.name, plan.api.problems.join("; "));
    }
    if !d.running_games.lock().unwrap().insert(game) {
        bail!("{} is already running", g.name);
    }
    plan.session = Some(crate::options::new_session_id());
    plan.game = Some(game);
    let id = d.new_game_job(JobKind::Play, game, g.name.clone());
    d.update_job(id, |j| j.detail = crate::mods::plan_lines(&plan.api));
    d.log_activity_game(source, format!("play {} (test game)", g.name), Some(game));
    d.emit(Event::LibraryChanged);
    let this = d.clone();
    tokio::spawn(async move {
        let detail: Arc<Mutex<Vec<String>>> = Arc::default();
        let det = detail.clone();
        let (cfg, d2) = (this.cfg.clone(), this.clone());
        let result = crate::run_local(move || async move {
            quiver_core::launcher::play_store_game(&cfg, &g, &plan, StoreLaunch::default(), |e| match e {
                LaunchEvent::Status(s) => d2.update_job(id, |j| j.status = s),
                LaunchEvent::Detail(v) => det.lock().unwrap().extend(v),
                _ => {}
            })
            .await
        })
        .await;
        this.running_games.lock().unwrap().remove(&game);
        let more = std::mem::take(&mut *detail.lock().unwrap());
        this.update_job(id, |j| {
            j.detail.extend(more);
            j.done = true;
            if let Err(e) = &result {
                j.status = "Failed".into();
                j.error = Some(format!("{e:#}"));
            }
        });
        this.emit(Event::LibraryChanged);
    });
    Ok(id)
}
