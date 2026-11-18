//! Owned games from Epic's library service and catalog (design/STORE-EPIC.md §3 C2, C3, C12).
//!
//! Each owned record (`library/api/public/items`) names a catalog item; the catalog says
//! what it is: title, art (`keyImages`), developer, description, platforms, its DLC
//! (`dlcItemList`) or its main game (`mainGameItem`), and `customAttributes`
//! (`CanRunOffline`, `CloudSaveFolder`, `FolderName`, `OwnershipToken`,
//! `ThirdPartyManaged*`, …). [`build`] turns records and catalog items into [`Title`]s:
//!
//! - **left out:** Unreal Engine marketplace items (namespace `ue`), private sandboxes,
//!   engines and plugins, and items with no Windows build (mobile only);
//! - **DLC and extras** are not titles of their own: they mark their main game's DLC list
//!   as owned;
//! - **Ubisoft games sold on Epic** (`ThirdPartyManagedApp`/`ThirdPartyManagedProvider`
//!   naming Ubisoft or Uplay, or `partnerLinkType = ubisoft`) carry the Ubisoft product id
//!   from `GameID`, so the library folds them into their Ubisoft entry (STORES.md §2.4);
//! - a record whose catalog item couldn't be read stays in the library under its app
//!   name, so a catalog failure never hides a game.
//!
//! Both are cached under `<data>/epic/cache/`: the library per account (the last good
//! read, used when Epic can't be reached) and catalog items (they rarely change).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{CatalogItem, LibraryRecord};

/// Re-read catalog items older than this.
pub const CATALOG_TTL: chrono::Duration = chrono::Duration::days(3);

/// A game in the Epic library, ready for the library and the game page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Title {
    /// Epic's stable id for the game (`Camellia`): the `id` of `epic:<app>`.
    pub app_name: String,
    pub namespace: String,
    pub catalog_item_id: String,
    pub title: String,
    pub description: Option<String>,
    pub developer: Option<String>,
    pub art: TitleArt,
    pub dlc: Vec<DlcInfo>,
    pub third_party: Option<ThirdParty>,
    pub attributes: Attributes,
    pub platforms: Vec<String>,
    /// Epic account ids that own it, in the order the accounts were read.
    pub owners: Vec<String>,
    pub acquired: Option<DateTime<Utc>>,
    /// The catalog item couldn't be read: only the library record is known.
    pub catalog_missing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleArt {
    /// Portrait box art (`DieselGameBoxTall`).
    pub tall: Option<String>,
    /// Wide hero (`DieselGameBox`).
    pub wide: Option<String>,
    /// Transparent logo (`DieselGameBoxLogo`).
    pub logo: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DlcInfo {
    pub catalog_item_id: String,
    pub title: String,
    /// The DLC's own app (installable DLC), when the account owns it.
    pub app_name: Option<String>,
    pub owned: bool,
}

/// A game Epic sells but another store's client manages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThirdParty {
    /// As the catalog names it: `UbisoftConnect`, `Uplay`, `Origin`, …
    pub provider: String,
    /// The Ubisoft product id (`GameID`), for Ubisoft games.
    pub ubisoft_product: Option<u32>,
}

impl ThirdParty {
    pub fn is_ubisoft(&self) -> bool {
        let p = self.provider.to_ascii_lowercase();
        p.contains("ubisoft") || p.contains("uplay")
    }

    /// For people: "Ubisoft Connect", "the EA app".
    pub fn client_name(&self) -> String {
        let p = self.provider.to_ascii_lowercase();
        if self.is_ubisoft() {
            "Ubisoft Connect".into()
        } else if p.contains("origin") || p == "ea" || p.contains("eaapp") {
            "the EA app".into()
        } else {
            self.provider.clone()
        }
    }
}

/// The catalog's launch-related attributes (used from phase 2 on; shown on the page now).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes {
    pub can_run_offline: bool,
    pub cloud_save_folder: Option<String>,
    pub folder_name: Option<String>,
    pub ownership_token: bool,
    pub additional_command_line: Option<String>,
}

/// Why a record isn't a title, or what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Game,
    /// DLC or an extra of the game whose catalog item id this is.
    Addon { main: String },
    Skip(&'static str),
}

fn attr<'a>(item: &'a CatalogItem, key: &str) -> Option<&'a str> {
    item.custom_attributes.get(key).map(|a| a.value.trim()).filter(|v| !v.is_empty())
}

fn flag(item: &CatalogItem, key: &str) -> bool {
    attr(item, key).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn has_category(item: &CatalogItem, path: &str) -> bool {
    item.categories.iter().any(|c| c.path == path || c.path.starts_with(&format!("{path}/")))
}

/// What a record is, from the record and (when read) its catalog item.
pub fn classify(r: &LibraryRecord, item: Option<&CatalogItem>) -> Kind {
    if r.namespace == "ue" {
        return Kind::Skip("Unreal Engine marketplace item");
    }
    if r.sandbox_type.eq_ignore_ascii_case("PRIVATE") {
        return Kind::Skip("private sandbox");
    }
    if r.app_name.is_empty() {
        return Kind::Skip("no app");
    }
    let Some(item) = item else { return Kind::Game };
    if has_category(item, "engines") || has_category(item, "plugins") {
        return Kind::Skip("engine or plugin");
    }
    if let Some(main) = item.main_game_item.as_ref().filter(|m| m.id != item.id) {
        return Kind::Addon { main: main.id.clone() };
    }
    if has_category(item, "addons") || has_category(item, "digitalextras") {
        return Kind::Addon { main: String::new() };
    }
    let platforms = platforms(item);
    if !platforms.is_empty() && !platforms.iter().any(|p| p == "Windows" || p == "Win32") {
        return Kind::Skip("no Windows build");
    }
    Kind::Game
}

fn platforms(item: &CatalogItem) -> Vec<String> {
    let mut v: Vec<String> = item.release_info.iter().flat_map(|r| r.platform.iter().cloned()).collect();
    v.sort();
    v.dedup();
    v
}

fn image(item: &CatalogItem, kinds: &[&str]) -> Option<String> {
    kinds.iter().find_map(|k| item.key_images.iter().find(|i| i.kind == *k && i.url.starts_with("https://")).map(|i| i.url.clone()))
}

/// A Ubisoft (or other) client manages this game.
pub fn third_party(item: &CatalogItem) -> Option<ThirdParty> {
    let provider = attr(item, "ThirdPartyManagedProvider").or(attr(item, "ThirdPartyManagedApp"));
    let partner = attr(item, "partnerLinkType");
    let provider = match (provider, partner) {
        (Some(p), _) => p.to_owned(),
        (None, Some(p)) if p.eq_ignore_ascii_case("ubisoft") => "UbisoftConnect".to_owned(),
        _ => return None,
    };
    let mut tp = ThirdParty { provider, ubisoft_product: None };
    if tp.is_ubisoft() || partner.is_some_and(|p| p.eq_ignore_ascii_case("ubisoft")) {
        tp.ubisoft_product = attr(item, "GameID").and_then(|g| g.parse().ok()).filter(|&id| id > 0);
        if !tp.is_ubisoft() {
            tp.provider = "UbisoftConnect".into();
        }
    }
    Some(tp)
}

fn title_of(r: &LibraryRecord, item: Option<&CatalogItem>) -> Title {
    let Some(item) = item else {
        return Title {
            app_name: r.app_name.clone(),
            namespace: r.namespace.clone(),
            catalog_item_id: r.catalog_item_id.clone(),
            title: r.app_name.clone(),
            acquired: r.acquisition_date,
            catalog_missing: true,
            ..Default::default()
        };
    };
    let description = item.description.clone().or(item.long_description.clone()).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty() && *d != item.title);
    Title {
        app_name: r.app_name.clone(),
        namespace: r.namespace.clone(),
        catalog_item_id: r.catalog_item_id.clone(),
        title: if item.title.trim().is_empty() { r.app_name.clone() } else { item.title.trim().to_owned() },
        description,
        developer: item.developer.clone().filter(|d| !d.trim().is_empty()),
        art: TitleArt {
            tall: image(item, &["DieselGameBoxTall", "OfferImageTall", "Thumbnail"]),
            wide: image(item, &["DieselGameBox", "DieselStoreFrontWide", "OfferImageWide", "DieselGameBoxWide"]),
            logo: image(item, &["DieselGameBoxLogo"]),
        },
        dlc: item
            .dlc_item_list
            .iter()
            .filter(|d| !d.id.is_empty())
            .map(|d| DlcInfo { catalog_item_id: d.id.clone(), title: d.title.trim().to_owned(), app_name: None, owned: false })
            .collect(),
        third_party: third_party(item),
        attributes: Attributes {
            can_run_offline: flag(item, "CanRunOffline"),
            cloud_save_folder: attr(item, "CloudSaveFolder").map(str::to_owned),
            folder_name: attr(item, "FolderName").map(str::to_owned),
            ownership_token: flag(item, "OwnershipToken"),
            additional_command_line: attr(item, "AdditionalCommandLine").map(str::to_owned),
        },
        platforms: platforms(item),
        owners: Vec::new(),
        acquired: r.acquisition_date,
        catalog_missing: false,
    }
}

/// Catalog key: `<namespace>/<catalog item id>`.
pub fn catalog_key(namespace: &str, id: &str) -> String {
    format!("{namespace}/{id}")
}

/// Titles from every account's records. A game owned by several accounts is one title
/// with every owner. Sorted by title.
pub fn build(accounts: &[(String, Vec<LibraryRecord>)], catalog: &HashMap<String, CatalogItem>) -> Vec<Title> {
    let mut titles: BTreeMap<String, Title> = BTreeMap::new();
    let mut addons: Vec<(String, &LibraryRecord, Option<&CatalogItem>)> = Vec::new();
    for (account, records) in accounts {
        for r in records {
            let item = catalog.get(&catalog_key(&r.namespace, &r.catalog_item_id));
            match classify(r, item) {
                Kind::Skip(why) => tracing::trace!(app = r.app_name, "Epic library: left out ({why})"),
                Kind::Addon { main } => addons.push((main, r, item)),
                Kind::Game => {
                    let t = titles.entry(r.app_name.clone()).or_insert_with(|| title_of(r, item));
                    if !t.owners.contains(account) {
                        t.owners.push(account.clone());
                    }
                }
            }
        }
    }
    for (main, r, item) in addons {
        // The main game by catalog id, else the game in the same namespace.
        let base = titles.values_mut().find(|t| !main.is_empty() && t.catalog_item_id == main).map(|t| t.app_name.clone());
        let base = base.or_else(|| titles.values().find(|t| t.namespace == r.namespace).map(|t| t.app_name.clone()));
        let Some(t) = base.and_then(|b| titles.get_mut(&b)) else { continue };
        match t.dlc.iter_mut().find(|d| d.catalog_item_id == r.catalog_item_id) {
            Some(d) => {
                d.owned = true;
                d.app_name = Some(r.app_name.clone());
            }
            None => t.dlc.push(DlcInfo {
                catalog_item_id: r.catalog_item_id.clone(),
                title: item.map(|i| i.title.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| r.app_name.clone()),
                app_name: Some(r.app_name.clone()),
                owned: true,
            }),
        }
    }
    let mut v: Vec<Title> = titles.into_values().collect();
    v.sort_by_key(|t| t.title.to_lowercase());
    v
}

// ---- caches

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedLibrary {
    pub fetched_at: DateTime<Utc>,
    pub records: Vec<LibraryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedItem {
    pub fetched_at: DateTime<Utc>,
    pub item: CatalogItem,
}

/// `<data>/epic/cache/`.
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(epic_dir: &Path) -> Self {
        Cache { dir: epic_dir.join("cache") }
    }

    fn library_path(&self, account_id: &str) -> PathBuf {
        self.dir.join(format!("library-{account_id}.json"))
    }

    pub fn library(&self, account_id: &str) -> Option<CachedLibrary> {
        serde_json::from_slice(&std::fs::read(self.library_path(account_id)).ok()?).ok()
    }

    pub fn save_library(&self, account_id: &str, c: &CachedLibrary) -> Result<()> {
        write_atomic(&self.library_path(account_id), &serde_json::to_vec(c)?)
    }

    pub fn forget_library(&self, account_id: &str) {
        let _ = std::fs::remove_file(self.library_path(account_id));
    }

    pub fn catalog(&self) -> HashMap<String, CachedItem> {
        std::fs::read(self.dir.join("catalog.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_catalog(&self, c: &HashMap<String, CachedItem>) -> Result<()> {
        // Sorted, so the file diffs and reads well.
        let sorted: BTreeMap<&String, &CachedItem> = c.iter().collect();
        write_atomic(&self.dir.join("catalog.json"), &serde_json::to_vec(&sorted)?)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("cache path")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    type Fixture = (Vec<(String, Vec<LibraryRecord>)>, HashMap<String, CatalogItem>);

    /// Two accounts' libraries and the catalog for them, from the fixtures. The ids are
    /// invented; the shapes follow Epic's replies.
    pub fn fixture() -> Fixture {
        let page = |s: &str| -> Vec<LibraryRecord> { serde_json::from_value(serde_json::from_str::<serde_json::Value>(s).unwrap()["records"].clone()).unwrap() };
        let mut first = page(include_str!("../tests/fixtures/library_page1.json"));
        first.extend(page(include_str!("../tests/fixtures/library_page2.json")));
        let second = page(include_str!("../tests/fixtures/library_second_account.json"));
        let mut catalog = HashMap::new();
        for f in [include_str!("../tests/fixtures/catalog_alanwake.json"), include_str!("../tests/fixtures/catalog_origins.json"), include_str!("../tests/fixtures/catalog_misc.json")] {
            let items: HashMap<String, CatalogItem> = serde_json::from_str(f).unwrap();
            for (_, i) in items {
                catalog.insert(catalog_key(&i.namespace, &i.id), i);
            }
        }
        (vec![("acct-one".into(), first), ("acct-two".into(), second)], catalog)
    }

    #[test]
    fn titles_from_records_and_catalog() {
        let (accounts, catalog) = fixture();
        let titles = build(&accounts, &catalog);
        let names: Vec<&str> = titles.iter().map(|t| t.title.as_str()).collect();
        // UE assets, the mobile-only game and the private sandbox are left out; the DLC and
        // the soundtrack are on their games, not titles; an item the catalog didn't return
        // stays, named by its app.
        assert_eq!(names, ["Alan Wake Remastered", "Assassin's Creed Origins", "Celeste", "MysteryApp"]);
        let aw = &titles[0];
        assert_eq!((aw.app_name.as_str(), aw.namespace.as_str()), ("Kestrel", "ns-alanwake"));
        assert_eq!(aw.developer.as_deref(), Some("Remedy Entertainment"));
        assert!(aw.art.tall.as_deref().unwrap().ends_with("tall.jpg") && aw.art.wide.as_deref().unwrap().ends_with("wide.jpg") && aw.art.logo.is_some());
        assert!(aw.attributes.can_run_offline && !aw.attributes.ownership_token);
        assert_eq!(aw.attributes.folder_name.as_deref(), Some("AlanWakeRemastered"));
        assert_eq!(aw.third_party, None);
        assert_eq!(aw.platforms, ["Windows"]);
        assert_eq!(aw.owners, ["acct-one"]);
        // Origins: a Ubisoft game sold on Epic, owned by both accounts, with its DLC.
        let ac = &titles[1];
        assert_eq!(ac.third_party.as_ref().map(|t| (t.is_ubisoft(), t.ubisoft_product, t.client_name())), Some((true, Some(3539), "Ubisoft Connect".to_owned())));
        assert_eq!(ac.owners, ["acct-one", "acct-two"]);
        let owned: Vec<(&str, bool)> = ac.dlc.iter().map(|d| (d.title.as_str(), d.owned)).collect();
        assert_eq!(owned, [("The Hidden Ones", true), ("The Curse of the Pharaohs", false), ("Origins Soundtrack", true)]);
        assert!(titles[3].catalog_missing && titles[3].art.tall.is_none());
    }

    #[test]
    fn classify_rules() {
        let r = |ns: &str, sandbox: &str, app: &str| LibraryRecord { namespace: ns.into(), sandbox_type: sandbox.into(), app_name: app.into(), ..Default::default() };
        assert_eq!(classify(&r("ue", "PUBLIC", "x"), None), Kind::Skip("Unreal Engine marketplace item"));
        assert_eq!(classify(&r("n", "PRIVATE", "x"), None), Kind::Skip("private sandbox"));
        assert_eq!(classify(&r("n", "PUBLIC", ""), None), Kind::Skip("no app"));
        assert_eq!(classify(&r("n", "PUBLIC", "x"), None), Kind::Game);
        let mut item = CatalogItem { id: "i".into(), ..Default::default() };
        item.categories.push(crate::api::Category { path: "plugins/engine".into() });
        assert_eq!(classify(&r("n", "PUBLIC", "x"), Some(&item)), Kind::Skip("engine or plugin"));
    }

    #[test]
    fn third_party_forms() {
        let item = |pairs: &[(&str, &str)]| CatalogItem {
            custom_attributes: pairs.iter().map(|(k, v)| ((*k).to_owned(), crate::api::Attribute { kind: "STRING".into(), value: (*v).to_owned() })).collect(),
            ..Default::default()
        };
        // Older catalogs: ThirdPartyManagedApp = Uplay; the id may be missing.
        let tp = third_party(&item(&[("ThirdPartyManagedApp", "Uplay"), ("GameID", "420")])).unwrap();
        assert_eq!((tp.is_ubisoft(), tp.ubisoft_product), (true, Some(420)));
        let tp = third_party(&item(&[("partnerLinkType", "ubisoft")])).unwrap();
        assert_eq!((tp.provider.as_str(), tp.ubisoft_product), ("UbisoftConnect", None));
        let ea = third_party(&item(&[("ThirdPartyManagedApp", "Origin"), ("GameID", "12")])).unwrap();
        assert_eq!((ea.is_ubisoft(), ea.ubisoft_product, ea.client_name()), (false, None, "the EA app".to_owned()));
        assert_eq!(third_party(&item(&[("CanRunOffline", "true")])), None);
        assert_eq!(third_party(&item(&[("ThirdPartyManagedApp", "Uplay"), ("GameID", "not-a-number")])).unwrap().ubisoft_product, None);
    }

    #[test]
    fn caches_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Cache::new(tmp.path());
        assert!(c.library("a").is_none() && c.catalog().is_empty());
        let (accounts, catalog) = fixture();
        let now = Utc::now();
        c.save_library("a", &CachedLibrary { fetched_at: now, records: accounts[0].1.clone() }).unwrap();
        assert_eq!(c.library("a").unwrap().records, accounts[0].1);
        let items: HashMap<String, CachedItem> = catalog.into_iter().map(|(k, item)| (k, CachedItem { fetched_at: now, item })).collect();
        c.save_catalog(&items).unwrap();
        assert_eq!(c.catalog().len(), items.len());
        c.forget_library("a");
        assert!(c.library("a").is_none());
    }
}
