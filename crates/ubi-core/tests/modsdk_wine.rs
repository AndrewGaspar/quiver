//! The mod SDK end to end under Wine: `sdk/test/quiver_mod_test.c` (built with MinGW) uses
//! `quiver_mod.h` against a real `ubi-bridge.exe`, which relays to an in-process mod
//! registry on an ephemeral port (a scratch stand-in for quiverd's broker). Both bridge
//! topologies: the bridge as the game's sibling (how quiverd launches: the test sets
//! `QUIVER_MODS_PIPE` like the launcher does) and as its parent (`--run`, as `quiver
//! steam-run`: the bridge sets it).
//!
//! Opt-in, headless, in a throwaway prefix and HOME:
//!
//! ```sh
//! cargo build -p ubi-bridge --target x86_64-pc-windows-gnu
//! QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton10-34 \
//!   cargo test -p ubi-core --test modsdk_wine -- --ignored --nocapture
//! ```
//!
//! Needs `x86_64-w64-mingw32-gcc`. `QUIVER_TEST_BRIDGE` overrides the bridge's path.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use ubi_api::mods::ModSdkConnection;
use ubi_core::modsdk::{ChangedValues, LiveOutcome, ModMetricsSink, ModOptionsSink, ModRegistry, Sample, value};
use ubi_proto::modsdk::Values;

#[derive(Default)]
struct Rec(Mutex<Vec<String>>);

impl ModOptionsSink for Rec {
    fn snapshot(&self, _m: &ModSdkConnection) -> Values {
        [("test.mode".to_owned(), value("slow", json!("slow")))].into_iter().collect()
    }
    fn changed(&self, _m: &ModSdkConnection, values: &ChangedValues, persist: bool) {
        self.0.lock().unwrap().push(format!("changed {values:?} {persist}"));
    }
}

impl ModMetricsSink for Rec {
    fn samples(&self, _m: &ModSdkConnection, _at: i64, samples: &[Sample]) {
        self.0.lock().unwrap().extend(samples.iter().map(|s| format!("sample {}", s.id)));
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bridge_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIVER_TEST_BRIDGE") {
        return p.into();
    }
    ["debug", "release"]
        .iter()
        .map(|p| repo().join("target/x86_64-pc-windows-gnu").join(p).join("ubi-bridge.exe"))
        .find(|p| p.is_file())
        .expect("build the bridge first: cargo build -p ubi-bridge --target x86_64-pc-windows-gnu")
}

fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

struct Wine {
    proton: PathBuf,
    t: PathBuf,
}

impl Wine {
    fn cmd(&self, args: &[&str]) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(self.proton.join("files/bin/wine"));
        c.args(args)
            .env("WINEPREFIX", self.t.join("pfx"))
            .env("HOME", self.t.join("home"))
            .env("XDG_DATA_HOME", self.t.join("home/.local/share"))
            .env("XDG_CONFIG_HOME", self.t.join("home/.config"))
            .env("XDG_CACHE_HOME", self.t.join("home/.cache"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("QUIVER_MODS_PIPE")
            .env_remove("QUIVER_SESSION")
            .kill_on_drop(true);
        c
    }

    /// Stop this prefix's wineserver (only this one: WINEPREFIX is the scratch prefix).
    async fn kill(&self) {
        let _ = tokio::process::Command::new(self.proton.join("files/bin/wineserver"))
            .arg("-k")
            .env("WINEPREFIX", self.t.join("pfx"))
            .status()
            .await;
    }
}

async fn until<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..secs * 20 {
        if let Some(x) = f() {
            return x;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out");
}

async fn wait_log(path: &Path, needle: &str, secs: u64) -> String {
    until(secs, || std::fs::read_to_string(path).ok().filter(|l| l.contains(needle))).await
}

/// Drive one connected test mod: metrics arrive, a live set is acked per option, an action
/// goes through, its own change reaches the options sink.
async fn exercise(reg: &Arc<ModRegistry>, rec: &Rec, product_id: u32) -> ModSdkConnection {
    let c = until(60, || {
        reg.connections(Some(product_id))
            .into_iter()
            .find(|c| c.options.len() == 2 && c.metrics.iter().filter(|m| m.value.is_some()).count() >= 4)
    })
    .await;
    assert_eq!(c.surface, "quiver-test");
    assert_eq!(c.proto, 2);
    assert_eq!(c.caps, ["options", "metrics", "actions", "log"]);
    assert_eq!(c.version.as_deref(), Some("test-1"));
    assert_eq!(c.module.as_deref(), Some("quiver_mod_test.exe"));
    assert_eq!(c.session.as_deref(), Some("s-winetest"));
    assert!(c.pid.is_some() && c.pid == c.client_pid, "{c:?}");
    let by_id = |id: &str| c.metrics.iter().find(|m| m.id == id).cloned();
    assert!(by_id("quiver-test.frame").is_none(), "frame metrics need rings: refused");
    assert_eq!(by_id("quiver-test.fps.base").unwrap().unit, "fps");
    assert!(by_id("quiver-test.fps.base").unwrap().value.unwrap().as_f64().unwrap() >= 79.5);
    assert!(by_id("quiver-test.frames.real").unwrap().value.unwrap().as_u64().unwrap() > 0);
    let text = by_id("quiver-test.upscaler").unwrap().value.unwrap();
    assert_eq!(text, json!("DLSS K \u{fffd}"));

    let vals: Values = [
        ("test.mode".to_owned(), value("turbo", json!("turbo"))),
        ("test.level".to_owned(), value("99", json!(99))),
        ("test.other".to_owned(), value("1", json!(1))),
    ]
    .into_iter()
    .collect();
    let out = reg.set_options(product_id, "quiver-test", &vals, Duration::from_secs(5)).await;
    assert_eq!(out["test.mode"], LiveOutcome::Applied, "{out:?}");
    assert_eq!(out["test.level"], LiveOutcome::Rejected("level is at most 10".into()), "{out:?}");
    assert_eq!(out["test.other"], LiveOutcome::NotHandled);
    assert_eq!(reg.invoke_action(product_id, "quiver-test", "fg.toggle", Some("now")).unwrap(), 1);
    until(10, || {
        let log = rec.0.lock().unwrap();
        log.iter().any(|l| l.starts_with("changed {\"test.mode\": \"fast\"} false")).then_some(())
    })
    .await;
    let c = until(10, || {
        reg.connections(Some(product_id))
            .into_iter()
            .find(|c| c.metrics.iter().any(|m| m.marks > 0) && c.metrics.iter().any(|m| m.id == "quiver-test.res" && m.value.is_some()))
    })
    .await;
    assert_eq!(c.metrics.iter().find(|m| m.id == "quiver-test.bench").unwrap().value, Some(json!("begin")));
    assert_eq!(c.metrics.iter().find(|m| m.id == "quiver-test.res").unwrap().value, Some(json!("1280x800")));
    c
}

fn check_mod_output(out: &str, poll: bool) {
    assert!(out.contains("open live"), "{out}");
    assert!(out.contains("connected=1"), "{out}");
    assert!(out.contains("value test.mode=slow json=\"slow\""), "welcome snapshot delivered: {out}");
    assert!(out.contains("value test.mode=turbo json=\"turbo\""), "{out}");
    assert!(out.contains("value test.level=99 json=99"), "{out}");
    assert!(out.contains("action fg.toggle arg=now"), "{out}");
    // The last of Quiver's set and the mod's own change (whichever came later).
    assert!(out.contains("get test.mode=turbo") || out.contains("get test.mode=fast"), "{out}");
    assert!(out.contains("closed"), "{out}");
    let _ = poll;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mod_sdk_through_the_bridge() {
    let proton = std::env::var_os("QUIVER_TEST_PROTON").map(PathBuf::from).expect("set QUIVER_TEST_PROTON to a Proton build directory");
    let bridge = bridge_exe();
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().to_path_buf();
    std::fs::create_dir_all(t.join("home")).unwrap();

    // The test mod.
    let exe = t.join("quiver_mod_test.exe");
    let cc = std::process::Command::new("x86_64-w64-mingw32-gcc")
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-O2", "-I"])
        .arg(repo().join("sdk"))
        .arg(repo().join("sdk/test/quiver_mod_test.c"))
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("x86_64-w64-mingw32-gcc");
    assert!(cc.status.success(), "{}", String::from_utf8_lossy(&cc.stderr));
    std::fs::copy(&bridge, t.join("ubi-bridge.exe")).unwrap();

    // The scratch "quiverd": a registry behind an ephemeral port.
    let rec = Arc::new(Rec::default());
    let reg = ModRegistry::new(rec.clone(), rec.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broker = listener.local_addr().unwrap().to_string();
    tokio::spawn(reg.clone().serve_listener(listener));
    let session = "s-winetest";
    let pipe = ubi_proto::modsdk::pipe_name(Some(session));
    let ctx = |uplay_id: u32| {
        json!({
            "uplay_id": uplay_id, "account_id": "test-account", "username": "test", "name_on_platform": "test",
            "upc_ticket": "none", "owned_uplay_ids": [uplay_id], "cd_keys": [], "savegame_path": null,
            "ubiservices_app_id": null, "language": "en-US", "broker": broker, "chunks": [],
            "session": session, "mods_pipe": pipe, "profile": "p-test",
        })
    };
    let w = Wine { proton, t: t.clone() };

    // No pipe in the environment: the SDK does nothing (and the prefix gets made).
    let out = w.cmd(&[&win(&exe), "1"]).output().await.unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && text.contains("open noop declare=-1,-1,-1,-1,-1,-1 bad=-1"), "{text}");
    // A pipe nobody serves: still runs, never connects, never blocks.
    let started = std::time::Instant::now();
    let out = w.cmd(&[&win(&exe), "1"]).env("QUIVER_MODS_PIPE", r"\\.\pipe\quiver-mods-nobody").output().await.unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("open live") && !text.contains("connected=1") && text.contains("closed"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(20));

    // 1. Sibling: the bridge serves, the game is started separately with the pipe's name.
    std::fs::write(t.join("ctx1.json"), ctx(4242).to_string()).unwrap();
    let log1 = t.join("bridge1.log");
    let mut b = w
        .cmd(&[&win(&t.join("ubi-bridge.exe")), &win(&t.join("ctx1.json"))])
        .env("UBI_BRIDGE_LOG", win(&log1))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_log(&log1, "(mods)", 120).await;
    for (mode, product) in [("thread", 4242u32), ("poll", 4242)] {
        rec.0.lock().unwrap().clear();
        let m = w.cmd(&[&win(&exe), "8", mode]).env("QUIVER_MODS_PIPE", &pipe).stdout(Stdio::piped()).spawn().unwrap();
        let c = exercise(&reg, &rec, product).await;
        assert_eq!(c.profile.as_deref(), Some("p-test"));
        let out = m.wait_with_output_async().await;
        check_mod_output(&out, mode == "poll");
        until(10, || reg.connections(Some(product)).is_empty().then_some(())).await;
    }
    let _ = b.start_kill();
    let _ = b.wait().await;
    let bridge_log = std::fs::read_to_string(&log1).unwrap();
    assert!(bridge_log.contains("quiver-test closed: the mod said bye"), "{bridge_log}");
    w.kill().await;

    // 2. Parent: `--run`, the bridge starts the "game" and tells it the pipe.
    std::fs::write(t.join("ctx2.json"), ctx(4343).to_string()).unwrap();
    let log2 = t.join("bridge2.log");
    let run = w
        .cmd(&[&win(&t.join("ubi-bridge.exe")), "--run", &win(&t.join("ctx2.json")), "--log", &win(&log2), "--", &win(&exe), "8"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    exercise(&reg, &rec, 4343).await;
    let out = tokio::time::timeout(Duration::from_secs(60), run.wait_with_output()).await.unwrap().unwrap();
    assert!(out.status.success(), "{}", std::fs::read_to_string(&log2).unwrap_or_default());
    let bridge_log = std::fs::read_to_string(&log2).unwrap();
    assert!(bridge_log.contains("quiver-test closed"), "{bridge_log}");
    assert!(!t.join("ctx2.json").exists(), "--run deletes its context");
    w.kill().await;
}

trait WaitOutput {
    async fn wait_with_output_async(self) -> String;
}

impl WaitOutput for tokio::process::Child {
    async fn wait_with_output_async(self) -> String {
        let out = tokio::time::timeout(Duration::from_secs(60), self.wait_with_output()).await.unwrap().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}
