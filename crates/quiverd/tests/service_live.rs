//! quiverd under systemd, live, against the user manager but never the person's quiverd:
//! a scratch unit `quiver-test-<pid>.service` (a runtime unit file in
//! `/run/user/<uid>/systemd/user`, removed after), a scratch daemon with its own HOME, XDG
//! folders, socket, perf shm root and broker port, and nothing enabled. Lingering is never
//! looked at.
//!
//! Ignored by default (it talks to the user manager): `cargo test -p quiverd --test
//! service_live -- --ignored`.
//!
//! - A daemon started outside the unit moves into it once idle, handing over its sockets
//!   (the client keeps working, the old process exits 0), and the unit's process is the
//!   unit's main pid, active, so `READY=1` arrived.
//! - A restart onto a new build under the unit keeps that pid and the unit active.
//! - Another daemon on the same socket exits 75.
//! - A launch's scope lands in `quiver.slice`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quiver_api::daemon::{DaemonRequest, DaemonStatus};
use quiver_api::{Client, Request};
use serde_json::json;

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    unit: String,
    unit_file: PathBuf,
    port: u16,
    legacy: Option<Child>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("systemctl").args(["--user", "stop", &self.unit]).stderr(std::process::Stdio::null()).status();
        let _ = Command::new("systemctl").args(["--user", "reset-failed", &self.unit]).stderr(std::process::Stdio::null()).status();
        if let Some(c) = self.legacy.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.unit_file);
    }
}

impl Env {
    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn sock(&self) -> PathBuf {
        self.dir("run/quiver/quiverd.sock")
    }

    fn log(&self) -> String {
        format!(
            "--- legacy\n{}\n--- unit\n{}",
            std::fs::read_to_string(self.dir("legacy.log")).unwrap_or_default(),
            std::fs::read_to_string(self.dir("unit.log")).unwrap_or_default()
        )
    }

    fn vars(&self) -> Vec<(String, String)> {
        let d = |n: &str| self.dir(n).display().to_string();
        [
            ("PATH", "/usr/bin:/bin".to_owned()),
            ("HOME", d("home")),
            ("XDG_DATA_HOME", d("data")),
            ("XDG_CONFIG_HOME", d("config")),
            ("XDG_STATE_HOME", d("state")),
            ("XDG_CACHE_HOME", d("cache")),
            ("XDG_RUNTIME_DIR", d("run")),
            ("QUIVER_SHM_ROOT", d("shm")),
            ("QUIVER_BROKER", format!("127.0.0.1:{}", self.port)),
            ("QUIVER_RESTART_IDLE_SECS", "1".into()),
            ("QUIVER_RESTART_SETTLE_SECS", "0".into()),
            ("QUIVER_RESTART_POLL_MS", "100".into()),
            ("QUIVER_TEST_UNIT", self.unit.clone()),
            ("RUST_LOG", "quiverd=info".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    fn install_build(&self, version: &str) {
        let exe = self.dir("bin/quiverd");
        let tmp = self.dir("bin/.quiverd.new");
        std::fs::create_dir_all(self.dir("bin")).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_quiverd"), &tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, &exe).unwrap();
        let manifest = json!({ "version": version, "commit": "0123456789abcdef", "files": [{ "path": exe, "sha256": "-", "mode": "0o755" }] });
        write(&self.dir("home/.local/state/quiver/installed.json"), &manifest.to_string());
    }

    /// The scratch unit: the shipped unit's settings, the scratch environment.
    fn write_unit(&self) {
        let shipped = include_str!("../../../contrib/quiverd.service");
        let mut service: Vec<String> = shipped
            .lines()
            .skip_while(|l| *l != "[Service]")
            .take_while(|l| *l != "[Install]")
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| if l.starts_with("ExecStart=") { format!("ExecStart={}", self.dir("bin/quiverd").display()) } else { l.to_owned() })
            .collect();
        service.extend(self.vars().iter().map(|(k, v)| format!("Environment=\"{k}={v}\"")));
        service.push(format!("StandardOutput=append:{}", self.dir("unit.log").display()));
        service.push(format!("StandardError=append:{}", self.dir("unit.log").display()));
        let text = format!("[Unit]\nDescription=quiverd live test (scratch)\n\n{}\n", service.join("\n"));
        write(&self.unit_file, &text);
    }

    fn legacy(&mut self) -> u32 {
        let mut c = Command::new(self.dir("bin/quiverd"));
        c.env_clear().envs(self.vars()).stdout(std::process::Stdio::null()).stderr(std::fs::File::create(self.dir("legacy.log")).unwrap());
        let child = c.spawn().unwrap();
        let pid = child.id();
        self.legacy = Some(child);
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.sock()).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        pid
    }

    fn show(&self) -> (u32, String) {
        let out = Command::new("systemctl").args(["--user", "show", "-p", "MainPID", "-p", "ActiveState", &self.unit]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).unwrap_or_default().to_owned();
        (get("MainPID=").parse().unwrap_or(0), get("ActiveState="))
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    let unit = format!("quiver-test-{}.service", std::process::id());
    let unit_file = PathBuf::from(format!("/run/user/{}/systemd/user", uid())).join(&unit);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let e = Env { _tmp: tmp, root, unit, unit_file, port, legacy: None };
    let data = e.dir("data/quiver");
    write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
    let cfg = json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("proton"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false
    });
    write(&data.join("config.json"), &cfg.to_string());
    e
}

async fn status(c: &Client) -> DaemonStatus {
    c.call(Request::Daemon(DaemonRequest::Status)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "talks to the systemd user manager (scratch unit only); run with --ignored"]
async fn moves_into_the_unit_and_restarts_there() {
    let mut e = setup();
    e.install_build("1.0.0-legacy");
    e.write_unit();
    let legacy = e.legacy();

    let cli = Client::connect_running_at(e.sock(), "cli").await.unwrap();
    let s = status(&cli).await;
    assert_eq!(s.pid, legacy);
    assert!(!s.in_unit, "{s:?}");

    // Idle for a second: it hands over and exits 0.
    let t = Instant::now();
    let code = loop {
        if let Some(st) = e.legacy.as_mut().unwrap().try_wait().unwrap() {
            break st.code();
        }
        assert!(t.elapsed() < Duration::from_secs(30), "the legacy daemon didn't move: {}", e.log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(code, Some(0), "{}", e.log());

    // The same client reaches the unit's daemon, on the same socket.
    let s = status(&cli).await;
    assert_ne!(s.pid, legacy);
    assert!(s.in_unit && s.unit.as_deref() == Some(e.unit.as_str()), "{s:?}");
    assert!(s.restarted_from.is_some(), "the build it took over from: {s:?}");
    let cg = std::fs::read_to_string(format!("/proc/{}/cgroup", s.pid)).unwrap();
    assert!(cg.trim_end().ends_with(&format!("/quiver.slice/{}", e.unit)), "{cg}");
    let (main, active) = e.show();
    assert_eq!((main, active.as_str()), (s.pid, "active"), "READY=1 reached systemd: {}", e.log());
    let unit_pid = s.pid;

    // Another daemon for the same socket: exits 75 at once.
    let st = Command::new(e.dir("bin/quiverd")).env_clear().envs(e.vars()).stderr(std::process::Stdio::null()).status().unwrap();
    assert_eq!(st.code(), Some(quiver_api::service::EXIT_ALREADY_RUNNING));

    // A new build: it restarts onto it in place, still the unit's main process.
    e.install_build("2.0.0-new");
    let t = Instant::now();
    let s = loop {
        let s = status(&cli).await;
        if s.restarts == 1 {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "no restart under the unit: {s:?}\n{}", e.log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(s.pid, unit_pid);
    assert!(s.in_unit);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(e.show(), (unit_pid, "active".to_owned()), "{}", e.log());

    // quiverd.log has every process's lines, each once: the legacy daemon's, the unit's
    // (whose stdout, the journal's stand-in here, has them too), and the new build's.
    let log = std::fs::read_to_string(e.dir("data/quiver/quiverd.log")).unwrap_or_default();
    let count = |n: &str| log.lines().filter(|l| l.contains(n)).count();
    assert_eq!(count(" listening on "), 3, "legacy, the unit, the restart:\n{log}");
    assert_eq!(count("took over the sockets of quiverd"), 1, "{log}");
    assert_eq!(count("restarted onto this build"), 1, "{log}");
    assert_eq!(count("quiverd is already running"), 1, "{log}");
    let unit_out = std::fs::read_to_string(e.dir("unit.log")).unwrap_or_default();
    assert!(unit_out.contains("took over the sockets of quiverd"), "the unit's stdout still carries the lines: {unit_out}");
}

/// A scratch daemon started through the scope path (`systemd-run --user --scope`, as a
/// client starts anyone's but the person's own daemon) writes quiverd.log, and runs in
/// quiver.slice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "talks to the systemd user manager (a transient scope); run with --ignored"]
async fn scope_path_writes_the_log() {
    let e = setup();
    e.install_build("1.0.0");
    let vars: Vec<(String, String)> = e.vars().into_iter().filter(|(k, _)| k != "QUIVER_TEST_UNIT").collect();
    let sd = quiver_api::service::Systemd::from_env();
    if !sd.can_scope() {
        eprintln!("no systemd-run scope here; skipped");
        return;
    }
    let how = quiver_api::service::start_with(&sd, false, &e.dir("bin/quiverd"), Some(&vars)).unwrap();
    assert_eq!(how, quiver_api::service::Autostart::Scope);
    let t = Instant::now();
    while std::os::unix::net::UnixStream::connect(e.sock()).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start in its scope");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cli = Client::connect_running_at(e.sock(), "test").await.unwrap();
    let s = status(&cli).await;
    let cg = std::fs::read_to_string(format!("/proc/{}/cgroup", s.pid)).unwrap();
    assert!(cg.contains("/quiver.slice/run-"), "in a scope under quiver.slice: {cg}");
    let _ = cli.call::<serde_json::Value>(Request::Shutdown).await;
    let t = Instant::now();
    while Path::new(&format!("/proc/{}", s.pid)).exists() {
        assert!(t.elapsed() < Duration::from_secs(10), "quiverd didn't stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let log = std::fs::read_to_string(e.dir("data/quiver/quiverd.log")).unwrap_or_default();
    assert_eq!(log.lines().filter(|l| l.contains(" listening on ")).count(), 1, "{log}");
    assert!(log.contains("shutdown requested by test"), "{log}");
}

/// A launch's process in its own scope under quiver.slice (only where this test runs
/// with the real runtime folder, as the person's quiverd does).
#[test]
#[ignore = "talks to the systemd user manager (a transient scope); run with --ignored"]
fn launch_scope_lands_in_quiver_slice() {
    if !quiver_core::scope::available() {
        eprintln!("no scopes here (runtime folder or systemd-run); skipped");
        return;
    }
    let mut argv = quiver_core::scope::wrap("game", "test:live", Vec::new());
    assert!(argv[0].ends_with("systemd-run"), "{argv:?}");
    argv.extend(["sh".into(), "-c".into(), "cat /proc/self/cgroup".into()]);
    let out = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
    let cg = String::from_utf8_lossy(&out.stdout);
    assert!(cg.contains("/quiver.slice/quiver-game-test_live-"), "{cg}");
}
