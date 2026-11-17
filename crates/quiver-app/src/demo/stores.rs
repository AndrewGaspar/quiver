//! Demo Steam games (Steam local mode): installed Steam games beside the Ubisoft library,
//! with Steam's art from its public CDN, made-up play time and achievement progress, and a
//! page per game (`Request::GameDetail`). Play answers as quiverd does for a Steam game: a
//! finished Play job ("Asked Steam to start it"). `QUIVER_DEMO_GLOBAL=steam-library=none`
//! leaves them out.

use anyhow::{Result, anyhow};
use chrono::{TimeZone, Utc};
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

fn detail(f: &SteamFixture) -> GameDetail {
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
        dlc: f.dlc.iter().enumerate().map(|(i, n)| Dlc { game: GameRef::steam(f.app_id + 1 + i as u32), name: Some((*n).into()), installed: i == 0 }).collect(),
        account: Some("demo".into()),
        unavailable: vec!["description".into()],
    }
}

/// Answer the requests that name a demo Steam game (and the store list).
pub(super) fn handle(demo: &Demo, st: &mut State, req: &Request) -> Result<Option<Value>> {
    let steam = |g: &GameRef| g.as_steam().and_then(fixture).ok_or_else(|| anyhow!("{g} is not installed in Steam"));
    Ok(Some(match req {
        Request::GameDetail { game, .. } if game.store == StoreId::Steam => json!(detail(steam(game)?)),
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
        Request::Stores => json!([
            {"store": "ubisoft", "account": "signed_in", "account_name": "demo", "games": st.entries.iter().filter(|e| e.game.is_ubisoft()).count(),
             "capabilities": {"library": "quiver", "install": "quiver", "launch": "quiver", "settings": "quiver", "mods": "quiver", "cloud_saves": "quiver", "achievements": "quiver", "playtime": "quiver"}},
            {"store": "steam", "account": "client_managed", "account_name": "demo", "games": STEAM.len(),
             "capabilities": {"library": "read_only", "launch": "delegated", "cloud_saves": "read_only", "achievements": "read_only", "playtime": "read_only"}},
        ]),
        _ => return Ok(None),
    }))
}
