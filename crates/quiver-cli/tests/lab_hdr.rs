//! HDR in a `quiver lab` instance, end to end (design/LAB.md §13), with the real `quiver lab`
//! commands and container (`localhost/quiver-lab:1`, gamescope headless with HDR forced, the
//! GPU through CDI) and the Sight layer's D3D12 test program
//! (`quiver-sight-layer/tests/wine/d3d12_present.c`) instead of a game, in a hand-made
//! DRM-free instance with `display.hdr` and `sight` on, in a scratch lab root. The program's
//! HDR modes draw grey at known luminances (left half 203 nits, right half 1000, a moving band
//! 4000) and set HDR metadata (max CLL 4000, FALL 600, mastering 0.005–1000 nits). Checked, for
//! HDR10 (`A2B10G10R10` PQ) and then scRGB (`R16G16B16A16_SFLOAT`):
//!
//! 1. `quiver lab hdr`: the swapchain's format and colour space, the metadata the game set, and
//!    gamescope reporting the game as HDR;
//! 2. `quiver lab shot --hdr`: the capture's nits (max ≈ 4000, the 203 / 1000 split);
//! 3. the Sight panel and HUD on an HDR swapchain (what happens is printed: the layer can't
//!    draw on HDR yet, and a lab instance has no desktop to fall back to).
//!
//! Opt-in:
//!
//! `cargo build -p quiver-cli -p quiver-sight-layer -p quiver-sight-input && cargo build --release -p quiver-sight &&
//! QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 cargo test
//! -p quiver-cli --test lab_hdr -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_core::lab::manifest::{Display, GamePaths, Licence, Manifest, Runtime};

const INST: &str = "hdrtest";

fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn quiver(root: &Path, sight_dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_quiver"))
        .arg("lab")
        .args(args)
        .env("QUIVER_LAB_ROOT", root)
        .env("QUIVER_LAB_SIGHT_DIR", sight_dir)
        .env("QUIVER_LAB_SIGHT_BIN", target_dir().join("release/quiver-sight"))
        .env("QUIVER_SOURCE", "test")
        .output()
        .expect("quiver");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn ok(root: &Path, sd: &Path, args: &[&str]) -> String {
    let (good, text) = quiver(root, sd, args);
    assert!(good, "quiver lab {} failed:\n{text}", args.join(" "));
    text
}

fn json(root: &Path, sd: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::from_str(&ok(root, sd, args)).unwrap_or_default()
}

fn wait_for(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("timed out waiting for {what}");
}

/// One mode: launch with the program's flag, check `hdr`, a capture, (HDR10) the panel and HUD.
fn mode(root: &Path, sd: &Path, flag: &str, format: &str, color_space: &str, panel: bool) {
    eprintln!("{}", ok(root, sd, &["launch", INST, "--arg", flag, "--wait", "frame", "--timeout", "300"]));
    let mut last = serde_json::Value::Null;
    wait_for(&format!("an {color_space} swapchain with metadata"), Duration::from_secs(60), || {
        last = json(root, sd, &["hdr", INST, "--json"]);
        last["swapchain"]["color_space_name"].as_str().is_some_and(|c| c.contains(color_space)) && last["swapchain"]["metadata"]["max_cll"].as_f64().is_some()
    });
    eprintln!("{}", ok(root, sd, &["hdr", INST]));
    let s = &last["swapchain"];
    assert_eq!(s["format_name"], format, "{last:#}");
    assert_eq!(s["hdr"], true);
    assert_eq!(s["metadata"]["max_cll"], 4000.0, "{last:#}");
    assert_eq!(s["metadata"]["max_fall"], 600.0);
    assert_eq!(s["metadata"]["max_luminance"], 1000.0);
    assert!((s["metadata"]["min_luminance"].as_f64().unwrap() - 0.005).abs() < 1e-6);
    assert!((s["metadata"]["primaries"]["red"][0].as_f64().unwrap() - 0.708).abs() < 1e-4);
    assert_eq!(s["capture_ready"], true);
    let g = &last["gamescope"];
    assert_eq!(g["output_hdr"], true, "{last:#}");
    assert_eq!(g["app_hdr"], true, "gamescope doesn't see the game as HDR: {last:#}");

    let out = root.join(format!("shots-test/hdr-{flag}.png"));
    let text = ok(root, sd, &["shot", INST, out.to_str().unwrap(), "--hdr"]);
    eprintln!("{text}");
    let side: serde_json::Value = serde_json::from_slice(&std::fs::read(out.with_extension("json")).unwrap()).unwrap();
    let st = &side["stats"];
    let (max, avg, a203, a1000) = (st["max_nits"].as_f64().unwrap(), st["avg_nits"].as_f64().unwrap(), st["above_203"].as_f64().unwrap(), st["above_1000"].as_f64().unwrap());
    assert!((max - 4000.0).abs() < 100.0, "max {max} nits: {side:#}");
    // Half at 203, half at 1000, a 40-row band of 4000 (≈ 790 nits on average).
    assert!((600.0..1000.0).contains(&avg), "avg {avg} nits");
    assert!((0.45..1.0).contains(&a203), "above 203: {a203}");
    assert!(a1000 < 0.6, "above 1000: {a1000}");
    assert_eq!(side["format_name"], format);
    assert!(side["hdr_metadata"]["max_cll"].as_f64() == Some(4000.0), "{side:#}");
    let raw = std::fs::metadata(out.with_extension("raw")).unwrap().len();
    assert_eq!(raw, side["row_bytes"].as_u64().unwrap() * side["height"].as_u64().unwrap());
    let png = image::open(&out).unwrap();
    assert!(matches!(png, image::DynamicImage::ImageRgb16(_)), "16-bit PNG");
    // gamescope's own screenshot still works (8-bit, tonemapped).
    let sdr = root.join(format!("shots-test/sdr-{flag}.png"));
    ok(root, sd, &["shot", INST, sdr.to_str().unwrap()]);

    if panel {
        // The panel and the HUD on an HDR swapchain: what happens (the layer can't draw on
        // HDR yet; a lab instance has no desktop for the panel to fall back to).
        ok(root, sd, &["key", INST, "shift+F2"]);
        std::thread::sleep(Duration::from_secs(3));
        let s = json(root, sd, &["sight", INST, "status", "--json"]);
        eprintln!("panel on HDR: {:#}", s["quiver_sight"]["panel"]);
        eprintln!("panel input: {}", s["panel_input"]);
        assert!(s["panel_input"]["capturing"].is_null(), "input captured for a panel the game can't show: {s:#}");
        ok(root, sd, &["key", INST, "shift+F3"]);
        std::thread::sleep(Duration::from_secs(2));
        let s = json(root, sd, &["sight", INST, "status", "--json"]);
        eprintln!("HUD on HDR: hud_on {}, layer {}", s["quiver_sight"]["hud_on"], s["layer"]);
        eprintln!("{}", ok(root, sd, &["sight", INST]));
    }
    ok(root, sd, &["kill", INST]);
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, podman, the lab image and the GPU; see the module docs"]
fn hdr_in_a_lab_instance() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let root = target_dir().join("quiver-lab-hdr-test");
    let inst = root.join("instances").join(INST);
    for d in ["prefix", "install", "lab"] {
        std::fs::create_dir_all(inst.join(d)).unwrap();
    }
    let sight_dir = root.join("sight-libs");
    std::fs::create_dir_all(sight_dir.join("x86_64")).unwrap();
    for so in ["libquiver_sight_layer.so", "libquiver_sight_input.so"] {
        let _ = std::fs::remove_file(sight_dir.join("x86_64").join(so));
        std::fs::copy(target_dir().join("debug").join(so), sight_dir.join("x86_64").join(so)).unwrap_or_else(|e| panic!("{so}: {e} (cargo build the Sight crates first)"));
    }
    assert!(target_dir().join("release/quiver-sight").is_file(), "cargo build --release -p quiver-sight first");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../quiver-sight-layer/tests/wine/d3d12_present.c");
    let exe = inst.join("install/d3d12_present.exe");
    assert!(Command::new("x86_64-w64-mingw32-gcc").args(["-O2", "-mwindows", "-o"]).arg(&exe).arg(&src).args(["-ld3d12", "-ldxgi", "-luuid", "-ldxguid"]).status().unwrap().success());
    for (dir, dll) in [("vkd3d-proton", "d3d12.dll"), ("vkd3d-proton", "d3d12core.dll"), ("dxvk", "dxgi.dll")] {
        let _ = std::fs::remove_file(inst.join("install").join(dll));
        std::fs::copy(proton.join("files/lib/wine").join(dir).join("x86_64-windows").join(dll), inst.join("install").join(dll)).unwrap();
    }
    let m = Manifest {
        name: INST.into(),
        game: quiver_api::GameRef::parse("steam:480").unwrap(),
        title: "HDR lab test".into(),
        licence: Licence::None,
        created: chrono::Utc::now(),
        created_by: "test".into(),
        display: Display::parse("1280x720@60+hdr").unwrap(),
        runtime: Runtime { proton: proton.file_name().unwrap().to_string_lossy().into(), proton_path: proton.clone(), dxvk: None, vkd3d: None, nvapi: None, image: quiver_core::lab::IMAGE.into() },
        paths: GamePaths { install: "/opt/qtest/install".into(), prefix: "/opt/qtest/pfx".into(), exe: "d3d12_present.exe".into(), args: vec!["1000000".into(), "quiver-lab-hdr".into(), "0".into()] },
        env: BTreeMap::from([("WINEDEBUG".to_owned(), "-all".to_owned()), ("PROTON_USE_XALIA".to_owned(), "0".to_owned())]),
        dll_overrides: BTreeMap::from([("d3d12".to_owned(), "n".to_owned()), ("d3d12core".to_owned(), "n".to_owned()), ("dxgi".to_owned(), "n".to_owned())]),
        mod_dir: None,
        graphics: None,
        fixture: None,
        saves: vec![],
        files: BTreeMap::new(),
        throttle: vec![],
        reload_pattern: None,
        log_file: None,
        notes: String::new(),
        sight: true,
        clock: None,
        pad: Default::default(),
    };
    m.save(&inst.join("lab.toml")).unwrap();
    let sd = sight_dir.as_path();
    let _ = quiver(&root, sd, &["down", INST]);
    struct Down<'a>(&'a Path, &'a Path);
    impl Drop for Down<'_> {
        fn drop(&mut self) {
            let _ = quiver(self.0, self.1, &["down", INST]);
        }
    }
    let _down = Down(&root, sd);
    eprintln!("{}", ok(&root, sd, &["up", INST]));
    // Program flags: 32 HDR10, 64 scRGB.
    mode(&root, sd, "32", "A2B10G10R10_UNORM_PACK32", "HDR10", true);
    mode(&root, sd, "64", "R16G16B16A16_SFLOAT", "scRGB", false);
    eprintln!("{}", ok(&root, sd, &["logs", INST]));
}
