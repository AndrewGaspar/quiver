//! Owned games from Epic's library service and catalog (design/STORE-EPIC.md §3 C2, C3, C12).
//!
//! Each owned record (`library/api/public/items`) names a catalog item; the catalog says
//! what it is: title, art (`keyImages`), developer, description, platforms, its DLC
//! (`dlcItemList`) or its main game (`mainGameItem`), and `customAttributes`
//! (`CanRunOffline`, `CloudSaveFolder`, `FolderName`, `OwnershipToken`,
//! `ThirdPartyManaged*`, …). [`build`] turns records and catalog items into [`Title`]s:
//!
//! - **only playable games and apps are titles**, each with a real name and art. A tile
//!   never shows a hex id: everything else is in [`Built::hidden`] with the reason, for
//!   `quiver library --all`. Left out: Unreal Engine marketplace assets (namespace `ue`),
//!   private sandboxes, engines, plugins and assets, digital extras (soundtracks, art
//!   books), audience entitlements, items with no Windows build, games another launcher
//!   (not Ubisoft's) runs, and records whose catalog item Epic didn't return (asked again
//!   at every read until it does);
//! - **DLC** goes on its game's list as owned (by `mainGameItem`, the games' `dlcItemList`,
//!   or the one game in the namespace), never as a title of its own;
//! - **one game, one title:** a game owned by several accounts, or listed under several
//!   app names (one catalog item, or one namespace with the same name), is one title;
//! - **Ubisoft games sold on Epic** (`ThirdPartyManagedApp`/`ThirdPartyManagedProvider`
//!   naming Ubisoft or Uplay, or `partnerLinkType = ubisoft`) carry the Ubisoft product id
//!   from `GameID`, so the library folds them into their Ubisoft entry (STORES.md §2.4).
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
    /// Every app name owned for this game, `app_name` among them.
    pub apps: Vec<String>,
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

/// What a record is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Game,
    /// DLC or an add-on of the game whose catalog item id this is (empty: the game in the
    /// same namespace).
    Addon { main: String },
    /// Not a tile, and why (for `quiver library --all`).
    Skip(String),
}

/// An owned record that isn't in the library, and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hidden {
    pub app_name: String,
    pub namespace: String,
    pub catalog_item_id: String,
    /// Epic's sandbox name (often the game's name, even when the catalog is silent).
    pub sandbox: String,
    pub title: Option<String>,
    pub reason: String,
    pub owners: Vec<String>,
}

/// The library proper and the owned records left out of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Built {
    pub titles: Vec<Title>,
    pub hidden: Vec<Hidden>,
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

/// A 32-hex id (Epic's newer app names and every catalog id): never a name for people.
pub fn is_hex_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An entitlement that only unlocks something inside another game (`AUDIENCE` items,
/// in-game currency packs).
fn is_entitlement_only(item: &CatalogItem) -> bool {
    item.entitlement_type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("AUDIENCE")) || has_category(item, "audience")
}

/// What a record is, from the record and (when read) its catalog item. `dlc_of` maps DLC
/// catalog ids to their game's, from every item's `dlcItemList`.
pub fn classify(r: &LibraryRecord, item: Option<&CatalogItem>, dlc_of: &HashMap<String, String>) -> Kind {
    let skip = |s: &str| Kind::Skip(s.to_owned());
    if r.namespace == "ue" {
        return skip("Unreal Engine marketplace asset");
    }
    if r.sandbox_type.eq_ignore_ascii_case("PRIVATE") {
        return skip("private sandbox (a developer or test build)");
    }
    if r.app_name.is_empty() {
        return skip("no app to install");
    }
    if !r.record_type.is_empty() && !r.record_type.eq_ignore_ascii_case("APPLICATION") {
        return Kind::Skip(format!("not an application ({})", r.record_type));
    }
    let Some(item) = item else { return skip(UNRESOLVED) };
    if has_category(item, "engines") || has_category(item, "plugins") || has_category(item, "assets") {
        return skip("engine, plugin or asset");
    }
    if has_category(item, "digitalextras") {
        return skip("digital extra (soundtrack, art book…)");
    }
    if is_entitlement_only(item) {
        return skip("an entitlement inside another game");
    }
    if let Some(main) = item.main_game_item.as_ref().filter(|m| m.id != item.id) {
        return Kind::Addon { main: main.id.clone() };
    }
    if let Some(main) = dlc_of.get(&item.id).filter(|m| **m != item.id) {
        return Kind::Addon { main: main.clone() };
    }
    if has_category(item, "addons") {
        return Kind::Addon { main: String::new() };
    }
    if !has_category(item, "games") && !has_category(item, "applications") {
        let cats: Vec<&str> = item.categories.iter().map(|c| c.path.as_str()).collect();
        return Kind::Skip(format!("not a game or app ({})", if cats.is_empty() { "no categories".to_owned() } else { cats.join(", ") }));
    }
    if let Some(t) = item.entitlement_type.as_deref().filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("EXECUTABLE")) {
        return Kind::Skip(format!("nothing to install ({t})"));
    }
    let platforms = platforms(item);
    if !platforms.is_empty() && !platforms.iter().any(|p| p == "Windows" || p == "Win32") {
        return skip("no Windows build");
    }
    Kind::Game
}

/// The reason for a record whose catalog item Epic didn't return.
pub const UNRESOLVED: &str = "Epic's catalog didn't describe it (asked again at the next library read)";

fn platforms(item: &CatalogItem) -> Vec<String> {
    let mut v: Vec<String> = item.release_info.iter().flat_map(|r| r.platform.iter().cloned()).collect();
    v.sort();
    v.dedup();
    v
}

fn image(item: &CatalogItem, kinds: &[&str]) -> Option<String> {
    kinds.iter().find_map(|k| item.key_images.iter().find(|i| i.kind == *k && i.url.starts_with("https://")).map(|i| i.url.clone()))
}

/// Art for the three slots, from `keyImages`.
pub fn art_of(item: &CatalogItem) -> TitleArt {
    TitleArt {
        tall: image(item, &["DieselGameBoxTall", "OfferImageTall", "Thumbnail", "CodeRedemption_340x440"]),
        wide: image(item, &["DieselGameBox", "DieselStoreFrontWide", "OfferImageWide", "DieselGameBoxWide"]),
        logo: image(item, &["DieselGameBoxLogo"]),
    }
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

/// A DLC list entry worth showing: real DLC, not the audience entitlements, engine kits
/// and editor tools some catalogs list next to it.
fn listed_dlc(d: &CatalogItem) -> bool {
    !d.id.is_empty()
        && !d.title.trim().is_empty()
        && !is_entitlement_only(d)
        && !has_category(d, "engines")
        && !has_category(d, "plugins")
        && !has_category(d, "digitalextras")
        && (has_category(d, "games") || has_category(d, "applications") || has_category(d, "addons"))
}

fn title_of(r: &LibraryRecord, item: &CatalogItem) -> Title {
    let description = item.description.clone().or(item.long_description.clone()).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty() && *d != item.title);
    Title {
        app_name: r.app_name.clone(),
        apps: vec![r.app_name.clone()],
        namespace: r.namespace.clone(),
        catalog_item_id: r.catalog_item_id.clone(),
        title: item.title.trim().to_owned(),
        description,
        developer: item.developer.clone().filter(|d| !d.trim().is_empty()),
        art: art_of(item),
        dlc: item
            .dlc_item_list
            .iter()
            .filter(|d| listed_dlc(d))
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
    }
}

/// Catalog key: `<namespace>/<catalog item id>`.
pub fn catalog_key(namespace: &str, id: &str) -> String {
    format!("{namespace}/{id}")
}

/// Why a game can't be a tile, if it can't: no real name, no art, or another launcher
/// (not Ubisoft's, whose games fold into their Ubisoft entries) would have to run it.
fn unfit(t: &Title) -> Option<String> {
    if t.title.is_empty() || is_hex_id(&t.title) {
        return Some("Epic's catalog gives it no name".into());
    }
    if t.art.tall.is_none() && t.art.wide.is_none() {
        return Some("Epic's catalog has no art for it".into());
    }
    match &t.third_party {
        Some(tp) if !tp.is_ubisoft() => Some(format!("managed by {}, which Quiver doesn't run", tp.client_name())),
        _ => None,
    }
}

/// Whether `candidate` should name its group instead of the current app: the app the
/// catalog lists for the item wins, else the first acquired.
fn better_primary(candidate: &LibraryRecord, current: &Title, item: &CatalogItem) -> bool {
    let listed = |app: &str| item.release_info.iter().any(|ri| ri.app_id == app);
    match (listed(&candidate.app_name), listed(&current.app_name)) {
        (true, false) => true,
        (false, true) => false,
        _ => matches!((candidate.acquisition_date, current.acquired), (Some(a), Some(b)) if a < b),
    }
}

fn normalized(title: &str) -> String {
    title.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// The library from every account's records: games and apps with a real name and art.
/// A game owned by several accounts, or listed under several app names (one catalog
/// item, or one namespace with the same title), is one title with every owner and app.
/// DLC goes on its game's list. Everything else is in [`Built::hidden`] with the reason.
/// Titles are sorted by name.
pub fn build(accounts: &[(String, Vec<LibraryRecord>)], catalog: &HashMap<String, CatalogItem>) -> Built {
    let mut dlc_of: HashMap<String, String> = HashMap::new();
    for item in catalog.values() {
        for d in &item.dlc_item_list {
            dlc_of.entry(d.id.clone()).or_insert_with(|| item.id.clone());
        }
    }
    // Grouped by catalog item here, then by namespace and name below.
    let mut games: BTreeMap<String, Title> = BTreeMap::new();
    let mut addons: Vec<(String, String, &LibraryRecord, Option<&CatalogItem>)> = Vec::new();
    let mut hidden: BTreeMap<String, Hidden> = BTreeMap::new();
    fn hide(hidden: &mut BTreeMap<String, Hidden>, account: &str, r: &LibraryRecord, item: Option<&CatalogItem>, reason: String) {
        let h = hidden.entry(r.app_name.clone()).or_insert_with(|| Hidden {
            app_name: r.app_name.clone(),
            namespace: r.namespace.clone(),
            catalog_item_id: r.catalog_item_id.clone(),
            sandbox: r.sandbox_name.clone(),
            title: item.map(|i| i.title.trim().to_owned()).filter(|t| !t.is_empty()),
            reason,
            owners: Vec::new(),
        });
        push_unique(&mut h.owners, account.to_owned());
    }
    for (account, records) in accounts {
        for r in records {
            let item = catalog.get(&catalog_key(&r.namespace, &r.catalog_item_id));
            match (classify(r, item, &dlc_of), item) {
                (Kind::Skip(why), _) => hide(&mut hidden, account, r, item, why),
                (Kind::Addon { main }, _) => addons.push((account.clone(), main, r, item)),
                (Kind::Game, None) => hide(&mut hidden, account, r, None, UNRESOLVED.into()),
                (Kind::Game, Some(item)) => {
                    let t = games.entry(catalog_key(&r.namespace, &r.catalog_item_id)).or_insert_with(|| title_of(r, item));
                    if !t.apps.contains(&r.app_name) {
                        t.apps.push(r.app_name.clone());
                        if better_primary(r, t, item) {
                            t.app_name = r.app_name.clone();
                            t.acquired = r.acquisition_date;
                        }
                    }
                    push_unique(&mut t.owners, account.clone());
                }
            }
        }
    }
    // One namespace, one name: the same game under two catalog items.
    let mut titles: Vec<Title> = Vec::new();
    for t in games.into_values() {
        match titles.iter_mut().find(|o| o.namespace == t.namespace && normalized(&o.title) == normalized(&t.title)) {
            Some(o) => {
                t.apps.into_iter().for_each(|a| push_unique(&mut o.apps, a));
                t.owners.into_iter().for_each(|w| push_unique(&mut o.owners, w));
            }
            None => titles.push(t),
        }
    }
    for (account, main, r, item) in addons {
        // The main game by catalog id, else the one game in the same namespace.
        let by_id = titles.iter().position(|t| !main.is_empty() && t.catalog_item_id == main);
        let in_ns: Vec<usize> = titles.iter().enumerate().filter(|(_, t)| t.namespace == r.namespace).map(|(i, _)| i).collect();
        let base = by_id.or(if in_ns.len() == 1 { Some(in_ns[0]) } else { None });
        let Some(t) = base.map(|i| &mut titles[i]) else {
            let main_title = item.and_then(|i| i.main_game_item.as_ref()).map(|m| m.title.trim().to_owned()).filter(|m| !m.is_empty());
            let reason = match main_title {
                Some(m) => format!("DLC for {m}, which isn't in your library"),
                None => "DLC or add-on for a game that isn't in your library".to_owned(),
            };
            hide(&mut hidden, &account, r, item, reason);
            continue;
        };
        match t.dlc.iter_mut().find(|d| d.catalog_item_id == r.catalog_item_id) {
            Some(d) => {
                d.owned = true;
                d.app_name = Some(r.app_name.clone());
            }
            None => {
                let name = item.map(|i| i.title.trim().to_owned()).filter(|n| !n.is_empty() && !is_hex_id(n));
                t.dlc.push(DlcInfo { catalog_item_id: r.catalog_item_id.clone(), title: name.unwrap_or_else(|| r.app_name.clone()), app_name: Some(r.app_name.clone()), owned: true });
            }
        }
    }
    let mut visible = Vec::new();
    for t in titles {
        match unfit(&t) {
            Some(reason) => {
                hidden.insert(
                    t.app_name.clone(),
                    Hidden {
                        app_name: t.app_name.clone(),
                        namespace: t.namespace.clone(),
                        catalog_item_id: t.catalog_item_id.clone(),
                        sandbox: String::new(),
                        title: Some(t.title.clone()).filter(|n| !n.is_empty()),
                        reason,
                        owners: t.owners.clone(),
                    },
                );
            }
            None => visible.push(t),
        }
    }
    visible.sort_by_key(|t| t.title.to_lowercase());
    let mut hidden: Vec<Hidden> = hidden.into_values().collect();
    hidden.sort_by(|a, b| (&a.reason, a.title.as_deref().unwrap_or(&a.sandbox)).cmp(&(&b.reason, b.title.as_deref().unwrap_or(&b.sandbox))));
    Built { titles: visible, hidden }
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
        let built = build(&accounts, &catalog);
        let titles = &built.titles;
        let names: Vec<&str> = titles.iter().map(|t| t.title.as_str()).collect();
        // UE assets, the mobile-only game, the private sandbox, the soundtrack and the item
        // the catalog didn't return are left out; the DLC is on its game.
        assert_eq!(names, ["Alan Wake Remastered", "Assassin's Creed Origins", "Celeste"]);
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
        assert_eq!(owned, [("The Hidden Ones", true), ("The Curse of the Pharaohs", false)]);
        let hidden: Vec<(&str, &str)> = built.hidden.iter().map(|h| (h.app_name.as_str(), h.reason.as_str())).collect();
        assert_eq!(
            hidden,
            [
                ("MysteryApp", UNRESOLVED),
                ("ForestPack", "Unreal Engine marketplace asset"),
                ("CamelliaOST", "digital extra (soundtrack, art book…)"),
                ("MobileOnly", "no Windows build"),
                ("PrivateThing", "private sandbox (a developer or test build)"),
            ]
        );
        assert_eq!(built.hidden[0].owners, ["acct-two"]);
    }

    /// The shapes behind the hex ids and odd entries a real library showed: a game listed
    /// with its DLC under hex app names in one namespace, audience entitlements in DLC
    /// lists, an EA game, a game with no art, one game under two app names.
    #[test]
    fn only_playable_titles_with_names_and_art() {
        let items: HashMap<String, CatalogItem> = serde_json::from_str(include_str!("../tests/fixtures/catalog_mixed.json")).unwrap();
        let catalog: HashMap<String, CatalogItem> = items.into_values().map(|i| (catalog_key(&i.namespace, &i.id), i)).collect();
        let rec = |ns: &str, item: &str, app: &str| LibraryRecord {
            namespace: ns.into(),
            catalog_item_id: item.into(),
            app_name: app.into(),
            sandbox_name: "Fallout: New Vegas".into(),
            sandbox_type: "PUBLIC".into(),
            record_type: "APPLICATION".into(),
            ..Default::default()
        };
        let records = vec![
            rec("ns-fnv", "item-fnv", "5daeb974a22a435988892319b3a4f476"),
            rec("ns-fnv", "item-fnv-dlc1", "ee9a44b4530942499ef1c8c390731fce"),
            rec("ns-fnv", "item-fnv-dlc2", "b290229eb58045cbab9501640f3278f3"),
            rec("ns-fnv", "item-fnv-gone", "4fa3d8d9b2cb4714a19a38d1a598be8f"),
            rec("ns-bf2", "item-bf2", "MtMassive"),
            rec("ns-noart", "item-noart", "NoArt"),
            rec("ns-hexname", "item-hexname", "8935bb3e1420443a9789fe01758039a5"),
            rec("ns-two", "item-two-a", "TwoA"),
            rec("ns-two", "item-two-b", "c8dae1ab0570475a8b38a9041e614840"),
            rec("ns-lonely", "item-lonely-dlc", "LonelyDLC"),
        ];
        let built = build(&[("acct".into(), records)], &catalog);
        let names: Vec<(&str, &str)> = built.titles.iter().map(|t| (t.title.as_str(), t.app_name.as_str())).collect();
        assert_eq!(names, [("Fallout: New Vegas", "5daeb974a22a435988892319b3a4f476"), ("Two Names", "TwoA")]);
        let fnv = &built.titles[0];
        // Owned DLC (one by mainGameItem, one only from the game's list) is on the game;
        // the audience entitlement and the engine kit in the list are not.
        let dlc: Vec<(&str, bool)> = fnv.dlc.iter().map(|d| (d.title.as_str(), d.owned)).collect();
        assert_eq!(dlc, [("Honest Hearts", true), ("Old World Blues", false), ("Dead Money", true)]);
        assert_eq!(built.titles[1].apps, ["TwoA", "c8dae1ab0570475a8b38a9041e614840"]);
        let hidden: Vec<(&str, &str)> = built.hidden.iter().map(|h| (h.app_name.as_str(), h.reason.as_str())).collect();
        assert_eq!(
            hidden,
            [
                ("LonelyDLC", "DLC for Some Other Game, which isn't in your library"),
                ("4fa3d8d9b2cb4714a19a38d1a598be8f", UNRESOLVED),
                ("8935bb3e1420443a9789fe01758039a5", "Epic's catalog gives it no name"),
                ("NoArt", "Epic's catalog has no art for it"),
                ("MtMassive", "managed by the EA app, which Quiver doesn't run"),
            ]
        );
        assert_eq!(built.hidden[1].sandbox, "Fallout: New Vegas");
        assert!(built.titles.iter().all(|t| !is_hex_id(&t.title)));
    }

    #[test]
    fn classify_rules() {
        let none = HashMap::new();
        let r = |ns: &str, sandbox: &str, app: &str| LibraryRecord { namespace: ns.into(), sandbox_type: sandbox.into(), app_name: app.into(), ..Default::default() };
        let skip = |s: &str| Kind::Skip(s.to_owned());
        assert_eq!(classify(&r("ue", "PUBLIC", "x"), None, &none), skip("Unreal Engine marketplace asset"));
        assert_eq!(classify(&r("n", "PRIVATE", "x"), None, &none), skip("private sandbox (a developer or test build)"));
        assert_eq!(classify(&r("n", "PUBLIC", ""), None, &none), skip("no app to install"));
        assert_eq!(classify(&r("n", "PUBLIC", "x"), None, &none), skip(UNRESOLVED));
        let mut item = CatalogItem { id: "i".into(), ..Default::default() };
        item.categories.push(crate::api::Category { path: "plugins/engine".into() });
        assert_eq!(classify(&r("n", "PUBLIC", "x"), Some(&item), &none), skip("engine, plugin or asset"));
        let mut item = CatalogItem { id: "i".into(), ..Default::default() };
        item.categories.push(crate::api::Category { path: "games".into() });
        assert_eq!(classify(&r("n", "PUBLIC", "x"), Some(&item), &none), Kind::Game);
        item.entitlement_type = Some("AUDIENCE".into());
        assert_eq!(classify(&r("n", "PUBLIC", "x"), Some(&item), &none), skip("an entitlement inside another game"));
        item.entitlement_type = None;
        let dlc_of = HashMap::from([("i".to_owned(), "main".to_owned())]);
        assert_eq!(classify(&r("n", "PUBLIC", "x"), Some(&item), &dlc_of), Kind::Addon { main: "main".into() });
        item.categories = vec![crate::api::Category { path: "bundles".into() }];
        assert_eq!(classify(&r("n", "PUBLIC", "x"), Some(&item), &none), skip("not a game or app (bundles)"));
        let mut rec = r("n", "PUBLIC", "x");
        rec.record_type = "SKIN".into();
        assert_eq!(classify(&rec, None, &none), skip("not an application (SKIN)"));
        assert!(is_hex_id("8935bb3e1420443a9789fe01758039a5") && !is_hex_id("Camellia") && !is_hex_id("8935bb3e1420443a9789fe01758039a"));
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
