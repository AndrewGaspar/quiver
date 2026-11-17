//! Quiver Tweaks against a scratch quiverd (its own XDG dirs and HOME in a temp folder; no
//! sign-in, no Steam, no game is launched): scopes, the agent policy, the Options entry for
//! the platform surface, providers and the plan of the next launch.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use quiver_api::mods::ModsRequest;
use quiver_api::options::{Live, OptionOp, OptionsChange, OptionsRequest, OptionsRevision, SurfaceState, ValueSource};
use quiver_api::tweaks::{Planned, Prefer, Scope, ScopeKind, TweaksPlan, TweaksRequest};
use quiver_api::{Client, GameRef, Request};

const INI: &str = "[upscaler]\nenabled=1\npreset=M\n[reflex]\nmode=on\nfps_limit=0\n[fg]\nmode=d3d12\nenabled=1\n";

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

    fn daemon(&self) -> Daemon {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiverd"));
        c.env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("HOME", self.dir("home"))
            .env("QUIVER_BROKER", "127.0.0.1:0")
            // Never the user's /dev/shm/quiver-<uid>: a daemon clears stale perf session dirs there.
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("RUST_LOG", "warn")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("QUIVER_TEST_SESSION")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(self.dir("quiverd.log")).unwrap());
        let child = Daemon(c.spawn().unwrap());
        let sock = self.dir("run").join("quiver/quiverd.sock");
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }
}

fn write(p: &Path, s: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let e = Env { root: tmp.path().to_owned(), _tmp: tmp };
    let data = e.dir("data/quiver");
    write(&data.join("accounts.json"), br#"{"accounts":[]}"#);
    let cfg = json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("GE-Proton11-12"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false,
        "env": [["PROTON_ENABLE_NVAPI", "1"], ["DXVK_ENABLE_NVAPI", "1"]]
    });
    write(&data.join("config.json"), cfg.to_string().as_bytes());
    let games = json!({ "games": [{ "product_id": 3539, "name": "Assassin's Creed Origins", "install_dir": e.dir("install"), "prefix": e.dir("prefix"), "exe": "ACOrigins.exe" }] });
    write(&data.join("games.json"), games.to_string().as_bytes());
    write(&e.dir("install/ACOrigins.exe"), b"exe");
    std::fs::create_dir_all(e.dir("prefix")).unwrap();
    write(&e.dir("dlss/aco_dlss.ini"), INI.as_bytes());
    write(&e.dir("dlss/d3d11.dll"), b"dll");
    // A Proton build as the probe reads it (version files, names in the DLLs).
    let b = e.dir("GE-Proton11-12");
    write(&b.join("proton"), b"#!/usr/bin/env python3\n# PROTON_ENABLE_HDR\n");
    write(&b.join("version"), b"1793687165 GE-Proton11-12\n");
    write(&b.join("files/lib/wine/dxvk/version"), b"f8637768 dxvk (v3.1.1-174-gf86377680)\n");
    write(&b.join("files/lib/wine/nvapi/version"), b"9c0f214c dxvk-nvapi (v0.9.3)\n");
    write(&b.join("files/lib/wine/dxvk/x86_64-windows/d3d11.dll"), b"MZ\0dxgi.maxFrameRate\0dxvk.latencySleep\0dxvk.disableNvLowLatency2\0");
    e
}

async fn connect(e: &Env, source: &str) -> Client {
    // SAFETY: this file's only test sets the environment before any client exists.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", e.dir("run")) };
    Client::connect(source).await.unwrap()
}

async fn tweaks<T: serde::de::DeserializeOwned>(c: &Client, r: TweaksRequest) -> anyhow::Result<T> {
    c.call(Request::Tweaks(r)).await
}

fn set(id: &str, v: Value) -> OptionOp {
    OptionOp::Set { surface: "quiver".into(), id: id.into(), value: v }
}

#[tokio::test(flavor = "multi_thread")]
async fn tweaks_end_to_end() {
    let e = setup();
    let _d = e.daemon();
    let cli = connect(&e, "cli").await;
    let agent = Client::connect("tweak-agent").await.unwrap();
    let game = GameRef::ubisoft(3539);

    // Global: Ask for agents, allowed with confirmation, and for people.
    let err = tweaks::<OptionsChange>(&agent, TweaksRequest::Set { scope: Scope::Global, ops: vec![set("fps.cap", json!(160))], message: None, confirmed: false }).await.unwrap_err();
    assert!(err.to_string().contains("agent policy: Ask"), "{err}");
    let c: OptionsChange = tweaks(&cli, TweaksRequest::Set { scope: Scope::Global, ops: vec![set("fps.cap", json!(160)), set("tex.anisotropy", json!("16"))], message: Some("everywhere".into()), confirmed: false }).await.unwrap();
    assert_eq!(c.revision, 1, "{c:?}");
    assert!(c.changes[0].starts_with("Global: fps.cap"), "{:?}", c.changes);
    // Game scope: Allow for agents; bad values rejected per op.
    let c: OptionsChange = tweaks(&agent, TweaksRequest::Set { scope: Scope::Game { game }, ops: vec![set("sync.vsync", json!("off")), set("sync.vsync", json!("maybe"))], message: None, confirmed: false }).await.unwrap();
    assert_eq!(c.revision, 1);
    assert!(matches!(&c.outcomes[1].outcome, quiver_api::options::Outcome::Rejected { .. }));

    // Without mods: the plan of the next launch.
    let p: TweaksPlan = tweaks(&cli, TweaksRequest::Plan { game, profile: None }).await.unwrap();
    assert_eq!(p.caps.proton.as_ref().unwrap().nvapi.as_deref(), Some("v0.9.3"));
    assert_eq!(p.env["DXVK_CONFIG"], "d3d9.maxFrameRate = 160; dxgi.maxFrameRate = 160; d3d9.presentInterval = 0; dxgi.syncInterval = 0; d3d11.samplerAnisotropy = 16; d3d9.samplerAnisotropy = 16", "{:?}", p.env);
    assert_eq!(p.revisions.global, 1);

    // With the DLSS mod in a profile: it provides the cap; Quiver's DXVK limiter stands down.
    let _: Value = cli.call(Request::Mods(ModsRequest::Install { product_id: 3539, builtin: None, path: Some(e.dir("dlss")), name: Some("DLSS".into()) })).await.unwrap();
    let _: Value = cli.call(Request::Mods(ModsRequest::ProfileCreate { product_id: 3539, name: "DLSS".into(), layers: vec!["DLSS".into()], message: None, saves: Some("default".into()) })).await.unwrap();
    let p: TweaksPlan = tweaks(&cli, TweaksRequest::Plan { game, profile: Some("DLSS".into()) }).await.unwrap();
    assert!(matches!(&p.entry("fps.cap").unwrap().planned, Planned::Provided { by, option, .. } if by == "DLSS" && option == "reflex.fps_limit"), "{:?}", p.entry("fps.cap"));
    assert!(!p.env["DXVK_CONFIG"].contains("maxFrameRate"));
    // The mod's option shows the bound value, from Quiver Tweaks (global).
    let list: Vec<SurfaceState> = cli.call(Request::Options(OptionsRequest::List { product_id: 3539, profile: Some("DLSS".into()), level: None })).await.unwrap();
    assert_eq!(list.len(), 2);
    let dlss = &list[0];
    assert_eq!(dlss.options["reflex.fps_limit"].value, json!(160.0));
    assert!(matches!(&dlss.options["reflex.fps_limit"].from, ValueSource::Tweak { scope: ScopeKind::Global, .. }));
    let platform = &list[1];
    assert_eq!(platform.key, "quiver/tweaks");
    assert!(matches!(platform.options["fps.cap"].tweak.as_ref().unwrap().planned, Some(Planned::Provided { .. })));

    // The profile scope through the Options request (surface "quiver").
    let c: OptionsChange = cli
        .call(Request::Options(OptionsRequest::Set { product_id: 3539, profile: Some("DLSS".into()), ops: vec![set("tex.anisotropy", json!("8"))], live: Live::IfRunning, message: None, confirmed: false }))
        .await
        .unwrap();
    assert!(c.revision > 0 && c.changes[0].starts_with("Profile: tex.anisotropy 16"), "{c:?}");
    let st: SurfaceState = tweaks(&cli, TweaksRequest::List { scope: Scope::Profile { game, profile: Some("DLSS".into()) }, level: None }).await.unwrap();
    assert_eq!(st.options["tex.anisotropy"].value, json!("8"));
    assert_eq!(st.options["tex.anisotropy"].default, json!("16"), "the lower scope's value");
    assert!(matches!(st.options["fps.cap"].from, ValueSource::Scope { scope: ScopeKind::Global, revision: 1, .. }));
    assert!(matches!(st.options["sync.vsync"].from, ValueSource::Scope { scope: ScopeKind::Game, .. }));
    // The game-scope view knows the profile overrides it.
    let st: SurfaceState = tweaks(&cli, TweaksRequest::List { scope: Scope::Game { game }, level: None }).await.unwrap();
    assert_eq!(st.options["tex.anisotropy"].tweak.as_ref().unwrap().overridden_by, None, "the default profile (vanilla) doesn't set it");

    // Prefer Quiver's cap for this game: the mod's cap goes off, but its FG blocks DXVK's.
    let c: OptionsChange = tweaks(&agent, TweaksRequest::Prefer { scope: Scope::Game { game }, id: "fps.cap".into(), prefer: Prefer::Quiver, message: None, confirmed: false }).await.unwrap();
    assert!(c.revision > 0);
    let p: TweaksPlan = tweaks(&cli, TweaksRequest::Plan { game, profile: Some("DLSS".into()) }).await.unwrap();
    assert!(matches!(&p.entry("fps.cap").unwrap().planned, Planned::Unavailable { reason } if reason.contains("DLSS: Frame Generation presents two frames")), "{:?}", p.entry("fps.cap"));
    assert!(p.bound.iter().any(|b| b.option == "reflex.fps_limit" && b.value == json!(0)));

    // History and restore (Ask for agents).
    let h: Vec<OptionsRevision> = tweaks(&cli, TweaksRequest::History { scope: Scope::Global }).await.unwrap();
    assert_eq!(h.len(), 1);
    assert!(tweaks::<OptionsChange>(&agent, TweaksRequest::Restore { scope: Scope::Global, revision: 0, confirmed: false }).await.is_err());
    let c: OptionsChange = tweaks(&cli, TweaksRequest::Restore { scope: Scope::Global, revision: 0, confirmed: false }).await.unwrap();
    assert_eq!(c.revision, 2);
    let st: SurfaceState = tweaks(&cli, TweaksRequest::List { scope: Scope::Global, level: None }).await.unwrap();
    assert_eq!(st.options["fps.cap"].value, json!(0));
    assert!(st.schema_note.contains("Quiver catalog"));
    // Presets, at the game scope.
    let c: OptionsChange = tweaks(&cli, TweaksRequest::Set { scope: Scope::Game { game }, ops: vec![OptionOp::Preset { surface: "quiver".into(), id: "competitive".into() }], message: None, confirmed: false }).await.unwrap();
    assert!(c.changes.iter().any(|l| l.contains("preset Competitive")), "{:?}", c.changes);
    // No sessions yet.
    let s: Vec<Value> = tweaks(&cli, TweaksRequest::Sessions { game, limit: None }).await.unwrap();
    assert!(s.is_empty());
}
