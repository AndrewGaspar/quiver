//! The preload's side of `panel.input` (quiver-perf `panel`): mapping it, the capture
//! decision every interposed call asks for, forwarding events, and the thread that does the
//! syscalls.
//!
//! **No syscalls on Wine's threads.** The interposed calls only load atomics, read the clock
//! (vDSO) and push ring slots. A thread of the preload's own (started by the first interposed
//! call, so processes that never touch X11 or Wayland never get one) maps the file when it
//! appears, heartbeats the preload's fields, and checks that the quiver-sight that opened the
//! panel is still alive.

use std::path::PathBuf;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use quiver_perf::clock::mono_raw_ns;
use quiver_perf::panel::{INPUT_CAPTURING, PANEL_INPUT_FILE, PanelEvent, PanelInput, open_panel_input};
use quiver_perf::shm::Mapping;

use crate::log;

/// The mapped `panel.input`, leaked once mapped (a game process maps it at most a few times).
static PANEL: AtomicPtr<PanelInput<Mapping>> = AtomicPtr::new(std::ptr::null_mut());
/// `INPUT_*` hooks seen so far.
static FLAGS: AtomicU32 = AtomicU32::new(0);
/// The epoch whose quiver-sight was found dead: never captured again.
static DEAD_EPOCH: AtomicU64 = AtomicU64::new(0);
/// The epoch being captured now (0: none), as last reported.
static CAPTURING: AtomicU64 = AtomicU64::new(0);
static OFF: AtomicBool = AtomicBool::new(false);
static START: Once = Once::new();

/// `QUIVER_SIGHT_INPUT=0`: the preload passes everything through and starts nothing.
pub const ENV_INPUT: &str = "QUIVER_SIGHT_INPUT";

fn disabled() -> bool {
    std::env::var_os(ENV_INPUT).is_some_and(|v| v == "0")
}

/// An interposed call saw a driver (`INPUT_X11`, `INPUT_WAYLAND`): note it and start the
/// thread (once).
pub fn hooked(flag: u32) {
    if FLAGS.fetch_or(flag, Ordering::Relaxed) & flag == 0 {
        log!("{} input interposed", if flag == quiver_perf::panel::INPUT_X11 { "X11" } else { "Wayland" });
    }
    START.call_once(|| {
        if disabled() {
            OFF.store(true, Ordering::Relaxed);
            log!("{ENV_INPUT}=0: passing everything through");
            return;
        }
        let Some(dir) = std::env::var_os(quiver_perf::session::ENV_PERF_DIR).filter(|d| !d.is_empty()) else {
            OFF.store(true, Ordering::Relaxed);
            return;
        };
        let path = PathBuf::from(dir).join(PANEL_INPUT_FILE);
        let spawned = std::thread::Builder::new().name("quiver-input".into()).stack_size(128 << 10).spawn(move || run(path));
        if let Err(e) = spawned {
            log!("no thread: {e}");
            OFF.store(true, Ordering::Relaxed);
        }
    });
}

fn panel() -> Option<&'static PanelInput<Mapping>> {
    let p = PANEL.load(Ordering::Acquire);
    // SAFETY: set once from a leaked Box, never freed.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// The epoch the panel captures input for right now, if it does. One or two atomic loads
/// when the panel is closed; also reports changes into the file.
#[inline]
pub fn capture_now() -> Option<u64> {
    let p = panel()?;
    let c = p.control();
    let epoch = if c.open() && c.epoch() != DEAD_EPOCH.load(Ordering::Relaxed) { quiver_perf::panel::capture(&c, &p.layer(), mono_raw_ns()) } else { None };
    let now = epoch.unwrap_or(0);
    if CAPTURING.swap(now, Ordering::Relaxed) != now {
        p.set_capturing(now);
        let f = FLAGS.load(Ordering::Relaxed);
        p.set_input(std::process::id(), if now != 0 { f | INPUT_CAPTURING } else { f }, mono_raw_ns());
        log!("{}", if now != 0 { format!("capturing input for panel {now}") } else { "input back to the game".to_owned() });
    }
    epoch
}

/// Forwards an event to the panel (lock-free).
#[inline]
pub fn forward(e: &PanelEvent) {
    if let Some(p) = panel() {
        p.push(e);
    }
}

/// A release passed to the game while capturing (for the stats).
pub fn note_release() {
    if let Some(p) = panel() {
        p.note_releases(1);
    }
}

pub fn now() -> i64 {
    mono_raw_ns()
}

/// The thread: map, heartbeat, watch quiver-sight's pid.
fn run(path: PathBuf) {
    let pid = std::process::id();
    let mut alive: Option<u32> = None;
    let mut last_seen = 0i64;
    loop {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if panel().is_none() {
                match open_panel_input(&path, true) {
                    Ok(p) => {
                        log!("{} mapped", path.display());
                        PANEL.store(Box::into_raw(Box::new(p)), Ordering::Release);
                    }
                    Err(_) => return Duration::from_millis(500),
                }
            }
            let Some(p) = panel() else { return Duration::from_millis(500) };
            let now = mono_raw_ns();
            if now - last_seen > 500_000_000 {
                let f = FLAGS.load(Ordering::Relaxed) | if CAPTURING.load(Ordering::Relaxed) != 0 { INPUT_CAPTURING } else { 0 };
                p.set_input(pid, f, now);
                last_seen = now;
            }
            let c = p.control();
            if !c.open() {
                alive = None;
                return Duration::from_millis(250);
            }
            // quiver-sight's pid: alive once seen alive, dead when it's gone after that (a
            // pid namespace that hides it is never "seen alive", so it never counts as dead).
            if c.sight_pid != 0 {
                // SAFETY: signal 0 only checks for existence.
                let r = unsafe { libc::kill(c.sight_pid as libc::pid_t, 0) };
                let exists = r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                if exists {
                    alive = Some(c.sight_pid);
                } else if alive == Some(c.sight_pid) && DEAD_EPOCH.swap(c.epoch(), Ordering::Relaxed) != c.epoch() {
                    log!("quiver-sight (pid {}) is gone: input back to the game", c.sight_pid);
                }
            }
            // While open: often, so a dead quiver-sight releases input fast; the capture
            // decision itself is made by every interposed call.
            let _ = capture_now();
            Duration::from_millis(50)
        }));
        std::thread::sleep(r.unwrap_or(Duration::from_secs(1)));
    }
}

pub fn off() -> bool {
    OFF.load(Ordering::Relaxed)
}
