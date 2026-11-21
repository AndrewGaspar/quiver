//! The preload under Proton's Wayland driver (winewayland, `PROTON_ENABLE_WAYLAND=1`): the
//! Windows input probe maps its window on a fake compositor (tests/wayland/fakecomp.c, built
//! here with wayland-scanner and cc; a private socket in a private runtime dir, never the
//! desktop's), which sends it keys, buttons, motion, wheel and relative motion the test asks
//! for. Checked as for X11: closed, everything reaches the game; open, nothing does and the
//! panel's ring gets it; a key the game held at open is released to it; keys pressed in the
//! panel never are; a dead panel process gives input back.
//!
//! Opt-in:
//!
//! `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 cargo test
//! -p quiver-sight-input --test wayland_input -- --ignored --nocapture`
//!
//! Needs `x86_64-w64-mingw32-gcc`, `cc`, `wayland-scanner`, wayland-protocols and the
//! libwayland-server and libxkbcommon development files.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use common::*;
use quiver_perf::panel::{EV_BUTTON, EV_KEY, EV_MOTION, EV_POINTER, EV_SCROLL, EVF_WAYLAND};

const SETTLE: Duration = Duration::from_millis(400);

/// evdev codes.
const KEY_W: u32 = 17;
const KEY_A: u32 = 30;
const KEY_B: u32 = 48;
const KEY_C: u32 = 46;
const KEY_D: u32 = 32;
const KEY_E: u32 = 18;
const KEY_F: u32 = 33;
const KEY_ESC: u32 = 1;
const BTN_LEFT: u32 = 0x110;

fn build_fakecomp(out: &Path) -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wayland");
    let protos = [
        "stable/xdg-shell/xdg-shell.xml",
        "stable/viewporter/viewporter.xml",
        "unstable/relative-pointer/relative-pointer-unstable-v1.xml",
        "unstable/pointer-constraints/pointer-constraints-unstable-v1.xml",
    ];
    let mut sources = vec![here.join("fakecomp.c")];
    for p in protos {
        let xml = Path::new("/usr/share/wayland-protocols").join(p);
        let stem = xml.file_stem().unwrap().to_str().unwrap().to_owned();
        let header = out.join(format!("{stem}-server-protocol.h"));
        let code = out.join(format!("{stem}-protocol.c"));
        for (mode, dst) in [("server-header", &header), ("private-code", &code)] {
            assert!(Command::new("wayland-scanner").arg(mode).arg(&xml).arg(dst).status().expect("wayland-scanner").success());
        }
        sources.push(code);
    }
    let exe = out.join("fakecomp");
    let flags = String::from_utf8(Command::new("pkg-config").args(["--cflags", "--libs", "wayland-server", "xkbcommon"]).output().expect("pkg-config").stdout).unwrap();
    let st = Command::new("cc").args(["-O1", "-Wall", "-o"]).arg(&exe).arg("-I").arg(out).args(&sources).args(flags.split_whitespace()).status().expect("cc");
    assert!(st.success(), "building the fake compositor");
    exe
}

struct Comp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines,
}

impl Comp {
    fn start(exe: &Path, runtime: &Path, socket: &str) -> Comp {
        let mut child = Command::new(exe).arg(socket).env("XDG_RUNTIME_DIR", runtime).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = Lines::default();
        let l2 = lines.clone();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                l2.0.lock().unwrap().push(l);
            }
        });
        assert!(lines.wait_for(|l| l == "READY", Duration::from_secs(5)));
        Comp { child, stdin, lines }
    }

    fn send(&mut self, cmd: &str) {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.stdin.flush().unwrap();
        std::thread::sleep(Duration::from_millis(15));
    }

    fn key(&mut self, code: u32, down: bool) {
        self.send(&format!("key {code} {}", down as u8));
    }

    fn tap(&mut self, code: u32) {
        self.key(code, true);
        self.key(code, false);
    }
}

impl Drop for Comp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn report(probe: &Probe, comp: &Comp, from: usize) -> String {
    format!("probe since: {:?}\ncompositor: {:?}\npreload: {:?}\nstderr tail: {:?}", probe.lines.since(from), comp.lines.since(0), probe.preload_log(), {
        let e = probe.stderr.lock().unwrap();
        e[e.len().saturating_sub(60)..].to_vec()
    })
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, wayland-scanner and libwayland-server; see the module docs"]
fn wayland_capture_and_release() {
    let scratch = tempfile::tempdir().unwrap();
    let runtime = scratch.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let exe = build_fakecomp(scratch.path());
    let socket = "wayland-quiver-test";
    let mut comp = Comp::start(&exe, &runtime, socket);
    let sight = FakeSight::new();
    let probe = Probe::start_on(Disp::Wayland { runtime: &runtime, socket }, &sight, 120, 0, scratch.path(), &[]);
    assert!(probe.lines.wait_for(|l| l.starts_with("READY"), Duration::from_secs(90)), "the probe didn't start: {}", report(&probe, &comp, 0));
    assert!(comp.lines.wait_for(|l| l.starts_with("MAPPED"), Duration::from_secs(20)), "no window mapped: {}", report(&probe, &comp, 0));
    assert!(comp.lines.wait_for(|l| l == "KEYBOARD", Duration::from_secs(5)) && comp.lines.wait_for(|l| l == "POINTER", Duration::from_secs(5)));
    comp.send("focus");
    std::thread::sleep(Duration::from_millis(500));

    // Closed: everything reaches the game.
    let m = probe.lines.len();
    comp.tap(KEY_A);
    comp.send("motion 200 150");
    comp.send(&format!("button {BTN_LEFT} 1"));
    comp.send(&format!("button {BTN_LEFT} 0"));
    comp.send("axis 1");
    std::thread::sleep(SETTLE);
    let got = probe.lines.input_since(m);
    for want in [keydown(vk::A), keyup(vk::A), "MOVE 200 150".into(), "BUTTON L DOWN".into(), "BUTTON L UP".into(), "WHEEL".into()] {
        assert!(got.iter().any(|l| l.starts_with(&want)), "{want} didn't reach the game: {}", report(&probe, &comp, m));
    }
    let now = quiver_perf::clock::mono_raw_ns();
    assert!(sight.panel.input().ready(now) && sight.panel.input().flags & quiver_perf::panel::INPUT_WAYLAND != 0, "{:?}", sight.panel.input());

    // W held when the panel opens.
    comp.key(KEY_W, true);
    std::thread::sleep(SETTLE);
    let m = probe.lines.len();
    let mut reader = sight.open(std::process::id());
    std::thread::sleep(Duration::from_millis(100));
    comp.tap(KEY_B);
    comp.tap(KEY_ESC);
    comp.send("motion 300 250");
    comp.send("motion 310 255");
    comp.send(&format!("button {BTN_LEFT} 1"));
    comp.send(&format!("button {BTN_LEFT} 0"));
    comp.send("axis 2");
    comp.send("axis 3 1"); // Finger scrolling has no wheel detents.
    comp.key(KEY_C, true); // held across the close
    std::thread::sleep(SETTLE);
    // Wine repeats W itself (client-side key repeat): the game holds it, as it should until
    // the release.
    let got: Vec<_> = probe.lines.input_since(m).into_iter().filter(|l| !l.starts_with(&keydown(vk::W)) && l != "RAWKEY 0x57 DOWN").collect();
    assert!(got.is_empty(), "the game got input while the panel was open: {got:?}\n{}", report(&probe, &comp, m));
    assert!(sight.wait_capturing(true, Duration::from_millis(100)));
    let ev = sight.events(&mut reader);
    let keys: Vec<(u32, bool)> = ev.iter().filter(|e| e.kind == EV_KEY).map(|e| (e.code, e.down())).collect();
    assert_eq!(keys, vec![(KEY_B, true), (KEY_B, false), (KEY_ESC, true), (KEY_ESC, false), (KEY_C, true)], "{ev:?}");
    assert!(ev.iter().all(|e| e.flags & EVF_WAYLAND != 0 || e.kind == EV_SCROLL));
    assert!(ev.iter().any(|e| e.kind == EV_BUTTON && e.code == BTN_LEFT && e.down()));
    assert!(ev.iter().any(|e| e.kind == EV_POINTER && e.xy() == (310.0, 255.0)), "{ev:?}");
    assert_eq!(ev.iter().filter(|e| e.kind == EV_SCROLL).map(|e| e.xy().1).collect::<Vec<_>>(), [2.0, 3.0]);
    // W's release goes to the game.
    let m = probe.lines.len();
    comp.key(KEY_W, false);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with(&keyup(vk::W))), "W's release didn't reach the game: {}", report(&probe, &comp, m));
    assert!(!probe.lines.held_after(m, Duration::from_secs(1)).unwrap().contains(&vk::W), "W stuck");

    // Relative motion (a relative pointer exists once the game clips the cursor; the probe
    // doesn't, so only if Wine made one).
    if comp.lines.since(0).iter().any(|l| l == "RELATIVE") {
        comp.send("rel 7 -3");
        std::thread::sleep(SETTLE);
        assert!(sight.events(&mut reader).iter().any(|e| e.kind == EV_MOTION && e.xy() == (7.0, -3.0)));
    }

    // Close with C held: its release never reaches the game; then everything passes.
    sight.close();
    let m = probe.lines.len();
    comp.key(KEY_C, false);
    comp.tap(KEY_D);
    std::thread::sleep(SETTLE);
    let got = probe.lines.input_since(m);
    assert!(!got.iter().any(|l| l.starts_with("KEY") && l.contains(&format!("0x{:02x}", vk::C))), "C reached the game: {got:?}");
    assert!(got.iter().any(|l| l.starts_with(&keydown(vk::D))) && got.iter().any(|l| l.starts_with(&keyup(vk::D))), "{}", report(&probe, &comp, m));
    let held = probe.lines.held_after(m + 1, Duration::from_secs(1)).unwrap();
    assert!(held.iter().all(|k| ![vk::W, vk::B, vk::C, vk::ESC].contains(k)), "stuck keys after close: {held:x?}");

    // Failsafe: the panel's process dies.
    let mut owner = Command::new("sleep").arg("60").spawn().unwrap();
    let _r = sight.open(owner.id());
    std::thread::sleep(Duration::from_millis(100));
    let m = probe.lines.len();
    comp.tap(KEY_E);
    std::thread::sleep(SETTLE);
    assert!(!probe.lines.input_since(m).iter().any(|l| l.starts_with("KEY")), "{}", report(&probe, &comp, m));
    owner.kill().unwrap();
    owner.wait().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let m = probe.lines.len();
    comp.tap(KEY_F);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with(&keydown(vk::F))), "input not back after the panel's process died: {}", report(&probe, &comp, m));
    sight.close();
    eprintln!("panel stats: {:?}", sight.panel.input());
}
