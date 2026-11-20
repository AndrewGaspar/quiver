//! Virtual pads in `quiver lab` instances, end to end (design/LAB.md §16), with the real
//! commands, container (`localhost/quiver-lab:1`, gamescope headless, the GPU) and uinput, in
//! two hand-made DRM-free instances of a test "game" (`quiver-pad/tests/wine/pad_game.c`: a
//! D3D11 window that logs XInput slots 0 and 1 every frame and shows slot 0 as colour). Checked:
//!
//! 1. `up` makes the pads before the container; every node is hidden from the host (this
//!    process, the user, can't open it; root:quiver-lab-pad 0660, no ACL);
//! 2. the game sees two Xbox 360 pads (XInput slots 0 and 1), and the probe sees them through
//!    XInput, DirectInput, HID and raw input; winedevice.exe has the nodes open;
//! 3. buttons, sticks and triggers reach the game, on the pad they're sent to; the screen
//!    changes with them;
//! 4. native Linux: evdev events on the node, and SDL3 lists the pads, inside the container;
//! 5. on the virtual clock, a script lands on exact frames, the same in two runs;
//! 6. two instances at once: each container has only its own nodes;
//! 7. teardown: `down` removes the pads; a killed pad host takes its pads with it; a stale pad
//!    host is stopped by the next `up`.
//!
//! Opt-in (needs the udev rule installed: `quiver lab doctor`):
//!
//! `cargo build -p quiver-cli && QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64
//! cargo test -p quiver-cli --test lab_pad -- --ignored --nocapture`
//!
//! Needs podman, the lab image, NVIDIA's CDI spec, `x86_64-w64-mingw32-gcc`, your ACL on
//! /dev/uinput, and the rule. The lab root is `target/quiver-lab-pad-test`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_core::lab::manifest::{Display, GamePaths, Licence, Manifest, PadConfig, Runtime};

const A: &str = "padtest";
const B: &str = "padtest2";

fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn quiver(root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_quiver")).arg("lab").args(args).env("QUIVER_LAB_ROOT", root).env("QUIVER_SOURCE", "test").output().expect("quiver");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn ok(root: &Path, args: &[&str]) -> String {
    let (good, text) = quiver(root, args);
    assert!(good, "quiver lab {} failed:\n{text}", args.join(" "));
    text
}

fn wait_for(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn game_log(root: &Path, inst: &str) -> String {
    std::fs::read_to_string(root.join("instances").join(inst).join("lab/padgame.log")).unwrap_or_default()
}

/// The last logged state of a slot.
fn last_state(log: &str, slot: u32) -> Option<String> {
    let tag = format!(" slot {slot} buttons ");
    log.lines().rev().find(|l| l.contains(&tag)).map(|l| l[l.find(" buttons ").unwrap() + 1..].to_owned())
}

/// (frame, state) for every change of a slot.
fn changes(log: &str, slot: u32) -> Vec<(u64, String)> {
    let tag = format!(" slot {slot} buttons ");
    log.lines()
        .filter(|l| l.contains(&tag))
        .map(|l| (l.split_whitespace().nth(1).unwrap().parse().unwrap(), l[l.find(" buttons ").unwrap() + 1..].to_owned()))
        .collect()
}

fn pad_status(root: &Path, inst: &str) -> serde_json::Value {
    serde_json::from_str(&ok(root, &["pad", inst, "status", "--json"])).expect("status json")
}

fn instance(root: &Path, name: &str, pads: u8, proton: &Path, exe: &Path) {
    let dir = root.join("instances").join(name);
    for d in ["prefix", "install", "lab"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let _ = std::fs::remove_file(dir.join("install/pad_game.exe"));
    std::fs::copy(exe, dir.join("install/pad_game.exe")).unwrap();
    let _ = std::fs::remove_file(dir.join("lab/padgame.log"));
    let m = Manifest {
        name: name.into(),
        game: quiver_api::GameRef::parse("steam:480").unwrap(),
        title: "Pad lab test".into(),
        licence: Licence::None,
        created: chrono::Utc::now(),
        created_by: "test".into(),
        display: Display { width: 1280, height: 720, refresh: 60, hdr: false },
        runtime: Runtime { proton: proton.file_name().unwrap().to_string_lossy().into(), proton_path: proton.to_owned(), dxvk: None, vkd3d: None, nvapi: None, image: quiver_core::lab::IMAGE.into() },
        paths: GamePaths { install: format!("/opt/{name}/install").into(), prefix: format!("/opt/{name}/pfx").into(), exe: "pad_game.exe".into(), args: vec![] },
        env: BTreeMap::from([("WINEDEBUG".to_owned(), "-all".to_owned()), ("PROTON_USE_XALIA".to_owned(), "0".to_owned()), ("QUIVER_PAD_GAME_LOG".to_owned(), r"Z:\lab\padgame.log".to_owned())]),
        dll_overrides: BTreeMap::new(),
        mod_dir: None,
        graphics: None,
        fixture: None,
        saves: vec![],
        files: BTreeMap::new(),
        throttle: vec![],
        reload_pattern: None,
        log_file: None,
        notes: String::new(),
        sight: false,
        clock: None,
        pad: PadConfig { count: pads, ..Default::default() },
    };
    m.save(&dir.join("lab.toml")).unwrap();
}

fn lab_pads() -> Vec<(String, String, String)> {
    quiver_pad::uinput::present(Path::new("/sys/devices/virtual/input"))
}

fn pads_of(inst: &str) -> usize {
    lab_pads().iter().filter(|(_, phys, _)| quiver_pad::model::parse_phys(phys).is_some_and(|(i, _)| i == inst)).count()
}

/// The script's changes as frame offsets from its first one (what must repeat run to run).
fn relative(ch: &[(u64, String)]) -> Vec<(u64, String)> {
    let f0 = ch.first().map(|c| c.0).unwrap_or(0);
    ch.iter().map(|(f, s)| (f - f0, s.clone())).collect()
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, podman, the lab image, the GPU and the lab pad udev rule; see the module docs"]
fn pads_in_lab_instances() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let (good, doctor) = quiver(&target_dir(), &["doctor"]);
    let _ = good;
    assert!(doctor.contains("ok   virtual pads"), "the lab pad rule isn't installed (quiver lab doctor):\n{doctor}");
    let root = target_dir().join("quiver-lab-pad-test");
    std::fs::create_dir_all(&root).unwrap();
    let exe = root.join("pad_game.exe");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../quiver-pad/tests/wine/pad_game.c");
    assert!(Command::new("x86_64-w64-mingw32-gcc").args(["-O2", "-mwindows", "-o"]).arg(&exe).arg(&src).args(["-ld3d11", "-ldxgi", "-lwinmm", "-luuid", "-ldxguid"]).status().unwrap().success());
    instance(&root, A, 2, &proton, &exe);
    instance(&root, B, 1, &proton, &exe);
    for i in [A, B] {
        let _ = quiver(&root, &["down", i]);
    }
    struct Down<'a>(&'a Path);
    impl Drop for Down<'_> {
        fn drop(&mut self) {
            for i in [A, B] {
                let _ = quiver(self.0, &["down", i]);
            }
        }
    }
    let _down = Down(&root);

    // 1. Pads before the container, hidden from the host.
    let up = ok(&root, &["up", A]);
    eprintln!("{up}");
    assert!(up.contains("pad 1: \"Quiver Lab Pad padtest\"") && up.contains("pad 2: \"Quiver Lab Pad padtest P2\""), "{up}");
    assert_eq!(pads_of(A), 2);
    let st = pad_status(&root, A);
    let checks = st["checks"].as_array().unwrap();
    assert!(checks.len() >= 2, "{st}");
    for c in checks {
        let path = c["path"].as_str().unwrap();
        assert_eq!(c["openable"], false, "{path} is openable by the user");
        assert_eq!(c["acl"], false, "{path} has an ACL");
        assert_eq!(c["uid"], 0);
        assert_eq!(c["mode"].as_u64().unwrap() & 0o777, 0o660);
        let e = std::fs::File::open(path).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{path}: {e}");
    }

    // 2. The game sees two Xbox 360 pads.
    eprintln!("{}", ok(&root, &["launch", A, "--wait", "frame", "--timeout", "300"]));
    wait_for("both slots connected in the game", Duration::from_secs(60), || {
        let l = game_log(&root, A);
        l.contains("slot 0 connected") && l.contains("slot 1 connected")
    });
    let st = pad_status(&root, A);
    eprintln!("{}", ok(&root, &["pad", A, "status"]));
    let probe = &st["probe"];
    assert_eq!(probe["xinput"][0]["connected"], true, "{st}");
    assert_eq!(probe["xinput"][1]["connected"], true, "{st}");
    assert_eq!(probe["xinput"][0]["subtype"], 1, "a gamepad");
    assert!(probe["dinput"].as_array().unwrap().iter().filter(|d| d["vid"] == 0x045e && d["pid"] == 0x028e).count() == 2, "{probe}");
    assert!(probe["hid"].as_array().unwrap().iter().any(|h| h["vid"] == 0x045e && h["usage"] == 5), "{probe}");
    assert!(probe["rawinput"].as_array().unwrap().iter().any(|r| r["vid"] == 0x045e), "{probe}");
    assert!(st["open_in_container"].as_array().unwrap().iter().any(|o| o["program"].as_str().unwrap_or("").contains("winedevice")), "{st}");

    // 3. Inputs reach the game, on the right pad; the screen follows.
    let shots = root.join("shots-test");
    std::fs::create_dir_all(&shots).unwrap();
    let idle = shots.join("idle.png");
    ok(&root, &["shot", A, idle.to_str().unwrap()]);
    ok(&root, &["pad", A, "hold", "a"]);
    ok(&root, &["pad", A, "stick", "left", "0", "1"]);
    ok(&root, &["pad", A, "trigger", "rt", "1"]);
    wait_for("A, left stick up and RT on slot 0", Duration::from_secs(5), || last_state(&game_log(&root, A), 0).as_deref() == Some("buttons 0x1000 lt 0 rt 255 lx 0 ly 32767 rx 0 ry 0"));
    let held = shots.join("a-stick-rt.png");
    ok(&root, &["shot", A, held.to_str().unwrap()]);
    let px = |p: &Path, fx: f32, fy: f32| {
        let i = image::open(p).unwrap().to_rgb8();
        *i.get_pixel((fx * i.width() as f32) as u32, (fy * i.height() as f32) as u32)
    };
    let g = px(&held, 0.1, 0.9);
    assert!(g[1] > 200 && g[0] < 30 && g[2] < 30, "A with RT: bright green, got {g:?}");
    assert!(px(&idle, 0.1, 0.9)[1] < 40, "idle is dark");
    // The block moved up by a third of the screen with the stick.
    assert_eq!(px(&held, 0.5, 0.5 - 0.333)[0], 255, "the white block follows the left stick");
    ok(&root, &["pad", A, "--pad", "2", "press", "b", "--for", "300ms"]);
    // The release reaches the game a moment after the command returns.
    wait_for("pad 2's release on slot 1", Duration::from_secs(5), || last_state(&game_log(&root, A), 1).is_some_and(|s| s.starts_with("buttons 0x0000")));
    let l = game_log(&root, A);
    let p2: Vec<String> = changes(&l, 1).into_iter().map(|c| c.1).collect();
    assert!(p2.iter().any(|s| s.starts_with("buttons 0x2000")) && p2.last().unwrap().starts_with("buttons 0x0000"), "pad 2's B on slot 1: {p2:?}");
    assert_eq!(last_state(&l, 0).as_deref(), Some("buttons 0x1000 lt 0 rt 255 lx 0 ly 32767 rx 0 ry 0"), "slot 0 untouched by pad 2");
    ok(&root, &["pad", A, "neutral"]);
    wait_for("slot 0 neutral", Duration::from_secs(5), || last_state(&game_log(&root, A), 0).as_deref() == Some("buttons 0x0000 lt 0 rt 0 lx 0 ly 0 rx 0 ry 0"));

    // 4. Native Linux: evdev and SDL3 in the container.
    let node = st["host"]["pads"][0]["nodes"]["event"].as_str().unwrap().to_owned();
    let reader = r#"
import os, select, struct, sys, time
fd = os.open(sys.argv[1], os.O_RDONLY)
end = time.time() + 6
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        d = os.read(fd, 24 * 64)
        for i in range(0, len(d), 24):
            _, _, t, c, v = struct.unpack('qqHHi', d[i:i + 24])
            if t == 1:
                print('key', hex(c), v, flush=True)
"#;
    let (r2, n2) = (root.clone(), node.clone());
    let t = std::thread::spawn(move || quiver(&r2, &["exec", A, "python3", "-c", reader, &n2]));
    std::thread::sleep(Duration::from_secs(2));
    ok(&root, &["pad", A, "press", "x"]);
    let (good, ev) = t.join().unwrap();
    assert!(good && ev.contains("key 0x133 1") && ev.contains("key 0x133 0"), "evdev X down and up in the container: {ev}");
    let sdl = r#"
import ctypes
s = ctypes.CDLL('libSDL3.so.0')
s.SDL_Init.restype = ctypes.c_bool
assert s.SDL_Init(0x2000), 'SDL_Init'
n = ctypes.c_int()
s.SDL_GetGamepads.restype = ctypes.POINTER(ctypes.c_uint32)
s.SDL_GetGamepadNameForID.restype = ctypes.c_char_p
ids = s.SDL_GetGamepads(ctypes.byref(n))
for i in range(n.value):
    print('gamepad', s.SDL_GetGamepadNameForID(ids[i]).decode())
"#;
    let out = ok(&root, &["exec", A, "env", "SDL_JOYSTICK_DISABLE_UDEV=1", "python3", "-c", sdl]);
    assert_eq!(out.matches("gamepad ").count(), 2, "SDL3 lists both pads: {out}");

    // 5. The virtual clock: a script on exact frames, twice the same.
    let mut runs = Vec::new();
    for _ in 0..2 {
        ok(&root, &["launch", A, "--clock", "virtual:62.5", "--wait", "frame", "--timeout", "300"]);
        wait_for("slot 0 connected under the virtual clock", Duration::from_secs(60), || game_log(&root, A).matches("slot 0 connected").count() > runs.len() + 1);
        let before = game_log(&root, A).len();
        let out = ok(&root, &["pad", A, "seq", "a 10f; wait 5f; b rt=0.5 3f; lstick=1,0 100ms"]);
        eprintln!("{out}");
        let l = game_log(&root, A)[before..].to_owned();
        let ch = relative(&changes(&l, 0));
        eprintln!("{ch:?}");
        runs.push(ch);
    }
    assert_eq!(runs[0], runs[1], "the same frames in both runs");
    let r = &runs[0];
    // a for 10 frames, 5 idle, b and RT for 3, then (in the same held frame as b's release) the
    // stick for 6 (100 ms at 62.5 Hz).
    assert_eq!(r.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0, 10, 15, 18, 24], "{r:?}");
    assert!(r[0].1.starts_with("buttons 0x1000") && r[2].1.starts_with("buttons 0x2000 lt 0 rt 128") && r[3].1 == "buttons 0x0000 lt 0 rt 0 lx 32767 ly 0 rx 0 ry 0", "{r:?}");

    // 6. A second instance at once (A's game killed first: one GPU context fewer, the GPU is shared): its own pad, and only its own nodes.
    ok(&root, &["kill", A]);
    eprintln!("{}", ok(&root, &["up", B]));
    assert_eq!((pads_of(A), pads_of(B)), (2, 1));
    let a_nodes: Vec<String> = pad_status(&root, A)["host"]["pads"].as_array().unwrap().iter().map(|p| p["nodes"]["event"].as_str().unwrap().to_owned()).collect();
    let b_nodes: Vec<String> = pad_status(&root, B)["host"]["pads"].as_array().unwrap().iter().map(|p| p["nodes"]["event"].as_str().unwrap().to_owned()).collect();
    let list = |i: &str| ok(&root, &["exec", i, "bash", "-c", "ls /dev/input"]);
    let (la, lb) = (list(A), list(B));
    for n in &a_nodes {
        let base = Path::new(n).file_name().unwrap().to_str().unwrap();
        assert!(la.contains(base) && !lb.contains(base), "{base}: A has it, B doesn't (A: {la}; B: {lb})");
    }
    for n in &b_nodes {
        let base = Path::new(n).file_name().unwrap().to_str().unwrap();
        assert!(lb.contains(base) && !la.contains(base), "{base}: B has it, A doesn't");
    }
    let (good, _) = quiver(&root, &["exec", B, "python3", "-c", "import os,sys; os.open(sys.argv[1], os.O_RDONLY)", &a_nodes[0]]);
    assert!(!good, "B's container can't open A's pad");

    // 7. Teardown.
    let pid = pad_status(&root, B)["host"]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    wait_for("a killed pad host's pads gone", Duration::from_secs(3), || pads_of(B) == 0);
    ok(&root, &["down", B]);
    ok(&root, &["down", A]);
    assert_eq!(pads_of(A) + pads_of(B), 0, "down removes the pads: {:?}", lab_pads());
    // A pad host whose container never came (or left): the next up stops it.
    let stale = Command::new(env!("CARGO_BIN_EXE_quiver")).args(["lab", "pad-host", B]).env("QUIVER_LAB_ROOT", &root).spawn().unwrap();
    wait_for("the stale pad host's pad", Duration::from_secs(10), || pads_of(B) == 1);
    let up = ok(&root, &["up", A]);
    assert!(up.contains("stale lab pad"), "{up}");
    assert_eq!(pads_of(B), 0);
    drop(stale);
    ok(&root, &["down", A]);
}
