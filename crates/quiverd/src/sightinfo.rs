//! The Sight panel's Session and Achievements sections (design/SIGHT.md §3.3, §3.7):
//! `SightRequest::{Session, Achievements, Icons, Screenshot}`.
//!
//! - **Session**: the session's Proton build (as its launch recorded it, else its processes'
//!   `STEAM_COMPAT_TOOL_PATHS`), its box art as files the app's art cache already holds (no
//!   download), and what happened while it ran: the activity log since it started, plus the
//!   finished downloads and achievement unlocks quiverd saw go by ([`start`] keeps the last
//!   of those; the activity log has no Ubisoft unlocks or Ubisoft downloads).
//! - **Achievements**: the game's achievements whichever store, with Ubisoft's report journal.
//!   Ubisoft's come from what Quiver already knows ([`Engine::status_offline`]): no read of
//!   Ubisoft's unlocks is started and nothing is sent from here.
//! - **Icons**: achievement icons as files: Epic's through its own icon cache, others into
//!   `~/.cache/quiver/achievement-icons/` (known image hosts only).
//! - **Screenshot**: asks the session's Sight layer for its next frame (`capture.req`, the
//!   capture `quiver lab shot --hdr` uses; launches have `QUIVER_SIGHT_CAPTURE=1`) and saves it
//!   as a PNG under `~/Pictures/Quiver/<game>/`.
//!
//! [`Engine::status_offline`]: quiver_core::achievements::Engine::status_offline

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::art::{ArtPreference, ArtRef, ArtSlot};
use quiver_api::sight::SightRequest;
use quiver_api::sight::session::{MAX_EVENTS, Reporting, Screenshot, SessionAchievements, SessionEvent, SessionEventKind, SessionInfo, captures_dir, session_events};
use quiver_api::{ActiveSession, Event, GameRef, JobKind, StoreId};
use serde_json::{Value, json};

use crate::Daemon;

/// Downloads and unlocks quiverd saw go by (with the game they're about), newest last.
static SEEN: Mutex<VecDeque<(Option<GameRef>, SessionEvent)>> = Mutex::new(VecDeque::new());
const SEEN_KEEP: usize = 200;

fn remember(game: Option<GameRef>, e: SessionEvent) {
    let mut s = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    s.push_back((game, e));
    while s.len() > SEEN_KEEP {
        s.pop_front();
    }
}

/// An event worth keeping for the panel, if it is one. `finished` holds the jobs already
/// counted (a job's last update can come more than once).
fn notable(e: &Event, finished: &mut BTreeSet<u64>) -> Option<(Option<GameRef>, SessionEvent)> {
    match e {
        Event::Job(j) if j.done && j.error.is_none() && j.kind == JobKind::Install && finished.insert(j.id) => {
            let what = if j.status.starts_with("Updated") { "Update finished" } else { "Download finished" };
            Some((Some(j.game), SessionEvent { at: Utc::now(), kind: SessionEventKind::Download, text: format!("{what}: {}", j.name), by: None }))
        }
        Event::AchievementUnlocked { product_id, game, name, key, achievement_id, unlocked_at, .. } => {
            let g = game.unwrap_or(GameRef::ubisoft(*product_id));
            let name = name.clone().filter(|n| !n.trim().is_empty()).or_else(|| key.clone()).unwrap_or_else(|| format!("achievement {achievement_id}"));
            Some((Some(g), SessionEvent { at: *unlocked_at, kind: SessionEventKind::Achievement, text: format!("Achievement unlocked: {name}"), by: None }))
        }
        _ => None,
    }
}

/// Follows quiverd's events for [`SEEN`].
pub(crate) fn start(d: &Arc<Daemon>) {
    let mut rx = d.events.subscribe();
    tokio::spawn(async move {
        let mut finished = BTreeSet::new();
        loop {
            match rx.recv().await {
                Ok(e) => {
                    if let Some((g, ev)) = notable(&e, &mut finished) {
                        remember(g, ev);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// The session's events: the activity log's and [`SEEN`]'s, the same line within two minutes
/// once, oldest first, at most [`MAX_EVENTS`].
pub fn merge_events(mut a: Vec<SessionEvent>, seen: &[(Option<GameRef>, SessionEvent)], game: GameRef, started: DateTime<Utc>) -> Vec<SessionEvent> {
    for (g, e) in seen {
        let ours = e.kind == SessionEventKind::Download || g.is_none_or(|g| g == game);
        if e.at >= started && ours && !a.iter().any(|x| x.kind == e.kind && x.text.eq_ignore_ascii_case(&e.text) && (x.at - e.at).num_seconds().abs() < 120) {
            a.push(e.clone());
        }
    }
    a.sort_by_key(|e| e.at);
    let n = a.len();
    a.drain(..n.saturating_sub(MAX_EVENTS));
    a
}

pub(crate) async fn handle(d: &Arc<Daemon>, source: &str, req: SightRequest) -> Result<Value> {
    Ok(match req {
        SightRequest::Session { session } => json!(session_info(d, &session).await?),
        SightRequest::Achievements { game } => json!(achievements(d, game).await),
        SightRequest::Icons { urls } => json!(icons(d, urls).await),
        SightRequest::Screenshot { session } => json!(screenshot(d, source, session.as_deref()).await?),
        other => d.handle_sight(source, other)?,
    })
}

/// The running session `id`, from every launch path.
fn active(d: &Daemon, id: &str) -> Option<ActiveSession> {
    d.sessions.list().into_iter().map(|s| d.options.detail(s)).chain(d.local.list()).find(|s| s.session.as_deref() == Some(id))
}

async fn session_info(d: &Arc<Daemon>, id: &str) -> Result<SessionInfo> {
    let a = active(d, id);
    let perf = d.perf.sessions().into_iter().find(|s| s.session == id);
    let (game, started, pids, steam) = match (&a, &perf) {
        (Some(a), _) => (a.game_ref(), a.started, a.pids.clone(), a.steam_app_id.is_some()),
        (None, Some(p)) => (p.game_ref(), p.started, Vec::new(), false),
        (None, None) => bail!("session {id} isn't running"),
    };
    let mut info = SessionInfo { session: id.into(), game: Some(game), started: Some(started), ..Default::default() };
    // Steam runs its own copies with its own Proton: their processes say which.
    info.proton = if steam || game.as_steam().is_some() { None } else { crate::mods::installed(game).ok().and_then(|g| quiver_core::launcher::session_proton(&g)) };
    if info.proton.is_none() {
        info.proton = pids.iter().find_map(|p| proton_of_process(*p));
    }
    let pref = ArtPreference::from_setting(quiver_core::settings::current(game).ok().and_then(|(s, _)| s.art).as_deref());
    if let Ok(lib) = d.library_all(false).await
        && let Some(e) = lib.iter().find(|e| e.game == game).or_else(|| lib.iter().find(|e| e.records.iter().any(|r| r.game == game)))
    {
        info.art = quiver_api::art::candidates(e, ArtSlot::Tile, pref).into_iter().map(|(_, r)| r).collect();
        let cache = crate::info::cache_dir().join("art");
        if let Some((p, tile)) = info.art.iter().find_map(|r| cached_art(&cache, r)) {
            info.art_file = Some(p);
            info.art_tile = tile;
        }
    }
    let log: Vec<quiver_api::ActivityEntry> = d.activity.lock().unwrap().iter().cloned().collect();
    let seen: Vec<_> = SEEN.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect();
    info.events = merge_events(session_events(&log, game, started), &seen, game, started);
    info.capture_off = match d.perf.capture_target(Some(id)) {
        Ok(t) => capture_off(t.status.as_ref()),
        Err(e) => Some(format!("{e:#}")),
    };
    Ok(info)
}

/// An art reference as a file the app's art cache (`~/.cache/quiver/art`, quiver-app's
/// `art.rs`) already holds, preferring its box-art crop (`derived/<stem>.tile.jpg`); the
/// bool says it's the crop. Nothing is downloaded.
pub fn cached_art(cache: &Path, r: &ArtRef) -> Option<(PathBuf, bool)> {
    let (original, stem) = match r {
        ArtRef::Asset(file) => {
            let (stem, ext) = file.rsplit_once('.')?;
            if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || !matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp") {
                return None;
            }
            (Some(cache.join(file)), stem.to_owned())
        }
        ArtRef::Url(url) => {
            let rest = url.strip_prefix("https://")?;
            let (host, path) = rest.split_once('/')?;
            if path.is_empty() || path.contains(['?', '#', '\\']) || path.split('/').any(|s| s == "..") || path.len() > 1024 {
                return None;
            }
            let base = PathBuf::from("url").join(host.replace(':', "_"));
            let segs: Vec<&str> = path.split('/').collect();
            let ok = |s: &&str| !s.is_empty() && s.len() <= 128 && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            let ext = segs.last().and_then(|l| l.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
            if segs.len() <= 8 && segs.iter().all(ok) && ext.as_deref().is_some_and(|e| matches!(e, "jpg" | "jpeg" | "png" | "webp")) {
                let mut rel = base;
                rel.extend(&segs);
                let flat: Vec<String> = rel.iter().skip(1).map(|s| s.to_string_lossy().into_owned()).collect();
                let flat = flat.join("_");
                (Some(cache.join(&rel)), format!("url-{}", flat.rsplit_once('.').map(|(s, _)| s).unwrap_or(&flat)))
            } else {
                use sha1::Digest as _;
                let hash = hex::encode(sha1::Sha1::digest(url.as_bytes()));
                let stem = cache.join(&base).join("h").join(&hash);
                (["jpg", "png", "webp"].iter().map(|e| stem.with_extension(e)).find(|p| p.is_file()), format!("url-h-{hash}"))
            }
        }
        // Steam's library cache: the file itself (the app's crop of it is keyed on a hash this
        // process can't reproduce reliably).
        ArtRef::File(p) => return p.is_file().then(|| (p.clone(), false)),
    };
    let tile = cache.join("derived").join(format!("{stem}.tile.jpg"));
    if tile.is_file() {
        return Some((tile, true));
    }
    original.filter(|p| p.is_file()).map(|p| (p, false))
}

/// Why a session's layer can't take a screenshot (`None`: it can).
fn capture_off(s: Option<&quiver_perf::LayerStatus>) -> Option<String> {
    match s {
        None => Some("the game has no Sight layer drawing (OpenGL, or the layer is off for it)".into()),
        Some(s) if s.hdr_flags & quiver_perf::layout::HDR_CAPTURE_READY == 0 => {
            Some("the game's swapchain isn't readable: it started before screenshots were on (restart it), or its format can't be captured".into())
        }
        Some(_) => None,
    }
}

/// The Proton build a process runs on, from `STEAM_COMPAT_TOOL_PATHS` (Steam's launches) in
/// its environment.
fn proton_of_process(pid: u32) -> Option<String> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    proton_from_environ(&env)
}

pub fn proton_from_environ(env: &[u8]) -> Option<String> {
    env.split(|b| *b == 0).find_map(|kv| {
        let v = kv.strip_prefix(b"STEAM_COMPAT_TOOL_PATHS=")?;
        let first = String::from_utf8_lossy(v).split(':').next()?.trim_end_matches('/').to_owned();
        Path::new(&first).file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
    })
}

// ---- Achievements ----------------------------------------------------------------------------

async fn achievements(d: &Arc<Daemon>, game: GameRef) -> SessionAchievements {
    let mut out = SessionAchievements { game: Some(game), ..Default::default() };
    match game.store {
        StoreId::Ubisoft => {
            let Some(product_id) = game.as_ubisoft() else { return out };
            let mode = quiver_core::settings::current(product_id).ok().and_then(|(s, _)| s.achievements);
            out.reporting = Reporting { store: Some(StoreId::Ubisoft), mode: mode.unwrap_or_else(|| "on".into()) };
            let account = match d.account_for(product_id, None).await {
                Ok(o) => o.user_id().map(str::to_owned),
                Err(e) => {
                    out.errors.push(format!("{e:#}"));
                    None
                }
            };
            match d.achievements.status_offline(account.as_deref(), product_id, "en-US").await {
                Ok(s) => {
                    out.errors.extend(s.errors);
                    if let Some(a) = s.achievements {
                        if !a.unlocked_known && a.unlocked == 0 {
                            out.errors.push("Quiver hasn't read this account's unlocks yet (the game's page in Quiver does)".into());
                        }
                        out.reports = a.items.iter().filter_map(|i| Some((i.id.clone(), i.report?))).collect();
                        out.achievements = Some(ubisoft_list(a));
                    }
                }
                Err(e) => out.errors.push(format!("{e:#}")),
            }
        }
        _ => {
            out.reporting = Reporting { store: Some(game.store), mode: if game.store == StoreId::Steam { "client".into() } else { "by_game".into() } };
            match d.game_detail(game, false).await {
                Ok(g) => out.achievements = g.achievements,
                Err(e) => out.errors.push(format!("{e:#}")),
            }
        }
    }
    out
}

/// Ubisoft's set as the panel lists every store's.
pub fn ubisoft_list(a: quiver_api::info::Achievements) -> quiver_api::stores::Achievements {
    let list: Vec<quiver_api::stores::Achievement> = a
        .items
        .into_iter()
        .map(|i| quiver_api::stores::Achievement {
            name: i.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| i.id.clone()),
            id: i.id,
            description: i.description.unwrap_or_default(),
            hidden: false,
            unlocked_at: i.unlocked_at,
            unlocked: i.unlocked,
            icon: i.image,
            icon_gray: None,
            progress: None,
        })
        .collect();
    quiver_api::stores::Achievements { unlocked: list.iter().filter(|x| x.unlocked).count(), total: (a.total as usize).max(list.len()), list }
}

// ---- Icons -----------------------------------------------------------------------------------

/// Hosts icons are fetched from (Steam's community CDN, Ubisoft's, Epic's).
const ICON_HOSTS: &[&str] = &[
    "cdn.steamstatic.com",
    "cdn.akamai.steamstatic.com",
    "cdn.cloudflare.steamstatic.com",
    "shared.steamstatic.com",
    "steamcdn-a.akamaihd.net",
    "ubistatic3-a.akamaihd.net",
    "ubistatic-a.akamaihd.net",
    "static-dm.akamaized.net",
    "ubiservices.cdn.ubi.com",
    "static-live.ubisoft.com",
    "cdn1.epicgames.com",
    "cdn2.epicgames.com",
    "cdn2.unrealengine.com",
];
const MAX_ICONS: usize = 200;

fn icon_host_ok(url: &str) -> bool {
    url.strip_prefix("https://").and_then(|r| r.split('/').next()).is_some_and(|h| ICON_HOSTS.contains(&h) || h.ends_with(".epicgames.com"))
}

/// Where an icon URL is cached: `<dir>/<sha1 of the URL>.<ext>`.
pub fn icon_path(dir: &Path, url: &str) -> PathBuf {
    use sha1::Digest as _;
    let ext = url.rsplit('.').next().map(str::to_ascii_lowercase).filter(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp")).unwrap_or_else(|| "png".into());
    dir.join(format!("{}.{ext}", hex::encode(sha1::Sha1::digest(url.as_bytes()))))
}

async fn icons(d: &Arc<Daemon>, urls: Vec<String>) -> Vec<Option<PathBuf>> {
    let dir = crate::info::cache_dir().join("achievement-icons");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok();
    let limit = Arc::new(tokio::sync::Semaphore::new(6));
    let mut tasks = Vec::new();
    for (i, url) in urls.into_iter().enumerate() {
        if i >= MAX_ICONS || !url.starts_with("https://") {
            tasks.push(None);
            continue;
        }
        let (d, dir, http, limit) = (d.clone(), dir.clone(), http.clone(), limit.clone());
        tasks.push(Some(tokio::spawn(async move {
            if url.contains("epicgames.com") || url.contains("unrealengine.com") {
                if let Some(p) = d.epic.icon_file(&url).await {
                    return Some(p);
                }
            }
            let path = icon_path(&dir, &url);
            if path.is_file() {
                return Some(path);
            }
            if !icon_host_ok(&url) {
                return None;
            }
            let _permit = limit.acquire().await.ok()?;
            let rsp = http?.get(&url).send().await.ok()?.error_for_status().ok()?;
            let bytes = rsp.bytes().await.ok()?;
            if bytes.len() > 4 << 20 {
                return None;
            }
            std::fs::create_dir_all(&dir).ok()?;
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(&tmp, &bytes).ok()?;
            std::fs::rename(&tmp, &path).ok()?;
            Some(path)
        })));
    }
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(match t {
            Some(t) => t.await.ok().flatten(),
            None => None,
        });
    }
    out
}

// ---- Screenshots ----------------------------------------------------------------------------

/// `~/Pictures` (`XDG_PICTURES_DIR`, else `user-dirs.dirs`'s, else `$HOME/Pictures`).
fn pictures_dir() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if let Some(p) = std::env::var_os("XDG_PICTURES_DIR").map(PathBuf::from).filter(|p| p.is_absolute()) {
        return p;
    }
    let cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    if let Ok(text) = std::fs::read_to_string(cfg.join("user-dirs.dirs"))
        && let Some(p) = user_dirs_pictures(&text, &home)
    {
        return p;
    }
    home.join("Pictures")
}

pub fn user_dirs_pictures(text: &str, home: &Path) -> Option<PathBuf> {
    let line = text.lines().map(str::trim).find(|l| l.starts_with("XDG_PICTURES_DIR="))?;
    let v = line.trim_start_matches("XDG_PICTURES_DIR=").trim_matches('"');
    let p = match v.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(v),
    };
    (p.is_absolute() && p != home).then_some(p)
}

/// A new file name in `dir`: `<game> 2026-11-19 21-14-03.png` (`-2`, `-3` … if taken).
fn shot_path(dir: &Path, name: &str, at: DateTime<chrono::Local>) -> PathBuf {
    let stem = format!("{} {}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.to_owned()), at.format("%Y-%m-%d %H-%M-%S"));
    let mut p = dir.join(format!("{stem}.png"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem}-{n}.png"));
        n += 1;
    }
    p
}

/// The newest capture-ready status among the session dir's layer rings (each process of
/// the game that presents has one).
fn layer_statuses(dir: &Path) -> Vec<quiver_perf::LayerStatus> {
    use quiver_perf::session::{FileKind, SessionDir, open_ring_reader};
    let Ok(sd) = SessionDir::open(dir) else { return Vec::new() };
    sd.list().unwrap_or_default().into_iter().filter(|(k, _)| matches!(k, FileKind::Layer { .. })).filter_map(|(_, p)| open_ring_reader(&p).ok()?.layer_status()).collect()
}

async fn screenshot(d: &Arc<Daemon>, source: &str, session: Option<&str>) -> Result<Screenshot> {
    let t = d.perf.capture_target(session)?;
    if let Some(why) = capture_off(t.status.as_ref()) {
        bail!("can't take a screenshot: {why}");
    }
    let dir = t.dir.clone();
    let req = dir.join("capture.req");
    let prev = std::fs::read_to_string(&req).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let done = layer_statuses(&dir).iter().map(|s| s.capture_done).max().unwrap_or(0);
    let n = prev.max(done) + 1;
    std::fs::write(&req, format!("{n}\n")).with_context(|| req.display().to_string())?;
    // The layer's heartbeat (250 ms) sees it, the next present copies the frame, a later one
    // reads it back, the heartbeat writes it out.
    let end = Instant::now() + Duration::from_secs(6);
    loop {
        if layer_statuses(&dir).iter().any(|s| s.capture_done >= n) {
            break;
        }
        if Instant::now() > end {
            bail!("the game didn't hand over a frame within 6 s (is it presenting? a paused or minimised game may not)");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (raw_src, meta_src) = (dir.join(format!("capture-{n}.raw")), dir.join(format!("capture-{n}.json")));
    let meta: quiver_core::lab::hdr::CaptureMeta = serde_json::from_slice(&std::fs::read(&meta_src).with_context(|| meta_src.display().to_string())?).context("the capture's sidecar")?;
    let raw = std::fs::read(&raw_src).with_context(|| raw_src.display().to_string())?;
    // The session dir is tmpfs: don't keep full frames there.
    let _ = std::fs::remove_file(&raw_src);
    let _ = std::fs::remove_file(&meta_src);
    let name = d.game_name(t.game).await;
    let out_dir = captures_dir(&pictures_dir(), &name);
    std::fs::create_dir_all(&out_dir).with_context(|| out_dir.display().to_string())?;
    let path = shot_path(&out_dir, &name, chrono::Local::now());
    let p = path.clone();
    let (width, height) = (meta.width, meta.height);
    let hdr = tokio::task::spawn_blocking(move || quiver_core::lab::hdr::save_png(&raw, &meta, &p)).await??;
    let shot = Screenshot { width, height, hdr, path };
    d.log_activity_game(source, format!("screenshot of {name}: {}", shot.path.display()), Some(t.game));
    Ok(shot)
}

/// What a screenshot needs from the perf hub.
pub struct CaptureTarget {
    pub game: GameRef,
    pub dir: PathBuf,
    /// The status of the layer that draws on the game's largest swapchain.
    pub status: Option<quiver_perf::LayerStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proton_from_a_steam_environment() {
        let env = b"HOME=/h\0STEAM_COMPAT_TOOL_PATHS=/home/u/.steam/root/compatibilitytools.d/GE-Proton10-25:/s/steamapps/common/SteamLinuxRuntime_sniper\0X=1\0";
        assert_eq!(proton_from_environ(env).as_deref(), Some("GE-Proton10-25"));
        assert_eq!(proton_from_environ(b"STEAM_COMPAT_TOOL_PATHS=/s/common/Proton - Experimental/\0").as_deref(), Some("Proton - Experimental"));
        assert_eq!(proton_from_environ(b"HOME=/h\0"), None);
    }

    #[test]
    fn events_merge_without_doubles() {
        let g = GameRef::ubisoft(3539);
        let started: DateTime<Utc> = "2026-11-19T20:00:00Z".parse().unwrap();
        let at = |m: i64| started + chrono::Duration::minutes(m);
        let ev = |m: i64, kind, text: &str| SessionEvent { at: at(m), kind, text: text.into(), by: None };
        let log = vec![ev(40, SessionEventKind::Achievement, "Achievement unlocked: Hidden Ones")];
        let seen = vec![
            (Some(g), ev(41, SessionEventKind::Achievement, "Achievement unlocked: Hidden Ones")),
            (Some(GameRef::ubisoft(1)), ev(10, SessionEventKind::Download, "Download finished: Far Cry 5")),
            (Some(GameRef::ubisoft(1)), ev(12, SessionEventKind::Achievement, "Achievement unlocked: elsewhere")),
            (Some(g), ev(-5, SessionEventKind::Achievement, "Achievement unlocked: before")),
        ];
        let m = merge_events(log, &seen, g, started);
        let texts: Vec<&str> = m.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["Download finished: Far Cry 5", "Achievement unlocked: Hidden Ones"]);
    }

    #[test]
    fn notable_events() {
        let mut fin = BTreeSet::new();
        let job = quiver_api::Job {
            id: 7,
            kind: JobKind::Install,
            game: GameRef::ubisoft(1),
            name: "Far Cry 5".into(),
            status: "Installed Far Cry 5".into(),
            downloaded: 1,
            total: 1,
            done: true,
            error: None,
            detail: vec![],
        };
        let (_, e) = notable(&Event::Job(job.clone()), &mut fin).unwrap();
        assert_eq!(e.text, "Download finished: Far Cry 5");
        assert!(notable(&Event::Job(job.clone()), &mut fin).is_none(), "once per job");
        assert!(notable(&Event::Job(quiver_api::Job { id: 8, done: false, ..job.clone() }), &mut fin).is_none());
        assert!(notable(&Event::Job(quiver_api::Job { id: 9, error: Some("x".into()), ..job }), &mut fin).is_none());
    }

    #[test]
    fn pictures_from_user_dirs() {
        let home = Path::new("/home/u");
        assert_eq!(user_dirs_pictures("# x\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\n", home), Some(PathBuf::from("/home/u/Bilder")));
        assert_eq!(user_dirs_pictures("XDG_PICTURES_DIR=\"$HOME/\"\n", home), None, "the home folder itself isn't a pictures folder");
        assert_eq!(user_dirs_pictures("XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n", home), None);
    }

    #[test]
    fn art_from_the_apps_cache() {
        let t = tempfile::tempdir().unwrap();
        let c = t.path();
        let url = "https://ubiservices.cdn.ubi.com/abc/DEF/box.jpg";
        let asset = ArtRef::Asset("79b8e5e2.jpg".into());
        assert_eq!(cached_art(c, &asset), None, "nothing cached: nothing (no download)");
        std::fs::write(c.join("79b8e5e2.jpg"), b"x").unwrap();
        assert_eq!(cached_art(c, &asset), Some((c.join("79b8e5e2.jpg"), false)));
        std::fs::create_dir_all(c.join("derived")).unwrap();
        std::fs::write(c.join("derived/79b8e5e2.tile.jpg"), b"x").unwrap();
        assert_eq!(cached_art(c, &asset), Some((c.join("derived/79b8e5e2.tile.jpg"), true)), "the box-art crop first");
        let u = ArtRef::Url(url.into());
        std::fs::create_dir_all(c.join("url/ubiservices.cdn.ubi.com/abc/DEF")).unwrap();
        std::fs::write(c.join("url/ubiservices.cdn.ubi.com/abc/DEF/box.jpg"), b"x").unwrap();
        assert_eq!(cached_art(c, &u).unwrap().0, c.join("url/ubiservices.cdn.ubi.com/abc/DEF/box.jpg"));
        std::fs::write(c.join("derived/url-ubiservices.cdn.ubi.com_abc_DEF_box.tile.jpg"), b"x").unwrap();
        assert!(cached_art(c, &u).unwrap().1);
        // Epic's URLs (spaces, no extension) are cached under a hash.
        let e = "https://cdn1.epicgames.com/x/Alan Wake&x";
        use sha1::Digest as _;
        let h = hex::encode(sha1::Sha1::digest(e.as_bytes()));
        std::fs::create_dir_all(c.join("url/cdn1.epicgames.com/h")).unwrap();
        std::fs::write(c.join(format!("url/cdn1.epicgames.com/h/{h}.png")), b"x").unwrap();
        assert_eq!(cached_art(c, &ArtRef::Url(e.into())).unwrap().0, c.join(format!("url/cdn1.epicgames.com/h/{h}.png")));
        assert_eq!(cached_art(c, &ArtRef::Url("https://h/../x.jpg".into())), None);
    }

    #[test]
    fn icon_cache_names() {
        let p = icon_path(Path::new("/c"), "https://cdn.akamai.steamstatic.com/steamcommunity/public/images/apps/620/abc.jpg");
        assert!(p.to_string_lossy().ends_with(".jpg") && p.starts_with("/c"));
        assert!(icon_path(Path::new("/c"), "https://cdn1.epicgames.com/x/y?z").to_string_lossy().ends_with(".png"));
        assert!(icon_host_ok("https://cdn.akamai.steamstatic.com/a.jpg"));
        assert!(!icon_host_ok("https://evil.example/a.jpg"));
        assert!(!icon_host_ok("http://cdn.akamai.steamstatic.com/a.jpg"));
    }
}
