//! The Sight panel drawn in a D3D12 game under Proton (vkd3d-proton), end to end with this
//! build's layer and input preload (design/SIGHT.md §14): tests/wine/d3d12_present.c renders
//! its pattern on a private Xvfb; the test stands in for quiver-sight only (it opens the panel
//! in `panel.input` and publishes `panel.image` at the extent the layer reports there).
//!
//! 1. Open: the screen shows the panel's image (a magenta block) and its cursor over the
//!    game's frames, the game's pattern around it; the preload keeps capturing input past the
//!    opening grace because the real layer marks the panel drawn every frame.
//! 2. The panel's image goes stale (quiver-sight stops heartbeating it, as if its renderer
//!    hung): the layer stops drawing it, and the preload gives input back.
//! 3. Closed: the game's frames only.
//!
//! Opt-in (build the preload first; it's found next to the layer, or by
//! `QUIVER_SIGHT_INPUT_SO`):
//!
//! `cargo build -p quiver-sight-input && QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64
//! cargo test -p quiver-sight-layer --test wine_panel -- --ignored --nocapture`
//!
//! Needs `x86_64-w64-mingw32-gcc`, Xvfb and ImageMagick. `QUIVER_TEST_SIGHT_PNG=<dir>` saves
//! the screens.

mod harness;
mod winescreen;
mod xvfb;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness::*;
use quiver_perf::HudBufferHeader;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::{PANEL_LIMITS, buffer_bytes_within, rgba_premul};
use quiver_perf::layout::HUD_FORMAT_RGBA8_PREMUL_SRGB;
use quiver_perf::panel::{PANEL_HOLD_NS, PANEL_IMAGE_FILE, PANEL_INPUT_FILE, create_panel_image, create_panel_input};
use winescreen::*;

fn preload_so() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIVER_SIGHT_INPUT_SO") {
        return PathBuf::from(p);
    }
    let p = layer_so().with_file_name("libquiver_sight_input.so");
    assert!(p.exists(), "{} missing: cargo build -p quiver-sight-input first", p.display());
    p
}

fn panel_image(extent: (u32, u32)) -> (HudBufferHeader, Vec<u8>, (u32, u32, u32, u32)) {
    let (w, h) = extent;
    let block = (w / 4, h / 4, w * 3 / 4, h * 3 / 4);
    let mut px = vec![0u8; (w * 4 * h) as usize];
    let m = rgba_premul(255, 0, 255, 255).to_le_bytes();
    for y in block.1..block.3 {
        for x in block.0..block.2 {
            let o = ((y * w + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&m);
        }
    }
    let mut d = HudBufferHeader::ZERO;
    (d.width, d.height, d.stride, d.format, d.scale, d.extent_w, d.extent_h, d.opacity) = (w, h, w * 4, HUD_FORMAT_RGBA8_PREMUL_SRGB, 1.5, w, h, 1.0);
    (d, px, block)
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, Xvfb, ImageMagick and a built preload; see the module docs"]
fn panel_in_a_d3d12_game() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let x = xvfb::Xvfb::start_size("1920x1080x24").expect("Xvfb");
    let display = x.display.clone();
    let scratch = tempfile::tempdir().unwrap();
    let t = scratch.path();
    let exe = build_exe(t);
    for (dir, dll) in [("vkd3d-proton", "d3d12.dll"), ("vkd3d-proton", "d3d12core.dll"), ("dxvk", "dxgi.dll")] {
        std::fs::copy(proton.join("files/lib/wine").join(dir).join("x86_64-windows").join(dll), t.join(dll)).expect("Proton's vkd3d-proton and DXVK");
    }
    write_manifest(&t.join("layer"), &layer_so());
    let session = TestSession::new();
    let panel = create_panel_input(&session.dir.path().join(PANEL_INPUT_FILE)).unwrap();
    let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/quiver-sight-wine-vkd3d");
    let compat = keep.join("compat");
    std::fs::create_dir_all(&compat).unwrap();
    for d in ["home", "steam"] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    let title = format!("quiver-sight-panel-{}", std::process::id());
    let frames: u64 = 100_000;
    let mut cmd = Command::new("python3");
    cmd.arg(proton.join("proton"))
        .args(["runinprefix", &win(&exe), &frames.to_string(), &title, "0", "0"])
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
        .env("LD_PRELOAD", preload_so())
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("PROTON_ENABLE_WAYLAND")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in layer_env(&t.join("layer"), None) {
        cmd.env(k, v);
    }
    if let Ok(d) = std::env::var("QUIVER_TEST_VK_DEVICE") {
        cmd.env("VKD3D_FILTER_DEVICE_NAME", &d).env("DXVK_FILTER_DEVICE_NAME", d);
    }
    let png = std::env::var_os("QUIVER_TEST_SIGHT_PNG").map(PathBuf::from);
    let child = cmd.spawn().unwrap();
    let waiter = std::thread::spawn(move || child.wait_with_output().unwrap());

    // Wait for the layer's word (the swapchain's extent) and the preload's.
    let deadline = Instant::now() + Duration::from_secs(240);
    let extent = loop {
        assert!(Instant::now() < deadline && !waiter.is_finished(), "no layer word in panel.input");
        let l = panel.layer();
        if l.can_draw(mono_raw_ns()) {
            break l.extent;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let bb = buffer_bytes_within(extent.0, extent.1, &PANEL_LIMITS).unwrap();
    let mut writer = create_panel_image(&session.dir.path().join(PANEL_IMAGE_FILE), bb).unwrap();
    let (desc, px, block) = panel_image(extent);
    writer.heartbeat(mono_raw_ns());
    writer.publish(&desc, &px).unwrap();
    let cursor = (block.0 as i32 - 40, block.1 as i32 - 40);
    let now = mono_raw_ns();
    panel.set_sight_pid(std::process::id());
    panel.set_cursor(cursor.0, cursor.1, extent);
    panel.open(1, now, now + PANEL_HOLD_NS as i64, true);
    let opened = Instant::now();
    eprintln!("panel opened on a {}x{} swapchain", extent.0, extent.1);

    let shot = |name: &str| -> Option<Screen> {
        let s = Screen::grab(&display, &title)?;
        if let Some(d) = &png {
            s.save(&d.join(format!("panel-{name}.ppm")));
        }
        Some(s)
    };
    let tick = |beat: bool, w: &quiver_perf::hud::HudWriter<quiver_perf::shm::Mapping>| {
        let now = mono_raw_ns();
        panel.renew(now + PANEL_HOLD_NS as i64);
        if beat {
            w.heartbeat(now);
        }
    };
    // 1. Open, 4 s (past the 1.5 s grace).
    let mut open_shot = None;
    while opened.elapsed() < Duration::from_secs(4) {
        tick(true, &writer);
        std::thread::sleep(Duration::from_millis(100));
        if opened.elapsed() > Duration::from_secs(2) && open_shot.is_none() {
            open_shot = shot("open");
        }
    }
    let input = panel.input();
    let layer = panel.layer();
    eprintln!("open: input {input:?}, layer {layer:?}");
    let s = open_shot.expect("no screen grab");
    let (sx, sy) = (s.w as f32 / extent.0 as f32, s.h as f32 / extent.1 as f32);
    let at = |x: u32, y: u32| s.px((x as f32 * sx) as usize, (y as f32 * sy) as usize);
    let near = |p: [u8; 3], want: [u8; 3], tol: i32| p.iter().zip(want).all(|(a, b)| (*a as i32 - b as i32).abs() <= tol);
    let mid = at((block.0 + block.2) / 2, (block.1 + block.3) / 2);
    assert!(near(mid, [255, 0, 255], 10), "the panel isn't on screen: {mid:?}");
    // Outside the block: the game (left half red, right half blue; skip the green band).
    let left = at(extent.0 / 10, extent.1 * 9 / 10);
    assert!(left[0] > 150 && left[2] < 90 || left[1] > 120, "the game around the panel: {left:?}");
    // The cursor's light fill (scale 1.5).
    let c = at((cursor.0 + 3) as u32, (cursor.1 + 12) as u32);
    assert!(c.iter().all(|&v| v > 220), "the cursor isn't drawn: {c:?}");
    assert_eq!(layer.drawn_epoch, 1);
    assert!(input.flags & quiver_perf::panel::INPUT_X11 != 0, "the preload isn't in the game: {input:?}");
    assert_eq!(input.capturing, 1, "the preload didn't keep capturing while the layer drew the panel");

    // 2. The image goes stale: the layer stops drawing it (3 s), the preload releases (1 s).
    let stale = Instant::now();
    let mut released = None;
    while stale.elapsed() < Duration::from_secs(8) {
        tick(false, &writer);
        std::thread::sleep(Duration::from_millis(100));
        if released.is_none() && panel.input().capturing == 0 {
            released = Some(stale.elapsed());
        }
    }
    eprintln!("input released {released:?} after the panel image stopped being refreshed");
    let r = released.expect("input stayed captured with the panel no longer drawn");
    assert!(r > Duration::from_millis(2500) && r < Duration::from_millis(5500), "{r:?}");
    let s = shot("stale").expect("no screen grab");
    let mid = s.px(s.w / 2, s.h / 2 + s.h / 8);
    assert!(!near(mid, [255, 0, 255], 40), "a stale panel is still drawn: {mid:?}");

    // 3. Closed.
    panel.close();
    std::thread::sleep(Duration::from_millis(500));
    let s = shot("closed").expect("no screen grab");
    let mid = s.px(s.w / 2, s.h / 2 + s.h / 8);
    assert!(!near(mid, [255, 0, 255], 40), "drawn after close: {mid:?}");

    // The game would run for minutes: end it.
    let _ = Command::new(proton.join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", compat.join("pfx")).status();
    let out = waiter.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    for l in stderr.lines().filter(|l| l.starts_with("quiver-sight")) {
        eprintln!("{l}");
    }
}
