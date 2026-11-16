//! `Request::Mods`: mod layers and profiles (`ubi_core::mods` does the work). None of
//! these requests reach Ubisoft.
//!
//! Agent policy (design/PROPOSAL.md, "What agents may do"): changing profiles is allowed
//! for everyone; deleting a profile or a layer, and restoring a profile revision, are
//! "Ask" for agents. A request from a source other than a person at a front end
//! (`cli`, `gui`) is refused unless it carries `confirmed`, which front ends set only
//! after asking the person.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ubi_api::mods::{GameProfiles, ModsRequest, is_human_source};
use ubi_api::{Event, SettingOp};
use ubi_core::launcher::{self, Games};
use ubi_core::mods::ModStore;

use crate::Daemon;

/// Serializes changes to the mod store (revision numbers, layer copies).
static WRITE: Mutex<()> = Mutex::new(());

/// Run store work on a blocking thread, one change at a time.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
        f()
    })
    .await?
}

/// The "Ask" rule for agents.
fn ask(source: &str, confirmed: bool, what: &str) -> Result<()> {
    if is_human_source(source) || confirmed {
        return Ok(());
    }
    bail!(
        "{what} needs your confirmation when requested by '{source}' (agent policy: Ask). \
         Run it in a terminal, where quiver asks, or without --source"
    )
}

fn installed(product_id: u32) -> Result<launcher::Game> {
    Games::load()?.get(product_id).cloned().context("not installed (mods need an installed game)")
}

impl Daemon {
    pub(crate) async fn handle_mods(self: &Arc<Self>, source: &str, req: ModsRequest) -> Result<Value> {
        let src = source.to_owned();
        Ok(match req {
            ModsRequest::Games => {
                let games = Games::load()?;
                let list = blocking(move || {
                    games
                        .games
                        .into_iter()
                        .map(|g| {
                            let profiles = ModStore::open(g.product_id).profiles()?.into_iter().map(|(p, _)| p).collect();
                            Ok(GameProfiles { product_id: g.product_id, name: g.name, profiles })
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .await?;
                json!(list)
            }
            ModsRequest::List { product_id } => {
                installed(product_id)?;
                json!(blocking(move || ModStore::open(product_id).state()).await?)
            }
            ModsRequest::LayerAdd { product_id, name, path } => {
                let game = installed(product_id)?;
                let layer = blocking(move || ModStore::open(product_id).add_layer(&name, &path, Some(&game.install_dir), &src)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("added mod layer '{}' ({} files) to {}", layer.name, layer.files, self.product_name(product_id)),
                );
                json!(layer)
            }
            ModsRequest::LayerRemove { product_id, layer, confirmed } => {
                ask(source, confirmed, "removing a mod layer")?;
                installed(product_id)?;
                if self.running.lock().unwrap().contains(&product_id) {
                    bail!("{} is running; stop it before removing a layer", self.product_name(product_id));
                }
                let layer = blocking(move || ModStore::open(product_id).remove_layer(&layer)).await?;
                self.mods_changed(source, product_id, format!("removed mod layer '{}' from {}", layer.name, self.product_name(product_id)));
                json!(layer)
            }
            ModsRequest::ProfileCreate { product_id, name, layers, message } => {
                installed(product_id)?;
                let rev = blocking(move || ModStore::open(product_id).create_profile(&name, &layers, &src, message)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("created mod profile '{}' ({}) for {}", rev.profile.name, rev.profile.id, self.product_name(product_id)),
                );
                json!(rev)
            }
            ModsRequest::ProfileSet { product_id, profile, ops, message } => {
                installed(product_id)?;
                for op in &ops {
                    if let SettingOp::Set { key, value } = op
                        && key == "proton"
                    {
                        launcher::resolve_proton(value)?;
                    }
                }
                let rev = blocking(move || ModStore::open(product_id).set_profile(&profile, &ops, &src, message)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("mod profile '{}' r{} for {}", rev.profile.name, rev.revision, self.product_name(product_id)),
                );
                json!(rev)
            }
            ModsRequest::ProfileDelete { product_id, profile, confirmed } => {
                ask(source, confirmed, "deleting a mod profile")?;
                installed(product_id)?;
                let rev = blocking(move || ModStore::open(product_id).delete_profile(&profile, &src)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!(
                        "deleted mod profile '{}' ({}) of {} (restore: r{})",
                        rev.profile.name,
                        rev.profile.id,
                        self.product_name(product_id),
                        rev.revision - 1
                    ),
                );
                json!(rev)
            }
            ModsRequest::ProfileHistory { product_id, profile } => {
                let (_, history) = blocking(move || ModStore::open(product_id).history_of(&profile)).await?;
                json!(history)
            }
            ModsRequest::ProfileRestore { product_id, profile, revision, confirmed } => {
                ask(source, confirmed, "restoring a mod profile revision")?;
                installed(product_id)?;
                let rev = blocking(move || ModStore::open(product_id).restore_profile(&profile, revision, &src)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!(
                        "restored mod profile '{}' r{revision} as r{} for {}",
                        rev.profile.name,
                        rev.revision,
                        self.product_name(product_id)
                    ),
                );
                json!(rev)
            }
        })
    }

    fn mods_changed(&self, source: &str, product_id: u32, action: String) {
        self.log_activity(source, action, Some(product_id));
        self.emit(Event::ModsChanged { product_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_must_confirm() {
        assert!(ask("cli", false, "x").is_ok());
        assert!(ask("gui", false, "x").is_ok());
        assert!(ask("agent", true, "x").is_ok());
        let e = ask("agent", false, "deleting a mod profile").unwrap_err().to_string();
        assert!(e.contains("deleting a mod profile") && e.contains("agent"), "{e}");
        assert!(ask("claude-code", false, "x").is_err());
    }
}
