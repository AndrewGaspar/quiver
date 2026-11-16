//! Mod layers and profiles on disk (see `ubi_api::mods` for the model).
//!
//! ```text
//! <data>/mods/<product>/layers/<layer-id>/        the layer's files, as overlaid on the install
//! <data>/mods/<product>/layers/<layer-id>.json    its metadata (ubi_api::mods::Layer)
//! <data>/mods/<product>/profiles/<id>/upper/      what the game wrote while running the profile
//! <data>/mods/<product>/profiles/<id>/work/       overlayfs scratch space
//! <data>/games/<product>/profiles/<id>.jsonl      the profile's history; the last line is current
//! ```
//!
//! Profile history lives next to the game's settings history (`games/<product>/`) and uses
//! the same JSON-lines revisions, but per profile: each profile is its own scope with its
//! own revision numbers, so restoring game settings never touches a profile and the
//! reverse. Deleting a profile appends a `deleted` revision; restoring an earlier one
//! brings it back with its saves and writable layer intact.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use ubi_api::mods::{Conflict, Layer, ModsState, Profile, ProfileInfo, ProfileRevision, VANILLA};
use ubi_api::SettingOp;

use crate::overlay::OverlaySpec;
use crate::settings::{append_jsonl, read_jsonl};

/// What the launcher needs to run a modded profile.
#[derive(Debug, Clone)]
pub struct ProfileLaunch {
    pub profile: Profile,
    pub revision: u32,
    pub overlay: OverlaySpec,
}

/// One game's layers and profiles under a data directory.
#[derive(Debug, Clone)]
pub struct ModStore {
    data: PathBuf,
    product_id: u32,
}

/// A fresh random id: `prefix` and 6 base-36 characters.
fn new_id(prefix: char) -> Result<String> {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut n = u64::from_le_bytes(bytes);
    let mut id = String::from(prefix);
    for _ in 0..6 {
        id.push(char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
    }
    Ok(id)
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Files under `dir` (relative paths with `/`) and their total size. Symlinks count as
/// files and aren't followed.
fn walk(dir: &Path) -> Result<Vec<(String, u64)>> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_dir() {
                go(root, &e.path(), out)?;
            } else {
                let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.push((rel, if ty.is_file() { e.metadata()?.len() } else { 0 }));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Rename entries under `layer` whose names differ only in case from the install's, so
/// they replace the install's files instead of sitting beside them (Windows games don't
/// care about case; overlayfs does). Returns how many were renamed.
pub fn match_case(layer: &Path, install: &Path) -> Result<usize> {
    let Ok(rd) = std::fs::read_dir(install) else { return Ok(0) };
    let theirs: BTreeMap<String, String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|n| (n.to_lowercase(), n))
        .collect();
    let mut renamed = 0;
    let mut entries: Vec<String> =
        std::fs::read_dir(layer)?.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    entries.sort();
    for name in entries {
        let mut name = name;
        if let Some(target) = theirs.get(&name.to_lowercase())
            && *target != name
            && !layer.join(target).exists()
        {
            std::fs::rename(layer.join(&name), layer.join(target))?;
            name = target.clone();
            renamed += 1;
        }
        let path = layer.join(&name);
        if path.symlink_metadata()?.is_dir() {
            renamed += match_case(&path, &install.join(&name))?;
        }
    }
    Ok(renamed)
}

impl ModStore {
    pub fn new(data: &Path, product_id: u32) -> Self {
        Self { data: data.to_owned(), product_id }
    }

    /// The store under Quiver's data directory.
    pub fn open(product_id: u32) -> Self {
        Self::new(&crate::launcher::data_dir(), product_id)
    }

    fn mods_dir(&self) -> PathBuf {
        self.data.join("mods").join(self.product_id.to_string())
    }

    fn layers_dir(&self) -> PathBuf {
        self.mods_dir().join("layers")
    }

    pub fn layer_dir(&self, id: &str) -> PathBuf {
        self.layers_dir().join(id)
    }

    fn layer_meta(&self, id: &str) -> PathBuf {
        self.layers_dir().join(format!("{id}.json"))
    }

    /// The profile's writable layer and overlayfs work dir live here.
    pub fn profile_dir(&self, id: &str) -> PathBuf {
        self.mods_dir().join("profiles").join(id)
    }

    fn histories_dir(&self) -> PathBuf {
        self.data.join("games").join(self.product_id.to_string()).join("profiles")
    }

    fn history_path(&self, id: &str) -> PathBuf {
        self.histories_dir().join(format!("{id}.jsonl"))
    }

    // -- layers ---------------------------------------------------------------------------

    /// All layers, oldest first.
    pub fn layers(&self) -> Result<Vec<Layer>> {
        let Ok(rd) = std::fs::read_dir(self.layers_dir()) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for e in rd {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json") {
                let text = std::fs::read_to_string(&p)?;
                out.push(serde_json::from_str::<Layer>(&text).with_context(|| format!("corrupt {}", p.display()))?);
            }
        }
        out.sort_by(|a, b| a.added.cmp(&b.added).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    /// A layer by id or name.
    pub fn layer(&self, query: &str) -> Result<Layer> {
        let layers = self.layers()?;
        if let Some(l) = layers.iter().find(|l| l.id == query) {
            return Ok(l.clone());
        }
        let found: Vec<&Layer> = layers.iter().filter(|l| same_name(&l.name, query)).collect();
        match found.as_slice() {
            [l] => Ok((*l).clone()),
            [] => bail!(
                "no layer '{query}' (layers: {})",
                if layers.is_empty() { "none".into() } else { layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ") }
            ),
            _ => bail!("'{query}' names more than one layer; use its id"),
        }
    }

    /// Copy `src` into the store as a new layer (reflinks where the filesystem supports
    /// them), with names matched to `install`'s case.
    pub fn add_layer(&self, name: &str, src: &Path, install: Option<&Path>, source: &str) -> Result<Layer> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a layer needs a name");
        }
        if self.layers()?.iter().any(|l| same_name(&l.name, name)) {
            bail!("there is already a layer named '{name}'");
        }
        let src = src.canonicalize().with_context(|| format!("{} not found", src.display()))?;
        if !src.is_dir() {
            bail!("{} is not a folder", src.display());
        }
        if src.starts_with(self.data.join("mods")) {
            bail!("{} is already in the mod store", src.display());
        }
        let id = loop {
            let id = new_id('l')?;
            if !self.layer_dir(&id).exists() {
                break id;
            }
        };
        std::fs::create_dir_all(self.layers_dir())?;
        let tmp = self.layers_dir().join(format!(".partial-{id}"));
        let copied = (|| -> Result<()> {
            std::fs::create_dir(&tmp)?;
            let out = std::process::Command::new("cp")
                .args(["-a", "--reflink=auto", "--no-preserve=ownership"])
                .arg(src.join("."))
                .arg(&tmp)
                .output()
                .context("running cp")?;
            if !out.status.success() {
                bail!("copying {} failed: {}", src.display(), String::from_utf8_lossy(&out.stderr).trim());
            }
            if let Some(install) = install {
                match_case(&tmp, install)?;
            }
            Ok(())
        })();
        if let Err(e) = copied {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        let files = walk(&tmp)?;
        std::fs::rename(&tmp, self.layer_dir(&id))?;
        let layer = Layer {
            id: id.clone(),
            name: name.to_owned(),
            added: chrono::Utc::now(),
            added_by: source.to_owned(),
            origin: Some(src),
            files: files.len() as u64,
            bytes: files.iter().map(|f| f.1).sum(),
        };
        std::fs::write(self.layer_meta(&id), serde_json::to_vec_pretty(&layer)?)?;
        Ok(layer)
    }

    /// Delete a layer and its files. Refused while a profile uses it.
    pub fn remove_layer(&self, query: &str) -> Result<Layer> {
        let layer = self.layer(query)?;
        let users: Vec<String> =
            self.profiles()?.into_iter().filter(|(p, _)| p.layers.contains(&layer.id)).map(|(p, _)| p.name).collect();
        if !users.is_empty() {
            bail!("layer '{}' is used by {}; take it out of those profiles first", layer.name, users.join(", "));
        }
        std::fs::remove_file(self.layer_meta(&layer.id))?;
        let dir = self.layer_dir(&layer.id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(layer)
    }

    /// Layer queries (ids or names) to ids, keeping order.
    fn resolve_layers(&self, queries: &[String]) -> Result<Vec<String>> {
        let mut ids: Vec<String> = Vec::new();
        for q in queries {
            let id = self.layer(q)?.id;
            if ids.contains(&id) {
                bail!("layer '{q}' is listed twice");
            }
            ids.push(id);
        }
        Ok(ids)
    }

    /// Files that more than one of `layers` (bottom to top) provides, compared without
    /// case like the game does.
    pub fn conflicts(&self, layers: &[String]) -> Vec<Conflict> {
        let mut seen: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
        for id in layers {
            for (path, _) in walk(&self.layer_dir(id)).unwrap_or_default() {
                let e = seen.entry(path.to_lowercase()).or_insert_with(|| (path.clone(), Vec::new()));
                e.1.push(id.clone());
            }
        }
        seen.into_values().filter(|(_, l)| l.len() > 1).map(|(path, layers)| Conflict { path, layers }).collect()
    }

    // -- profiles -------------------------------------------------------------------------

    pub fn history(&self, id: &str) -> Result<Vec<ProfileRevision>> {
        read_jsonl(&self.history_path(id))
    }

    /// Every profile ever created (deleted ones included) with its latest revision.
    fn all_profiles(&self) -> Result<Vec<ProfileRevision>> {
        let Ok(rd) = std::fs::read_dir(self.histories_dir()) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for e in rd {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "jsonl")
                && let Some(last) = read_jsonl::<ProfileRevision>(&p)?.pop()
            {
                out.push(last);
            }
        }
        out.sort_by_key(|r| r.profile.name.to_lowercase());
        Ok(out)
    }

    /// Live profiles and their current revision, by name.
    pub fn profiles(&self) -> Result<Vec<(Profile, u32)>> {
        Ok(self.all_profiles()?.into_iter().filter(|r| !r.deleted).map(|r| (r.profile, r.revision)).collect())
    }

    fn find(&self, query: &str, include_deleted: bool) -> Result<ProfileRevision> {
        if same_name(query, VANILLA) {
            bail!("vanilla is the unmodded game; it has no layers or history");
        }
        let all = self.all_profiles()?;
        if let Some(r) = all.iter().find(|r| r.profile.id == query && (include_deleted || !r.deleted)) {
            return Ok(r.clone());
        }
        let named = |deleted: bool| -> Vec<&ProfileRevision> {
            all.iter().filter(|r| r.deleted == deleted && same_name(&r.profile.name, query)).collect()
        };
        let mut found = named(false);
        if found.is_empty() && include_deleted {
            found = named(true);
        }
        match found.as_slice() {
            [r] => Ok((*r).clone()),
            [] => {
                let live: Vec<&str> = all.iter().filter(|r| !r.deleted).map(|r| r.profile.name.as_str()).collect();
                bail!("no profile '{query}' (profiles: {})", if live.is_empty() { "none".into() } else { live.join(", ") })
            }
            _ => bail!("'{query}' names more than one deleted profile; use its id"),
        }
    }

    /// A profile's history by id or name, deleted profiles included.
    pub fn history_of(&self, query: &str) -> Result<(Profile, Vec<ProfileRevision>)> {
        let r = self.find(query, true)?;
        Ok((r.profile.clone(), self.history(&r.profile.id)?))
    }

    /// A live profile by id or name.
    pub fn profile(&self, query: &str) -> Result<(Profile, u32)> {
        let r = self.find(query, false)?;
        Ok((r.profile, r.revision))
    }

    fn check_name(&self, name: &str, id: &str) -> Result<()> {
        if same_name(name, VANILLA) {
            bail!("'vanilla' is reserved for the unmodded game");
        }
        for (p, _) in self.profiles()? {
            if p.id != id && (same_name(&p.name, name) || p.id == name) {
                bail!("there is already a profile named '{name}'");
            }
        }
        Ok(())
    }

    fn append(&self, profile: Profile, deleted: bool, source: &str, message: Option<String>) -> Result<ProfileRevision> {
        let path = self.history_path(&profile.id);
        let revision = read_jsonl::<ProfileRevision>(&path)?.last().map_or(0, |r| r.revision) + 1;
        let rev = ProfileRevision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, deleted, profile };
        append_jsonl(&path, &rev)?;
        Ok(rev)
    }

    /// A new profile with `layers` (ids or names, bottom to top).
    pub fn create_profile(&self, name: &str, layers: &[String], source: &str, message: Option<String>) -> Result<ProfileRevision> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a profile needs a name");
        }
        let id = loop {
            let id = new_id('p')?;
            if !self.history_path(&id).exists() {
                break id;
            }
        };
        self.check_name(name, &id)?;
        let layers = self.resolve_layers(layers)?;
        self.append(Profile { id, name: name.to_owned(), layers, ..Default::default() }, false, source, message)
    }

    /// Edit a profile ([`Profile::apply`] keys; `layers` takes ids or names). No-op edits
    /// don't create a revision.
    pub fn set_profile(&self, query: &str, ops: &[SettingOp], source: &str, message: Option<String>) -> Result<ProfileRevision> {
        let current = self.find(query, false)?;
        let mut p = current.profile.clone();
        for op in ops {
            let op = match op {
                SettingOp::Set { key, value } if key == "layers" => {
                    let ids = self.resolve_layers(&ubi_api::mods::split_list(value))?;
                    SettingOp::Set { key: key.clone(), value: ids.join(",") }
                }
                op => op.clone(),
            };
            p.apply(&op).map_err(anyhow::Error::msg)?;
        }
        if p == current.profile {
            return Ok(current);
        }
        self.check_name(&p.name, &p.id)?;
        self.append(p, false, source, message)
    }

    /// Delete a profile: a `deleted` revision. Its saves and writable layer stay on disk,
    /// so restoring an earlier revision brings it back as it was.
    pub fn delete_profile(&self, query: &str, source: &str) -> Result<ProfileRevision> {
        let current = self.find(query, false)?;
        self.append(current.profile, true, source, Some("deleted".into()))
    }

    /// Make an earlier revision current again (recorded as a new revision). Works for
    /// deleted profiles too.
    pub fn restore_profile(&self, query: &str, revision: u32, source: &str) -> Result<ProfileRevision> {
        let current = self.find(query, true)?;
        let target = self
            .history(&current.profile.id)?
            .into_iter()
            .find(|r| r.revision == revision)
            .with_context(|| format!("profile '{}' has no revision {revision}", current.profile.name))?;
        if target.deleted {
            bail!("revision {revision} is the deletion; restore an earlier one");
        }
        self.check_name(&target.profile.name, &target.profile.id)?;
        self.append(target.profile, false, source, Some(format!("restore revision {revision}")))
    }

    fn info(&self, r: ProfileRevision) -> ProfileInfo {
        let missing_layers = r.profile.layers.iter().filter(|id| !self.layer_meta(id).exists()).cloned().collect();
        let conflicts = self.conflicts(&r.profile.layers);
        ProfileInfo { profile: r.profile, revision: r.revision, missing_layers, conflicts }
    }

    pub fn state(&self) -> Result<ModsState> {
        let (deleted, live): (Vec<_>, Vec<_>) = self.all_profiles()?.into_iter().partition(|r| r.deleted);
        Ok(ModsState {
            product_id: self.product_id,
            layers: self.layers()?,
            profiles: live.into_iter().map(|r| self.info(r)).collect(),
            deleted: deleted.into_iter().map(|r| self.info(r)).collect(),
        })
    }

    /// What a launch of `query` needs; `None` for vanilla.
    pub fn launch(&self, query: Option<&str>, install_dir: &Path) -> Result<Option<ProfileLaunch>> {
        let Some(query) = query.filter(|q| !same_name(q, VANILLA)) else { return Ok(None) };
        let (profile, revision) = self.profile(query)?;
        let mut lower = Vec::new();
        for id in profile.layers.iter().rev() {
            let dir = self.layer_dir(id);
            if !self.layer_meta(id).exists() || !dir.is_dir() {
                bail!("profile '{}' uses layer {id}, which was removed; update its layers", profile.name);
            }
            lower.push(dir);
        }
        let state = self.profile_dir(&profile.id);
        let overlay = OverlaySpec { target: install_dir.to_owned(), lower, upper: state.join("upper"), work: state.join("work") };
        Ok(Some(ProfileLaunch { profile, revision, overlay }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(list: &[&str]) -> Vec<SettingOp> {
        list.iter().map(|s| SettingOp::parse(s).unwrap()).collect()
    }

    fn folder(root: &Path, files: &[(&str, &str)]) -> PathBuf {
        for (p, c) in files {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        root.to_owned()
    }

    #[test]
    fn ids_are_random_and_shaped() {
        let a = new_id('p').unwrap();
        let b = new_id('p').unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 7);
        assert!(a.starts_with('p') && a[1..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn layers_add_remove_and_case() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let install = folder(&t.join("install"), &[("Data/Textures/rock.dds", "v"), ("game.exe", "e")]);
        let src = folder(&t.join("src"), &[("data/textures/ROCK.dds", "m"), ("data/textures/new.dds", "n"), ("ReShade.ini", "r")]);
        let store = ModStore::new(&t.join("data"), 42);
        let l = store.add_layer("HD rocks", &src, Some(&install), "cli").unwrap();
        assert_eq!((l.files, l.bytes), (3, 3));
        assert_eq!(l.origin.as_deref(), Some(src.canonicalize().unwrap().as_path()));
        // Case matched to the install so it replaces the vanilla file.
        let dir = store.layer_dir(&l.id);
        assert_eq!(std::fs::read_to_string(dir.join("Data/Textures/rock.dds")).unwrap(), "m");
        assert!(dir.join("Data/Textures/new.dds").is_file());
        // The source folder is left as it was.
        assert!(src.join("data/textures/ROCK.dds").is_file());
        assert!(store.add_layer("hd ROCKS", &src, None, "cli").unwrap_err().to_string().contains("already"));
        assert!(store.add_layer("x", &t.join("nope"), None, "cli").is_err());
        assert_eq!(store.layer("hd rocks").unwrap().id, l.id);
        assert_eq!(store.layer(&l.id).unwrap().name, "HD rocks");

        let p = store.create_profile("Rocks", &["HD rocks".into()], "cli", None).unwrap();
        assert!(store.remove_layer(&l.id).unwrap_err().to_string().contains("Rocks"));
        store.set_profile("rocks", &ops(&["layers="]), "cli", None).unwrap();
        store.remove_layer("HD rocks").unwrap();
        assert!(!dir.exists());
        assert!(store.layers().unwrap().is_empty());
        // Nothing left behind but the profile.
        assert!(std::fs::read_dir(t.join("data/mods/42/layers")).unwrap().next().is_none());
        assert_eq!(store.profile(&p.profile.id).unwrap().1, 2);
    }

    #[test]
    fn profile_history_and_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let store = ModStore::new(&t.join("data"), 7);
        let a = store.add_layer("A", &folder(&t.join("a"), &[("x.txt", "a"), ("y.txt", "a")]), None, "cli").unwrap();
        let b = store.add_layer("B", &folder(&t.join("b"), &[("X.txt", "b")]), None, "cli").unwrap();

        let r1 = store.create_profile("Visual overhaul", &["A".into()], "gui", Some("first".into())).unwrap();
        let id = r1.profile.id.clone();
        assert_eq!(r1.revision, 1);
        assert_eq!(r1.profile.layers, [a.id.clone()]);
        assert!(!r1.profile.cloud_saves && !r1.profile.pinned);
        assert!(store.create_profile("visual OVERHAUL", &[], "cli", None).is_err());
        assert!(store.create_profile("Vanilla", &[], "cli", None).is_err());
        assert!(store.create_profile("x", &["nope".into()], "cli", None).is_err());
        assert!(store.create_profile("x", &["A".into(), "A".into()], "cli", None).is_err());

        let r2 = store.set_profile("visual overhaul", &ops(&["layers=A,B", "cloud_saves=on"]), "agent", None).unwrap();
        assert_eq!(r2.revision, 2);
        assert_eq!(r2.profile.layers, [a.id.clone(), b.id.clone()]);
        assert!(r2.profile.cloud_saves);
        // A no-op doesn't make a revision.
        assert_eq!(store.set_profile(&id, &ops(&["cloud_saves=on"]), "cli", None).unwrap().revision, 2);
        let r3 = store.set_profile(&id, &ops(&["pinned=on", "env.DXVK_HUD=fps", "name=VO"]), "cli", None).unwrap();
        assert_eq!(r3.revision, 3);
        assert!(r3.profile.pinned);
        assert_eq!(r3.profile.id, id, "renames keep the id");
        assert!(store.profile("Visual overhaul").is_err());
        assert_eq!(store.profile("vo").unwrap().1, 3);

        // Conflicts between layers ignore case; the top layer is listed last.
        let state = store.state().unwrap();
        assert_eq!(state.profiles.len(), 1);
        assert_eq!(state.profiles[0].conflicts, [Conflict { path: "x.txt".into(), layers: vec![a.id.clone(), b.id.clone()] }]);

        // Restore r1, then delete and restore again.
        let r4 = store.restore_profile("VO", 1, "cli").unwrap();
        assert_eq!(r4.revision, 4);
        assert_eq!(r4.profile, r1.profile);
        assert_eq!(r4.message.as_deref(), Some("restore revision 1"));
        let r5 = store.delete_profile(&id, "cli").unwrap();
        assert!(r5.deleted);
        assert!(store.profiles().unwrap().is_empty());
        assert!(store.profile("Visual overhaul").is_err());
        assert_eq!(store.state().unwrap().deleted.len(), 1);
        assert!(store.restore_profile(&id, 5, "cli").is_err(), "the deletion itself can't be restored");
        // The name is free again while deleted; restoring then needs it back.
        let other = store.create_profile("Visual overhaul", &[], "cli", None).unwrap();
        assert!(store.restore_profile(&id, 1, "cli").unwrap_err().to_string().contains("already"));
        store.delete_profile(&other.profile.id, "cli").unwrap();
        let r6 = store.restore_profile(&id, 3, "cli").unwrap();
        assert_eq!((r6.revision, r6.profile.name.as_str()), (6, "VO"));
        let hist = store.history(&id).unwrap();
        assert_eq!(hist.iter().map(|r| r.revision).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(hist.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(), ["gui", "agent", "cli", "cli", "cli", "cli"]);
        // Deleted profiles are found by name too.
        assert!(store.restore_profile("visual overhaul", 1, "cli").is_ok());
    }

    #[test]
    fn launch_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let install = folder(&t.join("install"), &[("a", "v")]);
        let store = ModStore::new(&t.join("data"), 9);
        let a = store.add_layer("A", &folder(&t.join("a"), &[("a", "1")]), None, "cli").unwrap();
        let b = store.add_layer("B", &folder(&t.join("b"), &[("a", "2")]), None, "cli").unwrap();
        assert!(store.launch(None, &install).unwrap().is_none());
        assert!(store.launch(Some("Vanilla"), &install).unwrap().is_none());
        assert!(store.launch(Some("nope"), &install).is_err());
        let p = store.create_profile("P", &["A".into(), "B".into()], "cli", None).unwrap();
        let l = store.launch(Some("p"), &install).unwrap().unwrap();
        assert_eq!(l.revision, 1);
        assert_eq!(l.overlay.target, install);
        // Topmost first, as overlayfs wants.
        assert_eq!(l.overlay.lower, [store.layer_dir(&b.id), store.layer_dir(&a.id)]);
        assert_eq!(l.overlay.upper, store.profile_dir(&p.profile.id).join("upper"));
        // A removed layer is an error, not a silently different game.
        std::fs::remove_file(store.layer_meta(&b.id)).unwrap();
        assert!(store.launch(Some("P"), &install).unwrap_err().to_string().contains(&b.id));
        assert_eq!(store.state().unwrap().profiles[0].missing_layers, [b.id.clone()]);
    }
}
