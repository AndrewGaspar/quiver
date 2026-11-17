//! Mod options against a scratch quiverd (its own XDG dirs and HOME in a temp folder; no
//! sign-in, no Steam, no game). A stand-in "running game" comes from the daemon's test
//! hook (`QUIVER_TEST_SESSION`), whose process root is `/`, so live rewrites land in the
//! temp install folder.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use quiver_api::mods::ModsRequest;
use quiver_api::options::{Live, LiveChannel, OptionOp, OptionsChange, OptionsEvent, OptionsRequest, OptionsRevision, Outcome, SessionDetail, SurfaceState};
use quiver_api::{Client, Request};

const INI: &str = "; aco-dlss configuration\n[upscaler]\nenabled=1\n; DLSS render preset\npreset=M\n[fg]\nmode=d3d12\nenabled=1\ntoggle_key=F9\npresent_thread=1\n[world]\nnpc_density=1\nnpc_distance=1\n";

/// The scratch daemon, stopped when dropped (also when a test fails).
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

    fn install(&self) -> PathBuf {
        self.dir("install")
    }

    fn daemon(&self, test_session: Option<&str>) -> Daemon {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiverd"));
        c.env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("HOME", self.dir("home"))
            .env("QUIVER_BROKER", "127.0.0.1:0")
            // Its own perf session dirs: at start quiverd sweeps every session dir it
            // doesn't know, which in /dev/shm would include a running daemon's.
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("RUST_LOG", "warn")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("QUIVER_TEST_SESSION")
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(self.dir("quiverd.log")).unwrap());
        if let Some(s) = test_session {
            c.env("QUIVER_TEST_SESSION", s);
        }
        let child = c.spawn().unwrap();
        let child = Daemon(child);
        let sock = self.dir("run").join("quiver/quiverd.sock");
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        child
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
    // No accounts (the keyring is never asked), no Steam offers, an installed game.
    write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
    let cfg = json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("proton"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false
    });
    write(&data.join("config.json"), &cfg.to_string());
    let games = json!({ "games": [{ "product_id": 3539, "name": "Assassin's Creed Origins", "install_dir": e.install(), "prefix": e.dir("prefix"), "exe": "ACOrigins.exe" }] });
    write(&data.join("games.json"), &games.to_string());
    write(&e.install().join("ACOrigins.exe"), "exe");
    std::fs::create_dir_all(e.dir("prefix")).unwrap();
    write(&e.dir("dlss/aco_dlss.ini"), INI);
    write(&e.dir("dlss/d3d11.dll"), "dll");
    e
}

async fn connect(e: &Env, source: &str) -> Client {
    // SAFETY: this file's only test sets the environment before any client exists.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", e.dir("run")) };
    Client::connect(source).await.unwrap()
}

async fn options<T: serde::de::DeserializeOwned>(c: &Client, r: OptionsRequest) -> anyhow::Result<T> {
    c.call(Request::Options(r)).await
}

fn set(surface: &str, id: &str, v: Value) -> OptionOp {
    OptionOp::Set { surface: surface.into(), id: id.into(), value: v }
}

#[tokio::test(flavor = "multi_thread")]
async fn options_end_to_end() {
    let e = setup();
    let d = e.daemon(None);
    let cli = connect(&e, "cli").await;
    let _: Value = cli.call(Request::Mods(ModsRequest::Install { product_id: 3539, builtin: None, path: Some(e.dir("dlss")), name: Some("DLSS".into()) })).await.unwrap();
    let _: Value = cli.call(Request::Mods(ModsRequest::ProfileCreate { product_id: 3539, name: "DLSS".into(), layers: vec!["DLSS".into()], message: None, saves: Some("default".into()) })).await.unwrap();

    // Discovery: the catalog schema, defaults from the shipped file.
    let list: Vec<SurfaceState> = options(&cli, OptionsRequest::List { product_id: 3539, profile: Some("DLSS".into()), level: None }).await.unwrap();
    assert_eq!(list.len(), 2, "DLSS, then Quiver Tweaks");
    assert_eq!(list[1].key, "quiver/tweaks");
    assert_eq!(list[0].title, "DLSS");
    assert_eq!(list[0].options["upscaler.preset"].value, json!("M"));
    assert_eq!(list[0].live.channel, LiveChannel::File);
    assert!(!list[0].live.connected);
    let basic: Vec<SurfaceState> = options(&cli, OptionsRequest::List { product_id: 3539, profile: Some("DLSS".into()), level: Some(quiver_api::options::Level::Basic) }).await.unwrap();
    assert!(basic[0].options.len() < list[0].options.len());

    // Not running: saved for the next launch; bad values rejected per op.
    let c: OptionsChange = options(
        &cli,
        OptionsRequest::Set {
            product_id: 3539,
            profile: Some("DLSS".into()),
            ops: vec![set("DLSS", "world.npc_density", json!("4")), set("DLSS", "fg.mode", json!("vulkan2"))],
            live: Live::IfRunning,
            message: Some("busier".into()),
            confirmed: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(c.revision, 1);
    assert_eq!(c.outcomes[0].outcome, Outcome::SavedRestart { running: false });
    assert!(matches!(&c.outcomes[1].outcome, Outcome::Rejected { reason } if reason.contains("one of off, d3d12")), "{:?}", c.outcomes);
    // Agents may change profiles; danger options need confirmation.
    let agent = connect(&e, "agent").await;
    let c: OptionsChange = options(&agent, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("aco-dlss", "fg.pacing", json!(false))], live: Live::IfRunning, message: None, confirmed: false }).await.unwrap();
    assert_eq!(c.revision, 2);
    let err = options::<OptionsChange>(&agent, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("DLSS", "fg.hide_handle64", json!(true))], live: Live::No, message: None, confirmed: false }).await.unwrap_err().to_string();
    assert!(err.contains("dangerous option"), "{err}");
    // History, diff, restore (Ask for agents).
    let h: Vec<OptionsRevision> = options(&cli, OptionsRequest::History { product_id: 3539, profile: Some("DLSS".into()) }).await.unwrap();
    assert_eq!(h.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(), ["cli", "agent"]);
    let diff: Vec<quiver_api::options::OptionDiff> = options(&cli, OptionsRequest::Diff { product_id: 3539, profile: Some("DLSS".into()), against: Some("vanilla".into()) }).await.unwrap();
    assert_eq!(diff.len(), 2, "{diff:?}");
    assert!(options::<OptionsChange>(&agent, OptionsRequest::Restore { product_id: 3539, profile: Some("DLSS".into()), revision: 1, confirmed: false }).await.unwrap_err().to_string().contains("Ask"));
    let keys: quiver_api::options::KeysReport = options(&cli, OptionsRequest::Keys { product_id: 3539, profile: Some("DLSS".into()) }).await.unwrap();
    assert!(keys.bindings.iter().any(|b| b.key == "F9"));
    drop((cli, agent));
    drop(d);

    // "Running": the test hook registers a session whose root is `/`.
    write(&e.install().join("aco_dlss.ini"), INI);
    let d = e.daemon(Some("3539:DLSS:/"));
    let cli = connect(&e, "cli").await;
    let mut events = cli.subscribe_options().await.unwrap();
    let sessions: Vec<SessionDetail> = options(&cli, OptionsRequest::Sessions).await.unwrap();
    let _ = sessions; // (PlaySessions only lists sessions with a lease; the hook has none.)
    let c: OptionsChange = options(
        &cli,
        OptionsRequest::Set {
            product_id: 3539,
            profile: Some("DLSS".into()),
            ops: vec![set("DLSS", "world.npc_density", json!(5)), set("DLSS", "fg.mode", json!("off")), set("DLSS", "fg.present_thread", json!("0"))],
            live: Live::IfRunning,
            message: None,
            confirmed: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(c.outcomes[0].outcome, Outcome::AppliedLive { channel: LiveChannel::File, latency: Some("about a minute".into()) }, "{c:?}");
    assert_eq!(c.outcomes[1].outcome, Outcome::SavedRestart { running: true });
    assert!(matches!(&c.outcomes[2].outcome, Outcome::SavedAt { .. }));
    assert!(c.live[0].contains("rewrote aco_dlss.ini"), "{:?}", c.live);
    let text = std::fs::read_to_string(e.install().join("aco_dlss.ini")).unwrap();
    assert!(text.contains("npc_density=5\n") && text.starts_with("; aco-dlss configuration\n"), "{text}");
    // Pending: the restart option isn't what the running game has.
    let list: Vec<SurfaceState> = options(&cli, OptionsRequest::List { product_id: 3539, profile: Some("DLSS".into()), level: None }).await.unwrap();
    assert!(list[0].live.connected);
    assert_eq!(list[0].options["fg.mode"].pending, quiver_api::options::Pending::Restart);
    assert_eq!(list[0].options["world.npc_density"].pending, quiver_api::options::Pending::None);
    // Events: a new revision, the live values, and the outcomes (Sight's toasts).
    let mut got = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
        got.push(ev);
        if got.len() >= 3 {
            break;
        }
    }
    assert!(got.iter().any(|e| matches!(e, OptionsEvent::Changed { revision: 3, .. })), "{got:?}");
    assert!(got.iter().any(|e| matches!(e, OptionsEvent::Live { values, saved: true, .. } if values.contains_key("world.npc_density"))), "{got:?}");
    assert!(
        got.iter().any(|e| matches!(e, OptionsEvent::Outcome { outcomes, by, .. } if by == "cli" && outcomes.len() == 3 && matches!(outcomes[0].outcome, Outcome::AppliedLive { .. }))),
        "{got:?}"
    );
    // The same events reach `subscribe()` as `Event`s (the wire form since the fold).
    let mut all = cli.subscribe().await.unwrap();
    let _: OptionsChange = options(&cli, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("DLSS", "world.npc_distance", json!(1.5))], live: Live::No, message: None, confirmed: false }).await.unwrap();
    let mut folded = false;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), all.recv()).await {
        if matches!(ev, quiver_api::Event::OptionsChanged { revision: 4, .. }) {
            folded = true;
            break;
        }
    }
    assert!(folded, "no Event::OptionsChanged");
    let _: OptionsChange = options(&cli, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![OptionOp::Reset { surface: "DLSS".into(), id: "world.npc_distance".into() }], live: Live::No, message: None, confirmed: false }).await.unwrap();
    while tokio::time::timeout(Duration::from_millis(200), events.recv()).await.is_ok_and(|e| e.is_some()) {}
    // The app's requests: a mod's readable files, and the session records.
    let files: Vec<String> = options(&cli, OptionsRequest::Files { product_id: 3539, mod_id: "DLSS".into() }).await.unwrap();
    assert_eq!(files, ["aco_dlss.ini"]);
    let records: Vec<quiver_api::options::SessionOptions> = options(&cli, OptionsRequest::SessionRecords { product_id: 3539, profile: Some("DLSS".into()), limit: None }).await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(!records[0].live_changes.is_empty());
    // Agents need confirmation for live changes to a running game; --no-live is fine.
    let agent = connect(&e, "agent").await;
    let err = options::<OptionsChange>(&agent, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("DLSS", "world.npc_density", json!(3))], live: Live::IfRunning, message: None, confirmed: false }).await.unwrap_err().to_string();
    assert!(err.contains("is running") && err.contains("--no-live"), "{err}");
    let c: OptionsChange = options(&agent, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("DLSS", "world.npc_density", json!(3))], live: Live::No, message: None, confirmed: false }).await.unwrap();
    assert!(matches!(&c.outcomes[0].outcome, Outcome::SavedPending { detail } if detail.contains("not sent")), "{:?}", c.outcomes);
    assert!(c.live.is_empty());
    // Session only: not saved, written live.
    let before = c.revision;
    let c: OptionsChange = options(&cli, OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("DLSS", "world.npc_distance", json!(2))], live: Live::Only, message: None, confirmed: false }).await.unwrap();
    assert_eq!(c.revision, 0);
    assert!(matches!(c.outcomes[0].outcome, Outcome::AppliedLive { .. }));
    assert!(std::fs::read_to_string(e.install().join("aco_dlss.ini")).unwrap().contains("npc_distance=2\n"));
    let h: Vec<OptionsRevision> = options(&cli, OptionsRequest::History { product_id: 3539, profile: Some("DLSS".into()) }).await.unwrap();
    assert_eq!(h.last().unwrap().revision, before);
    // Revert the session: back to the values it launched with (r2), live.
    let list: Vec<SurfaceState> = options(&cli, OptionsRequest::List { product_id: 3539, profile: Some("DLSS".into()), level: None }).await.unwrap();
    let session = list[0].live.session.clone().unwrap();
    let c: OptionsChange = options(&cli, OptionsRequest::RevertSession { product_id: 3539, session, confirmed: false }).await.unwrap();
    assert!(c.revision > before);
    let text = std::fs::read_to_string(e.install().join("aco_dlss.ini")).unwrap();
    assert!(text.contains("npc_density=4\n") && text.contains("npc_distance=1\n") && text.contains("mode=d3d12\n"), "{text}");
    // Actions need the SDK.
    let err = options::<Value>(&cli, OptionsRequest::Invoke { product_id: 3539, surface: "DLSS".into(), action: "fg.toggle".into(), arg: None, session: None }).await.unwrap_err().to_string();
    assert!(err.contains("SDK"), "{err}");
    // Lint a folder before `version add`.
    write(&e.dir("build/aco_dlss.ini"), INI);
    write(&e.dir("build/quiver-options.toml"), "format = 1\nsurface = \"aco-dlss\"\n[file.ini]\npath = \"aco_dlss.ini\"\nowner = \"quiver\"\n[option.\"world.npc_density\"]\ntype = \"float\"\nmin = 1\nmax = 5\n");
    let r: quiver_api::options::LintReport = options(&cli, OptionsRequest::Lint { path: e.dir("build") }).await.unwrap();
    assert_eq!(r.options, 1);
    assert!(r.undescribed.iter().any(|k| k == "ini:fg/mode"), "{:?}", r.undescribed);
    drop((cli, agent));
    drop(d);
}
