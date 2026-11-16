//! Save stores on disk (see `ubi_api::savestores` for the model).
//!
//! ```text
//! <data>/games/<product>/saves.jsonl                    the stores' history; the last line is current
//! <prefix>/…/Ubisoft Game Launcher/savegames/<user>/<folder>/   a store's slots, per account
//! ```
//!
//! A store's `folder` never changes, so its files never move: Default is `<product>` (the
//! folder Ubisoft Connect uses), a new store `<product>-<store id>`, and a store made from
//! a profile's own saves from before save stores keeps that folder, `<product>-<profile id>`.
//! A game with no history yet has Default only, synced with the cloud, used by vanilla
//! (revision 0, nothing on disk).
//!
//! Profiles name their store in their own history (`Profile::saves`), so "who uses a
//! store" is read from the profiles, never stored twice. The glue between profiles and
//! stores (migration, advice, merge, the report) is the `impl ModStore` block below.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use ubi_api::mods::{Layer, Profile, SavesPlan, VANILLA};
pub use ubi_api::savestores::{
    DEFAULT_STORE, MergeOutcome, MergeReport, MergeSlot, NEW_STORE, SaveStore, SaveStoreInfo, SaveStoreOp, SaveStoresReport,
    SaveStoresRevision, SavesAdvice, StoreUser,
};

use crate::mods::ModStore;
use crate::settings::{append_jsonl, read_jsonl};

/// One game's save stores under a data directory.
#[derive(Debug, Clone)]
pub struct SaveStores {
    data: PathBuf,
    product_id: u32,
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The account folders under a prefix's `savegames` (sorted).
pub fn account_dirs(prefix: &Path) -> Vec<String> {
    let root = prefix.join(crate::launcher::UPC_REL).join("savegames");
    let mut out: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    out.sort();
    out
}

/// A store's folder for one account.
pub fn store_dir(prefix: &Path, user: &str, folder: &str) -> PathBuf {
    prefix.join(crate::launcher::UPC_REL).join("savegames").join(user).join(folder)
}

impl SaveStores {
    pub fn new(data: &Path, product_id: u32) -> Self {
        Self { data: data.to_owned(), product_id }
    }

    fn path(&self) -> PathBuf {
        self.data.join("games").join(self.product_id.to_string()).join("saves.jsonl")
    }

    /// The built-in store as it is before anything is recorded.
    pub fn default_store(&self) -> SaveStore {
        SaveStore { id: DEFAULT_STORE.into(), name: "Default".into(), folder: self.product_id.to_string(), cloud: true, ..Default::default() }
    }

    fn initial(&self) -> SaveStoresRevision {
        SaveStoresRevision {
            revision: 0,
            at: Utc::now(),
            source: "quiver".into(),
            message: Some("Default only (nothing recorded yet)".into()),
            stores: vec![self.default_store()],
            vanilla: DEFAULT_STORE.into(),
        }
    }

    /// The store whose slots are compared with the cloud and restored into: the cloud
    /// store, else Default.
    pub fn primary(&self) -> SaveStore {
        let cur = self.current().unwrap_or_else(|_| self.initial());
        cur.cloud_store().or_else(|| cur.get(DEFAULT_STORE)).cloned().unwrap_or_else(|| self.default_store())
    }

    /// Every revision, oldest first (empty: nothing recorded yet).
    pub fn history(&self) -> Result<Vec<SaveStoresRevision>> {
        read_jsonl(&self.path())
    }

    /// The current state (revision 0 if nothing is recorded). Default is always there.
    pub fn current(&self) -> Result<SaveStoresRevision> {
        let mut cur = self.history()?.pop().unwrap_or_else(|| self.initial());
        if !cur.stores.iter().any(SaveStore::is_default) {
            cur.stores.insert(0, self.default_store());
        }
        Ok(cur)
    }

    /// Record a new state, unless it's the same as `cur`.
    fn commit(&self, cur: &SaveStoresRevision, stores: Vec<SaveStore>, vanilla: String, source: &str, message: Option<String>) -> Result<SaveStoresRevision> {
        if stores == cur.stores && vanilla == cur.vanilla {
            return Ok(cur.clone());
        }
        ensure!(stores.iter().filter(|s| s.cloud && !s.retired).count() <= 1, "only one save store per game can sync with the cloud");
        let rev = SaveStoresRevision { revision: cur.revision + 1, at: Utc::now(), source: source.to_owned(), message, stores, vanilla };
        append_jsonl(&self.path(), &rev)?;
        Ok(rev)
    }

    /// A store by id or name (`default` always names the built-in one).
    pub fn find<'a>(rev: &'a SaveStoresRevision, query: &str, retired_too: bool) -> Result<&'a SaveStore> {
        let q = query.trim();
        if let Some(s) = rev.stores.iter().find(|s| s.id == q || (s.is_default() && same_name(q, DEFAULT_STORE))) {
            ensure!(retired_too || !s.retired, "save store '{}' is retired (quiver saves <game> store unretire {})", s.name, s.id);
            return Ok(s);
        }
        let named: Vec<&SaveStore> = rev.stores.iter().filter(|s| same_name(&s.name, q) && (retired_too || !s.retired)).collect();
        match named.as_slice() {
            [s] => Ok(s),
            [] => {
                let live: Vec<String> = rev.stores.iter().filter(|s| !s.retired).map(|s| s.name.clone()).collect();
                bail!("no save store '{q}' (stores: {})", live.join(", "))
            }
            _ => bail!("'{q}' names more than one save store; use its id"),
        }
    }

    fn check_name(rev: &SaveStoresRevision, name: &str, id: &str) -> Result<String> {
        let name = name.trim();
        ensure!(!name.is_empty(), "a save store needs a name");
        ensure!(!same_name(name, NEW_STORE), "'new' is reserved (saves=new makes a store)");
        ensure!(!same_name(name, VANILLA), "'vanilla' is reserved for the unmodded game");
        if id != DEFAULT_STORE {
            ensure!(!same_name(name, DEFAULT_STORE), "'default' is the built-in store's name");
        }
        if let Some(o) = rev.stores.iter().find(|s| s.id != id && !s.retired && (same_name(&s.name, name) || s.id == name)) {
            bail!("there is already a save store named '{}'", o.name);
        }
        Ok(name.to_owned())
    }

    /// A name like `name` that no live store has ("DLSS", then "DLSS 2", …).
    fn free_name(rev: &SaveStoresRevision, name: &str) -> String {
        let base = if same_name(name, NEW_STORE) || same_name(name, DEFAULT_STORE) { format!("{name} saves") } else { name.trim().to_owned() };
        let mut candidate = base.clone();
        let mut n = 2;
        while Self::check_name(rev, &candidate, "").is_err() {
            candidate = format!("{base} {n}");
            n += 1;
        }
        candidate
    }

    /// A new, empty store (`<product>-<id>`; the folder is made on first use).
    pub fn create(&self, name: &str, note: Option<String>, source: &str, message: Option<String>) -> Result<(SaveStore, SaveStoresRevision)> {
        let cur = self.current()?;
        let name = Self::check_name(&cur, name, "")?;
        let id = loop {
            let id = crate::mods::new_id('s')?;
            if cur.get(&id).is_none() {
                break id;
            }
        };
        let store = SaveStore {
            folder: format!("{}-{id}", self.product_id),
            id,
            name,
            created: Some(Utc::now()),
            created_by: Some(source.to_owned()),
            cloud: false,
            retired: false,
            note,
        };
        let mut stores = cur.stores.clone();
        stores.push(store.clone());
        let message = message.or_else(|| Some(format!("created '{}'", store.name)));
        let rev = self.commit(&cur, stores, cur.vanilla.clone(), source, message)?;
        Ok((store, rev))
    }

    /// The store whose folder is `folder`, made (or unretired) if needed: for migrating a
    /// profile's own saves folder, which stays where it is. A new store is named after
    /// `name` (made unique). Returns the store and whether anything was recorded.
    pub fn adopt_folder(&self, name: &str, folder: &str, note: String, source: &str, message: String) -> Result<(SaveStore, bool)> {
        let cur = self.current()?;
        if let Some(s) = cur.stores.iter().find(|s| s.folder == folder) {
            if !s.retired {
                return Ok((s.clone(), false));
            }
            let mut stores = cur.stores.clone();
            let found = stores.iter_mut().find(|x| x.id == s.id).unwrap();
            found.retired = false;
            found.name = Self::free_name(&cur, &found.name);
            let store = found.clone();
            self.commit(&cur, stores, cur.vanilla.clone(), source, Some(message))?;
            return Ok((store, true));
        }
        let id = loop {
            let id = crate::mods::new_id('s')?;
            if cur.get(&id).is_none() {
                break id;
            }
        };
        let store = SaveStore {
            id,
            name: Self::free_name(&cur, name),
            folder: folder.to_owned(),
            created: Some(Utc::now()),
            created_by: Some(source.to_owned()),
            cloud: false,
            retired: false,
            note: Some(note),
        };
        let mut stores = cur.stores.clone();
        stores.push(store.clone());
        self.commit(&cur, stores, cur.vanilla.clone(), source, Some(message))?;
        Ok((store, true))
    }

    /// The store whose folder is `folder`, or a new retired one for it: a saves folder
    /// nothing uses any more, kept so it can be merged or brought back. Returns the store and
    /// whether it was made now.
    pub fn keep_retired(&self, name: &str, folder: &str, note: String, source: &str, message: String) -> Result<(SaveStore, bool)> {
        let cur = self.current()?;
        if let Some(s) = cur.stores.iter().find(|s| s.folder == folder) {
            return Ok((s.clone(), false));
        }
        let id = loop {
            let id = crate::mods::new_id('s')?;
            if cur.get(&id).is_none() {
                break id;
            }
        };
        let store = SaveStore {
            id,
            name: Self::free_name(&cur, name),
            folder: folder.to_owned(),
            created: Some(Utc::now()),
            created_by: Some(source.to_owned()),
            cloud: false,
            retired: true,
            note: Some(note),
        };
        let mut stores = cur.stores.clone();
        stores.push(store.clone());
        self.commit(&cur, stores, cur.vanilla.clone(), source, Some(message))?;
        Ok((store, true))
    }

    /// Apply an edit. `users` says which stores profiles use (store id, user), for refusing
    /// to retire a store in use.
    pub fn edit(&self, op: &SaveStoreOp, users: &[(String, StoreUser)], source: &str, message: Option<String>) -> Result<SaveStoresRevision> {
        let cur = self.current()?;
        let mut stores = cur.stores.clone();
        let mut vanilla = cur.vanilla.clone();
        let auto_message;
        match op {
            SaveStoreOp::Create { name } => return self.create(name, None, source, message).map(|(_, r)| r),
            SaveStoreOp::Rename { store, name } => {
                let s = Self::find(&cur, store, true)?.clone();
                let name = Self::check_name(&cur, name, &s.id)?;
                auto_message = format!("renamed '{}' to '{name}'", s.name);
                stores.iter_mut().find(|x| x.id == s.id).unwrap().name = name;
            }
            SaveStoreOp::Cloud { store } => {
                let target = match store.as_deref().map(str::trim) {
                    None | Some("" | "none" | "off") => None,
                    Some(q) => Some(Self::find(&cur, q, false)?.id.clone()),
                };
                for s in &mut stores {
                    s.cloud = Some(&s.id) == target.as_ref();
                }
                auto_message = match &target {
                    Some(id) => format!("'{}' syncs with the cloud", cur.get(id).map_or(id.as_str(), |s| s.name.as_str())),
                    None => "no store syncs with the cloud".into(),
                };
            }
            SaveStoreOp::Retire { store } => {
                let s = Self::find(&cur, store, false)?.clone();
                ensure!(!s.is_default(), "Default can't be retired");
                ensure!(!s.cloud, "'{}' syncs with the cloud; move that first (store cloud default)", s.name);
                let using: Vec<&str> = users.iter().filter(|(id, _)| *id == s.id).map(|(_, u)| u.name.as_str()).collect();
                ensure!(
                    using.is_empty() && cur.vanilla != s.id,
                    "'{}' is used by {}; point {} at another store first (profile set PROFILE saves=STORE)",
                    s.name,
                    if cur.vanilla == s.id { "vanilla".to_owned() } else { using.join(", ") },
                    if using.len() + usize::from(cur.vanilla == s.id) == 1 { "it" } else { "them" }
                );
                stores.iter_mut().find(|x| x.id == s.id).unwrap().retired = true;
                auto_message = format!("retired '{}' (its files stay in savegames/<user>/{})", s.name, s.folder);
            }
            SaveStoreOp::Unretire { store } => {
                let s = Self::find(&cur, store, true)?.clone();
                ensure!(s.retired, "'{}' isn't retired", s.name);
                let name = Self::free_name(&cur, &s.name);
                let x = stores.iter_mut().find(|x| x.id == s.id).unwrap();
                x.retired = false;
                x.name = name;
                auto_message = format!("brought back '{}'", s.name);
            }
            SaveStoreOp::Vanilla { store } => {
                let s = Self::find(&cur, store, false)?;
                vanilla = s.id.clone();
                auto_message = format!("vanilla uses '{}'", s.name);
            }
            SaveStoreOp::Restore { revision } => {
                let target = self
                    .history()?
                    .into_iter()
                    .find(|r| r.revision == *revision)
                    .with_context(|| format!("the save stores have no revision {revision}"))?;
                stores = target.stores;
                vanilla = target.vanilla;
                auto_message = format!("restore revision {revision}");
            }
        }
        self.commit(&cur, stores, vanilla, source, message.or(Some(auto_message)))
    }
}

// ---------------------------------------------------------------------------------------
// Merging

/// Copy `from` to `to` through a temporary file, keeping its modification time (so a
/// later merge still compares the save's own time).
fn copy_keep_mtime(from: &Path, to: &Path) -> Result<()> {
    let tmp = to.with_extension("save.merge");
    std::fs::copy(from, &tmp).with_context(|| format!("copy {}", from.display()))?;
    if let Ok(t) = std::fs::metadata(from).and_then(|m| m.modified()) {
        std::fs::File::options().write(true).open(&tmp)?.set_modified(t)?;
    }
    std::fs::rename(&tmp, to)?;
    Ok(())
}

/// Merge the slots of one account's folder `from` into `into`: per slot, the newer file
/// (by modification time) wins; a slot only `from` has is copied. `into`'s saves are backed
/// up (`backup_root`, with `reason`) before any is replaced. With `mark`, slots taken from
/// `from` get an upload marker (`into` syncs with the cloud). `from` is never changed.
#[allow(clippy::too_many_arguments)]
pub fn merge_dirs(
    from: &Path,
    into: &Path,
    account: &str,
    backup_root: &Path,
    product_id: u32,
    reason: &str,
    source: Option<&str>,
    mark: bool,
) -> Result<(Vec<MergeSlot>, Option<ubi_api::info::SaveBackup>)> {
    let a = crate::saves::local_slots(from)?;
    let b = crate::saves::local_slots(into)?;
    let all: BTreeSet<u32> = a.keys().chain(b.keys()).copied().collect();
    let name = |slot: u32| format!("{slot}.save");
    let mut out = Vec::new();
    for slot in all {
        let (fa, fb) = (a.get(&slot).map(|x| &x.0), b.get(&slot).map(|x| &x.0));
        let outcome = match (fa, fb) {
            (Some(_), None) => MergeOutcome::Copied,
            (None, _) => MergeOutcome::TargetOnly,
            (Some(x), Some(y)) => {
                let same = x.size == y.size && std::fs::read(from.join(name(slot)))? == std::fs::read(into.join(name(slot)))?;
                match (x.modified, y.modified) {
                    _ if same => MergeOutcome::Same,
                    (Some(tx), Some(ty)) if tx > ty => MergeOutcome::Replaced,
                    _ => MergeOutcome::Kept,
                }
            }
        };
        out.push(MergeSlot {
            account: account.to_owned(),
            slot,
            outcome,
            from_modified: fa.and_then(|f| f.modified),
            into_modified: fb.and_then(|f| f.modified),
        });
    }
    let backup = if out.iter().any(|s| s.outcome == MergeOutcome::Replaced) {
        crate::saves::backup_now(backup_root, product_id, into, reason, source)?
    } else {
        None
    };
    let takes: Vec<u32> = out.iter().filter(|s| matches!(s.outcome, MergeOutcome::Copied | MergeOutcome::Replaced)).map(|s| s.slot).collect();
    if !takes.is_empty() {
        std::fs::create_dir_all(into)?;
    }
    for slot in takes {
        copy_keep_mtime(&from.join(name(slot)), &into.join(name(slot)))?;
        if mark {
            std::fs::write(into.join(format!("{}.upload", name(slot))), b"")?;
        }
    }
    Ok((out, backup))
}

// ---------------------------------------------------------------------------------------
// Profiles and stores

/// Which store an older profile (no `saves` yet) has been using, by the rule from before
/// save stores: its own folder if `separate_saves` was on, or (auto) if a mod in its stack
/// isn't save-compatible or it already has a folder of its own.
pub(crate) fn legacy_separate(p: &Profile, mods: &[Layer], has_folder: bool) -> bool {
    match p.separate_saves {
        Some(v) => v,
        None => has_folder || mods.iter().any(|m| !m.save_compatible()),
    }
}

impl ModStore {
    pub fn save_stores(&self) -> SaveStores {
        SaveStores::new(self.data_dir(), self.product_id())
    }

    fn current_stores(&self) -> SaveStoresRevision {
        let stores = self.save_stores();
        stores.current().unwrap_or_else(|e| {
            tracing::warn!("save stores unreadable, using Default only: {e:#}");
            stores.initial()
        })
    }

    /// See [`ModStore::saves_plan`].
    pub(crate) fn store_saves_plan(&self, profile: Option<&Profile>, mods: &[Layer]) -> SavesPlan {
        let cur = self.current_stores();
        let default = cur.get(DEFAULT_STORE).cloned().unwrap_or_else(|| self.save_stores().default_store());
        let (store, why) = match profile {
            None => match cur.get(&cur.vanilla) {
                Some(s) if !s.is_default() => (s.clone(), format!("vanilla uses the '{}' store", s.name)),
                _ => (default, "vanilla uses the Default store (the game's own saves)".to_owned()),
            },
            Some(p) => match p.saves.as_deref() {
                Some(id) => match cur.get(id) {
                    Some(s) => (s.clone(), format!("the profile uses the '{}' store{}", s.name, if s.retired { " (retired)" } else { "" })),
                    None => (default, format!("the profile's save store {id} doesn't exist")),
                },
                None if legacy_separate(p, mods, self.legacy_saves(&p.id).is_some()) => {
                    // Not migrated yet, and the old rule gives it its own folder.
                    let folder = format!("{}-{}", self.product_id(), p.id);
                    let cloud = if p.cloud_saves { "synced with the cloud (cloud_saves=on)" } else { "kept on this PC" };
                    return SavesPlan {
                        separate: true,
                        reason: format!("the profile's own saves from before save stores; launching it makes them a save store; {cloud}"),
                        folder: format!("savegames/<user>/{folder}"),
                        cloud: p.cloud_saves,
                        ..Default::default()
                    };
                }
                None => (default, "not given a save store yet: shares Default".to_owned()),
            },
        };
        let cloud_note = (!store.cloud).then(|| match cur.cloud_store() {
            Some(c) => format!(
                "'{}' stays on this PC: Ubisoft's cloud keeps one set of saves per game, and '{}' is the store that syncs with it",
                store.name, c.name
            ),
            None => format!("'{}' stays on this PC: no save store syncs with Ubisoft's cloud", store.name),
        });
        SavesPlan {
            separate: store.folder != self.product_id().to_string(),
            reason: format!("{why}; {}", if store.cloud { "synced with the cloud" } else { "kept on this PC (not the cloud store)" }),
            folder: format!("savegames/<user>/{}", store.folder),
            cloud: store.cloud,
            store: Some(store.id.clone()),
            store_name: Some(store.name.clone()),
            cloud_note,
        }
    }

    /// Problems and warnings about a profile's save store, for its stack's notes.
    pub(crate) fn store_notes(&self, p: &Profile, mods: &[Layer], problems: &mut Vec<String>, warnings: &mut Vec<String>) {
        let Some(id) = p.saves.as_deref() else { return };
        let cur = self.current_stores();
        match cur.get(id) {
            None => problems.push(format!("its save store {id} doesn't exist; pick one (profile set PROFILE saves=default|new|STORE)")),
            Some(s) => {
                if s.retired {
                    warnings.push(format!("its save store '{}' is retired (quiver saves <game> store unretire {})", s.name, s.id));
                }
                warnings.extend(ubi_api::savestores::shared_store_warning(mods, s, cur.vanilla == s.id));
            }
        }
    }

    /// Give every live profile without a store one: its own folder as a store if the rule
    /// from before save stores gave it one (`separate_saves=on`, a folder of its own, or a
    /// mod that isn't save-compatible), else Default. Nothing moves; each change is a
    /// revision (the store's and the profile's), so it can be undone.
    pub(crate) fn migrate_saves(&self, dry_run: bool, source: &str, report: &mut ubi_api::mods::MigrationReport) -> Result<()> {
        let all = self.layers()?;
        let stores = self.save_stores();
        let pid = self.product_id();
        for (p, _) in self.profiles()? {
            if p.saves.is_some() {
                continue;
            }
            let mods: Vec<Layer> = p.layers.iter().filter_map(|id| all.iter().find(|l| &l.id == id)).filter(|l| l.enabled()).cloned().collect();
            let legacy = self.legacy_saves(&p.id);
            let mut q = p.clone();
            q.separate_saves = None;
            q.cloud_saves = false;
            let cur = stores.current()?;
            if legacy_separate(&p, &mods, legacy.is_some()) {
                let folder = format!("{pid}-{}", p.id);
                let why = match (p.separate_saves, &legacy) {
                    (Some(true), _) => "separate_saves was on",
                    (_, Some(_)) => "it has its own saves folder",
                    _ => "a mod in its stack isn't save-compatible",
                };
                if let Some(f) = &legacy {
                    report.kept_separate_saves.push((p.id.clone(), f.clone()));
                }
                let name = if dry_run {
                    cur.stores.iter().find(|s| s.folder == folder).map_or_else(|| SaveStores::free_name(&cur, &p.name), |s| s.name.clone())
                } else {
                    let (s, made) = stores.adopt_folder(
                        &p.name,
                        &folder,
                        format!("the own saves of profile '{}' from before save stores", p.name),
                        source,
                        format!("save stores: the own saves folder of profile '{}' ({folder}) becomes a store; {why}", p.name),
                    )?;
                    if made {
                        report.stores_created.push(s.id.clone());
                    }
                    q.saves = Some(s.id.clone());
                    let msg = format!("save stores: uses '{}', its own saves folder ({folder}); nothing moved", s.name);
                    self.append(q, false, source, Some(msg))?;
                    report.profile_stores.push((p.id.clone(), s.id.clone()));
                    s.name
                };
                report.notes.push(format!("profile '{}' keeps its own saves (savegames/<user>/{folder}) as save store '{name}' ({why})", p.name));
                if p.cloud_saves {
                    let cloud = cur.cloud_store().map_or("no store".to_owned(), |c| format!("'{}'", c.name));
                    report.notes.push(format!(
                        "profile '{}' had cloud_saves=on, so its saves and vanilla's took turns in Ubisoft's one set of cloud saves; now only {cloud} syncs and '{name}' stays on this PC. \
                         To share one set again: quiver saves {pid} merge '{name}' into default, then quiver mods {pid} profile set {} saves=default",
                        p.name, p.id
                    ));
                }
            } else {
                let why = if p.separate_saves == Some(false) { "separate_saves was off" } else { "every mod in its stack is save-compatible" };
                if !dry_run {
                    q.saves = Some(DEFAULT_STORE.into());
                    self.append(q, false, source, Some(format!("save stores: shares Default with vanilla ({why})")))?;
                    report.profile_stores.push((p.id.clone(), DEFAULT_STORE.into()));
                }
                report.notes.push(format!("profile '{}' uses Default ({why})", p.name));
                if let Some(f) = legacy {
                    // Nothing uses it any more; keep it as a retired store, so it can be merged.
                    let folder = format!("{pid}-{}", p.id);
                    let name = if dry_run {
                        cur.stores.iter().find(|s| s.folder == folder).map_or_else(|| SaveStores::free_name(&cur, &p.name), |s| s.name.clone())
                    } else {
                        let (s, made) = stores.keep_retired(
                            &p.name,
                            &folder,
                            format!("the old saves of profile '{}', unused since separate_saves=off", p.name),
                            source,
                            format!("save stores: the unused saves folder of profile '{}' ({folder}) is kept as a retired store", p.name),
                        )?;
                        if made {
                            report.stores_created.push(s.id.clone());
                        }
                        s.name
                    };
                    report.notes.push(format!(
                        "the old saves folder of profile '{}' ({f}) stays where it was, as retired store '{name}'; to combine it: quiver saves {pid} merge '{name}' into default",
                        p.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// Who uses which store: (store id, user) for vanilla and every live profile that has
    /// one.
    pub fn store_users(&self) -> Result<Vec<(String, StoreUser)>> {
        let cur = self.save_stores().current()?;
        let mut out = vec![(cur.vanilla.clone(), StoreUser { id: VANILLA.into(), name: "Vanilla".into() })];
        for (p, _) in self.profiles()? {
            if let Some(s) = &p.saves {
                out.push((s.clone(), StoreUser { id: p.id.clone(), name: p.name.clone() }));
            }
        }
        Ok(out)
    }

    /// Edit the stores (refusing to retire one in use).
    pub fn edit_stores(&self, op: &SaveStoreOp, source: &str, message: Option<String>) -> Result<SaveStoresRevision> {
        let users = self.store_users()?;
        self.save_stores().edit(op, &users, source, message)
    }

    /// The store a profile named `profile` should start with for `layers` (ids).
    pub fn saves_advice(&self, layers: &[String]) -> Result<SavesAdvice> {
        let all = self.layers()?;
        let mods: Vec<&Layer> = layers.iter().filter_map(|id| all.iter().find(|l| &l.id == id)).filter(|l| l.enabled()).collect();
        Ok(ubi_api::savestores::saves_advice(mods))
    }

    /// A store id for `query` (`new`: a new store named after `profile_name`).
    pub(crate) fn resolve_store(&self, query: &str, profile_name: &str, source: &str) -> Result<String> {
        if same_name(query, NEW_STORE) {
            let cur = self.save_stores().current()?;
            let name = SaveStores::free_name(&cur, profile_name);
            let note = Some(format!("made for profile '{profile_name}'"));
            let (store, _) = self.save_stores().create(&name, note, source, Some(format!("created '{name}' for profile '{profile_name}'")))?;
            return Ok(store.id);
        }
        let cur = self.save_stores().current()?;
        Ok(SaveStores::find(&cur, query, false)?.id.clone())
    }

    /// A game's stores with who uses each and what's in them, for `user` (`None`: every
    /// account folder in the prefix).
    pub fn stores_report(&self, user: Option<&str>) -> Result<SaveStoresReport> {
        let cur = self.save_stores().current()?;
        let users = self.store_users()?;
        let accounts: Vec<String> = match (user, self.game_prefix()) {
            (Some(u), _) => vec![u.to_owned()],
            (None, Some(p)) => account_dirs(p),
            (None, None) => Vec::new(),
        };
        let info = |s: &SaveStore| {
            let mut i = SaveStoreInfo {
                store: s.clone(),
                used_by: users.iter().filter(|(id, _)| *id == s.id).map(|(_, u)| u.clone()).collect(),
                ..Default::default()
            };
            if let Some(prefix) = self.game_prefix() {
                for a in &accounts {
                    let dir = store_dir(prefix, a, &s.folder);
                    for (f, _) in crate::saves::local_slots(&dir).unwrap_or_default().values() {
                        i.slots += 1;
                        i.bytes += f.size;
                        i.last_modified = i.last_modified.max(f.modified);
                    }
                    if accounts.len() == 1 {
                        i.dir = Some(dir);
                    }
                }
            }
            i
        };
        let unassigned = self
            .profiles()?
            .into_iter()
            .filter(|(p, _)| p.saves.is_none())
            .map(|(p, _)| StoreUser { id: p.id, name: p.name })
            .collect();
        Ok(SaveStoresReport {
            product_id: self.product_id(),
            revision: cur.revision,
            account: user.map(str::to_owned),
            stores: cur.stores.iter().filter(|s| !s.retired).map(info).collect(),
            retired: cur.stores.iter().filter(|s| s.retired).map(info).collect(),
            vanilla: cur.vanilla.clone(),
            cloud: cur.cloud_store().map(|s| s.id.clone()),
            unassigned,
        })
    }

    /// Merge store `from` into `into` (`None`: Default) for every account folder that has
    /// `from`'s folder (see [`merge_dirs`]). Needs [`ModStore::with_game`]. The caller makes
    /// sure the game isn't running.
    pub fn merge_stores(&self, from: &str, into: Option<&str>, backup_root: &Path, source: &str) -> Result<MergeReport> {
        let prefix = self.game_prefix().context("merging saves needs the installed game")?;
        let cur = self.save_stores().current()?;
        let a = SaveStores::find(&cur, from, true)?.clone();
        let b = SaveStores::find(&cur, into.unwrap_or(DEFAULT_STORE), false)?.clone();
        ensure!(a.id != b.id, "'{}' and '{}' are the same store", a.name, b.name);
        let reason = format!("before merging save store '{}' into '{}'", a.name, b.name);
        let mut report = MergeReport { from: a.clone(), into: b.clone(), marked_for_upload: b.cloud, ..Default::default() };
        for account in account_dirs(prefix) {
            let src = store_dir(prefix, &account, &a.folder);
            if !src.is_dir() {
                continue;
            }
            let dst = store_dir(prefix, &account, &b.folder);
            let (slots, backup) = merge_dirs(&src, &dst, &account, backup_root, self.product_id(), &reason, Some(source), b.cloud)?;
            report.slots.extend(slots);
            report.backups.extend(backup);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_at(dir: &Path, name: &str, data: &[u8], secs: i64) {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        let t = chrono::DateTime::<Utc>::from_timestamp(secs, 0).unwrap();
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(t.into()).unwrap();
    }

    #[test]
    fn stores_lifecycle_and_cloud_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SaveStores::new(tmp.path(), 3539);
        let cur = s.current().unwrap();
        assert_eq!(cur.revision, 0);
        assert_eq!(cur.stores, [s.default_store()]);
        assert_eq!(cur.cloud_store().unwrap().id, DEFAULT_STORE);
        assert!(s.history().unwrap().is_empty(), "nothing written until something changes");

        let (run, r1) = s.create("Hardcore", None, "cli", None).unwrap();
        assert_eq!(r1.revision, 1);
        assert_eq!(run.folder, format!("3539-{}", run.id));
        assert!(!run.cloud);
        assert!(s.create("hardcore", None, "cli", None).unwrap_err().to_string().contains("already"));
        assert!(s.create("new", None, "cli", None).is_err());
        assert!(s.create("Default", None, "cli", None).is_err());

        // One cloud store at most: moving the flag clears the old one in the same revision.
        let r2 = s.edit(&SaveStoreOp::Cloud { store: Some("hardcore".into()) }, &[], "cli", None).unwrap();
        assert_eq!(r2.cloud_store().unwrap().id, run.id);
        assert_eq!(r2.stores.iter().filter(|x| x.cloud).count(), 1);
        assert!(s.edit(&SaveStoreOp::Retire { store: run.id.clone() }, &[], "cli", None).unwrap_err().to_string().contains("cloud"));
        let r3 = s.edit(&SaveStoreOp::Cloud { store: None }, &[], "cli", None).unwrap();
        assert!(r3.cloud_store().is_none());
        s.edit(&SaveStoreOp::Cloud { store: Some("default".into()) }, &[], "cli", None).unwrap();

        // In use: not retired. Retired: files stay, name free, can come back.
        let users = [(run.id.clone(), StoreUser { id: "p1".into(), name: "Hard".into() })];
        assert!(s.edit(&SaveStoreOp::Retire { store: "Hardcore".into() }, &users, "cli", None).unwrap_err().to_string().contains("Hard"));
        assert!(s.edit(&SaveStoreOp::Retire { store: "default".into() }, &[], "cli", None).is_err());
        let r = s.edit(&SaveStoreOp::Retire { store: "Hardcore".into() }, &[], "cli", None).unwrap();
        assert!(r.get(&run.id).unwrap().retired);
        assert!(SaveStores::find(&r, "Hardcore", false).is_err());
        let (again, _) = s.create("Hardcore", None, "cli", None).unwrap();
        let r = s.edit(&SaveStoreOp::Unretire { store: run.id.clone() }, &[], "cli", None).unwrap();
        assert_eq!(r.get(&run.id).unwrap().name, "Hardcore 2", "names stay unique");
        assert_ne!(again.id, run.id);

        // Vanilla can use another store; restore puts an old state back as a new revision.
        let r = s.edit(&SaveStoreOp::Vanilla { store: again.id.clone() }, &[], "cli", None).unwrap();
        assert_eq!(r.vanilla, again.id);
        let back = s.edit(&SaveStoreOp::Restore { revision: 1 }, &[], "cli", None).unwrap();
        assert_eq!(back.vanilla, DEFAULT_STORE);
        assert_eq!(back.stores, r1.stores);
        assert_eq!(back.revision, s.history().unwrap().len() as u32);
        // No-op edits record nothing.
        let n = s.history().unwrap().len();
        s.edit(&SaveStoreOp::Rename { store: "default".into(), name: "Default".into() }, &[], "cli", None).unwrap();
        assert_eq!(s.history().unwrap().len(), n);
    }

    #[test]
    fn adopt_folder_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SaveStores::new(tmp.path(), 3539);
        let (a, made) = s.adopt_folder("DLSS", "3539-ptkzxpv", "n".into(), "cli", "m".into()).unwrap();
        assert!(made && a.folder == "3539-ptkzxpv" && a.name == "DLSS");
        let (b, made) = s.adopt_folder("DLSS", "3539-ptkzxpv", "n".into(), "cli", "m".into()).unwrap();
        assert!(!made && b == a);
        assert_eq!(s.history().unwrap().len(), 1);
    }

    #[test]
    fn merge_newer_wins_with_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, into, root) = (tmp.path().join("savegames/u1/3539-p"), tmp.path().join("savegames/u1/3539"), tmp.path().join("bk"));
        write_at(&from, "1.save", b"from newer", 2000);
        write_at(&into, "1.save", b"into older", 1000);
        write_at(&from, "2.save", b"from older", 1000);
        write_at(&into, "2.save", b"into newer", 2000);
        write_at(&from, "3.save", b"only from", 1500);
        write_at(&into, "4.save", b"only into", 1500);
        write_at(&from, "5.save", b"same", 1000);
        write_at(&into, "5.save", b"same", 3000);
        let (slots, backup) = merge_dirs(&from, &into, "u1", &root, 3539, "before merging", Some("cli"), true).unwrap();
        let outcomes: Vec<(u32, MergeOutcome)> = slots.iter().map(|s| (s.slot, s.outcome)).collect();
        assert_eq!(
            outcomes,
            [
                (1, MergeOutcome::Replaced),
                (2, MergeOutcome::Kept),
                (3, MergeOutcome::Copied),
                (4, MergeOutcome::TargetOnly),
                (5, MergeOutcome::Same)
            ]
        );
        assert_eq!(std::fs::read(into.join("1.save")).unwrap(), b"from newer");
        assert_eq!(std::fs::read(into.join("2.save")).unwrap(), b"into newer");
        assert_eq!(std::fs::read(into.join("3.save")).unwrap(), b"only from");
        // Copies keep their own time; taken slots are marked for upload; the source is untouched.
        let m = std::fs::metadata(into.join("1.save")).unwrap().modified().unwrap();
        assert_eq!(chrono::DateTime::<Utc>::from(m).timestamp(), 2000);
        assert!(into.join("1.save.upload").exists() && into.join("3.save.upload").exists());
        assert!(!into.join("2.save.upload").exists());
        assert_eq!(std::fs::read(from.join("2.save")).unwrap(), b"from older");
        // Everything that was in the target is in the backup.
        let b = backup.unwrap();
        assert_eq!(b.slots.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2, 4, 5]);
        assert_eq!(std::fs::read(b.path.join("1.save")).unwrap(), b"into older");
        assert_eq!(b.account.as_deref(), Some("u1"));
        // Merging again changes nothing and backs up nothing.
        let (slots, backup) = merge_dirs(&from, &into, "u1", &root, 3539, "again", None, true).unwrap();
        assert!(backup.is_none());
        assert!(slots.iter().all(|s| !matches!(s.outcome, MergeOutcome::Replaced | MergeOutcome::Copied)));
    }
}
