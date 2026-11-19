//! The preload under Proton's X11 driver (winex11): a Windows program (tests/wine/input_probe.c)
//! reports every key and mouse message it gets, on a private Xvfb, with keys and the mouse
//! injected with XTEST; a stand-in for quiver-sight and the layer opens and closes the panel
//! in `panel.input`. Checked:
//!
//! 1. closed: everything reaches the game;
//! 2. open: the game gets no key, button, wheel, motion or raw-input message, and the panel's
//!    ring gets the keys, buttons, wheel and pointer positions; a key the game held when the
//!    panel opened is released to it (nothing stuck), keys pressed in the panel are never
//!    released to it, even after close;
//! 3. failsafe: input goes back to the game when the panel's process dies, when its deadline
//!    stops being renewed, and when the layer stops drawing the panel;
//! 4. with the cursor clipped and hidden (Wine grabs the pointer and selects XInput2 raw
//!    motion): no WM_INPUT while open; a game re-centring the pointer every 50 ms doesn't
//!    move it while open (its XWarpPointer is dropped).
//!
//! Opt-in:
//!
//! `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 cargo test
//! -p quiver-sight-input --test wine_input -- --ignored --nocapture --test-threads=1`
//!
//! Needs `x86_64-w64-mingw32-gcc` and Xvfb. The prefix is kept in
//! `target/quiver-sight-wine-input` between runs.

mod common;

use std::time::Duration;

use common::*;
use quiver_perf::panel::{EV_BUTTON, EV_KEY, EV_POINTER, EV_SCROLL};

const SETTLE: Duration = Duration::from_millis(400);

fn start(flags: u32) -> (xvfb::Xvfb, FakeSight, Probe, Fake, tempfile::TempDir) {
    let x = xvfb::Xvfb::start_size("1024x768x24").expect("Xvfb");
    let sight = FakeSight::new();
    let scratch = tempfile::tempdir().unwrap();
    let probe = Probe::start(&x.display, &sight, 120, flags, scratch.path(), &[]);
    assert!(probe.lines.wait_for(|l| l.starts_with("READY"), Duration::from_secs(180)), "the probe didn't start: {:?}", probe.stderr.lock().unwrap());
    let fake = Fake::new(&x.display);
    // The pointer over the window (no window manager: keys go to the window under it).
    fake.move_to(500, 400);
    std::thread::sleep(Duration::from_millis(500));
    (x, sight, probe, fake, scratch)
}

fn report(probe: &Probe, from: usize) -> String {
    format!("probe since: {:?}\npreload: {:?}", probe.lines.since(from), probe.preload_log())
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON and Xvfb; see the module docs"]
fn x11_capture_and_release() {
    let (_x, sight, probe, fake, _s) = start(0);

    // 1. Closed: keys, clicks, motion reach the game.
    let m = probe.lines.len();
    fake.tap(key::A);
    fake.click(1);
    fake.move_to(520, 410);
    std::thread::sleep(SETTLE);
    let got = probe.lines.input_since(m);
    assert!(got.iter().any(|l| l.starts_with(&keydown(vk::A))) && got.iter().any(|l| l.starts_with(&keyup(vk::A))), "{}", report(&probe, m));
    assert!(got.iter().any(|l| l.starts_with("BUTTON L DOWN")) && got.iter().any(|l| l.starts_with("BUTTON L UP")), "{}", report(&probe, m));
    assert!(got.iter().any(|l| l.starts_with("MOVE")), "{}", report(&probe, m));
    assert!(sight.panel.input().ready(quiver_perf::clock::mono_raw_ns()), "the preload didn't report in panel.input: {:?}", sight.panel.input());

    // 2. Shift+F2 held, and W held, when the panel opens.
    fake.key(key::SHIFT_L, true);
    fake.key(key::F2, true);
    fake.key(key::W, true);
    std::thread::sleep(SETTLE);
    let m = probe.lines.len();
    let mut reader = sight.open(std::process::id());
    // The preload notices at the next X event Wine reads (or its thread, within 50 ms).
    fake.move_to(530, 420);
    assert!(sight.wait_capturing(true, Duration::from_secs(2)), "the preload never captured: {}", report(&probe, m));
    fake.key(key::F2, false);
    fake.key(key::SHIFT_L, false);
    fake.tap(key::B);
    fake.tap(key::ESC);
    fake.click(1);
    fake.click(3);
    fake.click(5); // wheel down
    fake.move_to(600, 500);
    fake.move_to(610, 505);
    fake.key(key::C, true); // held across the close
    std::thread::sleep(SETTLE);
    let got = probe.lines.input_since(m);
    // The releases of what the game held pass; nothing else.
    let allowed = |l: &String| l.starts_with(&keyup(vk::F2)) || l.starts_with(&keyup(vk::SHIFT)) || l.starts_with("MOVE 530 420") || l.starts_with("RAWKEY 0x71 UP") || l.starts_with("RAWKEY 0x10 UP");
    let leaked: Vec<_> = got.iter().filter(|l| !allowed(l)).collect();
    assert!(leaked.is_empty(), "the game got input while the panel was open: {leaked:?}\n{}", report(&probe, m));
    assert!(got.iter().any(|l| l.starts_with(&keyup(vk::F2))) && got.iter().any(|l| l.starts_with(&keyup(vk::SHIFT))), "Shift/F2 releases didn't reach the game: {}", report(&probe, m));
    let ev = sight.events(&mut reader);
    let keys: Vec<(u32, bool)> = ev.iter().filter(|e| e.kind == EV_KEY).map(|e| (e.code, e.down())).collect();
    // evdev codes: B 48, Esc 1, C 46.
    assert_eq!(keys, vec![(48, true), (48, false), (1, true), (1, false), (46, true)], "keys the panel got: {ev:?}");
    let buttons: Vec<(u32, bool)> = ev.iter().filter(|e| e.kind == EV_BUTTON).map(|e| (e.code, e.down())).collect();
    assert_eq!(buttons, vec![(0x110, true), (0x110, false), (0x111, true), (0x111, false)]);
    assert!(ev.iter().any(|e| e.kind == EV_SCROLL && e.xy() == (0.0, 1.0)), "wheel: {ev:?}");
    assert!(ev.iter().any(|e| e.kind == EV_POINTER && (e.a, e.b) == (610, 505)), "pointer: {ev:?}");
    // W, held since before the panel, is still down for the game; its release goes to it.
    assert!(probe.lines.held_after(m, Duration::from_secs(1)).unwrap().contains(&vk::W));
    let m = probe.lines.len();
    fake.key(key::W, false);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with(&keyup(vk::W))), "W's release didn't reach the game: {}", report(&probe, m));
    assert!(!probe.lines.held_after(m, Duration::from_secs(1)).unwrap().contains(&vk::W), "W stuck");

    // Close with C still held: C's release never reaches the game; then everything passes.
    sight.close();
    let m = probe.lines.len();
    fake.move_to(640, 480);
    assert!(sight.wait_capturing(false, Duration::from_secs(2)));
    fake.key(key::C, false);
    fake.tap(key::D);
    std::thread::sleep(SETTLE);
    let got = probe.lines.input_since(m);
    assert!(!got.iter().any(|l| l.contains(&format!("0x{:02x}", vk::C)) && l.starts_with("KEY")), "C (pressed in the panel) reached the game: {got:?}");
    assert!(got.iter().any(|l| l.starts_with(&keydown(vk::D))) && got.iter().any(|l| l.starts_with(&keyup(vk::D))), "{}", report(&probe, m));
    let held = probe.lines.held_after(m + 1, Duration::from_secs(1)).unwrap();
    assert!(held.iter().all(|k| ![vk::SHIFT, vk::F2, vk::W, vk::B, vk::C, vk::ESC].contains(k)), "stuck keys after close: {held:x?}");
    eprintln!("panel stats: {:?}", sight.panel.input());
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON and Xvfb; see the module docs"]
fn x11_failsafe() {
    let (_x, sight, probe, fake, _s) = start(0);
    // (a) The panel's process dies.
    let mut owner = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let _r = sight.open(owner.id());
    fake.move_to(510, 400);
    assert!(sight.wait_capturing(true, Duration::from_secs(2)));
    let m = probe.lines.len();
    fake.tap(key::E);
    std::thread::sleep(SETTLE);
    assert!(!probe.lines.input_since(m).iter().any(|l| l.starts_with("KEY")), "{}", report(&probe, m));
    owner.kill().unwrap();
    owner.wait().unwrap();
    let t = std::time::Instant::now();
    // The fake keeps renewing the deadline and drawing: only the pid says it's gone.
    let m = probe.lines.len();
    let mut released = false;
    while t.elapsed() < Duration::from_secs(2) {
        fake.move_to(500 + (t.elapsed().as_millis() % 50) as i16, 400);
        if sight.wait_capturing(false, Duration::from_millis(20)) {
            released = true;
            break;
        }
    }
    eprintln!("released {:?} after the owner died", t.elapsed());
    assert!(released && t.elapsed() < Duration::from_millis(500), "input not released after the panel's process died: {}", report(&probe, m));
    fake.tap(key::F);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with(&keydown(vk::F))), "{}", report(&probe, m));
    sight.close();

    // (b) The deadline isn't renewed any more (quiver-sight hung).
    let _r = sight.open(std::process::id());
    fake.move_to(520, 400);
    assert!(sight.wait_capturing(true, Duration::from_secs(2)));
    sight.renewing.store(false, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_secs(3) && !sight.wait_capturing(false, Duration::from_millis(50)) {
        fake.move_to(500 + (t.elapsed().as_millis() % 50) as i16, 410);
    }
    eprintln!("released {:?} after renewals stopped", t.elapsed());
    assert!(t.elapsed() < Duration::from_millis(1300), "deadline failsafe too slow");
    let m = probe.lines.len();
    fake.tap(key::G);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with(&keydown(vk::G))), "{}", report(&probe, m));
    sight.close();

    // (c) The layer stops drawing the panel (the game stopped presenting).
    let _r = sight.open(std::process::id());
    fake.move_to(530, 400);
    assert!(sight.wait_capturing(true, Duration::from_secs(2)));
    sight.drawing.store(false, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_secs(3) && !sight.wait_capturing(false, Duration::from_millis(50)) {
        fake.move_to(500 + (t.elapsed().as_millis() % 50) as i16, 420);
    }
    eprintln!("released {:?} after the layer stopped drawing", t.elapsed());
    assert!(t.elapsed() < Duration::from_millis(1700), "drawn failsafe too slow");
    let m = probe.lines.len();
    fake.tap(key::A);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with(&keydown(vk::A))), "{}", report(&probe, m));
    sight.close();
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON and Xvfb; see the module docs"]
fn x11_clipped_raw_input_and_warps() {
    // Clipped and hidden (XInput2 raw motion in Wine), and re-centring every 50 ms.
    let (_x, sight, probe, fake, _s) = start(1 | 2);
    let m = probe.lines.len();
    fake.move_by(30, 10);
    fake.move_by(30, 10);
    std::thread::sleep(SETTLE);
    let got = probe.lines.input_since(m);
    assert!(got.iter().any(|l| l.starts_with("RAWMOUSE")), "no raw input while closed (the probe's setup?): {}", report(&probe, m));
    let _r = sight.open(std::process::id());
    fake.move_by(5, 0);
    assert!(sight.wait_capturing(true, Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(200));
    let mut reader = quiver_perf::panel::EventReader::at_end(&sight.panel);
    let m = probe.lines.len();
    for _ in 0..10 {
        fake.move_by(25, 15);
        std::thread::sleep(Duration::from_millis(30));
    }
    fake.click(1);
    std::thread::sleep(SETTLE);
    // WM_MOUSEMOVE at the centre is the game's own SetCursorPos (win32u queues one for each),
    // not the user's input.
    let got: Vec<_> = probe.lines.input_since(m).into_iter().filter(|l| l != "MOVE 512 384").collect();
    assert!(got.is_empty(), "the game got input while open: {got:?}\n{}", report(&probe, m));
    let ev = sight.events(&mut reader);
    let pos: Vec<(i32, i32)> = ev.iter().filter(|e| e.kind == EV_POINTER).map(|e| (e.a, e.b)).collect();
    eprintln!("pointer positions while open: {pos:?}");
    assert!(pos.len() >= 5, "{ev:?}");
    // Not dragged back to the centre (512, 384) by the game's SetCursorPos: it moves on.
    let (first, last) = (pos[0], pos[pos.len() - 1]);
    assert!(last.0 - first.0 >= 150 && last.1 - first.1 >= 90, "the pointer didn't follow the motion: {pos:?}");
    assert!(ev.iter().any(|e| e.kind == EV_BUTTON && e.code == 0x110 && e.down()));
    sight.close();
    let m = probe.lines.len();
    fake.move_by(10, 0);
    fake.move_by(10, 0);
    std::thread::sleep(SETTLE);
    assert!(probe.lines.input_since(m).iter().any(|l| l.starts_with("RAWMOUSE")), "raw input didn't come back: {}", report(&probe, m));
}
