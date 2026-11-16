//! Relaunch on update: when the binary the app was started from is replaced (a
//! `cargo build --release` in the checkout, a package upgrade), start the new build with the
//! current screen and hand over to it, without ever losing the app to a broken build.
//!
//! - **Detect.** A thread watches the executable's *path* (inotify on its directory, filtered
//!   to its file name, plus a slow poll). An update is a file at that path whose identity
//!   (device, inode, size, mtime) differs from the running image's (`/proc/self/exe`).
//!   [`Debounce`] waits until it has been unchanged for [`STABLE_FOR`] and [`check_candidate`]
//!   says it's a complete ELF (or a `#!` script) with an executable bit.
//! - **Defer.** [`Scheduler`] holds the update while anything would be interrupted (an
//!   overlay, a focused text field, a sign-in in progress; see [`Quiver::relaunch_blocker`]),
//!   then shows a toast with a [`COUNTDOWN`] and "Relaunch now" / "Later".
//! - **Hand over.** The old app writes a [`Resume`] state file and spawns the new binary with
//!   the same args and env plus `QUIVER_RESUME=<file>`. The new app restores the state and,
//!   once it has drawn its first frame, deletes the file. The old app sees the file gone and
//!   quits. If the new process exits or isn't ready within [`READY_TIMEOUT`], the old app
//!   kills it, keeps running and says so (with the new process's log).
//!
//! The daemon is never involved: the app is only a client of `quiverd`, which keeps running
//! (and keeps running games) across a relaunch.
//!
//! `QUIVER_AUTO_RELAUNCH=0` (or `false`/`no`/`off`) turns this off.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, IntoElement, ParentElement, Styled, Window, WindowBounds, div, point,
    prelude::*, px, size,
};
use serde::{Deserialize, Serialize};

use crate::app::{Conn, InputMode, Quiver, SideFilter};
use crate::nav::{Nav, Page, Tab};
use crate::theme::{Theme, rgba};
use crate::ui::{Icon, Kind, MEDIUM, button, icon, soft_shadow};

pub const ENV_RESUME: &str = "QUIVER_RESUME";
pub const ENV_AUTO: &str = "QUIVER_AUTO_RELAUNCH";

/// How long a new binary must stay unchanged before it counts as installed.
pub const STABLE_FOR: Duration = Duration::from_secs(2);
/// How long the new build gets to put its window up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// The "relaunching in 3…" toast.
pub const COUNTDOWN: Duration = Duration::from_secs(3);
/// "Later" holds this build back this long (a newer build asks again right away).
pub const SNOOZE: Duration = Duration::from_secs(10 * 60);
/// Poll interval when inotify is quiet (or unavailable).
const POLL: Duration = Duration::from_secs(10);
/// Poll interval while a new file is settling.
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// Env that only makes sense for the process it was given to.
const ENV_DROP: &[&str] = &["QUIVER_DEMO_INPUT", "DESKTOP_STARTUP_ID", "XDG_ACTIVATION_TOKEN"];

pub fn enabled() -> bool {
    !std::env::var(ENV_AUTO)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
}

// ---------------------------------------------------------------------------------------------
// Detection

/// The identity of a file's contents, as far as `stat` can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

impl Fingerprint {
    pub fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        m.is_file().then(|| Fingerprint {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
        })
    }

    /// The running image, even after its path has been replaced.
    pub fn running() -> Option<Self> {
        Self::of(Path::new("/proc/self/exe"))
    }
}

/// The path the app was started from (what to watch and what to relaunch).
pub fn exe_path() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?;
    // Already replaced before we looked: /proc/self/exe reads "<path> (deleted)".
    let s = p.to_str()?;
    Some(PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(s)))
}

/// Whether `path` looks like a finished build: executable, and either a `#!` script or an ELF
/// whose program and section header tables fit in the file (linkers write those last, so a
/// half-written binary fails this).
pub fn check_candidate(path: &Path) -> Result<(), String> {
    let m = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if m.permissions().mode() & 0o111 == 0 {
        return Err("not executable".into());
    }
    let mut head = [0u8; 64];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).map_err(|e| e.to_string())?;
    let head = &head[..n];
    if head.starts_with(b"#!") {
        return Ok(());
    }
    if !head.starts_with(b"\x7fELF") {
        return Err("not an ELF executable".into());
    }
    if head.len() < 52 {
        return Err("truncated ELF header".into());
    }
    let le = head[5] == 1;
    let u16_at = |o: usize| {
        let b = [head[o], head[o + 1]];
        (if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }) as u64
    };
    let u32_at = |o: usize| {
        let b = [head[o], head[o + 1], head[o + 2], head[o + 3]];
        (if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }) as u64
    };
    let (phoff, shoff, phentsize, phnum, shentsize, shnum) = match head[4] {
        1 => (u32_at(0x1c), u32_at(0x20), u16_at(0x2a), u16_at(0x2c), u16_at(0x2e), u16_at(0x30)),
        2 if head.len() >= 64 => {
            let u64_at = |o: usize| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&head[o..o + 8]);
                if le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) }
            };
            (u64_at(0x20), u64_at(0x28), u16_at(0x36), u16_at(0x38), u16_at(0x3a), u16_at(0x3c))
        }
        _ => return Err("unknown ELF class".into()),
    };
    let size = m.len();
    let fits = |off: u64, ent: u64, num: u64| off.checked_add(ent * num).is_some_and(|end| end <= size);
    if phnum == 0 || !fits(phoff, phentsize, phnum) {
        return Err("incomplete ELF (program headers)".into());
    }
    if shnum > 0 && !fits(shoff, shentsize, shnum) {
        return Err("incomplete ELF (section headers)".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing new (or nothing at the path right now).
    Idle,
    /// A new file is there but hasn't been stable for long enough.
    Settling,
    /// Stable, but not something we can run (yet). Reported once per file.
    Invalid(String),
    /// A new build, ready to run. Reported once per file.
    Ready(Fingerprint),
}

/// Turns observations of the executable's path into "a new build is ready", once per build.
#[derive(Debug)]
pub struct Debounce {
    running: Option<Fingerprint>,
    seen: Option<(Fingerprint, Instant)>,
    reported: Option<Fingerprint>,
}

impl Debounce {
    pub fn new(running: Option<Fingerprint>) -> Self {
        Self { running, seen: None, reported: None }
    }

    /// `now` is what's at the path; `valid` is asked only once it has been stable long enough.
    pub fn observe(&mut self, now: Option<Fingerprint>, at: Instant, valid: impl FnOnce() -> Result<(), String>) -> Step {
        let Some(fp) = now else {
            // Mid-rename, or removed (cargo clean).
            self.seen = None;
            return Step::Idle;
        };
        if Some(fp) == self.running || Some(fp) == self.reported {
            self.seen = None;
            return Step::Idle;
        }
        match self.seen {
            Some((s, since)) if s == fp => {
                if at.duration_since(since) < STABLE_FOR {
                    return Step::Settling;
                }
                self.reported = Some(fp);
                self.seen = None;
                match valid() {
                    Ok(()) => Step::Ready(fp),
                    Err(e) => Step::Invalid(e),
                }
            }
            _ => {
                self.seen = Some((fp, at));
                Step::Settling
            }
        }
    }
}

/// Watch `exe` on a background thread; `ready` gets each new build (once).
fn spawn_watcher(exe: PathBuf, ready: impl Fn(Fingerprint) -> bool + Send + 'static) {
    use notify::{RecursiveMode, Watcher};
    let Some(dir) = exe.parent().map(Path::to_path_buf) else { return };
    let Some(name) = exe.file_name().map(|n| n.to_owned()) else { return };
    let spawned = std::thread::Builder::new().name("selfupdate-watch".into()).spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut watcher = notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
            if e.is_ok_and(|e| e.paths.iter().any(|p| p.file_name() == Some(name.as_os_str()))) {
                let _ = tx.send(());
            }
        })
        .ok();
        let mut armed_on: Option<u64> = None;
        let mut deb = Debounce::new(Fingerprint::running());
        let mut settling = false;
        loop {
            // Re-arm when the directory is new (cargo clean, first build after a clean).
            let dir_ino = std::fs::metadata(&dir).ok().map(|m| m.ino());
            if let (Some(_), Some(w)) = (dir_ino.filter(|_| dir_ino != armed_on), watcher.as_mut()) {
                let _ = w.unwatch(&dir);
                if w.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                    armed_on = dir_ino;
                }
            }
            let _ = rx.recv_timeout(if settling { SETTLE_POLL } else { POLL });
            while rx.try_recv().is_ok() {}
            settling = false;
            match deb.observe(Fingerprint::of(&exe), Instant::now(), || check_candidate(&exe)) {
                Step::Idle => {}
                Step::Settling => settling = true,
                Step::Invalid(why) => tracing::warn!("{} changed but can't be run: {why}", exe.display()),
                Step::Ready(fp) => {
                    tracing::info!("new build at {}", exe.display());
                    if !ready(fp) {
                        return;
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("auto-relaunch: can't start the watcher: {e}");
    }
}

// ---------------------------------------------------------------------------------------------
// When to relaunch

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// A build is ready; waiting for the app to be idle (or for a snooze to end).
    Waiting,
    Countdown { until: Instant },
    /// The new process is starting.
    Starting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    Nothing,
    Redraw,
    Launch,
}

/// The UI-side state machine: pure, so the deferral rules are testable.
#[derive(Debug)]
pub struct Scheduler {
    pub phase: Phase,
    pending: Option<Fingerprint>,
    attempting: Option<Fingerprint>,
    snoozed: Option<(Fingerprint, Instant)>,
    /// Why the last start failed, until dismissed or the next attempt.
    pub failed: Option<String>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self { phase: Phase::Idle, pending: None, attempting: None, snoozed: None, failed: None }
    }
}

impl Scheduler {
    pub fn found(&mut self, fp: Fingerprint) {
        self.pending = Some(fp);
        if self.phase == Phase::Idle {
            self.phase = Phase::Waiting;
        }
    }

    fn snoozed(&self, now: Instant) -> bool {
        matches!((self.snoozed, self.pending), (Some((s, until)), Some(p)) if s == p && now < until)
    }

    pub fn tick(&mut self, blocked: bool, now: Instant) -> Tick {
        match self.phase {
            Phase::Idle | Phase::Starting => Tick::Nothing,
            Phase::Waiting if blocked || self.snoozed(now) => Tick::Nothing,
            Phase::Waiting => {
                self.phase = Phase::Countdown { until: now + COUNTDOWN };
                self.failed = None;
                Tick::Redraw
            }
            Phase::Countdown { .. } if blocked => {
                self.phase = Phase::Waiting;
                Tick::Redraw
            }
            Phase::Countdown { until } if now >= until => self.start(),
            Phase::Countdown { .. } => Tick::Nothing,
        }
    }

    /// Skip the countdown.
    pub fn now(&mut self) -> Tick {
        match self.phase {
            Phase::Waiting | Phase::Countdown { .. } => self.start(),
            _ => Tick::Nothing,
        }
    }

    fn start(&mut self) -> Tick {
        self.phase = Phase::Starting;
        self.attempting = self.pending;
        self.failed = None;
        Tick::Launch
    }

    /// Hold this build back; a newer one asks again.
    pub fn later(&mut self, now: Instant) {
        if let Some(p) = self.pending {
            self.snoozed = Some((p, now + SNOOZE));
            self.phase = Phase::Waiting;
        }
    }

    /// The new process didn't come up. A build that arrived meanwhile is still tried.
    pub fn launch_failed(&mut self, why: String) {
        self.failed = Some(why);
        if self.pending.is_some() && self.pending != self.attempting {
            self.phase = Phase::Waiting;
        } else {
            self.pending = None;
            self.phase = Phase::Idle;
        }
        self.attempting = None;
    }

    pub fn seconds_left(&self, now: Instant) -> Option<u64> {
        match self.phase {
            Phase::Countdown { until } => Some(until.saturating_duration_since(now).as_secs_f32().ceil() as u64),
            _ => None,
        }
    }

    /// Whether there's something to show (and so a reason to tick fast).
    pub fn visible(&self, blocked: bool, now: Instant) -> bool {
        self.failed.is_some()
            || matches!(self.phase, Phase::Countdown { .. } | Phase::Starting)
            || (self.phase == Phase::Waiting && blocked && !self.snoozed(now))
    }
}

// ---------------------------------------------------------------------------------------------
// The handover state

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Resume {
    pub from_pid: u32,
    /// [`Nav`] as JSON, decoded leniently (the new build may have changed it).
    pub nav: serde_json::Value,
    pub mode: Option<String>,
    pub side_filter: Option<String>,
    pub scroll: ScrollState,
    pub window: Option<WindowState>,
    pub theme: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrollState {
    pub library: f32,
    pub side: f32,
    pub body: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// "windowed", "maximized" or "fullscreen".
    pub kind: WindowKind,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowKind {
    Windowed,
    Maximized,
    Fullscreen,
}

impl WindowState {
    pub fn of(b: WindowBounds) -> Self {
        let (kind, r) = match b {
            WindowBounds::Windowed(r) => (WindowKind::Windowed, r),
            WindowBounds::Maximized(r) => (WindowKind::Maximized, r),
            WindowBounds::Fullscreen(r) => (WindowKind::Fullscreen, r),
        };
        WindowState {
            kind,
            x: r.origin.x.into(),
            y: r.origin.y.into(),
            w: r.size.width.into(),
            h: r.size.height.into(),
        }
    }

    pub fn bounds(&self) -> Option<WindowBounds> {
        if !(self.w >= 100. && self.h >= 100. && self.x.is_finite() && self.y.is_finite()) {
            return None;
        }
        let r = Bounds::new(point(px(self.x), px(self.y)), size(px(self.w), px(self.h)));
        Some(match self.kind {
            WindowKind::Windowed => WindowBounds::Windowed(r),
            WindowKind::Maximized => WindowBounds::Maximized(r),
            WindowKind::Fullscreen => WindowBounds::Fullscreen(r),
        })
    }
}

impl Resume {
    /// The navigation state, as much of it as this build understands. Overlays never carry
    /// over (the palette is closed).
    pub fn nav(&self) -> Nav {
        let mut nav = serde_json::from_value::<Nav>(self.nav.clone()).unwrap_or_else(|e| {
            tracing::info!("resume: nav state not fully understood ({e}); keeping page and tab");
            let mut nav = Nav::new();
            if let Some(p) = self.nav.get("page").and_then(|v| serde_json::from_value::<Page>(v.clone()).ok()) {
                match p {
                    Page::Game(id) => nav.open_game(id),
                    p => nav.open_section(p.section()),
                }
            }
            if let Some(t) = self.nav.get("tab").and_then(|v| serde_json::from_value::<Tab>(v.clone()).ok()) {
                nav.select_tab(t);
            }
            nav
        });
        nav.close_overlay();
        nav
    }
}

/// The `QUIVER_RESUME` handover, if this process was started by a relaunch. A file that
/// can't be read still counts (the old app waits for it to disappear), with default state.
pub fn take_resume() -> Option<(PathBuf, Resume)> {
    let path = PathBuf::from(std::env::var_os(ENV_RESUME)?);
    let state = std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice::<Resume>(&b).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| {
            tracing::warn!("resume: can't use {}: {e}", path.display());
            Resume::default()
        });
    tracing::info!("resumed from pid {} ({})", state.from_pid, path.display());
    Some((path, state))
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("quiver")).unwrap_or_else(std::env::temp_dir)
}

const LOG_MAX: u64 = 4 << 20;

/// Where a relaunched app's stdout and stderr go (appended; rotated to `.1` past 4 MiB).
pub fn log_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    data.join("quiver/quiver-app.log")
}

/// `path` with the home directory shortened to `~`.
fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from).and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Write the state and start `exe` with it. The caller waits for the state file to vanish.
fn spawn_successor(exe: &Path, state: &Resume) -> Result<(Child, PathBuf), String> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("app-resume-{}.json", std::process::id()));
    let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;

    let log = log_path();
    let out = (|| {
        use std::io::Write;
        if let Some(d) = log.parent() {
            std::fs::create_dir_all(d)?;
        }
        // One log, appended to by each relaunched app in turn (the running one may still be
        // writing to it), rotated once it gets big.
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_MAX) {
            std::fs::rename(&log, log.with_extension("log.1"))?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        writeln!(f, "--- {} relaunched by pid {}", chrono::Local::now().to_rfc3339(), std::process::id())?;
        Ok::<_, std::io::Error>(f)
    })();
    let (stdout, stderr) = match out.and_then(|f| Ok((f.try_clone()?, f))) {
        Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
        Err(e) => {
            tracing::warn!("relaunch: no log at {}: {e}", log.display());
            (Stdio::inherit(), Stdio::inherit())
        }
    };
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(ENV_RESUME, &path)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        // Its own process group: it outlives us and a terminal's hangup isn't ours to pass on.
        .process_group(0);
    for k in ENV_DROP {
        cmd.env_remove(k);
    }
    match cmd.spawn() {
        Ok(child) => Ok((child, path)),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(format!("couldn't start {}: {e}", exe.display()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The app side

/// Auto-relaunch state kept on [`Quiver`].
#[derive(Default)]
pub struct Relaunch {
    pub sched: Scheduler,
    exe: Option<PathBuf>,
    /// Why a relaunch is being held, as of the last tick.
    blocked: Option<&'static str>,
    restore: Option<Restoring>,
    /// The view has rendered at least once.
    rendered: bool,
    shown_seconds: Option<u64>,
}

/// A resumed start: signal readiness after the first frame, put scroll positions back once
/// there's content to scroll.
struct Restoring {
    path: PathBuf,
    signaled: bool,
    scroll: ScrollState,
    tries: u32,
}

impl Quiver {
    /// Restore a handed-over state and start watching for new builds. Called once, right
    /// after [`Quiver::new`].
    pub fn start_self_update(&mut self, resume: Option<(PathBuf, Resume)>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((path, r)) = resume {
            self.nav = r.nav();
            self.mode = match r.mode.as_deref() {
                Some("keyboard") => InputMode::Keyboard,
                Some("pad") => InputMode::Pad,
                _ => InputMode::Pointer,
            };
            if r.side_filter.as_deref() == Some("installed") {
                self.side_filter = SideFilter::Installed;
            }
            let now = crate::theme::theme(cx).name.clone();
            if let Some(t) = r.theme.filter(|t| *t != now.as_ref()) {
                // The theme follows Omarchy (and QUIVER_THEME), which the new build reads afresh.
                tracing::info!("resume: theme was {t:?}, now {now:?}");
            }
            self.relaunch.restore = Some(Restoring { path, signaled: false, scroll: r.scroll, tries: 0 });
        }

        let mut rx = None;
        if enabled() {
            if let (Some(exe), Some(_)) = (exe_path(), Fingerprint::running()) {
                let (tx, r) = futures::channel::mpsc::unbounded::<Fingerprint>();
                spawn_watcher(exe.clone(), move |fp| tx.unbounded_send(fp).is_ok());
                self.relaunch.exe = Some(exe);
                rx = Some(r);
            }
        } else {
            tracing::info!("{ENV_AUTO}=0: not watching for new builds");
        }
        if rx.is_none() && self.relaunch.restore.is_none() {
            return;
        }

        cx.spawn_in(window, async move |this, cx| {
            loop {
                let fast = this.update(cx, |this, _| this.relaunch_fast_tick()).unwrap_or(false);
                cx.background_executor().timer(if fast { Duration::from_millis(200) } else { Duration::from_secs(1) }).await;
                let mut found = None;
                if let Some(rx) = rx.as_mut() {
                    while let Ok(fp) = rx.try_recv() {
                        found = Some(fp);
                    }
                }
                if this.update_in(cx, |this, window, cx| this.relaunch_tick(found, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn relaunch_fast_tick(&self) -> bool {
        let r = &self.relaunch;
        r.restore.is_some() || r.sched.visible(r.blocked.is_some(), Instant::now())
    }

    /// Why relaunching now would interrupt something, if it would. Pages that add modal
    /// state (dialogs, edits) should keep it in `nav.overlay` or a focused text field, or
    /// add a check here.
    pub fn relaunch_blocker(&self, window: &Window, cx: &gpui::App) -> Option<&'static str> {
        if self.nav.overlay.is_some() {
            return Some("a menu is open");
        }
        match &self.conn {
            Conn::Login(f) if f.busy => return Some("signing in"),
            Conn::Login(f) => {
                if [&f.email, &f.password, &f.code].iter().any(|i| !i.read(cx).text().is_empty()) {
                    return Some("the sign-in form has input");
                }
            }
            Conn::Ready => {
                // Anything focused other than the app itself is a text field (or another
                // widget mid-edit).
                if window.focused(cx).is_some() && !self.focus.is_focused(window) {
                    return Some("a text field has focus");
                }
            }
            Conn::Connecting | Conn::Failed(_) => {}
        }
        None
    }

    fn relaunch_tick(&mut self, found: Option<Fingerprint>, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        if let Some(fp) = found {
            self.relaunch.sched.found(fp);
            cx.notify();
        }
        self.restore_tick(cx);

        let blocked = self.relaunch_blocker(window, cx);
        if blocked != self.relaunch.blocked {
            self.relaunch.blocked = blocked;
            cx.notify();
        }
        match self.relaunch.sched.tick(blocked.is_some(), now) {
            Tick::Nothing => {}
            Tick::Redraw => cx.notify(),
            Tick::Launch => self.relaunch_launch(window, cx),
        }
        let secs = self.relaunch.sched.seconds_left(now);
        if secs != self.relaunch.shown_seconds {
            self.relaunch.shown_seconds = secs;
            cx.notify();
        }
    }

    fn restore_tick(&mut self, cx: &mut Context<Self>) {
        let rendered = self.relaunch.rendered;
        let has_content = matches!(self.conn, Conn::Ready) && !self.games.is_empty();
        let Some(r) = self.relaunch.restore.as_mut() else { return };
        if rendered && !r.signaled {
            // The old app is waiting for this.
            if let Err(e) = std::fs::remove_file(&r.path) {
                tracing::warn!("resume: can't remove {}: {e}", r.path.display());
            }
            r.signaled = true;
            tracing::info!("relaunch: up; told the previous app to exit");
        }
        if !has_content {
            // Signed out, daemon down, ...: nothing to scroll; give up after a while.
            r.tries += 1;
            if r.signaled && r.tries > 50 {
                self.relaunch.restore = None;
            }
            return;
        }
        let s = r.scroll;
        let targets = [(&self.lib_scroll, s.library), (&self.side_scroll, s.side), (&self.body_scroll, s.body)];
        let done = targets.iter().all(|(h, y)| (f32::from(h.offset().y) - y).abs() < 1.);
        r.tries += 1;
        if (done || r.tries > 25) && r.signaled {
            self.relaunch.restore = None;
            return;
        }
        // Layout clamps offsets to the content, so keep setting them until it fits.
        for (h, y) in targets {
            if (f32::from(h.offset().y) - y).abs() >= 1. {
                h.set_offset(point(h.offset().x, px(y)));
            }
        }
        cx.notify();
    }

    fn resume_state(&self, window: &Window, cx: &gpui::App) -> Resume {
        Resume {
            from_pid: std::process::id(),
            nav: serde_json::to_value(&self.nav).unwrap_or_default(),
            mode: Some(
                match self.mode {
                    InputMode::Pointer => "pointer",
                    InputMode::Keyboard => "keyboard",
                    InputMode::Pad => "pad",
                }
                .into(),
            ),
            side_filter: Some(if self.side_filter == SideFilter::Installed { "installed" } else { "all" }.into()),
            scroll: ScrollState {
                library: self.lib_scroll.offset().y.into(),
                side: self.side_scroll.offset().y.into(),
                body: self.body_scroll.offset().y.into(),
            },
            window: Some(WindowState::of(window.window_bounds())),
            theme: Some(crate::theme::theme(cx).name.to_string()),
        }
    }

    fn relaunch_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        let Some(exe) = self.relaunch.exe.clone() else { return };
        let state = self.resume_state(window, cx);
        let (mut child, path) = match spawn_successor(&exe, &state) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("relaunch: {e}");
                self.relaunch.sched.launch_failed(e);
                return;
            }
        };
        tracing::info!("relaunch: started {} (pid {}), waiting for it", exe.display(), child.id());
        cx.spawn_in(window, async move |this, cx| {
            let started = Instant::now();
            let failure = loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if !path.exists() {
                    // Up. Give it a moment to prove it stays up before we go.
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    match child.try_wait() {
                        Ok(Some(status)) => break Some(format!("it exited right after starting ({status})")),
                        _ => break None,
                    }
                }
                match child.try_wait() {
                    Ok(Some(status)) => break Some(format!("it exited ({status})")),
                    Err(e) => break Some(format!("can't wait for it: {e}")),
                    Ok(None) if started.elapsed() > READY_TIMEOUT => {
                        // The whole group: a wrapper script's children too.
                        // SAFETY: plain syscall; the group is the child's own (process_group(0)).
                        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                        let _ = child.kill();
                        let _ = child.wait();
                        break Some(format!("it didn't open its window within {} s", READY_TIMEOUT.as_secs()));
                    }
                    Ok(None) => {}
                }
            };
            let _ = this.update(cx, |this, cx| match failure {
                None => {
                    tracing::info!("relaunch: handed over to pid {}; exiting", child.id());
                    cx.quit();
                }
                Some(why) => {
                    let _ = std::fs::remove_file(&path);
                    let why = format!("{why}. Log: {}", tilde(&log_path()));
                    tracing::warn!("relaunch failed: {why}");
                    this.relaunch.sched.launch_failed(why);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The relaunch toast (countdown, waiting, starting, failed), if any. Called at the end of
    /// `render`, so it also marks that a frame has been drawn.
    pub fn render_update_notice(&mut self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.relaunch.rendered = true;
        let now = Instant::now();
        let r = &self.relaunch;
        let (ic, color, title, detail, actions): (Icon, _, String, Option<String>, &[(&str, Kind)]) =
            if let Some(why) = &r.sched.failed {
                (
                    Icon::TriangleAlert,
                    th.warn,
                    "Update failed to start; still running the previous build".into(),
                    Some(why.clone()),
                    &[("Dismiss", Kind::Ghost)],
                )
            } else {
                match r.sched.phase {
                    Phase::Countdown { .. } => (
                        Icon::RefreshCw,
                        th.acc_hi,
                        format!("Update ready, relaunching in {}…", r.sched.seconds_left(now).unwrap_or(0).max(1)),
                        None,
                        &[("Later", Kind::Ghost), ("Relaunch now", Kind::Primary)],
                    ),
                    Phase::Starting => (Icon::RefreshCw, th.acc_hi, "Update ready, relaunching…".into(), None, &[]),
                    Phase::Waiting if r.sched.visible(r.blocked.is_some(), now) => (
                        Icon::RefreshCw,
                        th.acc_hi,
                        "Update ready; relaunches when you're done".into(),
                        r.blocked.map(|b| format!("Waiting: {b}")),
                        &[("Later", Kind::Ghost), ("Relaunch now", Kind::Secondary)],
                    ),
                    _ => return None,
                }
            };
        let buttons: Vec<AnyElement> = actions
            .iter()
            .map(|(label, kind)| {
                let label = *label;
                button(("relaunch", label.len()), th, *kind, crate::ui::Size::Small, None, label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        match label {
                            "Later" => this.relaunch.sched.later(Instant::now()),
                            "Dismiss" => this.relaunch.sched.failed = None,
                            _ => {
                                if this.relaunch.sched.now() == Tick::Launch {
                                    this.relaunch_launch(window, cx);
                                }
                            }
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("update-notice")
                .absolute()
                .right(px(20.))
                .bottom(px(20.))
                .w(px(380.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .px(px(14.))
                .py(px(11.))
                .rounded(px(10.))
                .bg(th.ink3)
                .border_1()
                .border_color(th.line2)
                .shadow(vec![soft_shadow(rgba(0x000000, 0.5), 14., 40.)])
                .text_size(px(12.5))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(9.))
                        .child(div().pt(px(2.)).child(icon(ic, 14., color)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .flex_1()
                                .min_w_0()
                                .child(div().font_weight(MEDIUM).child(title))
                                .when_some(detail, |d, t| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(t))),
                        ),
                )
                .when(!buttons.is_empty(), |d| d.child(div().flex().justify_end().gap(px(6.)).children(buttons)))
                .into_any_element(),
        )
    }
}

/// Window bounds to open with: the handed-over window's, else `default`.
pub fn window_bounds(resume: Option<&(PathBuf, Resume)>, default: WindowBounds) -> WindowBounds {
    resume.and_then(|(_, r)| r.window).and_then(|w| w.bounds()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u64) -> Fingerprint {
        Fingerprint { dev: 1, ino: n, size: 100, mtime_ns: n as i128 }
    }

    #[test]
    fn debounce_waits_for_a_stable_file_and_reports_it_once() {
        let t0 = Instant::now();
        let mut d = Debounce::new(Some(fp(1)));
        let ok = || Ok(());
        assert_eq!(d.observe(Some(fp(1)), t0, ok), Step::Idle, "the running build");
        assert_eq!(d.observe(Some(fp(2)), t0, ok), Step::Settling);
        assert_eq!(d.observe(Some(fp(2)), t0 + Duration::from_millis(1500), ok), Step::Settling);
        // Rewritten mid-settle: the clock restarts.
        assert_eq!(d.observe(Some(fp(3)), t0 + Duration::from_millis(1900), ok), Step::Settling);
        assert_eq!(d.observe(Some(fp(3)), t0 + Duration::from_millis(3000), ok), Step::Settling);
        assert_eq!(d.observe(Some(fp(3)), t0 + Duration::from_millis(3900), ok), Step::Ready(fp(3)));
        assert_eq!(d.observe(Some(fp(3)), t0 + Duration::from_secs(10), ok), Step::Idle, "only once");
        // A newer build after that is reported too.
        assert_eq!(d.observe(Some(fp(4)), t0 + Duration::from_secs(11), ok), Step::Settling);
        assert_eq!(d.observe(Some(fp(4)), t0 + Duration::from_secs(14), ok), Step::Ready(fp(4)));
    }

    #[test]
    fn debounce_resets_when_the_file_vanishes_and_reports_invalid_builds_once() {
        let t0 = Instant::now();
        let mut d = Debounce::new(Some(fp(1)));
        assert_eq!(d.observe(Some(fp(2)), t0, || Ok(())), Step::Settling);
        assert_eq!(d.observe(None, t0 + Duration::from_secs(1), || Ok(())), Step::Idle);
        assert_eq!(d.observe(Some(fp(2)), t0 + Duration::from_secs(2), || Ok(())), Step::Settling, "restarted");
        let bad = || Err("not an ELF executable".to_string());
        assert_eq!(d.observe(Some(fp(2)), t0 + Duration::from_secs(5), bad), Step::Invalid("not an ELF executable".into()));
        assert_eq!(d.observe(Some(fp(2)), t0 + Duration::from_secs(9), bad), Step::Idle);
        // Back to the running build (a revert): nothing to do.
        assert_eq!(d.observe(Some(fp(1)), t0 + Duration::from_secs(10), bad), Step::Idle);
    }

    #[test]
    fn fingerprint_changes_when_a_file_is_renamed_over() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("quiver-app");
        std::fs::write(&exe, b"one").unwrap();
        let a = Fingerprint::of(&exe).unwrap();
        assert_eq!(Fingerprint::of(&exe), Some(a));
        let tmp = dir.path().join("quiver-app.tmp");
        std::fs::write(&tmp, b"one").unwrap();
        std::fs::rename(&tmp, &exe).unwrap();
        assert_ne!(Fingerprint::of(&exe), Some(a), "new inode");
        assert_eq!(Fingerprint::of(&dir.path().join("missing")), None);
        assert!(Fingerprint::running().is_some());
    }

    #[test]
    fn candidates_must_be_complete_executables() {
        let dir = tempfile::tempdir().unwrap();
        let exec = |p: &Path| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();

        // This test binary is a complete ELF.
        let elf = dir.path().join("elf");
        std::fs::copy("/proc/self/exe", &elf).unwrap();
        assert_eq!(check_candidate(&elf), Ok(()));

        // Cut short: the section headers (at the end) are missing.
        let bytes = std::fs::read(&elf).unwrap();
        let short = dir.path().join("short");
        std::fs::write(&short, &bytes[..bytes.len() / 2]).unwrap();
        exec(&short);
        assert!(check_candidate(&short).unwrap_err().contains("incomplete"));

        let tiny = dir.path().join("tiny");
        std::fs::write(&tiny, &bytes[..20]).unwrap();
        exec(&tiny);
        assert!(check_candidate(&tiny).is_err());

        let script = dir.path().join("script");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(check_candidate(&script), Err("not executable".into()));
        exec(&script);
        assert_eq!(check_candidate(&script), Ok(()));

        let junk = dir.path().join("junk");
        std::fs::write(&junk, vec![0u8; 4096]).unwrap();
        exec(&junk);
        assert!(check_candidate(&junk).is_err());
    }

    #[test]
    fn scheduler_defers_while_blocked_then_counts_down() {
        let t0 = Instant::now();
        let mut s = Scheduler::default();
        assert_eq!(s.tick(false, t0), Tick::Nothing);
        s.found(fp(2));
        assert_eq!(s.tick(true, t0), Tick::Nothing, "busy: hold it");
        assert!(s.visible(true, t0), "and say why");
        assert_eq!(s.tick(false, t0), Tick::Redraw);
        assert_eq!(s.seconds_left(t0), Some(3));
        assert_eq!(s.tick(false, t0 + Duration::from_secs(1)), Tick::Nothing);
        assert_eq!(s.seconds_left(t0 + Duration::from_millis(1500)), Some(2));
        // Opening a menu mid-countdown pauses it; idle again restarts it from 3.
        assert_eq!(s.tick(true, t0 + Duration::from_secs(2)), Tick::Redraw);
        assert_eq!(s.phase, Phase::Waiting);
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(s.tick(false, t1), Tick::Redraw);
        assert_eq!(s.tick(false, t1 + Duration::from_millis(2900)), Tick::Nothing);
        assert_eq!(s.tick(false, t1 + COUNTDOWN), Tick::Launch);
        assert_eq!(s.phase, Phase::Starting);
        assert_eq!(s.tick(false, t1 + Duration::from_secs(10)), Tick::Nothing, "one launch at a time");
    }

    #[test]
    fn scheduler_later_holds_this_build_but_not_the_next() {
        let t0 = Instant::now();
        let mut s = Scheduler::default();
        s.found(fp(2));
        s.tick(false, t0);
        s.later(t0);
        assert_eq!(s.tick(false, t0 + Duration::from_secs(60)), Tick::Nothing);
        assert!(!s.visible(false, t0 + Duration::from_secs(60)));
        s.found(fp(3));
        assert_eq!(s.tick(false, t0 + Duration::from_secs(61)), Tick::Redraw, "a newer build asks again");
        s.later(t0 + Duration::from_secs(62));
        assert_eq!(s.tick(false, t0 + Duration::from_secs(62) + SNOOZE), Tick::Redraw, "the snooze ends");
        assert_eq!(s.now(), Tick::Launch, "Relaunch now skips the countdown");
    }

    #[test]
    fn scheduler_failure_keeps_running_and_tries_a_newer_build() {
        let t0 = Instant::now();
        let mut s = Scheduler::default();
        s.found(fp(2));
        s.tick(false, t0);
        assert_eq!(s.tick(false, t0 + COUNTDOWN), Tick::Launch);
        s.launch_failed("it exited (exit status: 1)".into());
        assert_eq!(s.phase, Phase::Idle);
        assert!(s.failed.is_some() && s.visible(false, t0));
        assert_eq!(s.tick(false, t0 + Duration::from_secs(30)), Tick::Nothing, "the broken build isn't retried");

        // A fix lands while another attempt is starting: it's tried after that one fails.
        s.found(fp(3));
        s.tick(false, t0);
        assert_eq!(s.now(), Tick::Launch);
        s.found(fp(4));
        s.launch_failed("timeout".into());
        assert_eq!(s.phase, Phase::Waiting);
        assert_eq!(s.tick(false, t0 + Duration::from_secs(40)), Tick::Redraw);
        assert_eq!(s.failed, None, "a new attempt clears the old error");
    }

    #[test]
    fn resume_round_trips_navigation_and_closes_overlays() {
        let mut nav = Nav::new();
        nav.open_game(4242);
        nav.select_tab(Tab::Saves);
        nav.open_palette();
        let r = Resume {
            from_pid: 7,
            nav: serde_json::to_value(&nav).unwrap(),
            mode: Some("pad".into()),
            scroll: ScrollState { library: -10., side: -20., body: -300. },
            window: Some(WindowState { kind: WindowKind::Maximized, x: 10., y: 20., w: 1200., h: 800. }),
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Resume = serde_json::from_str(&json).unwrap();
        let n = back.nav();
        assert_eq!(n.page, Page::Game(4242));
        assert_eq!(n.tab, Tab::Saves);
        assert_eq!(n.zone, nav.zone);
        assert_eq!(n.overlay, None, "the palette is closed");
        assert_eq!(back.scroll, r.scroll);
        assert!(matches!(back.window.unwrap().bounds(), Some(WindowBounds::Maximized(_))));
    }

    #[test]
    fn resume_is_lenient_about_state_from_another_build() {
        // A newer Nav this build doesn't understand: page and tab still come through.
        let r = Resume {
            nav: serde_json::json!({ "page": { "Game": 99 }, "tab": "LaunchOptions", "zone": "SomeNewZone" }),
            ..Default::default()
        };
        let n = r.nav();
        assert_eq!(n.page, Page::Game(99));
        assert_eq!(n.tab, Tab::LaunchOptions);
        // Garbage, or an older build's missing fields: defaults.
        let r: Resume = serde_json::from_str(r#"{"nav": 5, "unknown": true}"#).unwrap();
        assert_eq!(r.nav().page, Page::Library);
        let r: Resume = serde_json::from_str(r#"{"nav": {"page": "Activity"}}"#).unwrap();
        assert_eq!(r.nav().page, Page::Activity);
        assert_eq!(WindowState { kind: WindowKind::Windowed, x: 0., y: 0., w: 0., h: 0. }.bounds(), None);
    }
}
