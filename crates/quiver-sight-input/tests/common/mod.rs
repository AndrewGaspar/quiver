//! Shared by the opt-in Wine tests: a private Xvfb (the layer's helper: displays from :400
//! up, never the desktop's), XTEST input into it, the Windows input probe under Proton with
//! this build's preload, and a stand-in for quiver-sight and the layer on `panel.input`.

#![allow(dead_code)]

#[path = "../../../quiver-sight-layer/tests/xvfb/mod.rs"]
pub mod xvfb;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quiver_perf::clock::mono_raw_ns;
use quiver_perf::panel::{EventReader, PANEL_HOLD_NS, PANEL_INPUT_FILE, PanelEvent, PanelInput, create_panel_input};
use quiver_perf::session::{SessionDir, SessionId};
use quiver_perf::shm::Mapping;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;

pub fn proton() -> PathBuf {
    PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"))
}

/// The freshly built preload, next to the test binary's `deps/`.
pub fn preload_so() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIVER_SIGHT_INPUT_SO") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().unwrap();
    let mut dir = exe.parent().unwrap().to_path_buf();
    for _ in 0..3 {
        let p = dir.join("libquiver_sight_input.so");
        if p.exists() {
            return p;
        }
        dir = dir.parent().unwrap().to_path_buf();
    }
    panic!("libquiver_sight_input.so not found near {}", exe.display());
}

pub fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

pub fn build_probe(out: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wine/input_probe.c");
    let exe = out.join("input_probe.exe");
    let st = Command::new("x86_64-w64-mingw32-gcc").args(["-O2", "-o"]).arg(&exe).arg(&src).arg("-luser32").status().expect("x86_64-w64-mingw32-gcc");
    assert!(st.success());
    exe
}

/// XTEST into the private server.
pub struct Fake {
    conn: x11rb::xcb_ffi::XCBConnection,
    root: u32,
}

impl Fake {
    pub fn new(display: &str) -> Fake {
        let name = std::ffi::CString::new(display).unwrap();
        let (conn, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&name)).unwrap();
        let root = conn.setup().roots[screen].root;
        Fake { conn, root }
    }

    fn input(&self, ty: u8, detail: u8, x: i16, y: i16) {
        self.conn.xtest_fake_input(ty, detail, 0, self.root, x, y, 0).unwrap();
        self.conn.sync().unwrap();
    }

    pub fn key(&self, code: u8, down: bool) {
        self.input(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, code, 0, 0);
    }

    pub fn tap(&self, code: u8) {
        self.key(code, true);
        std::thread::sleep(Duration::from_millis(30));
        self.key(code, false);
    }

    pub fn button(&self, b: u8, down: bool) {
        self.input(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, b, 0, 0);
    }

    pub fn click(&self, b: u8) {
        self.button(b, true);
        std::thread::sleep(Duration::from_millis(30));
        self.button(b, false);
    }

    /// Absolute.
    pub fn move_to(&self, x: i16, y: i16) {
        self.input(MOTION_NOTIFY_EVENT, 0, x, y);
    }

    /// Relative (detail 1).
    pub fn move_by(&self, dx: i16, dy: i16) {
        self.input(MOTION_NOTIFY_EVENT, 1, dx, dy);
    }
}

/// X keycodes of Xvfb's default (evdev) keymap: evdev + 8.
pub mod key {
    pub const ESC: u8 = 9;
    pub const W: u8 = 25;
    pub const E: u8 = 26;
    pub const A: u8 = 38;
    pub const D: u8 = 40;
    pub const F: u8 = 41;
    pub const C: u8 = 54;
    pub const B: u8 = 56;
    pub const G: u8 = 42;
    pub const SHIFT_L: u8 = 50;
    pub const F2: u8 = 68;
}

/// Windows virtual keys the probe prints.
pub mod vk {
    pub const SHIFT: u32 = 0x10;
    pub const ESC: u32 = 0x1b;
    pub const A: u32 = 0x41;
    pub const B: u32 = 0x42;
    pub const C: u32 = 0x43;
    pub const D: u32 = 0x44;
    pub const E: u32 = 0x45;
    pub const F: u32 = 0x46;
    pub const G: u32 = 0x47;
    pub const W: u32 = 0x57;
    pub const F2: u32 = 0x71;
}

/// The probe's stdout, line by line.
#[derive(Clone, Default)]
pub struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn since(&self, from: usize) -> Vec<String> {
        self.0.lock().unwrap()[from..].to_vec()
    }

    /// Input lines since `from` (not HELD/ACTIVE bookkeeping).
    pub fn input_since(&self, from: usize) -> Vec<String> {
        self.since(from).into_iter().filter(|l| !l.starts_with("HELD") && !l.starts_with("ACTIVE")).collect()
    }

    pub fn wait_for(&self, pred: impl Fn(&str) -> bool, within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if self.0.lock().unwrap().iter().any(|l| pred(l)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The keys the last HELD line after `from` lists.
    pub fn held_after(&self, from: usize, within: Duration) -> Option<Vec<u32>> {
        let end = Instant::now() + within;
        while Instant::now() < end {
            let all = self.since(from);
            if let Some(l) = all.iter().rev().find(|l| l.starts_with("HELD")) {
                return Some(l.split_whitespace().skip(1).filter_map(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()).collect());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

pub fn keydown(vk: u32) -> String {
    format!("KEYDOWN 0x{vk:02x}")
}

pub fn keyup(vk: u32) -> String {
    format!("KEYUP 0x{vk:02x}")
}

/// A scratch session dir with `panel.input`, standing in for quiver-sight (open, renew,
/// close, the pid the preload watches) and for the layer (it marks the panel drawn while
/// `drawing`).
pub struct FakeSight {
    _root: tempfile::TempDir,
    pub dir: SessionDir,
    pub panel: Arc<PanelInput<Mapping>>,
    pub open: Arc<AtomicBool>,
    pub drawing: Arc<AtomicBool>,
    pub renewing: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    beat: Option<std::thread::JoinHandle<()>>,
}

impl Drop for FakeSight {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(b) = self.beat.take() {
            let _ = b.join();
        }
    }
}

impl FakeSight {
    pub fn new() -> FakeSight {
        let base = std::env::var_os("QUIVER_SHM_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/dev/shm"));
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new().prefix("qsi-").tempdir_in(&base).unwrap();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-inputtest").unwrap()).unwrap();
        let panel = Arc::new(create_panel_input(&dir.path().join(PANEL_INPUT_FILE)).unwrap());
        let (open, drawing, renewing, epoch, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let beat = {
            let (p, open, drawing, renewing, epoch, stop) = (panel.clone(), open.clone(), drawing.clone(), renewing.clone(), epoch.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let now = mono_raw_ns();
                    if open.load(Ordering::Relaxed) {
                        if renewing.load(Ordering::Relaxed) {
                            p.renew(now + PANEL_HOLD_NS as i64);
                        }
                        if drawing.load(Ordering::Relaxed) {
                            p.mark_drawn(epoch.load(Ordering::Relaxed), now);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        FakeSight { _root: root, dir, panel, open, drawing, renewing, epoch, stop, beat: Some(beat) }
    }

    /// Opens the panel as a new epoch, owned by process `pid`.
    pub fn open(&self, pid: u32) -> EventReader {
        let r = EventReader::at_end(&self.panel);
        let e = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.panel.set_sight_pid(pid);
        let now = mono_raw_ns();
        self.renewing.store(true, Ordering::Relaxed);
        self.drawing.store(true, Ordering::Relaxed);
        self.panel.open(e, now, now + PANEL_HOLD_NS as i64, true);
        self.panel.mark_drawn(e, now);
        self.open.store(true, Ordering::Relaxed);
        r
    }

    pub fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
        self.panel.close();
    }

    pub fn events(&self, r: &mut EventReader) -> Vec<PanelEvent> {
        let mut v = Vec::new();
        r.poll(&self.panel, |e| v.push(e));
        v
    }

    /// Waits until the preload says it captures (or not).
    pub fn wait_capturing(&self, on: bool, within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            let i = self.panel.input();
            if (i.capturing != 0) == on && (i.flags & quiver_perf::panel::INPUT_CAPTURING != 0) == on {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

/// The probe running under Proton with the preload, its stdout collected.
pub struct Probe {
    pub child: Option<Child>,
    pub lines: Lines,
    pub stderr: Arc<Mutex<Vec<String>>>,
    prefix: PathBuf,
}

impl Probe {
    pub fn start(display: &str, sight: &FakeSight, seconds: u32, flags: u32, scratch: &Path, extra_env: &[(&str, &str)]) -> Probe {
        let proton = proton();
        let exe = build_probe(scratch);
        let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/quiver-sight-wine-input");
        let compat = keep.join("compat");
        std::fs::create_dir_all(&compat).unwrap();
        for d in ["home", "steam"] {
            std::fs::create_dir_all(scratch.join(d)).unwrap();
        }
        let title = format!("quiver-input-probe-{}", std::process::id());
        let mut cmd = Command::new("python3");
        cmd.arg(proton.join("proton"))
            .args(["runinprefix", &win(&exe), &seconds.to_string(), &title, &flags.to_string()])
            .env("STEAM_COMPAT_DATA_PATH", &compat)
            .env("WINEPREFIX", compat.join("pfx"))
            .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", scratch.join("steam"))
            .env("STEAM_COMPAT_APP_ID", "0")
            .env("SteamAppId", "0")
            .env("SteamGameId", "0")
            .env("HOME", scratch.join("home"))
            .env("XDG_DATA_HOME", scratch.join("home/.local/share"))
            .env("XDG_CONFIG_HOME", scratch.join("home/.config"))
            .env("XDG_CACHE_HOME", scratch.join("home/.cache"))
            .env("DISPLAY", display)
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
            .env("LD_PRELOAD", preload_so())
            .env("QUIVER_PERF_DIR", sight.dir.path())
            .env("QUIVER_SIGHT_LOG", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("PROTON_ENABLE_WAYLAND")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let lines = Lines::default();
        let out = child.stdout.take().unwrap();
        let l2 = lines.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                l2.0.lock().unwrap().push(l);
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let err = child.stderr.take().unwrap();
        let e2 = stderr.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                e2.lock().unwrap().push(l);
            }
        });
        Probe { child: Some(child), lines, stderr, prefix: compat.join("pfx") }
    }

    pub fn preload_log(&self) -> Vec<String> {
        self.stderr.lock().unwrap().iter().filter(|l| l.starts_with("quiver-sight-input")).cloned().collect()
    }

    pub fn stop(&mut self) {
        let _ = Command::new(proton().join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", &self.prefix).status();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.stop();
    }
}
