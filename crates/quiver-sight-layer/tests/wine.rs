//! The layer under Proton: a D3D11 program (tests/wine/d3d11_present.c) renders through DXVK,
//! whose presents reach `VK_LAYER_QUIVER_sight` as vkQueuePresentKHR. Checks that the layer
//! loads into the Wine process, records DXVK's presents with its `VkPresentIdKHR` frame ids
//! and its engine name, and composites the HUD image this test publishes for the extent the
//! layer reports (as quiverd and quiver-sight would).
//!
//! Opens a window for a few seconds (Wine's X11 driver), so it's opt-in:
//!
//! `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton10-34 cargo test
//! -p quiver-sight-layer --test wine -- --ignored --nocapture`
//!
//! Needs `x86_64-w64-mingw32-gcc`. Uses a scratch prefix and HOME (the prefix is kept in
//! `target/ubi-sight-wine` between runs). `QUIVER_TEST_VK_DEVICE=<name substring>` picks
//! DXVK's adapter (`DXVK_FILTER_DEVICE_NAME`); `QUIVER_TEST_SIGHT_PNG=<dir>` saves a
//! screenshot of the window (grim); `QUIVER_TEST_VALIDATION=1` puts the Khronos validation
//! layer below Sight and prints what it reports.

mod harness;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness::*;
use quiver_perf::LayerStatus;
use quiver_perf::layout::{LAYER_DRAWING, LAYER_PRESENT_ID};
use quiver_perf::record::LayerPresent;
use quiver_perf::session::{FileKind, open_ring_reader};

fn build_exe(out: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wine/d3d11_present.c");
    let exe = out.join("d3d11_present.exe");
    let st = Command::new("x86_64-w64-mingw32-gcc")
        .args(["-O2", "-o"])
        .arg(&exe)
        .arg(&src)
        .args(["-ld3d11", "-ldxgi", "-lgdi32", "-luuid", "-ldxguid"])
        .status()
        .expect("x86_64-w64-mingw32-gcc");
    assert!(st.success());
    exe
}

fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON and a display; see the module docs"]
fn dxvk_presents_through_the_layer() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let scratch = tempfile::tempdir().unwrap();
    let t = scratch.path();
    let exe = build_exe(t);
    // `runinprefix` doesn't install DXVK into the prefix: load Proton's DXVK app-locally.
    for dll in ["d3d11.dll", "dxgi.dll"] {
        std::fs::copy(proton.join("files/lib/wine/dxvk/x86_64-windows").join(dll), t.join(dll)).expect("Proton's DXVK");
    }
    write_manifest(&t.join("layer"), &layer_so());
    let session = TestSession::new();
    let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ubi-sight-wine");
    let compat = keep.join("compat");
    std::fs::create_dir_all(&compat).unwrap();
    for d in ["home", "steam"] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    let title = format!("quiver-sight-d3d11-{}", std::process::id());
    let frames = 900;
    let mut cmd = Command::new("python3");
    cmd.arg(proton.join("proton"))
        .args(["runinprefix", &win(&exe), &frames.to_string(), &title])
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
        .env("WINEDEBUG", "-all")
        .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=;d3d11,dxgi=n")
        .env("DXVK_LOG_LEVEL", "none")
        .env("QUIVER_PERF_DIR", session.dir.path())
        .env("QUIVER_SIGHT_LOG", "1")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Validation below the layer (its messages go to stdout, no messenger in the game).
    let validation = if std::env::var("QUIVER_TEST_VALIDATION").as_deref() == Ok("1") { validation_dir() } else { None };
    for (k, v) in layer_env(&t.join("layer"), validation.as_deref()) {
        cmd.env(k, v);
    }
    if let Ok(d) = std::env::var("QUIVER_TEST_VK_DEVICE") {
        cmd.env("DXVK_FILTER_DEVICE_NAME", d);
    }
    let child = cmd.spawn().unwrap();
    let waiter = std::thread::spawn(move || child.wait_with_output().unwrap());

    // Play quiverd + quiver-sight: find the layer's ring, read the extent from its status,
    // publish a HUD image laid out for it.
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut published_for = (0, 0);
    let mut first_drawn: Option<Instant> = None;
    let mut status_seen: Option<LayerStatus> = None;
    let mut shot = false;
    let mut ring_path = None;
    while !waiter.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        if ring_path.is_none() {
            ring_path = session.dir.list().unwrap().into_iter().find(|(k, _)| matches!(k, FileKind::Layer { .. })).map(|(_, p)| p);
        }
        let Some(p) = &ring_path else { continue };
        let Ok(r) = open_ring_reader(p) else { continue };
        let Some(st) = r.layer_status() else { continue };
        if st.extent_w == 0 {
            continue;
        }
        let extent = (st.extent_w, st.extent_h);
        if extent != published_for {
            session.publish(&test_image(extent, (24, 24), (380, 190), published_for.0 % 7, 0.92));
            published_for = extent;
        }
        if st.flags & LAYER_DRAWING != 0 {
            status_seen = Some(st);
            let since = *first_drawn.get_or_insert_with(Instant::now);
            if !shot && since.elapsed() > Duration::from_millis(2500) {
                shot = true;
                if let Some(dir) = std::env::var_os("QUIVER_TEST_SIGHT_PNG") {
                    screenshot(&title, &Path::new(&dir).join("sight-dxvk.png"));
                }
            }
        }
    }
    let out = waiter.join().unwrap();
    let _ = Command::new(proton.join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", compat.join("pfx")).status();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The layer's log, and the loader's chain when VK_LOADER_DEBUG=layer is set.
    let layer_log: Vec<&str> = stderr.lines().filter(|l| l.starts_with("quiver-sight-layer") || l.contains("Inserted device layer")).collect();
    eprintln!("{}", layer_log.join("\n"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let vvl: Vec<&str> = stdout.lines().filter(|l| l.contains("Validation Error") || l.contains("Validation Warning")).collect();
    if validation.is_some() {
        eprintln!("validation: {} messages", vvl.len());
        for l in vvl.iter().take(20) {
            eprintln!("  {}", &l[..l.len().min(600)]);
        }
    }
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("presented {frames} frames")));

    let ring_path = ring_path.expect("the layer made no ring: not loaded into the Wine process?");
    let mut r = open_ring_reader(&ring_path).unwrap();
    let mut recs = Vec::new();
    r.poll(100_000, |_, rec| recs.push(LayerPresent::from_record(rec).unwrap()));
    let st = status_seen.expect("the layer never reported drawing");
    eprintln!(
        "DXVK under Proton: {} presents recorded, {} with the HUD; engine {:?} {:#x}, device {:?}, extent {}x{}, format {}, present mode {}, flags {:#x}",
        recs.len(),
        recs.iter().filter(|p| p.hud_drawn).count(),
        LayerStatus::get_str(&st.engine_name),
        st.engine_version,
        LayerStatus::get_str(&st.device_name),
        st.extent_w,
        st.extent_h,
        st.vk_format,
        st.present_mode,
        st.flags
    );
    assert_eq!(recs.len(), frames as usize, "one record per DXVK present");
    assert_eq!(LayerStatus::get_str(&st.engine_name), Some("DXVK"));
    assert!(recs.iter().filter(|p| p.hud_drawn).count() > frames as usize / 2);
    // DXVK chains its per-swapchain frame id when the device has VK_KHR_present_id.
    if st.flags & LAYER_PRESENT_ID != 0 {
        let ids: Vec<u64> = recs.iter().filter_map(|p| p.present_id).collect();
        assert_eq!(ids.len(), recs.len());
        assert!(ids.windows(2).all(|w| w[1] > w[0]), "present ids not increasing");
        eprintln!("DXVK present ids {}..={}", ids[0], ids[ids.len() - 1]);
    } else {
        eprintln!("no VkPresentIdKHR from DXVK on this device");
    }
}

fn screenshot(title: &str, path: &Path) {
    let out = Command::new("hyprctl").args(["clients", "-j"]).output().expect("hyprctl");
    let clients: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let Some(c) = clients.as_array().unwrap().iter().find(|c| c["title"] == title) else {
        eprintln!("window {title} not found for the screenshot");
        return;
    };
    let g = format!("{},{} {}x{}", c["at"][0], c["at"][1], c["size"][0], c["size"][1]);
    let st = Command::new("grim").args(["-g", &g]).arg(path).status().expect("grim");
    assert!(st.success());
    eprintln!("screenshot {} ({g})", path.display());
}
