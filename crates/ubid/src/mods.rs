//! `Request::Mods`: mods, layers and profiles (`ubi_core::mods` does the work). None of
//! these requests reach Ubisoft.
//!
//! Agent policy (design/PROPOSAL.md, "What agents may do"): changing profiles, installing
//! mods, changing a mod's settings and a layer's deletions are allowed for everyone (a
//! deletion is a whiteout that hides a file inside a launch's private mount; no real file
//! is deleted); deleting a profile or a layer, restoring a profile or layer revision,
//! uninstalling a mod that has install files or changed config values, and undoing config
//! patches are "Ask" for agents. A request from a source other than a person at a front
//! end (`cli`, `gui`) is refused unless it carries `confirmed`, which front ends set only
//! after asking the person.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ubi_api::mods::{GameProfiles, LaunchPlan, ModPrefs, ModsRequest, PatchState, ProfileRevision, is_human_source};
use ubi_api::{Event, SettingOp};
use ubi_core::launcher::{self, Config, Games};
use ubi_core::mods::{ModStore, Package, Plan};

use crate::Daemon;

/// Serializes changes to the mod store (revision numbers, layer copies, the patch record).
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

/// The app-level `fixes_on_vanilla` setting, as saved now.
fn fixes_on_vanilla() -> Option<bool> {
    Config::load().ok().and_then(|c| c.fixes_on_vanilla)
}

/// The store for an installed game, with its prefix and the app-level settings.
fn store_for(game: &launcher::Game) -> ModStore {
    ModStore::open(game.product_id).with_game(game, fixes_on_vanilla())
}

fn store(product_id: u32) -> Result<ModStore> {
    Ok(store_for(&installed(product_id)?))
}

/// The launch plan for `profile` (`None`: vanilla). A profile from before Mods v2 that has
/// its own saves folder first gets `separate_saves=on` recorded (see `ModStore::migrate`).
pub(crate) fn launch_plan(game: &launcher::Game, profile: Option<&str>, source: &str) -> Result<Plan> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let store = store_for(game);
    if profile.is_some() {
        match store.migrate(false, source) {
            Ok(r) => r.notes.iter().for_each(|n| tracing::info!("mods v2 migration: {n}")),
            Err(e) => tracing::warn!("mods v2 migration: {e:#}"),
        }
    }
    store.plan(profile)
}

/// What a launch plan says worth keeping with the launch job.
pub(crate) fn plan_lines(p: &LaunchPlan) -> Vec<String> {
    let mut out = Vec::new();
    if !p.mods.is_empty() {
        out.push(format!("mods: {}", p.mods.join(" < ")));
    }
    out.push(format!("saves: {} ({})", p.saves.folder, p.saves.reason));
    for c in &p.setting_conflicts {
        let by: Vec<String> = c.by.iter().map(|b| format!("{}={}", b.from, b.value.as_deref().unwrap_or("(deleted)"))).collect();
        out.push(format!("conflict: {} set by {} (the last wins)", c.key, by.join(", ")));
    }
    for c in &p.file_conflicts {
        out.push(format!("file conflict: {} from {} (the top one wins)", c.path, c.layers.join(" < ")));
    }
    for w in &p.warnings {
        out.push(format!("note: {w}"));
    }
    for d in &p.config_dirs {
        out.push(format!("own folder: {} -> {}", d.dir, d.upper.display()));
    }
    out
}

/// Notes for a person after a stack edit: what will conflict or fail at launch.
fn stack_notes(store: &ModStore, rev: &ProfileRevision) -> (Vec<String>, Option<ubi_api::mods::SavesPlan>) {
    let Ok(state) = store.state() else { return (Vec::new(), None) };
    let Some(info) = state.profiles.into_iter().find(|i| i.profile.id == rev.profile.id) else { return (Vec::new(), None) };
    let mut notes: Vec<String> = info.problems.iter().map(|p| format!("won't launch: {p}")).collect();
    for c in &info.setting_conflicts {
        let by: Vec<String> = c.by.iter().map(|b| format!("{}={}", b.from, b.value.as_deref().unwrap_or("(deleted)"))).collect();
        notes.push(format!("conflict: {} set by {} (the last wins)", c.key, by.join(", ")));
    }
    if !info.conflicts.is_empty() {
        notes.push(format!("{} file(s) provided or deleted by more than one mod (the top one wins)", info.conflicts.len()));
    }
    notes.extend(info.warnings.iter().cloned());
    (notes, info.saves)
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
                let store = store(product_id)?;
                json!(blocking(move || store.state()).await?)
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
                let store = store(product_id)?;
                self.not_running(product_id, "removing a layer")?;
                let (layer, undone) = blocking(move || store.uninstall(&layer)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("removed mod layer '{}' from {}{}", layer.name, self.product_name(product_id), undone_note(&undone)),
                );
                json!(layer)
            }
            ModsRequest::ProfileCreate { product_id, name, layers, message } => {
                let store = store(product_id)?;
                let (rev, notes) = blocking(move || {
                    let rev = store.create_profile(&name, &layers, &src, message)?;
                    let notes = stack_notes(&store, &rev);
                    Ok((rev, notes))
                })
                .await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("created mod profile '{}' ({}) for {}", rev.profile.name, rev.profile.id, self.product_name(product_id)),
                );
                with_notes(json!(rev), notes)
            }
            ModsRequest::ProfileSet { product_id, profile, ops, message } => {
                let store = store(product_id)?;
                for op in &ops {
                    if let SettingOp::Set { key, value } = op
                        && key == "proton"
                    {
                        launcher::resolve_proton(value)?;
                    }
                }
                let (rev, notes) = blocking(move || {
                    let rev = store.set_profile(&profile, &ops, &src, message)?;
                    let notes = stack_notes(&store, &rev);
                    Ok((rev, notes))
                })
                .await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("mod profile '{}' r{} for {}", rev.profile.name, rev.revision, self.product_name(product_id)),
                );
                with_notes(json!(rev), notes)
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
            // -- Mods v2 ------------------------------------------------------------------
            ModsRequest::Catalog { product_id } => {
                let store = store(product_id)?;
                json!(blocking(move || store.catalog()).await?)
            }
            ModsRequest::Install { product_id, builtin, path, name } => {
                let store = store(product_id)?;
                let pkg = match (builtin, path) {
                    (Some(id), None) => Package::Builtin(id),
                    (None, Some(path)) => Package::Dir { path, name },
                    _ => bail!("give a built-in mod id or a folder, not both"),
                };
                self.not_running(product_id, "installing a mod")?;
                let change = blocking(move || store.install(&pkg, &src)).await?;
                let l = &change.layer;
                self.mods_changed(
                    source,
                    product_id,
                    format!(
                        "installed mod '{}' ({}{}) for {} (r{})",
                        l.name,
                        l.id,
                        l.mod_id().map(|m| format!(", {m}")).unwrap_or_default(),
                        self.product_name(product_id),
                        change.revision
                    ),
                );
                json!(change)
            }
            ModsRequest::Uninstall { product_id, mod_id, confirmed } => {
                let store = store(product_id)?;
                let (layer, applied) = {
                    let (store, q) = (store.clone(), mod_id.clone());
                    blocking(move || {
                        let l = store.layer(&q)?;
                        let applied = store.applied_by(&l.id)?;
                        Ok((l, applied))
                    })
                    .await?
                };
                if layer.has_files() || !applied.is_empty() {
                    ask(source, confirmed, "uninstalling a mod with install files or changed config values")?;
                }
                self.not_running(product_id, "uninstalling a mod")?;
                let (layer, undone) = blocking(move || store.uninstall(&layer.id)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("uninstalled mod '{}' from {}{}", layer.name, self.product_name(product_id), undone_note(&undone)),
                );
                json!({ "mod": layer, "undone": undone })
            }
            ModsRequest::Show { product_id, mod_id } => {
                let store = store(product_id)?;
                json!(blocking(move || store.detail(&mod_id)).await?)
            }
            ModsRequest::ModSet { product_id, mod_id, ops, message } => {
                let store = store(product_id)?;
                self.not_running(product_id, "changing a mod")?;
                let changes = ops
                    .iter()
                    .map(|o| match o {
                        SettingOp::Set { key, value } => format!("{key}={value}"),
                        SettingOp::Unset { key } => format!("{key}="),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let change = blocking(move || store.set_mod(&mod_id, &ops, &src, message)).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("mod '{}' of {}: {changes} (r{})", change.layer.name, self.product_name(product_id), change.revision),
                );
                json!(change)
            }
            ModsRequest::Plan { product_id, profile } => {
                let game = installed(product_id)?;
                json!(blocking(move || store_for(&game).plan(profile.as_deref()).map(|p| p.api)).await?)
            }
            ModsRequest::PatchesUndo { product_id, mod_id, confirmed } => {
                ask(source, confirmed, "undoing config patches")?;
                let store = store(product_id)?;
                self.not_running(product_id, "undoing config patches")?;
                let undone = blocking(move || store.undo_patches(mod_id.as_deref())).await?;
                self.mods_changed(
                    source,
                    product_id,
                    format!("undid config patches of {}{}", self.product_name(product_id), undone_note(&undone)),
                );
                json!(undone)
            }
            ModsRequest::Prefs { fixes_on_vanilla } => {
                let prefs = blocking(move || {
                    let mut cfg = Config::load()?;
                    if let Some(v) = fixes_on_vanilla
                        && cfg.fixes_on_vanilla != Some(v)
                    {
                        cfg.fixes_on_vanilla = Some(v);
                        cfg.save()?;
                    }
                    Ok(ModPrefs { fixes_on_vanilla: cfg.fixes_on_vanilla })
                })
                .await?;
                if let Some(v) = fixes_on_vanilla {
                    self.log_activity(source, format!("fixes on unmodded games: {}", if v { "on" } else { "off" }), None);
                    for g in Games::load()?.games {
                        self.emit(Event::ModsChanged { product_id: g.product_id });
                    }
                }
                json!(prefs)
            }
            ModsRequest::Migrate { product_id, dry_run } => {
                let store = store(product_id)?;
                let report = blocking(move || store.migrate(dry_run, &src)).await?;
                if !dry_run && !report.kept_separate_saves.is_empty() {
                    self.mods_changed(
                        source,
                        product_id,
                        format!("mods v2 migration of {}: {}", self.product_name(product_id), report.notes.join("; ")),
                    );
                }
                json!(report)
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

/// A revision's or reply's notes, added to a JSON reply object (older clients ignore them).
fn with_notes(mut v: Value, (notes, saves): (Vec<String>, Option<ubi_api::mods::SavesPlan>)) -> Value {
    if let Some(o) = v.as_object_mut() {
        if !notes.is_empty() {
            o.insert("warnings".into(), json!(notes));
        }
        if let Some(s) = saves {
            o.insert("saves".into(), json!(s));
        }
    }
    v
}

fn undone_note(undone: &[ubi_api::mods::PatchStatus]) -> String {
    let n = undone.iter().filter(|s| s.state == PatchState::Undone).count();
    if n == 0 { String::new() } else { format!(" ({n} config value(s) put back)") }
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

    #[test]
    fn plan_lines_say_why() {
        let mut p = LaunchPlan::default();
        p.mods = vec!["m1".into(), "m2".into()];
        p.saves.folder = "savegames/<user>/1".into();
        p.saves.reason = "every mod is save-compatible".into();
        p.setting_conflicts.push(ubi_api::mods::SettingConflict {
            key: "dll.d3d11".into(),
            by: vec![
                ubi_api::mods::ValueFrom { from: "m1".into(), value: Some("n,b".into()) },
                ubi_api::mods::ValueFrom { from: "m2".into(), value: Some("b".into()) },
            ],
        });
        let lines = plan_lines(&p);
        assert_eq!(lines[0], "mods: m1 < m2");
        assert!(lines[1].contains("save-compatible"));
        assert!(lines[2].contains("dll.d3d11") && lines[2].contains("m2=b"), "{lines:?}");
    }
}
