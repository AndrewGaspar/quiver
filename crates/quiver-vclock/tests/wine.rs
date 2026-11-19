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
//! - a QPC busy-wait for two frames and a `Sleep(1)` loop on timeGetTime both terminate (the
//!   creep, counted);
//! - QueryPerformanceCounter looked up with GetProcAddress gives the virtual value;
//! - only the executable's clock imports point into the hook DLL: none of Wine's modules or
//!   DXVK's;
//! - no real-time escape fired (the run is deterministic).
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
        .args(["-ld3d11", "-ld3d9", "-lversion", "-lwinmm", "-lpsapi", "-luuid", "-ldxguid"])
        .status()
        .expect(gcc);
    assert!(st.success(), "{gcc} failed");
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

fn run(display: &str, arch: &str, api: &str, frames: u32, slow_ms: u32, tag: &str) -> Out {
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
    let out = Command::new("python3")
        .arg(proton().join("proton"))
        .args(["runinprefix", &win(&exe), api, &frames.to_string(), &slow_ms.to_string(), &title])
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
        .env_remove("PROTON_ENABLE_WAYLAND")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let log = std::fs::read_to_string(t.join("out").join(quiver_vclock::LOG_FILE)).unwrap_or_default();
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
            "A" | "B" | "C" => lines.insert(format!("{a} {b}"), rest.to_owned()),
            "iat" => lines.insert(format!("iat {b}"), rest.to_owned()),
            _ => lines.insert(a.to_owned(), format!("{b} {rest}").trim().to_owned()),
        };
    }
    Out { lines, report, stdout }
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
    assert!(o.lines.contains_key("B frames") && o.lines.contains_key("C frames"), "the waits terminated");
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
    // The B and C waits crept (one per frame, give or take the frame each phase starts with).
    assert!(r.creep_frames >= 55, "{}", r.counters());
    // The DLL's own census: frame steps, plus the creep's small steps from B and C.
    assert!(r.census_qpc.iter().any(|b| b.0 == TPF && b.1 >= n), "{:?}", r.census_qpc);
    assert!(r.census_ms.iter().any(|b| b.0 == 16 && b.1 >= n), "{:?}", r.census_ms);
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, mingw-w64 and Xvfb; see the module docs"]
fn the_game_sees_exactly_one_frame_per_present() {
    let x = xvfb::Xvfb::start_size("640x480x24").expect("Xvfb");
    let mut hashes = Vec::new();

    // A presenter faster than 62.5 Hz: faster than real time.
    let fast = run(&x.display, "x64", "d3d11", 1200, 0, "fast");
    check(&fast, 1200, "d3d11");
    let (v, r) = (fast.num("A frames", "virtual_s"), fast.num("A frames", "real_s"));
    eprintln!("fast: {v:.3} s virtual in {r:.3} s real = {:.2}x", v / r);
    assert!(v > r, "the fast presenter wasn't faster than real time ({v} virtual, {r} real)");
    hashes.push(fast.get("A sequence_hash").to_owned());

    // The same again: the same virtual-time sequence.
    let again = run(&x.display, "x64", "d3d11", 1200, 0, "again");
    check(&again, 1200, "d3d11");
    hashes.push(again.get("A sequence_hash").to_owned());

    // A presenter held to 40 fps (below 62.5 Hz): slower than real time, the same game time.
    let slow = run(&x.display, "x64", "d3d11", 1200, 25, "slow");
    check(&slow, 1200, "d3d11");
    let (v, r) = (slow.num("A frames", "virtual_s"), slow.num("A frames", "real_s"));
    eprintln!("slow: {v:.3} s virtual in {r:.3} s real = {:.2}x", v / r);
    assert!(v < r * 0.8, "the slow presenter's game time should run slower than real time ({v} virtual, {r} real)");
    assert!(slow.report.speedup < 1.0 && !slow.report.verdict().iter().any(|v| v.contains("slower")), "slower than real time is normal: {:?}", slow.report.verdict());
    hashes.push(slow.get("A sequence_hash").to_owned());
    assert!(hashes.iter().all(|h| *h == hashes[0]), "the virtual-time sequences differ between runs: {hashes:?}");
    // The DLL's own view agrees across the three runs too, the busy-waits included: the same
    // steps, the same creep, the same shortened sleeps.
    for o in [&again, &slow] {
        let (a, b) = (&fast.report, &o.report);
        assert_eq!((&a.census_qpc, &a.census_ms, a.creep_frames, a.overruns, a.sleeps), (&b.census_qpc, &b.census_ms, b.creep_frames, b.overruns, b.sleeps));
        assert_eq!(fast.get("C frames").split_whitespace().nth(2), o.get("C frames").split_whitespace().nth(2), "phase C's Sleep(1) count");
    }

    // Direct3D 9.
    let d9 = run(&x.display, "x64", "d3d9", 600, 0, "d3d9");
    check(&d9, 600, "d3d9");

    // 32-bit.
    if std::env::var("QUIVER_TEST_VCLOCK_32").as_deref() != Ok("0") && quiver_vclock::unavailable("x86").is_none() {
        let x86 = run(&x.display, "x86", "d3d11", 600, 0, "x86");
        check(&x86, 600, "d3d11");
    }
}
