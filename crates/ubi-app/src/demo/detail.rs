//! Demo answers for the game page: settings edits and restores, game info, progression,
//! saves and backups, mod layers and profiles, exports, and the per-launch logs. Shapes
//! follow the daemon (`crates/ubid`) and the UbiServices fixtures in
//! `crates/ubi-core/fixtures`; AC Origins (3539) and AC Odyssey (5059) are the rich ones,
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
use ubi_api::exports::{
    ExportKind, ExportRecord, ExportState, ExportStatus, PatchState, SteamExportResult, SteamGame, SteamGames, SteamPatchResult, SteamUser,
    SteamUsers,
};
use ubi_api::info::{
    Achievement, Achievements, AddOn, CloudRecord, CloudSlot, ConnectProfile, Features, GameInfo, InstallInfo, Link,
    ProductKind, Progress, RecordSlot, RestoreResult, Reward, SaveBackup, SaveSlot, SavesReport, SlotFile, SlotState, Stat,
};
use ubi_api::mods::{Conflict, Layer, ModsRequest, ModsState, ProfileInfo, ProfileRevision, VANILLA};
use ubi_api::{Event, GameSettings, LibraryEntry, Profile, Request, Revision, SettingOp};

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
}

static DETAILS: LazyLock<Mutex<HashMap<usize, Detail>>> = LazyLock::new(Mutex::default);

/// Answer a game-page request, or hand it back for `demo.rs` to answer.
pub(super) fn handle(demo: &Demo, st: &mut State, req: Request) -> Result<std::result::Result<Value, Request>> {
    let key = demo as *const Demo as usize;
    let mut all = DETAILS.lock().unwrap();
    let d = all.entry(key).or_insert_with(|| Detail::new(&st.entries));
    let mut events = Vec::new();
    let out = match req {
        Request::SettingsGet { product_id } => {
            // The daemon's shape: the settings and their revision.
            let settings = st.settings.get(&product_id).cloned().unwrap_or_default();
            let revision = st.history.get(&product_id).and_then(|h| h.iter().map(|r| r.revision).max()).unwrap_or(0);
            json!({ "revision": revision, "settings": settings })
        }
        Request::SettingsSet { product_id, ops, message } => {
            installed(st, product_id)?;
            let mut s = st.settings.get(&product_id).cloned().unwrap_or_default();
            for op in &ops {
                if let SettingOp::Set { key, value } = op
                    && key == "proton"
                    && !PROTONS.contains(&value.as_str())
                {
                    bail!("no Proton build '{value}' (installed: {})", PROTONS.join(", "));
                }
                s.apply(op).map_err(|e| anyhow!(e))?;
            }
            let rev = push_revision(st, product_id, s, "gui", message);
            events.push(Event::SettingsChanged { product_id, revision: rev.revision, source: "gui".into() });
            json!(rev)
        }
        Request::SettingsRestore { product_id, revision } => {
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
        Request::Mods(m) => {
            let (v, changed) = d.mods(st, m)?;
            if let Some(product_id) = changed {
                events.push(Event::ModsChanged { product_id });
            }
            v
        }
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
            let shown = if g.state.uses_quiver() { demo_patched(&g.launch_options) } else { demo_unpatched(&g.launch_options) };
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
        Request::SteamApplyStaged => {
            if d.steam_running {
                bail!("Steam is running; staged shortcuts are written when it closes");
            }
            json!({ "applied": d.apply_staged() })
        }
        Request::Play { product_id, profile } => {
            let mut id = None;
            if let Some(p) = profile.as_deref().filter(|p| *p != VANILLA) {
                let mods = d.mods.get(&product_id);
                let found = mods.and_then(|m| m.live().find(|r| r.profile.id == p || r.profile.name.eq_ignore_ascii_case(p)));
                match found {
                    Some(r) => id = Some(r.profile.id.clone()),
                    None => bail!("no mod profile '{p}' for {}", name(st, product_id)),
                }
            }
            if !st.sessions.iter().any(|s| s.product_id == product_id) {
                d.running.insert(product_id, (id, Utc::now()));
            }
            return Ok(Err(Request::Play { product_id, profile }));
        }
        Request::Stop { product_id } => {
            // Record the session like the daemon does (at least 25 minutes, so it shows).
            if let Some((profile, started)) = d.running.remove(&product_id) {
                let ended = Utc::now();
                let seconds = ((ended - started).num_seconds().max(0) as u64).max(25 * 60);
                let rev = st.history.get(&product_id).and_then(|h| h.iter().map(|r| r.revision).max()).unwrap_or(0);
                let pt = st.playtime.entry(product_id).or_default();
                let profile_revision = profile.as_ref().map(|_| 1);
                pt.sessions.insert(
                    0,
                    ubi_api::PlaySession { started: ended - Span::seconds(seconds as i64), ended, seconds, settings_revision: rev, profile, profile_revision },
                );
                pt.total_seconds += seconds;
                pt.last_played = Some(ended);
            }
            return Ok(Err(Request::Stop { product_id }));
        }
        other => return Ok(Err(other)),
    };
    drop(all);
    for e in events {
        demo.emit(e);
    }
    Ok(Ok(out))
}

impl Demo {
    /// What `quiver steam restart` does once the person confirmed it: Steam closes, the
    /// staged shortcuts are written, Steam starts again. Demo only.
    pub fn restart_steam(&self) -> usize {
        let key = self as *const Demo as usize;
        let mut all = DETAILS.lock().unwrap();
        all.get_mut(&key).map(|d| d.apply_staged()).unwrap_or(0)
    }
}

/// The text of a per-launch log in `<prefix>/ubi-run/` for the demo's games.
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
            format!("{} INFO ubi-bridge 0.1.0 starting (product {product_id})", ts(0)),
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
            format!("{} wine: using Proton GE-Proton10-25 (prefix ~/Games/ubi-prefixes/{product_id})", ts(2)),
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
            "ProtonFixes[1201] INFO: Using global defaults for ubi-bridge.exe".into(),
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

fn entry(st: &State, id: u32) -> Result<&LibraryEntry> {
    st.entries.iter().find(|e| e.product_id == id).ok_or_else(|| anyhow!("unknown product {id}"))
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
                unlocked: false,
                unlocked_at: None,
            })
        })
        .collect();
    Some(Achievements { unlocked_known: false, unlocked: 0, total: items.len() as u32, items })
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
        let name = |id: u32| entries.iter().find(|e| e.product_id == id).map(|e| e.name.clone()).unwrap_or_default();
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
            SaveBackup { path: root.join(&bid), id: bid, created: Some(t), slots, reason: Some(reason.into()), account: Some(ACCOUNT.into()) }
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
        installed(st, id)?;
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
            g.launch_options = if next == PatchState::On { demo_patched(&g.launch_options) } else { demo_unpatched(&g.launch_options) };
            g.state = next;
            n += 1;
        }
        n
    }
}

fn steam_command() -> String {
    format!("{} --source steam steam-run", home().join(".local/bin/quiver").display())
}

fn demo_patched(opts: &str) -> String {
    if opts.contains("steam-run") {
        return opts.to_owned();
    }
    match opts.find("%command%") {
        Some(i) => format!("{}{} {}", &opts[..i], steam_command(), &opts[i..]),
        None if opts.trim().is_empty() => format!("{} %command%", steam_command()),
        None => format!("{} %command% {}", steam_command(), opts.trim()),
    }
}

fn demo_unpatched(opts: &str) -> String {
    let out = opts.replace(&format!("{} ", steam_command()), "");
    if out.trim() == "%command%" { String::new() } else { out }
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
    };
    let mut v = vec![
        g(3751950, "Assassin's Creed Black Flag Resynced", "Assassin's Creed Black Flag Resynced", Some(66088), &demo_patched(""), PatchState::On),
        g(368500, "Assassin's Creed Syndicate", "Assassin's Creed Syndicate", Some(1875), "", PatchState::Off),
        g(298110, "Far Cry 4", "Far Cry 4", Some(420), "-skipintro", PatchState::Off),
        g(2239550, "Watch Dogs: Legion", "Watch Dogs Legion", Some(3353), "PROTON_ENABLE_NVAPI=1 %command%", if running { PatchState::Waiting } else { PatchState::On }),
        g(916440, "Anno 1800", "Anno 1800", None, "", PatchState::Off),
    ];
    if !running {
        v[3].launch_options = demo_patched(&v[3].launch_options);
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
        product_id: id,
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

fn rev(revision: u32, days: i64, source: &str, message: Option<&str>, profile: Profile) -> ProfileRevision {
    ProfileRevision { revision, at: at(days, 20, 30), source: source.into(), message: message.map(str::to_owned), deleted: false, profile }
}

fn odyssey_mods() -> Mods {
    let layers = vec![
        layer("l4n8d2k", "ReShade 6.3.3 + Aegean Light", 14, "gui", "Downloads/reshade-odyssey", 41, 84_000_000),
        layer("l9q2w7e", "HD texture pack (community) v2.1", 14, "gui", "Downloads/odyssey-hd-2.1", 1_204, 12_800_000_000),
        layer("l3x5v1b", "Better water and weather", 9, "cli", "mods/odyssey/better-water", 17, 640_000_000),
        layer("l8m1z6t", "Skip intro videos", 2, "claude-code", "mods/odyssey/skip-intro", 6, 2_048),
    ];
    let mut visual = Profile {
        id: "p7f3k2q".into(),
        name: "Visual overhaul".into(),
        layers: vec!["l3x5v1b".into(), "l9q2w7e".into()],
        ..Default::default()
    };
    let mut h1 = vec![rev(1, 14, "gui", Some("HD textures and better water"), visual.clone())];
    visual.layers.push("l4n8d2k".into());
    visual.settings.dll_overrides.insert("dxgi".into(), "n,b".into());
    h1.push(rev(2, 13, "gui", Some("Add ReShade"), visual.clone()));
    visual.pinned = true;
    h1.push(rev(3, 12, "gui", None, visual.clone()));
    let mut photo = Profile { id: "pq2m9xa".into(), name: "Photo mode".into(), layers: vec!["l4n8d2k".into(), "l8m1z6t".into()], ..Default::default() };
    photo.settings.dll_overrides.insert("dxgi".into(), "n,b".into());
    let mut h2 = vec![rev(1, 2, "claude-code", Some("Photo mode: ReShade without the texture pack"), photo.clone())];
    photo.cloud_saves = false;
    let mut old = Profile { id: "pk0w3rd".into(), name: "Hardcore loot".into(), layers: vec!["l3x5v1b".into()], ..Default::default() };
    let mut h3 = vec![rev(1, 30, "cli", None, old.clone())];
    old.layers.clear();
    let mut del = rev(2, 21, "cli", None, old);
    del.deleted = true;
    h3.push(del);
    let _ = &mut h2;
    Mods { layers, history: vec![h1, h2, h3] }
}

fn origins_mods() -> Mods {
    let layers = vec![
        layer("lr2s8hd", "ReShade 6.3 + Desert Sun", 20, "gui", "Downloads/reshade-origins", 39, 81_000_000),
        layer("lb5k1yq", "Skip intro videos", 20, "cli", "mods/origins/skip-intro", 4, 1_536),
    ];
    let mut p = Profile { id: "pz6c4uf".into(), name: "ReShade".into(), layers: vec!["lb5k1yq".into(), "lr2s8hd".into()], ..Default::default() };
    p.settings.dll_overrides.insert("dxgi".into(), "n,b".into());
    Mods { layers, history: vec![vec![rev(1, 20, "gui", Some("ReShade with the Desert Sun preset"), p)]] }
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

    fn resolve_layer(&self, q: &str) -> Result<String> {
        self.layers
            .iter()
            .find(|l| l.id == q || l.name.eq_ignore_ascii_case(q))
            .map(|l| l.id.clone())
            .ok_or_else(|| anyhow!("no layer '{q}'"))
    }

    fn info(&self, r: &ProfileRevision) -> ProfileInfo {
        let missing = r.profile.layers.iter().filter(|l| !self.layers.iter().any(|x| &x.id == *l)).cloned().collect();
        // Demo conflicts: the texture pack and the water mod both ship ocean normals.
        let mut conflicts = vec![];
        if r.profile.layers.iter().any(|l| l == "l9q2w7e") && r.profile.layers.iter().any(|l| l == "l3x5v1b") {
            for f in ["water/ocean_normal_01.dds", "water/ocean_normal_02.dds", "water/foam_mask.dds"] {
                let order: Vec<String> = r.profile.layers.iter().filter(|l| *l == "l9q2w7e" || *l == "l3x5v1b").cloned().collect();
                conflicts.push(Conflict { path: f.into(), layers: order, deleted_by: Vec::new() });
            }
        }
        ProfileInfo { profile: r.profile.clone(), revision: r.revision, missing_layers: missing, conflicts, ..Default::default() }
    }

    fn state(&self, id: u32) -> ModsState {
        let (deleted, live): (Vec<_>, Vec<_>) = self.history.iter().filter_map(|h| h.last()).partition(|r| r.deleted);
        ModsState {
            product_id: id,
            layers: self.layers.clone(),
            profiles: live.into_iter().map(|r| self.info(r)).collect(),
            deleted: deleted.into_iter().map(|r| self.info(r)).collect(),
            ..Default::default()
        }
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
            ModsRequest::List { product_id }
            | ModsRequest::LayerAdd { product_id, .. }
            | ModsRequest::LayerRemove { product_id, .. }
            | ModsRequest::ProfileCreate { product_id, .. }
            | ModsRequest::ProfileSet { product_id, .. }
            | ModsRequest::ProfileDelete { product_id, .. }
            | ModsRequest::ProfileHistory { product_id, .. }
            | ModsRequest::ProfileRestore { product_id, .. }
            | ModsRequest::LayerDeletes { product_id, .. }
            | ModsRequest::LayerHistory { product_id, .. }
            | ModsRequest::LayerRestore { product_id, .. }
            | ModsRequest::Catalog { product_id }
            | ModsRequest::Install { product_id, .. }
            | ModsRequest::Uninstall { product_id, .. }
            | ModsRequest::Show { product_id, .. }
            | ModsRequest::ModSet { product_id, .. }
            | ModsRequest::Plan { product_id, .. }
            | ModsRequest::PatchesUndo { product_id, .. }
            | ModsRequest::Migrate { product_id, .. } => Some(*product_id),
            ModsRequest::Prefs { .. } => None,
        };
        if let ModsRequest::Prefs { .. } = req {
            return Ok((json!(ubi_api::mods::ModPrefs::default()), None));
        }
        if let ModsRequest::Games = req {
            let list: Vec<Value> = st
                .entries
                .iter()
                .filter(|e| e.installed.is_some())
                .map(|e| {
                    let profiles: Vec<Profile> =
                        self.mods.get(&e.product_id).map(|m| m.live().map(|r| r.profile.clone()).collect()).unwrap_or_default();
                    json!({ "product_id": e.product_id, "name": e.name, "profiles": profiles })
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
            ModsRequest::List { .. } => (json!(m.state(id)), None),
            ModsRequest::LayerDeletes { .. } | ModsRequest::LayerHistory { .. } | ModsRequest::LayerRestore { .. } => {
                bail!("layer deletions aren't simulated in demo mode")
            }
            ModsRequest::Catalog { .. } => (json!(Vec::<ubi_api::mods::CatalogEntry>::new()), None),
            ModsRequest::Prefs { .. }
            | ModsRequest::Install { .. }
            | ModsRequest::Uninstall { .. }
            | ModsRequest::Show { .. }
            | ModsRequest::ModSet { .. }
            | ModsRequest::Plan { .. }
            | ModsRequest::PatchesUndo { .. }
            | ModsRequest::Migrate { .. } => bail!("Mods v2 requests aren't simulated in demo mode yet"),
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
                let lid = m.resolve_layer(&layer)?;
                if let Some(p) = m.live().find(|r| r.profile.layers.contains(&lid)) {
                    bail!("profile '{}' uses this layer; take it out of the profile first", p.profile.name);
                }
                let i = m.layers.iter().position(|l| l.id == lid).unwrap();
                (json!(m.layers.remove(i)), Some(id))
            }
            ModsRequest::ProfileCreate { name, layers, message, .. } => {
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
                let p = Profile { id: random_id('p', n), name, layers, ..Default::default() };
                let r = ProfileRevision { revision: 1, at: now, source: src.into(), message, deleted: false, profile: p };
                m.history.push(vec![r.clone()]);
                (json!(r), Some(id))
            }
            ModsRequest::ProfileSet { profile, ops, message, .. } => {
                let i = m.find(&profile, false)?;
                let mut p = m.history[i].last().unwrap().profile.clone();
                for op in &ops {
                    let op = match op {
                        SettingOp::Set { key, value } if key == "layers" => SettingOp::Set {
                            key: key.clone(),
                            value: ubi_api::mods::split_list(value).iter().map(|l| m.resolve_layer(l)).collect::<Result<Vec<_>>>()?.join(","),
                        },
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
    use ubi_api::exports::ExportStatus;
    use ubi_api::mods::ModsState;

    fn call<T: serde::de::DeserializeOwned>(demo: &Demo, req: Request) -> T {
        serde_json::from_value(demo.handle(req).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn settings_edit_and_restore() {
        let (demo, _events) = Demo::start();
        let v: Value = call(&demo, Request::SettingsGet { product_id: 3539 });
        assert_eq!(v["revision"], 14);
        assert_eq!(v["settings"]["proton"], "GE-Proton10-25");
        let ops = vec![SettingOp::parse("env.DXVK_HUD=fps").unwrap(), SettingOp::parse("proton=").unwrap()];
        let r: Revision = call(&demo, Request::SettingsSet { product_id: 3539, ops, message: Some("HUD".into()) });
        assert_eq!(r.revision, 15);
        assert_eq!(r.settings.env["DXVK_HUD"], "fps");
        assert_eq!(r.settings.proton, None);
        assert!(demo.handle(Request::SettingsSet { product_id: 3539, ops: vec![SettingOp::parse("proton=nope").unwrap()], message: None }).is_err());
        let r: Revision = call(&demo, Request::SettingsRestore { product_id: 3539, revision: 13 });
        assert_eq!(r.revision, 16);
        assert_eq!(r.settings.proton.as_deref(), Some("GE-Proton10-25"));
        let h: Vec<Revision> = call(&demo, Request::SettingsHistory { product_id: 3539 });
        assert_eq!(h[0].revision, 16);
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
        assert!(!a.unlocked_known && a.items.iter().all(|x| !x.unlocked));
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
        let m: ModsState = call(&demo, Request::Mods(ModsRequest::List { product_id: 5059 }));
        assert_eq!(m.profiles.len(), 2);
        assert_eq!(m.deleted.len(), 1);
        assert_eq!(m.profiles[0].conflicts.len(), 3);
        let l: Layer = call(&demo, Request::Mods(ModsRequest::LayerAdd { product_id: 5059, name: "Mine".into(), path: "/tmp/x".into(), deletes: Vec::new() }));
        let r: ProfileRevision = call(
            &demo,
            Request::Mods(ModsRequest::ProfileCreate { product_id: 5059, name: "Test".into(), layers: vec![l.id.clone()], message: None }),
        );
        assert_eq!(r.profile.layers, vec![l.id.clone()]);
        let mut got = false;
        for _ in 0..8 {
            if let Ok(Some(Event::ModsChanged { product_id: 5059 })) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
                got = true;
                break;
            }
        }
        assert!(got, "ModsChanged");
        let ops = vec![SettingOp::parse("cloud_saves=on").unwrap(), SettingOp::parse("pinned=on").unwrap()];
        let r2: ProfileRevision =
            call(&demo, Request::Mods(ModsRequest::ProfileSet { product_id: 5059, profile: "test".into(), ops, message: None }));
        assert!(r2.profile.cloud_saves && r2.profile.pinned && r2.revision == 2);
        assert!(demo.handle(Request::Mods(ModsRequest::ProfileDelete { product_id: 5059, profile: r.profile.id.clone(), confirmed: false })).is_err());
        let d: ProfileRevision = call(&demo, Request::Mods(ModsRequest::ProfileDelete { product_id: 5059, profile: r.profile.id.clone(), confirmed: true }));
        assert!(d.deleted);
        let h: Vec<ProfileRevision> = call(&demo, Request::Mods(ModsRequest::ProfileHistory { product_id: 5059, profile: r.profile.id.clone() }));
        assert_eq!(h.len(), 3);
        let back: ProfileRevision = call(
            &demo,
            Request::Mods(ModsRequest::ProfileRestore { product_id: 5059, profile: r.profile.id.clone(), revision: 2, confirmed: true }),
        );
        assert!(!back.deleted && back.profile.pinned);
        // Play with a profile that exists, by name; an unknown one fails.
        assert!(demo.handle(Request::Play { product_id: 5059, profile: Some("Visual overhaul".into()) }).is_ok());
        assert!(demo.handle(Request::Play { product_id: 3539, profile: Some("nope".into()) }).is_err());
    }

    #[tokio::test]
    async fn exports_wait_for_steam() {
        let (demo, _events) = Demo::start();
        let list: Vec<ExportStatus> = call(&demo, Request::ExportList);
        assert!(list.iter().any(|e| e.record.product_id == 5059 && e.state == ExportState::WaitingForSteam));
        let r: SteamExportResult = call(&demo, Request::ExportSteam { product_id: 1651, remove: false, steam_user: None });
        assert!(r.staged);
        assert!(demo.handle(Request::SteamApplyStaged).is_err(), "Steam is running");
        // Two shortcuts and Watch Dogs: Legion's launch options.
        assert_eq!(demo.restart_steam(), 3);
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
        assert!(r.launch_options.ends_with("steam-run %command% -skipintro"), "{}", r.launch_options);
        // Undo before Steam restarted: nothing waits any more.
        let r: SteamPatchResult = call(&demo, Request::SteamUnpatch { app_id: 298110, steam_user: None });
        assert!(r.unchanged);
        let s: SteamGames = call(&demo, Request::SteamGames { steam_user: None });
        assert_eq!(state(&s, 298110), PatchState::Off);
        // Anno 1800's product isn't known.
        assert!(demo.handle(Request::SteamPatch { app_id: 916440, steam_user: None }).is_err());
        let u: SteamUsers = call(&demo, Request::SteamUsers);
        assert!(u.staged.iter().any(|s| s.contains("Watch Dogs: Legion")));
        demo.restart_steam();
        let s: SteamGames = call(&demo, Request::SteamGames { steam_user: None });
        assert_eq!(state(&s, 2239550), PatchState::On);
        let v: Value = call(&demo, Request::SteamOffers { enabled: Some(false) });
        assert_eq!(v["enabled"], false);
    }
}
