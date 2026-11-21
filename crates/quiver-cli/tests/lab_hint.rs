//! The "please wrap up" hint (design/LAB.md §19), end to end: a scratch quiverd (its own
//! binary in a temp bin dir, HOME, XDG dirs, socket, perf shm root, broker port and lab
//! folder), a fake lab bridge connected to its broker under the session `lab-hinttest-r7`,
//! and a new build "installed" over it. `quiver` runs as different owners (handed over in
//! `QUIVER_LAB_OWNER_DETECTED`, as a `--bg` job's is) and must print the hint on stderr only,
//! to the run's owner only, at most once per five minutes; closing the bridge (what `quiver
//! lab kill` does) lets quiverd restart. A fake `podman` answers that nothing runs. Nothing
//! here touches a real lab instance, the person's quiverd or their podman.
//!
//! Needs `quiverd` built next to `quiver` (`cargo build -p quiverd`; `cargo test --workspace`
//! builds both); skipped otherwise.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const T1: &str = "01a1ff7d-5b9f-7dc2-a632-14a8fb0160c9";
const T2: &str = "01a1ff7f-dfa1-7102-8f8b-898e203f1a37";
const INSTANCE: &str = "hinttest";

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
    port: u16,
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn script(p: &Path, s: &str) {
    write(p, s);
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A Codex thread's owner, as `quiver lab whoami --json` would print it.
fn codex(thread: &str) -> String {
    json!({"agent": "codex", "session_id": thread, "at": "2026-11-21T19:00:00Z"}).to_string()
}

impl Env {
    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn exe(&self) -> PathBuf {
        self.dir("bin/quiverd")
    }

    fn isolate(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir("fake").display()))
            .env("HOME", self.dir("home"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("QUIVER_LAB_ROOT", self.dir("lab"))
            .env("QUIVER_BROKER", format!("127.0.0.1:{}", self.port))
            .env("QUIVER_SIGHT_SPAWN", "0")
            .env("RUST_LOG", "warn");
    }

    /// Put a build at `bin/quiverd` as an install does: copied beside it, renamed into place.
    fn install_build(&self, from: &Path, version: &str) {
        let tmp = self.dir("bin/.quiverd.new");
        std::fs::create_dir_all(self.dir("bin")).unwrap();
        std::fs::copy(from, &tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, self.exe()).unwrap();
        let manifest = json!({ "version": version, "files": [{ "path": self.exe() }] });
        write(&self.dir("home/.local/state/quiver/installed.json"), &manifest.to_string());
    }

    fn daemon(&self) -> Daemon {
        let mut c = Command::new(self.exe());
        self.isolate(&mut c);
        c.env("QUIVER_RESTART_IDLE_SECS", "1").env("QUIVER_RESTART_SETTLE_SECS", "0").env("QUIVER_RESTART_POLL_MS", "100");
        c.stdout(std::process::Stdio::null()).stderr(std::fs::File::create(self.dir("quiverd.log")).unwrap());
        let d = Daemon(c.spawn().unwrap());
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.dir("run/quiver/quiverd.sock")).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default()
    }

    /// `quiver ARGS` as `owner` (none: no handed-over owner), with extra environment.
    fn quiver(&self, owner: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiver"));
        self.isolate(&mut c);
        if let Some(o) = owner {
            c.env("QUIVER_LAB_OWNER_DETECTED", o);
        }
        c.envs(env.iter().copied());
        let out = c.args(args).output().unwrap();
        assert!(out.status.success(), "quiver {args:?} failed: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        out
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&self.quiver(None, &[], &["daemon", "status", "--json"]).stdout).unwrap()
    }

    fn wait_status(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let t = Instant::now();
        loop {
            let s = self.status();
            if f(&s) {
                return s;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; last {s}\n{}", self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn forget_cache(&self) {
        let p = self.dir("run/quiver/hints.json");
        if let Ok(t) = std::fs::read_to_string(&p) {
            let mut v: Value = serde_json::from_str(&t).unwrap();
            v.as_object_mut().unwrap().remove("cached");
            std::fs::write(&p, v.to_string()).unwrap();
        }
    }
}

fn setup() -> Option<Env> {
    let quiverd = Path::new(env!("CARGO_BIN_EXE_quiver")).with_file_name("quiverd");
    if !quiverd.is_file() {
        eprintln!("skipped: {} isn't built (cargo build -p quiverd)", quiverd.display());
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let e = Env { root: tmp.path().to_owned(), _tmp: tmp, port };
    write(&e.dir("data/quiver/accounts.json"), r#"{"accounts":[]}"#);
    let cfg = json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("proton"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false
    });
    write(&e.dir("data/quiver/config.json"), &cfg.to_string());
    std::fs::create_dir_all(e.dir("shm")).unwrap();
    // Nothing runs in podman; no user unit is ever started.
    script(&e.dir("fake/podman"), "#!/bin/sh\nexit 1\n");
    script(&e.dir("fake/systemctl"), "#!/bin/sh\nexit 1\n");
    // The instance: its manifest, and run 7's owner (a Codex thread, from inside it).
    let inst = e.dir("lab/instances").join(INSTANCE);
    {
        use quiver_core::lab::manifest::{Display, GamePaths, Licence, Manifest, Runtime};
        let m = Manifest {
            name: INSTANCE.into(),
            game: quiver_api::GameRef::ubisoft(3539),
            title: "Assassin's Creed Origins".into(),
            licence: Licence::UbisoftBridge,
            created: "2026-11-21T10:00:00Z".parse().unwrap(),
            created_by: "agent".into(),
            display: Display { width: 1280, height: 720, refresh: 60, hdr: false },
            runtime: Runtime { proton: "GE-Proton11-12".into(), proton_path: "/p".into(), dxvk: None, vkd3d: None, nvapi: None, image: quiver_core::lab::IMAGE.into() },
            paths: GamePaths { install: "/g".into(), prefix: "/pf".into(), exe: "ACOrigins.exe".into(), args: vec![] },
            env: Default::default(),
            dll_overrides: Default::default(),
            mod_dir: None,
            graphics: None,
            fixture: None,
            saves: vec![],
            files: Default::default(),
            throttle: vec![],
            reload_pattern: None,
            log_file: None,
            notes: String::new(),
            sight: false,
            clock: None,
            pad: Default::default(),
        };
        std::fs::create_dir_all(&inst).unwrap();
        m.save(&inst.join(quiver_core::lab::manifest::FILE)).unwrap();
    }
    let run_owner = json!({"agent": "codex", "session_id": T1, "source": "agent:codex-ports", "at": "2026-11-21T19:00:00Z"});
    write(&inst.join("lab/runs/r7/owner.json"), &run_owner.to_string());
    e.install_build(&quiverd, "1.0.0-old");
    Some(e)
}

/// A lab instance's bridge on the broker: its play session, held until dropped.
fn bridge(port: u16, session: &str) -> std::net::TcpStream {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    writeln!(s, "{}", json!({"play_session": {"uplay_id": 3539, "session": session}})).unwrap();
    let mut line = String::new();
    BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains("ok"), "broker answered {line}");
    s
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn wrap_up_hint_for_the_owner_of_a_run_holding_up_an_update() {
    let Some(e) = setup() else { return };
    let quiverd = Path::new(env!("CARGO_BIN_EXE_quiver")).with_file_name("quiverd");
    let _d = e.daemon();
    let (me, other) = (codex(T1), codex(T2));

    // Before: no update; the outputs agents parse.
    let list = e.quiver(Some(&me), &[], &["lab", "list", "--json"]);
    assert!(String::from_utf8_lossy(&list.stdout).contains(INSTANCE));
    assert_eq!(stderr(&list), "");
    let whoami = e.quiver(Some(&me), &[], &["lab", "whoami"]);
    assert!(e.status().get("restart_waiting").is_none());

    // The run's bridge connects and a new build lands: quiverd waits on the instance.
    let b = bridge(e.port, &format!("lab-{INSTANCE}-r7"));
    e.install_build(&quiverd, "2.0.0-new");
    let s = e.wait_status("the update to wait on the lab", |s| s["restart_waiting"]["lab"].as_array().is_some_and(|l| !l.is_empty()));
    let w = &s["restart_waiting"];
    assert_eq!(s["waiting_on"][0], "1 bridge connection (lab hinttest, codex:01a1ff7d/agent:codex-ports)", "{s}");
    assert_eq!((w["lab"][0]["instance"].as_str(), w["lab"][0]["run"].as_u64(), w["lab"][0]["session"].as_str()), (Some(INSTANCE), Some(7), Some("lab-hinttest-r7")));
    assert_eq!(w["lab"][0]["free_with"], "quiver lab kill hinttest");
    assert_eq!(w["lab"][0]["owner"]["name"], "codex:01a1ff7d/agent:codex-ports");
    assert_eq!(w["version"], "2.0.0-new");
    assert!(w["lab"][0].get("asked").is_none());
    let text = String::from_utf8(e.quiver(None, &[], &["daemon", "status"]).stdout).unwrap();
    assert!(text.contains("lab agents will be asked to wrap up on their next quiver command: hinttest r7, codex:01a1ff7d/agent:codex-ports"), "{text}");
    // quiverd's "nothing waits" from before is reused for a few seconds; not here.
    e.forget_cache();

    // Another session's commands: nothing (lab ones included), stdout as before.
    let o = e.quiver(Some(&other), &[], &["lab", "list", "--json"]);
    assert_eq!((o.stdout.as_slice(), stderr(&o).as_str()), (list.stdout.as_slice(), ""));
    // Hints off: nothing for the owner either.
    let o = e.quiver(Some(&me), &[("QUIVER_NO_HINTS", "1")], &["lab", "list", "--json"]);
    assert_eq!((o.stdout.as_slice(), stderr(&o).as_str()), (list.stdout.as_slice(), ""));

    // The owner: one line on stderr; stdout byte for byte what it was.
    let o = e.quiver(Some(&me), &[], &["lab", "list", "--json"]);
    assert_eq!(o.stdout, list.stdout, "stdout untouched");
    let err = stderr(&o);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.starts_with("quiver: a quiverd update is waiting on your lab instance \"hinttest\" ("), "{err}");
    assert!(err.contains("stop the run with `quiver lab kill hinttest`") && err.contains("quiver daemon hold --for 30m"), "{err}");
    // Not again within five minutes, whatever the command.
    let o = e.quiver(Some(&me), &[], &["lab", "whoami"]);
    assert_eq!((o.stdout.as_slice(), stderr(&o).as_str()), (whoami.stdout.as_slice(), ""));
    e.forget_cache();
    assert_eq!(stderr(&e.quiver(Some(&me), &[], &["lab", "list", "--json"])), "");

    // The person's view: the owner was asked.
    let s = e.status();
    assert!(s["restart_waiting"]["lab"][0]["asked"].is_string(), "{s}");
    let text = String::from_utf8(e.quiver(None, &[], &["daemon", "status"]).stdout).unwrap();
    assert!(text.contains("lab agents have been asked to wrap up: hinttest r7, codex:01a1ff7d/agent:codex-ports, asked"), "{text}");
    let acts = String::from_utf8(e.quiver(None, &[], &["activity"]).stdout).unwrap();
    assert!(acts.contains("asked codex:01a1ff7d to wrap up lab hinttest so quiverd can restart onto the new build"), "{acts}");

    // A hold the owner takes quiets it (and holds the restart) until it's released.
    let h = e.quiver(Some(&me), &[], &["daemon", "hold", "--for", "10m", "--reason", "finishing a capture", "--json"]);
    let hold: Value = serde_json::from_slice(&h.stdout).unwrap();
    assert_eq!(hold["owner"]["session_id"], T1);
    std::fs::remove_file(e.dir("run/quiver/hints.json")).unwrap();
    assert_eq!(stderr(&e.quiver(Some(&me), &[], &["lab", "list", "--json"])), "", "quiet while its hold lasts");
    e.quiver(None, &[], &["daemon", "release", "--all"]);
    std::fs::remove_file(e.dir("run/quiver/hints.json")).unwrap();
    assert!(stderr(&e.quiver(Some(&me), &[], &["lab", "list", "--json"])).contains("your lab instance \"hinttest\""), "back once released");

    // The run stops (its bridge goes): quiverd restarts onto the new build, nothing waits.
    drop(b);
    let s = e.wait_status("the restart", |s| s["restarts"] == 1);
    assert!(s.get("restart_waiting").is_none() && s.get("update").is_none(), "{s}");
    e.forget_cache();
    let o = e.quiver(Some(&me), &[], &["lab", "list", "--json"]);
    assert_eq!((o.stdout.as_slice(), stderr(&o).as_str()), (list.stdout.as_slice(), ""));
}
