//! Mods, layers and profiles on disk (see `quiver_api::mods` for the model).
//!
//! ```text
//! <data>/mods/<product>/layers/<mod-id>/          the mod's install files, as overlaid on the install
//! <data>/mods/<product>/layers/<mod-id>.json      its metadata and manifest (quiver_api::mods::Layer)
//! <data>/mods/<product>/profiles/<id>/upper/      what the game wrote to the install folder in the profile
//! <data>/mods/<product>/profiles/<id>/work/       overlayfs scratch space
//! <data>/mods/<product>/profiles/<id>/prefix/<dir>  the profile's own copy of a prefix folder
//! <data>/mods/<product>/profiles/vanilla/upper/   the same for vanilla, when fixes with files apply to it
//! <data>/mods/<product>/patches.json              config keys Quiver changed and their own values
//! <data>/games/<product>/profiles/<id>.jsonl      the profile's history; the last line is current
//! <data>/games/<product>/layers/<mod-id>.jsonl    the history of the mod's deletions and metadata
//! <data>/games/<product>/mods/<mod-id>.jsonl      a versioned mod's history (crate::modversions)
//! ```
//!
//! **Mod versions** ([`crate::modversions`]): a mod has one or more versions, each a layer
//! of its own (folder, metadata, deletions history). A layer that was never versioned is a
//! mod with one version whose id is the mod's. A profile's stack entry `MOD` tracks the
//! mod's current version, `MOD@VERSION` (or a bare version id) is pinned.
//!
//! **Mods v2.** An installed mod is a layer with a manifest (launch settings, config
//! patches, metadata); a layer from before Mods v2 is a file-only mod and needs no
//! migration: same id, same folder, same history. A profile's `layers` are its mod stack
//! and its `settings` its own overrides, so v1 profiles read as v2 profiles unchanged. Saves
//! are separate from mods: each profile points at a save store ([`crate::savestores`]). A
//! profile that already has its own saves folder keeps it ([`ModStore::migrate`] makes that
//! folder a store), so no saves move or vanish.
//!
//! Profile history lives next to the game's settings history (`games/<product>/`) and uses
//! the same JSON-lines revisions, but per profile: each profile is its own scope with its
//! own revision numbers, so restoring game settings never touches a profile and the
//! reverse. Deleting a profile appends a `deleted` revision; restoring an earlier one
//! brings it back with its saves and writable layer intact.
//!
//! A layer's files never change after `add`, but its deletions can, so those get the same
//! kind of per-layer history. A deletion is an overlayfs whiteout in the layer's folder (a
//! character device 0:0, which unprivileged users may create since Linux 5.8) or, for a
//! folder the layer has files of its own in, an opaque folder (`user.overlay.opaque=y`,
//! read because the overlay is mounted with `userxattr`). Both only act inside a launch's
//! mount: the install itself is never touched.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use quiver_api::mods::{
    Conflict, Layer, LayerChange, LayerRevision, ModRecord, ModsState, Profile, ProfileInfo, ProfileRevision, ResolvedMod, VANILLA,
};
use quiver_api::mods::{
    AppliedKey, CatalogEntry, ConfigDir, LaunchPlan, MigrationReport, ModDetail, ModKind, ModManifest, Mount, PatchState,
    PatchStatus, PatchedFile, SavesPlan, SettingConflict, ValueFrom, mods_of, resolve_entry, split_entry, stack_entry,
};
use quiver_api::{Game, GameSettings, SettingOp};

use crate::confpatch::{self, View, Wanted};
use crate::overlay::OverlaySpec;
use crate::settings::{append_jsonl, read_jsonl};

/// What the launcher needs to run a modded profile.
#[derive(Debug, Clone)]
pub struct ProfileLaunch {
    pub profile: Profile,
    pub revision: u32,
    pub overlay: OverlaySpec,
}

/// Everything a launch needs (Mods v2): what the API shows ([`LaunchPlan`]) and the
/// mounts and config patches to apply.
#[derive(Debug, Clone)]
pub struct Plan {
    pub api: LaunchPlan,
    /// Mounts for the launch's namespace (none: vanilla without file fixes).
    pub overlays: Vec<OverlaySpec>,
    /// Config patches the stack wants, in stack order.
    pub patches: Vec<Wanted>,
    /// The prefix as the launch sees it.
    pub view: View,
    /// The record of changed config keys (`patches.json`).
    pub record: PathBuf,
    /// The saves folder the launch uses, under `savegames/<user>/` (its save store's
    /// folder: `<product>` for Default).
    pub saves_folder: String,
    /// The launch's session id (`QUIVER_SESSION` for the bridge and the game), set by the
    /// daemon.
    pub session: Option<String>,
}

impl Plan {
    /// Bring the config files this launch sees in line with its stack (see
    /// [`confpatch::reconcile`]). Call before mounting the namespace.
    pub fn apply_patches(&self) -> Result<Vec<PatchStatus>> {
        // No prefix (a native game): the plan has no prefix patches, and nothing may resolve
        // against an empty path.
        if self.view.prefix.as_os_str().is_empty() {
            return Ok(Vec::new());
        }
        confpatch::reconcile(&self.record, &self.view, &self.patches)
    }
}

/// How a mod to install is given.
#[derive(Debug, Clone)]
pub enum Package {
    /// From Quiver's catalog, by id.
    Builtin(String),
    /// A folder: a package with a manifest, or a plain folder of files (named `name` or
    /// after the folder).
    Dir { path: PathBuf, name: Option<String> },
}

/// One game's layers and profiles under a data directory.
#[derive(Debug, Clone)]
pub struct ModStore {
    data: PathBuf,
    /// The game the store is for: its folders are `games/<dir>` and `mods/<dir>`
    /// ([`quiver_api::GameRef::dir_name`]: `3539` for a Ubisoft product, as always, and
    /// `steam-620` for a Steam game).
    key: quiver_api::GameRef,
    /// The game, for what needs its prefix or install folder (saves, per-profile folders,
    /// config patches, builds).
    game: Option<Game>,
    /// The app-level `fixes_on_vanilla` setting.
    fixes_on_vanilla: Option<bool>,
}

/// A fresh random id: `prefix` and 6 base-36 characters.
pub(crate) fn new_id(prefix: char) -> Result<String> {
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

pub(crate) fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Files under `dir` (relative paths with `/`) and their total size. Symlinks count as
/// files and aren't followed; whiteouts (deletions) aren't files.
fn walk(dir: &Path) -> Result<Vec<(String, u64)>> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_dir() {
                go(root, &e.path(), out)?;
            } else if !(ty.is_char_device() && is_whiteout(&e.metadata()?)) {
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

// -- deletions ------------------------------------------------------------------------------

/// The xattr that makes a folder opaque in `userxattr` mode.
const OPAQUE: &CStr = c"user.overlay.opaque";

/// An overlayfs whiteout: a character device 0:0 (as `lstat` sees it).
fn is_whiteout(m: &std::fs::Metadata) -> bool {
    m.file_type().is_char_device() && m.rdev() == 0
}

fn cpath(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains a NUL byte")
}

fn make_whiteout(p: &Path) -> Result<()> {
    let c = cpath(p)?;
    // SAFETY: a plain syscall on an owned C string.
    if unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o600, 0) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("creating the whiteout {} (unprivileged whiteouts need Linux 5.8+)", p.display()));
    }
    Ok(())
}

fn is_opaque(p: &Path) -> bool {
    let Ok(c) = cpath(p) else { return false };
    let mut buf = [0u8; 2];
    // SAFETY: the buffer outlives the call and its length is passed.
    let n = unsafe { libc::lgetxattr(c.as_ptr(), OPAQUE.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    n == 1 && buf[0] == b'y'
}

fn set_opaque(p: &Path, on: bool) -> Result<()> {
    let c = cpath(p)?;
    // SAFETY: plain syscalls on owned C strings.
    let r = unsafe {
        if on {
            libc::lsetxattr(c.as_ptr(), OPAQUE.as_ptr(), c"y".as_ptr().cast(), 1, 0)
        } else {
            libc::lremovexattr(c.as_ptr(), OPAQUE.as_ptr())
        }
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        if !on && e.raw_os_error() == Some(libc::ENODATA) {
            return Ok(());
        }
        return Err(e).with_context(|| format!("{} user.overlay.opaque on {}", if on { "setting" } else { "removing" }, p.display()));
    }
    Ok(())
}

/// Whether `dir` holds any file (whiteouts and empty folders don't count).
fn has_files(dir: &Path) -> Result<bool> {
    Ok(!walk(dir)?.is_empty())
}

/// Deletions already in a layer folder (e.g. a copied mod that ships whiteouts): its
/// whiteouts and opaque folders, as relative paths. Whiteouts inside an opaque folder
/// change nothing, so they are dropped.
fn scan_deletions(dir: &Path) -> Result<Vec<String>> {
    fn go(root: &Path, dir: &Path, in_opaque: bool, out: &mut Vec<String>) -> Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let (path, m) = (e.path(), e.metadata()?);
            let rel = || path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if m.is_dir() {
                let opaque = !in_opaque && is_opaque(&path);
                if opaque {
                    out.push(rel());
                }
                go(root, &path, in_opaque || opaque, out)?;
            } else if is_whiteout(&m) {
                if in_opaque {
                    std::fs::remove_file(&path)?;
                } else {
                    out.push(rel());
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(dir, dir, false, &mut out)?;
    out.sort_by_key(|d| d.to_lowercase());
    Ok(out)
}

/// A path to delete, as given (relative to the install folder; `\` works as a separator;
/// an absolute path must be inside `install`), as a clean relative path with `/`.
/// Refuses `..` and anything outside the install folder.
pub fn normalize_path(path: &str, install: Option<&Path>) -> Result<String> {
    if path.contains('\0') {
        bail!("{path:?}: contains a NUL byte");
    }
    let mut rel = path.replace('\\', "/");
    if rel.starts_with('/') {
        let p = Path::new(&rel);
        let roots: Vec<PathBuf> = install.into_iter().flat_map(|i| [Some(i.to_owned()), i.canonicalize().ok()]).flatten().collect();
        let Some(inside) = roots.iter().find_map(|r| p.strip_prefix(r).ok()) else {
            bail!("{path}: outside the install folder (give paths relative to it)");
        };
        rel = inside.to_string_lossy().into_owned();
    }
    let mut parts = Vec::new();
    for c in rel.split('/') {
        match c {
            "" | "." => {}
            ".." => bail!("{path}: '..' isn't allowed; paths stay inside the install folder"),
            c => parts.push(c),
        }
    }
    if parts.is_empty() {
        bail!("{path:?}: that's the install folder itself; name a file or folder inside it");
    }
    Ok(parts.join("/"))
}

/// `rel` with each component spelled as in `install` (Windows games don't care about case;
/// overlayfs does, and a whiteout only hides an exact name), and whether the whole path
/// exists there. Components past the first missing one stay as given.
pub fn match_install_case(install: &Path, rel: &str) -> (String, bool) {
    let mut cur = install.to_owned();
    let mut out: Vec<String> = Vec::new();
    let mut found = true;
    for comp in rel.split('/') {
        if found {
            if cur.join(comp).symlink_metadata().is_ok() {
                cur.push(comp);
                out.push(comp.to_owned());
                continue;
            }
            let lower = comp.to_lowercase();
            let mut hits: Vec<String> = std::fs::read_dir(&cur)
                .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.to_lowercase() == lower).collect())
                .unwrap_or_default();
            hits.sort();
            if let Some(name) = hits.into_iter().next() {
                cur.push(&name);
                out.push(name);
                continue;
            }
            found = false;
        }
        out.push(comp.to_owned());
    }
    (out.join("/"), found)
}

fn same_path(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Whether `path` is strictly inside the folder `dir` (ignoring case).
fn is_under(path: &str, dir: &str) -> bool {
    path.to_lowercase().strip_prefix(&dir.to_lowercase()).is_some_and(|rest| rest.starts_with('/'))
}

/// A layer's deletions after deleting `add` and keeping `keep` (normalized paths), sorted,
/// with notes for deletions folded into a new folder deletion.
fn plan_deletions(current: &[String], add: &[String], keep: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut list = current.to_vec();
    let mut notes = Vec::new();
    for k in keep {
        if let Some(i) = list.iter().position(|d| same_path(d, k)) {
            list.remove(i);
        } else if let Some(d) = list.iter().find(|d| is_under(k, d)) {
            bail!("{k} is hidden because the layer deletes the folder {d}; keep {d} instead");
        } else {
            bail!("the layer doesn't delete {k}");
        }
    }
    for a in add {
        if list.iter().any(|d| same_path(d, a)) {
            continue;
        }
        if let Some(d) = list.iter().find(|d| is_under(a, d)) {
            bail!("{a} is already deleted with the folder {d}");
        }
        let before = list.len();
        list.retain(|d| !is_under(d, a));
        if list.len() < before {
            notes.push(format!("the deletion of {a} takes in {} deletion(s) inside it", before - list.len()));
        }
        list.push(a.clone());
    }
    list.sort_by_key(|d| d.to_lowercase());
    Ok((list, notes))
}

/// Make the layer folder hide `rel`: a whiteout, or an opaque folder if the layer has files
/// of its own in that folder (they then replace the install's whole folder).
fn apply_deletion(dir: &Path, rel: &str) -> Result<()> {
    let p = dir.join(rel);
    match p.symlink_metadata() {
        Ok(m) if is_whiteout(&m) => Ok(()),
        Ok(m) if m.is_dir() => {
            if has_files(&p)? {
                set_opaque(&p, true)
            } else {
                std::fs::remove_dir_all(&p)?;
                make_whiteout(&p)
            }
        }
        Ok(_) => bail!("the layer has its own {rel}, which already replaces the install's; it can't also delete it"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = p.parent().context("deletion path")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("{rel}: the layer has a file where a folder of this path would be"))?;
            make_whiteout(&p)
        }
        Err(e) => Err(e.into()),
    }
}

/// Undo [`apply_deletion`]. Folders created for a whiteout stay (empty folders change
/// nothing in the merged view of a folder the install has).
fn undo_deletion(dir: &Path, rel: &str) -> Result<()> {
    let p = dir.join(rel);
    match p.symlink_metadata() {
        Ok(m) if is_whiteout(&m) => std::fs::remove_file(&p)?,
        Ok(m) if m.is_dir() => set_opaque(&p, false)?,
        _ => {}
    }
    Ok(())
}

/// Bring the whiteouts in `dir` from `old` to `new`.
fn sync_deletions(dir: &Path, old: &[String], new: &[String]) -> Result<()> {
    for d in old.iter().filter(|d| !new.contains(d)) {
        undo_deletion(dir, d)?;
    }
    for d in new.iter().filter(|d| !old.contains(d)) {
        apply_deletion(dir, d)?;
    }
    Ok(())
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

/// The mods a launch applies (bottom to top) and what was found about them.
pub(crate) struct Stack {
    /// The versions applied, each named after its mod ("DLSS v3").
    pub(crate) mods: Vec<Layer>,
    /// The same, as stack entries resolved (`mods[i]` is version `resolved[i]`).
    pub(crate) resolved: Vec<ResolvedMod>,
    pub(crate) problems: Vec<String>,
    pub(crate) warnings: Vec<String>,
    setting_conflicts: Vec<SettingConflict>,
    settings: GameSettings,
    file_conflicts: Vec<Conflict>,
}

/// Config keys that mods in a stack set differently (or one sets and another deletes), by
/// mod id.
fn patch_conflicts(mods: &[Layer], resolved: &[ResolvedMod]) -> Vec<SettingConflict> {
    let mut seen: BTreeMap<String, (String, Vec<ValueFrom>)> = BTreeMap::new();
    for (m, r) in mods.iter().zip(resolved) {
        for p in m.patches() {
            let ops = p.set.iter().map(|(k, v)| (k, Some(v.clone()))).chain(p.delete.iter().map(|k| (k, None)));
            for (k, value) in ops {
                let (label, norm) = match p.format {
                    quiver_api::mods::PatchFormat::Ini => {
                        (format!("{} [{}] {k}", p.file, p.section), format!("ini|{}|{}|{}", p.file, p.section.to_lowercase(), k.to_lowercase()))
                    }
                    quiver_api::mods::PatchFormat::Xml => (format!("{} {}@{k}", p.file, p.element), format!("xml|{}|{}|{k}", p.file, p.element)),
                };
                seen.entry(norm).or_insert_with(|| (label, Vec::new())).1.push(ValueFrom { from: r.mod_id.clone(), value });
            }
        }
    }
    seen.into_values()
        .filter(|(_, by)| by.iter().any(|b| b.value != by[0].value))
        .map(|(key, by)| SettingConflict { key, by })
        .collect()
}

/// A version as a stack shows it: named after its mod ("DLSS v3"), on or off with it.
pub(crate) fn named(l: &Layer, rec: &ModRecord) -> Layer {
    let mut l = l.clone();
    l.name = rec.display(&l.id);
    l.disabled = rec.disabled;
    l
}

/// A short stable name for a path (FNV-1a), for overlayfs work folders.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

impl ModStore {
    /// The store of `game` (a Ubisoft product id, or any [`quiver_api::GameRef`]).
    pub fn new(data: &Path, game: impl Into<quiver_api::GameRef>) -> Self {
        Self { data: data.to_owned(), key: game.into(), game: None, fixes_on_vanilla: None }
    }

    /// The store under Quiver's data directory.
    pub fn open(game: impl Into<quiver_api::GameRef>) -> Self {
        Self::new(&crate::launcher::data_dir(), game)
    }

    /// With the installed game (its prefix and install folder) and the app-level
    /// `fixes_on_vanilla` setting. The store is then the game's ([`Game::game_ref`]).
    pub fn with_game(mut self, game: &Game, fixes_on_vanilla: Option<bool>) -> Self {
        self.key = game.game_ref();
        self.game = Some(game.clone());
        self.fixes_on_vanilla = fixes_on_vanilla;
        self
    }

    fn record_path(&self) -> PathBuf {
        self.mods_dir().join("patches.json")
    }

    /// The config patch record (`patches.json`).
    pub(crate) fn patches_record(&self) -> PathBuf {
        self.record_path()
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.data
    }

    /// The Ubisoft product (0 for another store's game: see [`Self::game_ref`]).
    pub fn product_id(&self) -> u32 {
        self.key.as_ubisoft().unwrap_or(0)
    }

    /// The game the store is for.
    pub fn game_ref(&self) -> quiver_api::GameRef {
        self.key
    }

    /// Whether saves follow Quiver's save stores (Ubisoft games). Other stores' games use
    /// their real save folders, which their client syncs (Steam Cloud): profiles of theirs
    /// share them (design/STEAM-COPIES.md §4, "Saves").
    pub fn has_save_stores(&self) -> bool {
        self.key.is_ubisoft()
    }

    /// `<data>/games/<id>`: settings history, profile and layer histories.
    fn game_dir(&self) -> PathBuf {
        crate::paths::game_dir_in(&self.data, &self.key)
    }

    /// The game's prefix, when the store knows the game ([`Self::with_game`]).
    pub fn game_prefix(&self) -> Option<&Path> {
        self.prefix()
    }

    /// Where a profile's own copies of prefix folders live (laid out like the prefix).
    pub fn profile_prefix(&self, id: &str) -> PathBuf {
        self.profile_dir(id).join("prefix")
    }

    /// The game's current settings (revision 0: defaults).
    fn game_settings(&self) -> GameSettings {
        let path = self.game_dir().join("history.jsonl");
        read_jsonl::<quiver_api::Revision>(&path).ok().and_then(|h| h.last().map(|r| r.settings.clone())).unwrap_or_default()
    }

    fn mods_dir(&self) -> PathBuf {
        crate::paths::mods_dir_in(&self.data, &self.key)
    }

    pub(crate) fn layers_dir(&self) -> PathBuf {
        self.mods_dir().join("layers")
    }

    pub fn layer_dir(&self, id: &str) -> PathBuf {
        self.layers_dir().join(id)
    }

    pub(crate) fn layer_meta(&self, id: &str) -> PathBuf {
        self.layers_dir().join(format!("{id}.json"))
    }

    /// The profile's writable layer and overlayfs work dir live here.
    pub fn profile_dir(&self, id: &str) -> PathBuf {
        self.mods_dir().join("profiles").join(id)
    }

    fn histories_dir(&self) -> PathBuf {
        self.game_dir().join("profiles")
    }

    fn history_path(&self, id: &str) -> PathBuf {
        self.histories_dir().join(format!("{id}.jsonl"))
    }

    pub(crate) fn layer_history_path(&self, id: &str) -> PathBuf {
        self.game_dir().join("layers").join(format!("{id}.jsonl"))
    }

    pub(crate) fn write_layer_meta(&self, layer: &Layer) -> Result<()> {
        let path = self.layer_meta(&layer.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(layer)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
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

    /// A layer (a mod's version) by id, or a mod by id, name or manifest id (its current
    /// version), or `MOD@VERSION` (a version by id or label).
    pub fn layer(&self, query: &str) -> Result<Layer> {
        let layers = self.layers()?;
        if let Some(l) = layers.iter().find(|l| l.id == query.trim()) {
            return Ok(l.clone());
        }
        let mods = mods_of(&layers, &self.records()?);
        let (rec, pinned) = self.find_ref(&mods, &layers, query)?;
        let id = pinned.unwrap_or(rec.current);
        layers.into_iter().find(|l| l.id == id).with_context(|| format!("version {id} of '{}' has lost its record", rec.name))
    }

    /// What `query` names: a mod and, if it names one version (`MOD@VERSION`, a version id,
    /// or the name a version had as a mod of its own), that version's id. Mods match by id,
    /// then name, then manifest id.
    pub(crate) fn find_ref(&self, mods: &[ModRecord], layers: &[Layer], query: &str) -> Result<(ModRecord, Option<String>)> {
        let (q, v) = split_entry(query);
        let pick = |found: Vec<&ModRecord>| -> Result<Option<ModRecord>> {
            match found.as_slice() {
                [] => Ok(None),
                [r] => Ok(Some((*r).clone())),
                _ => bail!("'{q}' names more than one mod; use its id"),
            }
        };
        let layer_of = |id: &str| layers.iter().find(|l| l.id == id);
        let mut by_version = None;
        let rec = if let Some(r) = mods.iter().find(|r| r.id == q) {
            r.clone()
        } else if let Some(r) = mods.iter().find(|r| r.has(q)) {
            by_version = Some(q.to_owned());
            r.clone()
        } else if let Some(r) = pick(mods.iter().filter(|r| same_name(&r.name, q)).collect())? {
            r
        } else if let Some(r) = pick(
            mods.iter().filter(|r| r.versions.iter().any(|x| layer_of(&x.id).and_then(|l| l.mod_id()).is_some_and(|m| same_name(m, q)))).collect(),
        )? {
            r
        } else {
            // A version's own name from before it was merged ("DLSS-v2").
            let hits: Vec<(&ModRecord, &str)> = mods
                .iter()
                .flat_map(|r| r.versions.iter().filter(|x| layer_of(&x.id).is_some_and(|l| same_name(&l.name, q))).map(move |x| (r, x.id.as_str())))
                .collect();
            match hits.as_slice() {
                [(r, vid)] => {
                    by_version = Some((*vid).to_owned());
                    (*r).clone()
                }
                [] => bail!(
                    "no mod '{q}' (mods: {})",
                    if mods.is_empty() { "none".into() } else { mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ") }
                ),
                _ => bail!("'{q}' names more than one mod; use its id"),
            }
        };
        let pinned = match v {
            None => by_version,
            Some(v) if v.eq_ignore_ascii_case("current") => None,
            Some(v) => {
                if by_version.is_some() {
                    bail!("{query}: '{q}' is already a version; name the mod and the version (MOD@VERSION)");
                }
                let labels: Vec<&str> = rec.versions.iter().map(|x| x.label.as_str()).collect();
                Some(rec.version(v).with_context(|| format!("'{}' has no version '{v}' (versions: {})", rec.name, labels.join(", ")))?.id.clone())
            }
        };
        Ok((rec, pinned))
    }

    /// A mod by id, name or manifest id (or one of its versions' ids).
    pub fn find_mod(&self, query: &str) -> Result<ModRecord> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        Ok(self.find_ref(&mods, &layers, split_entry(query).0)?.0)
    }

    /// Every mod (one-version mods included), ordered by when each was first added.
    pub fn mods(&self) -> Result<Vec<ModRecord>> {
        Ok(mods_of(&self.layers()?, &self.records()?))
    }

    /// Copy `src` into the store as a new layer (reflinks where the filesystem supports
    /// them), with names matched to `install`'s case.
    pub fn add_layer(&self, name: &str, src: &Path, install: Option<&Path>, source: &str) -> Result<Layer> {
        Ok(self.add_layer_with(name, Some(src), &[], install, source)?.layer)
    }

    /// Paths to delete as given, normalized and matched to `install`'s case, with a
    /// warning for each one the install doesn't have.
    fn resolve_deletions(&self, paths: &[String], install: Option<&Path>) -> Result<(Vec<String>, Vec<String>)> {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        for p in paths {
            let rel = normalize_path(p, install)?;
            let Some(install) = install else {
                out.push(rel);
                continue;
            };
            let (rel, exists) = match_install_case(install, &rel);
            if !exists {
                warnings.push(format!(
                    "{rel} is not in the install folder; the deletion is kept and hides it if a layer below or a game update adds it"
                ));
            }
            out.push(rel);
        }
        Ok((out, warnings))
    }

    /// A new layer: `src`'s files (if any) copied into the store, with names matched to
    /// `install`'s case, plus `deletes` (paths relative to the install folder). Whiteouts
    /// and opaque folders already in `src` are kept as deletions. Revision 1 of the layer's
    /// history records its deletions.
    pub fn add_layer_with(
        &self,
        name: &str,
        src: Option<&Path>,
        deletes: &[String],
        install: Option<&Path>,
        source: &str,
    ) -> Result<LayerChange> {
        self.add_mod(name, src, deletes, install, source, None, false, None)
    }

    /// [`Self::add_layer_with`] with a manifest (Mods v2): a mod with a manifest may have no
    /// files at all (only launch settings or config patches).
    /// `version_of`: the new layer is a version of that mod (named after it, id `v…`), not a
    /// mod of its own.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_mod(
        &self,
        name: &str,
        src: Option<&Path>,
        deletes: &[String],
        install: Option<&Path>,
        source: &str,
        manifest: Option<ModManifest>,
        builtin: bool,
        version_of: Option<&str>,
    ) -> Result<LayerChange> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a layer needs a name");
        }
        if version_of.is_none() && self.mods()?.iter().any(|m| same_name(&m.name, name)) {
            bail!("there is already a mod named '{name}' (a new version of it: quiver mods <game> version add '{name}' DIR)");
        }
        let src = match src {
            Some(src) => {
                let src = src.canonicalize().with_context(|| format!("{} not found", src.display()))?;
                if !src.is_dir() {
                    bail!("{} is not a folder", src.display());
                }
                if src.starts_with(self.data.join("mods")) {
                    bail!("{} is already in the mod store", src.display());
                }
                Some(src)
            }
            None => None,
        };
        let (deletes, mut warnings) = self.resolve_deletions(deletes, install)?;
        if src.is_none() && deletes.is_empty() && manifest.is_none() {
            bail!("a layer needs a folder of files, files to delete, or both");
        }
        let prefix = match (version_of, &manifest) {
            (Some(_), _) => 'v',
            (None, Some(_)) => 'm',
            (None, None) => 'l',
        };
        let id = loop {
            let id = new_id(prefix)?;
            if !self.layer_dir(&id).exists() {
                break id;
            }
        };
        std::fs::create_dir_all(self.layers_dir())?;
        let tmp = self.layers_dir().join(format!(".partial-{id}"));
        let copied = (|| -> Result<(Vec<String>, Vec<String>)> {
            std::fs::create_dir(&tmp)?;
            if let Some(src) = &src {
                // -a keeps device nodes, so whiteouts in the source are copied as whiteouts
                // (and xattrs, so opaque folders stay opaque).
                let out = std::process::Command::new("cp")
                    .args(["-a", "--reflink=auto", "--no-preserve=ownership"])
                    .arg(src.join("."))
                    .arg(&tmp)
                    .output()
                    .context("running cp")?;
                if !out.status.success() {
                    bail!("copying {} failed: {}", src.display(), String::from_utf8_lossy(&out.stderr).trim());
                }
            }
            // Options: a plain folder's `quiver-options.toml` is the version's schema, not a
            // file laid over the install (as whiteouts are deletions).
            if let Some(schema) = crate::options::schema_in(&tmp) {
                crate::options::adopt_schema(self, &id, &schema)?;
                std::fs::remove_file(&schema)?;
            }
            if let Some(install) = install {
                match_case(&tmp, install)?;
            }
            let shipped = scan_deletions(&tmp)?;
            let (list, notes) = plan_deletions(&shipped, &deletes, &[])?;
            sync_deletions(&tmp, &shipped, &list)?;
            Ok((list, notes))
        })();
        let deletes = match copied {
            Ok((list, notes)) => {
                warnings.extend(notes);
                list
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                let _ = std::fs::remove_file(crate::options::version_schema_path(self, &id));
                return Err(e);
            }
        };
        let files = walk(&tmp)?;
        std::fs::rename(&tmp, self.layer_dir(&id))?;
        let layer = Layer {
            id: id.clone(),
            name: name.to_owned(),
            added: chrono::Utc::now(),
            added_by: source.to_owned(),
            origin: src,
            files: files.len() as u64,
            bytes: files.iter().map(|f| f.1).sum(),
            deletes,
            manifest,
            builtin,
            disabled: false,
        };
        self.write_layer_meta(&layer)?;
        let _ = std::fs::remove_file(self.layer_history_path(&id));
        let rev = self.append_layer_revision(&layer.id, &layer.deletes, layer.added, source, Some("added".into()), Vec::new())?;
        Ok(LayerChange { layer, revision: rev.revision, warnings })
    }

    pub fn layer_history(&self, id: &str) -> Result<Vec<LayerRevision>> {
        read_jsonl(&self.layer_history_path(id))
    }

    /// A layer's history by id or name.
    pub fn layer_history_of(&self, query: &str) -> Result<(Layer, Vec<LayerRevision>)> {
        let layer = self.layer(query)?;
        let hist = self.layer_history(&layer.id)?;
        Ok((layer, hist))
    }

    fn append_layer_revision(
        &self,
        id: &str,
        deletes: &[String],
        at: chrono::DateTime<chrono::Utc>,
        source: &str,
        message: Option<String>,
        changes: Vec<String>,
    ) -> Result<LayerRevision> {
        let path = self.layer_history_path(id);
        let revision = read_jsonl::<LayerRevision>(&path)?.last().map_or(0, |r| r.revision) + 1;
        let rev = LayerRevision { revision, at, source: source.to_owned(), message, deletes: deletes.to_vec(), changes };
        append_jsonl(&path, &rev)?;
        Ok(rev)
    }

    /// Give `layer` the deletions `list`: whiteouts first (undone again if one fails), then
    /// its metadata, then a revision. No change, no revision.
    fn set_deletions(
        &self,
        layer: Layer,
        list: Vec<String>,
        source: &str,
        message: Option<String>,
        warnings: Vec<String>,
    ) -> Result<LayerChange> {
        let updated = Layer { deletes: list, ..layer.clone() };
        self.commit(layer, updated, Vec::new(), source, message, warnings)
    }

    /// Make `old` (as stored) into `new`: whiteouts first (undone again if one fails), then
    /// the metadata, then a revision recording the deletions and `changes`. No change, no
    /// revision.
    fn commit(
        &self,
        old: Layer,
        new: Layer,
        changes: Vec<String>,
        source: &str,
        message: Option<String>,
        warnings: Vec<String>,
    ) -> Result<LayerChange> {
        let hist = self.layer_history(&old.id)?;
        if new == old {
            return Ok(LayerChange { revision: hist.last().map_or(0, |r| r.revision), layer: old, warnings });
        }
        let dir = self.layer_dir(&old.id);
        if new.deletes != old.deletes {
            if !dir.is_dir() {
                bail!("layer '{}' has lost its folder ({}); remove it and add it again", old.name, dir.display());
            }
            if let Err(e) = sync_deletions(&dir, &old.deletes, &new.deletes) {
                let _ = sync_deletions(&dir, &new.deletes, &old.deletes);
                return Err(e);
            }
        }
        if hist.is_empty() {
            // A layer from before deletions had a history: record how it was added first.
            self.append_layer_revision(&old.id, &old.deletes, old.added, &old.added_by, Some("added".into()), Vec::new())?;
        }
        self.write_layer_meta(&new)?;
        let rev = self.append_layer_revision(&new.id, &new.deletes, chrono::Utc::now(), source, message, changes)?;
        Ok(LayerChange { layer: new, revision: rev.revision, warnings })
    }

    /// Add deletions (`delete`) to a layer or take them back (`keep`); paths are relative
    /// to the install folder. Deleting a folder takes in deletions inside it; a path the
    /// install doesn't have is kept, with a warning.
    pub fn change_deletions(
        &self,
        query: &str,
        delete: &[String],
        keep: &[String],
        install: Option<&Path>,
        source: &str,
        message: Option<String>,
    ) -> Result<LayerChange> {
        let layer = self.layer(query)?;
        let (add, mut warnings) = self.resolve_deletions(delete, install)?;
        let keep: Vec<String> = keep.iter().map(|k| normalize_path(k, install)).collect::<Result<_>>()?;
        let (list, notes) = plan_deletions(&layer.deletes, &add, &keep)?;
        warnings.extend(notes);
        self.set_deletions(layer, list, source, message, warnings)
    }

    /// Make an earlier revision of a layer's deletions current again (as a new revision).
    pub fn restore_layer(&self, query: &str, revision: u32, source: &str) -> Result<LayerChange> {
        let layer = self.layer(query)?;
        let target = self
            .layer_history(&layer.id)?
            .into_iter()
            .find(|r| r.revision == revision)
            .with_context(|| format!("layer '{}' has no revision {revision}", layer.name))?;
        self.set_deletions(layer, target.deletes, source, Some(format!("restore revision {revision}")), Vec::new())
    }

    /// Delete a mod and every version's files. Refused while a profile uses it.
    pub fn remove_layer(&self, query: &str) -> Result<Layer> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let (rec, _) = self.find_ref(&mods, &layers, split_entry(query).0)?;
        let users: Vec<String> = self.users_of(&mods, &rec.id)?.into_iter().map(|(p, _)| p.name).collect();
        if !users.is_empty() {
            bail!("mod '{}' is used by {}; take it out of those profiles first", rec.name, users.join(", "));
        }
        let mut shown = layers.iter().find(|l| l.id == rec.current).cloned().unwrap_or_default();
        shown.name = rec.name.clone();
        for v in &rec.versions {
            let _ = std::fs::remove_file(self.layer_meta(&v.id));
            let dir = self.layer_dir(&v.id);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            let _ = std::fs::remove_file(self.layer_history_path(&v.id));
        }
        if self.versioned(&rec.id) {
            self.append_mod_revision(&rec, true, "store", Some("removed (with every version's files)".into()), Vec::new())?;
        }
        Ok(shown)
    }

    /// Live profiles whose stack has the mod `mod_id` (any version), and how.
    pub(crate) fn users_of(&self, mods: &[ModRecord], mod_id: &str) -> Result<Vec<(Profile, ResolvedMod)>> {
        Ok(self
            .profiles()?
            .into_iter()
            .filter_map(|(p, _)| {
                let r = p.layers.iter().filter_map(|e| resolve_entry(mods, e)).find(|r| r.mod_id == mod_id)?;
                Some((p, r))
            })
            .collect())
    }

    /// Stack entries as given (mod ids or names, `MOD@VERSION`, version ids) to stored ones,
    /// keeping order: `MOD` to track the current version, `MOD@VERSION-ID` to pin one.
    fn resolve_layers(&self, queries: &[String]) -> Result<Vec<String>> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let mut out: Vec<(String, String)> = Vec::new();
        for q in queries {
            let (rec, pinned) = self.find_ref(&mods, &layers, q)?;
            if out.iter().any(|(m, _)| *m == rec.id) {
                bail!("mod '{}' is listed twice ({q})", rec.name);
            }
            out.push((rec.id.clone(), stack_entry(&rec.id, pinned.as_deref())));
        }
        Ok(out.into_iter().map(|(_, e)| e).collect())
    }
    /// Files that more than one of `layers` (bottom to top) provides, or that a layer
    /// provides and a higher one deletes (the path or a folder above it), compared without
    /// case like the game does.
    pub fn conflicts(&self, layers: &[String]) -> Vec<Conflict> {
        let metas = self.layers().unwrap_or_default();
        // lowercase path -> (path, [(layer, deletes it)])
        let mut seen: BTreeMap<String, (String, Vec<(String, bool)>)> = BTreeMap::new();
        for id in layers {
            let deletes = metas.iter().find(|l| &l.id == id).map(|l| l.deletes.as_slice()).unwrap_or_default();
            for d in deletes {
                for (key, (_, by)) in seen.iter_mut() {
                    if (same_path(key, d) || is_under(key, d)) && by.last().is_some_and(|(l, _)| l != id) {
                        by.push((id.clone(), true));
                    }
                }
            }
            // A file of the layer's own under a folder it deletes (an opaque folder) is
            // what it provides there.
            for (path, _) in walk(&self.layer_dir(id)).unwrap_or_default() {
                let e = seen.entry(path.to_lowercase()).or_insert_with(|| (path.clone(), Vec::new()));
                match e.1.last_mut() {
                    Some((l, deleted)) if l == id => *deleted = false,
                    _ => e.1.push((id.clone(), false)),
                }
            }
        }
        seen.into_values()
            .filter(|(_, by)| by.len() > 1)
            .map(|(path, by)| Conflict {
                path,
                deleted_by: by.iter().filter(|b| b.1).map(|b| b.0.clone()).collect(),
                layers: by.into_iter().map(|b| b.0).collect(),
            })
            .collect()
    }

    // -- Mods v2: packages ---------------------------------------------------------------

    pub(crate) fn install_dir(&self) -> Option<&Path> {
        self.game.as_ref().map(|g| g.install_dir.as_path())
    }

    /// The game's prefix: none for a native game (Steam's Linux builds), and none until the
    /// store knows the game.
    fn prefix(&self) -> Option<&Path> {
        self.game.as_ref().and_then(|g| g.prefix())
    }

    /// The catalog's mods for this game, with whether each is installed (and whether the
    /// installed mod's current version has a different manifest).
    pub fn catalog(&self) -> Result<Vec<CatalogEntry>> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        Ok(crate::modpkg::catalog(&self.key)?
            .into_iter()
            .map(|manifest| {
                let have = self.builtin_mod(&mods, &layers, &manifest.id);
                CatalogEntry {
                    installed: have.map(|r| r.id.clone()),
                    update: have.is_some_and(|r| layers.iter().find(|l| l.id == r.current).and_then(|l| l.manifest.as_ref()) != Some(&manifest)),
                    manifest,
                }
            })
            .collect())
    }

    /// The installed mod a built-in manifest id belongs to.
    pub(crate) fn builtin_mod<'a>(&self, mods: &'a [ModRecord], layers: &[Layer], id: &str) -> Option<&'a ModRecord> {
        mods.iter().find(|r| {
            r.versions.iter().any(|v| layers.iter().any(|l| l.id == v.id && l.builtin && l.mod_id().is_some_and(|m| m.eq_ignore_ascii_case(id))))
        })
    }

    /// Install a mod package (see [`Package`]). A built-in mod that's already installed gets
    /// the catalog's manifest as a new current version (or, if one of its versions has it,
    /// that one becomes current). A package whose manifest id is already installed becomes a
    /// new version of that mod.
    pub fn install(&self, pkg: &Package, source: &str) -> Result<LayerChange> {
        let install = self.install_dir();
        let (manifest, files, builtin, name) = match pkg {
            Package::Builtin(id) => (crate::modpkg::builtin(&self.key, id)?, None, true, None),
            Package::Dir { path, name } => {
                let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
                match crate::modpkg::read_package(&path)? {
                    Some((m, files)) => (m, files, false, name.clone()),
                    None => {
                        // A plain folder of files: a file-only mod, as layers always were.
                        let name = name.clone().unwrap_or_else(|| path.file_name().unwrap_or_default().to_string_lossy().into_owned());
                        return self.add_mod(&name, Some(&path), &[], install, source, None, false, None);
                    }
                }
            }
        };
        crate::modpkg::check_game(&manifest, &self.key)?;
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let installed = mods.iter().find(|r| {
            r.versions.iter().any(|v| layers.iter().any(|l| l.id == v.id && l.mod_id().is_some_and(|m| m.eq_ignore_ascii_case(&manifest.id))))
        });
        if let Some(rec) = installed {
            let was_builtin = self.builtin_mod(&mods, &layers, &manifest.id).is_some();
            if builtin != was_builtin {
                bail!("{} is already installed (as {}) from {}; uninstall it first", manifest.id, rec.id, if was_builtin { "the catalog" } else { "a folder" });
            }
            if builtin {
                return self.update_builtin(rec, &layers, manifest, source);
            }
            let Package::Dir { path, .. } = pkg else { unreachable!() };
            let same = layers.iter().filter(|l| rec.has(&l.id)).find(|l| l.manifest.as_ref() == Some(&manifest));
            if let Some(l) = same {
                bail!(
                    "{} is already installed (as {}, version {}) with this manifest; to add the folder as a new version anyway: quiver mods <game> version add {} DIR",
                    manifest.id,
                    rec.id,
                    rec.label_of(&l.id),
                    rec.id
                );
            }
            let opts = crate::modversions::NewVersion { path: Some(path.clone()), ..Default::default() };
            let vc = self.add_version(&rec.id, &opts, source)?;
            let revision = self.layer_history(&vc.version.id)?.last().map_or(0, |r| r.revision);
            let mut warnings = vec![format!("{} was installed already: this is its new version {}", rec.name, vc.record.label_of(&vc.version.id))];
            warnings.extend(vc.warnings);
            return Ok(LayerChange { layer: vc.version, revision, warnings });
        }
        let name = name.unwrap_or_else(|| manifest.name.clone());
        let taken = mods.iter().any(|m| same_name(&m.name, &name));
        let name = if taken { format!("{name} ({})", manifest.id) } else { name };
        let deletes = manifest.files.delete.clone();
        // Options: a package's schema sits next to its manifest; checked before installing.
        let schema = match pkg {
            Package::Dir { path, .. } => crate::options::schema_in(&path.canonicalize()?),
            Package::Builtin(_) => None,
        };
        if let Some(f) = &schema {
            let text = std::fs::read_to_string(f)?;
            let s = crate::options::schema::parse(&text, f.extension().is_some_and(|e| e == "json")).with_context(|| f.display().to_string())?;
            crate::options::schema::check(&s).with_context(|| f.display().to_string())?;
        }
        let mut change = self.add_mod(&name, files.as_deref(), &deletes, install, source, Some(manifest), builtin, None)?;
        if let Some(f) = &schema {
            change.warnings.extend(crate::options::adopt_schema(self, &change.layer.id, f)?);
        }
        Ok(change)
    }

    /// The real file behind a recorded config key.
    fn physical(&self, scope: &str, rel: &str) -> Option<PathBuf> {
        match scope.strip_prefix("profile:") {
            Some(id) => Some(self.profile_prefix(id).join(rel)),
            None => self.prefix().map(|p| p.join(rel)),
        }
    }

    /// The ids config patches may be recorded under for a mod: its own (launches record
    /// the mod's id) and its versions' (what was recorded before they were versions).
    fn patch_ids(rec: &ModRecord) -> Vec<String> {
        let mut ids = vec![rec.id.clone()];
        ids.extend(rec.versions.iter().map(|v| v.id.clone()).filter(|v| *v != rec.id));
        ids
    }

    /// Put back the files' own values for config keys Quiver changed: for one mod (by id or
    /// name, any version), or all.
    pub fn undo_patches(&self, query: Option<&str>) -> Result<Vec<PatchStatus>> {
        let physical = |scope: &str, rel: &str| self.physical(scope, rel);
        let Some(q) = query else { return confpatch::undo(&self.record_path(), None, physical) };
        let rec = self.find_mod(q)?;
        let mut out = Vec::new();
        for id in Self::patch_ids(&rec) {
            out.extend(confpatch::undo(&self.record_path(), Some(&id), physical)?);
        }
        Ok(out)
    }

    /// Config keys a mod (by id or name, any version) has changed right now.
    pub fn applied_by(&self, query: &str) -> Result<Vec<AppliedKey>> {
        let ids = Self::patch_ids(&self.find_mod(query)?);
        Ok(confpatch::applied(&self.record_path())?.into_iter().filter(|k| k.mods.iter().any(|m| ids.contains(m))).collect())
    }

    /// Remove an installed mod (every version): refused while a profile stacks it; the
    /// config values it changed go back to the files' own first.
    pub fn uninstall(&self, query: &str) -> Result<(Layer, Vec<PatchStatus>)> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let (rec, _) = self.find_ref(&mods, &layers, split_entry(query).0)?;
        let users: Vec<String> = self.users_of(&mods, &rec.id)?.into_iter().map(|(p, _)| p.name).collect();
        if !users.is_empty() {
            bail!("mod '{}' is used by {}; take it out of those profiles first", rec.name, users.join(", "));
        }
        let undone = self.undo_patches(Some(&rec.id))?;
        Ok((self.remove_layer(&rec.id)?, undone))
    }

    /// Change an installed mod. The mod: `enabled=on|off`, `name=…`. A version (the current
    /// one, or `MOD@VERSION`): `save_compatible=on|off|unknown`, `kind=fix|mod` (its
    /// manifest; a layer from before Mods v2 gets one of its own), `label=…`, `notes=…`.
    /// Recorded in the version's history (and the mod's, for a versioned mod).
    pub fn set_mod(&self, query: &str, ops: &[SettingOp], source: &str, message: Option<String>) -> Result<LayerChange> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let (rec, pinned) = self.find_ref(&mods, &layers, query)?;
        let vid = pinned.unwrap_or_else(|| rec.current.clone());
        let old = layers.iter().find(|l| l.id == vid).cloned().with_context(|| format!("version {vid} of '{}' has lost its record", rec.name))?;
        // Labels and notes live in the mod's record, so they make it a versioned mod.
        let explicit = self.versioned(&rec.id) || ops.iter().any(|o| matches!(o, SettingOp::Set { key, .. } | SettingOp::Unset { key } if key == "label" || key == "notes"));
        let mut new = old.clone();
        let mut new_rec = rec.clone();
        let (mut changes, mut rec_changes) = (Vec::new(), Vec::new());
        for op in ops {
            let (key, value) = match op {
                SettingOp::Set { key, value } => (key.as_str(), Some(value.trim())),
                SettingOp::Unset { key } => (key.as_str(), None),
            };
            let bool_of = |v: Option<&str>| match v.map(str::to_ascii_lowercase).as_deref() {
                Some("on" | "true" | "yes" | "1") => Ok(Some(true)),
                Some("off" | "false" | "no" | "0") => Ok(Some(false)),
                None | Some("unknown" | "auto") => Ok(None),
                Some(o) => Err(anyhow::anyhow!("{key}: expected on or off, got '{o}'")),
            };
            fn manifest(l: &mut Layer) -> &mut ModManifest {
                let (id, name) = (l.id.clone(), l.name.clone());
                l.manifest.get_or_insert_with(|| ModManifest { id, name, ..Default::default() })
            }
            let said = match value {
                Some(v) => format!("{key}={v}"),
                None => format!("{key}="),
            };
            let mut on_record = false;
            match key {
                "enabled" => {
                    let off = bool_of(value)? == Some(false);
                    if explicit {
                        new_rec.disabled = off;
                        on_record = true;
                    } else {
                        new.disabled = off;
                    }
                }
                "save_compatible" => manifest(&mut new).save_compatible = bool_of(value)?,
                "kind" => {
                    manifest(&mut new).kind = match value {
                        Some("fix") => ModKind::Fix,
                        Some("mod") | None => ModKind::Mod,
                        Some(o) => bail!("kind: fix or mod, not '{o}'"),
                    }
                }
                "name" => {
                    let name = value.filter(|v| !v.is_empty()).context("a mod needs a name")?;
                    if mods.iter().any(|m| m.id != rec.id && same_name(&m.name, name)) {
                        bail!("there is already a mod named '{name}'");
                    }
                    if explicit {
                        new_rec.name = name.to_owned();
                        on_record = true;
                    } else {
                        new.name = name.to_owned();
                    }
                }
                "label" => {
                    let label = value.filter(|v| !v.is_empty()).context("a version needs a label")?;
                    if new_rec.versions.iter().any(|v| v.id != vid && v.label.eq_ignore_ascii_case(label)) {
                        bail!("'{}' already has a version labelled '{label}'", rec.name);
                    }
                    new_rec.versions.iter_mut().filter(|v| v.id == vid).for_each(|v| v.label = label.to_owned());
                    on_record = true;
                }
                "notes" => {
                    let notes = value.filter(|v| !v.is_empty()).map(str::to_owned);
                    new_rec.versions.iter_mut().filter(|v| v.id == vid).for_each(|v| v.notes = notes.clone());
                    on_record = true;
                }
                k => bail!("unknown mod setting '{k}' (enabled, name; for a version: save_compatible, kind, label, notes)"),
            }
            if on_record {
                rec_changes.push(if key == "enabled" || key == "name" { said } else { format!("{}: {said}", rec.label_of(&vid)) });
            } else {
                changes.push(said);
            }
        }
        if let Some(m) = &new.manifest {
            crate::modpkg::validate(m)?;
        }
        if new_rec != rec {
            self.write_record(&rec, new_rec.clone(), source, message.clone(), rec_changes)?;
        }
        let mut change = self.commit(old, new, changes, source, message, Vec::new())?;
        if explicit {
            change.layer.name = new_rec.display(&vid);
            change.layer.disabled = new_rec.disabled;
        }
        Ok(change)
    }

    /// An installed mod in detail: its current version, or the one asked for
    /// (`MOD@VERSION`, a version id).
    pub fn detail(&self, query: &str) -> Result<ModDetail> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let (rec, pinned) = self.find_ref(&mods, &layers, query)?;
        let vid = pinned.unwrap_or_else(|| rec.current.clone());
        let mut layer = layers.iter().find(|l| l.id == vid).cloned().with_context(|| format!("version {vid} of '{}' has lost its record", rec.name))?;
        let users = self.users_of(&mods, &rec.id)?;
        let current = layers.iter().find(|l| l.id == rec.current);
        let catalog_update = match current.filter(|l| l.builtin).and_then(|l| l.mod_id()) {
            Some(id) => crate::modpkg::catalog(&self.key)?.into_iter().find(|m| m.id == id).filter(|m| Some(m) != current.and_then(|l| l.manifest.as_ref())),
            None => None,
        };
        layer.name = rec.display(&vid);
        layer.disabled = rec.disabled;
        Ok(ModDetail {
            on_vanilla: self.vanilla_stack(&mods, &layers).iter().any(|(_, r)| r.mod_id == rec.id),
            applied: self.applied_by(&rec.id)?,
            used_by: users.iter().map(|(p, _)| p.name.clone()).collect(),
            used_as: users.iter().map(|(p, r)| format!("{}: {}", p.name, r.describe())).collect(),
            catalog_update,
            layer,
            record: Some(rec),
        })
    }

    /// Enabled `fix` mods (their current versions) that apply to vanilla (none unless
    /// `fixes_on_vanilla` is on), in the order they were installed.
    pub fn vanilla_fixes(&self) -> Result<Vec<Layer>> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        Ok(self.vanilla_stack(&mods, &layers).into_iter().map(|(l, _)| l).collect())
    }

    fn vanilla_stack(&self, mods: &[ModRecord], layers: &[Layer]) -> Vec<(Layer, ResolvedMod)> {
        if self.fixes_on_vanilla != Some(true) {
            return Vec::new();
        }
        mods.iter()
            .filter(|r| !r.disabled)
            .filter_map(|r| {
                let res = resolve_entry(mods, &r.id)?;
                let l = layers.iter().find(|l| l.id == res.version)?;
                (l.kind() == ModKind::Fix && l.save_compatible()).then(|| (named(l, r), res))
            })
            .collect()
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

    /// Which profile a launch runs. `requested`: what the launch asked for (a profile's
    /// name or id; `vanilla`: the unmodded game; `None`: the game's default). `default`: the
    /// game's `profile` setting (a profile id). Returns the profile to plan (`None`:
    /// vanilla) and, when the default can't be used (deleted, or gone), a warning to show:
    /// such a launch runs vanilla rather than failing.
    pub fn launch_profile(&self, requested: Option<&str>, default: Option<&str>) -> (Option<String>, Option<String>) {
        if let Some(q) = requested {
            return ((!same_name(q, VANILLA)).then(|| q.to_owned()), None);
        }
        let Some(id) = default.filter(|d| !d.trim().is_empty() && !same_name(d, VANILLA)) else { return (None, None) };
        if let Ok((p, _)) = self.profile(id) {
            return (Some(p.id), None);
        }
        let what = match self.find(id, true) {
            Ok(r) => format!("'{}' ({id}) was deleted", r.profile.name),
            Err(_) => format!("{id} doesn't exist"),
        };
        (None, Some(format!("the default profile {what}; playing vanilla (restore it, or pick another default)")))
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

    pub(crate) fn append(&self, profile: Profile, deleted: bool, source: &str, message: Option<String>) -> Result<ProfileRevision> {
        let path = self.history_path(&profile.id);
        let revision = read_jsonl::<ProfileRevision>(&path)?.last().map_or(0, |r| r.revision) + 1;
        let rev = ProfileRevision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, deleted, profile };
        append_jsonl(&path, &rev)?;
        Ok(rev)
    }

    /// A new profile with `layers` (ids or names, bottom to top), with the save store the
    /// advice picks (see [`Self::create_profile_with`]).
    pub fn create_profile(&self, name: &str, layers: &[String], source: &str, message: Option<String>) -> Result<ProfileRevision> {
        self.create_profile_with(name, layers, None, source, message).map(|(r, _)| r)
    }

    /// A new profile. `saves` names its save store (`default`, `new`, a store's id or
    /// name); `None` follows the advice: Default when every mod is save-compatible, else a
    /// new store named after the profile. Returns the advice too, so callers can say why.
    pub fn create_profile_with(
        &self,
        name: &str,
        layers: &[String],
        saves: Option<&str>,
        source: &str,
        message: Option<String>,
    ) -> Result<(ProfileRevision, quiver_api::savestores::SavesAdvice)> {
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
        let mut advice = self.saves_advice(&layers)?;
        if !self.has_save_stores() {
            // Steam and Epic games: the real saves, whatever the mods (and say so).
            advice.share_default = true;
            advice.reason = crate::savestores::real_saves(self.game_ref()).0.into();
        }
        let wanted = saves.unwrap_or(if advice.share_default { crate::savestores::DEFAULT_STORE } else { crate::savestores::NEW_STORE });
        let store = self.resolve_store(wanted, name, source)?;
        let p = Profile { id, name: name.to_owned(), layers, saves: Some(store), ..Default::default() };
        Ok((self.append(p, false, source, message)?, advice))
    }

    /// Edit a profile ([`Profile::apply`] keys; `layers` takes ids or names). No-op edits
    /// don't create a revision.
    pub fn set_profile(&self, query: &str, ops: &[SettingOp], source: &str, message: Option<String>) -> Result<ProfileRevision> {
        let current = self.find(query, false)?;
        let mut p = current.profile.clone();
        for op in ops {
            let op = match op {
                SettingOp::Set { key, value } if key == "layers" || key == "mods" => {
                    let ids = self.resolve_layers(&quiver_api::mods::split_list(value))?;
                    SettingOp::Set { key: key.clone(), value: ids.join(",") }
                }
                SettingOp::Set { key, value } if key == "profile_config" => {
                    let dirs = self.check_config_dirs(&quiver_api::mods::split_list(value))?;
                    SettingOp::Set { key: key.clone(), value: dirs.join(",") }
                }
                // `new` is made below, once the edit is known to go through.
                SettingOp::Set { key, value } if key == "saves" && !value.trim().is_empty() && !same_name(value, crate::savestores::NEW_STORE) => {
                    SettingOp::Set { key: key.clone(), value: self.resolve_store(value, &p.name, source)? }
                }
                op => op.clone(),
            };
            p.apply(&op).map_err(anyhow::Error::msg)?;
        }
        if p == current.profile {
            return Ok(current);
        }
        self.check_name(&p.name, &p.id)?;
        if p.saves.as_deref().is_some_and(|s| same_name(s, crate::savestores::NEW_STORE)) {
            p.saves = Some(self.resolve_store(crate::savestores::NEW_STORE, &p.name, source)?);
        }
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
        let (layers, mods) = match self.layers().and_then(|l| Ok((mods_of(&l, &self.records()?), l))) {
            Ok((m, l)) => (l, m),
            Err(_) => (Vec::new(), Vec::new()),
        };
        let resolved: Vec<ResolvedMod> = r.profile.layers.iter().filter_map(|e| resolve_entry(&mods, e)).collect();
        let missing_layers = r
            .profile
            .layers
            .iter()
            .filter(|e| resolve_entry(&mods, e).is_none_or(|x| !layers.iter().any(|l| l.id == x.version)))
            .cloned()
            .collect();
        let (conflicts, setting_conflicts, problems, warnings, saves) = match self.stack(Some(&r.profile)) {
            Ok(st) => {
                let saves = self.saves_plan(Some(&r.profile), &st.mods);
                (st.file_conflicts, st.setting_conflicts, st.problems, st.warnings, Some(saves))
            }
            Err(e) => (Vec::new(), Vec::new(), vec![format!("{e:#}")], Vec::new(), None),
        };
        let config_dirs = self.config_dirs(&r.profile);
        ProfileInfo { profile: r.profile, revision: r.revision, missing_layers, conflicts, setting_conflicts, problems, warnings, saves, config_dirs, resolved }
    }

    pub fn state(&self) -> Result<ModsState> {
        let (deleted, live): (Vec<_>, Vec<_>) = self.all_profiles()?.into_iter().partition(|r| r.deleted);
        Ok(ModsState {
            product_id: self.product_id(),
            layers: self.layers()?,
            profiles: live.into_iter().map(|r| self.info(r)).collect(),
            deleted: deleted.into_iter().map(|r| self.info(r)).collect(),
            fixes_on_vanilla: self.fixes_on_vanilla,
            vanilla_fixes: self.vanilla_fixes()?.into_iter().map(|l| l.id).collect(),
            // A game setting; the daemon fills it in.
            default_profile: None,
            game: (!self.key.is_ubisoft()).then_some(self.key),
            save_stores: if self.has_save_stores() { self.save_stores().current()?.stores } else { Vec::new() },
            vanilla_saves: Some(self.saves_plan(None, &[])),
            mods: self.mods()?,
        })
    }

    // -- Mods v2: stacks, saves, per-profile folders, launch plans --------------------------

    /// The installed build's manifest hash.
    fn install_hash(&self) -> Option<String> {
        let raw = std::fs::read(self.install_dir()?.join("uplay_install.manifest")).ok()?;
        crate::manifest::manifest_hash(&raw).ok()
    }

    /// The versions a stack of entries runs (enabled mods only, as a launch applies them),
    /// each named after its mod. Entries that don't resolve are left out.
    pub(crate) fn stack_layers(&self, entries: &[String]) -> Result<Vec<Layer>> {
        let p = Profile { layers: entries.to_vec(), ..Default::default() };
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        Ok(Self::resolve_stack(&mods, &layers, &p).0)
    }

    /// A profile's entries resolved to versions (skipping turned-off mods), with what's
    /// wrong (problems, warnings).
    fn resolve_stack(mods: &[ModRecord], layers: &[Layer], p: &Profile) -> (Vec<Layer>, Vec<ResolvedMod>, Vec<String>, Vec<String>) {
        let (mut out, mut resolved, mut problems, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for e in &p.layers {
            let Some(r) = resolve_entry(mods, e) else {
                problems.push(match split_entry(e) {
                    (m, Some(v)) if mods.iter().any(|r| r.id == m) => {
                        format!("{} has no version {v} any more; pick one (profile set PROFILE mods=…)", mods.iter().find(|r| r.id == m).unwrap().name)
                    }
                    _ => format!("mod {e} is no longer installed; take it out of the profile"),
                });
                continue;
            };
            let rec = mods.iter().find(|x| x.id == r.mod_id).expect("resolved from these mods");
            if rec.disabled {
                warnings.push(format!("{} is turned off, so it's skipped", rec.name));
                continue;
            }
            match layers.iter().find(|l| l.id == r.version) {
                None => problems.push(format!("{} {} ({}) is missing from the mod store", rec.name, r.label, r.version)),
                Some(l) => {
                    if r.pinned && r.retired {
                        warnings.push(format!("{} is pinned to {}, which is retired", rec.name, r.label));
                    }
                    out.push(named(l, rec));
                    resolved.push(r);
                }
            }
        }
        (out, resolved, problems, warnings)
    }

    /// The mods a launch of `profile` (`None`: vanilla) applies and what's wrong with them.
    pub(crate) fn stack(&self, profile: Option<&Profile>) -> Result<Stack> {
        let layers = self.layers()?;
        let all = mods_of(&layers, &self.records()?);
        let (mods, resolved, mut problems, mut warnings) = match profile {
            None => {
                let (m, r) = self.vanilla_stack(&all, &layers).into_iter().unzip();
                (m, r, Vec::new(), Vec::new())
            }
            Some(p) => Self::resolve_stack(&all, &layers, p),
        };
        // A mod answers to its store ids, manifest id and name (its mod's too).
        let is = |j: usize, q: &str| mods[j].is(q) || resolved[j].mod_id == q || same_name(&resolved[j].name, q);
        for (i, m) in mods.iter().enumerate() {
            let Some(man) = &m.manifest else { continue };
            for r in &man.requires {
                if !(0..mods.len()).any(|j| j != i && is(j, r)) {
                    problems.push(format!("{} needs {r}, which isn't in the stack (or is turned off)", m.name));
                }
            }
            for c in &man.conflicts {
                if let Some(j) = (0..mods.len()).find(|&j| j != i && is(j, c)) {
                    problems.push(format!("{} and {} don't work together ({} says so)", m.name, mods[j].name, m.name));
                }
            }
        }
        if mods.iter().any(|m| m.manifest.as_ref().is_some_and(|x| !x.builds.is_empty()))
            && let Some(hash) = self.install_hash()
        {
            for m in &mods {
                if let Some(man) = &m.manifest
                    && !man.builds.is_empty()
                    && !man.builds.iter().any(|b| b.eq_ignore_ascii_case(&hash))
                {
                    warnings.push(format!("{} was checked against other builds of the game (installed: {hash})", m.name));
                }
            }
        }
        let launches: Vec<(String, quiver_api::mods::ModLaunch)> = mods.iter().zip(&resolved).map(|(m, r)| (r.mod_id.clone(), m.launch())).collect();
        let empty = GameSettings::default();
        let base = self.game_settings();
        let over = profile.map_or(&empty, |p| &p.settings);
        // The same argument from two places is passed twice (arguments can repeat on
        // purpose, so this is only a note), e.g. a fix's flag still in the game settings.
        let mut args: Vec<(&str, String)> = base.args.iter().map(|a| (a.as_str(), "the game's settings".to_owned())).collect();
        args.extend(mods.iter().zip(&launches).flat_map(|(m, (_, l))| l.args.iter().map(|a| (a.as_str(), m.name.clone()))));
        args.extend(over.args.iter().map(|a| (a.as_str(), "the profile".to_owned())));
        for (i, (a, from)) in args.iter().enumerate() {
            if let Some((_, first)) = args[..i].iter().find(|(b, f)| b == a && f != from) {
                warnings.push(format!("{a} is passed twice: by {first} and by {from}"));
            }
        }
        let (settings, mut setting_conflicts) = quiver_api::mods::resolve_settings(&base, &launches, over);
        setting_conflicts.extend(patch_conflicts(&mods, &resolved));
        let ids: Vec<String> = mods.iter().map(|m| m.id.clone()).collect();
        let file_conflicts = self.conflicts(&ids);
        if let Some(p) = profile {
            self.store_notes(p, &mods, &mut problems, &mut warnings);
        }
        Ok(Stack { mods, resolved, problems, warnings, setting_conflicts, settings, file_conflicts })
    }
    /// The saves folder a profile made before Mods v2 (relative to the prefix), if any.
    pub(crate) fn legacy_saves(&self, id: &str) -> Option<String> {
        if !self.has_save_stores() {
            return None;
        }
        let saves = self.prefix()?.join(crate::launcher::UPC_REL).join("savegames");
        let name = format!("{}-{id}", self.product_id());
        let mut users: Vec<String> = std::fs::read_dir(&saves).ok()?.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        users.sort();
        users.into_iter().find(|u| saves.join(u).join(&name).is_dir()).map(|u| format!("{}/savegames/{u}/{name}", crate::launcher::UPC_REL))
    }

    /// Whose saves a launch uses: the save store the profile (or vanilla) points at, synced
    /// with the cloud if that's the cloud store. A profile from before save stores (not
    /// migrated yet) follows the old rule (see [`crate::savestores`]).
    pub fn saves_plan(&self, profile: Option<&Profile>, mods: &[Layer]) -> SavesPlan {
        self.store_saves_plan(profile, mods)
    }

    /// Prefix folders as given (prefix-relative or absolute inside the prefix), normalized
    /// and checked: not the prefix's own machinery (Windows, the Connect folder with the
    /// saves, the bridge), not one inside another.
    pub fn check_config_dirs(&self, dirs: &[String]) -> Result<Vec<String>> {
        let protected = [crate::launcher::UPC_REL, "drive_c/windows", "drive_c/quiver", "drive_c/ubi", "dosdevices", "pfx", "drive_c/users/Public"];
        let mut out: Vec<String> = Vec::new();
        for d in dirs {
            let rel = normalize_path(d, self.prefix()).map_err(|e| anyhow::anyhow!(format!("{e:#}").replace("install folder", "prefix")))?;
            let rel = match self.prefix() {
                Some(prefix) => match_install_case(prefix, &rel).0,
                None => rel,
            };
            for p in protected {
                if same_path(&rel, p) || is_under(&rel, p) || is_under(p, &rel) {
                    bail!("{rel}: the prefix's own {p} can't be per-profile");
                }
            }
            if rel.split('/').count() < 2 {
                bail!("{rel}: name a folder inside the prefix's drive, e.g. drive_c/users/steamuser/Documents/My Games/<game>");
            }
            if let Some(o) = out.iter().find(|o| same_path(o, &rel) || is_under(&rel, o) || is_under(o, &rel)) {
                bail!("{rel} and {o} overlap; give one of them");
            }
            if let (Some(prefix), Some(install)) = (self.prefix(), self.install_dir())
                && install.starts_with(prefix.join(&rel))
            {
                bail!("{rel} holds the game's install folder; pick a folder inside it");
            }
            out.push(rel);
        }
        Ok(out)
    }

    /// A profile's own prefix folders and where their files live.
    pub fn config_dirs(&self, p: &Profile) -> Vec<ConfigDir> {
        p.profile_config
            .iter()
            .map(|d| {
                let upper = self.profile_prefix(&p.id).join(d);
                ConfigDir {
                    dir: d.clone(),
                    prefix_path: self.prefix().map(|x| x.join(d)),
                    files: walk(&upper).unwrap_or_default().into_iter().map(|f| f.0).collect(),
                    upper,
                }
            })
            .collect()
    }

    /// Everything a launch of `query` (a profile; `None` or `vanilla`: the unmodded game,
    /// with fixes if `fixes_on_vanilla` is on) uses. Needs [`Self::with_game`].
    pub fn plan(&self, query: Option<&str>) -> Result<Plan> {
        let game = self.game.as_ref().context("a launch plan needs the installed game")?;
        let (profile, revision) = match query.filter(|q| !same_name(q, VANILLA)) {
            Some(q) => {
                let (p, r) = self.profile(q)?;
                (Some(p), r)
            }
            None => (None, 0),
        };
        let mut st = self.stack(profile.as_ref())?;
        let saves = self.saves_plan(profile.as_ref(), &st.mods);
        let mut overlays = Vec::new();
        let file_mods: Vec<&Layer> = st.mods.iter().filter(|m| m.has_files()).collect();
        for m in &file_mods {
            if !self.layer_dir(&m.id).is_dir() {
                st.problems.push(format!("{} has lost its folder ({}); uninstall and install it again", m.name, self.layer_dir(&m.id).display()));
            }
        }
        // A profile always runs over its own writable layer (as in v1); vanilla only when
        // fixes change install files.
        let state = match &profile {
            Some(p) => Some(self.profile_dir(&p.id)),
            None if !file_mods.is_empty() => Some(self.profile_dir(VANILLA)),
            None => None,
        };
        if let Some(state) = state {
            let lower = file_mods.iter().rev().map(|m| self.layer_dir(&m.id)).collect();
            overlays.push(OverlaySpec { target: game.install_dir.clone(), lower, upper: state.join("upper"), work: state.join("work") });
        }
        let mut config_dirs = Vec::new();
        let prefix = game.prefix();
        if let Some(p) = &profile
            && !p.profile_config.is_empty()
            && prefix.is_none()
        {
            st.problems.push("profile_config: the game has no Wine prefix (a native game), so there are no prefix folders to give the profile".into());
        }
        if let (Some(p), Some(prefix)) = (&profile, prefix) {
            if let Err(e) = self.check_config_dirs(&p.profile_config) {
                st.problems.push(format!("profile_config: {e:#}"));
            }
            for (d, info) in p.profile_config.iter().zip(self.config_dirs(p)) {
                overlays.push(OverlaySpec {
                    target: prefix.join(d),
                    lower: Vec::new(),
                    upper: info.upper.clone(),
                    work: self.profile_dir(&p.id).join("prefix-work").join(short_hash(d)),
                });
                config_dirs.push(info);
            }
        }
        let view = View {
            prefix: game.prefix.clone(),
            profile: profile
                .as_ref()
                .filter(|p| !p.profile_config.is_empty() && prefix.is_some())
                .map(|p| (p.id.clone(), self.profile_prefix(&p.id), p.profile_config.clone())),
        };
        // Recorded under the mod's id, so switching versions reconciles like any change of
        // what the mod wants.
        let mut patches: Vec<Wanted> = st
            .mods
            .iter()
            .zip(&st.resolved)
            .flat_map(|(m, r)| m.patches().iter().map(|p| Wanted { mod_id: r.mod_id.clone(), patch: p.clone() }))
            .collect();
        // Config patches are prefix-relative: a native game has nowhere to apply them.
        if prefix.is_none() && !patches.is_empty() {
            for w in &patches {
                st.warnings.push(format!("{}: config patch on {} not applied: the game has no Wine prefix (a native game)", w.mod_id, w.patch.file));
            }
            patches.clear();
        }
        // What the patches would touch (they're applied at launch).
        let preview = patches
            .iter()
            .map(|w| {
                let files: Vec<PatchedFile> = view
                    .glob(&w.patch.file)
                    .into_iter()
                    .map(|rel| PatchedFile { path: view.write_path(&rel), scope: view.scope(&rel), rel })
                    .collect();
                PatchStatus {
                    mod_id: w.mod_id.clone(),
                    file: w.patch.file.clone(),
                    target: w.patch.target().to_owned(),
                    state: if files.is_empty() { PatchState::Pending } else { PatchState::Unknown },
                    detail: if files.is_empty() { "pending until first run: the game hasn't written this file yet".into() } else { String::new() },
                    files,
                }
            })
            .collect();
        let api = LaunchPlan {
            product_id: self.product_id(),
            profile: profile.clone(),
            revision,
            mods: st.mods.iter().map(|m| m.id.clone()).collect(),
            settings: st.settings,
            setting_conflicts: st.setting_conflicts,
            file_conflicts: st.file_conflicts,
            problems: st.problems,
            warnings: st.warnings,
            saves: saves.clone(),
            mounts: overlays.iter().map(|o| Mount { target: o.target.clone(), lower: o.lower.clone(), upper: o.upper.clone() }).collect(),
            config_dirs,
            patches: preview,
            resolved: st.resolved,
        };
        let saves_folder = saves.folder.trim_start_matches("savegames/<user>/").to_owned();
        Ok(Plan { api, overlays, patches, view, record: self.record_path(), saves_folder, session: None })
    }

    /// Make older data explicit: every live profile without a save store gets one (see
    /// [`Self::migrate_saves`]). Layers need nothing (they're file-only mods as they are).
    /// Idempotent.
    pub fn migrate(&self, dry_run: bool, source: &str) -> Result<MigrationReport> {
        let mut report = MigrationReport { product_id: self.product_id(), dry_run, ..Default::default() };
        report.file_only_mods = self.layers()?.into_iter().filter(|l| l.manifest.is_none()).map(|l| l.id).collect();
        if self.prefix().is_none() {
            report.notes.push("no game prefix given; profiles' saves folders weren't checked".into());
        }
        self.migrate_saves(dry_run, source, &mut report)?;
        report.builtin_updates = self.update_builtins(dry_run, source)?;
        report.notes.extend(report.builtin_updates.iter().map(|u| format!("built-in mod updated: {u}")));
        Ok(report)
    }

    /// What a launch of `query` needs; `None` for vanilla.
    pub fn launch(&self, query: Option<&str>, install_dir: &Path) -> Result<Option<ProfileLaunch>> {
        let Some(query) = query.filter(|q| !same_name(q, VANILLA)) else { return Ok(None) };
        let (profile, revision) = self.profile(query)?;
        let st = self.stack(Some(&profile))?;
        if let Some(e) = profile.layers.iter().find(|e| !st.resolved.iter().any(|r| &r.entry == *e)) {
            bail!("profile '{}' uses layer {e}, which was removed; update its layers", profile.name);
        }
        let mut lower = Vec::new();
        for m in st.mods.iter().rev() {
            let dir = self.layer_dir(&m.id);
            if !dir.is_dir() {
                bail!("profile '{}' uses layer {}, which was removed; update its layers", profile.name, m.id);
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
    fn launch_profile_follows_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ModStore::new(&tmp.path().join("data"), 3539);
        let dlss = store.create_profile("DLSS", &[], "cli", None).unwrap().profile;
        let id = dlss.id.as_str();
        // Named launches win; `vanilla` forces vanilla whatever the default.
        assert_eq!(store.launch_profile(Some("other"), Some(id)), (Some("other".into()), None));
        assert_eq!(store.launch_profile(Some("Vanilla"), Some(id)), (None, None));
        // No profile named: the default, or vanilla without one.
        assert_eq!(store.launch_profile(None, Some(id)), (Some(dlss.id.clone()), None));
        assert_eq!(store.launch_profile(None, None), (None, None));
        assert_eq!(store.launch_profile(None, Some("vanilla")), (None, None));
        // A deleted default runs vanilla, with a warning naming it; restoring brings it back.
        store.delete_profile(id, "cli").unwrap();
        let (p, warn) = store.launch_profile(None, Some(id));
        assert_eq!(p, None);
        assert!(warn.unwrap().contains("'DLSS'"));
        store.restore_profile(id, 1, "cli").unwrap();
        assert_eq!(store.launch_profile(None, Some(id)).0.as_deref(), Some(id));
        let (p, warn) = store.launch_profile(None, Some("pgone00"));
        assert!(p.is_none() && warn.unwrap().contains("pgone00"));
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

        let r2 = store.set_profile("visual overhaul", &ops(&["layers=A,B"]), "agent", None).unwrap();
        assert_eq!(r2.revision, 2);
        assert_eq!(r2.profile.layers, [a.id.clone(), b.id.clone()]);
        // A no-op doesn't make a revision.
        assert_eq!(store.set_profile(&id, &ops(&["layers=A,B"]), "cli", None).unwrap().revision, 2);
        let r3 = store.set_profile(&id, &ops(&["pinned=on", "env.DXVK_HUD=fps", "name=VO"]), "cli", None).unwrap();
        assert_eq!(r3.revision, 3);
        assert!(r3.profile.pinned);
        assert_eq!(r3.profile.id, id, "renames keep the id");
        assert!(store.profile("Visual overhaul").is_err());
        assert_eq!(store.profile("vo").unwrap().1, 3);

        // Conflicts between layers ignore case; the top layer is listed last.
        let state = store.state().unwrap();
        assert_eq!(state.profiles.len(), 1);
        assert_eq!(
            state.profiles[0].conflicts,
            [Conflict { path: "x.txt".into(), layers: vec![a.id.clone(), b.id.clone()], ..Default::default() }]
        );

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

    fn strs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn whiteout_at(p: &Path) -> bool {
        p.symlink_metadata().is_ok_and(|m| is_whiteout(&m))
    }

    #[test]
    fn deletion_paths_are_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("My Game");
        std::fs::create_dir_all(&install).unwrap();
        let n = |p: &str| normalize_path(p, Some(&install));
        assert_eq!(n("videos/intro.bik").unwrap(), "videos/intro.bik");
        assert_eq!(n("./videos//intro.bik").unwrap(), "videos/intro.bik");
        assert_eq!(n(r"Videos\Intro.bik").unwrap(), "Videos/Intro.bik");
        assert_eq!(n("videos/").unwrap(), "videos");
        // Absolute paths only inside the install folder.
        assert_eq!(n(&format!("{}/data/x.dds", install.display())).unwrap(), "data/x.dds");
        assert!(n("/etc/passwd").unwrap_err().to_string().contains("outside"));
        assert!(normalize_path("/etc/passwd", None).is_err());
        assert!(n(&format!("{}/../other/x", install.display())).unwrap_err().to_string().contains(".."));
        // No climbing out, and not the root itself.
        for bad in ["../x", "a/../../x", "a/..", "..", r"a\..\..\x"] {
            assert!(n(bad).unwrap_err().to_string().contains(".."), "{bad}");
        }
        assert!(n("/").unwrap_err().to_string().contains("outside"));
        for bad in ["", ".", "./", &install.display().to_string()] {
            assert!(n(bad).unwrap_err().to_string().contains("install folder itself"), "{bad}");
        }
        assert!(n("a\0b").is_err());
    }

    #[test]
    fn deletion_paths_match_install_case() {
        let tmp = tempfile::tempdir().unwrap();
        let install = folder(&tmp.path().join("install"), &[("Videos/Intro.BIK", "v"), ("DataPC/a.forge", "f")]);
        assert_eq!(match_install_case(&install, "videos/intro.bik"), ("Videos/Intro.BIK".into(), true));
        assert_eq!(match_install_case(&install, "Videos/Intro.BIK"), ("Videos/Intro.BIK".into(), true));
        assert_eq!(match_install_case(&install, "VIDEOS"), ("Videos".into(), true));
        // Past the first missing component, the rest stays as given.
        assert_eq!(match_install_case(&install, "videos/outro.bik"), ("Videos/outro.bik".into(), false));
        assert_eq!(match_install_case(&install, "movies/a/b"), ("movies/a/b".into(), false));
        // Exact names win over case-insensitive ones.
        std::fs::create_dir_all(install.join("videos")).unwrap();
        assert_eq!(match_install_case(&install, "videos").0, "videos");
    }

    #[test]
    fn deletion_plans() {
        let (l, notes) = plan_deletions(&[], &strs(&["b/x", "a", "B/X"]), &[]).unwrap();
        assert_eq!((l.as_slice(), notes.len()), (&strs(&["a", "b/x"])[..], 0));
        // A folder takes in deletions inside it; inside a deleted folder is refused.
        let (l2, notes) = plan_deletions(&l, &strs(&["b"]), &[]).unwrap();
        assert_eq!(l2, strs(&["a", "b"]));
        assert_eq!(notes.len(), 1);
        assert!(plan_deletions(&l2, &strs(&["B/y"]), &[]).unwrap_err().to_string().contains("folder b"));
        // `b/` isn't `bc`.
        assert_eq!(plan_deletions(&l2, &strs(&["bc"]), &[]).unwrap().0, strs(&["a", "b", "bc"]));
        // Keep takes deletions back, ignoring case.
        assert_eq!(plan_deletions(&l2, &[], &strs(&["A"])).unwrap().0, strs(&["b"]));
        assert!(plan_deletions(&l2, &[], &strs(&["c"])).unwrap_err().to_string().contains("doesn't delete"));
        assert!(plan_deletions(&l2, &[], &strs(&["b/x"])).unwrap_err().to_string().contains("keep b"));
    }

    #[test]
    fn layer_deletions() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let install = folder(
            &t.join("install"),
            &[("Videos/Intro.bik", "i"), ("Videos/Logo.bik", "l"), ("Data/keep.txt", "k"), ("Data/old.txt", "o"), ("game.exe", "e")],
        );
        let store = ModStore::new(&t.join("data"), 3);

        // Deletions only: no folder at all. Case follows the install.
        let c = store.add_layer_with("No intro", None, &strs(&["videos/intro.bik", "nope.txt"]), Some(&install), "agent").unwrap();
        let ni = c.layer.clone();
        assert_eq!(ni.deletes, strs(&["nope.txt", "Videos/Intro.bik"]));
        assert_eq!((ni.files, ni.bytes, ni.origin.as_ref()), (0, 0, None));
        assert_eq!(c.revision, 1);
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(c.warnings[0].contains("nope.txt"));
        let dir = store.layer_dir(&ni.id);
        assert!(whiteout_at(&dir.join("Videos/Intro.bik")) && whiteout_at(&dir.join("nope.txt")));
        // The install keeps its files.
        assert!(install.join("Videos/Intro.bik").is_file());
        assert!(store.add_layer_with("Empty", None, &[], Some(&install), "cli").is_err());
        assert!(store.add_layer_with("Bad", None, &strs(&["../x"]), Some(&install), "cli").is_err());
        assert!(!store.layers().unwrap().iter().any(|l| l.name == "Bad"));

        // A folder whose own whiteouts come along (cp -a keeps device nodes), and a
        // deleted folder the layer has files in becomes opaque.
        let src = folder(&t.join("src"), &[("data/new.txt", "n")]);
        make_whiteout(&src.join("game.exe")).unwrap();
        let c = store.add_layer_with("Rework", Some(&src), &strs(&["data"]), Some(&install), "cli").unwrap();
        let rw = c.layer.clone();
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(rw.deletes, strs(&["Data", "game.exe"]));
        assert_eq!(rw.files, 1);
        let rdir = store.layer_dir(&rw.id);
        assert!(whiteout_at(&rdir.join("game.exe")));
        assert!(is_opaque(&rdir.join("Data")) && rdir.join("Data/new.txt").is_file());
        assert!(whiteout_at(&src.join("game.exe")), "the source is left as it was");
        // The layer can't delete a file it provides.
        let e = store.change_deletions("Rework", &strs(&["data/new.txt"]), &[], Some(&install), "cli", None).unwrap_err();
        assert!(e.to_string().contains("folder Data"), "{e}");
        let src2 = folder(&t.join("src2"), &[("game.exe", "x")]);
        let e = store.add_layer_with("Both", Some(&src2), &strs(&["game.exe"]), Some(&install), "cli").unwrap_err();
        assert!(e.to_string().contains("its own game.exe"), "{e}");
        assert!(std::fs::read_dir(t.join("data/mods/3/layers")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".partial")));

        // Later changes are revisions; a folder whiteout replaces whiteouts inside it.
        let c = store.change_deletions("no intro", &strs(&["VIDEOS"]), &strs(&["nope.txt"]), Some(&install), "agent", None).unwrap();
        assert_eq!((c.layer.deletes.as_slice(), c.revision), (&strs(&["Videos"])[..], 2));
        assert!(whiteout_at(&dir.join("Videos")) && !dir.join("nope.txt").exists());
        // No-op: no revision.
        assert_eq!(store.change_deletions(&ni.id, &strs(&["videos"]), &[], Some(&install), "cli", None).unwrap().revision, 2);
        let c = store.change_deletions(&ni.id, &[], &strs(&["videos"]), Some(&install), "cli", Some("intro back".into())).unwrap();
        assert!(c.layer.deletes.is_empty() && !dir.join("Videos").exists());
        assert_eq!(store.layer(&ni.id).unwrap().deletes, c.layer.deletes, "metadata is updated");
        // Keeping the opaque folder leaves the layer's files and clears the flag.
        store.change_deletions("rework", &[], &strs(&["data"]), Some(&install), "cli", None).unwrap();
        assert!(!is_opaque(&rdir.join("Data")) && rdir.join("Data/new.txt").is_file());

        let (_, hist) = store.layer_history_of("No intro").unwrap();
        assert_eq!(hist.iter().map(|r| r.revision).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(hist[0].deletes, strs(&["nope.txt", "Videos/Intro.bik"]));
        assert_eq!(hist[2].message.as_deref(), Some("intro back"));
        let c = store.restore_layer("No intro", 1, "cli").unwrap();
        assert_eq!((c.layer.deletes, c.revision), (hist[0].deletes.clone(), 4));
        assert!(whiteout_at(&dir.join("Videos/Intro.bik")));
        assert!(store.restore_layer("No intro", 9, "cli").is_err());

        // A layer from before deletions gets its "added" revision on first change.
        let old = store.add_layer("Old", &folder(&t.join("old"), &[("a.txt", "a")]), None, "gui").unwrap();
        std::fs::remove_file(store.layer_history_path(&old.id)).unwrap();
        let c = store.change_deletions("Old", &strs(&["b.txt"]), &[], None, "cli", None).unwrap();
        let hist = store.layer_history(&old.id).unwrap();
        assert_eq!((c.revision, hist[0].source.as_str(), hist[0].deletes.len()), (2, "gui", 0));

        store.remove_layer("No intro").unwrap();
        assert!(!store.layer_history_path(&ni.id).exists());
    }

    #[test]
    fn conflicts_with_deletions() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let store = ModStore::new(&t.join("data"), 4);
        let a = store.add_layer("A", &folder(&t.join("a"), &[("v/intro.bik", "a"), ("v/logo.bik", "a"), ("x.txt", "a")]), None, "cli").unwrap();
        let b = store.add_layer_with("B", Some(&folder(&t.join("b"), &[("v/logo.bik", "b")])), &strs(&["v", "X.txt"]), None, "cli").unwrap().layer;
        let c = store.conflicts(&[a.id.clone(), b.id.clone()]);
        let by = |path: &str| c.iter().find(|c| c.path == path).cloned();
        // B deletes the folder: A's intro is hidden, B's logo replaces A's.
        assert_eq!(by("v/intro.bik").unwrap(), Conflict { path: "v/intro.bik".into(), layers: vec![a.id.clone(), b.id.clone()], deleted_by: vec![b.id.clone()] });
        assert_eq!(by("v/logo.bik").unwrap().deleted_by, Vec::<String>::new());
        assert_eq!(by("x.txt").unwrap().deleted_by, [b.id.clone()]);
        // Below, a deletion hides nothing of a higher layer.
        assert!(store.conflicts(&[b.id.clone(), a.id.clone()]).iter().all(|c| c.deleted_by.is_empty()));
        assert_eq!(store.conflicts(&[b.id.clone(), a.id.clone()]).len(), 1);
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

    // -- Mods v2 ----------------------------------------------------------------------------

    const STORE_V1: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mods-v2/store-v1");
    const USER: &str = "USER-ID";

    fn game(t: &Path, product_id: u32) -> Game {
        Game {
            product_id,
            name: "Game".into(),
            install_dir: t.join("install"),
            prefix: t.join("pfx"),
            exe: "game.exe".into(),
            args: None,
            language: None,
            game: None,
        }
    }

    fn saves_dir(g: &Game, name: &str) -> PathBuf {
        g.prefix.join(crate::launcher::UPC_REL).join("savegames").join(USER).join(name)
    }

    /// A copy of the live v1 store for Origins (the DLSS layer and profile, the "Skip
    /// intros" deletions layer and their histories, with personal paths replaced), plus an
    /// install and a prefix with vanilla's and the DLSS profile's saves folders.
    fn v1_store(t: &Path) -> (ModStore, Game) {
        let data = t.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let out = std::process::Command::new("cp").arg("-a").arg(format!("{STORE_V1}/.")).arg(&data).output().unwrap();
        assert!(out.status.success());
        let layers = data.join("mods/3539/layers");
        folder(&layers.join("l9uv5ua"), &[("d3d11.dll", "x"), ("nvngx_dlss.dll", "y"), ("aco_dlss.ini", "z")]);
        let skip: Layer = serde_json::from_str(&std::fs::read_to_string(layers.join("lzrticn.json")).unwrap()).unwrap();
        std::fs::create_dir_all(layers.join("lzrticn")).unwrap();
        sync_deletions(&layers.join("lzrticn"), &[], &skip.deletes).unwrap();
        let g = game(t, 3539);
        folder(&g.install_dir, &[("ACOrigins.exe", "e"), ("videos/UbisoftLogo.bk2", "v"), ("videos/en/Epilepsy.bk2", "v")]);
        folder(&saves_dir(&g, "3539"), &[("1.save", "vanilla")]);
        folder(&saves_dir(&g, "3539-ptkzxpv"), &[("1.save", "dlss")]);
        (ModStore::new(&data, 3539).with_game(&g, None), g)
    }

    #[test]
    fn v1_store_reads_as_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, g) = v1_store(tmp.path());
        // Layers are file-only mods: same ids, no manifest, unknown save compatibility.
        let mods = store.layers().unwrap();
        assert_eq!(mods.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["l9uv5ua", "lzrticn"]);
        assert!(mods.iter().all(|l| l.manifest.is_none() && l.enabled() && !l.save_compatible()));
        assert_eq!(mods[1].deletes.len(), 4);
        // Old histories read as they are.
        assert_eq!(store.layer_history("lzrticn").unwrap().len(), 1);
        let (p, hist) = store.history_of("DLSS").unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(p.layers, ["l9uv5ua", "lzrticn"]);
        assert_eq!((p.separate_saves, p.profile_config.len()), (None, 0));
        // The profile's overrides stay its overrides; its layers are its mod stack.
        let plan = store.plan(Some("dlss")).unwrap();
        assert_eq!(plan.api.mods, ["l9uv5ua", "lzrticn"]);
        assert_eq!(plan.api.settings.dll_overrides["d3d11"], "n,b");
        assert!(plan.api.problems.is_empty(), "{:?}", plan.api.problems);
        // Its saves stay where they are, synced as before (cloud_saves=on) until it's
        // migrated to a save store.
        assert_eq!(plan.saves_folder, "3539-ptkzxpv");
        assert!(plan.api.saves.separate && plan.api.saves.cloud && plan.api.saves.store.is_none());
        assert_eq!(plan.overlays.len(), 1);
        assert_eq!(plan.overlays[0].lower, [store.layer_dir("lzrticn"), store.layer_dir("l9uv5ua")], "topmost first");
        assert_eq!(plan.overlays[0].upper, store.profile_dir("ptkzxpv").join("upper"), "the same writable layer as v1");
        // Vanilla: nothing mounted, its own saves.
        let v = store.plan(None).unwrap();
        assert!(v.overlays.is_empty() && v.api.mods.is_empty() && v.saves_folder == "3539");
        assert_eq!(v.api.saves.store.as_deref(), Some("default"));
        assert!(g.install_dir.join("videos/UbisoftLogo.bk2").is_file());
    }

    #[test]
    fn migration_keeps_existing_save_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, g) = v1_store(tmp.path());
        // Mark both mods save-compatible, as the DLSS and intro-skip mods are: the rule
        // alone would now share vanilla's saves, but the profile's own folder wins.
        store.set_mod("DLSS", &ops(&["save_compatible=on"]), "cli", None).unwrap();
        let c = store.set_mod("lzrticn", &ops(&["save_compatible=on", "kind=fix"]), "cli", None).unwrap();
        assert_eq!(store.layer_history("lzrticn").unwrap().last().unwrap().changes, ["save_compatible=on", "kind=fix"]);
        assert_eq!(c.layer.manifest.as_ref().unwrap().id, "lzrticn");
        let info = store.state().unwrap().profiles.remove(0);
        let saves = info.saves.unwrap();
        assert!(saves.separate && saves.reason.contains("before save stores"), "{saves:?}");

        let dry = store.migrate(true, "cli").unwrap();
        assert_eq!(dry.kept_separate_saves.len(), 1);
        assert!(dry.kept_separate_saves[0].1.ends_with(&format!("savegames/{USER}/3539-ptkzxpv")));
        assert_eq!(store.history("ptkzxpv").unwrap().len(), 4, "a dry run changes nothing");
        assert!(store.save_stores().history().unwrap().is_empty());
        assert!(dry.file_only_mods.is_empty(), "both have a manifest of their own now");

        let r = store.migrate(false, "cli").unwrap();
        assert_eq!(r.kept_separate_saves.len(), 1);
        assert_eq!(r.stores_created.len(), 1);
        let sid = r.stores_created[0].clone();
        assert_eq!(r.profile_stores, [("ptkzxpv".to_owned(), sid.clone())]);
        assert!(r.notes.iter().any(|n| n.contains("cloud_saves=on") && n.contains("merge")), "{:?}", r.notes);
        // The folder became a store (local only: Default keeps the cloud), as a revision.
        let stores = store.save_stores().current().unwrap();
        let s = stores.get(&sid).unwrap();
        assert_eq!((s.name.as_str(), s.folder.as_str(), s.cloud), ("DLSS", "3539-ptkzxpv", false));
        assert_eq!(stores.cloud_store().unwrap().id, "default");
        let hist = store.history("ptkzxpv").unwrap();
        assert_eq!(hist.len(), 5);
        assert_eq!(hist[4].profile.saves.as_deref(), Some(sid.as_str()));
        assert_eq!((hist[4].profile.separate_saves, hist[4].profile.cloud_saves), (None, false));
        assert!(hist[4].message.as_deref().unwrap().contains("nothing moved"));
        // Idempotent.
        let again = store.migrate(false, "cli").unwrap();
        assert!(again.kept_separate_saves.is_empty() && again.profile_stores.is_empty());
        assert_eq!(store.history("ptkzxpv").unwrap().len(), 5);
        assert_eq!(store.save_stores().history().unwrap().len(), 1);
        let plan = store.plan(Some("DLSS")).unwrap();
        assert_eq!(plan.saves_folder, "3539-ptkzxpv");
        assert!(!plan.api.saves.cloud && plan.api.saves.cloud_note.as_deref().unwrap().contains("'Default' is the store that syncs"));
        // Nothing moved.
        assert_eq!(std::fs::read_to_string(saves_dir(&g, "3539-ptkzxpv").join("1.save")).unwrap(), "dlss");
        assert_eq!(std::fs::read_to_string(saves_dir(&g, "3539").join("1.save")).unwrap(), "vanilla");
        // Pointing it at Default is a plain setting; the old folder stays a store.
        store.set_profile("DLSS", &ops(&["saves=default"]), "cli", None).unwrap();
        let plan = store.plan(Some("DLSS")).unwrap();
        assert!(plan.saves_folder == "3539" && plan.api.saves.cloud);
        assert!(saves_dir(&g, "3539-ptkzxpv").join("1.save").is_file(), "the old folder stays");
        // And reversible.
        store.restore_profile("DLSS", 5, "cli").unwrap();
        assert_eq!(store.plan(Some("DLSS")).unwrap().saves_folder, "3539-ptkzxpv");
        // Restoring a revision from before stores: the next migration finds the same store.
        store.restore_profile("DLSS", 4, "cli").unwrap();
        let r = store.migrate(false, "cli").unwrap();
        assert_eq!(r.profile_stores, [("ptkzxpv".to_owned(), sid.clone())]);
        assert!(r.stores_created.is_empty());
    }

    #[test]
    fn migration_keeps_unused_folders_as_retired_stores() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, g) = v1_store(tmp.path());
        // A profile that shared vanilla's saves (separate_saves=off) but still has a folder.
        let (mut p, _) = store.profile("DLSS").unwrap();
        p.separate_saves = Some(false);
        store.append(p, false, "cli", None).unwrap();
        let r = store.migrate(false, "cli").unwrap();
        assert_eq!(r.profile_stores, [("ptkzxpv".to_owned(), "default".to_owned())]);
        assert_eq!(r.stores_created.len(), 1);
        assert!(r.notes.iter().any(|n| n.contains("retired store 'DLSS'") && n.contains("merge")), "{:?}", r.notes);
        let s = store.save_stores().current().unwrap();
        let old = s.get(&r.stores_created[0]).unwrap();
        assert!(old.retired && !old.cloud && old.folder == "3539-ptkzxpv");
        // It can be merged from while retired.
        let m = store.merge_stores("DLSS", None, &tmp.path().join("bk"), "cli").unwrap();
        assert_eq!(m.slots.len(), 1);
        assert!(saves_dir(&g, "3539-ptkzxpv").join("1.save").is_file());
        // Idempotent.
        assert!(store.migrate(false, "cli").unwrap().stores_created.is_empty());
    }

    const STORE_V2: &str =concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mods-v2/store-v2");

    /// The live store for Origins as Mods v2 left it (DLSS-v2 and the built-in intro skip,
    /// `separate_saves=on` from the v1 migration, cloud saves on; personal paths replaced),
    /// with both saves folders as they are: vanilla's six slots from the evening before, the
    /// DLSS profile's six slots, newer.
    fn v2_store(t: &Path) -> (ModStore, Game) {
        let data = t.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let out = std::process::Command::new("cp").arg("-a").arg(format!("{STORE_V2}/.")).arg(&data).output().unwrap();
        assert!(out.status.success());
        let layers = data.join("mods/3539/layers");
        for id in ["l9uv5ua", "lbi2b9z"] {
            folder(&layers.join(id), &[("d3d11.dll", "x"), ("nvngx_dlss.dll", "y"), ("aco_dlss.ini", "z")]);
        }
        for id in ["lzrticn", "mevkhrc"] {
            let l: Layer = serde_json::from_str(&std::fs::read_to_string(layers.join(format!("{id}.json"))).unwrap()).unwrap();
            std::fs::create_dir_all(layers.join(id)).unwrap();
            sync_deletions(&layers.join(id), &[], &l.deletes).unwrap();
        }
        let g = game(t, 3539);
        folder(&g.install_dir, &[("ACOrigins.exe", "e"), ("videos/UbisoftLogo.bk2", "v")]);
        let at = |s: &str| -> std::time::SystemTime { s.parse::<chrono::DateTime<chrono::Utc>>().unwrap().into() };
        for (dir, when, tag) in [("3539", "2026-11-16T04:03:26Z", "vanilla"), ("3539-ptkzxpv", "2026-11-16T08:51:02Z", "dlss")] {
            for slot in 1..=6 {
                let p = saves_dir(&g, dir).join(format!("{slot}.save"));
                folder(&saves_dir(&g, dir), &[(&format!("{slot}.save"), &format!("{tag} {slot}"))]);
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(at(when)).unwrap();
            }
        }
        (ModStore::new(&data, 3539).with_game(&g, None), g)
    }

    #[test]
    fn live_v2_store_migrates_to_save_stores_and_merges_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, g) = v2_store(tmp.path());
        let (p, rev) = store.profile("DLSS").unwrap();
        assert_eq!((rev, p.separate_saves, p.cloud_saves, p.saves.as_deref()), (7, Some(true), true, None));
        assert_eq!(p.layers, ["lbi2b9z", "mevkhrc"]);
        // Before migration: its own folder, synced (the old rule).
        assert_eq!(store.plan(Some("DLSS")).unwrap().saves_folder, "3539-ptkzxpv");

        let r = store.migrate(false, "agent").unwrap();
        let sid = r.stores_created[0].clone();
        assert_eq!(r.profile_stores, [("ptkzxpv".to_owned(), sid.clone())]);
        assert!(r.notes[0].contains("separate_saves was on"), "{:?}", r.notes);
        let (p, rev) = store.profile("DLSS").unwrap();
        assert_eq!((rev, p.saves.as_deref()), (8, Some(sid.as_str())));
        let report = store.stores_report(Some(USER)).unwrap();
        assert_eq!(report.stores.len(), 2);
        assert_eq!(report.cloud.as_deref(), Some("default"));
        let (def, dlss) = (&report.stores[0], &report.stores[1]);
        assert_eq!((def.store.name.as_str(), def.slots, def.used_by.len()), ("Default", 6, 1));
        assert_eq!(def.used_by[0].id, VANILLA);
        assert_eq!((dlss.store.name.as_str(), dlss.slots, dlss.used_by[0].name.as_str()), ("DLSS", 6, "DLSS"));
        assert!(dlss.last_modified > def.last_modified);
        // Both mods are save-compatible, so sharing Default draws no warning.
        assert!(store.state().unwrap().profiles[0].warnings.iter().all(|w| !w.contains("could change saves")));

        // The user's way back to one set: merge DLSS into Default (backups first), then
        // point the profile at Default, then retire the DLSS store (its files stay).
        let backups = tmp.path().join("save-backups");
        let m = store.merge_stores("DLSS", Some("default"), &backups, "cli").unwrap();
        assert!(m.slots.iter().all(|s| s.outcome == quiver_api::savestores::MergeOutcome::Replaced), "{:?}", m.slots);
        assert_eq!(m.slots.len(), 6);
        assert!(m.marked_for_upload, "Default syncs: the merged slots win over the cloud's at the next sync");
        assert_eq!(m.backups.len(), 1);
        assert_eq!(std::fs::read_to_string(m.backups[0].path.join("3.save")).unwrap(), "vanilla 3");
        assert_eq!(std::fs::read_to_string(saves_dir(&g, "3539").join("3.save")).unwrap(), "dlss 3");
        assert!(saves_dir(&g, "3539").join("3.save.upload").is_file());
        assert_eq!(std::fs::read_to_string(saves_dir(&g, "3539-ptkzxpv").join("3.save")).unwrap(), "dlss 3");
        assert!(store.edit_stores(&SaveStoreOp::Retire { store: "DLSS".into() }, "cli", None).unwrap_err().to_string().contains("used by DLSS"));
        store.set_profile("DLSS", &ops(&["saves=default"]), "cli", None).unwrap();
        let plan = store.plan(Some("DLSS")).unwrap();
        assert!(plan.saves_folder == "3539" && plan.api.saves.cloud && plan.api.saves.cloud_note.is_none());
        store.edit_stores(&SaveStoreOp::Retire { store: "DLSS".into() }, "cli", None).unwrap();
        let report = store.stores_report(Some(USER)).unwrap();
        assert_eq!((report.stores.len(), report.retired.len(), report.retired[0].slots), (1, 1, 6));
        assert_eq!(report.stores[0].used_by.len(), 2);
    }

    use crate::savestores::SaveStoreOp;

    #[test]
    fn save_fork_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let g = game(t, 7);
        folder(&g.install_dir, &[("game.exe", "e")]);
        std::fs::create_dir_all(&g.prefix).unwrap();
        let store = ModStore::new(&t.join("data"), 7).with_game(&g, None);
        let pkg = |name: &str, save: Option<bool>| {
            let dir = t.join("pkgs").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let save = save.map(|s| format!("save_compatible = {s}\n")).unwrap_or_default();
            std::fs::write(dir.join("quiver-mod.toml"), format!("id = \"{name}\"\nname = \"{name}\"\n{save}[launch]\nargs = [\"-{name}\"]\n")).unwrap();
            store.install(&Package::Dir { path: dir, name: None }, "cli").unwrap().layer
        };
        let safe = pkg("safe", Some(true));
        let gameplay = pkg("gameplay", Some(false));
        let unknown = pkg("unknown", None);
        assert!(safe.files == 0 && store.layer_dir(&safe.id).is_dir());
        let mk = |name: &str, mods: &[&Layer]| {
            let (r, advice) = store.create_profile_with(name, &mods.iter().map(|l| l.id.clone()).collect::<Vec<_>>(), None, "cli", None).unwrap();
            assert!(r.profile.saves.is_some(), "a new profile always names its store");
            (store.plan(Some(name)).unwrap(), advice)
        };
        let (p, a) = mk("Empty", &[]);
        assert!(!p.api.saves.separate && a.share_default, "no mods: Default");
        let (p, _) = mk("Safe", &[&safe]);
        assert!(!p.api.saves.separate && p.api.saves.cloud && p.saves_folder == "7");
        assert_eq!(p.api.settings.args, ["-safe"]);
        let (p, a) = mk("Gameplay", &[&safe, &gameplay]);
        assert!(p.api.saves.separate && !a.share_default, "{:?}", p.api.saves);
        assert_eq!(a.reason, "gameplay (not save-compatible) could change saves, so it gets a store of its own");
        assert!(!p.api.saves.cloud && p.api.saves.cloud_note.is_some(), "only Default syncs");
        assert_eq!(p.api.saves.store_name.as_deref(), Some("Gameplay"));
        let sid = p.api.saves.store.clone().unwrap();
        assert_eq!(p.api.saves.folder, format!("savegames/<user>/7-{sid}"));
        let (p, a) = mk("Unknown", &[&unknown]);
        assert!(p.api.saves.separate && a.reason.contains("unknown save compatibility"), "unknown counts as not save-compatible");
        // Explicit choices win; sharing Default with a risky stack is allowed, with a warning.
        let (r, _) = store.create_profile_with("Risky", std::slice::from_ref(&gameplay.id), Some("default"), "cli", None).unwrap();
        assert_eq!(r.profile.saves.as_deref(), Some("default"));
        let info = store.state().unwrap().profiles.into_iter().find(|i| i.profile.name == "Risky").unwrap();
        assert!(info.warnings.iter().any(|w| w.contains("gameplay (not save-compatible) could change saves")), "{:?}", info.warnings);
        assert!(store.create_profile_with("X", &[], Some("nope"), "cli", None).unwrap_err().to_string().contains("no save store"));
        // Several profiles can share a store; a store is a plain setting.
        store.set_profile("Unknown", &ops(&["saves=Gameplay"]), "cli", None).unwrap();
        assert_eq!(store.plan(Some("Unknown")).unwrap().saves_folder, format!("7-{sid}"));
        let r = store.set_profile("Safe", &ops(&["saves=new"]), "cli", None).unwrap();
        let safe_store = r.profile.saves.clone().unwrap();
        assert_ne!(safe_store, "default");
        assert_eq!(store.save_stores().current().unwrap().get(&safe_store).unwrap().name, "Safe");
        // Changing the mods later warns while sharing with vanilla, but doesn't move it.
        store.set_profile("Empty", &ops(&[&format!("mods={}", unknown.id)]), "cli", None).unwrap();
        let p = store.plan(Some("Empty")).unwrap();
        assert!(p.saves_folder == "7" && p.api.warnings.iter().any(|w| w.contains("vanilla's saves")), "{:?}", p.api.warnings);
        // Old keys point at the new ones.
        assert!(store.set_profile("Safe", &ops(&["separate_saves=on"]), "cli", None).unwrap_err().to_string().contains("saves=new"));
        // A turned-off mod is skipped (and says so) and draws no save warning.
        store.set_mod("gameplay", &ops(&["enabled=off"]), "cli", None).unwrap();
        let p = store.plan(Some("Risky")).unwrap();
        assert!(p.api.mods.is_empty());
        assert!(p.api.warnings[0].contains("turned off") && p.api.warnings.len() == 1, "{:?}", p.api.warnings);
    }

    #[test]
    fn stacks_resolve_and_report_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let g = game(t, 8);
        folder(&g.install_dir, &[("game.exe", "e")]);
        std::fs::create_dir_all(&g.prefix).unwrap();
        std::fs::create_dir_all(t.join("data/games/8")).unwrap();
        std::fs::write(
            t.join("data/games/8/history.jsonl"),
            r#"{"revision":1,"at":"2026-11-16T00:00:00Z","source":"cli","settings":{"args":["-g"],"env":{"A":"game"}}}"#,
        )
        .unwrap();
        let store = ModStore::new(&t.join("data"), 8).with_game(&g, None);
        let pkg = |id: &str, body: &str| {
            let dir = t.join("pkgs").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("quiver-mod.toml"), format!("id = \"{id}\"\nname = \"{id}\"\nsave_compatible = true\n{body}")).unwrap();
            store.install(&Package::Dir { path: dir, name: None }, "cli").unwrap().layer.id
        };
        let a = pkg("a", "[launch]\nargs = [\"-a\"]\nenv = { A = \"a\" }\ndll = { d3d11 = \"n,b\" }\nwrappers = [\"gamemoderun\"]\n[[patch]]\nfile = \"drive_c/x.ini\"\nsection = \"S\"\nset = { K = \"1\" }\n");
        let b = pkg("b", "requires = [\"c\"]\nconflicts = [\"a\"]\n[launch]\nargs = [\"-b\"]\ndll = { d3d11 = \"b\" }\nwrappers = [\"gamemoderun\", \"mangohud\"]\n[[patch]]\nfile = \"drive_c/x.ini\"\nsection = \"s\"\ndelete = [\"k\"]\n");
        // Already installed: refused, not duplicated.
        assert!(store.install(&Package::Dir { path: t.join("pkgs/a"), name: None }, "cli").unwrap_err().to_string().contains("already installed"));
        let r = store.create_profile("P", &[a.clone(), "b".into()], "cli", None).unwrap();
        assert_eq!(r.profile.layers, [a.clone(), b.clone()], "mods by manifest id too");
        store.set_profile("P", &ops(&["args=-p", "env.A=profile"]), "cli", None).unwrap();
        let plan = store.plan(Some("P")).unwrap().api;
        assert_eq!(plan.settings.args, ["-g", "-a", "-b", "-p"]);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        store.set_profile("P", &ops(&["args=-p -a"]), "cli", None).unwrap();
        let w = store.plan(Some("P")).unwrap().api.warnings;
        assert_eq!(w, ["-a is passed twice: by a and by the profile"]);
        store.set_profile("P", &ops(&["args=-p"]), "cli", None).unwrap();
        let plan = store.plan(Some("P")).unwrap().api;
        assert_eq!(plan.settings.wrappers, ["gamemoderun", "mangohud"]);
        assert_eq!(plan.settings.env["A"], "profile");
        assert_eq!(plan.settings.dll_overrides["d3d11"], "b", "the later mod wins…");
        let keys: Vec<&str> = plan.setting_conflicts.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"dll.d3d11") && keys.contains(&"env.A"), "…and it's reported: {keys:?}");
        assert!(keys.iter().any(|k| k.contains("drive_c/x.ini") && k.ends_with('K')), "config keys too (any case): {keys:?}");
        // Declared requirements and conflicts block the launch.
        assert_eq!(plan.problems.len(), 2, "{:?}", plan.problems);
        assert!(plan.problems.iter().any(|p| p.contains("needs c")));
        assert!(plan.problems.iter().any(|p| p.contains("don't work together")));
        // The same report shows when editing the stack.
        let info = store.state().unwrap().profiles.remove(0);
        assert_eq!((info.problems.len(), info.setting_conflicts.len()), (2, plan.setting_conflicts.len()));
        // A missing mod is a problem too.
        std::fs::remove_file(store.layer_meta(&b)).unwrap();
        assert!(store.plan(Some("P")).unwrap().api.problems[0].contains("no longer installed"));
    }

    #[test]
    fn builtins_install_update_and_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let g = game(t, 3539);
        folder(&g.install_dir, &[("videos/UbisoftLogo.bk2", "v"), ("videos/en/Epilepsy.bk2", "v"), ("videos/en/pc_WarningSaving.bk2", "v")]);
        std::fs::create_dir_all(&g.prefix).unwrap();
        let store = ModStore::new(&t.join("data"), 3539).with_game(&g, Some(true));
        assert!(store.catalog().unwrap().iter().all(|e| e.installed.is_none()));
        let c = store.install(&Package::Builtin("aco-skip-intros".into()), "cli").unwrap();
        let l = c.layer.clone();
        assert!(l.builtin && l.kind() == ModKind::Fix && l.save_compatible());
        assert_eq!(l.deletes.len(), 4);
        assert_eq!(c.warnings.len(), 1, "warning_disclaimer.bk2 isn't in this install: {:?}", c.warnings);
        assert!(l.id.starts_with('m'));
        assert_eq!(store.catalog().unwrap()[0].installed.as_deref(), Some(l.id.as_str()));
        // Again: up to date. With an older manifest recorded: the catalog's becomes a new
        // version, current; the old one's folder and history stay as they were.
        assert!(store.install(&Package::Builtin("aco-skip-intros".into()), "cli").unwrap().warnings[0].contains("up to date"));
        let mut old = l.clone();
        old.manifest.as_mut().unwrap().version = Some("0".into());
        old.manifest.as_mut().unwrap().files.delete.pop();
        store.write_layer_meta(&old).unwrap();
        assert!(store.catalog().unwrap()[0].update);
        let u = store.install(&Package::Builtin("aco-skip-intros".into()), "cli").unwrap();
        assert_eq!(u.layer.manifest, l.manifest);
        assert!(u.layer.id.starts_with('v') && u.layer.builtin && u.layer.deletes.len() == 4);
        let (rec, hist) = store.mod_history_of(&l.id).unwrap();
        assert_eq!((rec.id.as_str(), rec.current.as_str(), rec.versions.len()), (l.id.as_str(), u.layer.id.as_str(), 2));
        assert_eq!(rec.versions.iter().map(|v| v.label.as_str()).collect::<Vec<_>>(), ["0", "1"]);
        assert!(hist.last().unwrap().changes.contains(&"current: 0 -> 1".to_owned()), "{:?}", hist.last().unwrap().changes);
        assert_eq!(store.layer_history(&l.id).unwrap().len(), 1, "the old version is untouched");
        assert!(!store.catalog().unwrap()[0].update);
        assert_eq!(store.catalog().unwrap()[0].installed.as_deref(), Some(l.id.as_str()), "the mod keeps its id");
        let l = u.layer.clone();
        // Wrong game: refused.
        assert!(store.install(&Package::Builtin("fc4-skip-intro".into()), "cli").unwrap_err().to_string().contains("no built-in mod"));

        // Fixes on vanilla: mounted over the install with vanilla's own writable layer.
        let v = store.plan(None).unwrap();
        assert_eq!(v.api.mods, [l.id.clone()]);
        assert_eq!(v.overlays[0].upper, store.profile_dir(VANILLA).join("upper"));
        assert_eq!(v.saves_folder, "3539");
        assert_eq!(store.state().unwrap().vanilla_fixes, [l.id.clone()]);
        assert!(store.detail(&l.id).unwrap().on_vanilla);
        // Not decided, off, or the fix turned off: vanilla is the game as installed.
        for s in [None, Some(false)] {
            let st = ModStore::new(&t.join("data"), 3539).with_game(&g, s);
            assert!(st.plan(None).unwrap().overlays.is_empty());
        }
        store.set_mod(&l.id, &ops(&["enabled=off"]), "cli", None).unwrap();
        assert!(store.plan(None).unwrap().api.mods.is_empty());
        // A fix must stay save-compatible.
        assert!(store.set_mod(&l.id, &ops(&["save_compatible=off"]), "cli", None).is_err());

        let p = store.create_profile("P", &["aco-skip-intros".into()], "cli", None).unwrap();
        assert!(store.uninstall("aco-skip-intros").unwrap_err().to_string().contains("used by P"));
        store.delete_profile(&p.profile.id, "cli").unwrap();
        let (gone, undone) = store.uninstall("aco-skip-intros").unwrap();
        assert_eq!((gone.id, undone.len()), (l.id.clone(), 0));
        assert!(store.layers().unwrap().is_empty(), "every version goes");
        assert!(store.mods().unwrap().is_empty());
    }

    #[test]
    fn profile_config_and_patches() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let g = game(t, 1651);
        folder(&g.install_dir, &[("game.exe", "e")]);
        let cfg = "drive_c/users/steamuser/Documents/My Games/UnrealEngine3/ACCGame/ACCGame/Config";
        let engine = format!("{cfg}/PCConsole-ACCEngine.ini");
        let system = format!("{cfg}/PCConsole-ACCSystemSettings.ini");
        let fixture = |n: &str| std::fs::read_to_string(format!("{}/fixtures/mods-v2/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        folder(&g.prefix, &[(&engine, &fixture("PCConsole-ACCEngine.ini")), (&system, &fixture("PCConsole-ACCSystemSettings.ini"))]);
        let store = ModStore::new(&t.join("data"), 1651).with_game(&g, Some(true));
        let skip = store.install(&Package::Builtin("acc-china-skip-intro".into()), "cli").unwrap().layer;
        let af = store.install(&Package::Builtin("acc-china-anisotropy-16".into()), "cli").unwrap().layer;
        assert_eq!(af.kind(), ModKind::Mod);

        // Folder checks: relative to the prefix (absolute inside it is fine), not the
        // prefix's machinery, no overlaps.
        let dir = "drive_c/users/steamuser/Documents/My Games/UnrealEngine3";
        assert_eq!(store.check_config_dirs(&[format!("{}/{dir}/", g.prefix.display())]).unwrap(), [dir]);
        for bad in ["drive_c", "/etc", "../x", "drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/savegames", "drive_c/windows/system32"] {
            assert!(store.check_config_dirs(&[bad.into()]).is_err(), "{bad}");
        }
        assert!(store.check_config_dirs(&[dir.into(), cfg.into()]).unwrap_err().to_string().contains("overlap"));

        // Vanilla, with fixes on: the intro skip edits the prefix's own file.
        let v = store.plan(None).unwrap();
        assert_eq!(v.api.mods, [skip.id.clone()]);
        assert!(v.overlays.is_empty(), "a config-only fix needs no mount");
        assert_eq!(v.api.patches[0].files[0].scope, "prefix");
        let st = v.apply_patches().unwrap();
        assert_eq!(st[0].state, PatchState::Applied);
        assert!(!std::fs::read_to_string(g.prefix.join(&engine)).unwrap().contains("StartupMovies="));
        assert_eq!(store.applied_by(&skip.id).unwrap().len(), 1);

        // A profile with its own copy of the game's settings folder, and the quality mod.
        store.create_profile("Pretty", &[af.id.clone()], "cli", None).unwrap();
        let r = store.set_profile("Pretty", &ops(&[&format!("profile_config={dir}")]), "cli", None).unwrap();
        assert_eq!(r.profile.profile_config, [dir]);
        let p = store.plan(Some("Pretty")).unwrap();
        let own = store.profile_prefix(&r.profile.id).join(dir);
        assert_eq!(p.api.config_dirs[0].upper, own);
        assert_eq!(p.overlays.len(), 2, "the install and the settings folder");
        assert_eq!((p.overlays[1].target.clone(), p.overlays[1].upper.clone()), (g.prefix.join(dir), own.clone()));
        assert!(p.overlays[1].lower.is_empty());
        assert_eq!(p.api.patches[0].files[0].scope, format!("profile:{}", r.profile.id));
        // The profile's copy of the system settings gets 16x AF (copied up from the
        // prefix's file first); the prefix's files don't change.
        let st = p.apply_patches().unwrap();
        assert_eq!(st[0].state, PatchState::Applied, "{st:#?}");
        assert!(std::fs::read_to_string(own.join("ACCGame/ACCGame/Config/PCConsole-ACCSystemSettings.ini")).unwrap().contains("MaxAnisotropy=16"));
        assert!(std::fs::read_to_string(g.prefix.join(&system)).unwrap().contains("MaxAnisotropy=4"));
        // Inspectable: which files are the profile's own, where they live.
        let info = store.state().unwrap().profiles.remove(0);
        assert_eq!(info.config_dirs[0].files, ["ACCGame/ACCGame/Config/PCConsole-ACCSystemSettings.ini"]);
        // The quality mod leaves the stack: the profile's copy gets its own value back.
        store.set_profile("Pretty", &ops(&["mods="]), "cli", None).unwrap();
        let st = store.plan(Some("Pretty")).unwrap().apply_patches().unwrap();
        assert!(st.iter().any(|s| s.state == PatchState::Undone));
        assert_eq!(
            std::fs::read_to_string(own.join("ACCGame/ACCGame/Config/PCConsole-ACCSystemSettings.ini")).unwrap(),
            fixture("PCConsole-ACCSystemSettings.ini")
        );
        // Undo all (as for the explicit request): vanilla's file is whole again.
        let undone = store.undo_patches(None).unwrap();
        assert_eq!(undone.len(), 1);
        assert_eq!(std::fs::read_to_string(g.prefix.join(&engine)).unwrap(), fixture("PCConsole-ACCEngine.ini"));
    }

    fn steam_game(t: &Path, app: u32, proton: bool) -> Game {
        Game {
            product_id: 0,
            name: "The Witcher 3".into(),
            install_dir: t.join("steamapps/common/The Witcher 3"),
            prefix: if proton { t.join(format!("steamapps/compatdata/{app}/pfx")) } else { PathBuf::new() },
            exe: String::new(),
            args: None,
            language: None,
            game: Some(quiver_api::GameRef::steam(app)),
        }
    }

    /// A Steam game's mods live under `steam-<appid>` folders (never a Ubisoft product's),
    /// run with the game's real saves, and a native game (no prefix) gets install files only.
    #[test]
    fn steam_games_have_stores_of_their_own() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let data = t.join("data");
        let g = steam_game(t, 292030, true);
        folder(&g.install_dir, &[("bin/x64/witcher3.exe", "e"), ("content/intro.usm", "intro")]);
        let ini = "drive_c/users/steamuser/Documents/The Witcher 3/user.settings";
        folder(&g.prefix, &[(ini, "[Visuals]\nMotionBlur=true\n")]);
        let pkg = folder(
            &t.join("pkg"),
            &[
                ("quiver-mod.toml", "id = \"w3-hud\"\nname = \"Better HUD\"\nsave_compatible = true\ngames = [\"steam:292030\"]\n[launch]\nargs = [\"-net\"]\n[[patch]]\nfile = \"drive_c/users/steamuser/Documents/The Witcher 3/user.settings\"\nsection = \"Visuals\"\nset = { MotionBlur = \"false\" }\n"),
                ("files/content/hud.bundle", "hud"),
            ],
        );
        let store = ModStore::new(&data, 0).with_game(&g, None);
        assert_eq!(store.game_ref(), quiver_api::GameRef::steam(292030));
        let l = store.install(&Package::Dir { path: pkg.clone(), name: None }, "cli").unwrap().layer;
        assert!(data.join(format!("mods/steam-292030/layers/{}/content/hud.bundle", l.id)).is_file());
        assert!(!data.join("mods/0").exists() && !data.join("games/0").exists(), "nothing under a Ubisoft-looking folder");
        // A manifest for another game is refused.
        let other = ModStore::new(&data, 0).with_game(&steam_game(t, 1, true), None);
        assert!(other.install(&Package::Dir { path: pkg, name: None }, "cli").unwrap_err().to_string().contains("not steam:1"));

        // A profile: the real saves, even with a saves-changing stack; `saves=new` refused.
        let r = store.create_profile_with("HUD", std::slice::from_ref(&l.id), None, "cli", None).unwrap();
        assert_eq!(r.0.profile.saves.as_deref(), Some(crate::savestores::DEFAULT_STORE));
        assert!(r.1.reason.contains("Steam Cloud"), "{}", r.1.reason);
        assert!(store.create_profile_with("Fork", &[], Some("new"), "cli", None).unwrap_err().to_string().contains("real saves"));
        assert!(data.join(format!("games/steam-292030/profiles/{}.jsonl", r.0.profile.id)).is_file());
        let p = store.plan(Some("HUD")).unwrap();
        assert!(p.api.saves.reason.contains("Steam Cloud") && !p.api.saves.separate);
        assert!(p.api.problems.is_empty(), "{:?}", p.api.problems);
        assert_eq!(p.api.settings.args, ["-net"], "the mod's launch settings");
        assert_eq!(p.overlays.len(), 1);
        assert_eq!(p.overlays[0].target, g.install_dir);
        assert_eq!(p.overlays[0].upper, data.join(format!("mods/steam-292030/profiles/{}/upper", r.0.profile.id)));
        // Its config patch goes to Steam's prefix, recorded the Quiver way.
        let st = p.apply_patches().unwrap();
        assert_eq!(st[0].state, PatchState::Applied, "{st:#?}");
        assert!(std::fs::read_to_string(g.prefix.join(ini)).unwrap().contains("MotionBlur=false"));
        assert!(data.join("mods/steam-292030/patches.json").is_file());
        let state = store.state().unwrap();
        assert_eq!((state.product_id, state.game_ref()), (0, quiver_api::GameRef::steam(292030)));
        assert!(state.save_stores.is_empty());
        assert_eq!(store.undo_patches(None).unwrap()[0].state, PatchState::Undone);
        assert!(std::fs::read_to_string(g.prefix.join(ini)).unwrap().contains("MotionBlur=true"));

        // A native game: install files mount, prefix patches and folders can't apply.
        let n = steam_game(&t.join("native"), 620, false);
        folder(&n.install_dir, &[("portal2.sh", "x")]);
        let native = ModStore::new(&data, 0).with_game(&n, None);
        let npkg = folder(&t.join("npkg"), &[("quiver-mod.toml", "id = \"cfg\"\nname = \"Cfg\"\n[[patch]]\nfile = \"drive_c/x.ini\"\nsection = \"S\"\nset = { a = \"1\" }\n"), ("files/cfg/autoexec.cfg", "x")]);
        let nl = native.install(&Package::Dir { path: npkg, name: None }, "cli").unwrap().layer;
        native.create_profile("P", std::slice::from_ref(&nl.id), "cli", None).unwrap();
        let p = native.plan(Some("P")).unwrap();
        assert!(p.patches.is_empty() && p.apply_patches().unwrap().is_empty());
        assert!(p.api.warnings.iter().any(|w| w.contains("no Wine prefix")), "{:?}", p.api.warnings);
        assert_eq!((p.overlays.len(), p.overlays[0].target.clone()), (1, n.install_dir.clone()));
        // Ubisoft games' folders are as they always were.
        let ubi = ModStore::new(&data, 3539);
        assert_eq!(ubi.profile_dir("p"), data.join("mods/3539/profiles/p"));
        assert_eq!(ubi.layer_history_path("l"), data.join("games/3539/layers/l.jsonl"));
        assert!(ubi.has_save_stores());
    }
}
