//! The virtual clock in a real Wine process: a stand-in game (tests/wine/clock_test.c) presents
//! through DXVK (Direct3D 11 or 9) on a private Xvfb and reads the clocks through its own
//! imports, with this build's hook DLL next to it as `version.dll` (as `quiver lab` places it).
//! Checks, per case:
//!
//! - every step between frames the game read is exactly one frame: 1/62.5 s on the performance
//!   counter (160000 ticks at QPF 10 MHz) and 16 ms on timeGetTime, GetTickCount and
//!   GetTickCount64, and the DLL's own census agrees;
//! - fast presenter: the game's time runs faster than real time; slow presenter (frames held
//!   to 25 ms real, 40 fps, below the virtual 62.5 Hz): slower than real time, and still
//!   exactly 1/62.5 s per frame;
//! - the virtual-time sequence is identical across runs (fast, fast again, slow);
//! - a QPC busy-wait for two frames and a `Sleep(1)` loop on timeGetTime on the presenting
//!   thread, and a worker's QPC busy-wait while the presenting thread waits for it, all
//!   terminate (the creep, counted: exactly those 70 frames);
//! - background threads that poll the clock with `Sleep(1)` and `Sleep(100)` throughout keep
//!   their real sleeps and don't make the game's frames creep (AC Chronicles China's and
//!   Rabbids Coding's first live runs: half their frames crept); with every sleep shortened
//!   (`QUIVER_VCLOCK_SLEEPS=all`, the old behaviour) they do, which the `all` case shows;
//! - a swap chain made from the device's own factory (D3D11CreateDevice, IDXGIDevice ->
//!   adapter -> parent, as Unity does) is hooked;
//! - a second process loading the proxy that never presents (Unity's crash handler, in Rabbids
//!   Coding's run) doesn't replace the game's report;
//! - QueryPerformanceCounter looked up with GetProcAddress gives the virtual value;
//! - only the executable's clock imports point into the hook DLL: none of Wine's modules or
//!   DXVK's;
//! - no real-time escape fired (the run is deterministic);
//! - (`another_modules_hooks_stay_in_the_chain`) another module's hooks between the clock's
//!   (Pharos's d3d11.dll proxy in AC Origins) stay in the chain: hooked over the clock's
//!   CreateSwapChain before the clock is asked to hook the same factory vtable again, they run
//!   once per call, see the swap chain, and their Present runs once per frame, which the clock
//!   still counts exactly once.
//!
//! Opt-in:
//!
//! `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 cargo test
//! -p quiver-vclock --test wine -- --ignored --nocapture --test-threads 1`
//!
//! Needs mingw-w64 (x86_64 and, for the 32-bit case, i686) and Xvfb. The prefix is kept in
//! `target/quiver-vclock-wine` between runs. `QUIVER_TEST_VCLOCK_32=0` skips the 32-bit case.

#[path = "../../quiver-sight-layer/tests/xvfb/mod.rs"]
mod xvfb;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use quiver_vclock::Report;

const RATE: &str = "62.5";
const TPF: i64 = 160_000;
/// The Right arrow's keycode in Xvfb's default (evdev) keymap.
const RIGHT: u8 = 114;

fn proton() -> PathBuf {
    PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"))
}

fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

/// The test exe and the DLLs it runs with, in a fresh folder: DXVK app-local (`runinprefix`
/// doesn't install it) and the hook DLL as version.dll.
fn stage(dir: &Path, arch: &str) -> PathBuf {
    let (gcc, dxvk) = if arch == "x64" { ("x86_64-w64-mingw32-gcc", "x86_64-windows") } else { ("i686-w64-mingw32-gcc", "i386-windows") };
    let exe = dir.join("clock_test.exe");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wine/clock_test.c");
    let st = Command::new(gcc)
        .args(["-O2", "-Wall", "-o"])
        .arg(&exe)
        .arg(&src)
        .args(["-ld3d11", "-ldxgi", "-ld3d9", "-lversion", "-lwinmm", "-lpsapi", "-luuid", "-ldxguid"])
        .status()
        .expect(gcc);
    assert!(st.success(), "{gcc} failed");
    std::fs::copy(&exe, dir.join("helper.exe")).unwrap();
    for dll in ["d3d11.dll", "dxgi.dll", "d3d9.dll"] {
        std::fs::copy(proton().join("files/lib/wine/dxvk").join(dxvk).join(dll), dir.join(dll)).expect("Proton's DXVK");
    }
    std::fs::write(dir.join("version.dll"), quiver_vclock::dll(arch, "version").expect("the hook DLL (mingw at build time)")).unwrap();
    exe
}

struct Out {
    lines: BTreeMap<String, String>,
    report: Report,
    stdout: String,
    #[allow(dead_code)]
    log: String,
}

impl Out {
    fn get(&self, k: &str) -> &str {
        self.lines.get(k).map(String::as_str).unwrap_or_else(|| panic!("no '{k}' line in\n{}", self.stdout))
    }
    /// `A frames 1198 virtual_s 19.168 real_s 3.2` -> the value after `key`.
    fn num(&self, line: &str, key: &str) -> f64 {
        let l = self.get(line);
        let mut w = l.split_whitespace();
        while let Some(x) = w.next() {
            if x == key {
                return w.next().unwrap().parse().unwrap();
            }
        }
        panic!("no {key} in '{line} {l}'")
    }
    /// The census line `A qpc_steps 160000:1197` as (value, count).
    fn steps(&self, line: &str) -> Vec<(i64, u64)> {
        self.get(line)
            .split_whitespace()
            .map(|b| {
                let (v, n) = b.split_once(':').unwrap();
                (v.parse().unwrap_or(i64::MIN), n.parse().unwrap())
            })
            .collect()
    }
}

struct Case<'a> {
    arch: &'a str,
    api: &'a str,
    frames: u32,
    slow_ms: u32,
    tag: &'a str,
    /// Start helper.exe (a second process with the proxy) first.
    helper: bool,
    /// QUIVER_VCLOCK_SLEEPS.
    sleeps: Option<&'a str>,
    /// QUIVER_TEST_KEYS: report the Right arrow per frame.
    keys: bool,
}

impl Default for Case<'_> {
    fn default() -> Self {
        Case { arch: "x64", api: "d3d11", frames: 1200, slow_ms: 0, tag: "", helper: false, sleeps: None, keys: false }
    }
}

fn run(display: &str, c: &Case) -> Out {
    run_with(display, c, None)
}

/// `run`, with a controller acting on the report folder while the game runs (pause and step).
fn run_with(display: &str, c: &Case, controller: Option<&dyn Fn(&Path)>) -> Out {
    let Case { arch, api, frames, slow_ms, tag, .. } = *c;
    let scratch = tempfile::tempdir().unwrap();
    let t = scratch.path();
    let exe = stage(t, arch);
    let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/quiver-vclock-wine");
    let compat = keep.join("compat");
    std::fs::create_dir_all(&compat).unwrap();
    for d in ["home", "steam", "out"] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    let title = format!("quiver-vclock-{tag}-{}", std::process::id());
    let mut cmd = Command::new("python3");
    cmd
        .arg(proton().join("proton"))
        .args(["runinprefix", &win(&exe), api, &frames.to_string(), &slow_ms.to_string(), &title])
        .args(c.helper.then(|| win(&t.join("helper.exe"))))
        .env("QUIVER_VCLOCK_EXE", "clock_test.exe")
        .env("QUIVER_VCLOCK_SLEEPS", c.sleeps.unwrap_or("present"))
        .env(if c.keys { "QUIVER_TEST_KEYS" } else { "QUIVER_TEST_NO_KEYS" }, "1")
        .current_dir(t)
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
        .env("DISPLAY", display)
        .env("WINEDEBUG", "-all")
        .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=;d3d11,dxgi,d3d9=n;version=n,b")
        .env("DXVK_LOG_LEVEL", "none")
        .env("DXVK_CONFIG", "dxgi.syncInterval = 0; d3d9.presentInterval = 0; dxgi.maxFrameRate = 0; d3d9.maxFrameRate = 0")
        .env("QUIVER_VCLOCK", RATE)
        .env("QUIVER_VCLOCK_DIR", win(&t.join("out")))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("PROTON_ENABLE_WAYLAND");
    let out = match controller {
        None => cmd.output().unwrap(),
        Some(f) => {
            let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&t.join("out")))) {
                // A held game waits for ever by design: don't leave it behind.
                let _ = Command::new(proton().join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", compat.join("pfx")).status();
                let _ = child.kill();
                std::panic::resume_unwind(e);
            }
            child.wait_with_output().unwrap()
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let log = std::fs::read_to_string(t.join("out").join(quiver_vclock::LOG_FILE)).unwrap_or_default();
    // The helper process's lines would only say it isn't the game.
    if c.helper {
        assert!(log.contains("this isn't the game's executable (clock_test.exe)"), "{log}");
    }
    eprintln!("--- {tag} ({arch} {api}, {frames} frames, slow {slow_ms} ms)\n{stdout}--- vclock.log\n{log}");
    if !stdout.contains("done") {
        eprintln!("stderr:\n{}", String::from_utf8_lossy(&out.stderr));
        panic!("{tag}: the test exe didn't finish");
    }
    let report = Report::read(&t.join("out")).expect("vclock.status");
    eprintln!("report: {}\n  {}\n  {}\n  verdict: {:?}", report.summary(), report.counters(), report.census().join("\n  "), report.verdict());
    let mut lines = BTreeMap::new();
    for l in stdout.lines() {
        let mut w = l.splitn(3, ' ');
        let (a, b, rest) = (w.next().unwrap_or(""), w.next().unwrap_or(""), w.next().unwrap_or(""));
        match a {
            "A" | "B" | "C" | "D" | "keys" | "chain" => lines.insert(format!("{a} {b}"), rest.to_owned()),
            "iat" => lines.insert(format!("iat {b}"), rest.to_owned()),
            _ => lines.insert(a.to_owned(), format!("{b} {rest}").trim().to_owned()),
        };
    }
    Out { lines, report, stdout, log }
}

/// The controller of the stepped case, as `quiver lab clock` drives it: pause as soon as the
/// game's process maps the page, hold 3 s, step, resume. Returns (frame, virtual s) after each.
/// Gives the X input focus to the test window titled `title` (there is no window manager in
/// the Xvfb to do it), so the keys pressed go to the game.
fn focus(display: &str, title: &str) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, InputFocus};
    use x11rb::wrapper::ConnectionExt as _;
    let (conn, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&std::ffi::CString::new(display).unwrap())).unwrap();
    let root = conn.setup().roots[screen].root;
    let mut todo = vec![root];
    while let Some(w) = todo.pop() {
        let name = conn.get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 256).unwrap().reply().map(|r| r.value).unwrap_or_default();
        if name == title.as_bytes() {
            conn.set_input_focus(InputFocus::PARENT, w, x11rb::CURRENT_TIME).unwrap();
            conn.sync().unwrap();
            return;
        }
        if let Ok(t) = conn.query_tree(w).unwrap().reply() {
            todo.extend(t.children);
        }
    }
    panic!("no window titled {title} on {display}");
}

fn control(dir: &Path, kb: Option<(&xvfb::Keyboard, &str, &str)>) -> Vec<(u64, f64)> {
    use quiver_vclock::control::{Cmd, Control, Snapshot};
    use std::time::{Duration, Instant};
    let end = Instant::now() + Duration::from_secs(120);
    let ctl = loop {
        if let Ok(c) = Control::open(dir) {
            break c;
        }
        assert!(Instant::now() < end, "no control page in {}", dir.display());
        std::thread::sleep(Duration::from_millis(5));
    };
    let wait = |seq: u64, ok: &dyn Fn(&Snapshot) -> bool| {
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let s = ctl.snapshot();
            if s.ack_seq >= seq && ok(&s) {
                return s;
            }
            assert!(Instant::now() < end, "timed out: {s:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    let mut out = Vec::new();
    let s = wait(ctl.send(Cmd::Pause, 0, 0), &|s| s.held());
    out.push((s.frames, s.virtual_s));
    std::thread::sleep(Duration::from_secs(3));
    let s2 = ctl.snapshot();
    assert_eq!((s2.frames, s2.vticks), (s.frames, s.vticks), "time moved while paused");
    assert!(s2.held_s >= 2.5, "held time is published every 250 ms: {s2:?}");
    let mut at = s.frames;
    for (n, k) in [1u64, 12, 1, 100].into_iter().enumerate() {
        // The Right arrow pressed while held, across the first two steps (13 frames), released
        // while held, before the third.
        if let Some((kb, display, title)) = kb
            && (n == 0 || n == 2)
        {
            if n == 0 {
                focus(display, title);
            }
            kb.key(RIGHT, n == 0);
            std::thread::sleep(Duration::from_millis(300));
        }
        let s = wait(ctl.send(Cmd::Step, k, 0), &|s| s.held() && s.frames >= s.ack_hold_at);
        assert_eq!(s.frames, at + k, "step {k}");
        assert_eq!(s.vticks, s.frames as i64 * TPF, "step {k}: exactly 1/62.5 s per frame");
        at = s.frames;
        out.push((s.frames, s.virtual_s));
    }
    wait(ctl.send(Cmd::Resume, 0, 0), &|s| s.running());
    out
}

/// What every case checks.
fn check(o: &Out, frames: u32, api: &str) {
    assert_eq!(o.num("qpf", "real_qpf") as i64, quiver_vclock::WINE_QPF, "Wine's QPC runs at 10 MHz");
    assert_eq!(o.get("qpf").split_whitespace().next().unwrap(), "10000000", "62.5 Hz: QPF stays 10 MHz, 160000 ticks a frame");
    // Exactly one frame per step, on every clock.
    let n = (frames - 2) as u64;
    assert_eq!(o.steps("A qpc_steps"), vec![(TPF, n)]);
    for l in ["A ms_steps", "A tick_steps", "A tick64_steps"] {
        assert_eq!(o.steps(l), vec![(16, n)], "{l}");
    }
    let v = o.num("A frames", "virtual_s");
    assert!((v - (frames - 2) as f64 * 0.016).abs() < 1e-9, "virtual time {v}");
    assert_eq!(o.get("gpa_virtual"), "1", "GetProcAddress(QueryPerformanceCounter) is the virtual clock");
    assert!(o.lines.contains_key("B frames") && o.lines.contains_key("C frames") && o.lines.contains_key("D frames"), "the waits terminated");
    // Only the exe's clock imports are ours.
    let iats: Vec<(&String, &String)> = o.lines.iter().filter(|(k, _)| k.starts_with("iat ")).collect();
    assert!(iats.len() > 5, "{iats:?}");
    for (k, v) in &iats {
        let hooked: u32 = v.split_whitespace().next().unwrap().parse().unwrap();
        if v.ends_with(" exe") {
            assert!(hooked >= 5, "the exe's clock imports: {k} {v}");
        } else {
            assert_eq!(hooked, 0, "{k}: a module that isn't the game's has hooked clock imports");
        }
    }
    let r = &o.report;
    assert!(r.engaged && r.present_path == if api == "d3d9" { "d3d9" } else { "dxgi" }, "{r:?}");
    assert!(r.deterministic(), "an escape fired: {}", r.counters());
    // Exactly the frames that busy-wait crept: B's 30, C's 30, D's 10. The background pollers
    // kept their real sleeps and crept nothing.
    assert_eq!(r.creep_frames, 70, "{}", r.counters());
    assert!(r.sleeps_real > 100, "the pollers' sleeps stay real: {}", r.counters());
    // The DLL's own census: frame steps, plus the creep's small steps from B and C.
    assert!(r.census_qpc.iter().any(|b| b.0 == TPF && b.1 >= n), "{:?}", r.census_qpc);
    assert!(r.census_ms.iter().any(|b| b.0 == 16 && b.1 >= n), "{:?}", r.census_ms);
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, mingw-w64 and Xvfb; see the module docs"]
fn the_game_sees_exactly_one_frame_per_present() {
    let x = xvfb::Xvfb::start_size("640x480x24").expect("Xvfb");
    let mut hashes = Vec::new();

    // A presenter faster than 62.5 Hz: faster than real time. With a second process loading
    // the proxy (a crash handler's place).
    let fast = run(&x.display, &Case { tag: "fast", helper: true, ..Default::default() });
    check(&fast, 1200, "d3d11");
    let (v, r) = (fast.num("A frames", "virtual_s"), fast.num("A frames", "real_s"));
    eprintln!("fast: {v:.3} s virtual in {r:.3} s real = {:.2}x", v / r);
    assert!(v > r, "the fast presenter wasn't faster than real time ({v} virtual, {r} real)");
    hashes.push(fast.get("A sequence_hash").to_owned());

    // Paused from the start through the control page, held for real seconds with the pollers
    // reading, stepped 1, 12, 1 and 100 frames, resumed: the game sees the same time as the
    // fast run, frame for frame.
    let log = std::cell::RefCell::new(Vec::new());
    let kb = x.keyboard();
    let title = format!("quiver-vclock-stepped-{}", std::process::id());
    let ctrl = |dir: &Path| log.borrow_mut().extend(control(dir, Some((&kb, &x.display, &title))));
    let stepped = run_with(&x.display, &Case { tag: "stepped", keys: true, ..Default::default() }, Some(&ctrl));
    check(&stepped, 1200, "d3d11");
    assert_eq!(stepped.get("A sequence_hash"), hashes[0], "stepping changed the virtual-time sequence");
    let r = &stepped.report;
    eprintln!("stepped: {:?}\n  {}", log.borrow(), r.hold_line().unwrap_or_default());
    assert!(r.control && r.pauses == 1 && r.steps == 114 && r.holds == 5 && r.leaked_ticks == 0 && r.hold_state == "running", "{r:?}");
    assert!(r.held_s >= 2.9 && r.held_reads > 0, "the pollers read while held: {r:?}");
    hashes.push(stepped.get("A sequence_hash").to_owned());
    // Input sent while held is read when the game next steps, by every API: held at frame 1,
    // pressed, stepped 1 + 12, released, stepped: frames 2 to 14 saw it down.
    for api in ["keys messages", "keys raw", "keys async"] {
        let l = stepped.get(api);
        assert!(l.starts_with("13 first 2 last 14"), "{api}: {l}");
    }

    // The same again: the same virtual-time sequence.
    let again = run(&x.display, &Case { tag: "again", ..Default::default() });
    check(&again, 1200, "d3d11");
    hashes.push(again.get("A sequence_hash").to_owned());

    // A presenter held to 40 fps (below 62.5 Hz): slower than real time, the same game time.
    let slow = run(&x.display, &Case { tag: "slow", slow_ms: 25, ..Default::default() });
    check(&slow, 1200, "d3d11");
    let (v, r) = (slow.num("A frames", "virtual_s"), slow.num("A frames", "real_s"));
    eprintln!("slow: {v:.3} s virtual in {r:.3} s real = {:.2}x", v / r);
    assert!(v < r * 0.8, "the slow presenter's game time should run slower than real time ({v} virtual, {r} real)");
    assert!(slow.report.speedup < 1.0 && !slow.report.verdict().iter().any(|v| v.contains("slower")), "slower than real time is normal: {:?}", slow.report.verdict());
    hashes.push(slow.get("A sequence_hash").to_owned());
    assert!(hashes.iter().all(|h| *h == hashes[0]), "the virtual-time sequences differ between runs: {hashes:?}");
    // The DLL's own view agrees across the three runs too, the busy-waits included: the same
    // creep, the same overruns, the same shortened sleeps. (Not the census: the background
    // pollers sample the clock at real-time moments while phase C creeps it through
    // timeGetTime, so how the QPC steps they see bunch up differs. The game thread's values
    // don't: the sequence hash and the phase counts above.)
    for o in [&again, &slow] {
        let (a, b) = (&fast.report, &o.report);
        assert_eq!((a.creep_frames, a.overruns, a.sleeps, a.sleep_ms_requested), (b.creep_frames, b.overruns, b.sleeps, b.sleep_ms_requested));
        assert!(b.census_qpc.contains(&(TPF, a.census_qpc.iter().find(|s| s.0 == TPF).unwrap().1)), "the frame steps");
        assert_eq!(fast.get("C frames").split_whitespace().nth(2), o.get("C frames").split_whitespace().nth(2), "phase C's Sleep(1) count");
    }

    // The swap chain from the device's own factory (Unity's way).
    let dev = run(&x.display, &Case { tag: "devfactory", api: "d3d11dev", frames: 600, helper: true, ..Default::default() });
    check(&dev, 600, "d3d11dev");

    // The old behaviour, every sleep shortened: the background pollers spin on the clock and
    // the game's own frames creep (what the first live runs showed).
    let all = run(&x.display, &Case { tag: "sleeps-all", frames: 600, sleeps: Some("all"), ..Default::default() });
    let steps = all.steps("A qpc_steps");
    eprintln!("sleeps=all: phase A steps {steps:?}, {}", all.report.counters());
    assert!(steps != vec![(TPF, 598)] && all.report.creep_frames > 70, "the pollers should have made phase A creep with every sleep shortened");

    // Direct3D 9.
    let d9 = run(&x.display, &Case { tag: "d3d9", api: "d3d9", frames: 600, ..Default::default() });
    check(&d9, 600, "d3d9");

    // 32-bit.
    if std::env::var("QUIVER_TEST_VCLOCK_32").as_deref() != Ok("0") && quiver_vclock::unavailable("x86").is_none() {
        let x86 = run(&x.display, &Case { tag: "x86", arch: "x86", frames: 600, helper: true, ..Default::default() });
        check(&x86, 600, "d3d11");
    }
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, mingw-w64 and Xvfb; see the module docs"]
fn another_modules_hooks_stay_in_the_chain() {
    let x = xvfb::Xvfb::start_size("640x480x24").expect("Xvfb");
    for arch in ["x64", "x86"] {
        if arch == "x86" && (std::env::var("QUIVER_TEST_VCLOCK_32").as_deref() == Ok("0") || quiver_vclock::unavailable("x86").is_some()) {
            continue;
        }
        let o = run(&x.display, &Case { tag: "chain", arch, api: "d3d11chain", frames: 600, ..Default::default() });
        check(&o, 600, "d3d11chain");
        assert_eq!(o.get("chain clock_first"), "1", "{arch}: the clock hooked the factory's CreateSwapChain first (the case Pharos hit)");
        assert_eq!(
            o.get("chain same_vtable"),
            "1 slot_theirs 1",
            "{arch}: the device's factory shares the vtable, and the clock left the other module's hook in the slot"
        );
        // 600 frames in phase A, 31 in B, 30 in C, 10 in D.
        assert_eq!(o.get("chain their_create_calls"), "1 their_presents 671", "{arch}: the other module's hooks ran once per call");
        // The first engages the clock; the report counts frames after it.
        assert_eq!(o.report.frames, 670, "{arch}: the clock counted each of those presents once");
        assert!(o.log.contains("another module's hook over ours"), "{arch}: the log says so:\n{}", o.log);
    }
}
