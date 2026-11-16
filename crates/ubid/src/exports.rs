//! Export requests: .desktop entries and Steam shortcuts (see `ubi_core::exports`).
//!
//! The daemon writes and stages exports but never restarts Steam on its own: that needs the
//! user's explicit confirmation each time. Front ends ask, then send `SteamRestart` with
//! `confirmed: true`; only then does quiverd run `restart_steam` (so the app and the CLI
//! can't disagree with the daemon about what's staged, whatever their versions).

use std::sync::Arc;
use std::time::Duration;

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

/// How long Steam gets to exit when asked to.
const STEAM_EXIT_TIMEOUT: Duration = Duration::from_secs(90);

/// A Steam restart only for a person who confirmed it just now, at the app or in a terminal.
fn restart_allowed(source: &str, confirmed: bool) -> Result<()> {
    if !confirmed {
        bail!("restarting Steam needs the person's confirmation every time: ask them, then send confirmed: true");
    }
    if !ubi_api::mods::is_human_source(source) {
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
                staged: steam::staged_descriptions(&paths),
            })
        }
        Request::SteamApplyStaged => {
            let applied = steam::apply_staged(&paths, steam::is_running())?;
            if applied > 0 {
                daemon.log_activity(source, format!("wrote {applied} staged Steam change(s)"), None);
                daemon.emit(ubi_api::Event::SteamGamesChanged { offer: vec![] });
            }
            json!({ "applied": applied })
        }
        Request::SteamRestart { confirmed } => {
            restart_allowed(source, confirmed)?;
            let steam = find_steam(&paths)?;
            let r = tokio::task::spawn_blocking(move || steam::restart_steam(&paths, &steam, STEAM_EXIT_TIMEOUT)).await?;
            // Something may have been written even when it failed.
            daemon.emit(ubi_api::Event::SteamGamesChanged { offer: vec![] });
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

    #[test]
    fn restart_needs_a_confirming_person() {
        assert!(restart_allowed("gui", true).is_ok());
        assert!(restart_allowed("cli", true).is_ok());
        assert!(restart_allowed("gui", false).unwrap_err().to_string().contains("confirmation"));
        assert!(restart_allowed("agent", true).unwrap_err().to_string().contains("'agent'"));
        assert!(restart_allowed("claude-code", true).is_err());
    }
}
