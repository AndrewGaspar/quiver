//! Save stores (`Request::SaveStores`, `SaveStoreEdit`, `SaveStoreHistory`, `SavesMerge`;
//! `ubi_core::savestores` does the work). None of these reach Ubisoft: the account whose
//! folders are counted is the game's launch account when one is signed in, else every
//! account folder in the prefix.
//!
//! Agent policy: listing and creating, renaming, retiring and unretiring stores, and
//! picking vanilla's store, are normal edits (retiring keeps the files). Moving the cloud
//! flag, restoring a revision (both change which saves sync) and merging (which replaces
//! saves, after a backup) are "Ask" for agents.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ubi_api::Request;
use ubi_api::mods::is_human_source;
use ubi_api::savestores::{MergeOutcome, SaveStoreOp};
use ubi_core::launcher::{self, Games};
use ubi_core::mods::ModStore;

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
        Request::SaveStoreEdit { product_id, op, message, confirmed } => {
            let name = daemon.product_name(product_id);
            if matches!(op, SaveStoreOp::Cloud { .. } | SaveStoreOp::Restore { .. }) && !is_human_source(source) && !confirmed {
                bail!(
                    "changing which saves sync with the cloud needs your confirmation when requested by '{source}' (agent policy: Ask); run it in a terminal, where quiver asks"
                );
            }
            Games::load()?.get(product_id).with_context(|| format!("{name} is not installed"))?;
            let rev = crate::mods::with_store_lock(move || ModStore::open(product_id).edit_stores(&op, &src, message)).await?;
            daemon.log_activity(
                source,
                format!("save stores of {name}: {} (r{})", rev.message.as_deref().unwrap_or("changed"), rev.revision),
                Some(product_id),
            );
            daemon.emit(ubi_api::Event::ModsChanged { product_id });
            json!(rev)
        }
        Request::SavesMerge { product_id, from, into, confirmed } => {
            let name = daemon.product_name(product_id);
            if !is_human_source(source) && !confirmed {
                bail!(
                    "merging saves needs your approval: run `quiver saves {product_id} merge {from}` yourself (the target's saves are backed up first)"
                );
            }
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
                    "merged {name} save store '{}' into '{}': {} copied, {} replaced, {} kept{}{}",
                    r.from.name,
                    r.into.name,
                    count(MergeOutcome::Copied),
                    count(MergeOutcome::Replaced),
                    count(MergeOutcome::Kept),
                    if backups.is_empty() { String::new() } else { format!("; previous saves backed up as {}", backups.join(", ")) },
                    if confirmed { " (approved)" } else { "" }
                ),
                Some(product_id),
            );
            daemon.emit(ubi_api::Event::ModsChanged { product_id });
            json!(r)
        }
        other => bail!("not a save store request: {other:?}"),
    })
}
