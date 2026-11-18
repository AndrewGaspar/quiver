//! The whole Epic flow on recorded JSON ([`Fake`]): no request leaves the process.

use std::sync::Arc;

use chrono::{Duration, Utc};
use quiver_api::stores::{AccountState, Relation};
use quiver_api::{GameRef, LibraryEntry};
use quiver_core::stores::StoreProvider;

use super::*;
use crate::http::{Fake, Method};
use crate::provider::EpicProvider;
use crate::secrets::Memory;

const ID: &str = "8645b4947bbc4c0092a8b7236df169d1";
const CODE: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";

/// A token reply with lifetimes relative to now (the fixtures' absolute times would make
/// tests depend on the clock).
fn token(access: &str, refresh: &str, account: &str, name: &str, expires_in: i64) -> String {
    serde_json::json!({
        "access_token": access, "expires_in": expires_in, "token_type": "bearer",
        "refresh_token": refresh, "refresh_expires": 30 * 86400,
        "account_id": account, "displayName": name, "client_id": config::EGL_CLIENT_ID,
    })
    .to_string()
}

struct Rig {
    _tmp: tempfile::TempDir,
    fake: Arc<Fake>,
    secrets: Arc<Memory>,
    epic: Arc<Epic>,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new());
    let secrets = Arc::new(Memory::new());
    let epic = Arc::new(Epic::new(tmp.path(), fake.clone(), secrets.clone()));
    Rig { _tmp: tmp, fake, secrets, epic }
}

impl Rig {
    fn stored(&self, id: &str) -> Option<Tokens> {
        self.secrets.get(&entry_name(id)).unwrap().map(|j| serde_json::from_str(&j).unwrap())
    }

    async fn login(&self, access: &str, refresh: &str, id: &str, name: &str) -> EpicAccount {
        self.fake.reply("/oauth/token", 200, &token(access, refresh, id, name, 8 * 3600));
        self.fake.reply(&format!("/public/account/{id}/externalAuths"), 200, if id == ID { include_str!("../tests/fixtures/external_auths.json") } else { "[]" });
        self.fake.reply(&format!("/public/account/{id}"), 200, &format!(r#"{{"id":"{id}","displayName":"{name}"}}"#));
        self.epic.login(CODE).await.unwrap()
    }
}

#[tokio::test]
async fn sign_in_stores_the_session_in_the_keyring_only() {
    let r = rig();
    assert!(r.epic.accounts().unwrap().is_empty());
    let a = r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    assert_eq!((a.display_name.as_str(), a.default, a.state), ("Player One", true, AccountState::SignedIn));
    assert_eq!(a.ubisoft.as_ref().map(|u| (u.id.as_str(), u.name.as_deref())), Some(("a1b2c3d4-0000-4000-8000-0000fixture0", Some("PlayerOneUbi"))));
    assert_eq!(r.secrets.entries(), [format!("epic:{ID}")]);
    assert_eq!(r.stored(ID).unwrap().refresh_token.expose(), "eg1~r1");
    // The index on disk has no token in it.
    let index = std::fs::read_to_string(r._tmp.path().join("epic/accounts.json")).unwrap();
    assert!(index.contains("Player One") && !index.contains("eg1~"), "{index}");
    let form = r.fake.requests()[0].form.clone().unwrap();
    assert!(form.contains(&("code".into(), CODE.into())));
    // The pasted page's JSON works as well as the code.
    r.fake.reply("/oauth/token", 400, include_str!("../tests/fixtures/error_code_not_found.json"));
    let e = r.epic.login(&format!(r#"{{"authorizationCode":"{CODE}"}}"#)).await.unwrap_err();
    assert!(e.to_string().contains("open the sign-in link again"), "{e}");
    r.fake.reply("/oauth/token", 400, include_str!("../tests/fixtures/error_corrective.json"));
    let e = r.epic.login(CODE).await.unwrap_err();
    assert!(e.to_string().contains("privacy policy") && e.to_string().contains("https://www.epicgames.com/id/login/correction/"), "{e}");
    // Still one account, unchanged.
    assert_eq!(r.epic.accounts().unwrap().len(), 1);
    assert_eq!(r.stored(ID).unwrap().refresh_token.expose(), "eg1~r1");
}

#[tokio::test]
async fn refresh_rotates_and_stores_before_use() {
    let r = rig();
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    // Fresh: no request.
    let n = r.fake.requests().len();
    assert_eq!(r.epic.access_token(ID).await.unwrap().expose(), "eg1~a1");
    assert_eq!(r.fake.requests().len(), n);
    // Five minutes left: refreshed, and the new pair is in the keyring.
    let mut t = r.stored(ID).unwrap();
    t.access_expires_at = Utc::now() + Duration::minutes(5);
    r.secrets.set(&entry_name(ID), &serde_json::to_string(&t).unwrap()).unwrap();
    r.epic.tokens.lock().await.clear();
    r.fake.reply("/oauth/token", 200, &token("eg1~a2", "eg1~r2", ID, "", 8 * 3600));
    assert_eq!(r.epic.access_token(ID).await.unwrap().expose(), "eg1~a2");
    let req = r.fake.requests().pop().unwrap();
    assert!(req.form.unwrap().contains(&("refresh_token".into(), "eg1~r1".into())));
    let stored = r.stored(ID).unwrap();
    assert_eq!((stored.refresh_token.expose().as_str(), stored.display_name.as_str()), ("eg1~r2", "Player One"));
    assert_eq!(r.epic.access_token(ID).await.unwrap().expose(), "eg1~a2");
}

#[tokio::test]
async fn a_refused_refresh_signs_the_account_out_and_is_not_retried() {
    let r = rig();
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    let mut t = r.stored(ID).unwrap();
    t.access_expires_at = Utc::now() - Duration::minutes(1);
    r.secrets.set(&entry_name(ID), &serde_json::to_string(&t).unwrap()).unwrap();
    r.epic.tokens.lock().await.clear();
    r.fake.reply("/oauth/token", 400, include_str!("../tests/fixtures/error_invalid_refresh.json"));
    let e = r.epic.access_token(ID).await.unwrap_err();
    assert!(format!("{e:#}").contains("sign in again"), "{e:#}");
    assert!(!format!("{e:#}").contains("eg1~"), "no token in the error: {e:#}");
    assert_eq!(r.epic.accounts().unwrap()[0].state, AccountState::NeedsSignIn);
    assert!(r.secrets.entries().is_empty());
    let n = r.fake.requests().len();
    assert!(r.epic.access_token(ID).await.is_err());
    assert_eq!(r.fake.requests().len(), n, "no second refresh");
    // Signing in again brings it back.
    r.login("eg1~a3", "eg1~r3", ID, "Player One").await;
    assert_eq!(r.epic.accounts().unwrap()[0].state, AccountState::SignedIn);
}

#[tokio::test]
async fn two_accounts_and_sign_out() {
    let r = rig();
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    let b = r.login("eg1~b1", "eg1~rb", "bbbb0000", "Second").await;
    assert!(!b.default);
    assert_eq!(r.epic.set_default("second").unwrap().account_id, "bbbb0000");
    r.fake.reply("/oauth/sessions/kill/eg1~b1", 204, "");
    let out = r.epic.logout(None).await.unwrap();
    assert_eq!(out.display_name, "Second");
    let kill = r.fake.requests().pop().unwrap();
    assert_eq!((kill.method, kill.label), (Method::Delete, "end session"));
    assert_eq!(r.secrets.entries(), [format!("epic:{ID}")]);
    let left = r.epic.accounts().unwrap();
    assert_eq!((left.len(), left[0].default), (1, true));
    // Ending the session at Epic can fail; signing out still happens.
    r.fake.reply("/oauth/sessions/kill/", 500, "");
    r.epic.logout(Some("player one")).await.unwrap();
    assert!(r.epic.accounts().unwrap().is_empty() && r.secrets.entries().is_empty());
    assert!(r.epic.logout(None).await.unwrap_err().to_string().contains("quiver epic login"));
}

fn script_library(fake: &Fake, account: &str, page1: &str, page2: Option<&str>) {
    let _ = account;
    if let Some(p2) = page2 {
        fake.reply("includeMetadata=true&cursor=page2", 200, p2);
    }
    fake.reply("includeMetadata=true", 200, page1);
}

/// One catalog reply per item, as Epic is asked (the fixture files hold several items;
/// Epic's reply to one id is the same map shape).
fn script_catalog(fake: &Fake) {
    let origins = include_str!("../tests/fixtures/catalog_origins.json");
    let misc = include_str!("../tests/fixtures/catalog_misc.json");
    fake.reply("/namespace/ns-alanwake/bulk/items?id=item-awr&", 200, include_str!("../tests/fixtures/catalog_alanwake.json"));
    for id in ["item-aco", "item-aco-dlc1", "item-aco-ost"] {
        fake.reply(&format!("/namespace/ns-origins/bulk/items?id={id}&"), 200, origins);
    }
    fake.reply("/namespace/ns-celeste/bulk/items?id=item-celeste&", 200, misc);
    fake.reply("/namespace/ns-mobile/bulk/items?id=item-mobile&", 200, misc);
}

#[tokio::test]
async fn library_reads_every_account_and_survives_a_failure() {
    let r = rig();
    // No account: nothing asked, nothing listed.
    assert!(r.epic.library(false).await.unwrap().titles.is_empty());
    assert!(r.fake.requests().is_empty());
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    script_library(&r.fake, ID, include_str!("../tests/fixtures/library_page1.json"), Some(include_str!("../tests/fixtures/library_page2.json")));
    script_catalog(&r.fake);
    let lib = r.epic.library(false).await.unwrap();
    let names: Vec<&str> = lib.titles.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(names, ["Alan Wake Remastered", "Assassin's Creed Origins", "Celeste"]);
    assert!(lib.errors.is_empty(), "{:?}", lib.errors);
    assert!(r.fake.unused().is_empty(), "{:?}", r.fake.unused());
    // Cached: no new requests; the catalog isn't asked again even on refresh of the
    // library within its TTL (refresh re-reads both).
    let n = r.fake.requests().len();
    r.epic.library(false).await.unwrap();
    assert_eq!(r.fake.requests().len(), n);
    // Epic down on refresh: the last good read, with the reason.
    r.fake.reply("includeMetadata=true", 503, "");
    let lib = r.epic.library(true).await.unwrap();
    assert_eq!(lib.titles.len(), 3);
    assert!(lib.errors[0].starts_with("Player One:") && lib.errors[0].contains("showing what Epic said on"), "{:?}", lib.errors);
}

fn provider_entries(lib: &Library) -> Vec<LibraryEntry> {
    lib.titles.iter().filter_map(provider::entry).collect()
}

#[tokio::test]
async fn provider_entries_pages_and_play() {
    let r = rig();
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    script_library(&r.fake, ID, include_str!("../tests/fixtures/library_page1.json"), Some(include_str!("../tests/fixtures/library_page2.json")));
    script_catalog(&r.fake);
    let p = EpicProvider::new(r.epic.clone());
    let info = p.account();
    assert_eq!((info.state, info.name.as_deref()), (AccountState::SignedIn, Some("Player One")));
    let lib = p.library(false).await.unwrap();
    let aw = lib.iter().find(|e| e.name == "Alan Wake Remastered").unwrap();
    assert_eq!(aw.game.to_string(), "epic:Kestrel");
    assert!(aw.installed.is_none() && aw.client.is_none() && !aw.installable && !aw.on_disk());
    assert_eq!(aw.stores(), [quiver_api::StoreId::Epic]);
    assert_eq!(aw.art.epic.tile.as_deref(), Some("https://cdn1.epicgames.com/fixture/awr/tall.jpg"));
    assert_eq!(aw.owned_by, [ID]);
    let tile = quiver_api::art::candidates(aw, quiver_api::art::ArtSlot::Tile, Default::default());
    assert_eq!(tile[0].0, quiver_api::art::ArtOrigin::Epic);
    let ac = lib.iter().find(|e| e.game.id.as_str() == "Camellia").unwrap();
    assert_eq!(ac.records[0].game, GameRef::ubisoft(3539));
    assert_eq!(ac.records[0].relation, Relation::Linked);

    let d = p.detail(aw.game, false).await.unwrap();
    assert_eq!((d.developer.as_deref(), d.account.as_deref(), d.linked), (Some("Remedy Entertainment"), Some("Player One"), None));
    assert!(d.description.unwrap().contains("Bright Falls"));
    let d = p.detail(ac.game, false).await.unwrap();
    assert_eq!(d.linked, Some(GameRef::ubisoft(3539)));
    let dlc: Vec<(String, Option<bool>)> = d.dlc.iter().map(|x| (x.game.to_string(), x.owned)).collect();
    assert_eq!(dlc, [("epic:CamelliaDLC1".to_owned(), Some(true)), ("epic:item-aco-dlc2".to_owned(), Some(false))]);

    let e = p.launch("cli", aw.game, None).await.unwrap_err();
    assert!(e.to_string().contains("isn't installed"), "{e}");
    let e = p.launch("cli", ac.game, None).await.unwrap_err();
    assert!(e.to_string().contains("play it as ubisoft:3539"), "{e}");
    assert!(p.detail(GameRef::new(quiver_api::StoreId::Epic, "Nope").unwrap(), false).await.unwrap_err().to_string().contains("not in your Epic library"));
}

#[tokio::test]
async fn ubisoft_titles_fold_into_their_ubisoft_entries() {
    let r = rig();
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    script_library(&r.fake, ID, include_str!("../tests/fixtures/library_page1.json"), Some(include_str!("../tests/fixtures/library_page2.json")));
    script_catalog(&r.fake);
    let epic = provider_entries(&r.epic.library(false).await.unwrap());
    let ubi = |id: u32, name: &str| -> LibraryEntry {
        serde_json::from_value(serde_json::json!({"product_id": id, "name": name, "installed": null, "update_available": false, "running": false, "owned_by": ["ubi-user"]})).unwrap()
    };
    // Signed in to Ubisoft with Origins: one entry, Ubisoft's, with the Epic record on it.
    let lib = quiver_core::stores::merge_libraries(vec![vec![ubi(3539, "Assassin's Creed® Origins"), ubi(420, "Far Cry 4")], epic.clone()]);
    let origins: Vec<&LibraryEntry> = lib.iter().filter(|e| e.name.contains("Origins")).collect();
    assert_eq!(origins.len(), 1);
    let o = origins[0];
    assert_eq!(o.game, GameRef::ubisoft(3539));
    assert_eq!(o.stores(), [quiver_api::StoreId::Ubisoft, quiver_api::StoreId::Epic]);
    assert_eq!(o.records[0].relation, Relation::CopyOf);
    assert_eq!(o.records[0].owned_by, [ID]);
    let camellia = GameRef::new(quiver_api::StoreId::Epic, "Camellia").unwrap();
    assert!(o.is_or_has(&camellia));
    // Epic's art backs up Ubisoft's.
    assert!(o.art.epic.tile.is_some());
    // Not on the Ubisoft account: it stays an Epic entry, linked.
    let lib = quiver_core::stores::merge_libraries(vec![vec![ubi(420, "Far Cry 4")], epic]);
    let o = lib.iter().find(|e| e.name.contains("Origins")).unwrap();
    assert_eq!((o.game, o.records[0].relation), (camellia, Relation::Linked));
    assert_eq!(o.stores(), [quiver_api::StoreId::Epic]);
}

/// Each catalog item is asked for on its own; a failed call is tried once more; an item
/// Epic still doesn't describe is hidden (with the reason, never as a hex-id tile) and
/// asked for again at the next read.
#[tokio::test]
async fn catalog_items_are_retried_and_unresolved_ones_hidden() {
    let r = rig();
    r.login("eg1~a1", "eg1~r1", ID, "Player One").await;
    script_library(&r.fake, ID, include_str!("../tests/fixtures/library_second_account.json"), None);
    let mystery = r#"{"item-mystery": {"id": "item-mystery", "namespace": "ns-mystery", "title": "Mystery Game",
        "keyImages": [{"type": "DieselGameBoxTall", "url": "https://cdn1.epicgames.com/fixture/m/tall.jpg"}],
        "categories": [{"path": "games"}, {"path": "applications"}], "entitlementType": "EXECUTABLE"}}"#;
    r.fake.reply("/namespace/ns-origins/bulk/items?id=item-aco&", 200, include_str!("../tests/fixtures/catalog_origins.json"));
    r.fake.reply("/namespace/ns-mystery/bulk/items?id=item-mystery&", 200, "{}");
    let lib = r.epic.library(false).await.unwrap();
    assert_eq!(lib.titles.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Assassin's Creed Origins"]);
    let h = lib.hidden("MysteryApp").unwrap();
    assert_eq!(h.reason, library::UNRESOLVED);
    let items = r.epic.hidden_items().await.unwrap();
    assert!(items.iter().any(|i| i.app_name == "PrivateThing") && items.iter().any(|i| i.app_name == "MysteryApp"));
    let p = EpicProvider::new(r.epic.clone());
    let e = p.detail(GameRef::new(quiver_api::StoreId::Epic, "MysteryApp").unwrap(), false).await.unwrap_err();
    assert!(e.to_string().contains("isn't in the library: Epic's catalog didn't describe it"), "{e}");
    // A refresh: every item is asked again; the mystery item fails once, then answers.
    script_library(&r.fake, ID, include_str!("../tests/fixtures/library_second_account.json"), None);
    r.fake.reply("/namespace/ns-origins/bulk/items?id=item-aco&", 200, include_str!("../tests/fixtures/catalog_origins.json"));
    r.fake.reply("/namespace/ns-mystery/bulk/items?id=item-mystery&", 503, "");
    r.fake.reply("/namespace/ns-mystery/bulk/items?id=item-mystery&", 200, mystery);
    let before = r.fake.requests().len();
    let lib = r.epic.library(true).await.unwrap();
    assert!(r.fake.unused().is_empty(), "{:?}", r.fake.unused());
    let catalog_calls = r.fake.requests()[before..].iter().filter(|q| q.url.contains("/bulk/items")).count();
    // Origins once, the mystery item twice; never the private sandbox's.
    assert_eq!(catalog_calls, 3);
    assert_eq!(lib.titles.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Assassin's Creed Origins", "Mystery Game"]);
    assert!(lib.errors.is_empty(), "{:?}", lib.errors);
}
