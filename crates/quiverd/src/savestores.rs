//! Save stores (`Request::SaveStores`, `SaveStoreEdit`, `SaveStoreHistory`, `SavesMerge`;
//! `quiver_core::savestores` does the work). None of these reach Ubisoft: the account whose
//! folders are counted is the game's launch account when one is signed in, else every
//! account folder in the prefix.
//!
//! Every source's requests apply the same way (agents act like the person, the user's
//! decision of 2026-11-18): creating, renaming, retiring stores (retiring keeps the files),
//! moving the cloud flag, restoring a revision and merging (after a backup of the target).
//! Changes record their source.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::Request;
use quiver_api::savestores::MergeOutcome;
use quiver_core::launcher::{self, Games};
use quiver_core::mods::ModStore;

use crate::Daemon;

fn store_for(game: &launcher::Game) -> ModStore {
    let fixes = launcher::Config::load().ok().and_then(|c| c.fixes_on_vanilla);
    ModStore::open(game.product_id).with_game(game, fixes)
}

/// The launch account's user id, if one is signed in (no network).
async fn account(daemon: &Arc<Daemon>, product_id: u32) -> Option<String> {
    let online = daemon.account_for(product_id, None).await.ok()?;
    online.user_id().map(str::to_owned)
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let src = source.to_owned();
    Ok(match req {
        Request::SaveStores { product_id } => {
            let game = Games::load()?.get(product_id).cloned().with_context(|| format!("{} is not installed", daemon.product_name(product_id)))?;
            let user = account(daemon, product_id).await;
            let store = store_for(&game);
            json!(tokio::task::spawn_blocking(move || store.stores_report(user.as_deref())).await??)
        }
        Request::SaveStoreHistory { product_id } => {
            json!(ModStore::open(product_id).save_stores().history()?)
        }
        Request::SaveStoreEdit { product_id, op, message } => {
            let name = daemon.product_name(product_id);
            Games::load()?.get(product_id).with_context(|| format!("{name} is not installed"))?;
            let rev = crate::mods::with_store_lock(move || ModStore::open(product_id).edit_stores(&op, &src, message)).await?;
            daemon.log_activity(
                source,
                format!("save stores of {name}: {} (r{})", rev.message.as_deref().unwrap_or("changed"), rev.revision),
                Some(product_id),
            );
            daemon.emit(quiver_api::Event::ModsChanged { product_id, game: None });
            json!(rev)
        }
        Request::SavesMerge { product_id, from, into } => {
            let name = daemon.product_name(product_id);
            if daemon.running.lock().unwrap().contains(&product_id) || daemon.sessions.list().iter().any(|s| s.product_id == product_id) {
                bail!("{name} is running; quit it before merging saves");
            }
            let game = Games::load()?.get(product_id).cloned().with_context(|| format!("{name} is not installed"))?;
            let backups = daemon.cfg.save_backups.clone();
            let store = store_for(&game);
            let r = crate::mods::with_store_lock(move || store.merge_stores(&from, into.as_deref(), &backups, &src)).await?;
            let count = |o: MergeOutcome| r.slots.iter().filter(|s| s.outcome == o).count();
            let backups: Vec<&str> = r.backups.iter().map(|b| b.id.as_str()).collect();
            daemon.log_activity(
                source,
                format!(
                    "merged {name} save store '{}' into '{}': {} copied, {} replaced, {} kept{}",
                    r.from.name,
                    r.into.name,
                    count(MergeOutcome::Copied),
                    count(MergeOutcome::Replaced),
                    count(MergeOutcome::Kept),
                    if backups.is_empty() { String::new() } else { format!("; previous saves backed up as {}", backups.join(", ")) },
                ),
                Some(product_id),
            );
            daemon.emit(quiver_api::Event::ModsChanged { product_id, game: None });
            json!(r)
        }
        other => bail!("not a save store request: {other:?}"),
    })
}
