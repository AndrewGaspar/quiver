//! `Request::Mods`: mod layers and profiles (`ubi_core::mods` does the work). None of
//! these requests reach Ubisoft.
//!
//! Agent policy (design/PROPOSAL.md, "What agents may do"): changing profiles, adding
//! layers and changing a layer's deletions are allowed for everyone (a deletion is a
//! whiteout that hides a file inside a launch's private mount; no real file is deleted);
//! deleting a profile or a layer, and restoring a profile or layer revision, are "Ask"
//! for agents. A request from a source other than a person at a front end
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
            ModsRequest::LayerAdd { product_id, name, path, deletes } => {
                let game = installed(product_id)?;
                let change = blocking(move || {
                    let path = (!path.as_os_str().is_empty()).then_some(path);
                    ModStore::open(product_id).add_layer_with(&name, path.as_deref(), &deletes, Some(&game.install_dir), &src)
                })
                .await?;
                let l = &change.layer;
                let deletes = if l.deletes.is_empty() { String::new() } else { format!(", deletes {}", l.deletes.len()) };
                self.mods_changed(
                    source,
                    product_id,
                    format!("added mod layer '{}' ({} files{deletes}) to {}", l.name, l.files, self.product_name(product_id)),
                );
                json!(change)
            }
            ModsRequest::LayerRemove { product_id, layer, confirmed } => {
                ask(source, confirmed, "removing a mod layer")?;
                installed(product_id)?;
                self.not_running(product_id, "removing a layer")?;
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
            ModsRequest::LayerDeletes { product_id, layer, delete, keep, message } => {
                // Whiteouts only hide files inside a launch's mount, so this is a normal
                // edit for agents too. A mounted layer must not change underneath it.
                let game = installed(product_id)?;
                self.not_running(product_id, "changing a layer's deletions")?;
                let change = blocking(move || {
                    ModStore::open(product_id).change_deletions(&layer, &delete, &keep, Some(&game.install_dir), &src, message)
                })
                .await?;
                self.layer_changed(source, product_id, &change, None);
                json!(change)
            }
            ModsRequest::LayerHistory { product_id, layer } => {
                let (_, history) = blocking(move || ModStore::open(product_id).layer_history_of(&layer)).await?;
                json!(history)
            }
            ModsRequest::LayerRestore { product_id, layer, revision, confirmed } => {
                ask(source, confirmed, "restoring a mod layer revision")?;
                installed(product_id)?;
                self.not_running(product_id, "restoring a layer's deletions")?;
                let change = blocking(move || ModStore::open(product_id).restore_layer(&layer, revision, &src)).await?;
                self.layer_changed(source, product_id, &change, Some(revision));
                json!(change)
            }
        })
    }

    fn not_running(&self, product_id: u32, what: &str) -> Result<()> {
        if self.running.lock().unwrap().contains(&product_id) {
            bail!("{} is running; stop it before {what}", self.product_name(product_id));
        }
        Ok(())
    }

    fn layer_changed(&self, source: &str, product_id: u32, change: &ubi_api::mods::LayerChange, restored: Option<u32>) {
        let l = &change.layer;
        let what = match restored {
            Some(r) => format!("restored r{r} of"),
            None => "changed deletions of".into(),
        };
        self.mods_changed(
            source,
            product_id,
            format!(
                "{what} mod layer '{}' of {}: deletes {} (r{})",
                l.name,
                self.product_name(product_id),
                l.deletes.len(),
                change.revision
            ),
        );
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
