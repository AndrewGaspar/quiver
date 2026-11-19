//! Mods and profiles.
//!
//! **Mods v2** (design/MODS-V2.md): a **mod** is a package with a manifest
//! ([`ModManifest`], `quiver-mod.toml`) that can declare install files (added, replaced or
//! deleted, the layer mechanics below), launch settings (args, env, DLL overrides,
//! wrappers), config patches (INI/XML key edits on files in the prefix) and metadata
//! (version, source, save compatibility, known-good builds, requires and conflicts). An
//! installed mod is a [`Layer`] (the name is kept for the wire): a layer from before Mods v2
//! is a file-only mod with no manifest. A **profile** is an ordered stack of mods
//! (`Profile::layers`) plus the person's own overrides (`Profile::settings`); see
//! [`resolve_settings`] for how they combine and [`SettingConflict`] for what is reported.
//!
//! A **layer** is a directory of files that is overlaid on a game's install folder. A
//! **profile** is a named stack of layers (bottom to top) with optional launch-setting
//! overrides and a save store (see [`crate::savestores`]). Vanilla is implicit: no layers,
//! never mounted. Running a profile never changes the install folder:
//! the layers and a per-profile writable directory are mounted over it with overlayfs,
//! visible only to that launch's processes.
//!
//! Every profile change (including deleting it) is a revision in the profile's own
//! history, so any earlier state can be restored, the same way as game settings.
//!
//! A layer can also **delete** files and folders of the install (or of layers below it),
//! e.g. intro videos. Deletions are paths relative to the install folder, recorded in the
//! layer's metadata and kept on disk as overlayfs whiteouts in the layer's folder, so they
//! only hide files inside a launch's private mount; nothing is ever deleted from the
//! install. Deleting a folder the layer has files of its own in makes it an opaque folder:
//! the layer's version replaces the install's whole folder. Each change to a layer's
//! deletions is a revision in the layer's own history.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{GameSettings, SettingOp};

/// The fixed id (and reserved name) of the implicit unmodded profile.
pub const VANILLA: &str = "vanilla";

/// Sources that are the person at a front end: the CLI, the app, and the in-game Sight
/// panel (`sight`, which acts only on the person's own keys or controller). Anything else
/// is an agent, a script, MCP…
///
/// Game changes (mods, options, tweaks, save stores, save restores) don't depend on it:
/// every source's requests apply the same way (the user's decision, 2026-11-18; an agent
/// could pass `--yes` itself anyway). It decides who activity entries name, and the gates
/// that protect store accounts and other apps: Steam restarts, store sign-ins, account
/// writes such as the first Epic upload.
pub fn is_human_source(source: &str) -> bool {
    matches!(source, "cli" | "gui" | "sight")
}

#[cfg(test)]
mod human_tests {
    #[test]
    fn the_sight_panel_is_a_person() {
        assert!(super::is_human_source("sight") && super::is_human_source("gui") && super::is_human_source("cli"));
        assert!(!super::is_human_source("agent") && !super::is_human_source("mcp") && !super::is_human_source("in-game"));
    }
}

/// A directory of files overlaid on the install folder (stored under
/// `<data>/mods/<product>/layers/<id>/`, described by `<id>.json` next to it).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Layer {
    /// Stable and random, e.g. `l4n8d2k`.
    pub id: String,
    pub name: String,
    pub added: DateTime<Utc>,
    /// Who added it (request source).
    pub added_by: String,
    /// The folder it was copied from.
    #[serde(default)]
    pub origin: Option<PathBuf>,
    pub files: u64,
    pub bytes: u64,
    /// Paths (relative to the install folder, `/`-separated, in the install's case) this
    /// layer hides from the layers below it and the install.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deletes: Vec<String>,
    /// Mods v2: the mod's manifest as installed (launch settings, config patches,
    /// metadata). `None`: a layer from before Mods v2, i.e. a file-only mod with unknown
    /// save compatibility (treated as not save-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<ModManifest>,
    /// Installed from Quiver's built-in catalog (audited recipes shipped in the repo).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    /// Turned off: skipped wherever it's stacked (and not applied to vanilla); its config
    /// patches are undone at the next launch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// An installed mod: the same record as a layer (see the module docs).
pub type InstalledMod = Layer;

impl Layer {
    /// The manifest id (`fc4-skip-intro`), if it came from a package.
    pub fn mod_id(&self) -> Option<&str> {
        self.manifest.as_ref().map(|m| m.id.as_str()).filter(|s| !s.is_empty())
    }

    pub fn kind(&self) -> ModKind {
        self.manifest.as_ref().map(|m| m.kind).unwrap_or_default()
    }

    pub fn enabled(&self) -> bool {
        !self.disabled
    }

    /// Whether running this mod leaves saves usable by the unmodded game. Unknown (a layer
    /// from before Mods v2, or a manifest that doesn't say) counts as no.
    pub fn save_compatible(&self) -> bool {
        self.manifest.as_ref().and_then(|m| m.save_compatible).unwrap_or(false)
    }

    /// Whether `query` names this mod: its store id, manifest id or name (ignoring case).
    pub fn is(&self, query: &str) -> bool {
        let q = query.trim();
        self.id == q || self.mod_id().is_some_and(|m| m.eq_ignore_ascii_case(q)) || self.name.trim().eq_ignore_ascii_case(q)
    }

    /// Launch settings the mod adds (empty for file-only mods).
    pub fn launch(&self) -> ModLaunch {
        self.manifest.as_ref().map(|m| m.launch.clone()).unwrap_or_default()
    }

    pub fn patches(&self) -> &[ConfigPatch] {
        self.manifest.as_ref().map(|m| m.patches.as_slice()).unwrap_or_default()
    }

    /// Whether the mod changes the install folder (files or deletions).
    pub fn has_files(&self) -> bool {
        self.files > 0 || !self.deletes.is_empty()
    }
}

// -- Mod versions -------------------------------------------------------------------------

/// A mod: a stable identity (id, name, on or off) with one or more **versions**, one of
/// them current. Each version is a [`Layer`] of its own (its files folder, which never
/// changes once added, and its manifest), so `ModsState::layers` lists every version.
///
/// A layer from before versions is a mod with one version: the mod's id *is* that layer's
/// id, and nothing about it is stored until it gets a second version (see
/// [`ModRecord::implicit`]). From then on the record lives in
/// `games/<product>/mods/<id>.jsonl`, one whole-state [`ModRevision`] per change.
///
/// A profile's stack entry is `MOD` (tracks the mod's current version, so a new version
/// reaches every such profile at its next launch) or `MOD@VERSION` (pinned; version ids are
/// stored, labels are resolved when the entry is set). An entry that is a version id on its
/// own (as written before versions, or by a merge) is pinned to that version.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModRecord {
    /// Stable: the id of the layer the mod started as.
    pub id: String,
    pub name: String,
    /// The version (a layer id) every profile that tracks the mod runs.
    pub current: String,
    /// Oldest first.
    #[serde(default)]
    pub versions: Vec<ModVersion>,
    /// Turned off: skipped wherever it's stacked (whatever the version).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// One version of a mod. Its files and manifest are the [`Layer`] with the same id.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModVersion {
    /// The layer id (its folder is `mods/<product>/layers/<id>/`).
    pub id: String,
    /// Free text, unique within the mod: `3.8.10`, `v3`.
    pub label: String,
    pub added: DateTime<Utc>,
    pub added_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Hidden from defaults (can't be current); its files are kept and pins to it still work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retired: bool,
    /// Added without becoming current, for profiles to pin and try first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
}

/// One entry in a mod's history: the whole record from then on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModRevision {
    pub revision: u32,
    pub at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub message: Option<String>,
    /// What changed, in words (`added v4 (vk2m9xq)`, `current: v3 -> v4`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    /// The mod was merged into another one (or removed); its versions belong elsewhere now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    #[serde(rename = "mod")]
    pub record: ModRecord,
}

impl ModRecord {
    /// The record a layer that was never versioned stands for: one version, current.
    pub fn implicit(l: &Layer) -> ModRecord {
        let label = l.manifest.as_ref().and_then(|m| m.version.clone()).filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "v1".into());
        ModRecord {
            id: l.id.clone(),
            name: l.name.clone(),
            current: l.id.clone(),
            versions: vec![ModVersion { id: l.id.clone(), label, added: l.added, added_by: l.added_by.clone(), ..Default::default() }],
            disabled: l.disabled,
        }
    }

    /// A version by id or label (labels ignore case).
    pub fn version(&self, query: &str) -> Option<&ModVersion> {
        let q = query.trim();
        self.versions.iter().find(|v| v.id == q).or_else(|| self.versions.iter().find(|v| v.label.eq_ignore_ascii_case(q)))
    }

    pub fn has(&self, version: &str) -> bool {
        self.versions.iter().any(|v| v.id == version)
    }

    pub fn current_version(&self) -> Option<&ModVersion> {
        self.versions.iter().find(|v| v.id == self.current)
    }

    /// The version's label, or its id if it isn't one of this mod's.
    pub fn label_of(&self, version: &str) -> String {
        self.versions.iter().find(|v| v.id == version).map_or_else(|| version.to_owned(), |v| v.label.clone())
    }

    /// "DLSS v3", or just "DLSS" while the mod has a single version.
    pub fn display(&self, version: &str) -> String {
        if self.versions.len() > 1 { format!("{} {}", self.name, self.label_of(version)) } else { self.name.clone() }
    }

    /// Versions a person would pick from: not retired, newest first.
    pub fn live_versions(&self) -> impl Iterator<Item = &ModVersion> {
        self.versions.iter().rev().filter(|v| !v.retired)
    }
}

/// `MOD@VERSION` -> (`MOD`, `Some(VERSION)`); `MOD` -> (`MOD`, `None`).
pub fn split_entry(entry: &str) -> (&str, Option<&str>) {
    match entry.split_once('@') {
        Some((m, v)) => (m.trim(), Some(v.trim()).filter(|v| !v.is_empty())),
        None => (entry.trim(), None),
    }
}

/// A stack entry: `MOD` (tracks current) or `MOD@VERSION` (pinned).
pub fn stack_entry(mod_id: &str, pinned: Option<&str>) -> String {
    match pinned {
        Some(v) => format!("{mod_id}@{v}"),
        None => mod_id.to_owned(),
    }
}

/// A profile's stack entry resolved to the version a launch runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResolvedMod {
    /// As stored in the profile.
    #[serde(default)]
    pub entry: String,
    #[serde(rename = "mod")]
    pub mod_id: String,
    pub name: String,
    /// The version (layer id) and its label.
    pub version: String,
    pub label: String,
    /// Pinned to this version (else it tracks the mod's current one).
    #[serde(default)]
    pub pinned: bool,
    /// The version is the mod's current one.
    #[serde(default)]
    pub current: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retired: bool,
    /// How many versions the mod has (labels are shown when there's more than one).
    #[serde(default)]
    pub versions: usize,
}

impl ResolvedMod {
    /// "DLSS v3 (current)", "DLSS v2 (pinned; current is v3)", "Skip intro videos".
    pub fn describe(&self) -> String {
        if self.versions <= 1 && !self.pinned {
            return self.name.clone();
        }
        let how = match (self.pinned, self.current) {
            (false, _) => "current".to_owned(),
            (true, true) => "pinned, current".to_owned(),
            (true, false) => "pinned".to_owned(),
        };
        format!("{} {} ({how}{})", self.name, self.label, if self.retired { ", retired" } else { "" })
    }
}

/// A game's mods: the stored records plus a one-version mod for every layer no record
/// claims, ordered by their first version's time.
pub fn mods_of(layers: &[Layer], records: &[ModRecord]) -> Vec<ModRecord> {
    let mut out: Vec<ModRecord> = records.to_vec();
    for l in layers {
        if !records.iter().any(|r| r.has(&l.id)) && !records.iter().any(|r| r.id == l.id) {
            out.push(ModRecord::implicit(l));
        }
    }
    let first = |r: &ModRecord| r.versions.iter().map(|v| v.added).min();
    out.sort_by(|a, b| first(a).cmp(&first(b)).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Resolve a stored stack entry against a game's mods ([`mods_of`]). `None`: the mod or the
/// version is gone.
pub fn resolve_entry(mods: &[ModRecord], entry: &str) -> Option<ResolvedMod> {
    let (m, v) = split_entry(entry);
    let (rec, version, pinned) = match mods.iter().find(|r| r.id == m) {
        Some(r) => match v {
            Some(v) => (r, r.version(v)?, true),
            None => (r, r.current_version()?, false),
        },
        // A version id on its own: pinned to it.
        None if v.is_none() => {
            let r = mods.iter().find(|r| r.has(m))?;
            (r, r.versions.iter().find(|x| x.id == m)?, true)
        }
        None => return None,
    };
    Some(ResolvedMod {
        entry: entry.to_owned(),
        mod_id: rec.id.clone(),
        name: rec.name.clone(),
        version: version.id.clone(),
        label: version.label.clone(),
        pinned,
        current: version.id == rec.current,
        retired: version.retired,
        versions: rec.versions.len(),
    })
}

// -- Mods v2 manifest ---------------------------------------------------------------------

/// The manifest file in a mod package's folder. TOML; `quiver-mod.json` (same fields) is
/// read too.
pub const MANIFEST_FILE: &str = "quiver-mod.toml";
pub const MANIFEST_FILE_JSON: &str = "quiver-mod.json";

/// What kind of change a mod is. Only `fix` mods can apply to the unmodded game (with the
/// app-level `fixes_on_vanilla` setting).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModKind {
    /// Changes the game (the default).
    #[default]
    Mod,
    /// A fix or quality-of-life change (intro skips, mouse settings): save-compatible and
    /// safe to apply to vanilla.
    Fix,
}

impl ModKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModKind::Mod => "mod",
            ModKind::Fix => "fix",
        }
    }
}

/// A mod package's manifest (`quiver-mod.toml`):
///
/// ```toml
/// id = "fc4-raw-mouse"
/// name = "Raw mouse input"
/// version = "1"
/// kind = "fix"
/// source = "https://www.pcgamingwiki.com/wiki/Far_Cry_4"
/// save_compatible = true
///
/// [launch]
/// args = ["-skipintro"]
///
/// [files]
/// delete = ["videos/intro.bk2"]   # relative to the install folder; files/ holds added files
///
/// [[patch]]
/// file = "drive_c/users/steamuser/Documents/My Games/Far Cry 4/*/GamerProfile.xml"
/// format = "xml"
/// element = "GamerProfile/ProfileSpecificGameProfile"
/// set = { MouseAcceleration = "0" }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModManifest {
    /// Stable package id: lowercase letters, digits and `-` (e.g. `fc4-skip-intro`).
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub kind: ModKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the recipe or files come from (a link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// For built-in mods: who checked the recipe, when, and against what.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audited: Option<String>,
    /// Saves made with this mod work without it (rendering, intro skips, input). `None`
    /// (not said) counts as no: a profile with the mod gets its own saves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_compatible: Option<bool>,
    /// Products the mod is for (empty: not checked).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub products: Vec<u32>,
    /// Games of other stores the mod is for (`steam:292030`; appended). A mod naming
    /// neither products nor games isn't checked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub games: Vec<crate::GameRef>,
    /// Game builds (install manifest hashes) the mod is known to work with (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builds: Vec<String>,
    /// Mods (manifest ids) that must be in the same stack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Mods (manifest ids) that must not be in the same stack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(default, skip_serializing_if = "ModFiles::is_empty")]
    pub files: ModFiles,
    #[serde(default, skip_serializing_if = "ModLaunch::is_empty")]
    pub launch: ModLaunch,
    #[serde(default, rename = "patch", alias = "patches", skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<ConfigPatch>,
}

/// The install-files part of a mod.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModFiles {
    /// Folder in the package laid out like the install folder (default `files`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Paths relative to the install folder to hide (overlayfs whiteouts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete: Vec<String>,
}

impl ModFiles {
    pub fn is_empty(&self) -> bool {
        self.dir.is_none() && self.delete.is_empty()
    }
}

/// Launch settings a mod adds. Args and wrappers are appended in stack order; env and
/// DLL overrides merge, later mods winning (and the clash is reported).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModLaunch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Wine DLL overrides (`d3d11 = "n,b"`).
    #[serde(default, alias = "dll_overrides", skip_serializing_if = "BTreeMap::is_empty")]
    pub dll: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrappers: Vec<String>,
}

impl ModLaunch {
    pub fn is_empty(&self) -> bool {
        *self == ModLaunch::default()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PatchFormat {
    #[default]
    Ini,
    Xml,
}

/// Structured key edits to one file the game keeps in its prefix. Applied at each launch
/// (ensure key = value), undone when the mod leaves the stack; the file's own values from
/// before Quiver touched it are recorded for that.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    /// Relative to the prefix, `/`-separated. A component may be `*` (any one name), e.g.
    /// a per-user folder: every match is patched.
    pub file: String,
    #[serde(default)]
    pub format: PatchFormat,
    /// INI: the section (without brackets; empty: keys before the first section). Names
    /// and keys match without case.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub section: String,
    /// XML: the element, as a path of element names from the root (`Root/Child`). The
    /// first match is edited; keys are its attributes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub element: String,
    /// Keys (attributes) to set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Keys (attributes) to remove; an INI key given several times loses every line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete: Vec<String>,
}

impl ConfigPatch {
    /// The section or element the patch edits.
    pub fn target(&self) -> &str {
        match self.format {
            PatchFormat::Ini => &self.section,
            PatchFormat::Xml => &self.element,
        }
    }
}

/// A layer after a change, with the revision of its deletions and anything worth telling
/// the person (e.g. a deleted path that isn't in the install).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerChange {
    #[serde(flatten)]
    pub layer: Layer,
    /// Revision of the layer's deletions (its history's last entry).
    #[serde(default)]
    pub revision: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One entry in a layer's history: the deletions it had from then on. A layer's files
/// never change after it's added; its deletions can.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayerRevision {
    pub revision: u32,
    pub at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub deletes: Vec<String>,
    /// Mods v2: other changes in this revision (`save_compatible=on`, `enabled=off`,
    /// `manifest version 1 -> 2`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
}

/// A named stack of mods (bottom to top) plus the person's overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// Stable and random, e.g. `p7f3k2q`; never derived from the name, never changes.
    pub id: String,
    pub name: String,
    /// Layer ids, bottom to top (a file in a higher layer wins).
    #[serde(default)]
    pub layers: Vec<String>,
    /// Launch-setting overrides applied on top of the game's settings.
    #[serde(default, skip_serializing_if = "is_default")]
    pub settings: GameSettings,
    /// Before save stores: sync this profile's own saves with the cloud. Ignored once
    /// `saves` is set (cloud sync is per store now, see [`crate::savestores`]); kept so
    /// older revisions read as they were.
    #[serde(default)]
    pub cloud_saves: bool,
    /// Shown as its own tile and exported like a game.
    #[serde(default)]
    pub pinned: bool,
    /// Mods v2 before save stores: `Some(true)` gave the profile its own saves
    /// (`savegames/<user>/<product>-<id>`), `Some(false)` shared vanilla's, `None` (auto)
    /// forked only if a mod in the stack wasn't save-compatible. Ignored once `saves` is
    /// set; migration turns it into a store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separate_saves: Option<bool>,
    /// Prefix folders (relative to the prefix) this profile has its own copy of: overlaid
    /// inside the launch with a per-profile upper folder, e.g. the game's settings folder
    /// under `Documents/My Games`, so the profile keeps its own graphics settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile_config: Vec<String>,
    /// The save store the profile uses (a [`crate::savestores::SaveStore`] id), chosen
    /// independently of its mods. `None`: from before save stores, not migrated yet (the
    /// old `separate_saves` rule applies until then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saves: Option<String>,
}

fn is_default(s: &GameSettings) -> bool {
    *s == GameSettings::default()
}

fn parse_bool(key: &str, v: Option<&str>) -> Result<bool, String> {
    match v.map(str::to_ascii_lowercase).as_deref() {
        Some("on" | "true" | "yes" | "1") => Ok(true),
        Some("off" | "false" | "no" | "0") | None => Ok(false),
        Some(other) => Err(format!("{key}: expected on or off, got '{other}'")),
    }
}

impl Profile {
    /// Apply one edit. Keys: `name`, `mods` (or `layers`: comma-separated mod ids, bottom
    /// to top; the daemon resolves names), `pinned=on|off`, `saves=STORE` (a save store's
    /// id; the daemon resolves names, `default` and `new`), `profile_config=DIR,…`
    /// (prefix-relative; the daemon normalizes them); any other key is a launch-setting
    /// override (`proton`, `env.NAME`, `dll.NAME`, `args`, `wrappers`, `language`).
    /// `account` is per game; `cloud_saves` and `separate_saves` are replaced by stores.
    pub fn apply(&mut self, op: &SettingOp) -> Result<(), String> {
        let (key, value) = match op {
            SettingOp::Set { key, value } => (key.as_str(), Some(value.as_str())),
            SettingOp::Unset { key } => (key.as_str(), None),
        };
        match key {
            "name" => {
                let name = value.map(str::trim).unwrap_or_default();
                if name.is_empty() {
                    return Err("a profile needs a name".into());
                }
                self.name = name.to_owned();
            }
            "layers" | "mods" => self.layers = split_list(value.unwrap_or_default()),
            "pinned" => self.pinned = parse_bool(key, value)?,
            "saves" => match value.map(str::trim).filter(|v| !v.is_empty()) {
                Some(v) => self.saves = Some(v.to_owned()),
                None => return Err("saves: name a save store (default, new, or a store's name or id)".into()),
            },
            "cloud_saves" => {
                return Err("cloud sync is set per save store now: `quiver saves <game> store cloud STORE` (one store per game syncs)".into());
            }
            "separate_saves" => {
                return Err("separate_saves is replaced by save stores: saves=default shares the Default store, saves=new gives the profile its own".into());
            }
            "profile_config" => self.profile_config = split_list(value.unwrap_or_default()),
            "account" => return Err("the launch account is set per game (`quiver set <game> account=…`)".into()),
            "profile" => return Err("the default profile is set per game (`quiver mods <game> default PROFILE`)".into()),
            _ => self.settings.apply(op).map_err(|e| {
                format!("{e}; profiles also take name, mods, saves, pinned, profile_config")
            })?,
        }
        Ok(())
    }
}

/// `a,b, c` -> `[a, b, c]`.
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// One entry in a profile's history. A deletion is a revision too (`deleted`), so a
/// deleted profile can be restored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRevision {
    pub revision: u32,
    pub at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    pub profile: Profile,
}

/// A file more than one of a profile's layers provides or deletes; the last layer listed
/// wins. A layer in `deleted_by` hides the file (it deletes the path or a folder above
/// it) instead of providing its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Conflict {
    pub path: String,
    pub layers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_by: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub profile: Profile,
    pub revision: u32,
    /// Layer ids the profile names that are no longer in the store.
    #[serde(default)]
    pub missing_layers: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    /// Mods v2: launch settings or config keys that sources set differently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setting_conflicts: Vec<SettingConflict>,
    /// Problems that stop a launch: declared conflicts between mods, missing requirements,
    /// missing mods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// Worth knowing but not blocking: disabled mods, builds a mod wasn't checked against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Whose saves the profile uses, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saves: Option<SavesPlan>,
    /// Prefix folders the profile has its own copy of, and where their files really live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_dirs: Vec<ConfigDir>,
    /// Mod versions: each stack entry and the version it runs now (missing ones left out).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<ResolvedMod>,
}

/// A game's layers and profiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModsState {
    pub product_id: u32,
    pub layers: Vec<Layer>,
    pub profiles: Vec<ProfileInfo>,
    /// Deleted profiles (restorable from their history).
    #[serde(default)]
    pub deleted: Vec<ProfileInfo>,
    /// Mods v2: the app-level setting (`None`: not decided yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes_on_vanilla: Option<bool>,
    /// Installed, enabled `fix` mods (ids) that apply to vanilla launches (empty unless
    /// `fixes_on_vanilla` is on).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vanilla_fixes: Vec<String>,
    /// The game's default profile: its `profile` setting, the id Play uses when no profile
    /// is named. `None`: vanilla. It can name a deleted profile (launches then run vanilla,
    /// with a warning), so check it against `profiles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// The game's save stores (retired ones included; see [`crate::savestores`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub save_stores: Vec<crate::savestores::SaveStore>,
    /// Whose saves vanilla launches use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vanilla_saves: Option<SavesPlan>,
    /// Mod versions: every mod with its versions (each a layer in `layers`) and the current
    /// one. Empty from daemons before versions: see [`ModsState::all_mods`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<ModRecord>,
    /// Another store's game (`steam:620`; `product_id` is then 0). Appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<crate::GameRef>,
    /// Who syncs the game's saves with a cloud (`None`: not known, or an older daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_sync: Option<crate::savestores::SaveSync>,
}

impl ModsState {
    /// The game the state is for.
    pub fn game_ref(&self) -> crate::GameRef {
        self.game.unwrap_or(crate::GameRef::ubisoft(self.product_id))
    }

    /// Who syncs the game's saves: [`ModsState::save_sync`], else Ubisoft Connect's cloud
    /// for a Ubisoft game (older daemons), else not known.
    pub fn saves_sync(&self) -> Option<crate::savestores::SaveSync> {
        self.save_sync.or_else(|| self.game_ref().is_ubisoft().then_some(crate::savestores::SaveSync::Ubisoft))
    }

    /// Whether the game's profiles pick a save store (Ubisoft games); other stores' games
    /// use their real saves for every profile.
    pub fn has_save_stores(&self) -> bool {
        self.game_ref().is_ubisoft()
    }

    /// Every mod, one-version mods included (also for a state without `mods`).
    pub fn all_mods(&self) -> Vec<ModRecord> {
        mods_of(&self.layers, &self.mods)
    }

    /// The mod a mod or version id belongs to.
    pub fn mod_of(&self, id: &str) -> Option<ModRecord> {
        let all = self.all_mods();
        let found = all.iter().find(|r| r.id == id).or_else(|| all.iter().find(|r| r.has(id)));
        found.cloned()
    }

    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// A mod or version id in words: "DLSS" for a mod, "DLSS v2" for one of its versions
    /// (just the name while it has one version), the id itself if it's unknown.
    pub fn display(&self, id: &str) -> String {
        match self.mod_of(id) {
            Some(r) if r.id == id => r.name.clone(),
            Some(r) => r.display(id),
            None => id.to_owned(),
        }
    }

    /// A stack entry resolved to the version it runs now.
    pub fn resolve(&self, entry: &str) -> Option<ResolvedMod> {
        resolve_entry(&self.all_mods(), entry)
    }

    /// The versions a stack runs (enabled mods only, as a launch applies them), each named
    /// after its mod ("DLSS v3"), for save advice and the like.
    pub fn stack_layers(&self, entries: &[String]) -> Vec<Layer> {
        let all = self.all_mods();
        entries
            .iter()
            .filter_map(|e| resolve_entry(&all, e))
            .filter(|r| all.iter().any(|m| m.id == r.mod_id && !m.disabled))
            .filter_map(|r| {
                let mut l = self.layer(&r.version)?.clone();
                l.name = all.iter().find(|m| m.id == r.mod_id).map_or(l.name, |m| m.display(&r.version));
                l.disabled = false;
                Some(l)
            })
            .collect()
    }

    /// Live profiles whose stack has the mod `mod_id`, with how (tracking or pinned).
    pub fn users_of(&self, mod_id: &str) -> Vec<(&ProfileInfo, ResolvedMod)> {
        let all = self.all_mods();
        self.profiles
            .iter()
            .filter_map(|p| p.profile.layers.iter().filter_map(|e| resolve_entry(&all, e)).find(|r| r.mod_id == mod_id).map(|r| (p, r)))
            .collect()
    }
}

// -- Mods v2: resolution, saves, config ---------------------------------------------------

/// Where a value comes from: `game` (the game's settings), a mod's id, or `profile`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ValueFrom {
    pub from: String,
    /// `None`: the source deletes the key (config patches).
    #[serde(default)]
    pub value: Option<String>,
}

/// A setting or config key that sources set differently. The last one listed wins; the
/// conflict is reported, never resolved silently.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SettingConflict {
    /// `env.NAME`, `dll.NAME`, or `<file> [section] key` / `<file> <element>@attribute`.
    pub key: String,
    pub by: Vec<ValueFrom>,
}

/// Which saves a launch uses.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SavesPlan {
    /// Not the Default store's folder (`savegames/<user>/<product>`).
    pub separate: bool,
    /// Why, in words.
    pub reason: String,
    /// Relative to the Connect folder in the prefix, with `<user>` for the account id.
    pub folder: String,
    /// Whether the launch syncs these saves with the cloud.
    pub cloud: bool,
    /// The save store's id and name (`None`: a profile from before save stores, not
    /// migrated yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_name: Option<String>,
    /// Said when the store doesn't sync with the cloud: which one does, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_note: Option<String>,
}

/// A prefix folder a profile has its own copy of.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConfigDir {
    /// Relative to the prefix.
    pub dir: String,
    /// The prefix's folder: what the profile sees there until it changes a file.
    #[serde(default)]
    pub prefix_path: Option<PathBuf>,
    /// Where the profile's own copies of changed files really live.
    pub upper: PathBuf,
    /// Files the profile has its own copy of (relative to `dir`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PatchState {
    /// The file now has the patched values.
    Applied,
    /// It had them already.
    Unchanged,
    /// The file doesn't exist yet: applied on the first launch after the game writes it.
    Pending,
    /// The mod left the stack (or was disabled or removed): the file's own values are back.
    Undone,
    Failed,
    /// Not checked yet (shown before a launch).
    #[default]
    #[serde(other)]
    Unknown,
}

/// A config patch's state for one launch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PatchStatus {
    /// Mod (store id).
    #[serde(rename = "mod")]
    pub mod_id: String,
    /// The patch's file pattern (relative to the prefix).
    pub file: String,
    /// Section or element.
    pub target: String,
    pub state: PatchState,
    /// The files as the game sees them and where they really are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PatchedFile>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PatchedFile {
    /// Relative to the prefix.
    pub rel: String,
    /// The real file on the host.
    pub path: PathBuf,
    /// `prefix`, or `profile:<id>` for a file in one of the profile's own folders.
    pub scope: String,
}

/// Everything a launch of a game with a profile (or vanilla) uses (`ModsRequest::Plan`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub product_id: u32,
    /// `None`: vanilla.
    #[serde(default)]
    pub profile: Option<Profile>,
    #[serde(default)]
    pub revision: u32,
    /// Mods applied (store ids), bottom to top.
    pub mods: Vec<String>,
    /// The launch settings after defaults, the game's settings, each mod and the profile.
    pub settings: GameSettings,
    #[serde(default)]
    pub setting_conflicts: Vec<SettingConflict>,
    #[serde(default)]
    pub file_conflicts: Vec<Conflict>,
    #[serde(default)]
    pub problems: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub saves: SavesPlan,
    /// Overlay mounts in the launch's private namespace.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub config_dirs: Vec<ConfigDir>,
    #[serde(default)]
    pub patches: Vec<PatchStatus>,
    /// Mod versions: each mod applied (bottom to top) and the version it runs, e.g.
    /// "DLSS v3 (current)". `mods` lists the same versions by layer id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<ResolvedMod>,
}

/// One overlay mount: `lower` (topmost first) over `target`, writes going to `upper`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Mount {
    pub target: PathBuf,
    pub lower: Vec<PathBuf>,
    pub upper: PathBuf,
}

/// A built-in mod in Quiver's catalog for a game.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub manifest: ModManifest,
    /// Store id, if installed for this game.
    #[serde(default)]
    pub installed: Option<String>,
    /// Installed from a different manifest than the catalog's (install again to update).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update: bool,
}

/// An installed mod in detail (`ModsRequest::Show`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModDetail {
    #[serde(rename = "mod")]
    pub layer: Layer,
    /// Profiles that stack it (names).
    pub used_by: Vec<String>,
    /// It applies to vanilla launches (an enabled fix, with `fixes_on_vanilla` on).
    #[serde(default)]
    pub on_vanilla: bool,
    /// Config keys it has changed in files right now, from the patch record.
    #[serde(default)]
    pub applied: Vec<AppliedKey>,
    /// The catalog's manifest, when it differs from the installed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_update: Option<ModManifest>,
    /// Mod versions: the mod (`layer` is the version shown: its current one unless a
    /// version was asked for) and how each profile in `used_by` runs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<ModRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_as: Vec<String>,
}

/// One config key Quiver has changed, with the file's own values from before.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppliedKey {
    /// `prefix` or `profile:<id>`.
    pub scope: String,
    /// Relative to the prefix.
    pub file: String,
    pub target: String,
    pub key: String,
    /// The file's values before Quiver changed it (empty: the key wasn't there).
    pub original: Vec<String>,
    /// `None`: deleted.
    pub applied: Option<String>,
    /// Mods (store ids) that want it.
    pub mods: Vec<String>,
}

/// App-level mod preferences (`ModsRequest::Prefs`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModPrefs {
    /// Apply installed, enabled built-in `fix` mods to vanilla launches too. `None`: not
    /// decided yet (a first-launch prompt can ask).
    #[serde(default)]
    pub fixes_on_vanilla: Option<bool>,
}

/// What `ModsRequest::Migrate` did (or, with `dry_run`, would do).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    pub product_id: u32,
    pub dry_run: bool,
    /// Layers that are now file-only mods (nothing changes on disk).
    pub file_only_mods: Vec<String>,
    /// Profiles given `separate_saves=on` because they already have a saves folder of
    /// their own: (profile id, folder relative to the prefix).
    pub kept_separate_saves: Vec<(String, String)>,
    pub notes: Vec<String>,
    /// Save stores: profiles given a store (profile id, store id), and stores created for
    /// profiles' own folders (ids).
    #[serde(default)]
    pub profile_stores: Vec<(String, String)>,
    #[serde(default)]
    pub stores_created: Vec<String>,
    /// Built-in mods given the catalog's newer manifest as a new current version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_updates: Vec<String>,
}

/// A mod after a version change (`VersionAdd`, `VersionUse`, `VersionRetire`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionChange {
    #[serde(rename = "mod")]
    pub record: ModRecord,
    /// The version the change was about.
    pub version: Layer,
    /// The mod's history revision (0: nothing changed).
    pub revision: u32,
    /// Profiles that track the mod's current version (they run it from their next launch).
    #[serde(default)]
    pub tracking: Vec<String>,
    /// Profiles pinned to a version: "Old run (v1)".
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Worth telling the person: a version that isn't save-compatible reaching a profile
    /// that shares vanilla's or the Default saves, and the like.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What `ModsRequest::MergeVersions` did (or, with `dry_run`, would do).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionMerge {
    pub dry_run: bool,
    /// The merged mod.
    #[serde(rename = "mod")]
    pub record: ModRecord,
    /// Profile stacks rewritten: (profile name, entry before, entry after, in words).
    #[serde(default)]
    pub profiles: Vec<(String, String, String, String)>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Settings for a launch: `base` (defaults and the game's settings), then each mod's
/// launch settings in stack order, then the profile's overrides. Args and wrappers are
/// appended in that order (a wrapper already present isn't added twice); env and DLL
/// overrides merge, the later source winning; the profile's `proton` and `language` win.
/// Every env or DLL key that sources set to different values comes back as a conflict.
/// The game's default `profile` isn't a launch setting, so it's left out.
pub fn resolve_settings(base: &GameSettings, mods: &[(String, ModLaunch)], over: &GameSettings) -> (GameSettings, Vec<SettingConflict>) {
    let mut out = GameSettings { profile: None, ..base.clone() };
    let mut seen: BTreeMap<String, Vec<ValueFrom>> = BTreeMap::new();
    let mut note = |key: String, from: &str, value: &str| {
        seen.entry(key).or_default().push(ValueFrom { from: from.to_owned(), value: Some(value.to_owned()) });
    };
    for (k, v) in &base.env {
        note(format!("env.{k}"), "game", v);
    }
    for (k, v) in &base.dll_overrides {
        note(format!("dll.{k}"), "game", v);
    }
    fn add_wrappers(out: &mut GameSettings, list: &[String]) {
        for w in list {
            if !out.wrappers.contains(w) {
                out.wrappers.push(w.clone());
            }
        }
    }
    for (id, m) in mods {
        out.args.extend(m.args.iter().cloned());
        add_wrappers(&mut out, &m.wrappers);
        for (k, v) in &m.env {
            note(format!("env.{k}"), id, v);
            out.env.insert(k.clone(), v.clone());
        }
        for (k, v) in &m.dll {
            note(format!("dll.{k}"), id, v);
            out.dll_overrides.insert(k.clone(), v.clone());
        }
    }
    if over.proton.is_some() {
        out.proton = over.proton.clone();
    }
    if over.language.is_some() {
        out.language = over.language.clone();
    }
    out.args.extend(over.args.iter().cloned());
    add_wrappers(&mut out, &over.wrappers);
    for (k, v) in &over.env {
        note(format!("env.{k}"), "profile", v);
        out.env.insert(k.clone(), v.clone());
    }
    for (k, v) in &over.dll_overrides {
        note(format!("dll.{k}"), "profile", v);
        out.dll_overrides.insert(k.clone(), v.clone());
    }
    let conflicts = seen
        .into_iter()
        .filter(|(_, by)| by.iter().any(|b| b.value != by[0].value))
        .map(|(key, by)| SettingConflict { key, by })
        .collect();
    (out, conflicts)
}

/// An installed game and its (live) profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameProfiles {
    pub product_id: u32,
    pub name: String,
    pub profiles: Vec<Profile>,
    /// Another store's game (`product_id` is then 0; appended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<crate::GameRef>,
}

impl GameProfiles {
    pub fn game_ref(&self) -> crate::GameRef {
        self.game.unwrap_or(crate::GameRef::ubisoft(self.product_id))
    }
}

/// Requests about mods (`Request::Mods`). Games are product ids; layers and profiles are
/// named by id or (case-insensitive) name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ModsRequest {
    /// Installed games and their profiles (`Vec<GameProfiles>`); no Ubisoft request.
    Games,
    /// A game's layers and profiles (`ModsState`).
    List { #[serde(flatten)] game: crate::game::GameField },
    /// Copy a folder into the store as a new layer (reflinks where the filesystem allows).
    /// File names are matched to the install's case. An empty `path` adds a layer with
    /// only `deletes` (paths relative to the install folder to hide); whiteouts already in
    /// the folder are kept as deletions. Answers a [`LayerChange`].
    LayerAdd {
        #[serde(flatten)]
        game: crate::game::GameField,
        name: String,
        path: PathBuf,
        #[serde(default)]
        deletes: Vec<String>,
    },
    /// Delete a layer's files. Refused while a profile uses it.
    LayerRemove {
        #[serde(flatten)]
        game: crate::game::GameField,
        layer: String,
    },
    /// A new profile. `saves`: a save store (id, name, `default` or `new`); `None` follows
    /// the advice (Default if every mod is save-compatible, else a new store).
    ProfileCreate {
        #[serde(flatten)]
        game: crate::game::GameField,
        name: String,
        #[serde(default)]
        layers: Vec<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saves: Option<String>,
    },
    /// Edit a profile (see [`Profile::apply`] for the keys); records a revision.
    ProfileSet {
        #[serde(flatten)]
        game: crate::game::GameField,
        profile: String,
        ops: Vec<SettingOp>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Delete a profile (a restorable revision; its saves and writable layer are kept).
    ProfileDelete {
        #[serde(flatten)]
        game: crate::game::GameField,
        profile: String,
    },
    /// A profile's history (`Vec<ProfileRevision>`), deleted profiles included.
    ProfileHistory { #[serde(flatten)] game: crate::game::GameField, profile: String },
    /// Make an earlier revision current again, as a new revision (also undeletes).
    ProfileRestore {
        #[serde(flatten)]
        game: crate::game::GameField,
        profile: String,
        revision: u32,
    },
    /// Add (`delete`) or take back (`keep`) a layer's deletions: paths relative to the
    /// install folder. Records a revision in the layer's history; answers a
    /// [`LayerChange`]. Refused while the game is running.
    LayerDeletes {
        #[serde(flatten)]
        game: crate::game::GameField,
        layer: String,
        #[serde(default)]
        delete: Vec<String>,
        #[serde(default)]
        keep: Vec<String>,
        #[serde(default)]
        message: Option<String>,
    },
    /// A layer's history of deletions (`Vec<LayerRevision>`).
    LayerHistory { #[serde(flatten)] game: crate::game::GameField, layer: String },
    /// Make an earlier revision of a layer's deletions current again, as a new revision.
    LayerRestore {
        #[serde(flatten)]
        game: crate::game::GameField,
        layer: String,
        revision: u32,
    },
    // -- Mods v2 (appended; older clients never send these) ------------------------------
    /// Quiver's built-in mods for a game (`Vec<CatalogEntry>`).
    Catalog { #[serde(flatten)] game: crate::game::GameField },
    /// Install a mod: a built-in one by id (`builtin`), or a package folder (`path`, with a
    /// `quiver-mod.toml`; a folder without one installs as a file-only mod named after
    /// it). Installing a built-in again updates it to the catalog's manifest. Answers a
    /// [`LayerChange`].
    Install {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(default)]
        builtin: Option<String>,
        #[serde(default)]
        path: Option<PathBuf>,
        /// Name for a folder without a manifest (default: the folder's name).
        #[serde(default)]
        name: Option<String>,
    },
    /// Remove an installed mod: its files, and the config values it changed go back to the
    /// file's own. Refused while a profile stacks it or the game runs.
    Uninstall {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
    },
    /// An installed mod in detail (`ModDetail`).
    Show {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
    },
    /// Change an installed mod: `enabled=on|off`, `save_compatible=on|off|unknown`,
    /// `kind=fix|mod`, `name=…`. Recorded in the mod's history. Answers a [`LayerChange`].
    ModSet {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
        ops: Vec<SettingOp>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Everything a launch would use (`LaunchPlan`): resolved settings, mods, conflicts,
    /// saves, mounts, per-profile folders and where their files live, config patches.
    /// `profile` `None` or `vanilla`: the unmodded game (with fixes, if on).
    Plan {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(default)]
        profile: Option<String>,
    },
    /// Put the config values Quiver changed back to the files' own, now (all mods, or one).
    /// Refused while the game runs.
    PatchesUndo {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(default, rename = "mod")]
        mod_id: Option<String>,
    },
    /// Read the app-level mod preferences, changing the ones given (`ModPrefs`).
    Prefs {
        #[serde(default)]
        fixes_on_vanilla: Option<bool>,
    },
    /// Make older data explicit (`MigrationReport`): every profile gets a save store. One
    /// with its own saves folder (or `separate_saves=on`, or a stack that would have forked
    /// its saves) gets a store pointing at that folder, so nothing moves; the others use
    /// Default. Layers need nothing. Idempotent. Launches do this on their own.
    Migrate {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(default)]
        dry_run: bool,
    },
    // -- Mod versions (appended) ---------------------------------------------------------
    /// A new version of an installed mod (`VersionChange`): a folder of files (`path`; empty:
    /// none) or a package folder with a `quiver-mod.toml`, copied into its own folder. A plain
    /// folder keeps the current version's manifest (launch settings, patches, deletions,
    /// save compatibility) with `deletes` added; `save_compatible` (`on`, `off`, `unknown`)
    /// overrides that. It becomes current, so every profile tracking the mod runs it next,
    /// unless `no_update` or `prerelease`.
    VersionAdd {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
        #[serde(default)]
        path: PathBuf,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        notes: Option<String>,
        #[serde(default)]
        no_update: bool,
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        save_compatible: Option<String>,
        #[serde(default)]
        deletes: Vec<String>,
        /// Options: the version's schema file, when it isn't in the folder (appended).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<PathBuf>,
    },
    /// Make a version the mod's current one, for every profile that tracks it: a roll back
    /// or forward (`VersionChange`).
    VersionUse {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
        version: String,
        #[serde(default)]
        message: Option<String>,
    },
    /// Retire a version (hidden from defaults, files kept, pins still work) or, with
    /// `retired: false`, bring it back (`VersionChange`).
    VersionRetire {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
        version: String,
        #[serde(default = "yes")]
        retired: bool,
        #[serde(default)]
        message: Option<String>,
    },
    /// A mod's history (`Vec<ModRevision>`; empty for a mod that never had a second version).
    ModHistory {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(rename = "mod")]
        mod_id: String,
    },
    /// Make separate mods versions of one (`VersionMerge`): every version of `from` joins
    /// `into`, ordered by when each was added; the newest becomes current. Live profiles that
    /// stacked one of them now stack `into`, tracking current if they ran the newest version,
    /// else pinned to the version they ran, so no launch changes. `labels` names the versions
    /// in order (default: `v1`, `v2`… for versions without a label of their own). Recorded as
    /// revisions (the mod's, each profile's).
    MergeVersions {
        #[serde(flatten)]
        game: crate::game::GameField,
        into: String,
        from: Vec<String>,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        dry_run: bool,
    },
}

fn yes() -> bool {
    true
}

// ---------------------------------------------------------------------------------------
// Mod SDK (build/mod-sdk)

/// Requests about mods connected through the mod SDK (`Request::ModSdk`). Kept apart from
/// [`ModsRequest`] (installed mods and profiles): these are about running games.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ModSdkRequest {
    /// Mods connected right now, for one game or all, oldest first (`Vec<ModSdkConnection>`).
    Status {
        #[serde(default)]
        product_id: Option<u32>,
    },
}

/// A mod connected through the SDK pipe of a running game (`ModSdkRequest::Status`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModSdkConnection {
    /// quiverd's id for this connection (unique while quiverd runs).
    pub conn: u64,
    pub product_id: u32,
    /// The schema surface id it said `hello` with; also its metrics' namespace.
    pub surface: String,
    /// The SDK's version string, and the protocol agreed on.
    pub sdk: String,
    pub proto: u32,
    /// Capabilities granted (`options`, `metrics`, `actions`, `log`).
    pub caps: Vec<String>,
    /// Windows pid the mod reported, and the one Wine reported for the pipe's client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// User id of the account playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub connected: DateTime<Utc>,
    /// Live options it applies itself (a `set` goes to it; others wait for a restart or the
    /// file channel).
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<ModSdkMetric>,
    /// It sent an inline schema in `hello`.
    #[serde(default)]
    pub inline_schema: bool,
    /// Messages quiverd refused (malformed, over a rate limit, not its namespace).
    #[serde(default)]
    pub refused: u64,
}

/// One metric a connected mod declared, with its latest value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModSdkMetric {
    pub index: u32,
    /// Full id: `aco-dlss.fg.state`.
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Latest value: a number (gauge), a total (counter), a string (state, text), or an
    /// event's latest label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
    /// `event` metrics: marks received.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub marks: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(p: &mut Profile, s: &str) -> Result<(), String> {
        p.apply(&SettingOp::parse(s)?)
    }

    #[test]
    fn profile_ops() {
        let mut p = Profile { id: "p1".into(), name: "x".into(), ..Default::default() };
        set(&mut p, "layers=la, lb,").unwrap();
        assert_eq!(p.layers, ["la", "lb"]);
        // Cloud sync is per save store now.
        assert!(set(&mut p, "cloud_saves=on").unwrap_err().contains("store cloud"));
        set(&mut p, "pinned=true").unwrap();
        assert!(p.pinned);
        set(&mut p, "env.DXVK_HUD=fps").unwrap();
        set(&mut p, "dll.dxgi=n,b").unwrap();
        assert_eq!(p.settings.env["DXVK_HUD"], "fps");
        assert!(set(&mut p, "account=me").is_err());
        assert!(set(&mut p, "name=").is_err());
        assert!(set(&mut p, "bogus=1").unwrap_err().contains("saves"));
        set(&mut p, "name= Visual overhaul ").unwrap();
        assert_eq!(p.name, "Visual overhaul");
        set(&mut p, "layers=").unwrap();
        assert!(p.layers.is_empty());
    }

    #[test]
    fn mod_sdk_wire() {
        let r: crate::Request = serde_json::from_str(r#"{"method":"mod_sdk","params":{"op":"status","product_id":5}}"#).unwrap();
        assert!(matches!(r, crate::Request::ModSdk(ModSdkRequest::Status { product_id: Some(5) })));
        let r: crate::Request = serde_json::from_str(r#"{"method":"mod_sdk","params":{"op":"status"}}"#).unwrap();
        assert!(matches!(r, crate::Request::ModSdk(ModSdkRequest::Status { product_id: None })));
        let c: ModSdkConnection =
            serde_json::from_str(r#"{"conn":1,"product_id":5,"surface":"m","sdk":"s","proto":2,"caps":[],"connected":"2026-11-16T00:00:00Z"}"#).unwrap();
        assert!(c.metrics.is_empty() && c.options.is_empty());
    }

    #[test]
    fn wire() {
        let r: crate::Request = serde_json::from_str(
            r#"{"method":"mods","params":{"op":"profile_delete","product_id":5,"profile":"x"}}"#,
        )
        .unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::ProfileDelete { .. })));
        // Older clients still send `confirmed` (for the agent policy's "Ask", dropped
        // 2026-11-18): it's ignored.
        let r: crate::Request = serde_json::from_str(
            r#"{"method":"mods","params":{"op":"profile_delete","product_id":5,"profile":"x","confirmed":true}}"#,
        )
        .unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::ProfileDelete { .. })));
        // Older clients send no profile (the game's default applies); `vanilla` forces vanilla.
        let r: crate::Request = serde_json::from_str(r#"{"method":"play","params":{"product_id":5}}"#).unwrap();
        assert!(matches!(r, crate::Request::Play { profile: None, .. }));
        let r: crate::Request = serde_json::from_str(r#"{"method":"play","params":{"product_id":5,"profile":"vanilla"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Play { profile: Some(ref p), .. } if p == VANILLA));
        // The default profile is per game, not a profile override.
        let mut p = Profile::default();
        assert!(p.apply(&SettingOp::Set { key: "profile".into(), value: "x".into() }).is_err());
        // Defaults stay out of stored revisions.
        let p = Profile { id: "p".into(), name: "n".into(), ..Default::default() };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("settings").is_none());
        let back: Profile = serde_json::from_value(serde_json::json!({ "id": "p", "name": "n" })).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn layer_wire() {
        // Requests from before deletions still parse.
        let r: crate::Request = serde_json::from_str(
            r#"{"method":"mods","params":{"op":"layer_add","product_id":5,"name":"x","path":"/m"}}"#,
        )
        .unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::LayerAdd { ref deletes, .. }) if deletes.is_empty()));
        let r: crate::Request = serde_json::from_str(
            r#"{"method":"mods","params":{"op":"layer_deletes","product_id":5,"layer":"x","delete":["a/b"]}}"#,
        )
        .unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::LayerDeletes { ref keep, .. }) if keep.is_empty()));
        // Layer metadata from before deletions; no deletions stay out of the JSON.
        let old = serde_json::json!({
            "id": "l1", "name": "n", "added": "2026-01-01T00:00:00Z", "added_by": "cli", "files": 1, "bytes": 2
        });
        let l: Layer = serde_json::from_value(old).unwrap();
        assert!(l.deletes.is_empty());
        assert!(serde_json::to_value(&l).unwrap().get("deletes").is_none());
        // A LayerChange reads as a Layer (older clients expect one from layer_add).
        let c = LayerChange { layer: l.clone(), revision: 1, warnings: vec!["w".into()] };
        let back: Layer = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        assert_eq!(back, l);
        let c = Conflict { path: "a".into(), layers: vec!["l1".into()], ..Default::default() };
        assert!(serde_json::to_value(&c).unwrap().get("deleted_by").is_none());
    }

    #[test]
    fn v2_profile_keys() {
        let mut p = Profile { id: "p1".into(), name: "x".into(), ..Default::default() };
        set(&mut p, "mods=a,b").unwrap();
        assert_eq!(p.layers, ["a", "b"]);
        assert_eq!((p.separate_saves, p.saves.as_deref()), (None, None));
        // Save stores replace separate_saves.
        assert!(set(&mut p, "separate_saves=on").unwrap_err().contains("saves=new"));
        set(&mut p, "saves=s123").unwrap();
        assert_eq!(p.saves.as_deref(), Some("s123"));
        assert!(set(&mut p, "saves=").is_err());
        assert_eq!(p.saves.as_deref(), Some("s123"));
        set(&mut p, "profile_config=drive_c/a, drive_c/b").unwrap();
        assert_eq!(p.profile_config, ["drive_c/a", "drive_c/b"]);
        // A v1 revision reads as auto saves and no per-profile folders; defaults stay out.
        let old: Profile = serde_json::from_str(r#"{"id":"p","name":"n","layers":["l1"],"cloud_saves":true,"pinned":false}"#).unwrap();
        assert_eq!((old.separate_saves, old.profile_config.len()), (None, 0));
        let v = serde_json::to_value(&old).unwrap();
        assert!(v.get("separate_saves").is_none() && v.get("profile_config").is_none() && v.get("saves").is_none());
        assert!(old.saves.is_none(), "not migrated yet");
    }

    #[test]
    fn v2_wire() {
        // A v1 layer is a file-only mod: no manifest, enabled, not save-compatible.
        let l: Layer = serde_json::from_value(serde_json::json!({
            "id": "l9", "name": "DLSS", "added": "2026-01-01T00:00:00Z", "added_by": "x", "files": 3, "bytes": 9
        }))
        .unwrap();
        assert!(l.manifest.is_none() && l.enabled() && !l.save_compatible() && !l.builtin);
        assert_eq!(l.kind(), ModKind::Mod);
        assert!(l.is("dlss") && l.is("l9") && !l.is("x"));
        let v = serde_json::to_value(&l).unwrap();
        assert!(v.get("manifest").is_none() && v.get("disabled").is_none() && v.get("builtin").is_none());
        let r: crate::Request = serde_json::from_str(r#"{"method":"mods","params":{"op":"install","product_id":1,"builtin":"x"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::Install { path: None, .. })));
        let r: crate::Request = serde_json::from_str(r#"{"method":"mods","params":{"op":"uninstall","product_id":1,"mod":"x"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::Uninstall { .. })));
        let r: crate::Request = serde_json::from_str(r#"{"method":"mods","params":{"op":"prefs"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::Prefs { fixes_on_vanilla: None })));
        let r: crate::Request = serde_json::from_str(r#"{"method":"mods","params":{"op":"plan","product_id":1}}"#).unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::Plan { profile: None, .. })));
        // A v1 ModsState and ProfileInfo still parse.
        let s: ModsState = serde_json::from_str(r#"{"product_id":1,"layers":[],"profiles":[{"profile":{"id":"p","name":"n"},"revision":1}]}"#).unwrap();
        assert!(s.fixes_on_vanilla.is_none() && s.profiles[0].saves.is_none());
        // Unknown manifest fields are refused (typos shouldn't silently do nothing).
        assert!(serde_json::from_str::<ModManifest>(r#"{"id":"a","name":"b","save_compat":true}"#).is_err());
    }

    #[test]
    fn versions_resolve_and_read_old_data() {
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let layer = |id: &str, name: &str, when: &str| Layer { id: id.into(), name: name.into(), added: at(when), ..Default::default() };
        let layers = vec![layer("l1", "DLSS", "2026-01-01T00:00:00Z"), layer("v2", "DLSS", "2026-02-01T00:00:00Z"), layer("l3", "Intro", "2026-01-15T00:00:00Z")];
        // No records: one mod per layer, current = itself, label v1.
        let implicit = mods_of(&layers, &[]);
        assert_eq!(implicit.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["l1", "l3", "v2"]);
        assert!(implicit.iter().all(|m| m.current == m.id && m.versions.len() == 1 && m.versions[0].label == "v1"));
        // A record claims its versions.
        let mut rec = ModRecord::implicit(&layers[0]);
        rec.versions.push(ModVersion { id: "v2".into(), label: "3.8".into(), added: layers[1].added, ..Default::default() });
        rec.current = "v2".into();
        let mods = mods_of(&layers, std::slice::from_ref(&rec));
        assert_eq!(mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["l1", "l3"]);
        let r = resolve_entry(&mods, "l1").unwrap();
        assert_eq!((r.version.as_str(), r.pinned, r.current, r.describe()), ("v2", false, true, "DLSS 3.8 (current)".to_owned()));
        let r = resolve_entry(&mods, "l1@v1").unwrap();
        assert_eq!((r.version.as_str(), r.pinned, r.describe()), ("l1", true, "DLSS v1 (pinned)".to_owned()));
        assert_eq!(resolve_entry(&mods, "l1@l1").unwrap().version, "l1", "by id too");
        // A bare version id (an old revision, or a merged mod's) is pinned to it.
        let r = resolve_entry(&mods, "v2").unwrap();
        assert_eq!((r.mod_id.as_str(), r.pinned), ("l1", true));
        assert_eq!(resolve_entry(&mods, "l3").unwrap().describe(), "Intro");
        assert!(resolve_entry(&mods, "l1@nope").is_none() && resolve_entry(&mods, "gone").is_none());
        // A state from before versions (no `mods`) still answers.
        let s: ModsState = serde_json::from_value(serde_json::json!({ "product_id": 1, "layers": layers, "profiles": [] })).unwrap();
        assert!(s.mods.is_empty() && s.all_mods().len() == 3);
        assert_eq!(s.display("v2"), "DLSS");
        let s = ModsState { mods: vec![rec.clone()], ..s };
        assert_eq!((s.display("v2"), s.display("l1")), ("DLSS 3.8".to_owned(), "DLSS".to_owned()));
        assert_eq!(s.stack_layers(&["l1@v1".into(), "l3".into()]).iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["DLSS v1", "Intro"]);
        // Records and requests on the wire.
        let rev: ModRevision = serde_json::from_value(serde_json::json!({
            "revision": 1, "at": "2026-01-01T00:00:00Z", "source": "cli",
            "mod": { "id": "l1", "name": "DLSS", "current": "l1", "versions": [{ "id": "l1", "label": "v1", "added": "2026-01-01T00:00:00Z", "added_by": "cli" }] }
        }))
        .unwrap();
        assert!(!rev.deleted && !rev.record.disabled && !rev.record.versions[0].retired);
        let r: crate::Request = serde_json::from_str(r#"{"method":"mods","params":{"op":"version_retire","product_id":1,"mod":"x","version":"v1"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::VersionRetire { retired: true, .. })));
        let r: crate::Request = serde_json::from_str(r#"{"method":"mods","params":{"op":"version_add","product_id":1,"mod":"x"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::VersionAdd { no_update: false, ref path, .. }) if path.as_os_str().is_empty()));
        // A session from before versions reads with no mods.
        let s: crate::PlaySession = serde_json::from_str(r#"{"started":"2026-01-01T00:00:00Z","ended":"2026-01-01T01:00:00Z","seconds":3600}"#).unwrap();
        assert!(s.mods.is_empty());
        assert_eq!(split_entry("a@ b"), ("a", Some("b")));
        assert_eq!(stack_entry("a", Some("b")), "a@b");
    }

    #[test]
    fn stack_resolution() {
        let mut base = GameSettings::default();
        for op in ["args=-a", "env.A=1", "dll.d3d11=b", "wrappers=gamemoderun"] {
            base.apply(&SettingOp::parse(op).unwrap()).unwrap();
        }
        let m1 = ModLaunch {
            args: vec!["-skipintro".into()],
            env: [("A".to_string(), "2".to_string()), ("B".into(), "x".into())].into(),
            dll: [("d3d11".to_string(), "n,b".to_string())].into(),
            wrappers: vec!["mangohud".into(), "gamemoderun".into()],
        };
        let m2 = ModLaunch { args: vec!["-dx11".into()], env: [("B".to_string(), "x".to_string())].into(), ..Default::default() };
        let mut over = GameSettings::default();
        for op in ["args=-windowed", "env.C=3", "dll.d3d11=n", "proton=GE-2", "wrappers=strace"] {
            over.apply(&SettingOp::parse(op).unwrap()).unwrap();
        }
        let (s, conflicts) = resolve_settings(&base, &[("m1".into(), m1), ("m2".into(), m2)], &over);
        assert_eq!(s.args, ["-a", "-skipintro", "-dx11", "-windowed"]);
        assert_eq!(s.wrappers, ["gamemoderun", "mangohud", "strace"]);
        assert_eq!((s.env["A"].as_str(), s.env["B"].as_str(), s.env["C"].as_str()), ("2", "x", "3"));
        assert_eq!(s.dll_overrides["d3d11"], "n");
        assert_eq!(s.proton.as_deref(), Some("GE-2"));
        // Same value from two mods isn't a conflict; different values are, in order.
        let keys: Vec<&str> = conflicts.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["dll.d3d11", "env.A"]);
        let from: Vec<&str> = conflicts[0].by.iter().map(|b| b.from.as_str()).collect();
        assert_eq!(from, ["game", "m1", "profile"]);
    }
}
