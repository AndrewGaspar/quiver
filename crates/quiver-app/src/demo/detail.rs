//! Demo answers for the game page: settings edits and restores, game info, progression,
//! saves and backups, mod layers and profiles, exports, and the per-launch logs. Shapes
//! follow the daemon (`crates/quiverd`) and the UbiServices fixtures in
//! `crates/quiver-core/fixtures`; AC Origins (3539) and AC Odyssey (5059) are the rich ones,
//! the other installed games get plainer data.
//!
//! State is per [`Demo`] instance (keyed by its address), so tests don't share it.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration as Span, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use quiver_api::exports::{
    ExportKind, ExportRecord, ExportState, ExportStatus, PatchState, RestartReport, SteamExportResult, SteamGame, SteamGames, SteamPatchResult,
    SteamUser,
    SteamUsers,
};
use quiver_api::info::{
    Achievement, Achievements, AddOn, CloudRecord, CloudSlot, ConnectProfile, Features, GameInfo, InstallInfo, Link,
    ProductKind, Progress, RecordSlot, RestoreResult, Reward, SaveBackup, SaveSlot, SavesReport, SlotFile, SlotState, Stat,
};
use quiver_api::mods::{
    Conflict, Layer, ModManifest, ModRecord, ModVersion, ModsRequest, ModsState, ProfileInfo, ProfileRevision, SavesPlan, VANILLA,
    VersionChange, mods_of, resolve_entry, split_entry, stack_entry,
};
use quiver_api::savestores::{DEFAULT_STORE, NEW_STORE, SaveStore, SaveStoreInfo, SaveStoreOp, SaveStoresReport, SaveStoresRevision, StoreUser};
use quiver_api::{Event, GameSettings, LibraryEntry, Profile, Request, Revision, SettingOp};

use super::{ART_JSON, Demo, GB, State, at, home};

/// The user id of the demo account (see `Request::Status` in `demo.rs`).
const ACCOUNT: &str = "demo";
/// AC Origins' achievement set (product config `uplay.achievements`).
const ORIGINS_ACH_SET: &str = "3483afa6b8475717dabfe0f88025a465";
const ORIGINS_ACH: &str = include_str!("achievements-3539.txt");

#[derive(Deserialize)]
struct ArtFacts {
    product_id: u32,
    #[serde(default)]
    sort_string: Option<String>,
    #[serde(default)]
    dlc_count: u32,
    #[serde(default)]
    cloud_saves: bool,
    #[serde(default)]
    achievements: bool,
}

static FACTS: LazyLock<Vec<ArtFacts>> = LazyLock::new(|| serde_json::from_str(ART_JSON).unwrap_or_default());

/// Mutable demo state for the game page, one per `Demo`.
struct Detail {
    mods: BTreeMap<u32, Mods>,
    backups: BTreeMap<u32, Vec<SaveBackup>>,
    slots: BTreeMap<u32, Vec<SaveSlot>>,
    exports: Vec<ExportStatus>,
    steam_running: bool,
    /// Ubisoft games installed through Steam, and the offers setting.
    steam_games: Vec<SteamGame>,
    steam_offers: bool,
    next: u32,
    /// Games started from the demo: the profile they run and when they started.
    running: HashMap<u32, (Option<String>, DateTime<Utc>)>,
}

#[derive(Default)]
struct Mods {
    layers: Vec<Layer>,
    /// Every profile's history, oldest first (as the daemon stores it).
    history: Vec<Vec<ProfileRevision>>,
    /// Save stores, Default first (empty: Default only).
    stores: Vec<SaveStore>,
    /// The store vanilla uses (`None`: Default).
    vanilla_store: Option<String>,
    /// Versioned mods (the others are one-version mods, one per layer).
    records: Vec<ModRecord>,
}

/// Mod options (`Request::Options`).
mod options;
pub use options::file_text as options_file_text;
/// Quiver Tweaks (`Request::Tweaks`).
mod tweaks;

static DETAILS: LazyLock<Mutex<HashMap<usize, Detail>>> = LazyLock::new(Mutex::default);

/// Answer a game-page request, or hand it back for `demo.rs` to answer.
pub(super) fn handle(demo: &Demo, st: &mut State, req: Request) -> Result<std::result::Result<Value, Request>> {
    let key = demo as *const Demo as usize;
    let mut all = DETAILS.lock().unwrap();
    let d = all.entry(key).or_insert_with(|| Detail::new(&st.entries));
    sync_steam(st, &d.steam_games);
    let mut events = Vec::new();
    let out = match req {
        // A game only Steam installed: Steam starts it, and (switched to Quiver) its bridge
        // reports a session from Steam.
        Request::Play { game, .. } if entry(st, game.as_ubisoft().unwrap_or(0)).is_ok_and(|e| e.steam_only()) => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let e = entry(st, product_id)?.clone();
            let g = e.steam.clone().unwrap_or_else(|| unreachable!());
            let id = st.next_job;
            st.next_job += 1;
            let job = quiver_api::Job {
                id,
                kind: quiver_api::JobKind::Play,
                game: quiver_api::GameRef::ubisoft(product_id),
                name: e.name.clone(),
                status: "Asked Steam to start it".into(),
                downloaded: 0,
                total: 0,
                done: true,
                error: None,
                detail: vec![format!("steam://rungameid/{}", g.app_id)],
            };
            st.jobs.push(job.clone());
            events.push(Event::Job(job));
            if g.state.uses_quiver() && !st.sessions.iter().any(|s| s.product_id == product_id) {
                st.sessions.push(steam_session(product_id, g.app_id, Utc::now()));
                events.push(Event::SessionsChanged { sessions: st.sessions.clone() });
            }
            json!(id)
        }
        Request::SettingsGet { game } => {
            let Some(product_id) = game.as_ubisoft() else { bail!("{}: the demo keeps settings for Ubisoft games", game.0) };
            // The daemon's shape: the settings and their revision.
            let settings = st.settings.get(&product_id).cloned().unwrap_or_default();
            let revision = st.history.get(&product_id).and_then(|h| h.iter().map(|r| r.revision).max()).unwrap_or(0);
            json!({ "revision": revision, "settings": settings })
        }
        Request::SettingsSet { game, ops, message } => {
            let Some(product_id) = game.as_ubisoft() else { bail!("{}: the demo keeps settings for Ubisoft games", game.0) };
            // As quiverd: a Steam copy's settings apply when Steam starts it through Quiver.
            if entry(st, product_id)?.steam.is_none() {
                installed(st, product_id)?;
            }
            let mut s = st.settings.get(&product_id).cloned().unwrap_or_default();
            for op in &ops {
                if let SettingOp::Set { key, value } = op
                    && key == "proton"
                    && !PROTONS.contains(&value.as_str())
                {
                    bail!("no Proton build '{value}' (installed: {})", PROTONS.join(", "));
                }
                // The default profile is stored as its id, like the daemon does.
                let op = match op {
                    SettingOp::Set { key, value } if key == "profile" && !value.trim().is_empty() => {
                        let id = launch_profile(d, st, product_id, Some(value))?;
                        SettingOp::Set { key: key.clone(), value: id.unwrap_or_default() }
                    }
                    op => op.clone(),
                };
                s.apply(&op).map_err(|e| anyhow!(e))?;
            }
            let rev = push_revision(st, product_id, s, "gui", message);
            events.push(Event::SettingsChanged { product_id, revision: rev.revision, source: "gui".into() });
            json!(rev)
        }
        Request::SettingsRestore { game, revision } => {
            let Some(product_id) = game.as_ubisoft() else { bail!("{}: the demo keeps settings for Ubisoft games", game.0) };
            let old = st
                .history
                .get(&product_id)
                .and_then(|h| h.iter().find(|r| r.revision == revision))
                .map(|r| r.settings.clone())
                .or_else(|| (revision == 0).then(GameSettings::default))
                .ok_or_else(|| anyhow!("no revision {revision}"))?;
            let rev = push_revision(st, product_id, old, "gui", Some(format!("Restore r{revision}")));
            events.push(Event::SettingsChanged { product_id, revision: rev.revision, source: "gui".into() });
            json!(rev)
        }
        Request::GameInfo { product_id, .. } => json!(game_info(st, product_id)?),
        Request::GameProgress { product_id, .. } => json!(progress(st, product_id)?),
        Request::RemotePlaytime { product_id } => {
            let secs = remote_seconds(st, product_id);
            let last = st.playtime.get(&product_id).and_then(|p| p.last_played);
            json!({ "result": 0, "seconds": secs, "last_played": last.map(|t| t.to_rfc3339()) })
        }
        Request::Saves { product_id, cloud } => json!(d.saves(st, product_id, cloud)?),
        Request::SavesRestore { product_id, backup, confirmed } => {
            if !confirmed {
                bail!("restoring saves needs your approval: confirm it in the Saves tab");
            }
            if st.sessions.iter().any(|s| s.product_id == product_id) {
                bail!("{} is running; quit it before restoring saves", name(st, product_id));
            }
            json!(d.restore(st, product_id, &backup)?)
        }
        Request::Options(o) => options::handle(key, d, st, o, &mut events)?,
        Request::Tweaks(t) => tweaks::handle(key, d, st, t, &mut events)?,
        Request::Mods(m) => {
            let (v, changed) = d.mods(st, m)?;
            if let Some(product_id) = changed {
                events.push(Event::ModsChanged { product_id, game: None });
            }
            v
        }
        // Save stores (appended requests).
        Request::SaveStores { product_id } => {
            installed(st, product_id)?;
            json!(d.mods.entry(product_id).or_default().stores_report(product_id))
        }
        Request::SaveStoreHistory { .. } => json!(Vec::<SaveStoresRevision>::new()),
        Request::SaveStoreEdit { product_id, op, .. } => {
            installed(st, product_id)?;
            d.next += 1;
            let n = d.next;
            let r = d.mods.entry(product_id).or_default().edit_stores(product_id, op, n)?;
            events.push(Event::ModsChanged { product_id, game: None });
            json!(r)
        }
        Request::SavesMerge { .. } => bail!("merging save stores isn't simulated in demo mode"),
        Request::ExportList => json!(d.exports),
        Request::SteamGames { .. } => json!(SteamGames {
            root: Some(home().join(".local/share/Steam")),
            running: d.steam_running,
            games: d.steam_games.clone(),
            offers: d.steam_offers,
            command: steam_command(),
        }),
        Request::SteamPatch { app_id, .. } | Request::SteamUnpatch { app_id, .. } => {
            let undo = matches!(req, Request::SteamUnpatch { .. });
            let running = d.steam_running;
            let g = d.steam_games.iter_mut().find(|g| g.app_id == app_id).ok_or_else(|| anyhow!("Steam app {app_id} isn't an installed Ubisoft game"))?;
            if !undo && !g.patchable {
                bail!("{}", g.note.clone().unwrap_or_else(|| "can't switch this game".into()));
            }
            let before = g.state;
            g.offer = false;
            g.state = match (undo, before) {
                (false, PatchState::Off) => if running { PatchState::Waiting } else { PatchState::On },
                (false, PatchState::WaitingUndo) => PatchState::On,
                (true, PatchState::On | PatchState::ByHand) => if running { PatchState::WaitingUndo } else { PatchState::Off },
                (true, PatchState::Waiting) => PatchState::Off,
                (_, s) => s,
            };
            let unchanged = g.state == before || matches!((before, g.state), (PatchState::WaitingUndo, PatchState::On) | (PatchState::Waiting, PatchState::Off));
            let staged = matches!(g.state, PatchState::Waiting | PatchState::WaitingUndo);
            let shown = if g.state.uses_quiver() { demo_patched(&g.launch_options, g.product_id) } else { demo_unpatched(&g.launch_options) };
            if !staged {
                g.launch_options = shown.clone();
            }
            events.push(Event::SteamGamesChanged { offer: vec![] });
            let dropped_staged = matches!((before, g.state), (PatchState::WaitingUndo, PatchState::On) | (PatchState::Waiting, PatchState::Off));
            json!(SteamPatchResult { app_id, name: g.name.clone(), steam_user: "84211337".into(), launch_options: shown, staged, unchanged, dropped_staged })
        }
        Request::SteamDismissOffer { app_id } => {
            if let Some(g) = d.steam_games.iter_mut().find(|g| g.app_id == app_id) {
                g.offer = false;
            }
            events.push(Event::SteamGamesChanged { offer: vec![] });
            Value::Null
        }
        Request::SteamOffers { enabled } => {
            if let Some(on) = enabled {
                d.steam_offers = on;
                if !on {
                    d.steam_games.iter_mut().for_each(|g| g.offer = false);
                }
            }
            json!({ "enabled": d.steam_offers })
        }
        Request::SteamUsers => json!(SteamUsers {
            root: Some(home().join(".local/share/Steam")),
            users: vec![SteamUser {
                id: "84211337".into(),
                steam_id64: 76561198044477065,
                persona_name: Some("demo".into()),
                account_name: Some("demo".into()),
                most_recent: true,
            }],
            selected: Some("84211337".into()),
            running: d.steam_running,
            staged: d
                .exports
                .iter()
                .filter(|e| e.state == ExportState::WaitingForSteam)
                .map(|e| format!("add/update \"{}\" for Steam user 84211337", e.record.name))
                .chain(d.steam_games.iter().filter_map(|g| match g.state {
                    PatchState::Waiting => Some(format!("start through Quiver: \"{}\" (Steam app {}) for Steam user 84211337", g.name, g.app_id)),
                    PatchState::WaitingUndo => Some(format!("start through Ubisoft Connect again: \"{}\" (Steam app {}) for Steam user 84211337", g.name, g.app_id)),
                    _ => None,
                }))
                .collect(),
        }),
        Request::ExportDesktop { product_id, remove } => {
            let key = format!("quiver:{product_id}");
            let path = home().join(format!(".local/share/applications/quiver-{product_id}.desktop"));
            d.exports.retain(|e| !(e.record.key == key && e.record.kind == ExportKind::Desktop));
            if remove {
                json!({ "removed": true, "path": path })
            } else {
                installed(st, product_id)?;
                let rec = record(ExportKind::Desktop, product_id, &name(st, product_id), path, Utc::now(), false);
                d.exports.push(ExportStatus { record: rec.clone(), state: ExportState::Ok });
                json!(rec)
            }
        }
        Request::ExportSteam { product_id, remove, .. } => {
            let key = format!("quiver:{product_id}");
            d.exports.retain(|e| !(e.record.key == key && e.record.kind == ExportKind::Steam));
            if !remove {
                installed(st, product_id)?;
                let staged = d.steam_running;
                let rec = record(ExportKind::Steam, product_id, &name(st, product_id), steam_vdf(), Utc::now(), staged);
                let state = if staged { ExportState::WaitingForSteam } else { ExportState::Ok };
                d.exports.push(ExportStatus { record: rec, state });
            }
            json!(SteamExportResult {
                key: key.clone(),
                appid: appid(&key),
                steam_user: "84211337".into(),
                staged: d.steam_running,
                applied_staged: 0,
            })
        }
        Request::SteamRestart { confirmed } => {
            if !confirmed {
                bail!("restarting Steam needs the person's confirmation every time");
            }
            let was_running = d.steam_running;
            json!(RestartReport { was_running, applied: d.apply_staged(), started: was_running })
        }
        Request::SteamApplyStaged => {
            if d.steam_running {
                bail!("Steam is running; staged shortcuts are written when it closes");
            }
            json!({ "applied": d.apply_staged() })
        }
        Request::Play { game, profile } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let id = launch_profile(d, st, product_id, profile.as_deref())?;
            if !st.sessions.iter().any(|s| s.product_id == product_id) {
                d.running.insert(product_id, (id, Utc::now()));
            }
            return Ok(Err(Request::Play { game, profile }));
        }
        Request::Stop { game } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            // Record the session like the daemon does (at least 25 minutes, so it shows).
            if let Some((profile, started)) = d.running.remove(&product_id) {
                let ended = Utc::now();
                let seconds = ((ended - started).num_seconds().max(0) as u64).max(25 * 60);
                let rev = st.history.get(&product_id).and_then(|h| h.iter().map(|r| r.revision).max()).unwrap_or(0);
                let pt = st.playtime.entry(product_id).or_default();
                let profile_revision = profile.as_ref().map(|_| 1);
                pt.sessions.insert(
                    0,
                    quiver_api::PlaySession { started: ended - Span::seconds(seconds as i64), ended, seconds, settings_revision: rev, profile, profile_revision, mods: Vec::new(), ..Default::default() },
                );
                pt.total_seconds += seconds;
                pt.last_played = Some(ended);
            }
            return Ok(Err(Request::Stop { game }));
        }
        other => return Ok(Err(other)),
    };
    drop(all);
    for e in events {
        demo.emit(e);
    }
    Ok(Ok(out))
}

/// Demo: an in-game options change while AC Origins runs (as Sight would make one).
pub(super) fn options_in_game(demo: &Demo, st: &mut State) -> Result<Vec<Event>> {
    let key = demo as *const Demo as usize;
    let mut all = DETAILS.lock().unwrap();
    let d = all.entry(key).or_insert_with(|| Detail::new(&st.entries));
    let mut events = Vec::new();
    options::simulate_in_game(key, d, st, &mut events)?;
    Ok(events)
}

/// The text of a per-launch log in `<prefix>/quiver-run/` for the demo's games.
pub fn log_text(product_id: u32, file: &str) -> Option<String> {
    let exe = match product_id {
        3539 => "ACOrigins.exe",
        5059 => "ACOdyssey.exe",
        13504 => "ACValhalla.exe",
        _ => "game.exe",
    };
    let t0 = at(1, 19, 33);
    let ts = |s: i64| (t0 + Span::seconds(s)).with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string();
    let lines: Vec<String> = match file {
        "bridge.log" => vec![
            format!("{} INFO quiver-bridge 0.1.0 starting (product {product_id})", ts(0)),
            format!("{} INFO context: account {ACCOUNT}, ticket 1.9 KB, {} owned products, broker 127.0.0.1:47810", ts(0), 43),
            format!("{} INFO pipe \\\\.\\pipe\\terminal_1_uplay_aux_ipc_pipe_ listening", ts(0)),
            format!("{} INFO pipe \\\\.\\pipe\\terminal_1_uplay_api_process_ipc_pipe_ listening", ts(0)),
            format!("{} INFO pipe \\\\.\\pipe\\terminal_1_uplay_ipc_pipe_ listening", ts(0)),
            format!("{} INFO ready", ts(1)),
            format!("{} INFO aux: InitReq (uplay_aux_r164 1.0)", ts(6)),
            format!("{} INFO process: InitProcessReq pid 412 -> ok", ts(6)),
            format!("{} INFO api: InitReq uplay_id {product_id}, sdk 2023.1 -> ok (account {ACCOUNT})", ts(7)),
            format!("{} INFO aux: Denuvo token requested (request token 1024 B)", ts(9)),
            format!("{} INFO broker: ownership token ok, denuvo_service.GetGameToken ok (412 ms)", ts(9)),
            format!("{} INFO aux: Denuvo token delivered", ts(10)),
            format!("{} INFO api: UPLAY_USER_GetNameUtf8 -> {ACCOUNT}", ts(11)),
            format!("{} INFO api: UPLAY_OVERLAY_SetShopUrl ignored (no overlay)", ts(12)),
            format!("{} INFO api: UPLAY_SAVE_GetSavegames -> 5 slots", ts(14)),
            format!("{} WARN api: UPLAY_ACH_EarnAchievement 12 accepted locally (not reported yet)", ts(2410)),
            format!("{} INFO api: UPLAY_SAVE_Write slot 1 (4.2 MB)", ts(7880)),
            format!("{} INFO api: UPLAY_Quit", ts(8040)),
            format!("{} INFO session ended after 2 h 14 m; 3 saves to upload", ts(8041)),
        ],
        "game.log" => vec![
            format!("{} wine: using Proton GE-Proton10-25 (prefix ~/Games/quiver-prefixes/{product_id})", ts(2)),
            "fsync: up and running.".into(),
            "0024:fixme:ntdll:NtQuerySystemInformation info_class SYSTEM_PERFORMANCE_INFORMATION".into(),
            format!("info:  Game: {exe}"),
            "info:  DXVK: v2.7.1".into(),
            "info:  Vulkan: Found vkGetInstanceProcAddr in winevulkan.dll @ 0x6ffff7a4e0".into(),
            "info:  Built-in extension providers:".into(),
            "info:    Win32 WSI".into(),
            "info:    OpenVR".into(),
            "info:    OpenXR".into(),
            "info:  Enabled instance extensions:".into(),
            "info:    VK_KHR_get_surface_capabilities2".into(),
            "info:    VK_KHR_surface".into(),
            "info:    VK_KHR_win32_surface".into(),
            "info:  AMD Radeon RX 7900 XTX (RADV NAVI31):".into(),
            "info:    Driver : radv 25.2.4".into(),
            "info:    Memory Heap[0]: 24560 MiB".into(),
            "info:  D3D11CoreCreateDevice: Probing D3D_FEATURE_LEVEL_11_1".into(),
            "info:  D3D11CoreCreateDevice: Using feature level D3D_FEATURE_LEVEL_11_1".into(),
            "info:  Presenter: Actual swapchain properties:".into(),
            "info:    Format:       VK_FORMAT_A2B10G10R10_UNORM_PACK32".into(),
            "info:    Color space:  VK_COLOR_SPACE_HDR10_ST2084_EXT".into(),
            "info:    Present mode: VK_PRESENT_MODE_FIFO_KHR (dynamic: no)".into(),
            "info:    Buffer size:  3440x1440".into(),
            "info:    Image count:  3".into(),
            "0154:fixme:mfplat:MFCreateDXGIDeviceManager (0000000000E9F620, 0000000000E9F628) semi-stub".into(),
            "0154:fixme:xaudio2:XAudio2_GetDeviceDetails stub".into(),
            "warn:  DXGI: MakeWindowAssociation: Ignoring flags".into(),
            "0190:err:winediag:wined3d_dll_init Using the Vulkan renderer for d3d10/11 applications.".into(),
            "info:  Presenter: Recreating swapchain".into(),
            "0198:fixme:ntdll:NtSetInformationThread ThreadPowerThrottlingState stub".into(),
            "0024:fixme:ole:CoInitializeSecurity 0000000000000000, -1, 0000000000000000, 0000000000000000, 0, 3, 0000000000000000, 0, 0000000000000000 - stub!".into(),
            format!("{} game exited with status 0", ts(8041)),
        ],
        "bridge-proton.log" => vec![
            "ProtonFixes[1201] INFO: Running protonfixes".into(),
            "ProtonFixes[1201] INFO: Using global defaults for quiver-bridge.exe".into(),
            "fsync: up and running.".into(),
            "wine: RLIMIT_NICE is <= 20, unable to use setpriority safely".into(),
            "0024:fixme:hid:handle_IRP_MN_QUERY_ID Unhandled type 00000005".into(),
            "0024:fixme:wineusb:query_id Unhandled ID query type 0x5.".into(),
            "0030:fixme:kernelbase:AppPolicyGetProcessTerminationMethod FFFFFFFFFFFFFFFA, 000000000011FE50".into(),
        ],
        _ => return None,
    };
    Some(lines.join("\n") + "\n")
}

// ---------------------------------------------------------------------------------------
// Settings

const PROTONS: &[&str] = &["GE-Proton10-26", "GE-Proton10-25", "Proton 9.0 (Beta)"];

fn push_revision(st: &mut State, product_id: u32, settings: GameSettings, source: &str, message: Option<String>) -> Revision {
    let hist = st.history.entry(product_id).or_default();
    let revision = hist.iter().map(|r| r.revision).max().unwrap_or(0) + 1;
    let rev = Revision { revision, at: Utc::now(), source: source.into(), message: message.filter(|m| !m.trim().is_empty()), settings: settings.clone() };
    hist.insert(0, rev.clone()); // demo.rs keeps them newest first
    st.settings.insert(product_id, settings);
    rev
}

// ---------------------------------------------------------------------------------------
// Game info and progression

/// Products the demo account owns as Steam copies (no Ubisoft PC build).
const OWNED_ON_STEAM: &[u32] = &[1875, 66088];

/// A play session started from Steam through `quiver steam-run`.
pub(super) fn steam_session(product_id: u32, app_id: u32, started: DateTime<Utc>) -> quiver_api::ActiveSession {
    quiver_api::ActiveSession {
        product_id,
        account: "demo".into(),
        started,
        holders: vec!["steam".into()],
        reported: true,
        steam_app_id: Some(app_id),
        ..Default::default()
    }
}

/// The library's Steam installs, as quiverd joins them: each Steam game on its product's
/// entry, and an entry for Steam copies the library didn't have (Black Flag Resynced).
fn sync_steam(st: &mut State, games: &[SteamGame]) {
    for e in st.entries.iter_mut() {
        e.steam = None;
    }
    for g in games {
        let Some(id) = g.product_id else { continue };
        match st.entries.iter_mut().find(|e| e.ubisoft_id() == Some(id)) {
            Some(e) => e.steam = Some(g.clone()),
            None => {
                let at = st.entries.iter().take_while(|e| e.installed.is_some()).count();
                st.entries.insert(
                    at,
                    LibraryEntry {
                        game: quiver_api::GameRef::ubisoft(id),
                        name: g.name.clone(),
                        installed: None,
                        update_available: false,
                        running: false,
                        art: Default::default(),
                        owned_by: vec!["demo".into()],
                        installable: false,
                        steam: Some(g.clone()),
                        space_id: None,
                        client: None,
                    },
                );
            }
        }
    }
    for e in st.entries.iter_mut().filter(|e| e.ubisoft_id().is_some_and(|id| OWNED_ON_STEAM.contains(&id))) {
        e.installable = false;
    }
    // As quiverd does: a Steam copy with a session is running (Steam started it).
    let playing: Vec<u32> = st.sessions.iter().map(|s| s.product_id).collect();
    for e in st.entries.iter_mut().filter(|e| e.steam_only()) {
        e.running = e.ubisoft_id().is_some_and(|id| playing.contains(&id));
    }
}

fn entry(st: &State, id: u32) -> Result<&LibraryEntry> {
    st.entries.iter().find(|e| e.ubisoft_id() == Some(id)).ok_or_else(|| anyhow!("unknown product {id}"))
}

fn name(st: &State, id: u32) -> String {
    entry(st, id).map(|e| e.name.clone()).unwrap_or_else(|_| id.to_string())
}

fn installed(st: &State, id: u32) -> Result<()> {
    if entry(st, id)?.installed.is_none() {
        bail!("{} is not installed", name(st, id));
    }
    Ok(())
}

fn facts(id: u32) -> Option<&'static ArtFacts> {
    FACTS.iter().find(|f| f.product_id == id)
}

fn has_denuvo(id: u32) -> bool {
    matches!(id, 3539 | 5059 | 13504 | 6100 | 1875)
}

/// (name, kind, owned)
fn addon_names(id: u32) -> Vec<(&'static str, ProductKind, bool)> {
    use ProductKind::*;
    match id {
        3539 => vec![
            ("Season Pass", SeasonPass, true),
            ("The Hidden Ones", AddOn, true),
            ("The Curse of the Pharaohs", AddOn, true),
            ("Deluxe Pack", AddOn, true),
            ("Roman Centurion Pack", AddOn, false),
            ("Horus Pack", AddOn, false),
            ("Secrets of the First Pyramids Pack", AddOn, false),
            ("Ancient Egypt Discovery Tour Pack", AddOn, false),
            ("Helix Credits: 2,300", Consumable, false),
        ],
        5059 => vec![
            ("Season Pass", SeasonPass, true),
            ("Legacy of the First Blade", AddOn, true),
            ("The Fate of Atlantis", AddOn, true),
            ("Ultimate Edition Pack", AddOn, false),
            ("Odyssey Deluxe Pack", AddOn, true),
            ("Capt. Kassandra Pack", AddOn, false),
            ("Naxos Pack", AddOn, false),
        ],
        13504 => vec![
            ("Season Pass", SeasonPass, false),
            ("Wrath of the Druids", AddOn, false),
            ("The Siege of Paris", AddOn, false),
            ("Dawn of Ragnarök", AddOn, true),
        ],
        _ => {
            let n = facts(id).map(|f| f.dlc_count).unwrap_or(0).min(3);
            [("Season Pass", SeasonPass, false), ("Deluxe Pack", AddOn, true), ("Expansion", AddOn, false)]
                .into_iter()
                .take(n as usize)
                .collect()
        }
    }
}

fn store_url(name: &str) -> String {
    let q: String = name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://store.ubisoft.com/search?q={q}")
}

fn size_of(id: u32) -> u64 {
    match id {
        3539 => 61_800_000_000,
        5059 => 112_400_000_000,
        13504 => 131_000_000_000,
        720 => 52_100_000_000,
        1803 => 41_300_000_000,
        2688 => 49_700_000_000,
        1651 => 2_600_000_000,
        277 => 7_400_000_000,
        _ => 20 * GB,
    }
}

fn version_of(id: u32) -> Option<&'static str> {
    match id {
        3539 => Some("1.6.0"),
        5059 => Some("1.5.6"),
        13504 => Some("1.7.0"),
        720 => Some("1.5.0"),
        1803 => Some("1.020"),
        2688 => Some("1.17"),
        _ => None,
    }
}

fn game_info(st: &State, id: u32) -> Result<GameInfo> {
    let e = entry(st, id)?;
    let f = facts(id);
    let full = |n: &str| format!("{} - {n}", e.name);
    let addons = addon_names(id)
        .into_iter()
        .enumerate()
        .map(|(i, (n, kind, owned))| AddOn {
            product_id: id * 10 + i as u32 + 1,
            name: Some(n.to_owned()),
            kind,
            owned,
            visible: true,
            description: None,
            thumb: None,
            store_url: (!owned).then(|| store_url(&full(n))),
        })
        .collect();
    let install = e.installed.as_ref().map(|g| InstallInfo {
        dir: g.install_dir.clone(),
        version: version_of(id).map(str::to_owned),
        language: g.language.clone(),
        languages: ["en-US", "fr-FR", "de-DE", "es-ES", "it-IT", "ja-JP", "pt-BR", "ru-RU"].map(str::to_owned).to_vec(),
        size_bytes: size_of(id),
        manifest: Some(format!("{:08X}{:08X}E4A1C0FFEE{:06X}", id.wrapping_mul(2_654_435_761), id ^ 0x3b9c, id)),
    });
    let ac = f.and_then(|f| f.sort_string.as_deref()).is_some_and(|s| s.starts_with("Assassin's Creed"));
    let mut links = vec![Link { kind: "homepage".into(), url: if ac { "http://assassinscreed.ubi.com/".into() } else { "https://www.ubisoft.com/".into() } }];
    links.push(Link { kind: "support".into(), url: format!("https://support.ubisoft.com/games/{id}?platform=9") });
    links.push(Link { kind: "forum".into(), url: "https://discord.gg/ubisoftofficial".into() });
    Ok(GameInfo {
        product_id: id,
        name: e.name.clone(),
        account: Some(ACCOUNT.into()),
        owned_by: e.owned_by.clone(),
        space_id: Some(format!("{:08x}-6af6-4029-873b-ba7e036464fa", id.wrapping_mul(97_531))),
        app_id: Some(format!("{:08x}-4df3-44a9-9a3c-2b1c32f86a1e", id.wrapping_mul(13_579))),
        sort_string: f.and_then(|f| f.sort_string.clone()),
        publisher: Some("Ubisoft".into()),
        developer: None,
        release_date: None,
        description: None,
        genres: vec![],
        links,
        features: Features {
            cloud_saves: f.is_some_and(|f| f.cloud_saves),
            achievements: f.is_some_and(|f| f.achievements),
            challenges: ac,
            denuvo: has_denuvo(id),
            player_stats: if id == 3539 {
                ["TimePlayed", "PlayerLevel", "TotalProgression", "TotalKills"].map(str::to_owned).to_vec()
            } else {
                vec![]
            },
            game_options: (id == 3539).then(|| "Animus Control Panel".into()),
            change_language: true,
        },
        install,
        addons,
        unavailable: vec!["developer".into(), "release_date".into(), "description".into()],
    })
}

/// Ubisoft's play-time record: a little under the local total, like the mock-ups.
fn remote_seconds(st: &State, id: u32) -> u64 {
    let local = st.playtime.get(&id).map(|p| p.total_seconds).unwrap_or(0);
    if local == 0 { 0 } else { local.saturating_sub(local / 70 + 3_600) }
}

fn stat(id: &str, name: &str, value: &str, number: Option<f64>, kind: &str) -> Stat {
    Stat { id: id.into(), name: Some(name.into()), value: Some(value.into()), number, kind: Some(kind.into()), image: None }
}

fn achievements(id: u32) -> Option<Achievements> {
    if id != 3539 {
        return None;
    }
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("quiver/achievements")
        .join(ORIGINS_ACH_SET);
    let items: Vec<Achievement> = ORIGINS_ACH
        .lines()
        .filter_map(|l| {
            let mut f = l.trim_start_matches('\u{feff}').trim_end_matches('\r').splitn(3, '\t');
            let id = f.next()?.trim();
            if id.is_empty() {
                return None;
            }
            let icon = id.parse::<u32>().ok().map(|n| dir.join(format!("{n}.png"))).filter(|p| p.is_file());
            Some(Achievement {
                id: id.to_owned(),
                name: f.next().map(|s| s.trim().to_owned()),
                description: f.next().map(|s| s.trim().to_owned()),
                image: icon.map(|p| p.to_string_lossy().into_owned()),
                // Demo: 59 "Namaste" earned (as on the real account).
                unlocked: id == "59",
                unlocked_at: (id == "59").then(|| "2026-11-17T01:00:00Z".parse().unwrap()),
                report: None,
            })
        })
        .collect();
    let unlocked = items.iter().filter(|a| a.unlocked).count() as u32;
    Some(Achievements { unlocked_known: true, unlocked, total: items.len() as u32, items, source: Some("ach_frontend".into()) })
}

fn progress(st: &State, id: u32) -> Result<Progress> {
    entry(st, id)?;
    let secs = remote_seconds(st, id);
    let f = facts(id);
    let mut p = Progress {
        product_id: id,
        account: Some(ACCOUNT.into()),
        space_id: Some(format!("{:08x}-6af6-4029-873b-ba7e036464fa", id.wrapping_mul(97_531))),
        fetched: Utc::now() - Span::minutes(2),
        playtime_seconds: (secs > 0).then_some(secs),
        playtime_source: (secs > 0).then(|| "playtime_service".into()),
        connect: Some(ConnectProfile { level: Some(84), xp: Some(41_975), level_start_xp: Some(41_500), next_level_xp: Some(42_250), units: Some(230) }),
        ..Default::default()
    };
    if id == 3539 {
        p.stats = vec![
            stat("TimePlayed", "Playtime", &format!("{} h", secs / 3600), Some(secs as f64), "LongTimespan"),
            stat("PlayerLevel", "Player Level", "39", Some(39.), "Integer"),
            stat("TotalProgression", "Game Progression (%)", "59%", Some(59.), "Percent"),
            stat("AbilitiesAcquired", "Abilities Unlocked (%)", "61%", Some(61.), "Percent"),
            stat("ReachHighPointsSynched", "Viewpoints Synched (%)", "77%", Some(77.), "Percent"),
            stat("TotalKills", "Kills", "2,635", Some(2635.), "Integer"),
            stat("TotalAssassinations", "Assassinations", "331", Some(331.), "Integer"),
            stat("LegendaryWeaponsCollected", "Legendary Weapons Collected", "8", Some(8.), "Integer"),
            stat("DistanceTravelled", "Distance Travelled", "420 km", Some(420_194.), "Integer"),
        ];
        let reward = |id: &str, name: &str, rarity: &str, cost: u64, status: &str, days: Option<i64>| Reward {
            id: id.into(),
            name: Some(name.into()),
            description: None,
            image: None,
            owned: status == "LimitReached",
            redeemed_at: days.map(|d| Utc::now() - Span::days(d)),
            cost: Some(cost),
            rarity: Some(rarity.into()),
            status: Some(status.into()),
        };
        p.rewards = vec![
            reward("f835074f", "Assassin's Creed Origins fan kit", "Common", 0, "Unlocked", None),
            reward("27b98c9d", "Aguilar's Outfit", "Legendary", 40, "LimitReached", Some(1767)),
            reward("4c1e2a90", "Corrupted Soul", "Legendary", 40, "LimitReached", Some(1767)),
            reward("9a0d71e3", "Altaïr's Outfit", "Exotic", 0, "Locked", None),
        ];
        p.achievements = achievements(id);
        p.errors = vec!["challenges: HTTP 404".into()];
    } else if f.is_some_and(|f| f.achievements) {
        p.errors = vec!["achievements: set not cached in demo mode".into()];
    }
    Ok(p)
}

// ---------------------------------------------------------------------------------------
// Saves

impl Detail {
    fn new(entries: &[LibraryEntry]) -> Self {
        let mut d = Detail {
            mods: BTreeMap::new(),
            backups: BTreeMap::new(),
            slots: BTreeMap::new(),
            exports: vec![],
            steam_running: crate::ui::global::demo_flag("steam-closed").is_none(),
            steam_games: steam_games(),
            steam_offers: crate::ui::global::demo_flag("steam-offers-off").is_none(),
            next: 1,
            running: HashMap::new(),
        };
        let name = |id: u32| entries.iter().find(|e| e.ubisoft_id() == Some(id)).map(|e| e.name.clone()).unwrap_or_default();
        // Exports: Origins is in Steam and the app launcher; Odyssey waits for a Steam restart.
        d.exports.push(ExportStatus { record: record(ExportKind::Steam, 3539, &name(3539), steam_vdf(), at(12, 21, 4), false), state: ExportState::Ok });
        d.exports.push(ExportStatus {
            record: record(ExportKind::Desktop, 3539, &name(3539), home().join(".local/share/applications/quiver-3539.desktop"), at(12, 21, 4), false),
            state: ExportState::Ok,
        });
        d.exports.push(ExportStatus { record: record(ExportKind::Steam, 5059, &name(5059), steam_vdf(), at(0, 9, 12), true), state: ExportState::WaitingForSteam });
        d.exports.push(ExportStatus {
            record: record(ExportKind::Desktop, 720, &name(720), home().join(".local/share/applications/quiver-720.desktop"), at(80, 18, 0), false),
            state: ExportState::Missing,
        });
        d.mods.insert(5059, odyssey_mods());
        d.mods.insert(3539, origins_mods());
        d
    }

    fn save_dir(st: &State, id: u32) -> Option<PathBuf> {
        let g = entry(st, id).ok()?.installed.as_ref()?;
        Some(g.prefix.join(format!("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/savegames/{ACCOUNT}/{id}")))
    }

    fn initial_slots(st: &State, id: u32) -> Vec<SaveSlot> {
        let last = st.playtime.get(&id).and_then(|p| p.last_played).unwrap_or_else(|| at(30, 20, 0));
        let cloud = facts(id).is_some_and(|f| f.cloud_saves);
        let mb = |x: f64| (x * 1_048_576.) as u64;
        let slot = |n: u32, size: u64, local: Option<DateTime<Utc>>, cloud_t: Option<DateTime<Utc>>, state: SlotState, pending: bool| SaveSlot {
            slot: n,
            local: local.map(|t| SlotFile { size, modified: Some(t) }),
            cloud: cloud_t.map(|t| CloudSlot { size: Some(size), stored_size: Some(size * 2 / 5), modified: Some(t) }),
            pending_upload: pending,
            state,
        };
        if id == 3539 {
            let t1 = last - Span::minutes(2);
            return vec![
                slot(1, mb(4.2), Some(t1), Some(t1 + Span::minutes(1)), SlotState::InSync, false),
                slot(2, mb(4.2), Some(t1 - Span::minutes(48)), Some(t1 + Span::minutes(1)), SlotState::InSync, false),
                slot(3, mb(4.1), Some(at(3, 21, 6)), Some(at(3, 21, 7)), SlotState::InSync, false),
                slot(4, mb(4.0), None, Some(at(17, 18, 3)), SlotState::CloudOnly, false),
                slot(5, mb(3.9), Some(at(5, 19, 34)), None, SlotState::LocalOnly, true),
                slot(6, mb(3.9), Some(at(9, 22, 40)), Some(at(2, 23, 15)), SlotState::CloudNewer, false),
            ];
        }
        let state = if cloud { SlotState::InSync } else { SlotState::LocalOnly };
        let c = cloud.then_some(last);
        vec![slot(1, mb(1.6), Some(last), c, state, false), slot(2, mb(1.5), Some(last - Span::days(4)), c.map(|t| t - Span::days(4)), state, false)]
    }

    fn initial_backups(st: &State, id: u32) -> Vec<SaveBackup> {
        let root = home().join(".local/share/quiver/save-backups");
        let b = |t: DateTime<Utc>, reason: &str, slots: Vec<(u32, u64)>| {
            let bid = format!("{id}-{}", t.format("%Y%m%d-%H%M%S"));
            SaveBackup { path: root.join(&bid), id: bid, created: Some(t), slots, reason: Some(reason.into()), account: Some(ACCOUNT.into()), name: None }
        };
        let s5 = vec![(1, 4_404_019), (2, 4_404_019), (3, 4_299_161), (5, 4_089_446), (6, 4_089_446)];
        if id == 3539 {
            vec![
                b(at(1, 19, 33), "before cloud download", s5.clone()),
                b(at(3, 20, 2), "before cloud download", s5.clone()),
                b(at(5, 19, 31), "before cloud download", s5.clone()),
                b(at(7, 18, 10), "before restoring 3539-20261101-201002", s5[..4].to_vec()),
                b(at(15, 20, 10), "before cloud download", s5[..4].to_vec()),
            ]
        } else if st.playtime.get(&id).is_some_and(|p| !p.sessions.is_empty()) {
            vec![b(at(20, 20, 15), "before cloud download", vec![(1, 1_677_721), (2, 1_572_864)])]
        } else {
            vec![]
        }
    }

    fn saves(&mut self, st: &State, id: u32, cloud: bool) -> Result<SavesReport> {
        if !entry(st, id)?.steam_only() {
            installed(st, id)?;
        }
        let slots = self.slots.entry(id).or_insert_with(|| Self::initial_slots(st, id)).clone();
        let backups = self.backups.entry(id).or_insert_with(|| Self::initial_backups(st, id)).clone();
        let cloud_on = facts(id).is_some_and(|f| f.cloud_saves);
        let slots = if cloud && cloud_on {
            slots
        } else {
            slots.into_iter().filter(|s| s.local.is_some()).map(|s| SaveSlot { cloud: None, state: SlotState::Unknown, ..s }).collect()
        };
        let record = (cloud && cloud_on).then(|| CloudRecord {
            machine: Some("DESKTOP-ROG".into()),
            modified: Some(at(2, 23, 15)),
            slots: slots
                .iter()
                .filter_map(|s| s.cloud.as_ref().map(|c| RecordSlot { slot: s.slot, size: c.size, modified: c.modified }))
                .collect(),
        });
        Ok(SavesReport {
            product_id: id,
            account: Some(ACCOUNT.into()),
            dir: Self::save_dir(st, id),
            slots,
            cloud_source: (cloud && cloud_on).then(|| "cloudsave".into()),
            cloud_error: None,
            connect_record: record,
            backups,
            store: Some("Default".into()),
        })
    }

    fn restore(&mut self, st: &State, id: u32, backup: &str) -> Result<RestoreResult> {
        installed(st, id)?;
        let list = self.backups.entry(id).or_insert_with(|| Self::initial_backups(st, id));
        let chosen = if backup == "latest" {
            list.first().cloned()
        } else {
            let hits: Vec<&SaveBackup> = list.iter().filter(|b| b.id.contains(backup)).collect();
            match hits[..] {
                [b] => Some(b.clone()),
                [] => None,
                _ => bail!("'{backup}' matches several backups"),
            }
        }
        .ok_or_else(|| anyhow!("no backup '{backup}'"))?;
        let now = Utc::now();
        let slots = self.slots.entry(id).or_insert_with(|| Self::initial_slots(st, id));
        let previous_slots: Vec<(u32, u64)> = slots.iter().filter_map(|s| s.local.as_ref().map(|l| (s.slot, l.size))).collect();
        let prev_id = format!("{id}-{}", now.format("%Y%m%d-%H%M%S"));
        let previous = SaveBackup {
            path: home().join(".local/share/quiver/save-backups").join(&prev_id),
            id: prev_id,
            created: Some(now),
            slots: previous_slots,
            reason: Some(format!("before restoring {}", chosen.id)),
            account: Some(ACCOUNT.into()),
            name: None,
        };
        for (n, size) in &chosen.slots {
            match slots.iter_mut().find(|s| s.slot == *n) {
                Some(s) => {
                    s.local = Some(SlotFile { size: *size, modified: Some(now) });
                    s.state = if s.cloud.is_some() { SlotState::LocalNewer } else { SlotState::LocalOnly };
                    s.pending_upload = true;
                }
                None => slots.push(SaveSlot {
                    slot: *n,
                    local: Some(SlotFile { size: *size, modified: Some(now) }),
                    cloud: None,
                    pending_upload: true,
                    state: SlotState::LocalOnly,
                }),
            }
        }
        slots.sort_by_key(|s| s.slot);
        list.insert(0, previous.clone());
        Ok(RestoreResult { restored: chosen.slots.iter().map(|(n, _)| *n).collect(), previous: Some(previous) })
    }

    // -----------------------------------------------------------------------------------
    // Exports

    fn apply_staged(&mut self) -> usize {
        let mut n = 0;
        for e in &mut self.exports {
            if e.state == ExportState::WaitingForSteam {
                e.state = ExportState::Ok;
                e.record.staged = false;
                n += 1;
            }
        }
        for g in &mut self.steam_games {
            let next = match g.state {
                PatchState::Waiting => PatchState::On,
                PatchState::WaitingUndo => PatchState::Off,
                _ => continue,
            };
            g.launch_options = if next == PatchState::On { demo_patched(&g.launch_options, g.product_id) } else { demo_unpatched(&g.launch_options) };
            g.state = next;
            n += 1;
        }
        n
    }
}

fn steam_command() -> String {
    format!("{} --source steam steam-run", home().join(".local/bin/quiver").display())
}

/// Like quiverd: the product id is always written.
fn demo_patched(opts: &str, product: Option<u32>) -> String {
    if opts.contains("steam-run") {
        return opts.to_owned();
    }
    let cmd = match product {
        Some(p) => format!("{} --product {p}", steam_command()),
        None => steam_command(),
    };
    match opts.find("%command%") {
        Some(i) => format!("{}{cmd} {}", &opts[..i], &opts[i..]),
        None if opts.trim().is_empty() => format!("{cmd} %command%"),
        None => format!("{cmd} %command% {}", opts.trim()),
    }
}

fn demo_unpatched(opts: &str) -> String {
    let Some(s) = opts.find(&steam_command()) else { return opts.to_owned() };
    let e = opts[s..].find("%command%").map_or(opts.len(), |i| s + i);
    let out = format!("{}{}", &opts[..s], &opts[e..]);
    if out.trim() == "%command%" { String::new() } else { out }
}

/// Steam's library-cache art for the demo's Black Flag Resynced (its product config has no
/// art, as the real one's doesn't, so the app falls back to Steam's): the files in
/// `$QUIVER_DEMO_STEAM_ART` (`library_capsule.jpg`, `library_hero.jpg`, `logo.png`,
/// `header.jpg`, `icon.jpg`; e.g. copies of a real Steam cache), else placeholder images
/// drawn once into the art cache.
fn demo_steam_art() -> quiver_api::art::SteamArt {
    let dir = match std::env::var_os("QUIVER_DEMO_STEAM_ART") {
        Some(d) => PathBuf::from(d),
        None => {
            let d = crate::art::default_cache_dir().join("demo-steam/3751950");
            // Tests don't draw into the real cache.
            if !cfg!(test)
                && let Err(e) = draw_steam_fixtures(&d)
            {
                tracing::warn!("demo Steam art: {e:#}");
            }
            d
        }
    };
    let f = |n: &str| Some(dir.join(n)).filter(|p| p.is_file());
    quiver_api::art::SteamArt {
        capsule: f("library_capsule.jpg"),
        hero: f("library_hero.jpg"),
        logo: f("logo.png"),
        header: f("header.jpg"),
        icon: f("icon.jpg"),
    }
}

/// Placeholder Steam art: a portrait capsule and a hero (sea-and-sunset gradients) and a
/// transparent "logo" (two bars), so the fallback is visible without Steam.
fn draw_steam_fixtures(dir: &std::path::Path) -> anyhow::Result<()> {
    if dir.join("logo.png").is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let grad = |w: u32, h: u32| {
        image::RgbImage::from_fn(w, h, |x, y| {
            let t = y as f32 / h as f32;
            let sun = (1.0 - ((x as f32 / w as f32 - 0.7).powi(2) + (t - 0.35).powi(2)).sqrt() * 3.0).max(0.0);
            let sea = if t > 0.6 { 1.0 } else { 0.0 };
            let r = (30.0 + 200.0 * sun + 20.0 * (1.0 - t)) * (1.0 - 0.6 * sea);
            let g = (40.0 + 120.0 * sun + 60.0 * t) * (1.0 - 0.3 * sea);
            let b = 70.0 + 90.0 * t + 40.0 * sea;
            image::Rgb([r.min(255.0) as u8, g.min(255.0) as u8, b.min(255.0) as u8])
        })
    };
    grad(300, 450).save(dir.join("library_capsule.jpg"))?;
    grad(1920, 620).save(dir.join("library_hero.jpg"))?;
    grad(460, 215).save(dir.join("header.jpg"))?;
    let logo = image::RgbaImage::from_fn(640, 360, |x, y| {
        let bar = (y > 120 && y < 190 && x > 40 && x < 600) || (y > 210 && y < 240 && x > 160 && x < 480);
        if bar { image::Rgba([236, 226, 205, 255]) } else { image::Rgba([0, 0, 0, 0]) }
    });
    let tmp = dir.join(".logo.png.part");
    logo.save_with_format(&tmp, image::ImageFormat::Png)?;
    std::fs::rename(&tmp, dir.join("logo.png"))?;
    Ok(())
}

/// Ubisoft games installed through Steam (`QUIVER_DEMO_GLOBAL`: `steam-games=none` for
/// none, `steam-offer` for a new install being offered, `steam-closed` for Steam closed).
fn steam_games() -> Vec<SteamGame> {
    if crate::ui::global::demo_flag("steam-games").is_some_and(|v| v == "none") {
        return vec![];
    }
    let running = crate::ui::global::demo_flag("steam-closed").is_none();
    let g = |app_id: u32, name: &str, dir: &str, product: Option<u32>, opts: &str, state: PatchState| SteamGame {
        app_id,
        name: name.into(),
        install_dir: home().join(".local/share/Steam/steamapps/common").join(dir),
        product_id: product,
        product_from: product.map(|_| "install_script".into()),
        owned: product.map(|_| true),
        steam_user: Some("84211337".into()),
        launch_options: opts.into(),
        state,
        patchable: product.is_some(),
        offer: false,
        note: product.is_none().then(|| "Quiver can't tell which Ubisoft game this is".into()),
        art: if app_id == 3751950 { demo_steam_art() } else { Default::default() },
    };
    let mut v = vec![
        g(3751950, "Assassin's Creed Black Flag Resynced", "Assassin's Creed Black Flag Resynced", Some(66088), &demo_patched("", Some(66088)), PatchState::On),
        g(368500, "Assassin's Creed Syndicate", "Assassin's Creed Syndicate", Some(1875), "", PatchState::Off),
        g(298110, "Far Cry 4", "Far Cry 4", Some(420), "-skipintro", PatchState::Off),
        g(2239550, "Watch Dogs: Legion", "Watch Dogs Legion", Some(3353), "PROTON_ENABLE_NVAPI=1 %command%", if running { PatchState::Waiting } else { PatchState::On }),
        g(916440, "Anno 1800", "Anno 1800", None, "", PatchState::Off),
    ];
    if !running {
        v[3].launch_options = demo_patched(&v[3].launch_options, v[3].product_id);
    }
    if crate::ui::global::demo_flag("steam-offer").is_some() {
        v[1].offer = true;
    }
    v
}

fn steam_vdf() -> PathBuf {
    home().join(".local/share/Steam/userdata/84211337/config/shortcuts.vdf")
}

fn appid(key: &str) -> u32 {
    // crc32 isn't worth a dependency here; any stable value with the top bit set will do.
    let mut h: u32 = 0x811c_9dc5;
    for b in key.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h | 0x8000_0000
}

fn record(kind: ExportKind, id: u32, name: &str, path: PathBuf, at: DateTime<Utc>, staged: bool) -> ExportRecord {
    let key = format!("quiver:{id}");
    ExportRecord {
        appid: (kind == ExportKind::Steam).then(|| appid(&key)),
        steam_user: (kind == ExportKind::Steam).then(|| "84211337".into()),
        key,
        kind,
        game: quiver_api::GameRef::ubisoft(id),
        profile_id: None,
        name: name.to_owned(),
        path,
        written_at: at,
        hash: format!("{:016x}", (id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)),
        staged,
    }
}

// ---------------------------------------------------------------------------------------
// Mods

fn layer(id: &str, name: &str, days: i64, by: &str, origin: &str, files: u64, bytes: u64) -> Layer {
    Layer {
        id: id.into(),
        name: name.into(),
        added: at(days, 21, 12),
        added_by: by.into(),
        origin: Some(home().join(origin)),
        files,
        bytes,
        deletes: Vec::new(),
        ..Default::default()
    }
}

/// A layer with a manifest saying whether saves made with it work without it.
fn with_saves(mut l: Layer, save_compatible: bool) -> Layer {
    l.manifest = Some(ModManifest { id: l.id.clone(), name: l.name.clone(), save_compatible: Some(save_compatible), ..Default::default() });
    l
}

fn rev(revision: u32, days: i64, source: &str, message: Option<&str>, profile: Profile) -> ProfileRevision {
    ProfileRevision { revision, at: at(days, 20, 30), source: source.into(), message: message.map(str::to_owned), deleted: false, profile }
}

fn odyssey_mods() -> Mods {
    let layers = vec![
        with_saves(layer("l4n8d2k", "ReShade 6.3.3 + Aegean Light", 14, "gui", "Downloads/reshade-odyssey", 41, 84_000_000), true),
        with_saves(layer("l9q2w7e", "HD texture pack (community) v2.1", 14, "gui", "Downloads/odyssey-hd-2.1", 1_204, 12_800_000_000), true),
        with_saves(layer("l3x5v1b", "Better water and weather", 9, "cli", "mods/odyssey/better-water", 17, 640_000_000), true),
        layer("l8m1z6t", "Skip intro videos", 2, "claude-code", "mods/odyssey/skip-intro", 6, 2_048),
    ];
    let mut visual = Profile {
        id: "p7f3k2q".into(),
        name: "Visual overhaul".into(),
        layers: vec!["l3x5v1b".into(), "l9q2w7e".into()],
        saves: Some(DEFAULT_STORE.into()),
        ..Default::default()
    };
    let mut h1 = vec![rev(1, 14, "gui", Some("HD textures and better water"), visual.clone())];
    visual.layers.push("l4n8d2k".into());
    visual.settings.dll_overrides.insert("dxgi".into(), "n,b".into());
    h1.push(rev(2, 13, "gui", Some("Add ReShade"), visual.clone()));
    visual.pinned = true;
    h1.push(rev(3, 12, "gui", None, visual.clone()));
    let mut photo = Profile {
        id: "pq2m9xa".into(),
        name: "Photo mode".into(),
        layers: vec!["l4n8d2k".into()],
        saves: Some(DEFAULT_STORE.into()),
        ..Default::default()
    };
    photo.settings.dll_overrides.insert("dxgi".into(), "n,b".into());
    let h2 = vec![rev(1, 2, "claude-code", Some("Photo mode: ReShade without the texture pack"), photo.clone())];
    let mut old = Profile { id: "pk0w3rd".into(), name: "Hardcore loot".into(), layers: vec!["l3x5v1b".into()], ..Default::default() };
    let mut h3 = vec![rev(1, 30, "cli", None, old.clone())];
    old.layers.clear();
    let mut del = rev(2, 21, "cli", None, old);
    del.deleted = true;
    h3.push(del);
    Mods { layers, history: vec![h1, h2, h3], ..Default::default() }
}

/// Origins: a DLSS mod with three versions (a DLSS profile tracking the current one and
/// sharing Default with vanilla, a benchmark profile pinned to an older one), and a hardcore
/// run (a gameplay mod) with a store of its own.
fn origins_mods() -> Mods {
    let layers = vec![
        with_saves(layer("lr2s8hd", "ReShade 6.3 + Desert Sun", 20, "gui", "Downloads/reshade-origins", 39, 81_000_000), true),
        with_saves(layer("lb5k1yq", "Skip intro videos", 20, "cli", "mods/origins/skip-intro", 4, 1_536), true),
        with_saves(layer("ld7s3vq", "DLSS + DLSSTweaks", 18, "gui", "Downloads/dlss-origins", 3, 48_000_000), true),
        with_saves(layer("lh4c9wz", "Nightmare difficulty rebalance", 4, "gui", "Downloads/aco-nightmare", 12, 3_200_000), false),
        with_saves(layer("vq4n2c8", "DLSS + DLSSTweaks", 9, "aco-dlss", "Downloads/dlss-origins-3.7", 4, 52_000_000), true),
        with_saves(layer("vt8w1hz", "DLSS + DLSSTweaks", 2, "aco-dlss", "Downloads/dlss-origins-3.8", 4, 77_900_000), true),
    ];
    let version = |l: &Layer, label: &str, notes: Option<&str>| ModVersion {
        id: l.id.clone(),
        label: label.into(),
        added: l.added,
        added_by: l.added_by.clone(),
        notes: notes.map(str::to_owned),
        ..Default::default()
    };
    let dlss_record = ModRecord {
        id: "ld7s3vq".into(),
        name: "DLSS + DLSSTweaks".into(),
        current: "vt8w1hz".into(),
        versions: vec![
            version(&layers[2], "3.5.10", Some("First build: DLSS 3.5 through DLSSTweaks")),
            version(&layers[4], "3.7.10", Some("DLSS 3.7, preset E")),
            version(&layers[5], "3.8.10", Some("DLSS 3.8 with frame generation DLLs")),
        ],
        disabled: false,
    };
    let mut p = Profile { id: "pz6c4uf".into(), name: "ReShade".into(), layers: vec!["lb5k1yq".into(), "lr2s8hd".into()], saves: Some(DEFAULT_STORE.into()), ..Default::default() };
    p.settings.dll_overrides.insert("dxgi".into(), "n,b".into());
    let mut dlss = Profile {
        id: ORIGINS_DEFAULT.into(),
        name: "DLSS".into(),
        layers: vec!["lb5k1yq".into(), "ld7s3vq".into()],
        saves: Some(DEFAULT_STORE.into()),
        ..Default::default()
    };
    dlss.settings.dll_overrides.insert("d3d11".into(), "n,b".into());
    let mut bench = Profile {
        id: "pb3x7tm".into(),
        name: "Benchmark (DLSS 3.7)".into(),
        layers: vec!["ld7s3vq@vq4n2c8".into()],
        saves: Some(DEFAULT_STORE.into()),
        ..Default::default()
    };
    bench.settings.dll_overrides.insert("d3d11".into(), "n,b".into());
    let hard = Profile { id: "pn2h8re".into(), name: "Nightmare".into(), layers: vec!["lh4c9wz".into()], saves: Some("s5kq2mt".into()), ..Default::default() };
    let stores = vec![
        default_store(3539),
        SaveStore {
            id: "s5kq2mt".into(),
            name: "Hardcore run".into(),
            folder: "3539-s5kq2mt".into(),
            created: Some(at(4, 21, 30)),
            created_by: Some("gui".into()),
            note: Some("made for profile 'Nightmare'".into()),
            ..Default::default()
        },
    ];
    Mods {
        layers,
        history: vec![
            vec![rev(1, 20, "gui", Some("ReShade with the Desert Sun preset"), p)],
            vec![rev(1, 6, "gui", Some("DLSS through DLSSTweaks, sharing vanilla's saves"), dlss)],
            vec![rev(1, 3, "gui", Some("Frame times against DLSS 3.7, kept on that build"), bench)],
            vec![rev(1, 4, "gui", Some("Nightmare difficulty: its own saves"), hard)],
        ],
        stores,
        vanilla_store: None,
        records: vec![dlss_record],
    }
}

fn default_store(id: u32) -> SaveStore {
    SaveStore { id: DEFAULT_STORE.into(), name: "Default".into(), folder: id.to_string(), cloud: true, ..Default::default() }
}

/// Origins' DLSS profile, the game's default profile in demo mode (demo.rs).
pub(super) const ORIGINS_DEFAULT: &str = "pd1s5xk";

/// The profile a demo launch runs (the daemon's rule): the one named (`vanilla`: none),
/// else the game's default if it's still a live profile.
fn launch_profile(d: &Detail, st: &State, product_id: u32, requested: Option<&str>) -> Result<Option<String>> {
    let mods = d.mods.get(&product_id);
    let live = |q: &str| mods.and_then(|m| m.live().find(|r| r.profile.id == q || r.profile.name.eq_ignore_ascii_case(q))).map(|r| r.profile.id.clone());
    Ok(match requested {
        Some(p) if p.eq_ignore_ascii_case(VANILLA) => None,
        Some(p) => Some(live(p).ok_or_else(|| anyhow!("no mod profile '{p}' for {}", name(st, product_id)))?),
        None => st.settings.get(&product_id).and_then(|s| s.profile.as_deref()).and_then(live),
    })
}

impl Mods {
    fn live(&self) -> impl Iterator<Item = &ProfileRevision> {
        self.history.iter().filter_map(|h| h.last()).filter(|r| !r.deleted)
    }

    fn find(&self, q: &str, deleted_too: bool) -> Result<usize> {
        let hits: Vec<usize> = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, h)| {
                h.last().is_some_and(|r| (deleted_too || !r.deleted) && (r.profile.id == q || r.profile.name.eq_ignore_ascii_case(q)))
            })
            .map(|(i, _)| i)
            .collect();
        match hits[..] {
            [i] => Ok(i),
            [] => bail!("no mod profile '{q}'"),
            _ => bail!("'{q}' matches several profiles; use the id"),
        }
    }

    fn all_mods(&self) -> Vec<ModRecord> {
        mods_of(&self.layers, &self.records)
    }

    /// A stack entry as given (a mod's id or name, `MOD@VERSION`) to the stored entry.
    fn resolve_layer(&self, q: &str) -> Result<String> {
        let (m, v) = split_entry(q);
        let all = self.all_mods();
        let rec = all
            .iter()
            .find(|r| r.id == m || r.name.eq_ignore_ascii_case(m))
            .or_else(|| all.iter().find(|r| r.has(m)))
            .ok_or_else(|| anyhow!("no mod '{m}'"))?;
        let pinned = match v {
            Some(v) => Some(rec.version(v).ok_or_else(|| anyhow!("'{}' has no version '{v}'", rec.name))?.id.clone()),
            None if rec.id != m && rec.has(m) => Some(m.to_owned()),
            None => None,
        };
        Ok(stack_entry(&rec.id, pinned.as_deref()))
    }

    /// The versions a stack runs (named after their mods), as the daemon resolves them.
    fn stack_layers(&self, entries: &[String]) -> Vec<Layer> {
        ModsState { layers: self.layers.clone(), mods: self.records.clone(), ..Default::default() }.stack_layers(entries)
    }

    /// A version change on a mod (demo: make current, retire, bring back).
    fn version_op(&mut self, mod_q: &str, version: &str, op: &str) -> Result<VersionChange> {
        let mut rec = self.all_mods().into_iter().find(|r| r.id == mod_q || r.name.eq_ignore_ascii_case(mod_q)).ok_or_else(|| anyhow!("no mod '{mod_q}'"))?;
        let v = rec.version(version).ok_or_else(|| anyhow!("'{}' has no version '{version}'", rec.name))?.clone();
        match op {
            "use" if v.retired => bail!("{} {} is retired; bring it back first", rec.name, v.label),
            "use" => rec.current = v.id.clone(),
            "retire" if v.id == rec.current => bail!("{} {} is the current version; make another one current first", rec.name, v.label),
            _ => {}
        }
        for x in rec.versions.iter_mut().filter(|x| x.id == v.id) {
            match op {
                "retire" => x.retired = true,
                "unretire" => x.retired = false,
                _ => x.prerelease = false,
            }
        }
        self.records.retain(|r| r.id != rec.id);
        self.records.push(rec.clone());
        let all = self.all_mods();
        let (mut tracking, mut pinned) = (Vec::new(), Vec::new());
        for r in self.live() {
            if let Some(x) = r.profile.layers.iter().filter_map(|e| resolve_entry(&all, e)).find(|x| x.mod_id == rec.id) {
                if x.pinned { pinned.push(format!("{} ({})", r.profile.name, x.label)) } else { tracking.push(r.profile.name.clone()) }
            }
        }
        let version = self.layers.iter().find(|l| l.id == v.id).cloned().unwrap_or_default();
        Ok(VersionChange { record: rec, version, revision: 1, tracking, pinned, warnings: Vec::new() })
    }

    fn info(&self, id: u32, r: &ProfileRevision) -> ProfileInfo {
        let all = self.all_mods();
        let missing = r.profile.layers.iter().filter(|e| resolve_entry(&all, e).is_none()).cloned().collect();
        let resolved = r.profile.layers.iter().filter_map(|e| resolve_entry(&all, e)).collect();
        // Demo conflicts: the texture pack and the water mod both ship ocean normals.
        let mut conflicts = vec![];
        if r.profile.layers.iter().any(|l| l == "l9q2w7e") && r.profile.layers.iter().any(|l| l == "l3x5v1b") {
            for f in ["water/ocean_normal_01.dds", "water/ocean_normal_02.dds", "water/foam_mask.dds"] {
                let order: Vec<String> = r.profile.layers.iter().filter(|l| *l == "l9q2w7e" || *l == "l3x5v1b").cloned().collect();
                conflicts.push(Conflict { path: f.into(), layers: order, deleted_by: Vec::new() });
            }
        }
        let saves = self.saves_plan(id, r.profile.saves.as_deref());
        let mods = self.stack_layers(&r.profile.layers);
        let stores = self.all_stores(id);
        let store = stores.iter().find(|s| Some(&s.id) == r.profile.saves.as_ref());
        let warnings = store.and_then(|s| quiver_api::savestores::shared_store_warning(&mods, s, self.vanilla() == s.id)).into_iter().collect();
        ProfileInfo { profile: r.profile.clone(), revision: r.revision, missing_layers: missing, conflicts, warnings, saves: Some(saves), resolved, ..Default::default() }
    }

    fn state(&self, id: u32) -> ModsState {
        let (deleted, live): (Vec<_>, Vec<_>) = self.history.iter().filter_map(|h| h.last()).partition(|r| r.deleted);
        ModsState {
            product_id: id,
            layers: self.layers.clone(),
            profiles: live.into_iter().map(|r| self.info(id, r)).collect(),
            deleted: deleted.into_iter().map(|r| self.info(id, r)).collect(),
            save_stores: self.all_stores(id),
            vanilla_saves: Some(self.saves_plan(id, Some(&self.vanilla()))),
            mods: self.all_mods(),
            ..Default::default()
        }
    }

    // -- save stores (the daemon's rules, simplified) ----------------------------------

    fn vanilla(&self) -> String {
        self.vanilla_store.clone().unwrap_or_else(|| DEFAULT_STORE.into())
    }

    fn all_stores(&self, id: u32) -> Vec<SaveStore> {
        if self.stores.is_empty() { vec![default_store(id)] } else { self.stores.clone() }
    }

    fn saves_plan(&self, id: u32, store: Option<&str>) -> SavesPlan {
        let stores = self.all_stores(id);
        let s = stores.iter().find(|s| Some(s.id.as_str()) == store).cloned().unwrap_or_else(|| default_store(id));
        let cloud = stores.iter().find(|s| s.cloud && !s.retired);
        SavesPlan {
            separate: !s.is_default(),
            reason: format!("uses the '{}' store; {}", s.name, if s.cloud { "synced with the cloud" } else { "kept on this PC (not the cloud store)" }),
            folder: format!("savegames/<user>/{}", s.folder),
            cloud: s.cloud,
            store: Some(s.id.clone()),
            store_name: Some(s.name.clone()),
            cloud_note: (!s.cloud).then(|| match cloud {
                Some(c) => format!("'{}' stays on this PC: Ubisoft's cloud keeps one set of saves per game, and '{}' is the store that syncs with it", s.name, c.name),
                None => format!("'{}' stays on this PC: no save store syncs with Ubisoft's cloud", s.name),
            }),
        }
    }

    /// A store id for `q` (`new`: a store named after the profile).
    fn resolve_store(&mut self, id: u32, q: &str, profile: &str, n: u32) -> Result<String> {
        if self.stores.is_empty() {
            self.stores = vec![default_store(id)];
        }
        if q.eq_ignore_ascii_case(NEW_STORE) {
            let mut name = profile.to_owned();
            let mut k = 2;
            while self.stores.iter().any(|s| !s.retired && s.name.eq_ignore_ascii_case(&name)) {
                name = format!("{profile} {k}");
                k += 1;
            }
            let sid = random_id('s', n);
            self.stores.push(SaveStore {
                folder: format!("{id}-{sid}"),
                id: sid.clone(),
                name,
                created: Some(Utc::now()),
                created_by: Some("gui".into()),
                note: Some(format!("made for profile '{profile}'")),
                ..Default::default()
            });
            return Ok(sid);
        }
        self.stores
            .iter()
            .find(|s| !s.retired && (s.id == q || s.name.eq_ignore_ascii_case(q)))
            .map(|s| s.id.clone())
            .ok_or_else(|| anyhow!("no save store '{q}'"))
    }

    fn stores_report(&self, id: u32) -> SaveStoresReport {
        let users = |sid: &str| -> Vec<StoreUser> {
            let mut v: Vec<StoreUser> = Vec::new();
            if self.vanilla() == sid {
                v.push(StoreUser { id: VANILLA.into(), name: "Vanilla".into() });
            }
            v.extend(self.live().filter(|r| r.profile.saves.as_deref() == Some(sid)).map(|r| StoreUser { id: r.profile.id.clone(), name: r.profile.name.clone() }));
            v
        };
        let info = |s: &SaveStore| {
            let (slots, days) = if s.is_default() { (6, 0) } else { (3, 1) };
            SaveStoreInfo {
                store: s.clone(),
                used_by: users(&s.id),
                slots,
                bytes: slots as u64 * 4_200_000,
                last_modified: Some(at(days, 22, 41)),
                dir: None,
            }
        };
        let stores = self.all_stores(id);
        SaveStoresReport {
            product_id: id,
            revision: stores.len() as u32 - 1,
            account: Some(ACCOUNT.into()),
            stores: stores.iter().filter(|s| !s.retired).map(info).collect(),
            retired: stores.iter().filter(|s| s.retired).map(info).collect(),
            vanilla: self.vanilla(),
            cloud: stores.iter().find(|s| s.cloud && !s.retired).map(|s| s.id.clone()),
            unassigned: Vec::new(),
        }
    }

    fn edit_stores(&mut self, id: u32, op: SaveStoreOp, n: u32) -> Result<SaveStoresRevision> {
        if self.stores.is_empty() {
            self.stores = vec![default_store(id)];
        }
        let find = |stores: &[SaveStore], q: &str| stores.iter().position(|s| s.id == q || s.name.eq_ignore_ascii_case(q)).ok_or_else(|| anyhow!("no save store '{q}'"));
        let message = match op {
            SaveStoreOp::Create { name } => {
                let sid = self.resolve_store(id, NEW_STORE, &name, n)?;
                format!("created '{}'", self.stores.iter().find(|s| s.id == sid).map_or(name, |s| s.name.clone()))
            }
            SaveStoreOp::Rename { store, name } => {
                let i = find(&self.stores, &store)?;
                self.stores[i].name = name.clone();
                format!("renamed to '{name}'")
            }
            SaveStoreOp::Cloud { store } => {
                let target = store.as_deref().map(|q| find(&self.stores, q)).transpose()?;
                for (i, s) in self.stores.iter_mut().enumerate() {
                    s.cloud = Some(i) == target;
                }
                "moved the cloud flag".into()
            }
            SaveStoreOp::Retire { store } => {
                let i = find(&self.stores, &store)?;
                let sid = self.stores[i].id.clone();
                if self.stores[i].is_default() || self.stores[i].cloud || self.vanilla() == sid || self.live().any(|r| r.profile.saves.as_deref() == Some(sid.as_str())) {
                    bail!("'{}' is in use (Default, the cloud store, or a profile's store)", self.stores[i].name);
                }
                self.stores[i].retired = true;
                format!("retired '{}' (its files stay)", self.stores[i].name)
            }
            SaveStoreOp::Unretire { store } => {
                let i = find(&self.stores, &store)?;
                self.stores[i].retired = false;
                format!("brought back '{}'", self.stores[i].name)
            }
            SaveStoreOp::Vanilla { store } => {
                let i = find(&self.stores, &store)?;
                self.vanilla_store = Some(self.stores[i].id.clone());
                format!("vanilla uses '{}'", self.stores[i].name)
            }
            SaveStoreOp::Restore { .. } => bail!("restoring save store revisions isn't simulated in demo mode"),
        };
        Ok(SaveStoresRevision { revision: self.stores.len() as u32, at: Utc::now(), source: "gui".into(), message: Some(message), stores: self.stores.clone(), vanilla: self.vanilla() })
    }
}

fn random_id(prefix: char, n: u32) -> String {
    const A: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut x = n.wrapping_mul(2_654_435_761) ^ 0x5bd1_e995;
    let mut s = String::from(prefix);
    for _ in 0..6 {
        s.push(A[(x % A.len() as u32) as usize] as char);
        x = x.rotate_left(5).wrapping_mul(31).wrapping_add(7);
    }
    s
}

impl Detail {
    /// A mods request; also returns the product whose mods changed.
    fn mods(&mut self, st: &State, req: ModsRequest) -> Result<(Value, Option<u32>)> {
        let src = "gui";
        let product = match &req {
            ModsRequest::Games => None,
            ModsRequest::List { game }
            | ModsRequest::LayerAdd { game, .. }
            | ModsRequest::LayerRemove { game, .. }
            | ModsRequest::ProfileCreate { game, .. }
            | ModsRequest::ProfileSet { game, .. }
            | ModsRequest::ProfileDelete { game, .. }
            | ModsRequest::ProfileHistory { game, .. }
            | ModsRequest::ProfileRestore { game, .. }
            | ModsRequest::LayerDeletes { game, .. }
            | ModsRequest::LayerHistory { game, .. }
            | ModsRequest::LayerRestore { game, .. }
            | ModsRequest::Catalog { game }
            | ModsRequest::Install { game, .. }
            | ModsRequest::Uninstall { game, .. }
            | ModsRequest::Show { game, .. }
            | ModsRequest::ModSet { game, .. }
            | ModsRequest::Plan { game, .. }
            | ModsRequest::PatchesUndo { game, .. }
            | ModsRequest::Migrate { game, .. }
            | ModsRequest::VersionAdd { game, .. }
            | ModsRequest::VersionUse { game, .. }
            | ModsRequest::VersionRetire { game, .. }
            | ModsRequest::ModHistory { game, .. }
            | ModsRequest::MergeVersions { game, .. } => Some(game.as_ubisoft().unwrap_or(0)),
            ModsRequest::Prefs { .. } => None,
        };
        if let ModsRequest::Prefs { .. } = req {
            return Ok((json!(quiver_api::mods::ModPrefs::default()), None));
        }
        if let ModsRequest::Games = req {
            let list: Vec<Value> = st
                .entries
                .iter()
                .filter(|e| e.installed.is_some())
                .map(|e| {
                    let profiles: Vec<Profile> =
                        e.ubisoft_id().and_then(|id| self.mods.get(&id)).map(|m| m.live().map(|r| r.profile.clone()).collect()).unwrap_or_default();
                    json!({ "product_id": e.ubisoft_id(), "name": e.name, "profiles": profiles })
                })
                .collect();
            return Ok((json!(list), None));
        }
        let id = product.unwrap_or_default();
        installed(st, id).map_err(|e| anyhow!("{e} (mods need an installed game)"))?;
        self.next += 1;
        let n = self.next;
        let m = self.mods.entry(id).or_default();
        let now = Utc::now();
        Ok(match req {
            ModsRequest::Games => unreachable!(),
            ModsRequest::List { .. } => {
                let mut s = m.state(id);
                s.default_profile = st.settings.get(&id).and_then(|s| s.profile.clone());
                (json!(s), None)
            }
            ModsRequest::LayerDeletes { .. } | ModsRequest::LayerHistory { .. } | ModsRequest::LayerRestore { .. } => {
                bail!("layer deletions aren't simulated in demo mode")
            }
            ModsRequest::Catalog { .. } => (json!(Vec::<quiver_api::mods::CatalogEntry>::new()), None),
            ModsRequest::Prefs { .. }
            | ModsRequest::Install { .. }
            | ModsRequest::Uninstall { .. }
            | ModsRequest::Show { .. }
            | ModsRequest::ModSet { .. }
            | ModsRequest::Plan { .. }
            | ModsRequest::PatchesUndo { .. }
            | ModsRequest::Migrate { .. } => bail!("Mods v2 requests aren't simulated in demo mode yet"),
            ModsRequest::VersionAdd { .. } | ModsRequest::MergeVersions { .. } => bail!("adding and merging mod versions isn't simulated in demo mode"),
            ModsRequest::ModHistory { .. } => (json!(Vec::<quiver_api::mods::ModRevision>::new()), None),
            ModsRequest::VersionUse { mod_id, version, .. } => (json!(m.version_op(&mod_id, &version, "use")?), Some(id)),
            ModsRequest::VersionRetire { mod_id, version, retired, .. } => {
                (json!(m.version_op(&mod_id, &version, if retired { "retire" } else { "unretire" })?), Some(id))
            }
            ModsRequest::LayerAdd { name, path, .. } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    bail!("a layer needs a name");
                }
                if !path.is_absolute() {
                    bail!("{}: use an absolute path", path.display());
                }
                let files = 3 + (name.len() as u64 * 7) % 40;
                let l = Layer { id: random_id('l', n), name, added: now, added_by: src.into(), origin: Some(path), files, bytes: files * 1_350_000, deletes: Vec::new(), ..Default::default() };
                m.layers.push(l.clone());
                (json!(l), Some(id))
            }
            ModsRequest::LayerRemove { layer, confirmed, .. } => {
                if !confirmed {
                    bail!("removing a mod layer needs your approval");
                }
                let lid = split_entry(&m.resolve_layer(&layer)?).0.to_owned();
                if let Some(p) = m.live().find(|r| r.profile.layers.iter().any(|e| split_entry(e).0 == lid)) {
                    bail!("profile '{}' uses this mod; take it out of the profile first", p.profile.name);
                }
                let rec = m.all_mods().into_iter().find(|r| r.id == lid).unwrap();
                let mut gone = m.layers.iter().find(|l| l.id == rec.current).cloned().unwrap_or_default();
                gone.name = rec.name.clone();
                m.layers.retain(|l| !rec.has(&l.id));
                m.records.retain(|r| r.id != rec.id);
                (json!(gone), Some(id))
            }
            ModsRequest::ProfileCreate { name, layers, message, saves, .. } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    bail!("a profile needs a name");
                }
                if name.eq_ignore_ascii_case(VANILLA) {
                    bail!("'vanilla' is reserved for the unmodded game");
                }
                if m.live().any(|r| r.profile.name.eq_ignore_ascii_case(&name)) {
                    bail!("there is already a profile called '{name}'");
                }
                let layers = layers.iter().map(|l| m.resolve_layer(l)).collect::<Result<Vec<_>>>()?;
                // The daemon's steering: Default when every mod is save-compatible.
                let advice = quiver_api::savestores::saves_advice(&m.stack_layers(&layers));
                let wanted = saves.unwrap_or_else(|| if advice.share_default { DEFAULT_STORE.into() } else { NEW_STORE.into() });
                let store = m.resolve_store(id, &wanted, &name, n)?;
                let p = Profile { id: random_id('p', n), name, layers, saves: Some(store), ..Default::default() };
                let r = ProfileRevision { revision: 1, at: now, source: src.into(), message, deleted: false, profile: p };
                m.history.push(vec![r.clone()]);
                (json!(r), Some(id))
            }
            ModsRequest::ProfileSet { profile, ops, message, .. } => {
                let i = m.find(&profile, false)?;
                let mut p = m.history[i].last().unwrap().profile.clone();
                for op in &ops {
                    let op = match op {
                        SettingOp::Set { key, value } if key == "layers" || key == "mods" => SettingOp::Set {
                            key: key.clone(),
                            value: quiver_api::mods::split_list(value).iter().map(|l| m.resolve_layer(l)).collect::<Result<Vec<_>>>()?.join(","),
                        },
                        SettingOp::Set { key, value } if key == "saves" => {
                            let pname = p.name.clone();
                            SettingOp::Set { key: key.clone(), value: m.resolve_store(id, value, &pname, n)? }
                        }
                        op => op.clone(),
                    };
                    p.apply(&op).map_err(|e| anyhow!(e))?;
                }
                let revision = m.history[i].last().unwrap().revision + 1;
                let r = ProfileRevision { revision, at: now, source: src.into(), message, deleted: false, profile: p };
                m.history[i].push(r.clone());
                (json!(r), Some(id))
            }
            ModsRequest::ProfileDelete { profile, confirmed, .. } => {
                if !confirmed {
                    bail!("deleting a mod profile needs your approval");
                }
                let i = m.find(&profile, false)?;
                let last = m.history[i].last().unwrap();
                let r = ProfileRevision { revision: last.revision + 1, at: now, source: src.into(), message: None, deleted: true, profile: last.profile.clone() };
                m.history[i].push(r.clone());
                (json!(r), Some(id))
            }
            ModsRequest::ProfileHistory { profile, .. } => {
                let i = m.find(&profile, true)?;
                (json!(m.history[i]), None)
            }
            ModsRequest::ProfileRestore { profile, revision, confirmed, .. } => {
                if !confirmed {
                    bail!("restoring a mod profile revision needs your approval");
                }
                let i = m.find(&profile, true)?;
                let old = m.history[i]
                    .iter()
                    .find(|r| r.revision == revision && !r.deleted)
                    .ok_or_else(|| anyhow!("no revision {revision} to restore"))?
                    .profile
                    .clone();
                let next = m.history[i].last().unwrap().revision + 1;
                let r = ProfileRevision { revision: next, at: now, source: src.into(), message: Some(format!("Restore r{revision}")), deleted: false, profile: old };
                m.history[i].push(r.clone());
                (json!(r), Some(id))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use quiver_api::exports::ExportStatus;
    use quiver_api::mods::ModsState;

    fn call<T: serde::de::DeserializeOwned>(demo: &Demo, req: Request) -> T {
        serde_json::from_value(demo.handle(req).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn steam_games_carry_their_hook_and_switch_it() {
        let (demo, _events) = Demo::start();
        let detail = |app: u32| -> quiver_api::stores::GameDetail { call(&demo, Request::GameDetail { game: quiver_api::GameRef::steam(app), refresh: false }) };
        assert!(detail(1145360).steam_hook.unwrap().hooked(), "Hades starts through Quiver");
        let celeste = detail(504230).steam_hook.unwrap();
        assert!(!celeste.hooked() && celeste.hookable && celeste.command.ends_with("--app 504230"));
        let on: quiver_api::exports::SteamHook = call(&demo, Request::SteamHook { app_id: 504230, on: true, steam_user: None });
        assert!(on.hooked() && !on.unchanged);
        assert!(detail(504230).steam_hook.unwrap().hooked());
        let again: quiver_api::exports::SteamHook = call(&demo, Request::SteamHook { app_id: 504230, on: true, steam_user: None });
        assert!(again.unchanged);
        let off: quiver_api::exports::SteamHook = call(&demo, Request::SteamHook { app_id: 504230, on: false, steam_user: None });
        assert!(!off.hooked() && off.launch_options.is_empty());
    }

    #[tokio::test]
    async fn settings_edit_and_restore() {
        let (demo, _events) = Demo::start();
        let v: Value = call(&demo, Request::SettingsGet { game: 3539.into() });
        assert_eq!(v["revision"], 15);
        assert_eq!(v["settings"]["proton"], "GE-Proton10-25");
        let ops = vec![SettingOp::parse("env.DXVK_HUD=fps").unwrap(), SettingOp::parse("proton=").unwrap()];
        let r: Revision = call(&demo, Request::SettingsSet { game: 3539.into(), ops, message: Some("HUD".into()) });
        assert_eq!(r.revision, 16);
        assert_eq!(r.settings.env["DXVK_HUD"], "fps");
        assert_eq!(r.settings.proton, None);
        assert!(demo.handle(Request::SettingsSet { game: 3539.into(), ops: vec![SettingOp::parse("proton=nope").unwrap()], message: None }).is_err());
        let r: Revision = call(&demo, Request::SettingsRestore { game: 3539.into(), revision: 13 });
        assert_eq!(r.revision, 17);
        assert_eq!(r.settings.proton.as_deref(), Some("GE-Proton10-25"));
        let h: Vec<Revision> = call(&demo, Request::SettingsHistory { game: 3539.into() });
        assert_eq!(h[0].revision, 17);
    }

    #[tokio::test]
    async fn default_profile() {
        let (demo, _events) = Demo::start();
        // Origins plays its DLSS profile by default.
        let m: ModsState = call(&demo, Request::Mods(ModsRequest::List { game: 3539.into() }));
        assert_eq!(m.default_profile.as_deref(), Some(ORIGINS_DEFAULT));
        assert!(m.profiles.iter().any(|p| p.profile.id == ORIGINS_DEFAULT && p.profile.name == "DLSS"));
        let session_profile = |demo: &Demo| {
            let pt: quiver_api::Playtime = call(demo, Request::Playtime { game: quiver_api::GameRef::ubisoft(3539).into() });
            pt.sessions[0].profile.clone()
        };
        // No profile named: the default. `vanilla`: forced vanilla.
        for (asked, ran) in [(None, Some(ORIGINS_DEFAULT)), (Some(VANILLA), None), (Some("reshade"), Some("pz6c4uf"))] {
            demo.handle(Request::Play { game: quiver_api::GameRef::ubisoft(3539).into(), profile: asked.map(str::to_owned) }).unwrap();
            demo.handle(Request::Stop { game: quiver_api::GameRef::ubisoft(3539).into() }).unwrap();
            assert_eq!(session_profile(&demo).as_deref(), ran, "{asked:?}");
        }
        // Set by name, stored as the id; unknown names are refused; `vanilla` clears it.
        let set = |v: &str| Request::SettingsSet { game: 3539.into(), ops: vec![SettingOp::parse(&format!("profile={v}")).unwrap()], message: None };
        let r: Revision = call(&demo, set("ReShade"));
        assert_eq!(r.settings.profile.as_deref(), Some("pz6c4uf"));
        assert!(demo.handle(set("nope")).is_err());
        let r: Revision = call(&demo, set("vanilla"));
        assert_eq!(r.settings.profile, None);
        demo.handle(Request::Play { game: quiver_api::GameRef::ubisoft(3539).into(), profile: None }).unwrap();
        demo.handle(Request::Stop { game: quiver_api::GameRef::ubisoft(3539).into() }).unwrap();
        assert_eq!(session_profile(&demo), None);
    }

    #[tokio::test]
    async fn info_progress_and_saves() {
        let (demo, _events) = Demo::start();
        let i: GameInfo = call(&demo, Request::GameInfo { product_id: 3539, refresh: false });
        assert!(i.install.as_ref().is_some_and(|i| i.size_bytes > 60 * GB));
        assert!(i.addons.iter().any(|a| a.owned) && i.addons.iter().any(|a| !a.owned && a.store_url.is_some()));
        assert!(i.addons.iter().all(|a| a.owned == a.store_url.is_none()));
        let p: Progress = call(&demo, Request::GameProgress { product_id: 3539, refresh: false });
        assert!(p.playtime_seconds.unwrap() > 80 * 3600);
        let a = p.achievements.unwrap();
        assert_eq!(a.total, 67);
        // Unlock status shown: 59 "Namaste" earned.
        assert!(a.unlocked_known && a.unlocked == 1);
        assert!(a.items.iter().all(|x| x.unlocked == (x.id == "59")));
        let s: SavesReport = call(&demo, Request::Saves { product_id: 3539, cloud: true });
        assert!(s.slots.iter().any(|x| x.state == SlotState::CloudNewer));
        let local: SavesReport = call(&demo, Request::Saves { product_id: 3539, cloud: false });
        assert!(local.slots.iter().all(|x| x.cloud.is_none()));
        // Restoring needs the person's confirmation, and backs up what was there.
        let backup = s.backups[2].id.clone();
        assert!(demo.handle(Request::SavesRestore { product_id: 3539, backup: backup.clone(), confirmed: false }).is_err());
        let r: RestoreResult = call(&demo, Request::SavesRestore { product_id: 3539, backup, confirmed: true });
        assert!(!r.restored.is_empty() && r.previous.is_some());
        let after: SavesReport = call(&demo, Request::Saves { product_id: 3539, cloud: true });
        assert_eq!(after.backups.len(), s.backups.len() + 1);
        assert!(log_text(3539, "bridge.log").unwrap().contains("Denuvo"));
        assert!(log_text(3539, "nope.log").is_none());
    }

    #[tokio::test]
    async fn mods_profiles_lifecycle() {
        let (demo, mut events) = Demo::start();
        let m: ModsState = call(&demo, Request::Mods(ModsRequest::List { game: 5059.into() }));
        assert_eq!(m.profiles.len(), 2);
        assert_eq!(m.deleted.len(), 1);
        assert_eq!(m.profiles[0].conflicts.len(), 3);
        let l: Layer = call(&demo, Request::Mods(ModsRequest::LayerAdd { game: 5059.into(), name: "Mine".into(), path: "/tmp/x".into(), deletes: Vec::new() }));
        let r: ProfileRevision = call(
            &demo,
            Request::Mods(ModsRequest::ProfileCreate { game: 5059.into(), name: "Test".into(), layers: vec![l.id.clone()], message: None, saves: None }),
        );
        assert_eq!(r.profile.layers, vec![l.id.clone()]);
        // A layer of unknown save compatibility: steered to a store of its own.
        assert!(r.profile.saves.as_deref().is_some_and(|s| s != "default"));
        let mut got = false;
        for _ in 0..8 {
            if let Ok(Some(Event::ModsChanged { product_id: 5059, .. })) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
                got = true;
                break;
            }
        }
        assert!(got, "ModsChanged");
        let ops = vec![SettingOp::parse("saves=default").unwrap(), SettingOp::parse("pinned=on").unwrap()];
        let r2: ProfileRevision =
            call(&demo, Request::Mods(ModsRequest::ProfileSet { game: 5059.into(), profile: "test".into(), ops, message: None }));
        assert!(r2.profile.saves.as_deref() == Some("default") && r2.profile.pinned && r2.revision == 2);
        // Origins has two stores; the stores report says who uses which.
        let s: SaveStoresReport = call(&demo, Request::SaveStores { product_id: 3539 });
        assert_eq!(s.stores.len(), 2);
        assert_eq!(s.cloud.as_deref(), Some("default"));
        assert_eq!(s.stores[1].used_by[0].name, "Nightmare");
        assert!(demo.handle(Request::Mods(ModsRequest::ProfileDelete { game: 5059.into(), profile: r.profile.id.clone(), confirmed: false })).is_err());
        let d: ProfileRevision = call(&demo, Request::Mods(ModsRequest::ProfileDelete { game: 5059.into(), profile: r.profile.id.clone(), confirmed: true }));
        assert!(d.deleted);
        let h: Vec<ProfileRevision> = call(&demo, Request::Mods(ModsRequest::ProfileHistory { game: 5059.into(), profile: r.profile.id.clone() }));
        assert_eq!(h.len(), 3);
        let back: ProfileRevision = call(
            &demo,
            Request::Mods(ModsRequest::ProfileRestore { game: 5059.into(), profile: r.profile.id.clone(), revision: 2, confirmed: true }),
        );
        assert!(!back.deleted && back.profile.pinned);
        // Play with a profile that exists, by name; an unknown one fails.
        assert!(demo.handle(Request::Play { game: quiver_api::GameRef::ubisoft(5059).into(), profile: Some("Visual overhaul".into()) }).is_ok());
        assert!(demo.handle(Request::Play { game: quiver_api::GameRef::ubisoft(3539).into(), profile: Some("nope".into()) }).is_err());
    }

    #[tokio::test]
    async fn exports_wait_for_steam() {
        let (demo, _events) = Demo::start();
        let list: Vec<ExportStatus> = call(&demo, Request::ExportList);
        assert!(list.iter().any(|e| e.record.game == quiver_api::GameRef::ubisoft(5059) && e.state == ExportState::WaitingForSteam));
        let r: SteamExportResult = call(&demo, Request::ExportSteam { product_id: 1651, remove: false, steam_user: None });
        assert!(r.staged);
        assert!(demo.handle(Request::SteamApplyStaged).is_err(), "Steam is running");
        // Only once the person confirmed.
        assert!(demo.handle(Request::SteamRestart { confirmed: false }).is_err());
        // Two shortcuts and Watch Dogs: Legion's launch options.
        let r: RestartReport = call(&demo, Request::SteamRestart { confirmed: true });
        assert_eq!((r.was_running, r.applied), (true, 3));
        let list: Vec<ExportStatus> = call(&demo, Request::ExportList);
        assert!(list.iter().all(|e| e.state != ExportState::WaitingForSteam));
        let _: ExportRecord = call(&demo, Request::ExportDesktop { product_id: 1651, remove: false });
        let v: Value = call(&demo, Request::ExportDesktop { product_id: 1651, remove: true });
        assert_eq!(v["removed"], true);
    }

    #[tokio::test]
    async fn steam_games_switch_and_wait_for_steam() {
        let (demo, _events) = Demo::start();
        let s: SteamGames = call(&demo, Request::SteamGames { steam_user: None });
        assert!(s.running && s.offers);
        let state = |s: &SteamGames, id: u32| s.games.iter().find(|g| g.app_id == id).unwrap().state;
        assert_eq!(state(&s, 3751950), PatchState::On);
        assert_eq!(state(&s, 2239550), PatchState::Waiting);
        // Far Cry 4: its own option is kept as a game argument.
        let r: SteamPatchResult = call(&demo, Request::SteamPatch { app_id: 298110, steam_user: None });
        assert!(r.staged && !r.unchanged);
        assert!(r.launch_options.ends_with("steam-run --product 420 %command% -skipintro"), "{}", r.launch_options);
        // Undo before Steam restarted: nothing waits any more.
        let r: SteamPatchResult = call(&demo, Request::SteamUnpatch { app_id: 298110, steam_user: None });
        assert!(r.unchanged);
        let s: SteamGames = call(&demo, Request::SteamGames { steam_user: None });
        assert_eq!(state(&s, 298110), PatchState::Off);
        // Anno 1800's product isn't known.
        assert!(demo.handle(Request::SteamPatch { app_id: 916440, steam_user: None }).is_err());
        let u: SteamUsers = call(&demo, Request::SteamUsers);
        assert!(u.staged.iter().any(|s| s.contains("Watch Dogs: Legion")));
        let _: RestartReport = call(&demo, Request::SteamRestart { confirmed: true });
        let s: SteamGames = call(&demo, Request::SteamGames { steam_user: None });
        assert_eq!(state(&s, 2239550), PatchState::On);
        let v: Value = call(&demo, Request::SteamOffers { enabled: Some(false) });
        assert_eq!(v["enabled"], false);
    }

    #[tokio::test]
    async fn steam_copies_in_the_library() {
        let (demo, _events) = Demo::start();
        let lib: Vec<LibraryEntry> = call(&demo, Request::Library { refresh: false, all_stores: true });
        let e = |id: u32| lib.iter().find(|e| e.ubisoft_id() == Some(id)).unwrap().clone();
        // Black Flag Resynced: only Steam has it, and it's owned on Steam.
        let bf = e(66088);
        assert!(bf.steam_only() && !bf.installable && bf.steam.as_ref().unwrap().app_id == 3751950);
        // Syndicate is owned on Steam too; Far Cry 4 has a Ubisoft build as well.
        assert!(e(1875).steam_only() && !e(1875).installable);
        assert!(e(420).steam_only() && e(420).installable);
        assert!(e(3539).steam.is_none());
        // Play asks Steam; a game switched to Quiver then reports a session from Steam.
        let _: u64 = call(&demo, Request::Play { game: quiver_api::GameRef::ubisoft(66088).into(), profile: None });
        let sessions: Vec<quiver_api::ActiveSession> = call(&demo, Request::Sessions);
        let s = sessions.iter().find(|s| s.product_id == 66088).unwrap();
        assert_eq!((s.holders.as_slice(), s.steam_app_id), (&["steam".to_owned()][..], Some(3751950)));
        // Syndicate still opens Connect: no Quiver session.
        let _: u64 = call(&demo, Request::Play { game: quiver_api::GameRef::ubisoft(1875).into(), profile: None });
        let sessions: Vec<quiver_api::ActiveSession> = call(&demo, Request::Sessions);
        assert!(sessions.iter().all(|s| s.product_id != 1875));
    }
}
