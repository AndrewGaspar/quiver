//! `quiver steam hook`: any Steam game started through `quiver steam-run`
//! (design/STORE-STEAM.md §9, steer 3b: opt-in per game).
//!
//! The hook is a launch option, `<quiver> --source steam steam-run --app <appid> %command%`,
//! written with the same staged, byte-preserving machinery as the Connect switch for Steam
//! copies of Ubisoft games (`quiver_core::exports::steamgames`), and undone exactly. A Ubisoft
//! game's Steam copy gets that switch itself (`SteamPatch`, with `--product`), so it behaves
//! exactly as `quiver steam patch` does.
//!
//! Status is read from Steam's files (read-only): the app manifest, `localconfig.vdf`'s
//! `LaunchOptions` for the Steam user who plays it, and Quiver's `steam-games.json`.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::exports::{SteamHook, SteamPatchResult};
use quiver_api::{Event, GameRef, Request};
use quiver_core::exports::Paths;
use quiver_core::exports::steam::{self, Steam};
use quiver_core::exports::steamgames;
use quiver_core::launcher::Config;

use crate::Daemon;

struct Found {
    paths: Paths,
    steam: Steam,
    app: steamgames::AppManifest,
    user: Option<String>,
    /// A Ubisoft game's Steam copy (`Some(None)`: one Quiver can't name the product of).
    ubisoft: Option<Option<u32>>,
}

fn find(app_id: u32, steam_user: Option<&str>) -> Result<Found> {
    let paths = Paths::from_env();
    let steam = Steam::find(&paths.home).context("Steam not found (looked in ~/.steam/steam, ~/.local/share/Steam and the Flatpak's data)")?;
    let app = steamgames::find_app(&steam, app_id).with_context(|| format!("Steam app {app_id} isn't installed in any Steam library"))?;
    let users = steam.users()?;
    let remembered = Config::load().ok().and_then(|c| c.steam_user);
    let user = match steam_user {
        Some(r) => Some(steam::choose_user(&users, Some(r), None)?.id),
        None => steamgames::owner_user(&app, &users).or_else(|| steam::choose_user(&users, None, remembered.as_deref()).ok().map(|u| u.id)),
    };
    let ubisoft = steamgames::detect(&app).map(|p| p.map(|(id, _)| id));
    Ok(Found { paths, steam, app, user, ubisoft })
}

fn status_of(f: &Found) -> SteamHook {
    let quiver = steamgames::quiver_for_steam(&f.paths.home);
    steamgames::hook_status(&f.paths, &f.steam, &f.app, f.user.as_deref(), quiver.as_deref(), f.ubisoft.flatten(), steam::is_running())
}

/// Where a Steam game's hook stands (blocking: reads Steam's files).
pub(crate) fn status(app_id: u32, steam_user: Option<&str>) -> Result<SteamHook> {
    Ok(status_of(&find(app_id, steam_user)?))
}

/// Whether a Steam game starts through `quiver steam-run` (`None`: not a Steam game, or
/// unknown). Blocking.
pub(crate) fn hooked(game: GameRef) -> Option<bool> {
    status(game.as_steam()?, None).ok().filter(|s| s.state != quiver_api::exports::PatchState::Unknown).map(|s| s.hooked())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    Ok(match req {
        Request::SteamHookStatus { app_id, steam_user } => json!(blocking(move || status(app_id, steam_user.as_deref())).await?),
        Request::SteamHook { app_id, on, steam_user } => json!(set(daemon, source, app_id, on, steam_user).await?),
        other => bail!("not a Steam hook request: {other:?}"),
    })
}

async fn set(daemon: &Arc<Daemon>, source: &str, app_id: u32, on: bool, steam_user: Option<String>) -> Result<SteamHook> {
    let u = steam_user.clone();
    let ubisoft = blocking(move || Ok(find(app_id, u.as_deref())?.ubisoft.is_some())).await?;
    if ubisoft {
        // A Ubisoft game's Steam copy: the Connect switch, exactly as `quiver steam patch`.
        let req = if on { Request::SteamPatch { app_id, steam_user } } else { Request::SteamUnpatch { app_id, steam_user } };
        let r: SteamPatchResult = serde_json::from_value(crate::steamgames::handle(daemon, source, req).await?)?;
        let user = r.steam_user.clone();
        let mut s = blocking(move || status(app_id, Some(&user))).await?;
        s.launch_options = r.launch_options;
        (s.staged, s.unchanged, s.dropped_staged) = (r.staged, r.unchanged, r.dropped_staged);
        return Ok(s);
    }
    let s = blocking(move || {
        let f = find(app_id, steam_user.as_deref())?;
        let before = status_of(&f);
        let users = || f.steam.users().unwrap_or_default().iter().map(|u| format!("{} ({})", u.id, u.persona_name.as_deref().unwrap_or("?"))).collect::<Vec<_>>().join(", ");
        let user = f.user.clone().with_context(|| format!("several Steam users; choose one with --user (users: {})", users()))?;
        let running = steam::is_running();
        let c = if on {
            if !before.hookable {
                bail!("{} can't be hooked: {}", f.app.name, before.note.as_deref().unwrap_or("unknown reason"));
            }
            steamgames::hook(&f.paths, &f.steam, &user, app_id, &f.app.name, &before.command, running)?
        } else {
            steamgames::unhook(&f.paths, &f.steam, &user, app_id, &f.app.name, running)?
        };
        let mut s = status_of(&f);
        s.launch_options = c.launch_options;
        (s.staged, s.unchanged, s.dropped_staged) = (c.staged, c.unchanged, c.dropped_staged);
        Ok(s)
    })
    .await?;
    let game = GameRef::steam(app_id);
    if s.dropped_staged && s.unchanged {
        daemon.log_activity_game(source, format!("Steam: dropped the hook change waiting for Steam for {}", s.name), Some(game));
    } else if !s.unchanged {
        let what = if on { "now starts through Quiver (quiver steam-run)" } else { "starts without Quiver again" };
        let wait = if s.staged { "; waits for Steam to close or restart" } else { "" };
        daemon.log_activity_game(source, format!("Steam: {} {what} (Steam user {}){wait}", s.name, s.steam_user.as_deref().unwrap_or("?")), Some(game));
    }
    daemon.emit(Event::SteamGamesChanged { offer: vec![] });
    Ok(s)
}
