//! Steam copies of Ubisoft games in the library: raw ownership data (to see why a product is
//! or isn't listed) and the launch settings `quiver steam-run` applies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
}
