//! Sight in lab instances (`quiver_core::lab::sight`, design/LAB.md §12): the container's
//! mounts at `up`, the session dir, layer manifests, game environment and the run's
//! `quiver-sight` at each launch, killing it with the run, and `quiver lab sight` / the
//! `quiver lab logs` summary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use quiver_core::lab::manifest::Manifest;
use quiver_core::lab::sight::{self as labsight, HostSight, LAYER_DIR, LabSightStatus};
use quiver_core::lab::{Inst, procs};
use quiver_perf::session::{FileKind, SessionDir, SessionId, open_ring_reader};
use serde::{Deserialize, Serialize};

/// Written at `up` when the container got Sight's mounts.
const MARKER: &str = "sight-up.json";
pub const STATUS_FILE: &str = "sight.json";
pub const LOG_FILE: &str = "sight.log";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SightUp {
    pub lib_dir: PathBuf,
    pub binary: PathBuf,
    pub shm: PathBuf,
    pub i686: bool,
}

fn uid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { libc::getuid() }
}

/// The instance's shared-memory root (host tmpfs; the same path in the container).
pub fn shm_root(inst: &Inst) -> PathBuf {
    let base = std::env::var_os("QUIVER_LAB_SHM_BASE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/dev/shm"));
    labsight::shm_root(&base, uid(), &inst.name)
}

fn private_dir(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// At `up`: for a `sight = true` instance, what the container mounts (and the marker launches
/// check); otherwise nothing, and leftovers of an earlier Sight container go.
pub fn up(inst: &Inst, m: &Manifest) -> Result<Option<(HostSight, PathBuf)>> {
    let shm = shm_root(inst);
    if !m.sight {
        let _ = std::fs::remove_file(inst.dir.join(MARKER));
        let _ = std::fs::remove_dir_all(&shm);
        return Ok(None);
    }
    let h = HostSight::discover().map_err(|e| anyhow::anyhow!("sight = on, but {e}"))?;
    if let Some(parent) = shm.parent() {
        private_dir(parent)?;
    }
    let _ = std::fs::remove_dir_all(&shm);
    private_dir(&shm)?;
    let marker = SightUp { lib_dir: h.lib_dir.clone(), binary: h.binary.clone(), shm: shm.clone(), i686: h.i686 };
    quiver_core::lab::write_atomic(&inst.dir.join(MARKER), &serde_json::to_vec_pretty(&marker)?)?;
    Ok(Some((h, shm)))
}

/// At `down` and `rm`: the session dirs go with the container.
pub fn down(inst: &Inst) {
    let _ = std::fs::remove_dir_all(shm_root(inst));
    let _ = std::fs::remove_file(inst.dir.join(MARKER));
}

fn marker(inst: &Inst) -> Option<SightUp> {
    serde_json::from_slice(&std::fs::read(inst.dir.join(MARKER)).ok()?).ok()
}

/// What a launch adds for Sight.
pub struct Prepared {
    pub env: Vec<(String, String)>,
    /// Starts the run's `quiver-sight` (through `podman exec -d`).
    pub cmd: Vec<String>,
    pub notes: Vec<String>,
}

/// At each launch of a `sight = true` instance: a fresh session dir with `hud.image`, the
/// layer's manifests in the lab folder, the game's environment, the `quiver-sight` command.
pub fn prepare(inst: &Inst, m: &Manifest, session: &str, run_mount: &str) -> Result<Option<Prepared>> {
    if !m.sight {
        return Ok(None);
    }
    let Some(up) = marker(inst) else {
        bail!("sight = on, but the container was started without Sight: quiver lab down {0} && quiver lab up {0}", inst.name);
    };
    // The last runs' session dirs (their games are gone: the kill came first).
    let _ = quiver_perf::session::sweep_stale(&up.shm, |_| false);
    let id = SessionId::parse(session).with_context(|| format!("session id {session}"))?;
    let dir = SessionDir::create(&up.shm, &id).context("the run's Sight session dir")?;
    dir.create_hud_image(quiver_perf::hud::buffer_bytes_for(1024, 1000).unwrap_or(quiver_perf::layout::HUD_MAX_BUFFER_BYTES)).context("hud.image")?;
    let layer_dir = inst.lab().join(LAYER_DIR);
    std::fs::create_dir_all(&layer_dir)?;
    for (name, json) in labsight::layer_manifests(up.i686) {
        quiver_core::lab::write_atomic(&layer_dir.join(name), json.as_bytes())?;
    }
    // The launch's env as the game gets it: an HDR instance's (gamescope's WSI layer, which the
    // layer order puts below Sight), then the manifest's.
    let mut game_env = m.env.clone();
    if m.display.hdr {
        for (k, v) in quiver_core::lab::hdr::launch_env() {
            game_env.entry(k).or_insert(v);
        }
    }
    let env = labsight::launch_env(dir.path(), &game_env);
    let cmd = labsight::sight_cmd(dir.path(), &format!("{run_mount}/{STATUS_FILE}"), &format!("{run_mount}/{LOG_FILE}"), &m.title, &m.game.to_string());
    let mut notes = vec![format!(
        "Sight: the layer (HUD, the panel drawn in the game), the input preload{} and a lab quiver-sight (read-only: no quiverd); logs in {LOG_FILE} and game.log; quiver lab sight {} for its state",
        if up.i686 { "" } else { " (64-bit only: no i686 build installed)" },
        inst.name
    )];
    if !env.iter().any(|(k, _)| k == "LD_PRELOAD") {
        notes.push("Sight: QUIVER_SIGHT_INPUT=0 in the manifest's env: no input preload, the panel can't take the game's input".into());
    }
    Ok(Some(Prepared { env, cmd, notes }))
}

/// The run's `quiver-sight` goes with the run (kill-all).
pub fn kill(container_id: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        let all = procs::scan();
        let left: Vec<_> = procs::in_container(&all, container_id).into_iter().filter(|p| p.comm == "quiver-sight").collect();
        if left.is_empty() {
            break;
        }
        for p in &left {
            // Our own uid's process, in the instance's container.
            unsafe { libc::kill(p.pid as i32, sig) };
            if sig == libc::SIGKILL {
                notes.push(format!("killed quiver-sight (pid {})", p.pid));
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    notes
}

/// What `quiver lab logs` says about Sight: the presenter chosen, captures and releases (and
/// why), the layer drawing the panel, refused panel actions.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct LogSummary {
    pub presenter: Vec<String>,
    pub drawn: bool,
    pub captures: usize,
    pub releases: Vec<(String, usize)>,
    pub failsafes: usize,
    pub hooked: Vec<String>,
    pub refused: Vec<String>,
    pub errors: Vec<String>,
}

fn after<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    line.find(needle).map(|i| &line[i..])
}

/// From `game.log` (the preload's and layer's lines) and `sight.log`.
pub fn summarize(game_log: &str, sight_log: &str, status: Option<&LabSightStatus>) -> LogSummary {
    let mut s = LogSummary::default();
    // Every process of the prefix logs (the game, explorer, services): one line each per
    // panel, and their writes may interleave. So: by segment, grouped by the panel's epoch,
    // the most telling reason per panel.
    let mut captured: Vec<String> = Vec::new();
    let mut released: Vec<(String, String)> = Vec::new();
    for l in game_log.lines() {
        for seg in l.split("quiver-sight-input[").skip(1) {
            let msg = seg.split_once("]: ").map_or("", |(_, m)| m).trim();
            if let Some(e) = msg.strip_prefix("capturing input for panel ") {
                if !captured.iter().any(|c| c == e) {
                    captured.push(e.to_owned());
                }
            } else if let Some(r) = msg.strip_prefix("input back to the game (panel ") {
                let (epoch, why) = r.split_once("): ").unwrap_or((r, "released"));
                let rank = |w: &str| if w.starts_with("failsafe") { 2 } else if w == "released" { 0 } else { 1 };
                match released.iter_mut().find(|(e, _)| e == epoch) {
                    Some((_, w)) if rank(why) > rank(w) => *w = why.to_owned(),
                    Some(_) => {}
                    None => released.push((epoch.to_owned(), why.to_owned())),
                }
            } else if msg.contains("input interposed") && !s.hooked.iter().any(|h| h == msg) {
                s.hooked.push(msg.to_owned());
            }
        }
    }
    s.captures = captured.len();
    for (_, why) in released {
        if why.starts_with("failsafe") {
            s.failsafes += 1;
        }
        match s.releases.iter_mut().find(|(w, _)| *w == why) {
            Some((_, n)) => *n += 1,
            None => s.releases.push((why, 1)),
        }
    }
    for l in sight_log.lines() {
        if let Some(r) = after(l, "panel opened in the game").or_else(|| after(l, "panel on the desktop")).or_else(|| after(l, "panel: moving to the desktop")) {
            s.presenter.push(r.to_owned());
        }
        if l.contains("the Sight layer draws the panel in the game") {
            s.drawn = true;
        }
        if (l.contains(" ERROR ") || l.contains(" WARN ")) && s.errors.len() < 10 {
            s.errors.push(l.trim().to_owned());
        }
    }
    if let Some(st) = status {
        s.refused = st.refused.clone();
    }
    s
}

pub fn read_status(run_dir: &Path) -> Option<LabSightStatus> {
    serde_json::from_slice(&std::fs::read(run_dir.join(STATUS_FILE)).ok()?).ok()
}

/// Prints the summary (`quiver lab logs`).
pub fn print_summary(run_dir: &Path) {
    let game = std::fs::read_to_string(run_dir.join("game.log")).unwrap_or_default();
    let Ok(sight) = std::fs::read_to_string(run_dir.join(LOG_FILE)) else { return };
    let st = read_status(run_dir);
    let s = summarize(&game, &sight, st.as_ref());
    println!("Sight ({LOG_FILE}, game.log):");
    println!("  input hooked: {}", if s.hooked.is_empty() { "nothing yet (the preload hooks on the game's first X11 or Wayland input call)".into() } else { s.hooked.join("; ") });
    for p in &s.presenter {
        println!("  {p}");
    }
    if s.drawn {
        println!("  the layer drew the panel in the game");
    }
    println!("  input captured {} time(s); given back: {}", s.captures, if s.releases.is_empty() { "-".into() } else { s.releases.iter().map(|(w, n)| format!("{w} (x{n})")).collect::<Vec<_>>().join(", ") });
    if s.failsafes > 0 {
        println!("  failsafe releases: {}", s.failsafes);
    }
    for r in &s.refused {
        println!("  refused (read-only lab): {r}");
    }
    for e in &s.errors {
        println!("  {e}");
    }
}

/// `quiver lab sight N status`.
/// The run's session dir on the host (in the instance's shared-memory root), when the
/// container has Sight mounted and the run made one.
pub fn session_dir(inst: &Inst, run_dir: Option<&Path>) -> Option<PathBuf> {
    let u = marker(inst)?;
    let id = run_dir.and_then(|d| d.file_name()).and_then(|n| n.to_str()).and_then(|r| r.strip_prefix('r')).and_then(|r| r.parse().ok()).map(|run| quiver_api::lab::session_id(&inst.name, run))?;
    let p = u.shm.join(format!("quiver-{}", uid())).join(id);
    p.is_dir().then_some(p)
}

/// The newest layer ring's status block in a session dir.
/// The run's stereo layout as the lab quiver-sight resolves it: a live mod's word through the
/// instance's mod SDK endpoint (in the session dir), else the launch's setting as the layer
/// reports it (`QUIVER_SIGHT_STEREO` in the instance's env).
pub fn lab_stereo(s: &quiver_perf::LayerStatus, session_dir: &Path) -> quiver_api::perf::StereoInfo {
    let word = quiver_api::lab::SdkStereo::read(session_dir);
    quiver_api::perf::StereoInfo::resolve(word.as_ref().and_then(|w| w.word()), &quiver_api::perf::LayerStereo::from_status(s))
}

pub fn layer_status(dir: &Path) -> Option<quiver_perf::LayerStatus> {
    let sd = SessionDir::open(dir).ok()?;
    let ring = sd.list().ok()?.into_iter().filter(|(k, _)| matches!(k, FileKind::Layer { .. })).filter_map(|(_, p)| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p))).max()?.1;
    open_ring_reader(&ring).ok()?.layer_status()
}

pub fn status(inst: &Inst, run_dir: Option<&Path>, json: bool) -> Result<()> {
    let m = inst.load()?;
    let up = marker(inst);
    let st = run_dir.and_then(read_status);
    let dir = session_dir(inst, run_dir);
    let panel = dir.as_ref().and_then(|d| quiver_perf::panel::open_panel_input(&d.join(quiver_perf::panel::PANEL_INPUT_FILE), false).ok());
    let now = quiver_perf::clock::mono_raw_ns();
    let layer_status = dir.as_deref().and_then(layer_status);
    let alive = st.as_ref().is_some_and(|s| s.at.is_some_and(|t| (chrono::Utc::now() - t).num_seconds() < 5));
    if json {
        let p = panel.as_ref().map(|p| serde_json::json!({ "control": format!("{:?}", p.control()), "capturing": p.capture(now), "input": format!("{:?}", p.input()), "layer": format!("{:?}", p.layer()), "cursor": p.cursor() }));
        let stereo = layer_status.as_ref().zip(dir.as_deref()).map(|(s, d)| lab_stereo(s, d));
        let sdk = run_dir.and_then(super::sdk::read_status);
        return super::print_json(&serde_json::json!({ "sight": m.sight, "mounted": up.is_some(), "session_dir": dir, "quiver_sight": st, "quiver_sight_alive": alive, "panel_input": p, "stereo": stereo, "mod_sdk": sdk, "layer": layer_status.map(|s| serde_json::json!({ "extent": [s.extent_w, s.extent_h], "presents": s.presents, "flags": s.flags, "keys": s.keys, "key_panels": s.key_panels, "key_toggles": s.key_toggles, "stereo_setting": s.stereo_setting, "stereo_drawn": s.stereo_drawn })) }));
    }
    if !m.sight {
        println!("sight: off (quiver lab edit {0} sight=on, then quiver lab down {0} && quiver lab up {0})", inst.name);
        return Ok(());
    }
    println!("sight: on{}", if up.is_some() { "" } else { " — but the container was started without it (quiver lab down/up)" });
    let Some(dir) = dir else {
        println!("no Sight session for this run (launch it: quiver lab launch {})", inst.name);
        return Ok(());
    };
    println!("session dir: {}", dir.display());
    match &st {
        Some(s) => {
            println!(
                "quiver-sight: pid {} ({}), HUD {} ({}), {} fps",
                s.pid,
                if alive { "alive" } else { "no status for 5 s: gone or hung" },
                if s.hud_on { "shown" } else { "hidden" },
                s.hud_layout,
                s.fps.map_or("-".into(), |f| format!("{f:.0}"))
            );
            if let Some(p) = &s.panel {
                println!("panel: {}{}", if p.open { "open" } else { "closed" }, p.session.as_deref().map(|x| format!(" over {x}")).unwrap_or_default());
                if !p.presenter.is_empty() {
                    let how = if p.presenter == "game" { "in the game" } else { "on the desktop" };
                    println!("drawn: {how}{}", if p.presenter_detail.is_empty() { String::new() } else { format!(" — {}", p.presenter_detail) });
                }
                println!("backdrop: {} — {}", p.backdrop.as_str(), p.backdrop_detail);
                if let Some(e) = &p.last_error {
                    println!("error: {e}");
                }
            }
            for r in &s.refused {
                println!("refused (read-only): {r}");
            }
            if let Some(ls) = &layer_status {
                let st = lab_stereo(ls, &dir);
                let eyes = match st.stereo() {
                    quiver_perf::stereo::Stereo::Mono => String::new(),
                    s => {
                        let (lw, lh) = s.logical_extent((ls.extent_w, ls.extent_h));
                        format!(" — the HUD and panel are laid out at {lw}x{lh} and drawn in each eye")
                    }
                };
                println!("stereo: {}{eyes}", st.line());
                if st.source == "default" {
                    println!(
                        "  (no mod said a layout through the mod SDK; or set it with quiver lab edit {} env.{}=sbs|hsbs|tab|htab, then relaunch)",
                        inst.name,
                        quiver_perf::stereo::ENV_STEREO
                    );
                }
            }
        }
        None => println!("quiver-sight: no status yet ({STATUS_FILE})"),
    }
    match &panel {
        Some(p) => {
            let c = p.control();
            let l = p.layer();
            let i = p.input();
            let ago = |ns: u64| if ns == 0 { "never".to_owned() } else { format!("{:.2} s ago", (now as u64).saturating_sub(ns) as f64 / 1e9) };
            println!(
                "capture: {} (panel {}, epoch {})",
                if p.capture(now).is_some() { "the panel has the game's keyboard and mouse" } else { "the game has its input" },
                if c.open() { "open" } else { "closed" },
                c.epoch()
            );
            println!(
                "preload: {} (pid {}, seen {}), forwarded {}, lost {}, held keys released to the game {}",
                if i.flags & quiver_perf::panel::INPUT_X11 != 0 { "X11" } else if i.flags & quiver_perf::panel::INPUT_WAYLAND != 0 { "Wayland" } else { "not hooked yet" },
                i.pid,
                ago(i.seen_ns),
                i.forwarded,
                i.lost,
                i.releases
            );
            println!("layer: {} {}x{} (seen {}), panel last drawn {}", if l.can_draw(now) { "can draw the panel on" } else { "can't draw the panel on" }, l.extent.0, l.extent.1, ago(l.seen_ns), ago(l.drawn_ns));
            if let Some(((x, y), _)) = p.cursor() {
                println!("cursor: {x},{y}");
            }
        }
        None => println!("panel.input: not made yet (quiver-sight makes it once the layer has a swapchain)"),
    }
    if let Some(s) = layer_status {
        println!("layer status: {} presents, swapchain {}x{}, keys {:#x} (Shift+F2 {}, Shift+F3 {})", s.presents, s.extent_w, s.extent_h, s.keys, s.key_panels, s.key_toggles);
    }
    match run_dir.and_then(super::sdk::read_status) {
        Some(conns) if !conns.is_empty() => {
            for c in conns {
                println!(
                    "mod SDK: {} connected (protocol {}, {}{}), stereo {} — the instance's read-only endpoint",
                    c.surface,
                    c.proto,
                    if c.sdk.is_empty() { "SDK" } else { &c.sdk },
                    c.module.as_deref().map(|m| format!(", {m}")).unwrap_or_default(),
                    c.stereo.as_deref().unwrap_or("not said")
                );
            }
        }
        _ => println!("mod SDK: no mod connected in this run"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_summary() {
        let game = "\
quiver-sight-input[40]: X11 input interposed
quiver-sight-input[40]: /dev/shm/x/panel.input mapped
quiver-sight-input[40]: capturing input for panel 7
quiver-sight-input[41]: capturing input for panel 7
quiver-sight-input[40]: input back to the game (panel 7): the panel closed
quiver-sight-input[40]: capturing input for panel 8
quiver-sight-input[41]: quiver-sight-input[40]: input back to the game (panel 8): failsafe: quiver-sight is gone
input back to the game (panel 8): released
quiver-sight-input[41]: input back to the game (panel 7): the panel closed
quiver-sight-layer: panel.input mapped
";
        let sight = "\
2026-11-19T10:00:00Z  INFO quiver_sight::panel::ingame: panel opened in the game session=\"lab-a-r1\" extent=(1280, 720)
2026-11-19T10:00:01Z  INFO quiver_sight::panel::ingame: the Sight layer draws the panel in the game after=120ms
2026-11-19T10:00:09Z  WARN quiver_sight::panel: panel: boom
";
        let st = LabSightStatus { refused: vec!["set tweak fps".into()], ..Default::default() };
        let s = summarize(game, sight, Some(&st));
        assert_eq!(s.captures, 2);
        assert_eq!(s.releases, vec![("the panel closed".into(), 1), ("failsafe: quiver-sight is gone".into(), 1)]);
        assert_eq!(s.failsafes, 1);
        assert_eq!(s.hooked, vec!["X11 input interposed".to_owned()]);
        assert!(s.drawn && s.presenter[0].starts_with("panel opened in the game"));
        assert_eq!(s.refused, ["set tweak fps"]);
        assert_eq!(s.errors.len(), 1);
    }
}
