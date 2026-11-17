//! Steam copies of Ubisoft games: listing them, switching them to start through Quiver
//! instead of Ubisoft Connect (their launch options; see `quiver_core::exports::steamgames`),
//! and noticing new installs to offer the switch.
//!
//! Like shortcuts, launch options can only be written while Steam is closed, so changes made
//! while it runs are staged. The daemon never restarts Steam on its own: front ends ask the
//! user, then send a confirmed `SteamRestart` (see `exports`). The watcher writes staged
//! changes once Steam has been seen closed twice in a row.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::exports::{PatchState, SteamGame, SteamGames, SteamPatchResult, SteamUser};
use quiver_api::{Event, LibraryEntry, Request};
use quiver_core::exports::steam::{self, Steam};
use quiver_core::exports::steamgames::{self, Found};
use quiver_core::exports::Paths;
use quiver_core::launcher::Config;

use crate::Daemon;

/// How often the watcher looks at Steam's app manifests (a few `stat`s).
const WATCH_EVERY: Duration = Duration::from_secs(30);

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// The product: what Steam's files say, else an owned game with the same name.
fn product(f: &Found, lib: Option<&[LibraryEntry]>) -> Option<(u32, &'static str)> {
    if let Some((id, src)) = f.product {
        return Some((id, src.as_str()));
    }
    let n = norm(&f.app.name);
    lib?.iter().find(|e| norm(&e.name) == n).map(|e| (e.product_id, "library_name"))
}

/// Whose launch options: the user asked for, else the one who installed it, else the
/// remembered shortcut user, else the only one.
fn user_for(f: &Found, users: &[SteamUser], requested: Option<&SteamUser>, remembered: Option<&str>) -> Option<String> {
    if let Some(u) = requested {
        return Some(u.id.clone());
    }
    steamgames::owner_user(&f.app, users).or_else(|| steam::choose_user(users, None, remembered).ok().map(|u| u.id))
}

struct Ctx {
    paths: Paths,
    steam: Steam,
    users: Vec<SteamUser>,
    requested: Option<SteamUser>,
    cfg: Config,
}

impl Ctx {
    fn new(requested: Option<&str>) -> Result<Self> {
        let paths = Paths::from_env();
        let steam = Steam::find(&paths.home)
            .context("Steam not found (looked in ~/.steam/steam, ~/.local/share/Steam and the Flatpak's data)")?;
        let users = steam.users()?;
        let requested = requested.map(|r| steam::choose_user(&users, Some(r), None)).transpose()?;
        Ok(Self { paths, steam, users, requested, cfg: Config::load()? })
    }

    fn user(&self, f: &Found) -> Option<String> {
        user_for(f, &self.users, self.requested.as_ref(), self.cfg.steam_user.as_deref())
    }

    fn game(&self, f: &Found, lib: Option<&[LibraryEntry]>, st: &steamgames::State, quiver: Option<&PathBuf>) -> SteamGame {
        let product = product(f, lib);
        let user = self.user(f);
        let launch_options = user.as_deref().map(|u| steamgames::launch_options(&self.steam, u, f.app.app_id)).unwrap_or_default();
        let state = match &user {
            Some(u) => st.patch_state(u, f.app.app_id, &launch_options),
            None => PatchState::Unknown,
        };
        let owned = lib.map(|l| product.is_some_and(|(id, _)| l.iter().any(|e| e.product_id == id)));
        let note = if self.steam.flatpak {
            Some("Steam from Flatpak can't start Quiver from inside its sandbox".to_owned())
        } else if product.is_none() {
            Some("Quiver can't tell which Ubisoft game this is".to_owned())
        } else if quiver.is_none() {
            Some("The quiver command wasn't found (install Quiver to ~/.local/bin)".to_owned())
        } else if user.is_none() {
            Some("Several Steam users: choose which one plays it".to_owned())
        } else if owned == Some(false) {
            Some("Not in your Ubisoft library: link your Steam account to the Ubisoft account that owns it".to_owned())
        } else {
            None
        };
        SteamGame {
            app_id: f.app.app_id,
            name: f.app.name.clone(),
            install_dir: f.app.install_dir.clone(),
            product_id: product.map(|p| p.0),
            product_from: product.map(|p| p.1.to_owned()),
            owned,
            steam_user: user.clone(),
            launch_options,
            state,
            patchable: product.is_some() && !self.steam.flatpak && quiver.is_some() && user.is_some(),
            offer: st.offers.contains(&f.app.app_id),
            note,
            art: quiver_core::exports::steamart::library_art(&self.steam.root, f.app.app_id),
        }
    }
}

/// The library as quiverd has it (fetched once if there's none yet); `None` without one.
async fn library(daemon: &Arc<Daemon>) -> Option<Vec<LibraryEntry>> {
    daemon.owned_library(false).await.map_err(|e| tracing::debug!("library for Steam games: {e:#}")).ok()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let undo = matches!(req, Request::SteamUnpatch { .. });
    Ok(match req {
        Request::SteamGames { steam_user } => {
            if Steam::find(&Paths::from_env().home).is_none() {
                return Ok(json!(SteamGames { root: None, offers: Config::load()?.steam_offers, ..Default::default() }));
            }
            let lib = library(daemon).await;
            json!(
                blocking(move || {
                    let ctx = Ctx::new(steam_user.as_deref())?;
                    let found = steamgames::find_games(&ctx.steam);
                    let st = steamgames::load(&ctx.paths);
                    let quiver = steamgames::quiver_for_steam(&ctx.paths.home);
                    Ok(SteamGames {
                        root: Some(ctx.steam.root.clone()),
                        running: steam::is_running(),
                        games: found.iter().map(|f| ctx.game(f, lib.as_deref(), &st, quiver.as_ref())).collect(),
                        offers: ctx.cfg.steam_offers,
                        command: quiver.map(|q| steamgames::command(&q, None)).unwrap_or_default(),
                    })
                })
                .await?
            )
        }
        Request::SteamPatch { app_id, steam_user } | Request::SteamUnpatch { app_id, steam_user } => {
            let lib = library(daemon).await;
            let (r, product_id) = blocking(move || {
                let ctx = Ctx::new(steam_user.as_deref())?;
                let found = steamgames::find_games(&ctx.steam);
                let f = found
                    .iter()
                    .find(|f| f.app.app_id == app_id)
                    .with_context(|| format!("Steam app {app_id} isn't an installed Ubisoft game"))?;
                let users = || ctx.users.iter().map(|u| format!("{} ({})", u.id, u.persona_name.as_deref().unwrap_or("?"))).collect::<Vec<_>>().join(", ");
                let user = ctx.user(f).with_context(|| format!("several Steam users; choose one with --user (users: {})", users()))?;
                let running = steam::is_running();
                let product = product(f, lib.as_deref());
                let c = if undo {
                    steamgames::unpatch(&ctx.paths, &ctx.steam, &user, app_id, &f.app.name, running)?
                } else {
                    let Some((id, _)) = product else {
                        bail!("Quiver can't tell which Ubisoft game {} is; set its launch options by hand: quiver --source steam steam-run --product <id> %command%", f.app.name)
                    };
                    let quiver = steamgames::quiver_for_steam(&ctx.paths.home)
                        .context("the quiver command wasn't found next to quiverd, in ~/.local/bin or on PATH")?;
                    // The id is always written: steam-run could often find it itself, but
                    // not when only the library name matched, and it's plainer to read.
                    let cmd = steamgames::command(&quiver, Some(id));
                    steamgames::patch(&ctx.paths, &ctx.steam, &user, app_id, &f.app.name, &cmd, running)?
                };
                Ok((
                    SteamPatchResult {
                        app_id,
                        name: f.app.name.clone(),
                        steam_user: user,
                        launch_options: c.launch_options,
                        staged: c.staged,
                        unchanged: c.unchanged,
                        dropped_staged: c.dropped_staged,
                    },
                    product.map(|p| p.0),
                ))
            })
            .await?;
            if r.dropped_staged && r.unchanged {
                daemon.log_activity(source, format!("Steam: dropped the change waiting for Steam for {}", r.name), product_id);
            } else if !r.unchanged {
                daemon.log_activity(
                    source,
                    format!(
                        "Steam: {} {} (Steam user {}){}",
                        r.name,
                        if undo { "starts through Ubisoft Connect again" } else { "now starts through Quiver instead of Ubisoft Connect" },
                        r.steam_user,
                        if r.staged { "; waits for Steam to restart" } else { "" }
                    ),
                    product_id,
                );
            }
            daemon.emit(Event::SteamGamesChanged { offer: vec![] });
            json!(r)
        }
        Request::SteamDismissOffer { app_id } => {
            steamgames::dismiss_offer(&Paths::from_env(), app_id)?;
            daemon.emit(Event::SteamGamesChanged { offer: vec![] });
            Value::Null
        }
        Request::SteamOffers { enabled } => {
            let mut cfg = Config::load()?;
            if let Some(on) = enabled
                && on != cfg.steam_offers
            {
                cfg.steam_offers = on;
                cfg.save()?;
                if !on {
                    steamgames::clear_offers(&Paths::from_env())?;
                }
                daemon.log_activity(
                    source,
                    format!("{} offering Quiver for Ubisoft games newly installed in Steam", if on { "started" } else { "stopped" }),
                    None,
                );
                daemon.emit(Event::SteamGamesChanged { offer: vec![] });
            }
            json!({ "enabled": cfg.steam_offers })
        }
        other => bail!("not a Steam games request: {other:?}"),
    })
}

// ---------------------------------------------------------------------------------------
// Watching for new installs

/// What changes when a game is installed, updated or removed: the library list and each
/// app manifest's modification time.
fn fingerprint() -> Option<Vec<(PathBuf, Option<SystemTime>)>> {
    let paths = Paths::from_env();
    let steam = Steam::find(&paths.home)?;
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let lf = steam.root.join("steamapps/libraryfolders.vdf");
    let mut out = vec![(lf.clone(), mtime(&lf))];
    for lib in steamgames::library_folders(&steam) {
        let Ok(rd) = std::fs::read_dir(lib.join("steamapps")) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let n = e.file_name();
            if n.to_string_lossy().starts_with("appmanifest_") {
                out.push((e.path(), e.metadata().and_then(|m| m.modified()).ok()));
            }
        }
    }
    out.sort();
    Some(out)
}

/// A new offer: app id, name, product.
type Offer = (u32, String, Option<u32>);

/// Note the installed Ubisoft games; returns them and the new offers (app id, name,
/// product). Only games Quiver can switch are offered.
fn rescan(lib: Option<Vec<LibraryEntry>>) -> Result<(Vec<u32>, Vec<Offer>)> {
    let paths = Paths::from_env();
    let Some(steam) = Steam::find(&paths.home) else { return Ok((vec![], vec![])) };
    let users = steam.users().unwrap_or_default();
    let cfg = Config::load()?;
    let found = steamgames::find_games(&steam);
    let ids: Vec<u32> = found.iter().map(|f| f.app.app_id).collect();
    let skip = |id: u32| {
        let Some(f) = found.iter().find(|f| f.app.app_id == id) else { return true };
        let uses_quiver = || {
            user_for(f, &users, None, cfg.steam_user.as_deref())
                .is_some_and(|u| steamgames::find_patch(&steamgames::launch_options(&steam, &u, id)).is_some())
        };
        steam.flatpak || product(f, lib.as_deref()).is_none() || uses_quiver()
    };
    let new = steamgames::note_installed(&paths, &ids, cfg.steam_offers, skip)?;
    let offers = found
        .iter()
        .filter(|f| new.contains(&f.app.app_id))
        .map(|f| (f.app.app_id, f.app.name.clone(), product(f, lib.as_deref()).map(|p| p.0)))
        .collect();
    Ok((ids, offers))
}

pub(crate) fn start(daemon: &Arc<Daemon>) {
    let d = daemon.clone();
    tokio::spawn(async move {
        let mut last = None;
        let mut installed: Option<Vec<u32>> = None;
        let mut closed = 0u32;
        loop {
            let fp = tokio::task::spawn_blocking(fingerprint).await.ok().flatten();
            if fp.is_some() && fp != last {
                // The library as it is (no fetch): it names games Steam's files don't.
                let lib = d.library.lock().await.clone();
                match tokio::task::spawn_blocking(move || rescan(lib)).await {
                    Ok(Ok((ids, offers))) => {
                        for (app_id, name, product_id) in &offers {
                            d.log_activity(
                                "system",
                                format!("Steam: {name} was installed; Quiver can start it instead of Ubisoft Connect (quiver steam patch {app_id})"),
                                *product_id,
                            );
                        }
                        if !offers.is_empty() || installed.as_ref().is_some_and(|i| *i != ids) {
                            d.emit(Event::SteamGamesChanged { offer: offers.iter().map(|o| o.0).collect() });
                        }
                        installed = Some(ids);
                    }
                    Ok(Err(e)) => tracing::warn!("looking for Ubisoft games in Steam: {e:#}"),
                    Err(e) => tracing::warn!("looking for Ubisoft games in Steam: {e}"),
                }
            }
            last = fp;
            // Staged Steam changes: written once Steam has been closed for a whole interval.
            if last.is_some() {
                closed = if steam::is_running() { 0 } else { closed + 1 };
                if closed >= 2 {
                    let r = tokio::task::spawn_blocking(|| steam::apply_staged(&Paths::from_env(), steam::is_running())).await;
                    match r {
                        Ok(Ok(0)) => {}
                        Ok(Ok(n)) => {
                            d.log_activity("system", format!("Steam is closed: wrote {n} staged Steam change(s)"), None);
                            d.emit(Event::SteamGamesChanged { offer: vec![] });
                        }
                        Ok(Err(e)) => tracing::warn!("writing staged Steam changes: {e:#}"),
                        Err(e) => tracing::warn!("writing staged Steam changes: {e}"),
                    }
                }
            }
            tokio::time::sleep(WATCH_EVERY).await;
        }
    });
}
