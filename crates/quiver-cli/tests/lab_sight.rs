//! Sight in a `quiver lab` instance, end to end (design/LAB.md §12), with the real `quiver lab`
//! commands and container (`localhost/quiver-lab:1`, gamescope headless, the GPU through CDI),
//! but a test program instead of a game: the Sight layer's D3D12 program
//! (`quiver-sight-layer/tests/wine/d3d12_present.c`, which also prints the key messages it
//! gets) under Proton with vkd3d-proton, in a hand-made DRM-free instance with `sight = true`
//! in a scratch lab root. Checked:
//!
//! 1. the launch carries the layer, the preload and the run's lab quiver-sight;
//! 2. Shift+F2 (`quiver lab key`) opens the panel **in the game**: drawn in the game's frames
//!    (the screenshot changes), the game's input captured, and a key then never reaches the
//!    game; Escape closes it and keys reach the game again;
//! 3. Shift+F3 shows the lab HUD (the top-left of the frame changes);
//! 4. the failsafe: with the panel open, killing the run's quiver-sight gives the game its
//!    input back;
//! 5. `quiver lab sight` and `quiver lab logs` say all that.
//!
//! Opt-in:
//!
//! `cargo build -p quiver-cli -p quiver-sight-layer -p quiver-sight-input && cargo build --release -p quiver-sight &&
//! QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 cargo test
//! -p quiver-cli --test lab_sight -- --ignored --nocapture`
//!
//! Needs podman, the lab image (`quiver lab image build`), NVIDIA's CDI spec and
//! `x86_64-w64-mingw32-gcc`. The lab root is kept in `target/quiver-lab-sight-test` (its
//! prefix is made once).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_core::lab::manifest::{Display, GamePaths, Licence, Manifest, Runtime};

const INST: &str = "sighttest";

fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn quiver(root: &Path, sight_dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_quiver"))
        .arg("lab")
        .args(args)
        .env("QUIVER_LAB_ROOT", root)
        .env("QUIVER_LAB_SIGHT_DIR", sight_dir)
        // Release: a debug build reads its embedded fonts and icons from the source tree (rust-embed),
        // which the container doesn't have.
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

fn status(root: &Path, sd: &Path) -> serde_json::Value {
    serde_json::from_str(&ok(root, sd, &["sight", INST, "status", "--json"])).unwrap_or_default()
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

fn capturing(s: &serde_json::Value) -> bool {
    !s["panel_input"]["capturing"].is_null()
}

fn panel_open(s: &serde_json::Value) -> bool {
    s["quiver_sight"]["panel"]["open"] == true
}

/// The mean absolute difference of two screenshots in a rectangle of fractions.
fn diff(a: &Path, b: &Path, r: (f32, f32, f32, f32)) -> f64 {
    let (a, b) = (image::open(a).unwrap().to_rgb8(), image::open(b).unwrap().to_rgb8());
    let (w, h) = (a.width().min(b.width()), a.height().min(b.height()));
    let (x0, y0, x1, y1) = ((r.0 * w as f32) as u32, (r.1 * h as f32) as u32, (r.2 * w as f32) as u32, (r.3 * h as f32) as u32);
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (y0..y1).step_by(4) {
        for x in (x0..x1).step_by(4) {
            let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
            sum += (0..3).map(|i| (p[i] as i32 - q[i] as i32).unsigned_abs() as u64).sum::<u64>();
            n += 3;
        }
    }
    sum as f64 / n.max(1) as f64
}

/// What the program says it got (`QUIVER_PROBE_LOG`, in the lab folder).
fn game_log(root: &Path) -> String {
    std::fs::read_to_string(root.join("instances").join(INST).join("lab/probe.log")).unwrap_or_default()
}

fn keydowns(log: &str, vk: u32) -> usize {
    log.lines().filter(|l| *l == format!("KEYDOWN 0x{vk:02x}")).count()
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, podman, the lab image and the GPU; see the module docs"]
fn sight_in_a_lab_instance() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let root = target_dir().join("quiver-lab-sight-test");
    let inst = root.join("instances").join(INST);
    for d in ["prefix", "install", "lab"] {
        std::fs::create_dir_all(inst.join(d)).unwrap();
    }
    // The Sight libraries as the install lays them out ({x86_64}/…), from this build.
    let sight_dir = root.join("sight-libs");
    std::fs::create_dir_all(sight_dir.join("x86_64")).unwrap();
    for so in ["libquiver_sight_layer.so", "libquiver_sight_input.so"] {
        let _ = std::fs::remove_file(sight_dir.join("x86_64").join(so));
        std::fs::copy(target_dir().join("debug").join(so), sight_dir.join("x86_64").join(so)).unwrap_or_else(|e| panic!("{so}: {e} (cargo build the Sight crates first)"));
    }
    assert!(target_dir().join("release/quiver-sight").is_file(), "cargo build --release -p quiver-sight first");
    // The program, and vkd3d-proton and DXVK's dxgi next to it.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../quiver-sight-layer/tests/wine/d3d12_present.c");
    let exe = inst.join("install/d3d12_present.exe");
    // A GUI program, as games are (a console one gets Wine's console window, which takes focus).
    assert!(Command::new("x86_64-w64-mingw32-gcc").args(["-O2", "-mwindows", "-o"]).arg(&exe).arg(&src).args(["-ld3d12", "-ldxgi", "-luuid", "-ldxguid"]).status().unwrap().success());
    for (dir, dll) in [("vkd3d-proton", "d3d12.dll"), ("vkd3d-proton", "d3d12core.dll"), ("dxvk", "dxgi.dll")] {
        // Proton's copies are read-only: a rerun replaces them.
        let _ = std::fs::remove_file(inst.join("install").join(dll));
        std::fs::copy(proton.join("files/lib/wine").join(dir).join("x86_64-windows").join(dll), inst.join("install").join(dll)).unwrap();
    }
    let _ = std::fs::remove_file(inst.join("lab/probe.log"));
    let m = Manifest {
        name: INST.into(),
        game: quiver_api::GameRef::parse("steam:480").unwrap(),
        title: "Sight lab test".into(),
        licence: Licence::None,
        created: chrono::Utc::now(),
        created_by: "test".into(),
        display: Display { width: 1280, height: 720, refresh: 60, hdr: false },
        runtime: Runtime { proton: proton.file_name().unwrap().to_string_lossy().into(), proton_path: proton.clone(), dxvk: None, vkd3d: None, nvapi: None, image: quiver_core::lab::IMAGE.into() },
        paths: GamePaths { install: "/opt/qtest/install".into(), prefix: "/opt/qtest/pfx".into(), exe: "d3d12_present.exe".into(), args: vec!["1000000".into(), "quiver-lab-sight".into(), "0".into(), "0".into()] },
        env: BTreeMap::from([("WINEDEBUG".to_owned(), "-all".to_owned()), ("PROTON_USE_XALIA".to_owned(), "0".to_owned()), ("QUIVER_PROBE_LOG".to_owned(), r"Z:\lab\probe.log".to_owned())]),
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
    let launched = ok(&root, sd, &["launch", INST, "--wait", "frame", "--timeout", "300"]);
    eprintln!("{launched}");
    assert!(launched.contains("Sight: the layer"), "{launched}");

    // 1. The layer has a swapchain and the preload is in: the panel can be drawn in the game.
    wait_for("the layer and the preload in panel.input", Duration::from_secs(60), || {
        let s = status(&root, sd);
        let p = &s["panel_input"];
        p["layer"].as_str().is_some_and(|l| l.contains("flags: 1") && !l.contains("extent: (0, 0)")) && p["input"].as_str().is_some_and(|i| !i.contains("flags: 0,"))
    });
    std::thread::sleep(Duration::from_secs(2));
    let shots = root.join("shots-test");
    std::fs::create_dir_all(&shots).unwrap();
    let before = shots.join("before.png");
    ok(&root, sd, &["shot", INST, before.to_str().unwrap()]);
    let log0 = game_log(&root);
    ok(&root, sd, &["key", INST, "a"]);
    std::thread::sleep(Duration::from_millis(600));
    assert!(keydowns(&game_log(&root), 0x41) > keydowns(&log0, 0x41), "keys don't reach the program before the panel opens: {}", game_log(&root));

    // 2. Shift+F2: the panel, in the game.
    ok(&root, sd, &["key", INST, "shift+F2"]);
    wait_for("the panel open and capturing", Duration::from_secs(10), || {
        let s = status(&root, sd);
        panel_open(&s) && capturing(&s)
    });
    let s = status(&root, sd);
    assert_eq!(s["quiver_sight"]["panel"]["presenter"], "game", "{s:#}");
    std::thread::sleep(Duration::from_secs(1));
    let open = shots.join("open.png");
    ok(&root, sd, &["shot", INST, open.to_str().unwrap()]);
    let d = diff(&before, &open, (0.3, 0.3, 0.7, 0.7));
    eprintln!("panel: mean difference {d:.1} in the middle of the frame");
    assert!(d > 15.0, "the panel isn't in the game's frame (difference {d:.1}; {} vs {})", before.display(), open.display());
    let log1 = game_log(&root);
    ok(&root, sd, &["key", INST, "a", "Down"]);
    std::thread::sleep(Duration::from_millis(600));
    let log2 = game_log(&root);
    assert_eq!(keydowns(&log2, 0x41), keydowns(&log1, 0x41), "a key reached the game while the panel was open");
    // Escape closes it; keys reach the game again.
    ok(&root, sd, &["key", INST, "Escape"]);
    wait_for("the panel closed and input back", Duration::from_secs(10), || {
        let s = status(&root, sd);
        !panel_open(&s) && !capturing(&s)
    });
    std::thread::sleep(Duration::from_millis(500));
    let log3 = game_log(&root);
    ok(&root, sd, &["key", INST, "a"]);
    std::thread::sleep(Duration::from_millis(600));
    assert!(keydowns(&game_log(&root), 0x41) > keydowns(&log3, 0x41), "keys don't reach the game after the panel closed");
    assert!(!game_log(&root)[log1.len()..].contains("KEYDOWN 0x1b"), "the Escape that closed the panel reached the game");

    // 3. Shift+F3: the lab HUD (top-left).
    ok(&root, sd, &["key", INST, "shift+F3"]);
    wait_for("the HUD shown", Duration::from_secs(10), || status(&root, sd)["quiver_sight"]["hud_on"] == true);
    std::thread::sleep(Duration::from_secs(2));
    let hud = shots.join("hud.png");
    ok(&root, sd, &["shot", INST, hud.to_str().unwrap()]);
    let d = diff(&before, &hud, (0.0, 0.0, 0.15, 0.08));
    eprintln!("HUD: mean difference {d:.1} at the top left");
    assert!(d > 10.0, "the HUD isn't in the frame ({d:.1})");

    // 4. Failsafe: the panel open, its quiver-sight killed: input back.
    ok(&root, sd, &["key", INST, "shift+F2"]);
    wait_for("the panel open again", Duration::from_secs(10), || capturing(&status(&root, sd)));
    let killed = Command::new("podman").args(["exec", &format!("quiver-lab-{INST}"), "pkill", "-KILL", "-x", "quiver-sight"]).status().unwrap();
    assert!(killed.success());
    let t = Instant::now();
    wait_for("input back after quiver-sight died", Duration::from_secs(5), || !capturing(&status(&root, sd)));
    eprintln!("input back {:?} after quiver-sight was killed", t.elapsed());
    let log4 = game_log(&root);
    ok(&root, sd, &["key", INST, "a"]);
    std::thread::sleep(Duration::from_millis(600));
    assert!(keydowns(&game_log(&root), 0x41) > keydowns(&log4, 0x41), "keys don't reach the game after the failsafe");

    // 5. What the logs say.
    let logs = ok(&root, sd, &["logs", INST]);
    eprintln!("{logs}");
    assert!(logs.contains("panel opened in the game"), "{logs}");
    assert!(logs.contains("failsafe: quiver-sight is gone"), "{logs}");
    ok(&root, sd, &["kill", INST]);
}
