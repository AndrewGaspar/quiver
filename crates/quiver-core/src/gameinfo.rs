//! Game detail from the product configuration, ownership and the local install.
//!
//! What a root config actually carries (checked against the 23 configs of this library):
//! name, art, `sort_string`, `installer.publisher`, links (`homepage_url`, `help_url`,
//! `forum_url`, `facebook_url`, `installer.about_url`), `cloud_saves`, `uplay.achievements`,
//! `club.challenges_enabled`, `player_stats.items`, `game_options`, `space_id`/`app_id`, and
//! sometimes an `addons` list (id, visibility, name/description/thumb keys). There is no
//! developer, release date, description or genre, so those stay empty. Languages and size
//! come from the installed manifest.

use std::collections::HashMap;
use std::path::Path;

pub use quiver_api::info::{AddOn, Features, GameInfo, InstallInfo, Link, ProductKind};
use quiver_api::Game;

use crate::library::{Product, ProductConfig};

/// Detail fields no source provides yet.
pub const UNAVAILABLE: &[&str] = &["developer", "release_date", "description", "genres"];

/// A Ubisoft Store search for a product name (until a direct link source turns up).
pub fn store_search_url(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !matches!(c, '®' | '™' | '©')).collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut url = reqwest::Url::parse("https://store.ubisoft.com/search").unwrap();
    url.query_pairs_mut().append_pair("q", &clean);
    url.to_string()
}

/// A YAML 1.1 style boolean (`yes`/`no` are strings to serde_yaml).
fn flag(v: &serde_yaml::Value) -> Option<bool> {
    v.as_bool().or_else(|| match v.as_str()?.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" => Some(true),
        "no" | "false" | "off" => Some(false),
        _ => None,
    })
}

fn yes(v: &serde_yaml::Value) -> bool {
    flag(v).unwrap_or(false)
}

fn str_field(v: &serde_yaml::Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// An entry of the game's own `addons` list.
#[derive(Debug, Clone, Default)]
pub struct ListedAddOn {
    pub visible: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub thumb: Option<String>,
}

/// The game's `addons` list, keyed by product id, with localization keys resolved.
pub fn listed_addons(cfg: &ProductConfig) -> HashMap<u32, ListedAddOn> {
    let resolve = |v: &serde_yaml::Value| {
        let s = v.as_str()?;
        let r = cfg.localize(s);
        // An unresolved key (`RELATED_GAMENAME_1213` with no localization) is no name.
        (r != s || !s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')).then_some(r)
    };
    cfg.root()["addons"]
        .as_sequence()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let id = a["id"].as_u64()? as u32;
            Some((
                id,
                ListedAddOn {
                    visible: flag(&a["is_visible"]).unwrap_or(true),
                    name: resolve(&a["name"]),
                    description: resolve(&a["description"]),
                    thumb: resolve(&a["thumb_image"]).filter(|t| t.contains('.')),
                },
            ))
        })
        .collect()
}

/// Facts from a game's root config.
pub fn apply_config(info: &mut GameInfo, cfg: &ProductConfig) {
    let root = cfg.root();
    if info.name.is_empty() {
        info.name = cfg.name().unwrap_or_default();
    }
    info.sort_string = str_field(&root["sort_string"]).map(|s| cfg.localize(&s));
    info.publisher = str_field(&root["installer"]["publisher"]);
    info.space_id = info.space_id.take().or_else(|| str_field(&root["space_id"]));
    info.app_id = info.app_id.take().or_else(|| str_field(&root["app_id"]));
    let mut links = Vec::new();
    for (kind, v) in [
        ("homepage", &root["homepage_url"]),
        ("about", &root["installer"]["about_url"]),
        ("support", &root["help_url"]),
        ("forum", &root["forum_url"]),
        ("facebook", &root["facebook_url"]),
    ] {
        let Some(url) = str_field(v).map(|u| cfg.localize(&u)) else { continue };
        if url.starts_with("http") && !links.iter().any(|l: &Link| l.url == url) {
            links.push(Link { kind: kind.to_owned(), url });
        }
    }
    info.links = links;
    let start = &root["start_game"];
    let denuvo = ["online", "offline"].iter().any(|m| {
        start[m]["executables"].as_sequence().into_iter().flatten().any(|e| yes(&e["denuvo"]))
    });
    info.features = Features {
        cloud_saves: yes(&root["cloud_saves"]),
        achievements: str_field(&root["uplay"]["achievements"]).is_some(),
        challenges: yes(&root["club"]["challenges_enabled"]),
        denuvo,
        player_stats: root["player_stats"]["items"]
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|i| str_field(&i["id"]))
            .collect(),
        game_options: str_field(&root["game_options"]["name"]),
        change_language: yes(&root["change_language_enabled"]),
    };
}

/// Fill what the config left out from UbiServices' games API: release date, description,
/// and links of kinds the config doesn't have (`website`, `twitter`, …).
pub fn apply_space(info: &mut GameInfo, g: &crate::spaces::SpaceGame) {
    if info.release_date.is_none() {
        info.release_date = g.release_date();
    }
    if info.description.is_none() {
        info.description = g.display_description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_owned);
    }
    for (kind, url) in g.links() {
        let kind = if kind == "website" { "homepage".to_owned() } else { kind };
        if !info.links.iter().any(|l| l.kind == kind || l.url == url) {
            info.links.push(Link { kind, url });
        }
    }
    let have = [("release_date", info.release_date.is_some()), ("description", info.description.is_some())];
    info.unavailable.retain(|u| !have.iter().any(|(k, h)| *h && u == k));
}

/// What we know about an associated product.
#[derive(Debug, Clone, Default)]
pub struct Associated {
    pub product_id: u32,
    /// From the ownership list, when the product is in it.
    pub product_type: Option<u32>,
    pub owned: bool,
    /// Its own product config (from the ownership list or `GetProductConfigReq`).
    pub config: Option<String>,
}

impl Associated {
    pub fn from_product(p: &Product) -> Self {
        Self { product_id: p.product_id, product_type: Some(p.product_type), owned: p.owned, config: p.configuration.clone() }
    }
}

/// Add-ons in the order of the game's association list.
pub fn addons(game_cfg: Option<&ProductConfig>, associated: &[Associated]) -> Vec<AddOn> {
    let listed = game_cfg.map(listed_addons).unwrap_or_default();
    associated
        .iter()
        .map(|a| {
            let cfg = a.config.as_deref().and_then(|c| ProductConfig::parse(c).ok());
            let l = listed.get(&a.product_id);
            let kind = a.product_type.map(ProductKind::from_type).unwrap_or_default();
            let name = cfg.as_ref().and_then(ProductConfig::name).or_else(|| l.and_then(|l| l.name.clone()));
            let store_url = (!a.owned).then(|| name.as_deref().map(store_search_url)).flatten();
            AddOn {
                product_id: a.product_id,
                kind,
                owned: a.owned,
                visible: l.map(|l| l.visible).unwrap_or(kind.is_dlc() || kind == ProductKind::Unknown && name.is_some()),
                description: l.and_then(|l| l.description.clone()),
                thumb: cfg.as_ref().and_then(|c| c.art("thumb_image")).or_else(|| l.and_then(|l| l.thumb.clone())),
                name,
                store_url,
            }
        })
        .collect()
}

/// Version, languages and size of an installed game, from its `uplay_install.manifest`.
pub fn install_info(game: &Game) -> InstallInfo {
    let mut info = InstallInfo { dir: game.install_dir.clone(), language: game.language.clone(), ..Default::default() };
    let path = game.install_dir.join("uplay_install.manifest");
    let Ok(raw) = std::fs::read(&path) else { return info };
    info.manifest = crate::manifest::manifest_hash(&raw).ok();
    if let Ok(m) = crate::manifest::parse_manifest(&raw) {
        let s = crate::manifest::summarize(&m);
        info.version = s.game_version.filter(|v| !v.is_empty());
        info.languages = s.languages;
        let lang = game.language.as_deref().unwrap_or("en-US");
        info.size_bytes = crate::install::plan(&m, lang).map(|p| p.install_bytes).unwrap_or(s.install_bytes);
    }
    info
}

/// Total size of the files under a directory (for installs without a readable manifest).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

/// Assemble a game's detail. `product` is the game in the launch account's ownership list.
pub fn build(product_id: u32, name: &str, product: Option<&Product>, associated: &[Associated], installed: Option<&Game>) -> GameInfo {
    let mut info = GameInfo {
        product_id,
        name: name.to_owned(),
        space_id: product.and_then(|p| p.space_id.clone()).filter(|s| !s.is_empty()),
        app_id: product.and_then(|p| p.app_id.clone()).filter(|s| !s.is_empty()),
        unavailable: UNAVAILABLE.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let cfg = product.and_then(|p| p.configuration.as_deref()).and_then(|c| ProductConfig::parse(c).ok());
    if let Some(cfg) = &cfg {
        apply_config(&mut info, cfg);
    }
    info.addons = addons(cfg.as_ref(), associated);
    info.install = installed.map(install_info);
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> ProductConfig {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/product-config").join(name);
        ProductConfig::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn origins_root_facts() {
        let cfg = fixture("3539.yaml");
        let mut info = GameInfo::default();
        apply_config(&mut info, &cfg);
        assert_eq!(info.name, "Assassin's Creed® Origins");
        assert_eq!(info.sort_string.as_deref(), Some("Assassin's Creed 16"));
        assert_eq!(info.publisher.as_deref(), Some("Ubisoft"));
        assert_eq!(info.space_id.as_deref(), Some("bfd34091-6af6-4029-873b-ba7e036464fa"));
        assert!(info.features.cloud_saves && info.features.achievements && info.features.denuvo);
        assert!(!info.features.challenges);
        assert_eq!(info.features.player_stats, ["TimePlayed", "TotalProgression", "PlayerLevel", "TotalKills", "DistanceTravelled"]);
        assert_eq!(info.features.game_options.as_deref(), Some("Animus Control Panel"));
        let kinds: Vec<&str> = info.links.iter().map(|l| l.kind.as_str()).collect();
        assert_eq!(kinds, ["homepage", "support", "forum", "facebook"]); // about_url == homepage_url
        assert!(info.developer.is_none() && info.release_date.is_none() && info.description.is_none());
    }

    #[test]
    fn listed_addons_resolve_localization_keys() {
        let fc4 = listed_addons(&fixture("420.yaml"));
        let a = &fc4[&1213];
        assert!(a.visible);
        assert_eq!(a.name.as_deref(), Some("Far Cry® 4 - Escape from Durgesh Prison"));
        assert!(a.description.as_deref().unwrap().starts_with("Escape from the prison"));
        assert_eq!(a.thumb.as_deref(), Some("d356c08f04da6e6b0b1b54de94c5f012.jpg"));
        assert!(!fc4[&1163].visible);
        let wd2 = listed_addons(&fixture("2688.yaml"));
        assert_eq!(wd2[&3962].name.as_deref(), Some("High-Rez Texture Pack"));
        assert!(wd2[&3367].name.is_none() && !wd2[&3367].visible);
    }

    #[test]
    fn addons_owned_and_store_links() {
        let game = fixture("420.yaml");
        let dlc_cfg = "root:\n  name: l1\n  thumb_image: abc.jpg\nlocalizations:\n  default:\n    l1: Far Cry® 4 Season Pass\n";
        let assoc = vec![
            Associated { product_id: 1213, product_type: Some(1), owned: false, config: None },
            Associated { product_id: 1471, product_type: Some(7), owned: true, config: Some(dlc_cfg.into()) },
            Associated { product_id: 1163, product_type: None, owned: false, config: None },
            Associated { product_id: 99999, product_type: None, owned: false, config: None },
        ];
        let list = addons(Some(&game), &assoc);
        assert_eq!(list[0].kind, ProductKind::AddOn);
        assert_eq!(list[0].name.as_deref(), Some("Far Cry® 4 - Escape from Durgesh Prison"));
        assert_eq!(list[0].store_url.as_deref(), Some("https://store.ubisoft.com/search?q=Far+Cry+4+-+Escape+from+Durgesh+Prison"));
        assert_eq!(list[1].name.as_deref(), Some("Far Cry® 4 Season Pass"));
        assert_eq!(list[1].thumb.as_deref(), Some("abc.jpg"));
        assert!(list[1].owned && list[1].visible && list[1].store_url.is_none());
        // Hidden in the game's list: kept, but not shown by default.
        assert!(!list[2].visible && list[2].name.is_some());
        // Unknown everywhere: no name, not shown, no store link.
        assert!(!list[3].visible && list[3].name.is_none() && list[3].store_url.is_none());
    }

    #[test]
    fn store_url_encoding() {
        assert_eq!(store_search_url("Assassin's Creed® Origins - The Curse of the Pharaohs"), "https://store.ubisoft.com/search?q=Assassin%27s+Creed+Origins+-+The+Curse+of+the+Pharaohs");
    }

    fn product(id: u32, config: Option<String>, associations: Vec<u32>) -> Product {
        Product {
            product_id: id,
            name: None,
            product_type: 0,
            state: 3,
            owned: true,
            download_id: None,
            latest_manifest: None,
            associations,
            space_id: Some("space-from-ownership".into()),
            app_id: None,
            locked_by_subscription: false,
            configuration: config,
            raw: Default::default(),
        }
    }

    #[test]
    fn build_from_ownership() {
        let cfg = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/product-config/5183.yaml")).unwrap();
        let p = product(5183, Some(cfg), vec![13347]);
        let assoc = [Associated { product_id: 13347, product_type: Some(0), owned: true, config: None }];
        let tmp = tempfile::tempdir().unwrap();
        let game = Game {
            product_id: 5183,
            name: "AC3".into(),
            install_dir: tmp.path().into(),
            prefix: tmp.path().into(),
            exe: "ACIII.exe".into(),
            args: None,
            language: Some("en-US".into()),
            game: None,
        };
        let info = build(5183, "Assassin's Creed® III - Remastered", Some(&p), &assoc, Some(&game));
        // Ownership's space id wins over the config's.
        assert_eq!(info.space_id.as_deref(), Some("space-from-ownership"));
        assert_eq!(info.addons.len(), 1);
        assert_eq!(info.addons[0].kind, ProductKind::Game);
        assert!(!info.addons[0].visible);
        // No manifest in the install dir: the record's facts only.
        let inst = info.install.unwrap();
        assert_eq!((inst.language.as_deref(), inst.size_bytes, inst.manifest), (Some("en-US"), 0, None));
        assert_eq!(info.unavailable, ["developer", "release_date", "description", "genres"]);
    }

    /// Reads a real installed manifest (no network): `cargo test -- --ignored real_install`.
    #[test]
    #[ignore]
    fn real_install() {
        let dir = Path::new(&std::env::var("HOME").unwrap()).join("Games/ubi-library/assassins-creed-origins");
        let game = Game {
            product_id: 3539,
            name: "Origins".into(),
            install_dir: dir.clone(),
            prefix: dir,
            exe: String::new(),
            args: None,
            language: Some("en-US".into()),
            game: None,
        };
        let i = install_info(&game);
        eprintln!("{i:?}");
        assert!(i.size_bytes > 10 << 30 && !i.languages.is_empty() && i.manifest.is_some());
    }

    #[test]
    fn every_fixture_parses() {
        for name in ["3539.yaml", "420.yaml", "2688.yaml", "5183.yaml"] {
            let mut info = GameInfo::default();
            apply_config(&mut info, &fixture(name));
            assert!(!info.name.is_empty(), "{name}");
            assert_eq!(info.publisher.as_deref(), Some("Ubisoft"), "{name}");
        }
    }

    #[test]
    fn games_api_fills_gaps() {
        let mut info = GameInfo {
            unavailable: UNAVAILABLE.iter().map(|s| s.to_string()).collect(),
            links: vec![Link { kind: "homepage".into(), url: "http://assassinscreed.ubi.com/".into() }],
            ..Default::default()
        };
        let g = crate::spaces::SpaceGame {
            space_id: "s".into(),
            display_description: Some(" Pirates. ".into()),
            release_at: Some("2013-11-19T00:00:00Z".into()),
            link_urls: [("website", Some("https://example.com/bf")), ("twitter", Some("https://twitter.com/ac")), ("reddit", None)]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.map(str::to_owned)))
                .collect(),
            ..Default::default()
        };
        apply_space(&mut info, &g);
        assert_eq!(info.release_date.as_deref(), Some("2013-11-19"));
        assert_eq!(info.description.as_deref(), Some("Pirates."));
        // The config's homepage stays; twitter is new.
        assert_eq!(info.links.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), ["homepage", "twitter"]);
        assert_eq!(info.unavailable, ["developer", "genres"]);
    }
}
