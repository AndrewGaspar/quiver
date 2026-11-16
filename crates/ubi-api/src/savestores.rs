//! Save stores: a game's named sets of save slots, chosen per profile independently of
//! its mods (design/MODS-V2.md, "Saves are orthogonal to profiles").
//!
//! Every game has the built-in **Default** store, the folder Ubisoft Connect itself uses
//! (`savegames/<user>/<product>` in the prefix), and any number of extra stores
//! (`savegames/<user>/<product>-<id>`; stores made from a Mods v1 profile's own saves keep
//! that profile's folder, `<product>-<profile id>`). A profile points at one store
//! (`Profile::saves`); vanilla points at one too (normally Default); several may share
//! one. Ubisoft's cloud keeps one set of slots per game and account, so at most one store
//! per game syncs with it (`SaveStore::cloud`, Default unless changed).
//!
//! The stores' state is a history of whole-state revisions
//! (`games/<product>/saves.jsonl`), so creating, renaming, retiring and moving the cloud
//! flag are all visible and reversible. Retiring a store keeps its files.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::mods::Layer;

/// The id (and, case-insensitively, a reserved name) of the built-in store.
pub const DEFAULT_STORE: &str = "default";

/// Asks for a new store named after the profile (`saves=new`).
pub const NEW_STORE: &str = "new";

/// A named set of save slots.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SaveStore {
    /// `default`, or random (`s7f3k2q`); never changes.
    pub id: String,
    pub name: String,
    /// The folder under `savegames/<user>/` in the prefix: `<product>` for Default,
    /// `<product>-<id>` for others. Never changes, so the files never move.
    pub folder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// Synced with Ubisoft's cloud (at most one store per game).
    #[serde(default)]
    pub cloud: bool,
    /// Retired: hidden from pickers, files kept (`unretire` brings it back).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retired: bool,
    /// Where it came from, e.g. "profile DLSS's own saves from before save stores".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SaveStore {
    pub fn is_default(&self) -> bool {
        self.id == DEFAULT_STORE
    }
}

/// One revision of a game's save stores: the whole state from then on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SaveStoresRevision {
    /// 0: nothing recorded yet (Default only, synced with the cloud, used by vanilla).
    pub revision: u32,
    pub at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub message: Option<String>,
    /// Default first, then the others by creation, retired ones included.
    pub stores: Vec<SaveStore>,
    /// The store vanilla launches use.
    #[serde(default = "default_store")]
    pub vanilla: String,
}

fn default_store() -> String {
    DEFAULT_STORE.to_owned()
}

impl SaveStoresRevision {
    /// The store that syncs with the cloud, if any.
    pub fn cloud_store(&self) -> Option<&SaveStore> {
        self.stores.iter().find(|s| s.cloud && !s.retired)
    }
    pub fn get(&self, id: &str) -> Option<&SaveStore> {
        self.stores.iter().find(|s| s.id == id)
    }
}

/// A profile (or vanilla) that uses a store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StoreUser {
    /// Profile id, or `vanilla`.
    pub id: String,
    pub name: String,
}

/// A store with who uses it and what's in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SaveStoreInfo {
    #[serde(flatten)]
    pub store: SaveStore,
    #[serde(default)]
    pub used_by: Vec<StoreUser>,
    /// `<slot>.save` files for the account (all accounts' folders if none is known).
    #[serde(default)]
    pub slots: u32,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
    /// The account's folder, when the game's prefix and account are known.
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// A game's save stores (`Request::SaveStores`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SaveStoresReport {
    pub product_id: u32,
    pub revision: u32,
    /// User id whose folders were counted (`None`: every account folder in the prefix).
    #[serde(default)]
    pub account: Option<String>,
    /// Live stores, Default first.
    pub stores: Vec<SaveStoreInfo>,
    #[serde(default)]
    pub retired: Vec<SaveStoreInfo>,
    /// The store vanilla uses.
    pub vanilla: String,
    /// The store that syncs with the cloud (`None`: none does).
    #[serde(default)]
    pub cloud: Option<String>,
    /// Profiles that haven't been given a store yet (from before save stores; launches
    /// and `quiver mods <game> migrate` give them one).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unassigned: Vec<StoreUser>,
}

/// A change to a game's save stores (`Request::SaveStoreEdit`). Stores are named by id or
/// (case-insensitive) name; `default` is the built-in one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SaveStoreOp {
    /// A new, empty store (`<product>-<id>`).
    Create { name: String },
    Rename { store: String, name: String },
    /// Make `store` the one that syncs with the cloud (the others stop); `None`: none.
    Cloud {
        #[serde(default)]
        store: Option<String>,
    },
    /// Hide a store; its files stay. Refused for Default, the cloud store and stores a
    /// profile or vanilla uses.
    Retire { store: String },
    Unretire { store: String },
    /// The store vanilla launches use.
    Vanilla { store: String },
    /// Make an earlier revision current again (as a new revision).
    Restore { revision: u32 },
}

/// What happened to one slot in a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeOutcome {
    /// Only the source had it: copied in.
    Copied,
    /// The source's copy was newer: it replaced the target's (backed up first).
    Replaced,
    /// The target's copy was newer (or as new): kept.
    Kept,
    /// Both had the same bytes.
    Same,
    /// Only the target had it.
    TargetOnly,
}

/// One slot of a merge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeSlot {
    /// The account folder it was in.
    pub account: String,
    pub slot: u32,
    pub outcome: MergeOutcome,
    #[serde(default)]
    pub from_modified: Option<DateTime<Utc>>,
    #[serde(default)]
    pub into_modified: Option<DateTime<Utc>>,
}

/// What `Request::SavesMerge` did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergeReport {
    pub from: SaveStore,
    pub into: SaveStore,
    #[serde(default)]
    pub slots: Vec<MergeSlot>,
    /// Backups of the target's saves made before anything there was replaced (one per
    /// account folder that changed).
    #[serde(default)]
    pub backups: Vec<crate::info::SaveBackup>,
    /// Slots taken from the source got an upload marker, because the target syncs with
    /// the cloud: the next sync sends them instead of downloading the cloud's over them.
    #[serde(default)]
    pub marked_for_upload: bool,
}

/// Which store a new profile should use, and why.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SavesAdvice {
    /// Every mod in the stack is save-compatible: share Default. Otherwise a new store.
    pub share_default: bool,
    pub reason: String,
    /// Mods that could change saves (not save-compatible, or unknown), by name.
    #[serde(default)]
    pub risky: Vec<String>,
}

/// A mod in words for save advice: "Skip intros (unknown save compatibility)".
fn risk(l: &Layer) -> Option<String> {
    match l.manifest.as_ref().and_then(|m| m.save_compatible) {
        Some(true) => None,
        Some(false) => Some(format!("{} (not save-compatible)", l.name)),
        None => Some(format!("{} (unknown save compatibility)", l.name)),
    }
}

/// The store a profile with `mods` should start with: Default when every mod is
/// save-compatible, else a store of its own.
pub fn saves_advice<'a>(mods: impl IntoIterator<Item = &'a Layer>) -> SavesAdvice {
    let mods: Vec<&Layer> = mods.into_iter().collect();
    let risky: Vec<String> = mods.iter().filter_map(|l| risk(l)).collect();
    if risky.is_empty() {
        let reason = if mods.is_empty() {
            "no mods yet, so it can share Default with vanilla".to_owned()
        } else {
            "every mod in the stack is save-compatible, so it can share Default with vanilla".to_owned()
        };
        return SavesAdvice { share_default: true, reason, risky };
    }
    SavesAdvice { share_default: false, reason: format!("{} could change saves, so it gets a store of its own", risky.join(", ")), risky }
}

/// A warning for a profile whose stack could change saves while it shares a store with
/// vanilla or uses Default. `None` when there's nothing to warn about.
pub fn shared_store_warning<'a>(mods: impl IntoIterator<Item = &'a Layer>, store: &SaveStore, shared_with_vanilla: bool) -> Option<String> {
    let advice = saves_advice(mods);
    if advice.share_default || !(store.is_default() || shared_with_vanilla) {
        return None;
    }
    let with = if shared_with_vanilla { "vanilla's saves" } else { "the Default saves" };
    Some(format!(
        "{} could change saves, and this profile uses {with} ({}); give it a store of its own with saves=new",
        advice.risky.join(", "),
        store.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::ModManifest;

    fn layer(name: &str, compat: Option<bool>) -> Layer {
        Layer {
            id: name.to_lowercase(),
            name: name.into(),
            manifest: compat.map(|c| ModManifest { save_compatible: Some(c), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn advice() {
        let a = saves_advice(&[]);
        assert!(a.share_default && a.reason.contains("no mods"));
        let a = saves_advice(&[layer("DLSS", Some(true))]);
        assert!(a.share_default && a.risky.is_empty());
        let a = saves_advice(&[layer("DLSS", Some(true)), layer("Skip intros", None)]);
        assert!(!a.share_default);
        assert_eq!(a.reason, "Skip intros (unknown save compatibility) could change saves, so it gets a store of its own");
        let a = saves_advice(&[layer("Hardcore", Some(false))]);
        assert_eq!(a.risky, ["Hardcore (not save-compatible)"]);
    }

    #[test]
    fn warning_only_when_shared() {
        let default = SaveStore { id: DEFAULT_STORE.into(), name: "Default".into(), ..Default::default() };
        let other = SaveStore { id: "s1".into(), name: "Run".into(), ..Default::default() };
        let risky = [layer("Hardcore", Some(false))];
        assert!(shared_store_warning(&risky, &default, false).unwrap().contains("Hardcore (not save-compatible)"));
        assert!(shared_store_warning(&risky, &other, true).unwrap().contains("vanilla's saves"));
        assert!(shared_store_warning(&risky, &other, false).is_none());
        assert!(shared_store_warning(&[layer("DLSS", Some(true))], &default, true).is_none());
    }

    #[test]
    fn wire() {
        let op: SaveStoreOp = serde_json::from_str(r#"{"op":"cloud"}"#).unwrap();
        assert_eq!(op, SaveStoreOp::Cloud { store: None });
        let r: SaveStoresRevision = serde_json::from_str(r#"{"revision":1,"at":"2026-11-16T00:00:00Z","source":"cli","stores":[]}"#).unwrap();
        assert_eq!(r.vanilla, DEFAULT_STORE);
        assert_eq!(serde_json::to_value(MergeOutcome::TargetOnly).unwrap(), "target_only");
    }
}
