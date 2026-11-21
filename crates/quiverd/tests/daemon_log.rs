//! quiverd writes `<data>/quiverd.log` itself, however it was started, and each line once.
//! Scratch daemons only (own HOME, XDG folders, socket, perf shm root, broker port). The
//! scope and unit paths are in `service_live.rs` (they talk to the user manager).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quiver_api::service::{Autostart, Systemd};
use quiver_api::{Client, Request};
use serde_json::json;

pub struct Scratch {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub port: u16,
}

impl Scratch {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let s = Self { _tmp: tmp, root, port };
        let data = s.dir("data/quiver");
        write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
        let cfg = json!({
            "library": s.dir("lib"), "prefixes": s.dir("pfx"), "proton": s.dir("proton"), "sdk_source": s.dir("sdk"),
            "save_backups": s.dir("backups"), "report_playtime": false, "steam_offers": false
        });
        write(&data.join("config.json"), &cfg.to_string());
        s
    }

    pub fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    pub fn sock(&self) -> PathBuf {
        self.dir("run/quiver/quiverd.sock")
    }

    pub fn log(&self) -> PathBuf {
        self.dir("data/quiver/quiverd.log")
    }

    pub fn vars(&self) -> Vec<(String, String)> {
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
            ("RUST_LOG", "quiverd=info".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    pub fn wait_up(&self) {
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.sock()).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start; its log:\n{}", std::fs::read_to_string(self.log()).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `Request::Shutdown`, then wait for the socket to go.
    pub async fn stop(&self) {
        let c = Client::connect_running_at(self.sock(), "test").await.unwrap();
        let _ = c.call::<serde_json::Value>(Request::Shutdown).await;
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.sock()).is_ok() {
            assert!(t.elapsed() < Duration::from_secs(10), "quiverd didn't stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The log's lines containing `needle`.
    pub fn count(&self, needle: &str) -> usize {
        std::fs::read_to_string(self.log()).unwrap_or_default().lines().filter(|l| l.contains(needle)).count()
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// A plain child (no systemd tools on the PATH the client looks on): nobody redirects its
/// output, and it still writes the log, each line once.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_child_writes_the_log() {
    let s = Scratch::new();
    let none = tempfile::tempdir().unwrap();
    let how = quiver_api::service::start_with(&Systemd::with_path(none.path()), false, Path::new(env!("CARGO_BIN_EXE_quiverd")), Some(&s.vars())).unwrap();
    assert_eq!(how, Autostart::Spawn);
    s.wait_up();
    assert_eq!(s.count(" listening on "), 1, "{}", std::fs::read_to_string(s.log()).unwrap_or_default());
    let text = std::fs::read_to_string(s.log()).unwrap();
    assert!(!text.contains('\u{1b}'), "no colour codes in the file");
    s.stop().await;
    assert_eq!(s.count("shutdown requested by test"), 1);
}

/// A client from before redirected the daemon's stdout and stderr into the log: the lines
/// are still there once, not twice.
#[tokio::test(flavor = "multi_thread")]
async fn an_old_clients_redirect_is_not_doubled() {
    let s = Scratch::new();
    std::fs::create_dir_all(s.log().parent().unwrap()).unwrap();
    let out = std::fs::OpenOptions::new().create(true).append(true).open(s.log()).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_quiverd"))
        .env_clear()
        .envs(s.vars())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    s.wait_up();
    assert_eq!(s.count(" listening on "), 1, "{}", std::fs::read_to_string(s.log()).unwrap_or_default());
    s.stop().await;
    let _ = child.wait();
}

/// A second daemon for the same socket says so in the log (it exits 75).
#[tokio::test(flavor = "multi_thread")]
async fn already_running_is_logged() {
    let s = Scratch::new();
    let none = tempfile::tempdir().unwrap();
    quiver_api::service::start_with(&Systemd::with_path(none.path()), false, Path::new(env!("CARGO_BIN_EXE_quiverd")), Some(&s.vars())).unwrap();
    s.wait_up();
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quiverd")).env_clear().envs(s.vars()).stdout(std::process::Stdio::null()).status().unwrap();
    assert_eq!(st.code(), Some(quiver_api::service::EXIT_ALREADY_RUNNING));
    assert_eq!(s.count("quiverd is already running"), 1);
    s.stop().await;
}
