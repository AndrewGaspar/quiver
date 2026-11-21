//! Another store's launch (Epic's: `launcher::play_store_game`) under Wine, with the mod SDK:
//! the real launch path starts a mods-only `quiver-bridge.exe` as the game's sibling, gives the
//! game `QUIVER_MODS_PIPE`, and the bridge relays to a scratch mod registry (quiverd's broker
//! stand-in). The "game" is `sdk/test/quiver_mod_test.c`, which calls
//! `quiver_set_stereo_layout("stereo:sbs")` right after `quiver_open`, before it's connected
//! (as Torchbearer does in Alan Wake Remastered).
//!
//! First the launch as it was (no broker, so no bridge): the mod never connects. Then with
//! the broker: it connects, as `epic:<app>`, and its stereo word arrives after the welcome.
//!
//! Opt-in, headless, in a throwaway prefix, HOME and data dir:
//!
//! ```sh
//! cargo build -p quiver-bridge --target x86_64-pc-windows-gnu --release
//! QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton10-34 \
//!   cargo test -p quiver-core --test storegame_wine -- --ignored --nocapture
//! ```
//!
//! Needs `x86_64-w64-mingw32-gcc`. `QUIVER_TEST_BRIDGE` overrides the bridge's path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quiver_api::GameRef;
use quiver_api::mods::ModSdkConnection;
use quiver_core::launcher::{self, LaunchEvent, StoreLaunch};
use quiver_core::modsdk::{ChangedValues, ModMetricsSink, ModOptionsSink, ModRegistry};

#[derive(Default)]
struct Rec(Mutex<Vec<String>>);

impl ModOptionsSink for Rec {
    fn changed(&self, _m: &ModSdkConnection, values: &ChangedValues, persist: bool) {
        self.0.lock().unwrap().push(format!("changed {values:?} {persist}"));
    }
}

impl ModMetricsSink for Rec {
    fn stereo(&self, m: &ModSdkConnection) {
        self.0.lock().unwrap().push(format!("stereo {:?} for {}", m.stereo, m.game_ref()));
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bridge_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIVER_TEST_BRIDGE") {
        return p.into();
    }
    ["release", "debug"]
        .iter()
        .map(|p| repo().join("target/x86_64-pc-windows-gnu").join(p).join("quiver-bridge.exe"))
        .find(|p| p.is_file())
        .expect("build the bridge first: cargo build -p quiver-bridge --target x86_64-pc-windows-gnu --release")
}

fn plan(prefix: &Path, record: &Path, session: &str, game: GameRef) -> quiver_core::mods::Plan {
    quiver_core::mods::Plan {
        api: Default::default(),
        overlays: Vec::new(),
        patches: Vec::new(),
        view: quiver_core::confpatch::View { prefix: prefix.to_owned(), profile: None },
        record: record.to_owned(),
        saves_folder: String::new(),
        session: Some(session.to_owned()),
        game: Some(game),
        stream: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn epic_launch_offers_the_mod_sdk() {
    let proton = std::env::var_os("QUIVER_TEST_PROTON").map(PathBuf::from).expect("set QUIVER_TEST_PROTON to a Proton build directory");
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().to_path_buf();
    for d in ["home", "data", "game", "pfx", "lib", "sdk", "backups"] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    // Everything Quiver writes goes to the scratch folder (Proton's steam stub is under the
    // data dir). This test binary has this one test, so nothing else reads these meanwhile.
    unsafe {
        std::env::set_var("HOME", t.join("home"));
        std::env::set_var("XDG_DATA_HOME", t.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", t.join("home/.config"));
        std::env::set_var("XDG_CACHE_HOME", t.join("home/.cache"));
        std::env::set_var("WINEDEBUG", "-all");
        // Proton's steam.exe asserts without a Steam client under HOME (the scratch one has
        // none): without its lsteamclient it runs the game as usual.
        std::env::set_var("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=;lsteamclient=d");
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("QUIVER_MODS_PIPE");
        std::env::remove_var("QUIVER_SESSION");
    }

    // The "game": the test mod, in its install folder.
    let exe = t.join("game/quiver_mod_test.exe");
    let cc = std::process::Command::new("x86_64-w64-mingw32-gcc")
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-O2", "-I"])
        .arg(repo().join("sdk"))
        .arg(repo().join("sdk/test/quiver_mod_test.c"))
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("x86_64-w64-mingw32-gcc");
    assert!(cc.status.success(), "{}", String::from_utf8_lossy(&cc.stderr));

    let cfg: launcher::Config = serde_json::from_value(serde_json::json!({
        "library": t.join("lib"), "prefixes": t, "proton": proton, "sdk_source": t.join("sdk"),
        "save_backups": t.join("backups"), "bridge_exe": bridge_exe(),
    }))
    .unwrap();
    let epic = GameRef::parse("epic:storegametest").unwrap();
    let game = launcher::Game {
        product_id: 0,
        name: "Store game test".into(),
        install_dir: t.join("game"),
        prefix: t.join("pfx"),
        exe: "quiver_mod_test.exe".into(),
        args: Some("6".into()),
        language: None,
        game: Some(epic),
    };

    // The scratch "quiverd": a registry behind an ephemeral port.
    let rec = Arc::new(Rec::default());
    let reg = ModRegistry::new(rec.clone(), rec.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broker = listener.local_addr().unwrap().to_string();
    tokio::spawn(reg.clone().serve_listener(listener));
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let ev = |e: LaunchEvent| {
        let line = match e {
            LaunchEvent::Status(s) => format!("status {s}"),
            LaunchEvent::Running => "running".into(),
            LaunchEvent::Exited(c) => format!("exited {c:?}"),
            LaunchEvent::Detail(d) => format!("detail {}", d.join(" | ")),
            LaunchEvent::Pids(p) => format!("pids {p:?}"),
            LaunchEvent::SavesUploaded(s) => format!("saves uploaded {s:?}"),
        };
        eprintln!("launch: {line}");
        events.lock().unwrap().push(line);
    };
    // Another store's licence arguments (Epic's exchange code): never in the bridge's log.
    let licence = vec!["-AUTH_PASSWORD=not-a-real-code".to_owned(), "-AUTH_TYPE=exchangecode".to_owned()];

    // 1. The launch as it was: no broker, so no bridge and no QUIVER_MODS_PIPE. The mod's
    //    quiver_open is a no-op; nothing ever connects.
    let p1 = plan(&game.prefix, &t.join("patches1.json"), "s-storetest1", epic);
    let store = StoreLaunch { args: licence.clone(), env: Vec::new(), setup: Vec::new(), broker: None };
    let (r, seen) = tokio::join!(launcher::play_store_game(&cfg, &game, &p1, store, &ev), async {
        let mut seen = false;
        for _ in 0..300 {
            seen |= !reg.connections_of(epic).is_empty();
            if events.lock().unwrap().iter().any(|l| l.starts_with("exited")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        seen
    });
    r.unwrap();
    assert!(!seen, "without a bridge no mod can connect");
    assert!(events.lock().unwrap().iter().any(|l| l == "running"));
    events.lock().unwrap().clear();

    // 2. With the broker: a mods-only bridge, the pipe in the game's environment.
    let p2 = plan(&game.prefix, &t.join("patches2.json"), "s-storetest2", epic);
    let store = StoreLaunch { args: licence.clone(), env: Vec::new(), setup: Vec::new(), broker: Some(broker) };
    let (r, conn) = tokio::join!(launcher::play_store_game(&cfg, &game, &p2, store, &ev), async {
        let mut found: Option<ModSdkConnection> = None;
        for _ in 0..600 {
            if let Some(c) = reg.connections_of(epic).into_iter().find(|c| c.stereo.is_some()) {
                found = Some(c);
                break;
            }
            if events.lock().unwrap().iter().any(|l| l.starts_with("exited")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        found
    });
    r.unwrap();
    let log = std::fs::read_to_string(game.prefix.join("quiver-run/bridge.log")).unwrap_or_default();
    let c = conn.unwrap_or_else(|| panic!("the mod never connected; bridge log:\n{log}\nevents: {:?}", events.lock().unwrap()));
    assert_eq!(c.surface, "quiver-test");
    assert_eq!(c.proto, 3);
    assert_eq!(c.game, Some(epic), "{c:?}");
    assert_eq!(c.product_id, 0);
    assert_eq!(c.session.as_deref(), Some("s-storetest2"));
    assert_eq!(c.account, None, "no Ubisoft account behind an Epic game's channel");
    // Said before quiver_open's connection completed, sent after the welcome.
    assert_eq!(c.stereo.as_deref(), Some("sbs"));
    assert!(rec.0.lock().unwrap().iter().any(|l| l == "stereo Some(\"sbs\") for epic:storegametest"), "{:?}", rec.0.lock().unwrap());
    assert!(reg.connections(Some(0)).is_empty(), "an Epic game's mods aren't Ubisoft product 0's");
    // The bridge: mods only, no Connect pipe, no licence arguments in its log.
    assert!(log.contains("mods-only") && log.contains("(mods)"), "{log}");
    assert!(!log.contains("uplay_ipc_pipe"), "no Connect pipes: {log}");
    assert!(!log.contains("not-a-real-code"), "{log}");
    let ev = events.lock().unwrap().clone();
    assert!(ev.iter().any(|l| l.starts_with("detail mod SDK: \\\\.\\pipe\\")), "{ev:?}");
    // The game's own exit code, once the bridge is stopped (proton run waits for it).
    assert!(ev.iter().any(|l| l == "exited Some(0)"), "{ev:?}");
    until_empty(&reg, epic).await;
}

async fn until_empty(reg: &ModRegistry, game: GameRef) {
    for _ in 0..100 {
        if reg.connections_of(game).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the mod's channel outlived the game");
}
