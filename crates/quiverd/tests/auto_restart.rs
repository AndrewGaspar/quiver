//! quiverd restarting itself onto a new build, against a scratch daemon: its own binary in
//! a temp bin dir, its own HOME, XDG dirs, socket, perf shm root and broker port, and an
//! empty environment otherwise (no keyring, no display). Nothing here touches a real
//! install or a running quiverd.
//!
//! The build is "replaced" the way `tools/install-user.sh` does it: a copy renamed over the
//! binary (a new inode), with an install manifest naming its version.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quiver_api::daemon::{DaemonRequest, DaemonStatus, Hold};
use quiver_api::{ActivityEntry, Client, Event, Request};
use serde_json::{Value, json};

/// The scratch daemon, killed when dropped (also when the test fails).
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn exe(&self) -> PathBuf {
        self.dir("bin/quiverd")
    }

    fn sock(&self) -> PathBuf {
        self.dir("run/quiver/quiverd.sock")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default()
    }

    /// Put a build at `bin/quiverd` as an install does: copied beside it, renamed into place.
    fn install_build(&self, version: &str) {
        let tmp = self.dir("bin/.quiverd.quiver-new");
        std::fs::create_dir_all(self.dir("bin")).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_quiverd"), &tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, self.exe()).unwrap();
        let manifest = json!({ "version": version, "commit": "0123456789abcdef", "files": [{ "path": self.exe(), "sha256": "-", "mode": "0o755" }] });
        write(&self.dir("home/.local/state/quiver/installed.json"), &manifest.to_string());
    }

    fn daemon(&self, broker_port: u16) -> Daemon {
        let child = Command::new(self.exe())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir("home"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("QUIVER_BROKER", format!("127.0.0.1:{broker_port}"))
            .env("QUIVER_RESTART_IDLE_SECS", "1")
            .env("QUIVER_RESTART_SETTLE_SECS", "0")
            .env("QUIVER_RESTART_POLL_MS", "100")
            .env("RUST_LOG", "quiverd=info")
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(self.dir("quiverd.log")).unwrap())
            .spawn()
            .unwrap();
        let d = Daemon(child);
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.sock()).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    let e = Env { _tmp: tmp, root };
    let data = e.dir("data/quiver");
    // No accounts (the keyring is never asked), no Steam offers, no games.
    write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
    let cfg = json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("proton"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false
    });
    write(&data.join("config.json"), &cfg.to_string());
    e
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn status(c: &Client) -> DaemonStatus {
    c.call(Request::Daemon(DaemonRequest::Status)).await.unwrap()
}

/// Poll the daemon's status until `f` holds.
async fn wait_for(e: &Env, c: &Client, what: &str, f: impl Fn(&DaemonStatus) -> bool) -> DaemonStatus {
    let t = Instant::now();
    loop {
        let s = status(c).await;
        if f(&s) {
            return s;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; last status {s:?}\n{}", e.log());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next event matching `f`, skipping others.
async fn event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>, what: &str, f: impl Fn(&Event) -> bool) -> Event {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(e)) if f(&e) => return e,
            Ok(Some(_)) => {}
            Ok(None) => panic!("event stream ended waiting for {what}"),
            Err(_) => panic!("no {what} event"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restarts_onto_a_new_build_once_idle() {
    let e = setup();
    e.install_build("1.0.0-old");
    let port = free_port();
    let d = e.daemon(port);
    let pid = d.0.id();

    // An app window: event subscriptions don't keep the daemon from restarting.
    let app = Client::connect_running_at(e.sock(), "gui").await.unwrap();
    let mut events = app.subscribe().await.unwrap();
    let cli = Client::connect_running_at(e.sock(), "cli").await.unwrap();
    let s = status(&cli).await;
    assert_eq!((s.pid, s.restarts, s.update.is_none()), (pid, 0, true), "{s:?}");
    assert!(s.waiting_on.is_empty(), "an event subscriber isn't busy: {:?}", s.waiting_on);
    assert_eq!(s.exe.as_deref(), Some(e.exe().to_str().unwrap()));

    // Busy: a hold for as long as its client's connection, and a bridge on the broker.
    let holder = Client::connect_running_at(e.sock(), "agent").await.unwrap();
    let hold: Hold = holder.call(Request::Daemon(DaemonRequest::Hold { reason: "measuring".into(), for_secs: None, owner: None })).await.unwrap();
    assert!(hold.expires.is_none());
    let bridge = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

    // A new build lands. It is noticed, but the daemon waits.
    e.install_build("2.0.0-new");
    let s = wait_for(&e, &cli, "the staged update", |s| s.update.is_some()).await;
    assert_eq!(s.update.as_ref().unwrap().version.as_deref(), Some("2.0.0-new"));
    tokio::time::sleep(Duration::from_millis(1500)).await; // past the idle time
    let s = status(&cli).await;
    assert_eq!(s.restarts, 0);
    assert_eq!(s.waiting_on.len(), 2, "{:?}", s.waiting_on);
    assert_eq!(s.waiting_on[0], "1 bridge connection");
    assert!(s.waiting_on[1].starts_with("hold #1: measuring (agent"), "{:?}", s.waiting_on);

    // The bridge leaves; the hold still holds.
    drop(bridge);
    let s = wait_for(&e, &cli, "the bridge to go", |s| s.waiting_on.len() == 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s2 = status(&cli).await;
    assert_eq!((s2.restarts, s2.waiting_on.clone()), (0, s.waiting_on.clone()));

    // The holder exits while another client keeps calling: every call succeeds, across
    // the restart (refused or unanswered ones are sent again to the new build).
    let caller = {
        let cli = cli.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            let mut calls = 0;
            loop {
                let _: Value = cli.call(Request::Queue).await.expect("a call during the restart failed");
                calls += 1;
                let s = status(&cli).await;
                if s.restarts == 1 {
                    return (calls, s);
                }
                assert!(t.elapsed() < Duration::from_secs(20), "no restart");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    drop(holder);
    let (calls, s) = caller.await.unwrap();
    assert!(calls > 1);
    assert_eq!(s.pid, pid, "the new build runs in the same process");
    assert_eq!(s.restarted_from.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert!(s.update.is_none(), "the new build is the one on disk: {s:?}");
    assert!(s.holds.is_empty() && s.waiting_on.is_empty(), "{s:?}");
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    assert_eq!(exe, e.exe(), "running the replaced file, not the deleted one");

    // The app heard about it, lost the connection and got it back.
    match event(&mut events, "DaemonRestarting", |e| matches!(e, Event::DaemonRestarting { .. })).await {
        Event::DaemonRestarting { to, .. } => assert_eq!(to.as_deref(), Some("2.0.0-new")),
        _ => unreachable!(),
    }
    event(&mut events, "Disconnected", |e| matches!(e, Event::Disconnected)).await;
    event(&mut events, "Reconnected", |e| matches!(e, Event::Reconnected)).await;

    // One activity line for the restart, kept across it.
    let activity: Vec<ActivityEntry> = cli.call(Request::Activity { limit: 50 }).await.unwrap();
    let lines: Vec<&str> = activity.iter().map(|a| a.action.as_str()).filter(|a| a.contains("restarting onto")).collect();
    assert_eq!(lines.len(), 1, "{activity:?}");
    assert!(lines[0].starts_with("quiverd restarting onto 2.0.0-new"), "{lines:?}");

    // The broker kept its port.
    let st: quiver_api::Status = cli.call(Request::Status).await.unwrap();
    assert_eq!(st.broker, format!("127.0.0.1:{port}"));
    std::net::TcpStream::connect(("127.0.0.1", port)).expect("the broker listens on the same port");

    // A timed hold blocks the next build too, until released.
    let h: Hold = cli.call(Request::Daemon(DaemonRequest::Hold { reason: "bench".into(), for_secs: Some(600), owner: None })).await.unwrap();
    e.install_build("3.0.0-newer");
    wait_for(&e, &cli, "the next staged update", |s| s.update.is_some()).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(status(&cli).await.restarts, 1);
    let released: Vec<Hold> = cli.call(Request::Daemon(DaemonRequest::Release { id: Some(h.id), all: false })).await.unwrap();
    assert_eq!(released, [h]);
    let s = wait_for(&e, &cli, "the second restart", |s| s.restarts == 2).await;
    assert_eq!(s.pid, pid);

    // `quiver daemon stop` still stops it.
    let _ = cli.call::<Value>(Request::Shutdown).await;
    let mut d = d;
    let t = Instant::now();
    while d.0.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < Duration::from_secs(10), "quiverd didn't stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}
