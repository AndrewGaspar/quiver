//! Mod layers and profiles.
//!
//! A **layer** is a directory of files that is overlaid on a game's install folder. A
//! **profile** is a named stack of layers (bottom to top) with its own saves, optional
//! launch-setting overrides and a cloud-save switch. Vanilla is implicit: no layers, the
//! game's own saves, never mounted. Running a profile never changes the install folder:
//! the layers and a per-profile writable directory are mounted over it with overlayfs,
//! visible only to that launch's processes.
//!
//! Every profile change (including deleting it) is a revision in the profile's own
//! history, so any earlier state can be restored, the same way as game settings.

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

/// A named stack of layers with its own saves.
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
    /// Sync this profile's own saves with the cloud (off by default, so a broken mod can't
    /// overwrite cloud saves).
    #[serde(default)]
    pub cloud_saves: bool,
    /// Shown as its own tile and exported like a game.
    #[serde(default)]
    pub pinned: bool,
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
    /// Apply one edit. Keys: `name`, `layers` (comma-separated layer ids, bottom to top;
    /// the daemon resolves names), `cloud_saves=on|off`, `pinned=on|off`; any other key
    /// is a launch-setting override (`proton`, `env.NAME`, `dll.NAME`, `args`,
    /// `wrappers`, `language`). `account` is per game, not per profile.
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
            "layers" => self.layers = split_list(value.unwrap_or_default()),
            "cloud_saves" => self.cloud_saves = parse_bool(key, value)?,
            "pinned" => self.pinned = parse_bool(key, value)?,
            "account" => return Err("the launch account is set per game (`quiver set <game> account=…`)".into()),
            _ => self.settings.apply(op).map_err(|e| format!("{e}; profiles also take name, layers, cloud_saves, pinned"))?,
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

/// A file more than one of a profile's layers provides; the last layer listed wins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conflict {
    pub path: String,
    pub layers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub profile: Profile,
    pub revision: u32,
    /// Layer ids the profile names that are no longer in the store.
    #[serde(default)]
    pub missing_layers: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
}

/// A game's layers and profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModsState {
    pub product_id: u32,
    pub layers: Vec<Layer>,
    pub profiles: Vec<ProfileInfo>,
    /// Deleted profiles (restorable from their history).
    #[serde(default)]
    pub deleted: Vec<ProfileInfo>,
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
    /// File names are matched to the install's case.
    LayerAdd { product_id: u32, name: String, path: PathBuf },
    /// Delete a layer's files. Refused while a profile uses it. `confirmed`: a person
    /// approved it in a front end (needed for agent sources).
    LayerRemove {
        product_id: u32,
        layer: String,
        #[serde(default)]
        confirmed: bool,
    },
    ProfileCreate {
        product_id: u32,
        name: String,
        #[serde(default)]
        layers: Vec<String>,
        #[serde(default)]
        message: Option<String>,
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
        set(&mut p, "cloud_saves=on").unwrap();
        assert!(p.cloud_saves);
        set(&mut p, "cloud_saves=").unwrap();
        assert!(!p.cloud_saves);
        assert!(set(&mut p, "cloud_saves=maybe").is_err());
        set(&mut p, "pinned=true").unwrap();
        assert!(p.pinned);
        set(&mut p, "env.DXVK_HUD=fps").unwrap();
        set(&mut p, "dll.dxgi=n,b").unwrap();
        assert_eq!(p.settings.env["DXVK_HUD"], "fps");
        assert!(set(&mut p, "account=me").is_err());
        assert!(set(&mut p, "name=").is_err());
        assert!(set(&mut p, "bogus=1").unwrap_err().contains("cloud_saves"));
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
        let r: crate::Request = serde_json::from_str(r#"{"method":"play","params":{"product_id":5}}"#).unwrap();
        assert!(matches!(r, crate::Request::Play { profile: None, .. }));
        // Defaults stay out of stored revisions.
        let p = Profile { id: "p".into(), name: "n".into(), ..Default::default() };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("settings").is_none());
        let back: Profile = serde_json::from_value(serde_json::json!({ "id": "p", "name": "n" })).unwrap();
        assert_eq!(back, p);
    }
}
