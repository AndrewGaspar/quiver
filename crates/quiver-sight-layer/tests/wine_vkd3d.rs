//! The layer under Proton with vkd3d-proton: a D3D12 program (tests/wine/d3d12_present.c)
//! renders a known pattern on a direct queue and presents through DXGI; vkd3d-proton's
//! presents reach `VK_LAYER_QUIVER_sight` as vkQueuePresentKHR. The window is on a private
//! Xvfb (never the desktop) and read back from it, so the test checks what reached the screen:
//!
//! 1. with no HUD image (the HUD hidden, as every launch starts), the game's frames must be
//!    on screen untouched;
//! 2. with a HUD image published, the HUD is drawn and the game's frames are still there
//!    around it;
//! 3. with the HUD toggled off (opacity 0) and then a Sight panel's backdrop request, the
//!    frames are still the game's (blurred while the request is live);
//! 4. after a `ResizeBuffers` (a new Vulkan swapchain with `oldSwapchain`), the same.
//!
//! Opt-in:
//!
//! `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 cargo test
//! -p quiver-sight-layer --test wine_vkd3d -- --ignored --nocapture`
//!
//! Needs `x86_64-w64-mingw32-gcc`, Xvfb and ImageMagick (`magick import`). Uses a scratch
//! prefix and HOME (the prefix is kept in `target/quiver-sight-wine-vkd3d` between runs).
//! `QUIVER_TEST_VALIDATION=1` puts the Khronos validation layer below Sight and prints what it
//! reports; `QUIVER_TEST_SIGHT_PNG=<dir>` saves the screens (PPM).
//!
//! `QUIVER_TEST_D3D12_FLAGS` picks what the program does (d3d12_present.c): 1 uncapped, 2
//! tearing (presents in IMMEDIATE through `VkSwapchainPresentModeInfoEXT`), 4 a 10-bit
//! swapchain, 8 vsync, 16 exclusive fullscreen at 640x480 for the second half.
//! `QUIVER_TEST_DISPLAY=:N` uses that X server instead of Xvfb, e.g. a headless gamescope's
//! Xwayland (DRI3, as on the desktop). Run that one in private mount and network namespaces
//! (`unshare -rmn`, a tmpfs on `/tmp`) so its sockets, abstract ones included, and lock files
//! can't take a display the desktop uses.

mod harness;
mod winescreen;
mod xvfb;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness::*;
use winescreen::*;
use quiver_perf::LayerStatus;
use quiver_perf::hud::BackdropRequest;
use quiver_perf::record::LayerPresent;
use quiver_perf::session::{FileKind, open_ring_reader};

/// Mostly `want` (the test's clear colours: red 0.8/0.1/0.1, blue 0.1/0.1/0.8, green band).
fn near(p: [u8; 3], want: [u8; 3]) -> bool {
    p.iter().zip(want).all(|(a, b)| (*a as i32 - b as i32).abs() <= 40)
}

const RED: [u8; 3] = [204, 26, 26];
/// The backdrop request's dim (1/1000): blurred frames are this much darker.
const DIM: u16 = 300;
const BLUE: [u8; 3] = [26, 26, 204];

/// The game's pattern is on screen: the left half red, the right half blue, sampled on rows
/// the HUD (top-left) doesn't cover, skipping the moving green band. Returns what was wrong.
fn pattern_ok(s: &Screen, blurred: bool) -> Result<(), String> {
    // The window: 1280x720, or the screen once fullscreen.
    let (w, h) = (s.w, s.h);
    let mut bad = Vec::new();
    let mut checked = 0;
    for &fy in &[0.55f32, 0.7, 0.85, 0.95] {
        for &fx in &[0.05f32, 0.2, 0.4, 0.6, 0.8, 0.95] {
            let (x, y) = ((fx * w as f32) as usize, (fy * h as f32) as usize);
            let p = s.px(x, y);
            // The band is green: skip it.
            if p[1] > 120 && p[0] < 120 && p[2] < 120 {
                continue;
            }
            // Blurred, the band spreads: skip anything with some of its green.
            if blurred && p[1] > 40 {
                continue;
            }
            // A blur mixes colours near the middle and the band: only check far from them.
            if blurred && (0.3..0.7).contains(&fx) {
                continue;
            }
            checked += 1;
            let mut want = if fx < 0.5 { RED } else { BLUE };
            if blurred {
                want = want.map(|c| (c as u32 * (1000 - DIM as u32) / 1000) as u8);
            }
            if !near(p, want) {
                bad.push(format!("({x},{y}) = {p:?}, want ~{want:?}"));
            }
        }
    }
    if checked < 6 {
        return Err(format!("only {checked} points to check"));
    }
    if bad.len() > 2 { Err(bad.join("; ")) } else { Ok(()) }
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, Xvfb and ImageMagick; see the module docs"]
fn vkd3d_presents_through_the_layer() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    // A private Xvfb, or an X server the caller made (QUIVER_TEST_DISPLAY: e.g. a headless
    // gamescope's Xwayland, which has DRI3 like the desktop's).
    let (_x, display) = match std::env::var("QUIVER_TEST_DISPLAY") {
        Ok(d) => (None, d),
        Err(_) => {
            let x = xvfb::Xvfb::start_size("1920x1080x24").expect("Xvfb");
            let d = x.display.clone();
            (Some(x), d)
        }
    };
    let scratch = tempfile::tempdir().unwrap();
    let t = scratch.path();
    let exe = build_exe(t);
    // `runinprefix` doesn't install vkd3d-proton or DXVK into the prefix: load them
    // app-locally (DXVK's dxgi hands D3D12 swapchains to vkd3d-proton).
    for (dir, dll) in [("vkd3d-proton", "d3d12.dll"), ("vkd3d-proton", "d3d12core.dll"), ("dxvk", "dxgi.dll")] {
        std::fs::copy(proton.join("files/lib/wine").join(dir).join("x86_64-windows").join(dll), t.join(dll)).expect("Proton's vkd3d-proton and DXVK");
    }
    write_manifest(&t.join("layer"), &layer_so());
    let session = TestSession::new();
    let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/quiver-sight-wine-vkd3d");
    let compat = keep.join("compat");
    std::fs::create_dir_all(&compat).unwrap();
    for d in ["home", "steam"] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    let title = format!("quiver-sight-d3d12-{}", std::process::id());
    // d3d12_present.c's flags; uncapped runs present ~10x as often, so 10x the frames.
    let flags: u32 = std::env::var("QUIVER_TEST_D3D12_FLAGS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let frames: u64 = if flags & 1 != 0 { 15000 } else { 1500 };
    let resize_at = frames * 65 / 100;
    let mut cmd = Command::new("python3");
    cmd.arg(proton.join("proton"))
        .args(["runinprefix", &win(&exe), &frames.to_string(), &title, &resize_at.to_string(), &flags.to_string()])
        .env("STEAM_COMPAT_DATA_PATH", &compat)
        .env("WINEPREFIX", compat.join("pfx"))
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", t.join("steam"))
        .env("STEAM_COMPAT_APP_ID", "0")
        .env("SteamAppId", "0")
        .env("SteamGameId", "0")
        .env("HOME", t.join("home"))
        .env("XDG_DATA_HOME", t.join("home/.local/share"))
        .env("XDG_CONFIG_HOME", t.join("home/.config"))
        .env("XDG_CACHE_HOME", t.join("home/.cache"))
        .env("DISPLAY", &display)
        .env("WINEDEBUG", "-all")
        .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=;d3d12,d3d12core,dxgi=n")
        .env("DXVK_LOG_LEVEL", "none")
        .env("VKD3D_DEBUG", "none")
        .env("VKD3D_SHADER_DEBUG", "none")
        .env("QUIVER_PERF_DIR", session.dir.path())
        .env("QUIVER_SIGHT_LOG", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("PROTON_ENABLE_WAYLAND")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let validation = if std::env::var("QUIVER_TEST_VALIDATION").as_deref() == Ok("1") { validation_dir() } else { None };
    for (k, v) in layer_env(&t.join("layer"), validation.as_deref()) {
        cmd.env(k, v);
    }
    if let Ok(d) = std::env::var("QUIVER_TEST_VK_DEVICE") {
        cmd.env("VKD3D_FILTER_DEVICE_NAME", &d).env("DXVK_FILTER_DEVICE_NAME", d);
    }
    let png = std::env::var_os("QUIVER_TEST_SIGHT_PNG").map(PathBuf::from);
    let child = cmd.spawn().unwrap();
    let waiter = std::thread::spawn(move || child.wait_with_output().unwrap());

    // Phases by the layer's present count (the game's frame index, give or take).
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Phase {
        Hidden,
        Shown,
        Off,
        Blur,
        Resized,
    }
    let phase_of = |presents: u64| match presents * 100 / frames {
        0..17 => Phase::Hidden,
        17..33 => Phase::Shown,
        33..47 => Phase::Off,
        47..63 => Phase::Blur,
        _ => Phase::Resized,
    };
    let deadline = Instant::now() + Duration::from_secs(if flags & 1 != 0 { 900 } else { 240 });
    let mut ring_path = None;
    let mut published_for = (0, 0, 0.0f32);
    let mut checks: Vec<(Phase, u64, Result<(), String>)> = Vec::new();
    let mut statuses: Vec<(Phase, LayerStatus)> = Vec::new();
    let mut last_check = Instant::now();
    while !waiter.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if ring_path.is_none() {
            ring_path = session.dir.list().unwrap().into_iter().find(|(k, _)| matches!(k, FileKind::Layer { .. })).map(|(_, p)| p);
        }
        let Some(p) = &ring_path else { continue };
        let Ok(r) = open_ring_reader(p) else { continue };
        let Some(st) = r.layer_status() else { continue };
        let presents = st.presents;
        let phase = phase_of(presents);
        let extent = (st.extent_w, st.extent_h);
        let opacity = match phase {
            Phase::Hidden => None,
            Phase::Shown | Phase::Resized => Some(0.92),
            Phase::Off | Phase::Blur => Some(0.0),
        };
        if let Some(o) = opacity
            && extent.0 > 0
            && (extent.0, extent.1, o) != published_for
        {
            session.publish(&test_image(extent, (24, 24), (380, 190), 0, o));
            published_for = (extent.0, extent.1, o);
        }
        let now = quiver_perf::clock::mono_raw_ns();
        session.backdrop(&if phase == Phase::Blur { BackdropRequest::blur(DIM, 0, now + 1_000_000_000) } else { BackdropRequest::NONE });
        if statuses.last().is_none_or(|(ph, _)| *ph != phase) {
            statuses.push((phase, st));
        }
        // Check the screen a few times per phase, once the phase has settled.
        if last_check.elapsed() > Duration::from_millis(400) {
            last_check = Instant::now();
            let Some(s) = Screen::grab(&display, &title) else { continue };
            let res = pattern_ok(&s, phase == Phase::Blur);
            if let (Some(dir), Err(_)) = (&png, &res) {
                s.save(&dir.join(format!("vkd3d-{phase:?}-{presents}.ppm")));
            }
            if checks.iter().filter(|(ph, ..)| *ph == phase).count() == 1
                && let Some(dir) = &png
            {
                s.save(&dir.join(format!("vkd3d-{phase:?}.ppm")));
            }
            checks.push((phase, presents, res));
        }
    }
    let out = waiter.join().unwrap();
    let _ = Command::new(proton.join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", compat.join("pfx")).status();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let layer_log: Vec<&str> = stderr.lines().filter(|l| l.starts_with("quiver-sight-layer")).collect();
    eprintln!("{}", layer_log.join("\n"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let vvl: Vec<&str> = stdout.lines().chain(stderr.lines()).filter(|l| l.contains("Validation Error") || l.contains("Validation Warning")).collect();
    if validation.is_some() {
        eprintln!("validation: {} messages", vvl.len());
        for l in vvl.iter().take(30) {
            eprintln!("  {}", &l[..l.len().min(900)]);
        }
    }
    for (ph, st) in &statuses {
        eprintln!(
            "{ph:?}: swapchain {} ({} of them) {}x{} format {} colour space {} mode {} images {} draw_error {} backdrop_error {} flags {:#x}",
            st.swapchain,
            st.swapchain_count,
            st.extent_w,
            st.extent_h,
            st.vk_format,
            st.color_space,
            st.present_mode,
            st.image_count,
            st.draw_error,
            st.backdrop_error,
            st.flags
        );
    }
    for (ph, n, r) in &checks {
        eprintln!("screen at {n} ({ph:?}): {}", match r {
            Ok(()) => "the game's frame".to_owned(),
            Err(e) => format!("WRONG: {e}"),
        });
    }
    assert!(out.status.success() && stdout.contains(&format!("presented {frames} frames")), "{stdout}\n{stderr}");
    let ring_path = ring_path.expect("the layer made no ring: not loaded into the Wine process?");
    let mut r = open_ring_reader(&ring_path).unwrap();
    let mut recs = Vec::new();
    r.poll(100_000, |_, rec| recs.push(LayerPresent::from_record(rec).unwrap()));
    eprintln!("{} presents recorded, {} with the HUD", recs.len(), recs.iter().filter(|p| p.hud_drawn).count());
    assert!(recs.len() as u64 >= frames.min(4096), "one record per present");
    assert!(recs.iter().any(|p| p.hud_drawn), "the HUD was never drawn");
    // Hidden (nothing published yet): every present passed through, nothing drawn.
    // (The first records, when the ring still has every present.)
    if recs.len() as u64 == frames {
        let hidden_until = (frames * 15 / 100) as usize;
        assert!(recs[..hidden_until].iter().all(|p| !p.hud_drawn && p.hud_ns == 0), "the layer drew while the HUD was hidden");
    }
    // Grabs that count: after the first frames (Wine maps the window first), before the
    // last (the window goes), and not the first of a phase (the blur fades in and out).
    let settled: Vec<_> = checks
        .iter()
        .enumerate()
        .filter(|(i, (ph, n, _))| *n >= 20 && *n + frames / 50 < frames && *i > 0 && checks[i - 1].0 == *ph)
        .map(|(_, c)| c)
        .collect();
    let wrong: Vec<_> = settled.iter().filter(|(_, _, r)| r.is_err()).collect();
    assert!(wrong.is_empty(), "the screen didn't show the game's frames: {wrong:?}");
    for ph in [Phase::Hidden, Phase::Shown, Phase::Off, Phase::Blur, Phase::Resized] {
        assert!(settled.iter().any(|(p, _, r)| *p == ph && r.is_ok()), "no good screen in phase {ph:?}");
    }
}
