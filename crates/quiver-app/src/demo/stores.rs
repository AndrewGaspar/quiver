//! Demo Steam games (Steam local mode): installed Steam games beside the Ubisoft library,
//! with Steam's art from its public CDN, made-up play time and achievement progress, and a
//! page per game (`Request::GameDetail`). Play answers as quiverd does for a Steam game: a
//! finished Play job ("Asked Steam to start it"). `QUIVER_DEMO_GLOBAL=steam-library=none`
//! leaves them out. Hades starts through `quiver steam-run` (the hook); the others don't,
//! and `SteamHook` turns it on or off (Steam isn't running in the demo, so nothing waits).

use anyhow::{Result, anyhow, bail};
use chrono::{TimeZone, Utc};
use quiver_api::exports::{PatchState, SteamHook};
use quiver_api::stores::{Achievement, AchievementCount, Achievements, ClientInstall, CloudState, Dlc, GameDetail, StorePlaytime};
use quiver_api::{ActivityEntry, Art, Event, GameRef, Job, JobKind, LibraryEntry, Playtime, Request, StoreId};
use serde_json::{Value, json};

use super::{Demo, GB, State, at};

/// A demo Steam game.
struct SteamFixture {
    app_id: u32,
    name: &'static str,
    developer: &'static str,
    released: (i32, u32, u32),
    gb: f64,
    linux: bool,
    hours: u64,
    last_days: i64,
    unlocked: usize,
    total: usize,
    dlc: &'static [&'static str],
}

const STEAM: &[SteamFixture] = &[
    SteamFixture { app_id: 1145360, name: "Hades", developer: "Supergiant Games", released: (2020, 9, 17), gb: 15.2, linux: false, hours: 61, last_days: 2, unlocked: 37, total: 49, dlc: &[] },
    SteamFixture { app_id: 413150, name: "Stardew Valley", developer: "ConcernedApe", released: (2016, 2, 26), gb: 0.6, linux: true, hours: 128, last_days: 4, unlocked: 31, total: 49, dlc: &[] },
    SteamFixture { app_id: 1086940, name: "Baldur's Gate 3", developer: "Larian Studios", released: (2023, 8, 3), gb: 148.9, linux: false, hours: 212, last_days: 9, unlocked: 22, total: 54, dlc: &["Digital Deluxe Edition"] },
    SteamFixture { app_id: 367520, name: "Hollow Knight", developer: "Team Cherry", released: (2017, 2, 24), gb: 9.1, linux: true, hours: 44, last_days: 30, unlocked: 41, total: 63, dlc: &[] },
    SteamFixture { app_id: 504230, name: "Celeste", developer: "Maddy Makes Games", released: (2018, 1, 25), gb: 1.2, linux: true, hours: 19, last_days: 75, unlocked: 18, total: 32, dlc: &[] },
    SteamFixture { app_id: 1091500, name: "Cyberpunk 2077", developer: "CD PROJEKT RED", released: (2020, 12, 10), gb: 71.4, linux: false, hours: 96, last_days: 140, unlocked: 30, total: 57, dlc: &["Phantom Liberty", "REDmod"] },
];

/// Demo Steam games hooked from the start.
pub(super) const HOOKED: &[u32] = &[1145360];

fn hook_state(f: &SteamFixture, hooked: bool) -> SteamHook {
    let command = format!("{}/.local/bin/quiver --source steam steam-run --app {}", super::home().display(), f.app_id);
    SteamHook {
        app_id: f.app_id,
        name: f.name.into(),
        state: if hooked { PatchState::On } else { PatchState::Off },
        steam_user: Some("1001".into()),
        launch_options: if hooked { format!("{command} %command%") } else { String::new() },
        command,
        hookable: true,
        note: None,
        product_id: None,
        steam_running: false,
        staged: false,
        unchanged: false,
        dropped_staged: false,
    }
}

fn fixture(app_id: u32) -> Option<&'static SteamFixture> {
    STEAM.iter().find(|f| f.app_id == app_id)
}

fn enabled() -> bool {
    crate::ui::global::demo_flag("steam-library").is_none_or(|v| v != "none")
}

fn playtime(f: &SteamFixture) -> StorePlaytime {
    StorePlaytime {
        total_seconds: f.hours * 3600 + 17 * 60,
        two_weeks_seconds: (f.last_days < 14).then_some((f.hours % 9 + 3) * 3600),
        last_played: Some(at(f.last_days, 21, 40)),
    }
}

fn install(f: &SteamFixture) -> ClientInstall {
    let dir = super::home().join(".local/share/Steam/steamapps/common").join(f.name.replace(['\'', ':'], ""));
    ClientInstall {
        dir,
        library: Some(super::home().join(".local/share/Steam")),
        size_bytes: (f.gb * GB as f64) as u64,
        complete: true,
        update_pending: f.app_id == 1086940,
        // Steam downloading an update, as its app manifest says.
        update: (f.app_id == 1086940).then(|| quiver_api::stores::ClientUpdate {
            state: quiver_api::stores::ClientUpdateState::Downloading,
            bytes_to_download: 1_800_000_000,
            bytes_downloaded: 720_000_000,
            target_build_id: Some(format!("{}", 19_000_001 + f.app_id % 1_000_000)),
            open_url: Some("steam://open/downloads".into()),
            ..Default::default()
        }),
        build_id: Some(format!("{}", 19_000_000 + f.app_id % 1_000_000)),
        platforms: if f.linux { vec!["windows".into(), "linux".into()] } else { vec!["windows".into()] },
        art: Default::default(),
        playtime: Some(playtime(f)),
        achievements: Some(AchievementCount { unlocked: f.unlocked, total: f.total }),
        launch_url: Some(format!("steam://rungameid/{}", f.app_id)),
    }
}

/// The demo's Steam games as library entries.
pub(super) fn entries() -> Vec<LibraryEntry> {
    if !enabled() {
        return Vec::new();
    }
    STEAM
        .iter()
        .map(|f| LibraryEntry {
            game: GameRef::steam(f.app_id),
            name: f.name.into(),
            installed: None,
            update_available: f.app_id == 1086940,
            running: false,
            art: Art::default(),
            owned_by: Vec::new(),
            installable: false,
            steam: None,
            space_id: None,
            client: Some(install(f)),
            records: Vec::new(),
        })
        .collect()
}

/// Made-up achievement names, so the page shows a realistic list.
const NAMES: &[(&str, &str)] = &[
    ("First Steps", "Finish the opening"),
    ("Old Friend", "Meet someone you knew"),
    ("Collector", "Find 25 hidden items"),
    ("Night Owl", "Play past midnight"),
    ("No Hits", "Clear an area without getting hit"),
    ("Explorer", "Visit every region"),
    ("Big Spender", "Spend 10,000 coins"),
    ("Perfectionist", "Finish everything"),
];

fn detail(f: &SteamFixture, hooked: bool) -> GameDetail {
    let list = (0..f.total.min(20))
        .map(|i| {
            let (name, description) = NAMES[i % NAMES.len()];
            let unlocked = i < f.unlocked.min(20) && i % 5 != 3;
            Achievement {
                id: format!("ACH_{i:02}"),
                name: if i < NAMES.len() { name.into() } else { format!("{name} {}", i / NAMES.len() + 1) },
                description: description.into(),
                hidden: i == 7,
                unlocked_at: unlocked.then(|| at(f.last_days + (i as i64) * 3, 20, 5)),
                unlocked,
                icon: None,
                icon_gray: None,
            }
        })
        .collect();
    let (y, m, d) = f.released;
    GameDetail {
        game: GameRef::steam(f.app_id),
        name: f.name.into(),
        capabilities: quiver_api::stores::Capabilities {
            library: quiver_api::stores::Support::ReadOnly,
            launch: quiver_api::stores::Support::Delegated,
            cloud_saves: quiver_api::stores::Support::ReadOnly,
            achievements: quiver_api::stores::Support::ReadOnly,
            playtime: quiver_api::stores::Support::ReadOnly,
            ..Default::default()
        },
        kind: Some("game".into()),
        developer: Some(f.developer.into()),
        publisher: Some(f.developer.into()),
        release_date: Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).single(),
        store_url: Some(format!("https://store.steampowered.com/app/{}/", f.app_id)),
        install: Some(install(f)),
        playtime: Some(playtime(f)),
        achievements: Some(Achievements { unlocked: f.unlocked, total: f.total, list }),
        cloud: Some(CloudState { files: 3 + (f.app_id % 7) as usize, bytes: 4_200_000 + (f.app_id as u64 % 97) * 100_000, last_upload: Some(at(f.last_days, 22, 31)), unsynced: 0 }),
        dlc: f.dlc.iter().enumerate().map(|(i, n)| Dlc { game: GameRef::steam(f.app_id + 1 + i as u32), name: Some((*n).into()), installed: i == 0, owned: None }).collect(),
        account: Some("demo".into()),
        unavailable: vec!["description".into()],
        steam_hook: Some(hook_state(f, hooked)),
        description: None,
        linked: None,
    }
}

/// Answer the requests that name a demo Steam game (and the store list).
pub(super) fn handle(demo: &Demo, st: &mut State, req: &Request) -> Result<Option<Value>> {
    let steam = |g: &GameRef| g.as_steam().and_then(fixture).ok_or_else(|| anyhow!("{g} is not installed in Steam"));
    Ok(Some(match req {
        Request::GameDetail { game, .. } if game.store == StoreId::Steam => {
            let f = steam(game)?;
            json!(detail(f, st.steam_hooked.contains(&f.app_id)))
        }
        Request::SteamHookStatus { app_id, .. } => {
            let f = steam(&GameRef::steam(*app_id))?;
            json!(hook_state(f, st.steam_hooked.contains(app_id)))
        }
        Request::SteamHook { app_id, on, .. } => {
            let f = steam(&GameRef::steam(*app_id))?;
            let changed = if *on { st.steam_hooked.insert(*app_id) } else { st.steam_hooked.remove(app_id) };
            let what = if *on { "now starts through Quiver (quiver steam-run)" } else { "starts without Quiver again" };
            if changed {
                st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action: format!("Steam: {} {what} (Steam user 1001)", f.name), game: Some(GameRef::steam(*app_id)) });
                demo.emit(Event::SteamGamesChanged { offer: vec![] });
            }
            json!(SteamHook { unchanged: !changed, ..hook_state(f, *on) })
        }
        Request::Playtime { game } if game.store == StoreId::Steam => {
            let p = playtime(steam(game)?);
            json!(Playtime { total_seconds: p.total_seconds, last_played: p.last_played, sessions: vec![] })
        }
        Request::Play { game, .. } if game.store == StoreId::Steam => {
            let f = steam(game)?;
            let id = st.next_job;
            st.next_job += 1;
            let job = Job {
                id,
                kind: JobKind::Play,
                game: **game,
                name: f.name.into(),
                status: "Asked Steam to start it".into(),
                downloaded: 0,
                total: 0,
                done: true,
                error: None,
                detail: vec![format!("steam://rungameid/{}", f.app_id)],
            };
            st.jobs.push(job.clone());
            st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action: format!("play {} from Steam", f.name), game: Some(**game) });
            demo.emit(Event::Job(job));
            json!(id)
        }
        // Steam updates its own games: quiverd opens Steam's downloads (a finished job).
        Request::Update { game } if game.store == StoreId::Steam => {
            let f = steam(game)?;
            if f.app_id != 1086940 {
                bail!("Steam has no update waiting for {}; Steam checks for updates itself", f.name);
            }
            let id = st.next_job;
            st.next_job += 1;
            let job = Job {
                id,
                kind: JobKind::Install,
                game: **game,
                name: f.name.into(),
                status: "Opened Steam's Downloads: Steam is updating it · 40% of 1.8 GB".into(),
                downloaded: 0,
                total: 0,
                done: true,
                error: None,
                detail: vec!["steam://open/downloads".into()],
            };
            st.jobs.push(job.clone());
            st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action: format!("asked Steam to update {}", f.name), game: Some(**game) });
            demo.emit(Event::Job(job));
            json!(id)
        }
        Request::Stores => {
            let caps = json!({"library": "quiver", "install": "quiver", "launch": "quiver", "settings": "quiver", "mods": "quiver", "cloud_saves": "quiver", "achievements": "quiver", "playtime": "quiver"});
            // Signed out, Ubisoft still has a provider; it brings no games.
            let ubisoft = if super::global::signed_in(st) {
                json!({"store": "ubisoft", "account": "signed_in", "account_name": "demo", "games": st.entries.iter().filter(|e| e.game.is_ubisoft()).count(), "capabilities": caps})
            } else {
                json!({"store": "ubisoft", "account": "needs_sign_in", "games": 0, "capabilities": caps})
            };
            json!([
                ubisoft,
                {"store": "steam", "account": "client_managed", "account_name": "demo", "games": STEAM.len(),
                 "capabilities": {"library": "read_only", "launch": "delegated", "cloud_saves": "read_only", "achievements": "read_only", "playtime": "read_only"}},
            ])
        }
        // No Epic account in the demo, and no signing in to the real Epic from it.
        Request::Epic(quiver_api::epic::EpicRequest::Accounts) => json!([]),
        Request::Epic(_) => anyhow::bail!("The demo doesn't sign in to Epic"),
        _ => return Ok(None),
    }))
}
