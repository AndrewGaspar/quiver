//! Restarting onto a newly installed build once nothing needs the daemon.
//!
//! - **Staged update:** the file quiverd was started from now is a different file (inode)
//!   than the one running (`tools/install-user.sh`, pacman and `cargo build` all rename a
//!   new file into place). The install manifest names its version.
//! - **Busy:** a game session (any holder), a game quiverd is starting, an install,
//!   update, repair, verify or import running, a perf subscription or attached HUD, a perf
//!   session of a launch that is starting, a sign-in waiting for its 2FA code, a Moonlight
//!   stream (streaming, starting or kept for a resume, whatever it shows: its host runs in
//!   quiverd), a bridge connection to the broker, a hold, or a request in flight. Event subscriptions (app
//!   windows) and finished one-shot calls are not.
//! - **Restart:** once an update has been staged for [`Settings::settle`] and nothing has
//!   been busy for [`Settings::idle`] (requests in flight aside, which are waited out), the
//!   main loop stops accepting connections and [`Lifecycle::try_close`] checks again and,
//!   in the same critical section, starts refusing requests ([`quiver_api::daemon::RESTARTING`]).
//!   Every connection gets `Reply::Closing`; then quiverd execs the new binary with the same
//!   pid, handing it the listening sockets (`QUIVER_LISTEN_FDS`), so connections made in
//!   between wait in the backlog and the broker keeps its port.
//! - **Moving into the unit:** a daemon outside `quiverd.service` (started from a terminal
//!   before the unit existed) moves itself in the same way once idle, waiting on streams
//!   too: instead of an exec it hands its sockets to the unit's process and exits
//!   (`unit.rs`, design/SERVICE.md).
//! - **Holds** keep it from restarting: tied to the connection that asked (gone when that
//!   client exits or crashes), or with a time limit (at most a day), so a holder that
//!   crashed never blocks updates for long.

use std::collections::BTreeMap;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::daemon::{DaemonStatus, Hold, MAX_HOLD_SECS, StagedUpdate};

/// Set by `tools/install-user.sh` for the build (`0.1.0.r512.gabc1234`, the commit).
pub(crate) const BUILD_VERSION: Option<&str> = option_env!("QUIVER_BUILD_VERSION");
pub(crate) const BUILD_COMMIT: Option<&str> = option_env!("QUIVER_BUILD_COMMIT");

/// Passed to the new build across the exec.
const LISTEN_FDS: &str = "QUIVER_LISTEN_FDS";
const RESTARTED_FROM: &str = "QUIVER_RESTARTED_FROM";
const RESTART_COUNT: &str = "QUIVER_RESTART_COUNT";

/// A sign-in waiting for its 2FA code counts as busy for this long.
pub(crate) const TWO_FA_BUSY: Duration = Duration::from_secs(600);

/// This build's full version.
pub(crate) fn build() -> String {
    BUILD_VERSION.unwrap_or(env!("CARGO_PKG_VERSION")).to_owned()
}

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// `QUIVER_AUTO_RESTART=0` turns restarting onto new builds off.
    pub auto: bool,
    /// `QUIVER_RESTART_IDLE_SECS` (30): how long nothing must need the daemon.
    pub idle: Duration,
    /// `QUIVER_RESTART_SETTLE_SECS` (3): how long a new build must have been in place (the
    /// rest of an install lands meanwhile).
    pub settle: Duration,
    /// `QUIVER_RESTART_POLL_MS` (2000): how often to look.
    pub poll: Duration,
}

impl Settings {
    fn from_env() -> Self {
        let num = |k: &str, default: u64| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default);
        let auto = !std::env::var("QUIVER_AUTO_RESTART").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no"));
        Self {
            auto,
            idle: Duration::from_secs(num("QUIVER_RESTART_IDLE_SECS", 30)),
            settle: Duration::from_secs(num("QUIVER_RESTART_SETTLE_SECS", 3)),
            poll: Duration::from_millis(num("QUIVER_RESTART_POLL_MS", 2000).max(50)),
        }
    }
}

/// A file's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        (m.is_file() && m.mode() & 0o111 != 0).then(|| Self { dev: m.dev(), ino: m.ino() })
    }
}

/// What keeps the daemon busy, from outside this module (the daemon gathers it).
#[derive(Debug, Clone, Default)]
pub(crate) struct Busy {
    /// Game sessions, as "AC Origins (bridge)".
    pub sessions: Vec<String>,
    /// Games quiverd is starting (no session yet), by name.
    pub starting: Vec<String>,
    /// Running jobs: "installing X", "verifying Y".
    pub jobs: Vec<String>,
    pub perf_subscriptions: usize,
    pub huds: usize,
    /// Perf sessions of launches without a game session yet (`quiver steam-run`), by name.
    pub perf_sessions: Vec<String>,
    pub awaiting_2fa: bool,
    /// Streams whose host runs in quiverd (Moonlight's), as "Moonlight stream of X (kept…)".
    /// They keep the daemon from any restart. Quiver's own streams aren't here: their hosts
    /// outlive a restart and are re-adopted.
    pub streams: Vec<String>,
    /// Every stream this machine serves (kept ones too), by title. An exec leaves their hosts
    /// running and re-adopts them, so they only keep the daemon from moving into the unit,
    /// which hands over to another process.
    pub hosted_streams: Vec<String>,
}

struct HoldEntry {
    hold: Hold,
    /// Released when this connection closes.
    conn: Option<u64>,
    deadline: Option<Instant>,
}

struct Staged {
    file: FileId,
    at: Instant,
    update: StagedUpdate,
}

#[derive(Default)]
struct State {
    /// Requests in flight, by method.
    inflight: BTreeMap<String, usize>,
    broker_conns: usize,
    holds: BTreeMap<u64, HoldEntry>,
    next_hold: u64,
    staged: Option<Staged>,
    /// A build exec failed for: not tried again until another one is staged.
    failed: Option<FileId>,
    idle_since: Option<Instant>,
    /// Move into this unit once idle (`unit.rs`).
    relocate: Option<Relocation>,
}

struct Relocation {
    unit: String,
    /// Moving failed (why): not tried again by this process.
    failed: Option<String>,
}

impl State {
    /// The unit to move into, while that is still to be tried.
    fn relocating(&self) -> Option<&str> {
        self.relocate.as_ref().filter(|r| r.failed.is_none()).map(|r| r.unit.as_str())
    }
}

pub(crate) struct Lifecycle {
    pub settings: Settings,
    /// The binary quiverd was started from, which the new build replaces.
    exe: Option<PathBuf>,
    /// The running binary's identity (as it was started).
    running: Option<FileId>,
    /// The install manifest (version and commit of the staged build).
    manifest: PathBuf,
    pub started: DateTime<Utc>,
    pub restarts: u32,
    pub restarted_from: Option<String>,
    /// The unit this process's cgroup names (`quiverd.service`, `kitty-1-0.scope`).
    pub unit: Option<String>,
    /// It runs as its unit (`unit::in_unit`).
    pub in_unit: bool,
    /// Refusing new work: the restart is under way. Changed only under `state`'s lock;
    /// read without it.
    closed: AtomicBool,
    state: Mutex<State>,
}

/// A request in flight; refused requests get none.
pub(crate) struct Pass {
    lc: Arc<Lifecycle>,
    what: Option<String>,
}

impl Drop for Pass {
    fn drop(&mut self) {
        let Some(what) = self.what.take() else { return };
        let mut st = self.lc.lock();
        if let Some(n) = st.inflight.get_mut(&what) {
            *n -= 1;
            if *n == 0 {
                st.inflight.remove(&what);
            }
        }
    }
}

/// An open bridge connection to the broker.
pub(crate) struct BrokerPass(Arc<Lifecycle>);

impl Drop for BrokerPass {
    fn drop(&mut self) {
        self.0.lock().broker_conns -= 1;
    }
}

/// The restart [`Lifecycle::try_close`] committed to.
pub(crate) struct Plan {
    pub exe: PathBuf,
    /// The new build's version, if the manifest says.
    pub to: Option<String>,
    /// The staged build (none when only moving into the unit).
    file: Option<FileId>,
    /// Move into this unit (hand the sockets over) instead of exec'ing: the unit runs the
    /// installed build anyway.
    pub relocate: Option<String>,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What keeps the daemon busy, in words; empty: idle. Requests in flight count only
/// with `inflight` (the restart itself waits them out; the idle timer ignores them).
fn reasons(st: &State, busy: &Busy, inflight: bool, now: DateTime<Utc>) -> Vec<String> {
    let mut out = busy.sessions.clone();
    if st.relocating().is_some() {
        out.extend(busy.hosted_streams.iter().map(|t| format!("stream: {t}")));
    }
    out.extend(busy.starting.iter().map(|n| format!("starting {n}")));
    out.extend(busy.jobs.iter().cloned());
    if busy.perf_subscriptions > 0 {
        out.push(plural(busy.perf_subscriptions, "perf subscription", "perf subscriptions"));
    }
    if busy.huds > 0 {
        out.push(plural(busy.huds, "HUD attached", "HUDs attached"));
    }
    out.extend(busy.perf_sessions.iter().map(|n| format!("perf session for {n}")));
    out.extend(busy.streams.iter().cloned());
    if busy.awaiting_2fa {
        out.push("a sign-in waiting for its 2FA code".into());
    }
    // A bridge's session explains its connection; without one, say there are connections.
    if st.broker_conns > 0 && busy.sessions.is_empty() {
        out.push(plural(st.broker_conns, "bridge connection", "bridge connections"));
    }
    out.extend(st.holds.values().map(|h| h.hold.describe(now)));
    if inflight && !st.inflight.is_empty() {
        let n: usize = st.inflight.values().sum();
        let names: Vec<&str> = st.inflight.keys().map(String::as_str).collect();
        out.push(format!("{} ({})", plural(n, "request in flight", "requests in flight"), names.join(", ")));
    }
    out
}

/// `~/.local/state/quiver/installed.json` (where `tools/install-user.sh` keeps it).
fn manifest_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/quiver/installed.json")
}

/// The version and commit the install manifest gives `exe`, if it lists it.
fn manifest_build(manifest: &Path, exe: &Path) -> (Option<String>, Option<String>) {
    let Some(v) = std::fs::read_to_string(manifest).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else {
        return (None, None);
    };
    let lists = v["files"].as_array().is_some_and(|f| f.iter().any(|f| f["path"].as_str().map(Path::new) == Some(exe)));
    if !lists {
        return (None, None);
    }
    let s = |k: &str| v[k].as_str().map(str::to_owned);
    (s("version"), s("commit"))
}

/// The binary this process was started from: `/proc/self/exe` without " (deleted)".
fn started_from() -> Option<PathBuf> {
    let p = std::fs::read_link("/proc/self/exe").ok()?;
    let s = p.to_str()?;
    Some(PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(s)))
}

impl Lifecycle {
    pub(crate) fn new(settings: Settings, exe: Option<PathBuf>, running: Option<FileId>, manifest: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            settings,
            exe,
            running,
            manifest,
            started: Utc::now(),
            restarts: 0,
            restarted_from: None,
            unit: None,
            in_unit: false,
            closed: AtomicBool::new(false),
            state: Mutex::default(),
        })
    }

    /// This process's: its binary, settings from the environment, and the restart it
    /// came from (whose variables it takes out of the environment, see [`take_inherited`]).
    pub(crate) fn from_env(in_unit: bool, moved_from: Option<&str>) -> Arc<Self> {
        let exe = started_from();
        // The running image, even once its file was replaced.
        let running = FileId::of(Path::new("/proc/self/exe"));
        let mut lc = Self::new(Settings::from_env(), exe, running, manifest_path());
        let l = Arc::get_mut(&mut lc).expect("fresh");
        l.restarts = std::env::var(RESTART_COUNT).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        l.restarted_from = std::env::var(RESTARTED_FROM).ok().filter(|v| !v.is_empty()).or_else(|| moved_from.map(str::to_owned));
        l.unit = quiver_api::service::current_unit();
        l.in_unit = in_unit;
        // SAFETY: called from `main` before anything else runs that reads the environment
        // (the runtime's threads are idle); games must not inherit these.
        unsafe {
            std::env::remove_var(RESTART_COUNT);
            std::env::remove_var(RESTARTED_FROM);
        }
        lc
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Admit a request, counted as in flight until the pass drops; `None` is a request
    /// that never blocks a restart (status, subscribe). Refused while restarting.
    pub(crate) fn enter(self: &Arc<Self>, what: Option<&str>) -> Result<Pass, String> {
        let Some(what) = what else { return Ok(Pass { lc: self.clone(), what: None }) };
        let mut st = self.lock();
        if self.closed() {
            return Err(format!("quiverd is restarting onto a new build; '{what}' did not run (send it again)"));
        }
        *st.inflight.entry(what.to_owned()).or_default() += 1;
        Ok(Pass { lc: self.clone(), what: Some(what.to_owned()) })
    }

    /// A bridge connected to the broker. (The main loop doesn't accept while restarting.)
    pub(crate) fn broker_conn(self: &Arc<Self>) -> BrokerPass {
        self.lock().broker_conns += 1;
        BrokerPass(self.clone())
    }

    /// Take a hold: for `for_secs` (at most [`MAX_HOLD_SECS`]), else while `conn` lasts.
    pub(crate) fn hold(&self, reason: &str, source: &str, for_secs: Option<u64>, conn: Option<u64>) -> Result<Hold> {
        let reason = reason.trim();
        if reason.is_empty() {
            bail!("say why (--reason): it shows in `quiver daemon status`");
        }
        let conn = match (for_secs, conn) {
            (Some(0), _) => bail!("a hold needs a duration above zero"),
            (Some(s), _) if s > MAX_HOLD_SECS => bail!("holds last at most {} (renew it for longer)", quiver_api::daemon::human_secs(MAX_HOLD_SECS)),
            (Some(_), _) => None,
            (None, Some(c)) => Some(c),
            (None, None) => bail!("a hold without a time limit needs its own connection"),
        };
        let mut st = self.lock();
        st.next_hold += 1;
        let now = Utc::now();
        let hold = Hold {
            id: st.next_hold,
            reason: reason.to_owned(),
            source: source.to_owned(),
            since: now,
            expires: for_secs.map(|s| now + chrono::Duration::seconds(s as i64)),
        };
        let deadline = for_secs.map(|s| Instant::now() + Duration::from_secs(s));
        st.holds.insert(hold.id, HoldEntry { hold: hold.clone(), conn, deadline });
        Ok(hold)
    }

    /// Release a hold by id, every hold, or the only one.
    pub(crate) fn release(&self, id: Option<u64>, all: bool) -> Result<Vec<Hold>> {
        let mut st = self.lock();
        Self::expire(&mut st, Instant::now());
        let ids: Vec<u64> = match (id, all) {
            (_, true) => st.holds.keys().copied().collect(),
            (Some(id), _) if st.holds.contains_key(&id) => vec![id],
            (Some(id), _) => bail!("no hold #{id}"),
            (None, _) if st.holds.len() <= 1 => st.holds.keys().copied().collect(),
            (None, _) => {
                let now = Utc::now();
                let list: Vec<String> = st.holds.values().map(|h| h.hold.describe(now)).collect();
                bail!("there are {} holds; name one (or --all): {}", st.holds.len(), list.join("; "))
            }
        };
        Ok(ids.into_iter().filter_map(|id| st.holds.remove(&id)).map(|h| h.hold).collect())
    }

    /// A connection closed: its holds go.
    pub(crate) fn disconnect(&self, conn: u64) -> Vec<Hold> {
        let mut st = self.lock();
        let ids: Vec<u64> = st.holds.iter().filter(|(_, h)| h.conn == Some(conn)).map(|(id, _)| *id).collect();
        ids.into_iter().filter_map(|id| st.holds.remove(&id)).map(|h| h.hold).collect()
    }

    /// Drop holds whose time ran out (returned).
    fn expire(st: &mut State, now: Instant) -> Vec<Hold> {
        let ids: Vec<u64> = st.holds.iter().filter(|(_, h)| h.deadline.is_some_and(|d| d <= now)).map(|(id, _)| *id).collect();
        ids.into_iter().filter_map(|id| st.holds.remove(&id)).map(|h| h.hold).collect()
    }

    /// Move into `unit` once idle (unless that already failed).
    pub(crate) fn want_relocation(&self, unit: String) {
        let mut st = self.lock();
        if self.in_unit || st.relocate.as_ref().is_some_and(|r| r.failed.is_some()) {
            return;
        }
        if st.relocate.is_none() {
            tracing::info!("quiverd runs outside {unit}; moving into it once nothing needs it");
        }
        st.relocate = Some(Relocation { unit, failed: None });
    }

    /// Notice a new build in place of ours.
    fn check_staged(&self, st: &mut State, now: Instant) {
        let (Some(exe), Some(running)) = (&self.exe, self.running) else { return };
        let on_disk = FileId::of(exe).filter(|f| *f != running);
        match on_disk {
            None => st.staged = None,
            Some(f) if st.staged.as_ref().is_some_and(|s| s.file == f) => {}
            Some(f) => {
                let (version, commit) = manifest_build(&self.manifest, exe);
                let note = (st.failed == Some(f)).then(|| "restarting onto it failed; see quiverd.log".to_owned());
                tracing::info!(exe = %exe.display(), version = version.as_deref().unwrap_or("?"), "a new build is installed; restarting onto it once idle");
                st.staged = Some(Staged { file: f, at: now, update: StagedUpdate { since: Utc::now(), version, commit, note } });
            }
        }
    }

    /// The watcher's look: expire holds, notice a staged build, track idleness. True when
    /// it is time to try restarting. Returns the holds that ran out too.
    pub(crate) fn tick(&self, busy: &Busy) -> (bool, Vec<Hold>) {
        self.tick_at(busy, Instant::now())
    }

    fn tick_at(&self, busy: &Busy, now: Instant) -> (bool, Vec<Hold>) {
        let mut st = self.lock();
        let expired = Self::expire(&mut st, now);
        self.check_staged(&mut st, now);
        if reasons(&st, busy, false, Utc::now()).is_empty() {
            st.idle_since.get_or_insert(now);
        } else {
            st.idle_since = None;
        }
        let staged = st.staged.as_ref().is_some_and(|s| Some(s.file) != st.failed && now.duration_since(s.at) >= self.settings.settle);
        let ready = self.settings.auto
            && !self.closed()
            && (staged || st.relocating().is_some())
            && st.idle_since.is_some_and(|t| now.duration_since(t) >= self.settings.idle);
        (ready, expired)
    }

    /// Commit to restarting if a build is staged and nothing at all is busy, requests in
    /// flight included: from here on [`Self::enter`] refuses. `busy` is read under the same
    /// lock `enter` takes, so no request can start in between. The caller must not be
    /// accepting new connections. `Err`: why not.
    pub(crate) fn try_close(&self, busy: impl FnOnce() -> Busy) -> Result<Plan, Vec<String>> {
        let mut st = self.lock();
        let now = Instant::now();
        Self::expire(&mut st, now);
        self.check_staged(&mut st, now);
        if self.closed() {
            return Err(vec!["already restarting".into()]);
        }
        let plan = match (&self.exe, &st.staged, st.relocating()) {
            (exe, staged, Some(unit)) => Plan {
                exe: exe.clone().unwrap_or_default(),
                to: staged.as_ref().and_then(|s| s.update.version.clone()),
                file: staged.as_ref().map(|s| s.file),
                relocate: Some(unit.to_owned()),
            },
            (Some(exe), Some(staged), None) => Plan { exe: exe.clone(), to: staged.update.version.clone(), file: Some(staged.file), relocate: None },
            _ => return Err(vec!["no new build staged".into()]),
        };
        let why = reasons(&st, &busy(), true, Utc::now());
        if !why.is_empty() {
            return Err(why);
        }
        self.closed.store(true, Ordering::SeqCst);
        Ok(plan)
    }

    /// The exec failed: take requests again, and don't retry this build.
    pub(crate) fn reopen(&self, plan: &Plan, error: &str) {
        let mut st = self.lock();
        if plan.relocate.is_some() {
            if let Some(r) = st.relocate.as_mut() {
                r.failed = Some(error.to_owned());
            }
        } else {
            st.failed = plan.file;
            if let Some(s) = st.staged.as_mut().filter(|s| Some(s.file) == plan.file) {
                s.update.note = Some(format!("restarting onto it failed: {error}"));
            }
        }
        self.closed.store(false, Ordering::SeqCst);
    }

    pub(crate) fn status(&self, busy: &Busy) -> DaemonStatus {
        let mut st = self.lock();
        let now = Instant::now();
        Self::expire(&mut st, now);
        self.check_staged(&mut st, now);
        let waiting_on = reasons(&st, busy, true, Utc::now());
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            build: build(),
            commit: BUILD_COMMIT.map(str::to_owned),
            pid: std::process::id(),
            exe: self.exe.as_ref().map(|p| p.display().to_string()),
            started: self.started,
            restarts: self.restarts,
            restarted_from: self.restarted_from.clone(),
            auto_restart: self.settings.auto,
            idle_secs: self.settings.idle.as_secs(),
            update: st.staged.as_ref().map(|s| s.update.clone()),
            restarting: self.closed(),
            idle_for_secs: if waiting_on.is_empty() { st.idle_since.map(|t| now.duration_since(t).as_secs()) } else { None },
            waiting_on,
            holds: st.holds.values().map(|h| h.hold.clone()).collect(),
            unit: self.unit.clone(),
            in_unit: self.in_unit,
            relocation: st.relocate.as_ref().map(|r| match &r.failed {
                None => format!("moving into {} once nothing needs it", r.unit),
                Some(e) => format!("moving into {} failed: {e}", r.unit),
            }),
        }
    }
}

fn set_cloexec(fd: RawFd, on: bool) {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
            libc::fcntl(fd, libc::F_SETFD, flags);
        }
    }
}

/// Replace this process with the new build, handing it the listening sockets (named
/// `fds`). Returns only if the exec failed.
pub(crate) fn exec(plan: &Plan, fds: &[(&str, RawFd)], restarts: u32) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    for (_, fd) in fds {
        set_cloexec(*fd, false);
    }
    let spec: Vec<String> = fds.iter().map(|(name, fd)| format!("{name}:{fd}")).collect();
    let mut cmd = std::process::Command::new(&plan.exe);
    // The unit's notify socket, taken out of the environment at start (`unit.rs`).
    if let Some(n) = crate::unit::notify_socket() {
        cmd.env("NOTIFY_SOCKET", n);
    }
    let err = cmd
        .env(LISTEN_FDS, spec.join(","))
        .env(RESTARTED_FROM, build())
        .env(RESTART_COUNT, (restarts + 1).to_string())
        .exec();
    for (_, fd) in fds {
        set_cloexec(*fd, true);
    }
    err
}

/// Listening sockets handed over by the previous build.
#[derive(Default)]
pub(crate) struct Inherited {
    pub unix: Option<std::os::unix::net::UnixListener>,
    pub broker: Option<std::net::TcpListener>,
    /// Remote play's listener, while it was on (`remote`).
    pub remote: Option<std::net::TcpListener>,
}

/// A descriptor that is a listening socket of `domain`.
fn listening(fd: RawFd, domain: libc::c_int) -> bool {
    let get = |opt: libc::c_int| -> Option<libc::c_int> {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt into a c_int of the right size.
        let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, opt, (&raw mut v).cast(), &mut len) };
        (r == 0).then_some(v)
    };
    get(libc::SO_ACCEPTCONN) == Some(1) && get(libc::SO_DOMAIN) == Some(domain)
}

impl Inherited {
    pub(crate) fn is_empty(&self) -> bool {
        self.unix.is_none() && self.broker.is_none() && self.remote.is_none()
    }
}

/// Take the sockets the previous build handed over (`QUIVER_LISTEN_FDS`), out of the
/// environment too. Call first thing in `main`.
pub(crate) fn take_inherited() -> Inherited {
    let spec = std::env::var(LISTEN_FDS).ok();
    // SAFETY: first thing in `main`; nothing else reads the environment yet.
    unsafe { std::env::remove_var(LISTEN_FDS) };
    let mut fds = Vec::new();
    for part in spec.as_deref().unwrap_or_default().split(',').filter(|p| !p.is_empty()) {
        let Some((name, fd)) = part.split_once(':').and_then(|(n, f)| Some((n, f.parse::<RawFd>().ok()?))) else {
            tracing::warn!("{LISTEN_FDS}: ignoring '{part}'");
            continue;
        };
        fds.push((name.to_owned(), fd));
    }
    inherited_from(fds)
}

/// Named listening sockets (from an exec or a handover into the unit) as [`Inherited`];
/// a descriptor named wrongly is closed, one that isn't a listening socket left alone.
pub(crate) fn inherited_from(fds: Vec<(String, RawFd)>) -> Inherited {
    use std::os::fd::FromRawFd;
    let mut out = Inherited::default();
    for (name, fd) in fds {
        let name = name.as_str();
        let domain = match name {
            "unix" => libc::AF_UNIX,
            "broker" | "remote" => libc::AF_INET,
            _ => {
                tracing::warn!("inherited sockets: unknown socket '{name}'");
                // SAFETY: handed to us to own; nothing else holds it.
                unsafe { libc::close(fd) };
                continue;
            }
        };
        let ok = listening(fd, domain) || (domain == libc::AF_INET && listening(fd, libc::AF_INET6));
        if !ok {
            tracing::warn!("inherited sockets: fd {fd} ({name}) is not a listening socket");
            continue;
        }
        // Games and other children must not inherit it.
        set_cloexec(fd, true);
        // SAFETY: the previous build handed this descriptor over for us to own; checked above.
        match name {
            "unix" => out.unix = Some(unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) }),
            "remote" => out.remote = Some(unsafe { std::net::TcpListener::from_raw_fd(fd) }),
            _ => out.broker = Some(unsafe { std::net::TcpListener::from_raw_fd(fd) }),
        }
    }
    out
}

/// A session's holders for "waiting on": a lab holder as the instance and its owner
/// (`lab origins, codex:pharos-xreal`, from `lab_word`), else as it is.
fn holders_in_words(holders: &[String], lab_word: impl Fn() -> Option<String>) -> String {
    holders.iter().map(|h| if h == quiver_api::lab::LAB_HOLDER { lab_word().unwrap_or_else(|| h.clone()) } else { h.clone() }).collect::<Vec<_>>().join(", ")
}

// ---------------------------------------------------------------------------------------
// The daemon's side

impl crate::Daemon {
    /// What keeps the daemon busy right now (besides what [`Lifecycle`] tracks itself).
    pub(crate) fn busy(&self) -> Busy {
        let sessions = self.sessions.list();
        let running: Vec<u32> = self.running.lock().unwrap().iter().copied().collect();
        let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        // Installed games by name, then the cached library (a container's game may not be
        // installed here), then the product id.
        let mut name = |id: u32| {
            names
                .entry(id)
                .or_insert_with(|| {
                    let n = self.product_name(id);
                    if n != id.to_string() {
                        return n;
                    }
                    let lib = self.library.try_lock().ok();
                    let cached = lib.as_ref().and_then(|l| l.as_ref()?.iter().find(|e| e.ubisoft_id() == Some(id)).map(|e| e.name.clone()));
                    cached.unwrap_or(n)
                })
                .clone()
        };
        // Steam games played through `quiver steam-run` count as running games too.
        let local = self.local.list();
        // A lab instance's session names the instance and who's using it, so whoever is
        // waiting for the restart knows whom to ask (design/LAB.md "Owners").
        let lab = quiver_core::lab::Lab::new(quiver_core::lab::root(&self.cfg.prefixes));
        let holders = |s: &quiver_api::ActiveSession| holders_in_words(&s.holders, || quiver_core::lab::owner::describe_running(&lab, &quiver_api::GameRef::ubisoft(s.product_id)));
        let mut busy = Busy {
            sessions: sessions
                .iter()
                .map(|s| format!("{} ({})", name(s.product_id), holders(s)))
                .chain(local.iter().map(|s| format!("{} ({})", s.game_ref(), s.holders.join(", "))))
                .collect(),
            starting: running.iter().filter(|p| !sessions.iter().any(|s| s.product_id == **p)).map(|p| name(*p)).collect(),
            ..Default::default()
        };
        busy.jobs = self.downloads.running(|g| match g.as_ubisoft() {
            Some(id) => name(id),
            None => g.to_string(),
        });
        // Other stores' games Quiver started (Epic) are running games too.
        busy.sessions.extend(self.running_games.lock().unwrap().iter().map(|g| format!("{g} (quiver)")));
        busy.jobs.extend(
            self.jobs
                .lock()
                .unwrap()
                .values()
                .filter(|j| j.kind == quiver_api::JobKind::Import && !j.done)
                .map(|j| format!("importing {}", j.name)),
        );
        (busy.perf_subscriptions, busy.huds) = self.perf.consumers();
        busy.perf_sessions = self
            .perf
            .sessions()
            .into_iter()
            .filter(|p| !sessions.iter().chain(&local).any(|s| s.game_ref() == p.game_ref()) && !running.contains(&p.product_id))
            .map(|p| if p.product_id == 0 { p.game_ref().to_string() } else { name(p.product_id) })
            .collect();
        busy.streams = self.streams.busy();
        busy.awaiting_2fa = self.pending_2fa.lock().unwrap().is_some()
            && self.pending_2fa_at.lock().unwrap().is_some_and(|t| t.elapsed() < TWO_FA_BUSY);
        busy.hosted_streams = self.streams.list().into_iter().map(|s| s.title).collect();
        busy
    }

    /// `Request::Daemon`. `conn`: the asking connection (holds without a time limit).
    pub(crate) fn handle_daemon(&self, source: &str, conn: Option<u64>, req: quiver_api::daemon::DaemonRequest) -> Result<serde_json::Value> {
        use quiver_api::daemon::DaemonRequest as D;
        Ok(match req {
            D::Status => serde_json::json!(self.lifecycle.status(&self.busy())),
            D::Hold { reason, for_secs } => {
                let hold = self.lifecycle.hold(&reason, source, for_secs, conn)?;
                let how = match for_secs {
                    Some(s) => format!("for {}", quiver_api::daemon::human_secs(s)),
                    None => "while its client runs".into(),
                };
                self.log_activity(source, format!("hold #{} on restarting quiverd {how}: {}", hold.id, hold.reason), None);
                serde_json::json!(hold)
            }
            D::Release { id, all } => {
                let released = self.lifecycle.release(id, all)?;
                for h in &released {
                    self.log_activity(source, format!("released hold #{} ({})", h.id, h.reason), None);
                }
                serde_json::json!(released)
            }
        })
    }

    /// A connection closed: release its holds.
    pub(crate) fn lifecycle_disconnect(&self, conn: u64) {
        for h in self.lifecycle.disconnect(conn) {
            self.log_activity(&h.source, format!("released hold #{} ({}): its client went away", h.id, h.reason), None);
        }
    }

    /// The restart is decided: log it, tell subscribers, and send every connection
    /// `Reply::Closing`, waiting (briefly) until each has it.
    pub(crate) async fn announce_restart(&self, plan: &Plan) {
        let to = plan.to.clone();
        let from = build();
        let (what, reason) = match &plan.relocate {
            Some(unit) => (
                format!(
                    "quiverd moving into {unit} (it ran in {}): nothing needs the daemon",
                    self.lifecycle.unit.as_deref().unwrap_or("another cgroup")
                ),
                format!("moving into {unit}"),
            ),
            None => (
                format!(
                    "quiverd restarting onto {} (was {from}): a new build is installed and nothing needs the daemon",
                    to.as_deref().unwrap_or("a new build")
                ),
                "restarting onto a new build".to_owned(),
            ),
        };
        self.log_activity("system", what, None);
        self.emit(quiver_api::Event::DaemonRestarting { from, to });
        // Events reach connections through their forwarding tasks; let them go first.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let conns: Vec<_> = self.conns.lock().unwrap().values().cloned().collect();
        for (tx, _) in &conns {
            let _ = tx.send(quiver_api::Reply::Closing { reason: reason.clone() });
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        for (_, written) in &conns {
            let _ = tokio::time::timeout_at(deadline, written.notified()).await;
        }
    }
}

/// Look for a staged build and idleness every [`Settings::poll`]; ask the main loop to
/// restart (`restart`) when it is time.
pub(crate) fn start_watch(daemon: &Arc<crate::Daemon>, restart: tokio::sync::mpsc::Sender<()>) {
    let weak = Arc::downgrade(daemon);
    let poll = daemon.lifecycle.settings.poll;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(poll).await;
            let Some(d) = weak.upgrade() else { return };
            let busy = d.busy();
            let (ready, expired) = d.lifecycle.tick(&busy);
            for h in expired {
                d.log_activity(&h.source, format!("hold #{} ({}) ran out", h.id, h.reason), None);
            }
            if ready {
                let _ = restart.try_send(());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn settings(idle: u64) -> Settings {
        Settings { auto: true, idle: Duration::from_secs(idle), settle: Duration::ZERO, poll: Duration::from_millis(10) }
    }

    /// A lifecycle whose "binary" is a temp file; `replace` stages a new build.
    struct Bin {
        dir: tempfile::TempDir,
        lc: Arc<Lifecycle>,
    }

    impl Bin {
        fn new(idle: u64) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join("quiverd");
            std::fs::write(&exe, b"old").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            let running = FileId::of(&exe);
            let manifest = dir.path().join("installed.json");
            let lc = Lifecycle::new(settings(idle), Some(exe), running, manifest);
            Self { dir, lc }
        }

        fn replace(&self) {
            use std::os::unix::fs::PermissionsExt;
            let exe = self.dir.path().join("quiverd");
            let tmp = self.dir.path().join(".quiverd.new");
            std::fs::write(&tmp, b"new").unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &exe).unwrap();
            let manifest = serde_json::json!({"version": "0.2.0.r9.gabc", "commit": "abc", "files": [{"path": exe}]});
            std::fs::write(self.dir.path().join("installed.json"), manifest.to_string()).unwrap();
        }
    }

    #[test]
    fn lab_holders_name_the_instance_and_its_owner() {
        let h = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let word = || Some("lab origins, codex:pharos-xreal".to_owned());
        assert_eq!(
            format!("Assassin's Creed® Origins ({})", holders_in_words(&h(&["lab"]), word)),
            "Assassin's Creed® Origins (lab origins, codex:pharos-xreal)"
        );
        assert_eq!(holders_in_words(&h(&["bridge", "lab"]), word), "bridge, lab origins, codex:pharos-xreal");
        // No instance found running it: just the holder.
        assert_eq!(holders_in_words(&h(&["lab"]), || None), "lab");
        assert_eq!(holders_in_words(&h(&["launcher"]), || panic!("only asked for lab holders")), "launcher");
    }

    #[test]
    fn busy_in_words() {
        let mut st = State::default();
        let idle = Busy::default();
        assert!(reasons(&st, &idle, true, Utc::now()).is_empty());
        let busy = Busy {
            sessions: vec!["AC Origins (bridge)".into()],
            starting: vec!["Far Cry 5".into()],
            jobs: vec!["installing Anno 1800".into()],
            perf_subscriptions: 1,
            huds: 2,
            perf_sessions: vec!["Rayman".into()],
            awaiting_2fa: true,
            streams: vec!["Moonlight stream of X (kept for 8 min (until 14:05), resume from Moonlight (roth))".into()],
            hosted_streams: vec!["Test pattern".into()],
        };
        st.broker_conns = 1;
        let r = reasons(&st, &busy, false, Utc::now());
        assert_eq!(
            r,
            [
                "AC Origins (bridge)",
                "starting Far Cry 5",
                "installing Anno 1800",
                "1 perf subscription",
                "2 HUDs attached",
                "perf session for Rayman",
                "Moonlight stream of X (kept for 8 min (until 14:05), resume from Moonlight (roth))",
                "a sign-in waiting for its 2FA code",
            ]
        );
        // Without a session, the bridge connections say so; requests in flight only count
        // for the restart itself.
        st.inflight.insert("play".into(), 1);
        st.inflight.insert("library".into(), 2);
        assert_eq!(reasons(&st, &idle, false, Utc::now()), ["1 bridge connection"]);
        assert_eq!(reasons(&st, &idle, true, Utc::now()), ["1 bridge connection", "3 requests in flight (library, play)"]);
    }

    #[test]
    fn waits_for_idle_then_closes_and_refuses() {
        let bin = Bin::new(0);
        let lc = &bin.lc;
        // Nothing staged: never ready.
        assert!(!lc.tick(&Busy::default()).0);
        assert!(lc.try_close(Busy::default).is_err());
        bin.replace();
        let busy = Busy { sessions: vec!["AC Origins (bridge)".into()], ..Default::default() };
        assert!(!lc.tick(&busy).0, "a session blocks");
        // A Moonlight stream with no game behind it (the test pattern), or one kept for a
        // resume: its host is in quiverd, so it blocks too.
        let moon = Busy { streams: vec!["Moonlight stream of the test pattern".into()], ..Default::default() };
        assert!(!lc.tick(&moon).0, "a Moonlight stream blocks");
        assert!(lc.try_close(|| moon.clone()).is_err());
        assert_eq!(lc.status(&moon).waiting_on, ["Moonlight stream of the test pattern"]);
        let s = lc.status(&busy);
        assert_eq!(s.update.as_ref().and_then(|u| u.version.as_deref()), Some("0.2.0.r9.gabc"));
        assert_eq!(s.waiting_on, ["AC Origins (bridge)"]);
        assert!(lc.tick(&Busy::default()).0, "idle (idle 0 s, settle 0 s)");

        // A request in flight doesn't stop the idle timer but blocks the close itself.
        let pass = lc.enter(Some("play")).unwrap();
        assert!(lc.tick(&Busy::default()).0);
        assert_eq!(lc.try_close(Busy::default).err().unwrap(), ["1 request in flight (play)"]);
        drop(pass);
        let plan = lc.try_close(Busy::default).unwrap();
        assert_eq!(plan.to.as_deref(), Some("0.2.0.r9.gabc"));
        assert!(lc.closed());
        let err = lc.enter(Some("play")).err().unwrap();
        assert!(err.contains("restarting") && err.contains("'play' did not run"), "{err}");
        // Status and subscriptions still answer.
        assert!(lc.enter(None).is_ok());
        assert!(lc.status(&Busy::default()).restarting);

        // A failed exec takes requests again and doesn't retry that build.
        lc.reopen(&plan, "no such file");
        assert!(!lc.closed());
        assert!(lc.enter(Some("play")).is_ok());
        assert!(!lc.tick(&Busy::default()).0);
        assert!(lc.status(&Busy::default()).update.unwrap().note.unwrap().contains("failed"));
        // A newer build is tried again.
        bin.replace();
        assert!(lc.tick(&Busy::default()).0);
    }

    #[test]
    fn idle_timer() {
        let bin = Bin::new(30);
        bin.replace();
        let t0 = Instant::now();
        assert!(!bin.lc.tick_at(&Busy::default(), t0).0);
        assert!(!bin.lc.tick_at(&Busy::default(), t0 + Duration::from_secs(29)).0);
        assert!(bin.lc.tick_at(&Busy::default(), t0 + Duration::from_secs(30)).0);
        // Busy for a moment restarts the count.
        let busy = Busy { jobs: vec!["verifying X".into()], ..Default::default() };
        assert!(!bin.lc.tick_at(&busy, t0 + Duration::from_secs(31)).0);
        assert!(!bin.lc.tick_at(&Busy::default(), t0 + Duration::from_secs(32)).0);
        assert!(bin.lc.tick_at(&Busy::default(), t0 + Duration::from_secs(62)).0);
        // Off: never.
        let mut off = Bin::new(0);
        Arc::get_mut(&mut off.lc).unwrap().settings.auto = false;
        off.replace();
        assert!(!off.lc.tick(&Busy::default()).0);
    }

    #[test]
    fn holds_by_connection_and_time() {
        let bin = Bin::new(0);
        let lc = &bin.lc;
        bin.replace();
        assert!(lc.hold("", "cli", Some(60), None).is_err());
        assert!(lc.hold("x", "cli", None, None).is_err(), "no connection, no time limit");
        assert!(lc.hold("x", "cli", Some(MAX_HOLD_SECS + 1), None).is_err());
        let timed = lc.hold("measuring", "agent", Some(60), None).unwrap();
        let bound = lc.hold("watching", "cli", None, Some(7)).unwrap();
        assert!(timed.expires.is_some() && bound.expires.is_none());
        let s = lc.status(&Busy::default());
        assert_eq!(s.holds.len(), 2);
        assert!(s.waiting_on[0].starts_with("hold #1: measuring (agent, ") && s.waiting_on[0].ends_with(" left)"), "{:?}", s.waiting_on);
        assert_eq!(s.waiting_on[1], "hold #2: watching (cli, while its client runs)");
        assert!(!lc.tick(&Busy::default()).0);
        assert!(lc.try_close(Busy::default).is_err());

        // The connection's hold goes with it; the timed one when its time is up.
        assert_eq!(lc.disconnect(7), [bound]);
        assert!(lc.disconnect(7).is_empty());
        let (ready, expired) = lc.tick_at(&Busy::default(), Instant::now() + Duration::from_secs(61));
        assert_eq!(expired, [timed]);
        assert!(ready);

        // Release: by id, the only one, or all.
        let a = lc.hold("a", "cli", Some(60), None).unwrap();
        let b = lc.hold("b", "cli", Some(60), None).unwrap();
        assert!(lc.release(None, false).unwrap_err().to_string().contains("there are 2 holds"));
        assert!(lc.release(Some(99), false).is_err());
        assert_eq!(lc.release(Some(a.id), false).unwrap(), [a]);
        assert_eq!(lc.release(None, false).unwrap(), [b]);
        assert!(lc.release(None, false).unwrap().is_empty());
        lc.hold("c", "cli", Some(60), None).unwrap();
        lc.hold("d", "cli", Some(60), None).unwrap();
        assert_eq!(lc.release(None, true).unwrap().len(), 2);
    }

    /// Moving into the unit: wanted without a staged build, waits for idle *and* for
    /// streams (an exec doesn't), commits like a restart, and isn't retried once it failed.
    #[test]
    fn moving_into_the_unit() {
        let bin = Bin::new(0);
        let lc = &bin.lc;
        let streaming = Busy { hosted_streams: vec!["Test pattern".into()], ..Default::default() };
        // A stream doesn't hold up an exec onto a new build.
        bin.replace();
        assert!(lc.tick(&streaming).0);
        let plan = lc.try_close(|| streaming.clone()).unwrap();
        assert!(plan.relocate.is_none());
        lc.reopen(&plan, "test");

        let bin = Bin::new(0);
        let lc = &bin.lc;
        assert!(!lc.tick(&Busy::default()).0, "nothing staged, nothing wanted");
        lc.want_relocation("quiverd.service".into());
        assert_eq!(lc.status(&Busy::default()).relocation.as_deref(), Some("moving into quiverd.service once nothing needs it"));
        // A game or a stream holds it up.
        assert!(!lc.tick(&Busy { sessions: vec!["AC Origins (bridge)".into()], ..Default::default() }).0);
        assert!(!lc.tick(&streaming).0);
        assert_eq!(lc.status(&streaming).waiting_on, ["stream: Test pattern"]);
        assert_eq!(lc.try_close(|| streaming.clone()).err().unwrap(), ["stream: Test pattern"]);
        assert!(lc.tick(&Busy::default()).0);
        let pass = lc.enter(Some("play")).unwrap();
        assert!(lc.try_close(Busy::default).is_err(), "requests in flight are waited out");
        drop(pass);
        let plan = lc.try_close(Busy::default).unwrap();
        assert_eq!(plan.relocate.as_deref(), Some("quiverd.service"));
        assert!(lc.closed() && lc.enter(Some("play")).is_err());

        // It failed: requests run again, and it isn't tried again (nor re-wanted).
        lc.reopen(&plan, "the unit didn't come");
        assert!(!lc.closed() && lc.enter(Some("play")).is_ok());
        assert!(!lc.tick(&Busy::default()).0);
        lc.want_relocation("quiverd.service".into());
        assert!(!lc.tick(&Busy::default()).0);
        assert!(lc.status(&Busy::default()).relocation.unwrap().contains("failed: the unit didn't come"));
        // A new build is still restarted onto (by exec).
        bin.replace();
        assert!(lc.tick(&Busy::default()).0);
        assert!(lc.try_close(Busy::default).unwrap().relocate.is_none());
    }

    #[test]
    fn broker_connections_block() {
        let bin = Bin::new(0);
        bin.replace();
        let conn = bin.lc.broker_conn();
        assert!(!bin.lc.tick(&Busy::default()).0);
        assert_eq!(bin.lc.try_close(Busy::default).err().unwrap(), ["1 bridge connection"]);
        drop(conn);
        assert!(bin.lc.try_close(Busy::default).is_ok());
    }

    /// The race: launches arriving while the daemon decides to restart. Every launch that
    /// got in is seen by the close (it fails), and none gets in after it succeeds.
    #[test]
    fn launch_racing_the_restart() {
        let (mut closes, mut launches_first) = (0, 0);
        for i in 0..200 {
            let bin = Bin::new(0);
            bin.replace();
            let lc = bin.lc.clone();
            // Games started: a launch counts as a session once its request returns.
            let games = Arc::new(AtomicUsize::new(0));
            let go = Arc::new(std::sync::Barrier::new(2));
            let launcher = {
                let (lc, games, go) = (lc.clone(), games.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    // Start at different moments relative to the restart's attempts.
                    for _ in 0..(i * 37) % 4000 {
                        std::hint::spin_loop();
                    }
                    let mut started = 0;
                    for _ in 0..50 {
                        if let Ok(_pass) = lc.enter(Some("play")) {
                            games.fetch_add(1, Ordering::SeqCst);
                            started += 1;
                        }
                        std::thread::yield_now();
                    }
                    started
                })
            };
            let mut closed_with = None;
            go.wait();
            for _ in 0..200 {
                let g = games.clone();
                let busy = move || Busy { sessions: (0..g.load(Ordering::SeqCst)).map(|i| format!("game {i}")).collect(), ..Default::default() };
                if lc.try_close(busy).is_ok() {
                    closed_with = Some(games.load(Ordering::SeqCst));
                    break;
                }
                std::thread::yield_now();
            }
            let started = launcher.join().unwrap();
            if let Some(n) = closed_with {
                // Closing is only possible with no game started, and then none ever is.
                assert_eq!(n, 0);
                assert_eq!(started, 0, "a launch got in after the restart was decided");
                closes += 1;
            } else {
                assert!(started > 0);
                launches_first += 1;
            }
            assert_eq!(games.load(Ordering::SeqCst), started);
        }
        eprintln!("restart first {closes}×, launch first {launches_first}×");
        assert!(closes > 0, "the restart never won the race");
    }
}
