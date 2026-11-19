//! Steam copies of Ubisoft games in the library: raw ownership data (to see why a product is
//! or isn't listed) and the launch settings `quiver steam-run` applies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::GameRef;

/// One product as `ownership_service` reports it (`Request::Ownership`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OwnedProduct {
    pub product_id: u32,
    #[serde(default)]
    pub name: Option<String>,
    /// `ProductType`: 0 game, 1 add-on, 2 pre-order game, … 6 bundle, 7 season pass.
    #[serde(default)]
    pub product_type: u32,
    /// `State`: 0 unavailable, 1 pre-released, 2 downloadable, 3 released, 4 expired.
    #[serde(default)]
    pub state: u32,
    #[serde(default)]
    pub owned: bool,
    /// `latest_manifest` is set: Ubisoft has a PC build to download.
    #[serde(default)]
    pub has_pc_build: bool,
    #[serde(default)]
    pub download_id: Option<u32>,
    #[serde(default)]
    pub platform: Option<u32>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub locked_by_subscription: bool,
    /// `ActivationType`: 0 purchase, 2 subscription.
    #[serde(default)]
    pub activation_type: Option<i32>,
    /// `TargetPartner`: 0 none, 1 Epic Games, 2 Neowiz, 3 Microsoft.
    #[serde(default)]
    pub target_partner: Option<i32>,
    /// `PackageOwnershipState`: 0 full, 1 trial, 2 suspended.
    #[serde(default)]
    pub package_ownership_state: Option<i32>,
    #[serde(default)]
    pub brand_id: Option<u32>,
    #[serde(default)]
    pub associations: Vec<u32>,
    #[serde(default)]
    pub has_configuration: bool,
    /// Whether Quiver's library lists it, and if not, why.
    #[serde(default)]
    pub listed: bool,
    #[serde(default)]
    pub not_listed_because: Option<String>,
}

/// What `quiver steam-run` applies to a Steam copy (`Request::SteamLaunchPlan`): the
/// game's launch settings, as far as they make sense inside Steam's own launch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SteamLaunchPlan {
    pub product_id: u32,
    /// Settings revision these come from.
    #[serde(default)]
    pub revision: u32,
    /// Added to the game's environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Merged into `WINEDLLOVERRIDES` (`d3d11` -> `n,b`).
    #[serde(default)]
    pub dll_overrides: BTreeMap<String, String>,
    /// Appended after the game's own (and Steam's) arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Commands run in front of Steam's Proton command, outermost first.
    #[serde(default)]
    pub wrappers: Vec<String>,
    /// Settings that don't apply to a Steam launch (Steam picks Proton, …).
    #[serde(default)]
    pub ignored: Vec<String>,
    /// The perf session quiverd started for this launch (`SteamLaunchPlan { perf: true }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf_session: Option<String>,
    /// Lines about the perf session ("hud: …"), for the launch log.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub perf_notes: Vec<String>,
    // ---- Any Steam game (appended; design/STORE-STEAM.md §9)
    /// The game, when it isn't a Ubisoft product (`steam:620`; `product_id` is then 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
    /// The tweak revisions the plan was compiled from (recorded with the play session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tweaks: Option<crate::tweaks::TweakRevisions>,
    // ---- Mods (appended; design/STEAM-COPIES.md §4)
    /// The launch's mods (`SteamLaunchPlan { mods: true }`): what to mount and what was
    /// applied. `None`: not asked for, or not a Steam game that mods apply to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<SteamMods>,
}

/// A Steam launch's mods, prepared by quiverd: config patches and options are already
/// applied (to Steam's prefix, or to the profile's own copies) when this comes back; what's
/// left for `steam-run` is mounting `overlays` in a namespace of the launch's own and
/// starting Steam's command in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SteamMods {
    /// The launch's id: `SteamSession { mods_launch }` names it, and it is the options
    /// session (and, with the HUD, the perf session too).
    pub launch: String,
    /// The profile's id (`None`: vanilla).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    /// The profile revision (0 for vanilla).
    #[serde(default)]
    pub revision: u32,
    /// Why this profile: "steam_profile", "steam-run --profile", "vanilla".
    #[serde(default)]
    pub chosen_by: String,
    /// The mods it runs, bottom to top ("DLSS v3 (current) < Skip intro videos").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<crate::mods::ResolvedMod>,
    /// Overlay mounts, in order: the install folder first, then the profile's own prefix
    /// folders. Empty: nothing to mount (`steam-run` keeps its `exec`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<SteamOverlay>,
    /// What the launch would refuse to run with (a mod lost its files, a broken stack).
    /// Non-empty: `steam-run` doesn't start the game.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// Lines for the launch log: mods, saves, config patches and options as applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
}

/// One overlayfs mount (`quiver_core::overlay::OverlaySpec` on the wire).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SteamOverlay {
    pub target: std::path::PathBuf,
    /// Topmost first.
    #[serde(default)]
    pub lower: Vec<std::path::PathBuf>,
    pub upper: std::path::PathBuf,
    pub work: std::path::PathBuf,
    /// Folders under `target` kept as they are on the host (bound over the overlay): the
    /// saves inside a profile's own settings folder. Appended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<std::path::PathBuf>,
}

impl SteamLaunchPlan {
    /// An empty plan for `game`: a Ubisoft product, or another Steam game.
    pub fn for_game(game: GameRef) -> Self {
        match game.as_ubisoft() {
            Some(product_id) => Self { product_id, ..Default::default() },
            None => Self { game: Some(game), ..Default::default() },
        }
    }

    /// The game the plan is for.
    pub fn game_ref(&self) -> GameRef {
        self.game.unwrap_or(GameRef::ubisoft(self.product_id))
    }
}
