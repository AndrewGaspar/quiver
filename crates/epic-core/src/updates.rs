//! New builds of installed Epic games (design/STORE-EPIC.md §9.12).
//!
//! Epic doesn't tell a launcher a build changed; EGL asks. Quiver asks the build API
//! (`assets/v2 … label/Live`, the call an install already makes) once per installed game
//! and keeps only the reply's `buildVersion` and manifest SHA-1: the keys and signed URLs
//! in the same reply are dropped at once, never stored or logged.
//!
//! - **Cached** in `<data>/epic/builds.json` (versions and hashes only), so the library's
//!   update badge survives a quiverd restart without asking Epic again, and a library
//!   read never waits on Epic for it.
//! - **How often:** quiverd's upkeep checks every [`CHECK_INTERVAL`]; a library refresh
//!   (the app's refresh, `quiver library --refresh`) checks when the last check is older
//!   than [`REFRESH_INTERVAL`], so pressing refresh repeatedly costs nothing. One check
//!   runs at a time.
//! - **An update is available** when the `Live` build's manifest SHA-1 differs from the
//!   one the game was installed from (a new build, or one rebuilt under the same version).
//!   The SHA-1 decides rather than the version string because the record's version comes
//!   from the manifest and the check's from the build API: if those were ever spelled
//!   differently, comparing versions would show an update forever and auto-update would
//!   fetch the same build every hour. Without a hash on either side (an older record),
//!   the versions are compared. Installing writes the new hash and version to the record,
//!   so the badge goes away by itself after an update.
//! - DLC with builds of its own updates with its game (`Epic::install`), so only games are
//!   checked.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use quiver_api::stores::AccountState;
use serde::{Deserialize, Serialize};

use crate::Epic;
use crate::installs::Installed;

/// How often quiverd's upkeep asks Epic for new builds.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// A library refresh asks again only when the last check is older than this.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Epic's current build of an app, as last asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestBuild {
    pub version: String,
    /// SHA-1 (hex) of the build's manifest file.
    #[serde(default)]
    pub manifest_hash: String,
    pub checked_at: DateTime<Utc>,
}

impl LatestBuild {
    /// Whether `installed` is behind this build.
    pub fn newer_than(&self, installed: &Installed) -> bool {
        if !self.manifest_hash.is_empty() && !installed.manifest_hash.is_empty() {
            return !self.manifest_hash.eq_ignore_ascii_case(&installed.manifest_hash);
        }
        !self.version.is_empty() && self.version != installed.version
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    builds: BTreeMap<String, LatestBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_check: Option<DateTime<Utc>>,
}

/// The cache of latest builds (`<data>/epic/builds.json`).
pub struct Builds {
    path: PathBuf,
    saved: Mutex<Saved>,
    /// One check at a time.
    checking: tokio::sync::Mutex<()>,
}

impl Builds {
    pub fn new(epic_dir: &Path) -> Self {
        let path = epic_dir.join("builds.json");
        let saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).map_err(|e| tracing::info!("ignoring {}: {e}", path.display())).ok()).unwrap_or_default();
        Builds { path, saved: Mutex::new(saved), checking: Default::default() }
    }

    fn save(&self) {
        let r = (|| -> Result<()> {
            let bytes = serde_json::to_vec_pretty(&*self.saved.lock().unwrap())?;
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if let Err(e) = r {
            tracing::warn!("saving {}: {e:#}", self.path.display());
        }
    }

    fn get(&self, app: &str) -> Option<LatestBuild> {
        self.saved.lock().unwrap().builds.get(app).cloned()
    }

    fn last_check(&self) -> Option<DateTime<Utc>> {
        self.saved.lock().unwrap().last_check
    }
}

/// What a check found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// The check didn't run: the last one is recent enough.
    pub skipped: bool,
    /// Games asked about.
    pub checked: usize,
    /// Installed games with a newer build (app names), after the check.
    pub updates: Vec<String>,
    /// Games whose answer changed since the last check (a new build appeared).
    pub changed: Vec<String>,
    /// What couldn't be checked, for people ("Alan Wake Remastered: …").
    pub errors: Vec<String>,
}

impl Epic {
    /// Epic's latest build of an installed app, as last checked (never asks Epic).
    pub fn latest_build(&self, app: &str) -> Option<LatestBuild> {
        self.builds.get(app)
    }

    /// Whether an installed game has a newer build, from the last check (never asks Epic).
    pub fn update_available(&self, installed: &Installed) -> bool {
        self.builds.get(&installed.app_name).is_some_and(|b| b.newer_than(installed))
    }

    /// When the last check ran.
    pub fn last_update_check(&self) -> Option<DateTime<Utc>> {
        self.builds.last_check()
    }

    /// Ask Epic for the current build of every installed game, unless the last check is
    /// younger than `max_age`. Failures are per game (an account signed out, Epic
    /// unreachable): the last answer for that game is kept.
    pub async fn check_updates(&self, max_age: Duration) -> CheckReport {
        let _one = self.builds.checking.lock().await;
        let installed = self.installed().unwrap_or_default();
        let fresh = self.builds.last_check().is_some_and(|t| (Utc::now() - t).to_std().is_ok_and(|age| age < max_age));
        if installed.is_empty() && !self.builds.saved.lock().unwrap().builds.is_empty() {
            self.builds.saved.lock().unwrap().builds.clear();
            self.builds.save();
        }
        if fresh || installed.is_empty() {
            return CheckReport { skipped: fresh, updates: installed.iter().filter(|i| self.update_available(i)).map(|i| i.app_name.clone()).collect(), ..Default::default() };
        }
        let mut report = CheckReport::default();
        let library = self.cached_library().await;
        for i in &installed {
            report.checked += 1;
            let before = self.builds.get(&i.app_name);
            match self.current_build(i, library.as_deref()).await {
                Ok((version, hash)) => {
                    let now = LatestBuild { version, manifest_hash: hash, checked_at: Utc::now() };
                    let same = before.as_ref().is_some_and(|b| b.version == now.version && b.manifest_hash == now.manifest_hash);
                    if now.newer_than(i) && !same {
                        report.changed.push(i.app_name.clone());
                    }
                    self.builds.saved.lock().unwrap().builds.insert(i.app_name.clone(), now);
                }
                Err(e) => {
                    tracing::info!(app = %i.app_name, "Epic build check: {e:#}");
                    report.errors.push(format!("{}: {e:#}", i.title));
                }
            }
        }
        {
            let mut s = self.builds.saved.lock().unwrap();
            // Games uninstalled since are forgotten.
            s.builds.retain(|app, _| installed.iter().any(|i| &i.app_name == app));
            s.last_check = Some(Utc::now());
        }
        self.builds.save();
        report.updates = installed.iter().filter(|i| self.update_available(i)).map(|i| i.app_name.clone()).collect();
        if !report.changed.is_empty() {
            tracing::info!("Epic: new builds for {}", report.changed.join(", "));
        }
        report
    }

    /// The `Live` build version and manifest hash of an installed game, through the account
    /// that installed it (else another signed-in account that owns it).
    async fn current_build(&self, i: &Installed, library: Option<&crate::Library>) -> Result<(String, String)> {
        let accounts = self.accounts()?;
        let signed_in = |id: &str| accounts.iter().any(|a| a.account_id == id && a.state == AccountState::SignedIn);
        let owners = library.and_then(|l| l.title(&i.app_name)).map(|t| t.owners.clone()).unwrap_or_default();
        let account = std::iter::once(&i.account).chain(owners.iter()).find(|a| !a.is_empty() && signed_in(a)).context("no signed-in Epic account owns it")?;
        let token = self.access_token(account).await?;
        let element = self.api.build_info(token.expose(), &i.namespace, &i.catalog_item_id, &i.app_name).await?;
        Ok((element.build_version.clone(), element.hash.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(version: &str, hash: &str) -> Installed {
        serde_json::from_value(serde_json::json!({
            "app_name": "Kestrel", "title": "Alan Wake Remastered", "namespace": "ns", "catalog_item_id": "item",
            "dir": "/g", "prefix": "/p", "exe": "a.exe", "version": version, "manifest_hash": hash,
            "installed_at": "2026-11-18T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn newer_by_manifest_then_version() {
        let b = |v: &str, h: &str| LatestBuild { version: v.into(), manifest_hash: h.into(), checked_at: Utc::now() };
        assert!(b("2.0", "bb").newer_than(&installed("1.0", "aa")));
        assert!(!b("1.0", "AA").newer_than(&installed("1.0", "aa")), "hashes compare without case");
        assert!(b("1.0", "bb").newer_than(&installed("1.0", "aa")), "same version, rebuilt manifest");
        // The same manifest is the same build, however the API spells its version.
        assert!(!b("1.0+CL-123", "aa").newer_than(&installed("1.0", "aa")));
        // An older record without a hash: the versions decide; an empty answer never claims one.
        assert!(!b("1.0", "bb").newer_than(&installed("1.0", "")));
        assert!(b("1.1", "bb").newer_than(&installed("1.0", "")));
        assert!(!b("", "").newer_than(&installed("1.0", "aa")));
    }

    /// Signed in on a fake Epic, with Alan Wake Remastered installed at 1.0.
    async fn rig() -> (tempfile::TempDir, std::sync::Arc<crate::http::Fake>, Epic) {
        let tmp = tempfile::tempdir().unwrap();
        let fake = std::sync::Arc::new(crate::http::Fake::new());
        let epic = Epic::new(tmp.path(), fake.clone(), std::sync::Arc::new(crate::secrets::Memory::new()));
        let token = serde_json::json!({
            "access_token": "eg1~a", "expires_in": 8 * 3600, "token_type": "bearer", "refresh_token": "eg1~r",
            "refresh_expires": 30 * 86400, "account_id": "acc", "displayName": "Player One", "client_id": crate::config::EGL_CLIENT_ID,
        });
        fake.reply("/oauth/token", 200, &token.to_string());
        fake.reply("/public/account/acc/externalAuths", 200, "[]");
        fake.reply("/public/account/acc", 200, r#"{"id":"acc","displayName":"Player One"}"#);
        epic.login("0a1b2c3d4e5f60718293a4b5c6d7e8f9").await.unwrap();
        let mut i = installed("1.0", "aa");
        i.account = "acc".into();
        epic.installs.upsert(i).unwrap();
        (tmp, fake, epic)
    }

    const BUILD_API: &str = "/assets/v2/platform/Windows/namespace/ns/catalogItem/item/app/Kestrel/label/Live";

    fn build(version: &str, hash: &str) -> String {
        serde_json::json!({"elements": [{
            "appName": "Kestrel", "labelName": "Live", "buildVersion": version, "hash": hash,
            "manifests": [{"uri": "https://cdn.example/Builds/m.manifest", "queryParams": [{"name": "sig", "value": "s3cr3t"}]}],
            "secrets": {"0011": "AESKEY"},
        }]})
        .to_string()
    }

    #[tokio::test]
    async fn checks_are_cached_and_find_new_builds() {
        let (tmp, fake, epic) = rig().await;
        let asked = || fake.requests().iter().filter(|r| r.url.contains(BUILD_API)).count();
        // The installed build is current.
        fake.reply(BUILD_API, 200, &build("1.0", "aa"));
        let r = epic.check_updates(Duration::ZERO).await;
        assert_eq!((r.skipped, r.checked, r.updates.len(), r.changed.len(), r.errors.len()), (false, 1, 0, 0, 0), "{r:?}");
        assert!(!epic.update_available(&epic.installed_game("Kestrel").unwrap()));
        // Within the interval nothing is asked.
        assert!(epic.check_updates(CHECK_INTERVAL).await.skipped);
        assert_eq!(asked(), 1);
        // A new build: found once (changed), then still an update but not news.
        fake.reply(BUILD_API, 200, &build("1.1", "bb"));
        let r = epic.check_updates(Duration::ZERO).await;
        assert_eq!((r.updates.clone(), r.changed.clone()), (vec!["Kestrel".to_owned()], vec!["Kestrel".to_owned()]));
        fake.reply(BUILD_API, 200, &build("1.1", "bb"));
        let r = epic.check_updates(Duration::ZERO).await;
        assert_eq!((r.updates.len(), r.changed.len()), (1, 0));
        assert_eq!(epic.latest_build("Kestrel").unwrap().version, "1.1");
        // A failed check keeps the last answer.
        fake.reply(BUILD_API, 503, "{}");
        let r = epic.check_updates(Duration::ZERO).await;
        assert_eq!((r.updates.len(), r.errors.len()), (1, 1), "{r:?}");
        // Kept across restarts, and nothing from the reply but versions and hashes.
        let text = std::fs::read_to_string(tmp.path().join("epic/builds.json")).unwrap();
        assert!(text.contains("1.1") && !text.contains("AESKEY") && !text.contains("s3cr3t") && !text.contains("eg1~"), "{text}");
        let again = Epic::new(tmp.path(), fake.clone(), std::sync::Arc::new(crate::secrets::Memory::new()));
        assert!(again.update_available(&again.installed_game("Kestrel").unwrap()));
        // Updated: the record's build matches and the badge goes away without asking.
        let mut i = epic.installed_game("Kestrel").unwrap();
        (i.version, i.manifest_hash) = ("1.1".into(), "bb".into());
        epic.installs.upsert(i.clone()).unwrap();
        assert!(!epic.update_available(&i));
        // Uninstalled games are forgotten at the next check.
        epic.installs.remove("Kestrel").unwrap();
        assert!(epic.check_updates(Duration::ZERO).await.updates.is_empty());
        assert!(epic.latest_build("Kestrel").is_none());
        assert!(fake.unused().is_empty(), "{:?}", fake.unused());
    }

    #[tokio::test]
    async fn a_signed_out_account_is_an_error_not_a_request() {
        let (_tmp, fake, epic) = rig().await;
        let mut i = epic.installed_game("Kestrel").unwrap();
        i.account = "someone-else".into();
        epic.installs.upsert(i).unwrap();
        let r = epic.check_updates(Duration::ZERO).await;
        assert!(r.errors[0].contains("no signed-in Epic account owns it"), "{r:?}");
        assert!(!fake.requests().iter().any(|q| q.url.contains(BUILD_API)));
    }

    #[test]
    fn a_corrupt_cache_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("builds.json"), b"{not json").unwrap();
        let b = Builds::new(tmp.path());
        assert!(b.get("Kestrel").is_none() && b.last_check().is_none());
    }
}
