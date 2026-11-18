//! Export requests: .desktop entries and Steam shortcuts (see `quiver_core::exports`).
//!
//! The daemon writes and stages exports but never restarts Steam on its own: that needs the
//! user's explicit confirmation each time. Front ends ask, then send `SteamRestart` with
//! `confirmed: true`; only then does quiverd run `restart_steam` (so the app and the CLI
//! can't disagree with the daemon about what's staged, whatever their versions).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::exports::{ExportKind, SteamUsers};
use quiver_api::{GameRef, Request, StoreId};
use quiver_core::exports::{self, ExportKey, Paths, Target, desktop, steam};
use quiver_core::launcher::{Config, Games};

use crate::Daemon;

/// Name, art and window class for a game Quiver installs: from the library (cached; it has
/// the art), else from the installed game. A Ubisoft game launches as `quiver play <id>`
/// (unchanged), an Epic game as `quiver play epic:<app>`.
async fn target(daemon: &Arc<Daemon>, game: GameRef) -> Result<Target> {
    match game.store {
        StoreId::Ubisoft => {}
        StoreId::Epic => return epic_target(daemon, game).await,
        StoreId::Steam => bail!("{game} is a Steam game: Steam lists and starts it, so it needs no shortcut from Quiver"),
        s => bail!("{game}: Quiver doesn't install {} games, so it has nothing to export", s.label()),
    }
    let product_id = game.as_ubisoft().context("not a Ubisoft product")?;
    let entry = match daemon.library(false).await {
        Ok(lib) => lib.into_iter().find(|e| e.ubisoft_id() == Some(product_id)),
        Err(e) => {
            tracing::info!("library unavailable for export of {product_id}: {e:#}");
            None
        }
    };
    let game = Games::load()?.get(product_id).cloned();
    let name = match (&entry, &game) {
        (Some(e), _) => e.name.clone(),
        (None, Some(g)) => g.name.clone(),
        (None, None) => bail!("product {product_id} is not in your library"),
    };
    Ok(Target {
        key: ExportKey::game(product_id),
        name,
        art: entry.map(|e| e.art).unwrap_or_default(),
        wm_class: game.and_then(|g| exports::wm_class_for_exe(&g.exe)),
        quiver_exe: exports::quiver_exe(),
    })
}

/// An Epic game Quiver installed: its title and Epic's catalog art.
async fn epic_target(daemon: &Arc<Daemon>, game: GameRef) -> Result<Target> {
    let installed = daemon.epic.installed_game(game.id.as_str());
    let entry = match daemon.library_all(false).await {
        Ok(lib) => lib.into_iter().find(|e| e.game == game),
        Err(e) => {
            tracing::info!("library unavailable for export of {game}: {e:#}");
            None
        }
    };
    let Some(i) = installed else {
        let name = entry.map(|e| e.name).unwrap_or_else(|| game.to_string());
        bail!("{name} isn't installed: install it first (quiver install {game})");
    };
    Ok(Target {
        key: ExportKey::for_game(game),
        name: entry.as_ref().map(|e| e.name.clone()).unwrap_or_else(|| i.title.clone()),
        art: entry.map(|e| e.art).unwrap_or_default(),
        wm_class: exports::wm_class_for_exe(&i.exe),
        quiver_exe: exports::quiver_exe(),
    })
}

/// After an uninstall: remove every export Quiver wrote for the game (its .desktop entries
/// and Steam shortcuts, profile ones too), since they would only start a game that's gone.
/// Returns what went, for the reply; failures are logged and skipped (the uninstall itself
/// already happened).
pub(crate) fn remove_game_exports(game: GameRef) -> Vec<String> {
    remove_exports_in(&Paths::from_env(), game, steam::is_running())
}

/// "; removed the .desktop entry and the Steam shortcut" for an activity line.
pub(crate) fn removed_note(removed: &[String]) -> String {
    match removed {
        [] => String::new(),
        [one] => format!("; removed {one}"),
        [rest @ .., last] => format!("; removed {} and {last}", rest.join(", ")),
    }
}

fn remove_exports_in(paths: &Paths, game: GameRef, steam_running: bool) -> Vec<String> {
    let mut out = Vec::new();
    let records: Vec<_> = exports::load_records(paths).into_iter().filter(|r| r.game == game).collect();
    for r in records {
        let key = match ExportKey::parse(&r.key) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("export record {}: {e:#}", r.key);
                continue;
            }
        };
        let what = match r.kind {
            ExportKind::Desktop => desktop::remove(paths, &key).map(|_| "the .desktop entry".to_owned()),
            ExportKind::Steam => steam::Steam::find(&paths.home)
                .context("Steam not found")
                .and_then(|s| steam::remove(paths, &s, None, &key, steam_running))
                .map(|res| if res.staged { "the Steam shortcut (when Steam restarts)".to_owned() } else { "the Steam shortcut".to_owned() }),
        };
        match what {
            Ok(w) => out.push(if r.profile_id.is_some() { format!("{w} of a profile") } else { w }),
            Err(e) => tracing::warn!("remove {} after uninstalling {game}: {e:#}", r.key),
        }
    }
    out
}

/// How long Steam gets to exit when asked to.
const STEAM_EXIT_TIMEOUT: Duration = Duration::from_secs(90);

/// A Steam restart only for a person who confirmed it just now, at the app or in a terminal.
fn restart_allowed(source: &str, confirmed: bool) -> Result<()> {
    if !confirmed {
        bail!("restarting Steam needs the person's confirmation every time: ask them, then send confirmed: true");
    }
    if !quiver_api::mods::is_human_source(source) {
        bail!("only the person at the app or in a terminal can restart Steam (this request is from '{source}'); ask them to");
    }
    Ok(())
}

fn find_steam(paths: &Paths) -> Result<steam::Steam> {
    steam::Steam::find(&paths.home)
        .context("Steam not found (looked in ~/.steam/steam, ~/.local/share/Steam and the Flatpak's data)")
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let paths = Paths::from_env();
    Ok(match req {
        Request::ExportList => json!(exports::list(&paths)),
        Request::ExportDesktop { game, remove } => {
            let game = game.0;
            let key = ExportKey::for_game(game);
            if remove {
                let existed = desktop::remove(&paths, &key)?;
                daemon.log_activity_game(source, format!("removed .desktop entry for {}", daemon.game_name(game).await), Some(game));
                json!({ "removed": existed, "path": desktop::path(&paths, &key) })
            } else {
                let t = target(daemon, game).await?;
                let rec = desktop::export(&paths, &t).await?;
                daemon.log_activity_game(source, format!("wrote .desktop entry for {}", t.name), Some(game));
                json!(rec)
            }
        }
        Request::ExportSteam { game, remove, steam_user } => {
            let game = game.0;
            if game.store == StoreId::Steam && !remove {
                bail!("{game} is a Steam game: Steam lists and starts it, so it needs no shortcut from Quiver");
            }
            let steam = find_steam(&paths)?;
            let users = steam.users()?;
            let mut cfg = Config::load()?;
            let running = steam::is_running();
            let key = ExportKey::for_game(game);
            let result = if remove {
                // The recorded user wins; this is only for shortcuts Quiver has no record of.
                let user = steam::choose_user(&users, steam_user.as_deref(), cfg.steam_user.as_deref()).ok();
                steam::remove(&paths, &steam, user.as_ref().map(|u| u.id.as_str()), &key, running)?
            } else {
                let user = steam::choose_user(&users, steam_user.as_deref(), cfg.steam_user.as_deref())?;
                if steam_user.is_some() && cfg.steam_user.as_deref() != Some(&user.id) {
                    cfg.steam_user = Some(user.id.clone());
                    cfg.save()?;
                }
                let t = target(daemon, game).await?;
                steam::export(&paths, &steam, &user.id, &t, running).await?
            };
            daemon.log_activity_game(
                source,
                format!(
                    "{} Steam shortcut for {} (user {}){}",
                    if remove { "removed" } else { "wrote" },
                    daemon.game_name(game).await,
                    result.steam_user,
                    if result.staged { "; staged until Steam restarts" } else { "" }
                ),
                Some(game),
            );
            json!(result)
        }
        Request::SteamUsers => {
            let cfg = Config::load()?;
            let steam = steam::Steam::find(&paths.home);
            let users = match &steam {
                Some(s) => s.users()?,
                None => Vec::new(),
            };
            json!(SteamUsers {
                root: steam.map(|s| s.root),
                selected: steam::choose_user(&users, None, cfg.steam_user.as_deref()).ok().map(|u| u.id),
                users,
                running: steam::is_running(),
                staged: steam::staged_descriptions(&paths),
            })
        }
        Request::SteamApplyStaged => {
            let applied = steam::apply_staged(&paths, steam::is_running())?;
            if applied > 0 {
                daemon.log_activity(source, format!("wrote {applied} staged Steam change(s)"), None);
                daemon.emit(quiver_api::Event::SteamGamesChanged { offer: vec![] });
            }
            json!({ "applied": applied })
        }
        Request::SteamRestart { confirmed } => {
            restart_allowed(source, confirmed)?;
            let steam = find_steam(&paths)?;
            let r = tokio::task::spawn_blocking(move || steam::restart_steam(&paths, &steam, STEAM_EXIT_TIMEOUT)).await?;
            // Something may have been written even when it failed.
            daemon.emit(quiver_api::Event::SteamGamesChanged { offer: vec![] });
            let r = r.inspect_err(|e| daemon.log_activity(source, format!("Steam restart: {e:#}"), None))?;
            daemon.log_activity(
                source,
                if r.was_running {
                    format!("restarted Steam (the person confirmed); wrote {} staged change(s)", r.applied)
                } else {
                    format!("wrote {} staged Steam change(s) (Steam wasn't running)", r.applied)
                },
                None,
            );
            json!(r)
        }
        other => bail!("not an export request: {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_target(game: GameRef) -> Target {
        // No art: nothing is fetched (the entry gets the generic games icon).
        Target { key: ExportKey::for_game(game), name: game.to_string(), art: Default::default(), wm_class: None, quiver_exe: "/q/quiver".into() }
    }

    #[tokio::test]
    async fn uninstall_removes_only_that_games_exports() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths { data_home: tmp.path().join("data"), home: tmp.path().join("home") };
        std::fs::create_dir_all(paths.home.join(".local/share/Steam/userdata/1001/config")).unwrap();
        let s = steam::Steam::find(&paths.home).unwrap();
        let epic = GameRef::new(StoreId::Epic, "Hornbill").unwrap();
        let other = GameRef::ubisoft(1651);
        for g in [epic, other] {
            desktop::export(&paths, &bare_target(g)).await.unwrap();
            steam::export(&paths, &s, "1001", &bare_target(g), false).await.unwrap();
        }
        let entry = paths.data_home.join("applications/quiver-epic-Hornbill.desktop");
        assert!(std::fs::read_to_string(&entry).unwrap().contains("Exec=/q/quiver play epic:Hornbill\n"));

        let removed = remove_exports_in(&paths, epic, false);
        assert_eq!(removed, ["the .desktop entry", "the Steam shortcut"]);
        assert!(!entry.exists());
        let left = exports::list(&paths);
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|x| x.record.game == other && x.state == quiver_api::exports::ExportState::Ok));
        let sc = quiver_core::exports::vdf::Shortcuts::parse(&std::fs::read(s.shortcuts_path("1001")).unwrap()).unwrap();
        assert!(sc.find(ExportKey::for_game(epic).steam_appid()).is_none());
        assert!(sc.find(ExportKey::game(1651).steam_appid()).is_some());
        // Nothing left to remove; while Steam runs, the shortcut's removal waits for it.
        assert!(remove_exports_in(&paths, epic, false).is_empty());
        assert_eq!(remove_exports_in(&paths, other, true), ["the .desktop entry", "the Steam shortcut (when Steam restarts)"]);
    }

    #[test]
    fn restart_needs_a_confirming_person() {
        assert!(restart_allowed("gui", true).is_ok());
        assert!(restart_allowed("cli", true).is_ok());
        assert!(restart_allowed("gui", false).unwrap_err().to_string().contains("confirmation"));
        assert!(restart_allowed("agent", true).unwrap_err().to_string().contains("'agent'"));
        assert!(restart_allowed("claude-code", true).is_err());
    }
}
