//! Where a game's art comes from, and in which order the sources are tried.
//!
//! Every front end (the app's tiles, heroes, logos, Now Playing, notifications; `quiver
//! info`) asks [`candidates`] and uses the first image that loads. With the default
//! preference the order is:
//!
//! 1. the product config's art (`thumb_image`, `background_image`, `logo_image`: file names
//!    on Ubisoft's asset CDN), in [`crate::Art`]'s plain fields;
//! 2. UbiServices' games API (`imageUrls` of the game's space, [`SpaceArt`]), which also
//!    covers games bought on Steam, whose configs carry no art;
//! 3. Steam's local library cache (`appcache/librarycache`, [`SteamArt`]), for games with a
//!    Steam install;
//! 4. Steam's public CDN ([`steam_cdn_url`]), for the same games when the cache is missing.
//!
//! The per-game `art=steam` setting moves 3 and 4 ahead of 1 and 2. The user's own image
//! files (the app's custom art) win over all of them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::LibraryEntry;

/// Art URLs from UbiServices' games API (`GET /v1/spaces/global/ubiconnect/games/api`), on
/// `ubiservices.cdn.ubi.com`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceArt {
    /// Portrait box art (`highBoxArt`, else `lowBoxArt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile: Option<String>,
    /// `background`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hero: Option<String>,
    /// `gameLogoColor`, else `gameLogoFlat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// `gameAvatar` (square).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl SpaceArt {
    pub fn is_empty(&self) -> bool {
        self.tile.is_none() && self.hero.is_none() && self.logo.is_none() && self.icon.is_none()
    }
}

/// Steam's own art for an app, from its local library cache (`<Steam>/appcache/librarycache`).
/// Read-only: Quiver never writes there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamArt {
    /// Portrait capsule (`library_600x900.jpg`, or `library_capsule.jpg` at 300×450).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capsule: Option<PathBuf>,
    /// `library_hero.jpg` (1920×620, no logo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hero: Option<PathBuf>,
    /// `logo.png` (transparent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<PathBuf>,
    /// `header.jpg` / `library_header.jpg` (460×215).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<PathBuf>,
    /// The 32×32 icon (`<sha1>.jpg`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
}

impl SteamArt {
    pub fn is_empty(&self) -> bool {
        self.capsule.is_none() && self.hero.is_none() && self.logo.is_none() && self.header.is_none() && self.icon.is_none()
    }
}

/// Steam's public store CDN.
pub const STEAM_CDN: &str = "https://shared.steamstatic.com/store_item_assets/steam/apps/";

/// `file` (`library_600x900.jpg`, `library_hero.jpg`, `logo.png`, `header.jpg`) of Steam app
/// `app_id` on Steam's CDN.
pub fn steam_cdn_url(app_id: u32, file: &str) -> String {
    format!("{STEAM_CDN}{app_id}/{file}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtSlot {
    /// Portrait box art.
    Tile,
    /// Wide background.
    Hero,
    /// Transparent logo.
    Logo,
    /// Small square icon.
    Icon,
}

/// Where a candidate image comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtOrigin {
    /// The product config.
    Config,
    /// UbiServices' games API.
    GamesApi,
    /// Steam's local library cache.
    SteamCache,
    /// Steam's public CDN.
    SteamCdn,
}

impl ArtOrigin {
    pub fn is_steam(self) -> bool {
        matches!(self, Self::SteamCache | Self::SteamCdn)
    }

    /// For people: "Ubisoft" or "Steam".
    pub fn vendor(self) -> &'static str {
        if self.is_steam() { "Steam" } else { "Ubisoft" }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Config => "Ubisoft product config",
            Self::GamesApi => "Ubisoft games API",
            Self::SteamCache => "Steam library cache",
            Self::SteamCdn => "Steam CDN",
        }
    }
}

/// An image to load.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtRef {
    /// A product-config file name on Ubisoft's asset CDN.
    Asset(String),
    /// A full `https` URL (games API, Steam CDN).
    Url(String),
    /// A file on this machine (Steam's library cache).
    File(PathBuf),
}

/// Which vendor's art a game prefers (its `art` setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArtPreference {
    /// Ubisoft's, then Steam's (also the default when unset).
    #[default]
    Ubisoft,
    /// Steam's first, for games with a Steam install.
    Steam,
}

impl ArtPreference {
    /// From the `art` setting's value (`steam`, `ubisoft`, unset).
    pub fn from_setting(v: Option<&str>) -> Self {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("steam") => Self::Steam,
            _ => Self::Ubisoft,
        }
    }
}

/// Every image that could fill `slot` for `entry`, best first (see the module docs).
pub fn candidates(entry: &LibraryEntry, slot: ArtSlot, pref: ArtPreference) -> Vec<(ArtOrigin, ArtRef)> {
    let art = &entry.art;
    let mut ubisoft = Vec::new();
    let config = match slot {
        ArtSlot::Tile => &art.thumb,
        ArtSlot::Hero => &art.background,
        ArtSlot::Logo => &art.logo,
        ArtSlot::Icon => &art.icon,
    };
    if let Some(f) = config.as_ref().filter(|f| !f.is_empty()) {
        ubisoft.push((ArtOrigin::Config, ArtRef::Asset(f.clone())));
    }
    let space = match slot {
        ArtSlot::Tile => &art.space.tile,
        ArtSlot::Hero => &art.space.hero,
        ArtSlot::Logo => &art.space.logo,
        ArtSlot::Icon => &art.space.icon,
    };
    if let Some(u) = space.as_ref().filter(|u| u.starts_with("https://")) {
        ubisoft.push((ArtOrigin::GamesApi, ArtRef::Url(u.clone())));
    }
    let mut steam = Vec::new();
    if let Some(g) = &entry.steam {
        let cached = match slot {
            ArtSlot::Tile => &g.art.capsule,
            ArtSlot::Hero => &g.art.hero,
            ArtSlot::Logo => &g.art.logo,
            ArtSlot::Icon => &g.art.icon,
        };
        if let Some(p) = cached {
            steam.push((ArtOrigin::SteamCache, ArtRef::File(p.clone())));
        }
        let cdn = match slot {
            ArtSlot::Tile => Some("library_600x900.jpg"),
            ArtSlot::Hero => Some("library_hero.jpg"),
            ArtSlot::Logo => Some("logo.png"),
            ArtSlot::Icon => None,
        };
        if let Some(f) = cdn {
            steam.push((ArtOrigin::SteamCdn, ArtRef::Url(steam_cdn_url(g.app_id, f))));
        }
    }
    match pref {
        ArtPreference::Ubisoft => ubisoft.extend(steam),
        ArtPreference::Steam => {
            steam.extend(ubisoft);
            return steam;
        }
    }
    ubisoft
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Art;
    use crate::exports::{PatchState, SteamGame};

    fn entry(art: Art, steam: Option<SteamArt>) -> LibraryEntry {
        LibraryEntry {
            product_id: 66088,
            name: "BF".into(),
            installed: None,
            update_available: false,
            running: false,
            art,
            owned_by: vec![],
            installable: false,
            space_id: None,
            steam: steam.map(|art| SteamGame {
                app_id: 3751950,
                name: "BF".into(),
                install_dir: "/s/common/BF".into(),
                product_id: Some(66088),
                product_from: None,
                owned: Some(true),
                steam_user: None,
                launch_options: String::new(),
                state: PatchState::Off,
                patchable: true,
                offer: false,
                note: None,
                art,
            }),
        }
    }

    fn origins(v: &[(ArtOrigin, ArtRef)]) -> Vec<ArtOrigin> {
        v.iter().map(|c| c.0).collect()
    }

    #[test]
    fn ubisoft_first_then_steam() {
        let art = Art {
            thumb: Some("t.jpg".into()),
            space: SpaceArt { tile: Some("https://ubiservices.cdn.ubi.com/x/box.jpg".into()), logo: Some("http://insecure/logo.png".into()), ..Default::default() },
            ..Default::default()
        };
        let steam = SteamArt { capsule: Some("/lc/3751950/h/library_capsule.jpg".into()), ..Default::default() };
        let e = entry(art, Some(steam));
        let tile = candidates(&e, ArtSlot::Tile, ArtPreference::Ubisoft);
        assert_eq!(origins(&tile), [ArtOrigin::Config, ArtOrigin::GamesApi, ArtOrigin::SteamCache, ArtOrigin::SteamCdn]);
        assert_eq!(tile[3].1, ArtRef::Url("https://shared.steamstatic.com/store_item_assets/steam/apps/3751950/library_600x900.jpg".into()));
        // Steam preferred: Steam's sources move ahead, in the same order among themselves.
        let tile = candidates(&e, ArtSlot::Tile, ArtPreference::Steam);
        assert_eq!(origins(&tile), [ArtOrigin::SteamCache, ArtOrigin::SteamCdn, ArtOrigin::Config, ArtOrigin::GamesApi]);
        // Only https URLs from the API; no cache file: straight to the CDN.
        assert_eq!(origins(&candidates(&e, ArtSlot::Logo, ArtPreference::Ubisoft)), [ArtOrigin::SteamCdn]);
        // Steam's CDN has no icon.
        assert!(candidates(&e, ArtSlot::Icon, ArtPreference::Ubisoft).is_empty());
        // No Steam install: Ubisoft's only.
        let e = entry(Art { background: Some("b.jpg".into()), ..Default::default() }, None);
        assert_eq!(origins(&candidates(&e, ArtSlot::Hero, ArtPreference::Steam)), [ArtOrigin::Config]);
        assert!(candidates(&e, ArtSlot::Tile, ArtPreference::Ubisoft).is_empty());
    }

    #[test]
    fn preference_setting() {
        assert_eq!(ArtPreference::from_setting(Some(" Steam ")), ArtPreference::Steam);
        assert_eq!(ArtPreference::from_setting(Some("ubisoft")), ArtPreference::Ubisoft);
        assert_eq!(ArtPreference::from_setting(None), ArtPreference::Ubisoft);
    }
}
