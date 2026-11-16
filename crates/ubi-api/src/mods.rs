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

/// Sources treated as a person at a front end. Anything else (`agent`, scripts, MCP…)
/// gets the "Ask" rules of the agent policy.
pub fn is_human_source(source: &str) -> bool {
    matches!(source, "cli" | "gui")
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
}

/// Requests about mods (`Request::Mods`). Games are product ids; layers and profiles are
/// named by id or (case-insensitive) name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ModsRequest {
    /// Installed games and their profiles (`Vec<GameProfiles>`); no Ubisoft request.
    Games,
    /// A game's layers and profiles (`ModsState`).
    List { product_id: u32 },
    /// Copy a folder into the store as a new layer (reflinks where the filesystem allows).
    /// File names are matched to the install's case. An empty `path` adds a layer with
    /// only `deletes` (paths relative to the install folder to hide); whiteouts already in
    /// the folder are kept as deletions. Answers a [`LayerChange`].
    LayerAdd {
        product_id: u32,
        name: String,
        path: PathBuf,
        #[serde(default)]
        deletes: Vec<String>,
    },
    /// Delete a layer's files. Refused while a profile uses it. `confirmed`: a person
    /// approved it in a front end (needed for agent sources).
    LayerRemove {
        product_id: u32,
        layer: String,
        #[serde(default)]
        confirmed: bool,
    },
    /// A new profile. `saves`: a save store (id, name, `default` or `new`); `None` follows
    /// the advice (Default if every mod is save-compatible, else a new store).
    ProfileCreate {
        product_id: u32,
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
        product_id: u32,
        profile: String,
        ops: Vec<SettingOp>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Delete a profile (a restorable revision; its saves and writable layer are kept).
    /// Agents need `confirmed` (the policy's "Ask").
    ProfileDelete {
        product_id: u32,
        profile: String,
        #[serde(default)]
        confirmed: bool,
    },
    /// A profile's history (`Vec<ProfileRevision>`), deleted profiles included.
    ProfileHistory { product_id: u32, profile: String },
    /// Make an earlier revision current again, as a new revision (also undeletes).
    /// Agents need `confirmed`.
    ProfileRestore {
        product_id: u32,
        profile: String,
        revision: u32,
        #[serde(default)]
        confirmed: bool,
    },
    /// Add (`delete`) or take back (`keep`) a layer's deletions: paths relative to the
    /// install folder. Records a revision in the layer's history; answers a
    /// [`LayerChange`]. Refused while the game is running.
    LayerDeletes {
        product_id: u32,
        layer: String,
        #[serde(default)]
        delete: Vec<String>,
        #[serde(default)]
        keep: Vec<String>,
        #[serde(default)]
        message: Option<String>,
    },
    /// A layer's history of deletions (`Vec<LayerRevision>`).
    LayerHistory { product_id: u32, layer: String },
    /// Make an earlier revision of a layer's deletions current again, as a new revision.
    /// Agents need `confirmed`, as for profile restores.
    LayerRestore {
        product_id: u32,
        layer: String,
        revision: u32,
        #[serde(default)]
        confirmed: bool,
    },
    // -- Mods v2 (appended; older clients never send these) ------------------------------
    /// Quiver's built-in mods for a game (`Vec<CatalogEntry>`).
    Catalog { product_id: u32 },
    /// Install a mod: a built-in one by id (`builtin`), or a package folder (`path`, with a
    /// `quiver-mod.toml`; a folder without one installs as a file-only mod named after
    /// it). Installing a built-in again updates it to the catalog's manifest. Answers a
    /// [`LayerChange`].
    Install {
        product_id: u32,
        #[serde(default)]
        builtin: Option<String>,
        #[serde(default)]
        path: Option<PathBuf>,
        /// Name for a folder without a manifest (default: the folder's name).
        #[serde(default)]
        name: Option<String>,
    },
    /// Remove an installed mod: its files, and the config values it changed go back to the
    /// file's own. Refused while a profile stacks it or the game runs. Agents need
    /// `confirmed` when the mod has install files or applied config patches.
    Uninstall {
        product_id: u32,
        #[serde(rename = "mod")]
        mod_id: String,
        #[serde(default)]
        confirmed: bool,
    },
    /// An installed mod in detail (`ModDetail`).
    Show {
        product_id: u32,
        #[serde(rename = "mod")]
        mod_id: String,
    },
    /// Change an installed mod: `enabled=on|off`, `save_compatible=on|off|unknown`,
    /// `kind=fix|mod`, `name=…`. Recorded in the mod's history. Answers a [`LayerChange`].
    ModSet {
        product_id: u32,
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
        product_id: u32,
        #[serde(default)]
        profile: Option<String>,
    },
    /// Put the config values Quiver changed back to the files' own, now (all mods, or one).
    /// Refused while the game runs. Agents need `confirmed`.
    PatchesUndo {
        product_id: u32,
        #[serde(default, rename = "mod")]
        mod_id: Option<String>,
        #[serde(default)]
        confirmed: bool,
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
        product_id: u32,
        #[serde(default)]
        dry_run: bool,
    },
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
    fn wire() {
        let r: crate::Request = serde_json::from_str(
            r#"{"method":"mods","params":{"op":"profile_delete","product_id":5,"profile":"x"}}"#,
        )
        .unwrap();
        assert!(matches!(r, crate::Request::Mods(ModsRequest::ProfileDelete { confirmed: false, .. })));
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
        assert!(matches!(r, crate::Request::Mods(ModsRequest::Uninstall { confirmed: false, .. })));
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
