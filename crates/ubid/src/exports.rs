//! Export requests: .desktop entries and Steam shortcuts (see `ubi_core::exports`).
//!
//! The daemon writes and stages exports but never restarts Steam: that needs the user's
//! explicit confirmation each time, so only front ends call `restart_steam` after asking.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ubi_api::Request;
use ubi_api::exports::SteamUsers;
use ubi_core::exports::{self, ExportKey, Paths, Target, desktop, steam};
use ubi_core::launcher::{Config, Games};

use crate::Daemon;

/// Name, art and window class for a product: from the library (cached; it has the art),
/// else from the installed game.
async fn target(daemon: &Arc<Daemon>, product_id: u32) -> Result<Target> {
    let entry = match daemon.library(false).await {
        Ok(lib) => lib.into_iter().find(|e| e.product_id == product_id),
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

fn find_steam(paths: &Paths) -> Result<steam::Steam> {
    steam::Steam::find(&paths.home)
        .context("Steam not found (looked in ~/.steam/steam, ~/.local/share/Steam and the Flatpak's data)")
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let paths = Paths::from_env();
    Ok(match req {
        Request::ExportList => json!(exports::list(&paths)),
        Request::ExportDesktop { product_id, remove } => {
            let key = ExportKey::game(product_id);
            if remove {
                let existed = desktop::remove(&paths, &key)?;
                daemon.log_activity(source, format!("removed .desktop entry for {}", daemon.product_name(product_id)), Some(product_id));
                json!({ "removed": existed, "path": desktop::path(&paths, &key) })
            } else {
                let t = target(daemon, product_id).await?;
                let rec = desktop::export(&paths, &t).await?;
                daemon.log_activity(source, format!("wrote .desktop entry for {}", t.name), Some(product_id));
                json!(rec)
            }
        }
        Request::ExportSteam { product_id, remove, steam_user } => {
            let steam = find_steam(&paths)?;
            let users = steam.users()?;
            let mut cfg = Config::load()?;
            let running = steam::is_running();
            let key = ExportKey::game(product_id);
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
                let t = target(daemon, product_id).await?;
                steam::export(&paths, &steam, &user.id, &t, running).await?
            };
            daemon.log_activity(
                source,
                format!(
                    "{} Steam shortcut for {} (user {}){}",
                    if remove { "removed" } else { "wrote" },
                    daemon.product_name(product_id),
                    result.steam_user,
                    if result.staged { "; staged until Steam restarts" } else { "" }
                ),
                Some(product_id),
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
                staged: steam::staged(&paths).iter().map(|s| s.describe()).collect(),
            })
        }
        Request::SteamApplyStaged => {
            let applied = steam::apply_staged(&paths, steam::is_running())?;
            if applied > 0 {
                daemon.log_activity(source, format!("wrote {applied} staged Steam shortcut change(s)"), None);
            }
            json!({ "applied": applied })
        }
        other => bail!("not an export request: {other:?}"),
    })
}
