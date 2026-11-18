//! Steam in local mode (design/STORE-STEAM.md phase 1): the Steam store's provider, built
//! only from the Steam client's own files, read-only.
//!
//! - **Library:** every app manifest in every Steam library (`libraryfolders.vdf`),
//!   Proton or native, less tools and runtimes ([`super::is_tool`]). Names, type,
//!   platforms, developer and publisher from `appinfo.vdf` (falling back to the
//!   manifest's name), size and update state from the manifest, art from Steam's local
//!   library cache (`exports::steamart`), play time and last played from the playing
//!   user's `localconfig.vdf`, achievement counts from the stats cache.
//! - **Account:** the client's ([`AccountState::ClientManaged`]): each game's data is read
//!   for the Steam user who installed it (`LastOwner`), else the client's most recent user.
//!   No Steam sign-in, no Steam secret: only the play-time keys of `localconfig.vdf` are
//!   read (`playtime`), never its tickets.
//! - **Play:** `steam://rungameid/<appid>` through the running (or started) client, which
//!   keeps licensing, DRM, Steamworks, the overlay, cloud sync and play-time records.
//!   Install, update, verify and uninstall are Steam's own for now.
//!
//! Nothing here writes, moves or locks a Steam file, or signals Steam. A scan is cached
//! for [`TTL`]; `refresh` rescans.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::art::SteamArt;
use quiver_api::exports::SteamUser;
use quiver_api::stores::{
    Achievement, AchievementCount, Achievements, ClientInstall, CloudState, Dlc, GameDetail, StorePlaytime,
};
use quiver_api::{Art, GameRef, LibraryEntry, Playtime, StoreId};

use super::{AppInfo, appinfo, is_tool, playtime, remotecache, stats};
use crate::exports::steam::{self, Steam};
use crate::exports::steamgames::{self, AppManifest};
use crate::exports::{steamart, steamlaunch};
use crate::stores::{AccountInfo, AccountState, BoxFuture, Capabilities, Launched, StoreProvider, Support};

/// How long a scan of Steam's files is reused.
pub const TTL: Duration = Duration::from_secs(15);

/// Achievement icons live on Steam's community CDN, named by the schema's file names.
const ICON_CDN: &str = "https://cdn.akamai.steamstatic.com/steamcommunity/public/images/apps";

/// The Steam provider. Cheap to clone (scans run on a blocking thread).
#[derive(Clone)]
pub struct SteamLocal {
    inner: Arc<Inner>,
}

struct Inner {
    home: PathBuf,
    cache: Mutex<Option<(Instant, Arc<Scan>)>>,
}

/// One read of Steam's files.
#[derive(Debug, Default)]
pub struct Scan {
    pub root: Option<PathBuf>,
    pub users: Vec<SteamUser>,
    pub apps: Vec<App>,
    /// Why there are no Steam games ("Steam not found").
    pub note: Option<String>,
}

/// An installed Steam app with what the client knows of it.
#[derive(Debug, Clone)]
pub struct App {
    pub manifest: AppManifest,
    pub info: Option<AppInfo>,
    /// The Steam user (account id) whose play time and achievements these are.
    pub user: Option<String>,
    pub playtime: Option<playtime::AppPlaytime>,
    pub art: SteamArt,
    pub achievements: Option<AchievementCount>,
}

impl App {
    pub fn name(&self) -> String {
        self.info.as_ref().and_then(|i| i.name.clone()).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| self.manifest.name.clone())
    }

    fn last_played(&self) -> Option<i64> {
        self.playtime.and_then(|p| p.last_played).max(self.manifest.last_played)
    }

    fn store_playtime(&self) -> Option<StorePlaytime> {
        let p = self.playtime;
        let last = self.last_played();
        (p.is_some() || last.is_some()).then(|| StorePlaytime {
            total_seconds: p.map_or(0, |p| p.minutes * 60),
            two_weeks_seconds: p.map(|p| p.two_weeks_minutes * 60).filter(|&s| s > 0),
            last_played: last.and_then(time),
        })
    }

    fn client_install(&self) -> ClientInstall {
        let m = &self.manifest;
        ClientInstall {
            dir: m.install_dir.clone(),
            library: Some(m.library.clone()),
            size_bytes: m.size_on_disk,
            complete: m.installed,
            update_pending: m.update_pending(),
            build_id: m.build_id.clone(),
            platforms: self.info.as_ref().map(|i| i.oslist.clone()).unwrap_or_default(),
            art: self.art.clone(),
            playtime: self.store_playtime(),
            achievements: self.achievements,
            launch_url: Some(steamlaunch::rungameid_url(m.app_id)),
        }
    }

    /// The library entry (see `LibraryEntry::client`).
    pub fn entry(&self, running: bool) -> LibraryEntry {
        LibraryEntry {
            game: GameRef::steam(self.manifest.app_id),
            name: self.name(),
            installed: None,
            update_available: self.manifest.update_pending(),
            running,
            art: Art::default(),
            owned_by: Vec::new(),
            installable: false,
            steam: None,
            space_id: None,
            client: Some(self.client_install()),
            records: Vec::new(),
        }
    }
}

fn time(unix: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(unix, 0)
}

impl Scan {
    /// Read Steam's files under `steam` (None: Steam isn't installed).
    pub fn read(steam: Option<&Steam>) -> Scan {
        let Some(steam) = steam else { return Scan { note: Some("Steam not found".into()), ..Default::default() } };
        let users = steam.users().unwrap_or_default();
        let manifests: Vec<AppManifest> = steamgames::library_folders(steam).iter().flat_map(|l| steamgames::app_manifests(l)).collect();
        let ids: HashSet<u32> = manifests.iter().map(|m| m.app_id).collect();
        let mut infos = appinfo::read(&steam.root, |id| ids.contains(&id)).unwrap_or_else(|e| {
            tracing::info!("Steam appinfo.vdf: {e:#} (names come from app manifests)");
            HashMap::new()
        });
        let default_user = users.iter().find(|u| u.most_recent).or(users.first()).map(|u| u.id.clone());
        let mut playtimes: HashMap<String, HashMap<u32, playtime::AppPlaytime>> = HashMap::new();
        let mut seen = HashSet::new();
        let mut apps = Vec::new();
        for m in manifests {
            let info = infos.remove(&m.app_id);
            if is_tool(m.app_id, info.as_ref(), &m.name) || !seen.insert(m.app_id) {
                continue;
            }
            let user = steamgames::owner_user(&m, &users).or_else(|| default_user.clone());
            let playtime = user.as_ref().and_then(|u| playtimes.entry(u.clone()).or_insert_with(|| playtime::read(&steam.root, u)).get(&m.app_id).copied());
            let achievements = user
                .as_ref()
                .and_then(|u| stats::read(&steam.root, u, m.app_id).ok().flatten())
                .map(|a| AchievementCount { unlocked: a.unlocked, total: a.total });
            let art = steamart::library_art(&steam.root, m.app_id);
            apps.push(App { manifest: m, info, user, playtime, art, achievements });
        }
        apps.sort_by_key(|a| a.name().to_lowercase());
        Scan { root: Some(steam.root.clone()), users, apps, note: None }
    }

    pub fn app(&self, app_id: u32) -> Option<&App> {
        self.apps.iter().find(|a| a.manifest.app_id == app_id)
    }

    fn user_name(&self, id: Option<&str>) -> Option<String> {
        let u = self.users.iter().find(|u| Some(u.id.as_str()) == id)?;
        u.persona_name.clone().or(u.account_name.clone()).or(Some(u.id.clone()))
    }

    /// A game's page, reading what the library scan leaves out (the achievement list, cloud
    /// files, DLC names).
    pub fn detail(&self, app_id: u32) -> Result<GameDetail> {
        let root = self.root.as_deref().context("Steam not found")?;
        let app = self.app(app_id).with_context(|| format!("steam:{app_id} is not installed in Steam"))?;
        let user = app.user.as_deref();
        let mut unavailable = Vec::new();
        let achievements = match user.map(|u| stats::read(root, u, app_id)) {
            Some(Ok(Some(a))) => Some(achievements_of(app_id, a)),
            Some(Err(e)) => {
                tracing::info!(app_id, "Steam stats cache: {e:#}");
                unavailable.push("achievements".to_owned());
                None
            }
            _ => {
                unavailable.push("achievements".to_owned());
                None
            }
        };
        let cloud = user.and_then(|u| remotecache::read(root, u, app_id)).map(|c| cloud_of(&c));
        let info = app.info.as_ref();
        if info.is_none() {
            unavailable.extend(["developer", "publisher", "release date"].map(str::to_owned));
        }
        let dlc_ids: Vec<u32> = info.map(|i| i.dlc.clone()).unwrap_or_default();
        let wanted: HashSet<u32> = dlc_ids.iter().copied().collect();
        let names = if wanted.is_empty() { HashMap::new() } else { appinfo::read(root, |id| wanted.contains(&id)).unwrap_or_default() };
        let dlc = dlc_ids
            .iter()
            .map(|&id| Dlc { game: GameRef::steam(id), name: names.get(&id).and_then(|i| i.name.clone()), installed: app.manifest.dlc_installed.contains(&id), owned: None })
            .collect();
        unavailable.push("description".to_owned());
        Ok(GameDetail {
            game: GameRef::steam(app_id),
            name: app.name(),
            capabilities: capabilities(),
            kind: info.and_then(|i| i.kind.clone()),
            developer: info.and_then(|i| i.developer.clone()),
            publisher: info.and_then(|i| i.publisher.clone()),
            release_date: info.and_then(|i| i.release).and_then(time),
            store_url: Some(format!("https://store.steampowered.com/app/{app_id}/")),
            install: Some(app.client_install()),
            playtime: app.store_playtime(),
            achievements,
            cloud,
            dlc,
            account: self.user_name(user),
            unavailable,
            // quiverd adds the hook (it knows where `quiver` is installed).
            steam_hook: None,
            description: None,
            linked: None,
            extras: Vec::new(),
        })
    }
}

fn achievements_of(app_id: u32, a: stats::Achievements) -> Achievements {
    let icon = |f: Option<String>| f.filter(|f| !f.is_empty()).map(|f| format!("{ICON_CDN}/{app_id}/{f}"));
    Achievements {
        unlocked: a.unlocked,
        total: a.total,
        list: a
            .list
            .into_iter()
            .map(|x| Achievement {
                id: x.api_name,
                name: x.name,
                description: x.description,
                hidden: x.hidden,
                unlocked_at: x.unlocked_at.and_then(time),
                unlocked: x.unlocked,
                icon: icon(x.icon),
                icon_gray: icon(x.icon_gray),
            })
            .collect(),
    }
}

/// Sync state 1 is "in sync" (the only value seen; others count as not synced).
fn cloud_of(c: &remotecache::CloudState) -> CloudState {
    CloudState {
        files: c.files.len(),
        bytes: c.total_size(),
        last_upload: c.files.iter().filter_map(|f| f.remote_time).max().and_then(time),
        unsynced: c.files.iter().filter(|f| f.sync_state != 1).count(),
    }
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        library: Support::ReadOnly,
        install: Support::No,
        launch: Support::Delegated,
        settings: Support::No,
        // Mod profiles, overlays, config patches and options, applied by `quiver steam-run`
        // (STORE-STEAM.md §10).
        mods: Support::Quiver,
        cloud_saves: Support::ReadOnly,
        achievements: Support::ReadOnly,
        playtime: Support::ReadOnly,
    }
}

impl SteamLocal {
    /// Steam under `home` (native, else Flatpak; see `Steam::find`).
    pub fn new(home: &Path) -> Self {
        Self { inner: Arc::new(Inner { home: home.to_path_buf(), cache: Mutex::new(None) }) }
    }

    /// The cached scan, or a fresh one when it is older than [`TTL`] or `refresh`.
    pub fn scan(&self, refresh: bool) -> Arc<Scan> {
        let mut c = self.inner.cache.lock().unwrap();
        if let Some((at, s)) = c.as_ref()
            && !refresh
            && at.elapsed() < TTL
        {
            return s.clone();
        }
        let s = Arc::new(Scan::read(Steam::find(&self.inner.home).as_ref()));
        *c = Some((Instant::now(), s.clone()));
        s
    }

    async fn scan_async(&self, refresh: bool) -> Result<Arc<Scan>> {
        let me = self.clone();
        Ok(tokio::task::spawn_blocking(move || me.scan(refresh)).await?)
    }
}

fn app_id(game: &GameRef) -> Result<u32> {
    game.as_steam().with_context(|| format!("{game} is not a Steam game"))
}

impl StoreProvider for SteamLocal {
    fn id(&self) -> StoreId {
        StoreId::Steam
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn account(&self) -> AccountInfo {
        let s = self.scan(false);
        let name = s.users.iter().find(|u| u.most_recent).or(s.users.first()).and_then(|u| s.user_name(Some(&u.id)));
        AccountInfo { state: AccountState::ClientManaged, name, note: s.note.clone() }
    }

    fn library(&self, refresh: bool) -> BoxFuture<'_, Result<Vec<LibraryEntry>>> {
        Box::pin(async move {
            let s = self.scan_async(refresh).await?;
            let running = if s.apps.is_empty() { HashSet::new() } else { tokio::task::spawn_blocking(steam::running_apps).await? };
            Ok(s.apps.iter().map(|a| a.entry(running.contains(&a.manifest.app_id))).collect())
        })
    }

    fn detail(&self, game: GameRef, refresh: bool) -> BoxFuture<'_, Result<GameDetail>> {
        Box::pin(async move {
            let id = app_id(&game)?;
            let s = self.scan_async(refresh).await?;
            tokio::task::spawn_blocking(move || s.detail(id)).await?
        })
    }

    fn playtime(&self, game: GameRef) -> BoxFuture<'_, Result<Playtime>> {
        Box::pin(async move {
            let id = app_id(&game)?;
            let s = self.scan_async(false).await?;
            let p = s.app(id).and_then(App::store_playtime).unwrap_or_default();
            // Steam's totals (they count launches through `quiver steam-run` too), with the
            // sessions Quiver recorded for those launches.
            let sessions = crate::settings::playtime(game).map(|p| p.sessions).unwrap_or_default();
            Ok(Playtime { total_seconds: p.total_seconds, last_played: p.last_played, sessions })
        })
    }

    fn launch<'a>(&'a self, _source: &'a str, game: GameRef, profile: Option<String>) -> BoxFuture<'a, Result<Launched>> {
        Box::pin(async move {
            let id = app_id(&game)?;
            if profile.is_some() {
                bail!("{game}: Steam plays it as it is; mod profiles don't apply");
            }
            let s = self.scan_async(false).await?;
            let app = s.app(id).with_context(|| format!("{game} is not installed in Steam"))?;
            if !app.manifest.installed {
                bail!("{} is still downloading in Steam", app.name());
            }
            let home = self.inner.home.clone();
            let argv = tokio::task::spawn_blocking(move || {
                let steam = Steam::find(&home).context("Steam not found")?;
                steamlaunch::launch(&steam, id)
            })
            .await??;
            Ok(Launched::Delegated { url: steamlaunch::rungameid_url(id), argv })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(app_id: u32, name: &str, flags: u32, extra: &str) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\t\"{flags}\"\n\t\"installdir\"\t\t\"{name}\"\n\t\"buildid\"\t\t\"7\"\n\t\"LastOwner\"\t\t\"76561197960266729\"\n\t\"SizeOnDisk\"\t\t\"1000000\"\n{extra}}}\n"
        )
    }

    /// A Steam tree with two libraries, a game, a native app, Proton and a download.
    fn fixture() -> (tempfile::TempDir, Steam) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".steam/steam");
        let lib2 = tmp.path().join("games/SteamLibrary");
        for d in [root.join("steamapps"), root.join("userdata/1001/config"), root.join("userdata/1001/620"), lib2.join("steamapps"), root.join("config")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n", root.display(), lib2.display()),
        )
        .unwrap();
        std::fs::write(root.join("steamapps/appmanifest_620.acf"), manifest(620, "Portal 2", 4, "\t\"LastPlayed\"\t\t\"1700000000\"\n")).unwrap();
        std::fs::write(root.join("steamapps/appmanifest_1493710.acf"), manifest(1493710, "Proton Experimental", 4, "")).unwrap();
        std::fs::write(lib2.join("steamapps/appmanifest_105600.acf"), manifest(105600, "Terraria", 4, "")).unwrap();
        std::fs::write(lib2.join("steamapps/appmanifest_400.acf"), manifest(400, "Portal", 1026, "")).unwrap();
        std::fs::write(
            root.join("config/loginusers.vdf"),
            "\"users\"\n{\n\t\"76561197960266729\"\n\t{\n\t\t\"AccountName\"\t\t\"someone\"\n\t\t\"PersonaName\"\t\t\"Some One\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t}\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("userdata/1001/config/localconfig.vdf"),
            "\"UserLocalConfigStore\"\n{\n\"Software\"\n{\n\"Valve\"\n{\n\"Steam\"\n{\n\"apps\"\n{\n\"620\"\n{\n\"Playtime\"\t\"125\"\n\"Playtime2wks\"\t\"30\"\n\"LastPlayed\"\t\"1700000500\"\n}\n}\n}\n}\n}\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("userdata/1001/620/remotecache.vdf"),
            "\"620\"\n{\n\"ChangeNumber\"\t\"5\"\n\"save1.sav\"\n{\n\"root\"\t\"0\"\n\"size\"\t\"100\"\n\"remotetime\"\t\"1700000400\"\n\"syncstate\"\t\"1\"\n}\n\"save2.sav\"\n{\n\"root\"\t\"0\"\n\"size\"\t\"50\"\n\"remotetime\"\t\"1700000600\"\n\"syncstate\"\t\"3\"\n}\n}\n",
        )
        .unwrap();
        let steam = Steam::find(tmp.path()).unwrap();
        (tmp, steam)
    }

    #[test]
    fn library_from_every_steam_library() {
        let (_tmp, steam) = fixture();
        let s = Scan::read(Some(&steam));
        let names: Vec<String> = s.apps.iter().map(App::name).collect();
        // Proton is a tool; no appinfo cached here, so names and the manifest decide.
        assert_eq!(names, ["Portal", "Portal 2", "Terraria"]);
        let p2 = s.app(620).unwrap().entry(true);
        assert_eq!((p2.game, p2.running, p2.steam_only(), p2.ubisoft_id()), (GameRef::steam(620), true, true, None));
        let c = p2.client.as_ref().unwrap();
        assert!(c.complete && !c.update_pending && c.size_bytes == 1_000_000);
        assert_eq!(c.launch_url.as_deref(), Some("steam://rungameid/620"));
        let pt = c.playtime.unwrap();
        assert_eq!((pt.total_seconds, pt.two_weeks_seconds), (125 * 60, Some(30 * 60)));
        // The later of localconfig's and the manifest's last-played times.
        assert_eq!(pt.last_played.map(|t| t.timestamp()), Some(1_700_000_500));
        // Still downloading an update: listed, with the update flagged.
        let portal = s.app(400).unwrap().entry(false);
        assert!(portal.update_available && !portal.client.as_ref().unwrap().complete);
        // Read for the installing user, named from loginusers.vdf.
        assert_eq!(s.apps[1].user.as_deref(), Some("1001"));
        assert_eq!(s.user_name(Some("1001")).as_deref(), Some("Some One"));
    }

    /// This machine's Steam, read-only, printing counts only:
    /// `cargo test -p quiver-core steamlocal::provider::tests::real_steam -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_steam() {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let t = Instant::now();
        let s = Scan::read(Steam::find(&home).as_ref());
        let took = t.elapsed();
        let n = |f: &dyn Fn(&App) -> bool| s.apps.iter().filter(|a| f(a)).count();
        println!(
            "apps {} (appinfo {}, kind game {}, linux {}, playtime {}, achievements {}, capsule art {}, complete {}, update pending {}) in {took:?}",
            s.apps.len(),
            n(&|a| a.info.is_some()),
            n(&|a| a.info.as_ref().and_then(|i| i.kind.as_deref()) == Some("game")),
            n(&|a| a.info.as_ref().is_some_and(|i| i.oslist.iter().any(|o| o == "linux"))),
            n(&|a| a.playtime.is_some()),
            n(&|a| a.achievements.is_some_and(|c| c.total > 0)),
            n(&|a| a.art.capsule.is_some()),
            n(&|a| a.manifest.installed),
            n(&|a| a.manifest.update_pending()),
        );
        let t = Instant::now();
        let details: Vec<_> = s.apps.iter().map(|a| s.detail(a.manifest.app_id)).collect();
        println!(
            "details ok {} of {}, with achievements {}, cloud {}, dlc lists {}, in {:?}",
            details.iter().filter(|d| d.is_ok()).count(),
            details.len(),
            details.iter().flatten().filter(|d| d.achievements.is_some()).count(),
            details.iter().flatten().filter(|d| d.cloud.is_some()).count(),
            details.iter().flatten().filter(|d| !d.dlc.is_empty()).count(),
            t.elapsed()
        );
    }

    #[test]
    fn detail_reads_cloud_state_and_says_what_is_missing() {
        let (_tmp, steam) = fixture();
        let s = Scan::read(Some(&steam));
        let d = s.detail(620).unwrap();
        assert_eq!(d.name, "Portal 2");
        assert_eq!(d.store_url.as_deref(), Some("https://store.steampowered.com/app/620/"));
        let cloud = d.cloud.unwrap();
        assert_eq!((cloud.files, cloud.bytes, cloud.unsynced), (2, 150, 1));
        assert_eq!(cloud.last_upload.map(|t| t.timestamp()), Some(1_700_000_600));
        // No stats or appinfo cached: unknown, not "none".
        assert!(d.achievements.is_none());
        assert!(d.unavailable.iter().any(|u| u == "achievements") && d.unavailable.iter().any(|u| u == "developer"));
        assert_eq!(d.account.as_deref(), Some("Some One"));
        assert!(s.detail(1).is_err());
        assert!(Scan::read(None).note.is_some());
    }
}
