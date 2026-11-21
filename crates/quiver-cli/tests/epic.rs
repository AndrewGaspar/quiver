//! `quiver epic …`, `quiver stores` and `quiver library --store epic` against a scratch
//! quiverd (its own XDG dirs, HOME and shm root, no session bus so no keyring). Nothing here
//! reaches Epic: no account exists, and every sign-in attempt is refused (agent source) or
//! rejected (no code) before quiverd would call Epic.
//!
//! Needs `quiverd` built next to `quiver` (`cargo test --workspace` builds both); skipped
//! otherwise.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

    fn isolate(&self, c: &mut Command) {
        c.env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .env("HOME", self.dir("home"))
            .env("QUIVER_BROKER", "127.0.0.1:0")
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("QUIVER_SIGHT_SPAWN", "0")
            .env("RUST_LOG", "warn")
            .env_remove("QUIVER_SOURCE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("QUIVER_TEST_SESSION")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            // Never Quiver's sign-in window on the person's desktop (it would open Epic's
            // real page): no helper and no display, so `epic login` falls back to pasting.
            .env("QUIVER_WEBVIEW", self.dir("no-webview"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("PATH", format!("{}:{}", self.dir("bin").display(), std::env::var("PATH").unwrap_or_default()));
    }

    fn daemon(&self, quiverd: &Path) -> Daemon {
        let mut c = Command::new(quiverd);
        self.isolate(&mut c);
        c.stdout(Stdio::null()).stderr(std::fs::File::create(self.dir("quiverd.log")).unwrap());
        let child = Daemon(c.spawn().unwrap());
        let sock = self.dir("run").join("quiver/quiverd.sock");
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }

    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiver"));
        self.isolate(&mut c);
        let mut child = c.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args, "");
        assert!(out.status.success(), "quiver {args:?} failed: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str], stdin: &str) -> String {
        let out = self.run(args, stdin);
        assert!(!out.status.success(), "quiver {args:?} should fail: {}", String::from_utf8_lossy(&out.stdout));
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

#[test]
fn epic_accounts_and_library_without_signing_in() {
    let quiverd = Path::new(env!("CARGO_BIN_EXE_quiver")).with_file_name("quiverd");
    if !quiverd.is_file() {
        eprintln!("skipped: {} isn't built (cargo test --workspace builds it)", quiverd.display());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let e = Env { root: tmp.path().to_owned(), _tmp: tmp };
    write(&e.dir("data/quiver/accounts.json"), r#"{"accounts":[]}"#);
    let cfg = serde_json::json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("GE-Proton11-12"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false, "env": []
    });
    write(&e.dir("data/quiver/config.json"), &cfg.to_string());
    std::fs::create_dir_all(e.dir("shm")).unwrap();
    std::fs::create_dir_all(e.dir("home")).unwrap();
    write(&e.dir("bin/systemctl"), "#!/bin/sh\nexit 1\n");
    std::fs::set_permissions(e.dir("bin/systemctl"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let _d = e.daemon(&quiverd);

    // Epic is a store, not signed in.
    let stores = e.ok(&["stores"]);
    let epic = stores.lines().find(|l| l.starts_with("Epic")).unwrap_or_else(|| panic!("no Epic line: {stores}"));
    assert!(epic.contains("not signed in") && epic.contains("quiver epic login"), "{epic}");
    assert_eq!(e.ok(&["epic", "accounts"]).trim(), "no Epic accounts (quiver epic login)");
    assert_eq!(e.ok(&["epic", "accounts", "--json"]).trim(), "[]");
    // No store has games: the library answers with Ubisoft's "not signed in", as before.
    assert!(e.fails(&["library", "--store", "epic"], "").contains("not signed in"));

    // `login` prints Epic's page and waits for the code; nothing pasted is an error, and
    // nothing was sent anywhere.
    let out = e.fails(&["epic", "login"], "");
    assert!(out.contains("https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode"), "{out}");
    assert!(out.contains("nothing was pasted"), "{out}");
    // Something that isn't a code is turned down by quiverd before it asks Epic.
    let out = e.fails(&["epic", "login"], "my password\n");
    assert!(out.contains("doesn't look like Epic's code"), "{out}");
    // Agents can't sign in or out, whatever they send.
    let out = e.fails(&["--source", "agent", "epic", "login", "--code", "0a1b2c3d4e5f60718293a4b5c6d7e8f9"], "");
    assert!(out.contains("only you can sign in to Epic"), "{out}");
    let out = e.fails(&["--source", "agent", "epic", "logout"], "");
    assert!(out.contains("only you can sign out of Epic"), "{out}");
    let out = e.fails(&["epic", "logout"], "");
    assert!(out.contains("no Epic account is signed in"), "{out}");
    // No Epic data was written: nothing signed in, nothing fetched.
    assert!(!e.dir("data/quiver/epic/accounts.json").exists());
    assert!(!e.dir("data/quiver/epic/cache").exists());
    let log = std::fs::read_to_string(e.dir("quiverd.log")).unwrap_or_default();
    assert!(!log.contains("0a1b2c3d4e5f60718293a4b5c6d7e8f9"), "the code is never logged: {log}");
}
