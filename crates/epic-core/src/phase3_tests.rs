//! Phase 3 on recorded and synthesized JSON: cloud saves against an in-memory savesync
//! (the listing, link and pre-signed-URL behaviour Epic's has), play time and achievements
//! through [`Fake`]. No request leaves the process.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use quiver_api::epicgame::{FileState, Resolve, SaveMode, SessionReport, SyncState};

use super::*;
use crate::api::{Attribute, CatalogItem};
use crate::http::{Fake, Method, Request, Response, Transport};
use crate::library::CachedItem;
use crate::playtime::Played;
use crate::saves::Trigger;
use crate::secrets::Memory;

const ID: &str = "8645b4947bbc4c0092a8b7236df169d1";
const AWR: &str = "7315f614b0504081bcdceef138075c9c";

/// Epic's savesync, in memory: listing, links for posted names, pre-signed GET and PUT.
/// Everything else goes to the scripted [`Fake`].
#[derive(Default)]
struct Cloud {
    fake: Fake,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    puts: Mutex<Vec<String>>,
    /// Answer PUTs to pre-signed links with this status (0: 200).
    put_status: Mutex<u16>,
}

fn key(app: &str, rel: &str) -> String {
    format!("savesync/{ID}/{app}/{rel}")
}

impl Cloud {
    fn listing(&self, app: &str) -> String {
        let files = self.files.lock().unwrap();
        let mut out = serde_json::Map::new();
        for (k, v) in files.iter().filter(|(k, _)| k.starts_with(&key(app, ""))) {
            out.insert(
                k.clone(),
                serde_json::json!({"readLink": format!("https://cloud.test/r/{k}"), "writeLink": format!("https://cloud.test/w/{k}"), "lastModified": "2026-11-18T09:00:00.000Z", "length": v.len()}),
            );
        }
        serde_json::json!({ "files": out }).to_string()
    }
}

impl Transport for Cloud {
    fn send(&self, req: Request) -> quiver_core::stores::BoxFuture<'_, anyhow::Result<Response>> {
        Box::pin(async move {
            let ok = |body: Vec<u8>| Ok(Response { status: 200, body });
            if let Some(rest) = req.url.strip_prefix(&format!("{}/api/v1/access/egstore/savesync/{ID}/", saves::DATASTORAGE_HOST)) {
                let app = rest.trim_end_matches('/').to_owned();
                assert!(req.headers.iter().any(|(k, v)| k == "Authorization" && v.starts_with("bearer ")));
                return match req.method {
                    Method::Get => ok(self.listing(&app).into_bytes()),
                    Method::Post => {
                        let body: serde_json::Value = serde_json::from_slice(&req.body.as_ref().unwrap().1).unwrap();
                        let mut out = serde_json::Map::new();
                        for n in body["files"].as_array().unwrap() {
                            let k = key(&app, n.as_str().unwrap());
                            out.insert(n.as_str().unwrap().to_owned(), serde_json::json!({"readLink": format!("https://cloud.test/r/{k}"), "writeLink": format!("https://cloud.test/w/{k}")}));
                        }
                        ok(serde_json::json!({ "files": out }).to_string().into_bytes())
                    }
                    _ => panic!("{req:?}"),
                };
            }
            if let Some(k) = req.url.strip_prefix("https://cloud.test/r/") {
                assert!(!req.headers.iter().any(|(k, _)| k == "Authorization"), "no token goes to a pre-signed link");
                return match self.files.lock().unwrap().get(k) {
                    Some(d) => ok(d.clone()),
                    None => Ok(Response { status: 404, body: b"<Error>NoSuchKey</Error>".to_vec() }),
                };
            }
            if let Some(k) = req.url.strip_prefix("https://cloud.test/w/") {
                assert_eq!(req.method, Method::Put);
                let status = *self.put_status.lock().unwrap();
                if status != 0 {
                    return Ok(Response { status, body: Vec::new() });
                }
                self.puts.lock().unwrap().push(k.to_owned());
                self.files.lock().unwrap().insert(k.to_owned(), req.body.unwrap().1);
                return ok(Vec::new());
            }
            self.fake.send(req).await
        })
    }
}

struct Rig {
    tmp: tempfile::TempDir,
    cloud: Arc<Cloud>,
    epic: Arc<Epic>,
}

fn item(ns: &str, id: &str, title: &str, attrs: &[(&str, &str)]) -> CatalogItem {
    CatalogItem {
        id: id.into(),
        namespace: ns.into(),
        title: title.into(),
        custom_attributes: attrs.iter().map(|(k, v)| ((*k).to_owned(), Attribute { kind: "STRING".into(), value: (*v).to_owned() })).collect(),
        // A game with art, so the library's tile rule takes it (library::classify).
        categories: vec![crate::api::Category { path: "games".into() }],
        key_images: vec![crate::api::KeyImage { kind: "DieselGameBoxTall".into(), url: format!("https://cdn1.epicgames.com/{id}/tall.jpg"), ..Default::default() }],
        ..Default::default()
    }
}

async fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let cloud = Arc::new(Cloud::default());
    let epic = Arc::new(Epic::new(tmp.path(), cloud.clone(), Arc::new(Memory::new())));
    epic.set_save_backups(tmp.path().join("backups"));
    let token = serde_json::json!({"access_token": "eg1~a", "expires_in": 28800, "refresh_token": "eg1~r", "refresh_expires": 2592000, "account_id": ID, "displayName": "Player One"}).to_string();
    cloud.fake.reply("/oauth/token", 200, &token);
    cloud.fake.reply("/externalAuths", 200, "[]");
    cloud.fake.reply(&format!("/public/account/{ID}"), 200, &format!(r#"{{"id":"{ID}","displayName":"Player One"}}"#));
    epic.login("0a1b2c3d4e5f60718293a4b5c6d7e8f9").await.unwrap();
    // The catalog, already cached; the library, from Epic.
    let now = Utc::now();
    let catalog: HashMap<String, CachedItem> = [
        item("ns-wren", "item-wren", "Wren", &[("CloudSaveFolder", "{AppData}/../LocalLow/Fixture/Wren/{EpicID}/"), ("CloudExcludeList", "*.log"), ("MaxSizeMB", "50")]),
        item("ns-aw", "item-aw", "Alan Wake Remastered", &[("FolderName", "AlanWakeRemastered")]),
    ]
    .into_iter()
    .map(|i| (library::catalog_key(&i.namespace, &i.id), CachedItem { fetched_at: now, item: i }))
    .collect();
    library::Cache::new(&tmp.path().join("epic")).save_catalog(&catalog).unwrap();
    cloud.fake.reply(
        "includeMetadata=true",
        200,
        &serde_json::json!({"records": [
            {"namespace": "ns-wren", "catalogItemId": "item-wren", "appName": "Wren", "sandboxType": "PUBLIC"},
            {"namespace": "ns-aw", "catalogItemId": "item-aw", "appName": AWR, "sandboxType": "PUBLIC"},
        ]})
        .to_string(),
    );
    epic.library(true).await.unwrap();
    Rig { tmp, cloud, epic }
}

impl Rig {
    fn prefix(&self) -> PathBuf {
        self.tmp.path().join("pfx")
    }
    fn save_dir(&self) -> PathBuf {
        self.prefix().join(format!("drive_c/users/steamuser/AppData/LocalLow/Fixture/Wren/{ID}"))
    }
    fn write(&self, rel: &str, data: &str) {
        let p = self.save_dir().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.save_dir().join(rel)).unwrap()
    }
    fn manifests(&self) -> Vec<String> {
        self.cloud.files.lock().unwrap().keys().filter(|k| k.contains("/manifests/")).cloned().collect()
    }
    /// Another machine uploads a version (a manifest and its chunks).
    fn upload_elsewhere(&self, version: &str, files: &[(&str, &str)]) {
        let files: Vec<(String, Vec<u8>)> = files.iter().map(|(n, d)| ((*n).to_owned(), d.as_bytes().to_vec())).collect();
        let p = savefmt::package(&format!("Wren{ID}"), version, BTreeMap::new(), &files, savefmt::Guid::random).unwrap();
        let mut cloud = self.cloud.files.lock().unwrap();
        for (path, data) in p.chunks {
            cloud.insert(key("Wren", &path), data);
        }
        cloud.insert(key("Wren", &format!("manifests/{version}.manifest")), p.manifest_file);
    }
}

fn set_mtime(path: &Path, t: chrono::DateTime<Utc>) {
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t.into()).unwrap();
}

#[tokio::test]
async fn first_upload_waits_for_a_yes_then_uploads_and_reads_back() {
    let r = rig().await;
    let p = r.epic.before_launch("Wren", &r.prefix(), None).await;
    assert_eq!(p.note, "saves in sync");
    let loc = p.location.clone().unwrap();
    assert_eq!(loc.dir, r.save_dir());
    // The game writes saves (and a log the catalog excludes).
    r.write("slot1.sav", "chapter one");
    r.write("profile/settings.ini", "fov=90");
    r.write("debug.log", "noise");
    let f = r.epic.after_exit(&p, None, true).await;
    let saves = f.saves.unwrap().unwrap();
    assert!(saves.uploaded.is_empty() && saves.held.as_deref().unwrap().contains("first yes"), "{saves:?}");
    assert!(r.cloud.puts.lock().unwrap().is_empty(), "nothing goes up before the person says yes");
    let s = r.epic.saves_report("Wren", true).await.unwrap();
    assert_eq!((s.mode, s.state), (SaveMode::Launcher, SyncState::LocalNewer));
    assert_eq!(s.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["profile/settings.ini", "slot1.sav"]);
    // The person says yes.
    let rep = session::sync_now(&r.epic, "Wren", Trigger::Manual { resolve: None, first_upload_ok: true }).await.unwrap();
    assert_eq!(rep.uploaded, ["profile/settings.ini", "slot1.sav"]);
    let w = r.epic.writes().unwrap();
    assert!(w.saves.allowed_at.is_some() && w.saves.verified_at.is_some());
    // Chunks went up before the manifest, and the version names the app plus the account.
    let puts = r.cloud.puts.lock().unwrap().clone();
    assert!(puts.last().unwrap().contains("/manifests/") && puts[0].contains("/ChunksV4/"), "{puts:?}");
    let m = savefmt::read_manifest(&r.cloud.files.lock().unwrap()[&puts.last().unwrap().clone()]).unwrap();
    assert_eq!(m.app_name, format!("Wren{ID}"));
    assert_eq!(m.custom["CloudSaveFolder"], "{AppData}/../LocalLow/Fixture/Wren/{EpicID}/");
    let s = r.epic.saves_report("Wren", true).await.unwrap();
    assert_eq!(s.state, SyncState::InSync);
    assert_eq!(s.cloud_versions, 1);
    // From now on uploads happen on their own after a session.
    r.write("slot1.sav", "chapter two");
    let p = r.epic.before_launch("Wren", &r.prefix(), None).await;
    let f = r.epic.after_exit(&p, None, true).await;
    assert_eq!(f.saves.unwrap().unwrap().uploaded, ["slot1.sav"]);
    assert_eq!(r.manifests().len(), 2, "every upload is a new version; none is deleted");
}

#[tokio::test]
async fn cloud_changes_come_down_with_a_backup_and_conflicts_wait() {
    let r = rig().await;
    r.epic.allow_writes(progress::WriteKind::Saves).unwrap();
    r.write("slot1.sav", "mine");
    let p = r.epic.before_launch("Wren", &r.prefix(), None).await;
    r.epic.after_exit(&p, None, true).await.saves.unwrap().unwrap();
    // Another machine plays on and uploads.
    r.upload_elsewhere("2030.01.01-00.00.00", &[("slot1.sav", "theirs"), ("slot2.sav", "new slot")]);
    let s = r.epic.saves_report("Wren", true).await.unwrap();
    assert_eq!(s.state, SyncState::CloudNewer);
    let p = r.epic.before_launch("Wren", &r.prefix(), None).await;
    assert_eq!(p.saves.downloaded, ["slot1.sav", "slot2.sav"]);
    assert_eq!((r.read("slot1.sav").as_str(), r.read("slot2.sav").as_str()), ("theirs", "new slot"));
    let b = p.saves.backup.clone().unwrap();
    assert_eq!(std::fs::read_to_string(b.path.join("slot1.sav")).unwrap(), "mine");
    assert_eq!(saves::backups(&r.tmp.path().join("backups"), "Wren")[0].reason.as_deref(), Some("before cloud download"));
    // Downloads before a launch never upload.
    assert_eq!(r.manifests().len(), 2);
    // Both sides change the same file: nothing moves.
    r.write("slot1.sav", "mine again");
    r.upload_elsewhere("2030.01.02-00.00.00", &[("slot1.sav", "theirs again"), ("slot2.sav", "new slot")]);
    let f = r.epic.after_exit(&p, None, true).await.saves.unwrap().unwrap();
    assert_eq!(f.conflicts, ["slot1.sav"]);
    assert!(f.uploaded.is_empty() && f.downloaded.is_empty());
    let s = r.epic.saves_report("Wren", true).await.unwrap();
    assert_eq!(s.state, SyncState::Conflict);
    assert_eq!(s.files.iter().find(|f| f.path == "slot1.sav").unwrap().state, FileState::Conflict);
    // Keep the cloud's: this machine's copy is backed up, then replaced.
    let rep = session::sync_now(&r.epic, "Wren", Trigger::Manual { resolve: Some(Resolve::KeepCloud), first_upload_ok: false }).await.unwrap();
    assert_eq!(rep.downloaded, ["slot1.sav"]);
    assert_eq!(r.read("slot1.sav"), "theirs again");
    assert_eq!(std::fs::read_to_string(rep.backup.unwrap().path.join("slot1.sav")).unwrap(), "mine again");
    assert_eq!(r.epic.saves_report("Wren", true).await.unwrap().state, SyncState::InSync);
    // And a restore puts the backup back; it uploads with the next sync.
    let (b, prev) = r.epic.restore_saves("Wren", "latest").await.unwrap();
    assert!(prev.is_some() && b.reason.as_deref() == Some("before cloud download"));
    assert_eq!(r.read("slot1.sav"), "mine again");
    assert_eq!(r.epic.saves_report("Wren", true).await.unwrap().state, SyncState::LocalNewer);
    let rep = session::sync_now(&r.epic, "Wren", Trigger::Manual { resolve: None, first_upload_ok: false }).await.unwrap();
    assert_eq!(rep.uploaded, ["slot1.sav"]);
}

#[tokio::test]
async fn first_sync_on_a_machine_decides_by_time_and_deletions_stay_local() {
    let r = rig().await;
    r.epic.allow_writes(progress::WriteKind::Saves).unwrap();
    r.upload_elsewhere("2026.11.01-10.00.00", &[("slot1.sav", "cloud one"), ("slot2.sav", "cloud two")]);
    r.write("slot1.sav", "local, much newer");
    r.write("slot2.sav", "local, as old");
    set_mtime(&r.save_dir().join("slot1.sav"), saves::version_time("2026.11.15-10.00.00").unwrap());
    set_mtime(&r.save_dir().join("slot2.sav"), saves::version_time("2026.11.01-10.00.30").unwrap());
    r.epic.remember_place("Wren", &r.prefix(), None).unwrap();
    let s = r.epic.saves_report("Wren", true).await.unwrap();
    let st: HashMap<&str, FileState> = s.files.iter().map(|f| (f.path.as_str(), f.state)).collect();
    assert_eq!(st["slot1.sav"], FileState::LocalNewer);
    assert_eq!(st["slot2.sav"], FileState::Conflict, "within 60 s of the version: can't tell");
    // Keep this machine's: uploaded as a new version.
    let rep = session::sync_now(&r.epic, "Wren", Trigger::Manual { resolve: Some(Resolve::KeepLocal), first_upload_ok: false }).await.unwrap();
    assert_eq!(rep.uploaded, ["slot1.sav", "slot2.sav"]);
    // A file deleted here isn't downloaded again; the next upload leaves it out.
    std::fs::remove_file(r.save_dir().join("slot2.sav")).unwrap();
    let s = r.epic.saves_report("Wren", true).await.unwrap();
    assert_eq!(s.files.iter().find(|f| f.path == "slot2.sav").unwrap().state, FileState::DeletedHere);
    let p = r.epic.before_launch("Wren", &r.prefix(), None).await;
    assert!(p.saves.downloaded.is_empty());
    assert!(!r.save_dir().join("slot2.sav").exists());
}

#[tokio::test]
async fn a_failed_upload_keeps_the_base_and_games_without_a_folder_sync_in_game() {
    let r = rig().await;
    r.epic.allow_writes(progress::WriteKind::Saves).unwrap();
    let p = r.epic.before_launch("Wren", &r.prefix(), None).await;
    r.write("slot1.sav", "x");
    *r.cloud.put_status.lock().unwrap() = 403;
    let f = r.epic.after_exit(&p, None, true).await;
    assert!(f.saves.unwrap().unwrap_err().contains("savesync chunk upload"), "a refused upload is an error");
    *r.cloud.put_status.lock().unwrap() = 0;
    assert_eq!(r.epic.saves_report("Wren", true).await.unwrap().state, SyncState::LocalNewer, "still waiting to go up");
    // Alan Wake Remastered names no save folder: saves sync in game.
    let p = r.epic.before_launch(AWR, &r.tmp.path().join("pfx-aw"), None).await;
    assert!(p.saves.in_game && p.location.is_none());
    assert_eq!(p.note, "saves sync in game");
    let s = r.epic.saves_report(AWR, true).await.unwrap();
    assert_eq!(s.mode, SaveMode::InGame);
    assert!(s.next.unwrap().starts_with("Saves sync in game"));
}

#[tokio::test]
async fn savesync_listing_parses_and_picks_the_newest_version() {
    let fake = Arc::new(Fake::new());
    fake.reply("/savesync/", 200, include_str!("../tests/fixtures/savesync_list.json"));
    let api = Api::new(fake.clone(), config::ClientConfig::default());
    let files = api.savesync_list("tok", ID, "Wren").await.unwrap();
    assert_eq!(files.len(), 3);
    let req = &fake.requests()[0];
    assert_eq!(req.url, format!("https://datastorage-public-service-liveegs.live.use1a.on.epicgames.com/api/v1/access/egstore/savesync/{ID}/Wren/"));
    let newest = files.keys().filter(|k| k.contains("/manifests/")).max().unwrap();
    assert!(newest.ends_with("2026.11.17-21.00.00.manifest"));
    assert_eq!(files[newest].length, Some(498));
    assert_eq!(saves::version_time("2026.11.17-21.00.00").unwrap().to_rfc3339(), "2026-11-17T21:00:00+00:00");
    // Links for names relative to the app.
    fake.reply("/savesync/", 200, r#"{"files":{"manifests/x.manifest":{"writeLink":"https://w"}}}"#);
    let l = api.savesync_links("tok", ID, "Wren", &["manifests/x.manifest".into()]).await.unwrap();
    assert_eq!(l["manifests/x.manifest"].write_link.as_deref(), Some("https://w"));
    let req = &fake.requests()[1];
    assert_eq!(req.method, Method::Post);
    assert_eq!(req.body.as_ref().unwrap().1, br#"{"files":["manifests/x.manifest"]}"#);
}

// ---- play time

fn played(id: &str) -> Played {
    let end = Utc::now();
    Played { id: id.into(), app: AWR.into(), account_id: ID.into(), started: end - chrono::Duration::minutes(42), ended: end }
}

#[tokio::test]
async fn sessions_are_held_until_the_first_yes_then_reported_once() {
    let r = rig().await;
    let f = &r.cloud.fake;
    // Held: reporting is on, but the person hasn't said yes yet.
    let q = r.epic.queue_session(&played("s-1"), true).await.unwrap();
    assert_eq!((q.state, q.seconds), (SessionReport::Held, 42 * 60));
    assert!(r.epic.send_queue(true, false).await.unwrap().is_empty());
    // The same session queued again is the same entry.
    r.epic.queue_session(&played("s-1"), true).await.unwrap();
    assert_eq!(playtime::Queue::load(r.epic.epic_dir()).unwrap().sessions.len(), 1);
    // The person says yes: sent, and the gate stays open.
    f.reply("/library/api/public/playtime/account/", 204, "");
    let sent = r.epic.send_queue(true, true).await.unwrap();
    assert_eq!(sent[0].state, SessionReport::Reported);
    let put = f.requests().into_iter().rfind(|q| q.label == "playtime report").unwrap();
    assert_eq!((put.method, put.url.as_str()), (Method::Put, format!("https://library-service.live.use1a.on.epicgames.com/library/api/public/playtime/account/{ID}").as_str()));
    let body: serde_json::Value = serde_json::from_slice(&put.body.unwrap().1).unwrap();
    assert_eq!(body["artifactId"], AWR);
    assert_eq!((body["startSegment"].as_bool(), body["endSegment"].as_bool()), (Some(true), Some(true)));
    assert_eq!(body["machineId"].as_str().unwrap(), r.epic.machine_id().unwrap());
    assert!(body["startTime"].as_str().unwrap().ends_with('Z'));
    // Reported sessions are never sent again.
    assert!(r.epic.send_queue(true, false).await.unwrap().is_empty());
    assert!(r.epic.writes().unwrap().playtime.verified_at.is_some());
    // Off: kept here only.
    assert_eq!(r.epic.queue_session(&played("s-2"), false).await.unwrap().state, SessionReport::Off);
    assert!(r.epic.send_queue(false, false).await.unwrap().is_empty());
}

#[tokio::test]
async fn discarded_sessions_are_never_sent_even_after_the_first_yes() {
    let r = rig().await;
    let f = &r.cloud.fake;
    r.epic.queue_session(&played("s-test"), true).await.unwrap();
    let dropped = r.epic.discard_sessions(Some(&crate::provider::game_ref(AWR).unwrap())).await.unwrap();
    assert_eq!((dropped.len(), dropped[0].state), (1, SessionReport::Discarded));
    // Nothing left to drop; the yes opens the gate but sends nothing.
    assert!(r.epic.discard_sessions(None).await.unwrap().is_empty());
    f.reply("/library/api/public/playtime/account/", 204, "");
    assert!(r.epic.send_queue(true, true).await.unwrap().is_empty());
    assert!(!f.requests().iter().any(|q| q.label == "playtime report"));
    // A later session is reported as usual.
    assert_eq!(r.epic.send_queue(true, false).await.unwrap().len(), 0);
    r.epic.queue_session(&played("s-real"), true).await.unwrap();
    assert_eq!(r.epic.send_queue(true, false).await.unwrap()[0].state, SessionReport::Reported);
}

#[tokio::test]
async fn offline_retries_and_lost_answers_are_never_resent() {
    let r = rig().await;
    let f = &r.cloud.fake;
    r.epic.allow_writes(progress::WriteKind::Playtime).unwrap();
    // No connection (nothing scripted): back in the queue, the attempt not counted.
    r.epic.queue_session(&played("s-offline"), true).await.unwrap();
    let s = r.epic.send_queue(true, false).await.unwrap();
    assert_eq!((s[0].state, s[0].attempts), (SessionReport::Queued, 0));
    // Epic busy: retried.
    f.reply("/playtime/account/", 503, r#"{"errorCode":"errors.com.epicgames.common.server_error","errorMessage":"busy"}"#);
    assert_eq!(r.epic.send_queue(true, false).await.unwrap()[0].state, SessionReport::Queued);
    // Refused outright: failed, not retried.
    f.reply("/playtime/account/", 400, r#"{"errorCode":"errors.com.epicgames.validation.validation_failed","errorMessage":"bad segment"}"#);
    let s = r.epic.send_queue(true, false).await.unwrap();
    assert_eq!(s[0].state, SessionReport::Failed);
    assert!(s[0].last_error.as_deref().unwrap().contains("HTTP 400"));
    assert!(r.epic.send_queue(true, false).await.unwrap().is_empty());
    // A session that was mid-send when Quiver stopped is never sent on its own.
    let mut q = playtime::Queue::load(r.epic.epic_dir()).unwrap();
    q.sessions.push(quiver_api::epicgame::QueuedSession { id: "s-lost".into(), state: SessionReport::Unconfirmed, ..q.sessions[0].clone() });
    progress::write_json(&r.tmp.path().join("epic/playtime-queue.json"), &q).unwrap();
    assert!(r.epic.send_queue(true, false).await.unwrap().is_empty());
}

#[tokio::test]
async fn totals_are_epics_and_cached() {
    let r = rig().await;
    r.cloud.fake.reply("/playtime/account/8645b4947bbc4c0092a8b7236df169d1/all", 200, include_str!("../tests/fixtures/playtime_all.json"));
    assert_eq!(r.epic.playtime_total(AWR, false).await.unwrap(), Some(7380));
    // Cached: no second request.
    assert_eq!(r.epic.playtime_total("Wren", false).await.unwrap(), Some(125));
    let v = r.epic.playtime_view(None, false, true).await.unwrap();
    assert_eq!(v.games.first().map(|g| g.0.as_str()), Some("Fortnite"));
    assert!(r.cloud.fake.unused().is_empty());
}

// ---- achievements

#[tokio::test]
async fn achievements_merge_definitions_with_unlocks() {
    let r = rig().await;
    let f = &r.cloud.fake;
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_defs.json"));
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_player.json"));
    let a = r.epic.achievements("Wren", false).await.unwrap();
    let list = a.achievements.unwrap();
    assert_eq!((list.unlocked, list.total), (1, 3));
    assert_eq!(a.xp, Some((50, 250)));
    let first = &list.list[0];
    assert!(first.unlocked && first.unlocked_at.unwrap().to_rfc3339().starts_with("2026-11-17T21:04:11"));
    let secret = list.list.iter().find(|x| x.id == "SECRET_END").unwrap();
    assert!(secret.hidden && !secret.unlocked && secret.name == "Hidden achievement");
    let gql: Vec<Request> = f.requests().into_iter().filter(|q| q.label.contains("achievements")).collect();
    assert_eq!(gql[0].url, "https://launcher.store.epicgames.com/graphql");
    assert!(gql[0].headers.iter().any(|(k, v)| k == "User-Agent" && v.starts_with("EpicGamesLauncher/")));
    let body: serde_json::Value = serde_json::from_slice(&gql[1].body.as_ref().unwrap().1).unwrap();
    assert_eq!((body["variables"]["sandboxId"].as_str(), body["variables"]["epicAccountId"].as_str()), (Some("ns-wren"), Some(ID)));
    assert!(body["query"].as_str().unwrap().contains("playerAchievementGameRecordsBySandbox"));
    // A game with none, and an error reply.
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_none.json"));
    let none = r.epic.achievements(AWR, false).await.unwrap();
    assert_eq!(none.achievements.unwrap().total, 0);
    f.reply("/graphql", 200, include_str!("../tests/fixtures/graphql_error.json"));
    let e = r.epic.api.achievement_definitions("t", "ns-x", "en-US").await.unwrap_err();
    assert!(e.to_string().contains("Unauthorized"), "{e}");
}

#[tokio::test]
async fn new_unlocks_toast_once_and_the_first_look_only_records() {
    let r = rig().await;
    let f = &r.cloud.fake;
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_defs.json"));
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_player.json"));
    assert!(r.epic.new_unlocks("Wren", ID).await.unwrap().is_empty(), "what was there before isn't new");
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_player_later.json"));
    let u = r.epic.new_unlocks("Wren", ID).await.unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!((u[0].id.as_str(), u[0].name.as_str()), ("DEEP_DIVE", "Deep Dive"));
    assert_eq!(u[0].icon_url.as_deref(), Some("https://cdn1.epicgames.com/fixture/deep.png"));
    // Seen: not again.
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_player_later.json"));
    assert!(r.epic.new_unlocks("Wren", ID).await.unwrap().is_empty());
    // The icon, once, for the toast.
    f.reply_bytes("fixture/deep.png", 200, vec![0x89, b'P', b'N', b'G']);
    let icon = r.epic.icon_file("https://cdn1.epicgames.com/fixture/deep.png").await.unwrap();
    assert!(icon.is_file());
    assert_eq!(r.epic.icon_file("https://cdn1.epicgames.com/fixture/deep.png").await.unwrap(), icon);
}

#[tokio::test]
async fn the_game_page_carries_epics_progress() {
    let r = rig().await;
    let f = &r.cloud.fake;
    f.reply("/playtime/account/", 200, include_str!("../tests/fixtures/playtime_all.json"));
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_defs.json"));
    f.reply("/graphql", 200, include_str!("../tests/fixtures/achievements_player.json"));
    r.epic.remember_place("Wren", &r.prefix(), None).unwrap();
    let mut d: quiver_api::stores::GameDetail = serde_json::from_value(serde_json::json!({"game": "epic:Wren", "name": "Wren", "unavailable": ["play time", "achievements", "cloud saves"]})).unwrap();
    r.upload_elsewhere("2026.11.17-21.00.00", &[("slot1.sav", "cloud")]);
    r.epic.fill_detail(&mut d, true).await;
    assert_eq!(d.playtime.unwrap().total_seconds, 125);
    assert_eq!(d.achievements.unwrap().unlocked, 1);
    let c = d.cloud.unwrap();
    assert_eq!((c.files, c.unsynced), (1, 1));
    assert!(d.unavailable.is_empty(), "{:?}", d.unavailable);
}
