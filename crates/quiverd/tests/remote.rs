//! Remote play's control plane between two scratch quiverds on this machine (design/REMOTE.md
//! §3, §4, §8): each with its own HOME, XDG dirs, socket, perf shm root, broker port and
//! remote port, mDNS off, and an empty environment otherwise (no keyring, no display).
//! Nothing here touches a real install or a running quiverd.
//!
//! "tower" is the server (remote on, pairing), "laptop" the client (pairs with tower; the
//! test then acts as a process on the laptop, reading its identity and peers to connect).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quiver_api::remote::{PairCode, PeerInfo, PeerState, RemoteStatus, Role};
use quiver_api::{ActivityEntry, CallError, Client, Event, Request};
use serde_json::{Value, json};

/// A scratch daemon, killed when dropped (also when the test fails).
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Machine {
    root: PathBuf,
    name: &'static str,
}

impl Machine {
    fn new(tmp: &Path, name: &'static str) -> Machine {
        let m = Machine { root: tmp.join(name), name };
        let data = m.dir("data/quiver");
        // No accounts (the keyring is never asked), no Steam offers, no games.
        write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
        let cfg = json!({
            "library": m.dir("lib"), "prefixes": m.dir("pfx"), "proton": m.dir("proton"), "sdk_source": m.dir("sdk"),
            "save_backups": m.dir("backups"), "report_playtime": false, "steam_offers": false
        });
        write(&data.join("config.json"), &cfg.to_string());
        write(&data.join("remote/machine.json"), &json!({ "name": name }).to_string());
        m
    }

    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn sock(&self) -> PathBuf {
        self.dir("run/quiver/quiverd.sock")
    }

    fn remote_dir(&self) -> PathBuf {
        self.dir("data/quiver/remote")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default()
    }

    fn daemon(&self) -> Daemon {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir("quiverd.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_quiverd"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir("home"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("QUIVER_BROKER", "127.0.0.1:0")
            .env("QUIVER_AUTO_RESTART", "0")
            .env("QUIVER_REMOTE_MDNS", "0")
            .env("RUST_LOG", "quiverd=info")
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let d = Daemon(child);
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.sock()).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "{} didn't start: {}", self.name, self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    async fn client(&self) -> Client {
        Client::connect_running_at(self.sock(), "cli").await.unwrap()
    }

    /// The paired server `name`, as this machine's peers list it.
    fn server(&self, name: &str) -> quiver_remote::Machine {
        let peers = quiver_remote::Peers::load(&self.remote_dir()).unwrap();
        peers.one(name, Some(quiver_remote::Role::Server)).unwrap().machine().unwrap()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
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

fn err_code(e: &anyhow::Error) -> Option<String> {
    CallError::code_of(e).map(str::to_owned).or_else(|| quiver_remote::wire::refused_code(e).map(str::to_owned))
}

async fn pair(tower: &Client, laptop: &Client, addr: &str) -> PeerInfo {
    let code: PairCode = tower.call(Request::RemotePairBegin).await.unwrap();
    laptop.call(Request::RemotePairWith { addr: addr.into(), code: code.code }).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_serve_revoke_and_reconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let tower_m = Machine::new(tmp.path(), "tower");
    let laptop_m = Machine::new(tmp.path(), "laptop");
    let _tower_d = tower_m.daemon();
    let _laptop_d = laptop_m.daemon();
    let tower = tower_m.client().await;
    let laptop = laptop_m.client().await;
    let mut tower_events = tower.subscribe().await.unwrap();

    // Off by default: nothing listens, and pairing needs it on.
    let s: RemoteStatus = tower.call(Request::RemoteStatus).await.unwrap();
    assert_eq!((s.name.as_str(), s.enabled, s.listening.as_deref()), ("tower", false, None));
    assert!(!s.local_only.is_empty());
    let e = tower.call::<PairCode>(Request::RemotePairBegin).await.unwrap_err();
    assert!(format!("{e:#}").contains("remote enable"), "{e:#}");

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let s: RemoteStatus = tower.call(Request::RemoteEnable { listen: Some(addr.clone()) }).await.unwrap();
    assert_eq!((s.enabled, s.listening.as_deref(), s.discoverable), (true, Some(addr.as_str()), false));
    assert!(std::fs::read_to_string(tower_m.dir("data/quiver/config.json")).unwrap().contains(&addr), "remembered");
    let key = tower_m.remote_dir().join("machine.key");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);

    // A wrong code is refused (a try is used), then the right one pairs.
    let code: PairCode = tower.call(Request::RemotePairBegin).await.unwrap();
    assert_eq!(code.name, "tower");
    let wrong = format!("{:06}", (code.code.parse::<u32>().unwrap() + 1) % 1_000_000);
    let e = laptop.call::<PeerInfo>(Request::RemotePairWith { addr: addr.clone(), code: wrong }).await.unwrap_err();
    assert!(format!("{e:#}").contains("wrong code"), "{e:#}");
    let s: RemoteStatus = tower.call(Request::RemoteStatus).await.unwrap();
    assert_eq!(s.pairing.as_ref().map(|p| p.tries_left), Some(2), "{s:?}");
    let server: PeerInfo = laptop.call(Request::RemotePairWith { addr: addr.clone(), code: code.code.clone() }).await.unwrap();
    assert_eq!((server.name.as_str(), server.role, server.state), ("tower", Role::Server, PeerState::Online));
    let ended = event(&mut tower_events, "PairingEnded", |e| matches!(e, Event::PairingEnded { .. })).await;
    let Event::PairingEnded { outcome, name, .. } = ended else { unreachable!() };
    assert_eq!((outcome.as_str(), name.as_deref()), ("paired", Some("laptop")));
    let peers: Vec<PeerInfo> = tower.call(Request::RemotePeers).await.unwrap();
    assert_eq!(peers.len(), 1);
    let laptop_id = peers[0].id.clone();
    assert_eq!((peers[0].name.as_str(), peers[0].role), ("laptop", Role::Client));
    // The code was spent: it pairs nothing more.
    let e = laptop.call::<PeerInfo>(Request::RemotePairWith { addr: addr.clone(), code: code.code.clone() }).await.unwrap_err();
    assert!(format!("{e:#}").contains("isn't pairing"), "{e:#}");

    // A process on the laptop reaches tower's quiverd over the tunnel.
    let machine = laptop_m.server("tower");
    let remote = Client::machine_at(laptop_m.remote_dir(), &machine, "agent").await.unwrap();
    assert_eq!(remote.remote_machine().map(|m| m.name.as_str()), Some("tower"));
    event(&mut tower_events, "PeerOnline", |e| matches!(e, Event::PeerOnline { name, .. } if name == "laptop")).await;
    let st: quiver_api::Status = remote.call(Request::Status).await.unwrap();
    assert_eq!(st.version, env!("CARGO_PKG_VERSION"));
    let rs: RemoteStatus = remote.call(Request::RemoteStatus).await.unwrap();
    assert_eq!(rs.name, "tower");
    let peers: Vec<PeerInfo> = tower.call(Request::RemotePeers).await.unwrap();
    assert_eq!((peers[0].state, peers[0].connections), (PeerState::Online, 1));

    // Its requests are the laptop's, keeping the label a non-person client gave itself
    // (`remote::remote_source_as`; REMOTE.md §6.1).
    let _: Value = remote.call(Request::DownloadSettings { max_active: Some(2), bandwidth_limit: None, auto_update: None }).await.unwrap();
    let activity: Vec<ActivityEntry> = tower.call(Request::Activity { limit: 20 }).await.unwrap();
    let line = activity.iter().find(|a| a.action.starts_with("download settings")).expect("logged");
    assert_eq!(line.source, "remote:laptop/agent");

    // Local-only requests are refused over the tunnel, with a code; stubs say what they are.
    let e = remote.call::<Value>(Request::Shutdown).await.unwrap_err();
    assert_eq!(err_code(&e).as_deref(), Some("local_only"), "{e:#}");
    let e = remote.call::<Value>(Request::Import { game: 3539.into(), path: "/mnt/x".into(), language: None }).await.unwrap_err();
    assert_eq!(err_code(&e).as_deref(), Some("local_only"), "{e:#}");
    let caps: quiver_api::remote::ClientCaps = serde_json::from_value(json!({
        "decoders": [], "display": {"width": 1920, "height": 1080, "refresh_mhz": 60000, "hdr": false, "vrr": false, "stereo_sbs": false, "scale": 1.0},
        "audio_channels": 2, "pads": [], "microphone": false
    }))
    .unwrap();
    let start = Request::StartStream { target: quiver_api::remote::StreamTarget::Desktop, prefs: Default::default(), caps };
    let e = remote.call::<Value>(start).await.unwrap_err();
    assert_eq!(err_code(&e).as_deref(), Some("not_implemented"), "{e:#}");

    // Events come over the tunnel.
    let mut remote_events = remote.subscribe().await.unwrap();
    let _: RemoteStatus = tower.call(Request::RemoteRename { id: None, name: "Tower".into() }).await.unwrap();
    event(&mut remote_events, "RemoteChanged over the tunnel", |e| matches!(e, Event::RemoteChanged)).await;

    // A key that was never paired is refused before any request.
    let stranger = quiver_remote::Identity::load_or_create(&tmp.path().join("stranger")).unwrap();
    let e = quiver_remote::connect(&stranger, &machine, quiver_remote::wire::Intent::Api).await.unwrap_err();
    assert_eq!(err_code(&e).as_deref(), Some("not_paired"), "{e:#}");
    // And a server with another key fails the client's TLS handshake.
    let wrong_key = quiver_remote::Machine { pin: stranger.pin(), ..machine.clone() };
    let e = quiver_remote::connect(&stranger, &wrong_key, quiver_remote::wire::Intent::Api).await.unwrap_err();
    assert!(format!("{e:#}").contains("key changed"), "{e:#}");

    // Revoking drops the laptop's connection at once; it is no longer served.
    let _: Vec<PeerInfo> = tower.call(Request::RemoteRevoke { id: laptop_id.clone() }).await.unwrap();
    event(&mut remote_events, "Disconnected after the revoke", |e| matches!(e, Event::Disconnected)).await;
    let e = remote.call::<Value>(Request::Status).await.unwrap_err();
    assert_eq!(err_code(&e).as_deref(), Some("not_paired"), "{e:#}");

    // Paired again, the same client reconnects by itself and events flow again.
    pair(&tower, &laptop, &addr).await;
    event(&mut remote_events, "Reconnected", |e| matches!(e, Event::Reconnected)).await;
    let rs: RemoteStatus = remote.call(Request::RemoteStatus).await.unwrap();
    assert_eq!(rs.name, "Tower");

    // Tower restarts (stopped and started again): remote stays on, and the client comes back.
    drop(_tower_d);
    event(&mut remote_events, "Disconnected when tower stops", |e| matches!(e, Event::Disconnected)).await;
    let _tower_d = tower_m.daemon();
    event(&mut remote_events, "Reconnected after tower restarts", |e| matches!(e, Event::Reconnected)).await;
    let rs: RemoteStatus = remote.call(Request::RemoteStatus).await.unwrap();
    assert_eq!((rs.enabled, rs.listening.as_deref()), (true, Some(addr.as_str())));

    // The laptop's own quiverd sees tower as reachable.
    // Its peer check runs on its own clock and may have caught tower mid-restart, so allow it
    // until the next check (every 20 s) to see tower again.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let peers: Vec<PeerInfo> = laptop.call(Request::RemotePeers).await.unwrap();
        let state = peers.iter().find(|p| p.role == Role::Server).map(|p| p.state);
        if state == Some(PeerState::Online) {
            break;
        }
        assert!(Instant::now() < deadline, "laptop never saw tower online again: {state:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Turning remote off drops the connection and stops listening.
    let tower = tower_m.client().await;
    let () = tower.call(Request::RemoteDisable).await.unwrap();
    event(&mut remote_events, "Disconnected when remote goes off", |e| matches!(e, Event::Disconnected)).await;
    assert!(std::net::TcpStream::connect(&addr).is_err(), "nothing listens");
    let s: RemoteStatus = tower.call(Request::RemoteStatus).await.unwrap();
    assert!(!s.enabled && s.listening.is_none());
    assert!(!tower_m.log().contains(&hex_key(&tower_m)), "the key never reaches the log");
}

/// The private key file's base64 body, to check it never shows up anywhere else.
fn hex_key(m: &Machine) -> String {
    let pem = std::fs::read_to_string(m.remote_dir().join("machine.key")).unwrap();
    pem.lines().filter(|l| !l.starts_with("-----")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn three_wrong_codes_close_pairing() {
    let tmp = tempfile::tempdir().unwrap();
    let tower_m = Machine::new(tmp.path(), "tower");
    let laptop_m = Machine::new(tmp.path(), "laptop");
    let _a = tower_m.daemon();
    let _b = laptop_m.daemon();
    let tower = tower_m.client().await;
    let laptop = laptop_m.client().await;
    let mut events = tower.subscribe().await.unwrap();
    let addr = format!("127.0.0.1:{}", free_port());
    let _: RemoteStatus = tower.call(Request::RemoteEnable { listen: Some(addr.clone()) }).await.unwrap();
    let code: PairCode = tower.call(Request::RemotePairBegin).await.unwrap();
    let wrong = format!("{:06}", (code.code.parse::<u32>().unwrap() + 7) % 1_000_000);
    for _ in 0..3 {
        let e = laptop.call::<PeerInfo>(Request::RemotePairWith { addr: addr.clone(), code: wrong.clone() }).await.unwrap_err();
        assert!(format!("{e:#}").contains("wrong code"), "{e:#}");
    }
    let ended = event(&mut events, "PairingEnded", |e| matches!(e, Event::PairingEnded { .. })).await;
    assert!(matches!(ended, Event::PairingEnded { ref outcome, .. } if outcome == "failed"), "{ended:?}");
    let e = laptop.call::<PeerInfo>(Request::RemotePairWith { addr: addr.clone(), code: code.code }).await.unwrap_err();
    assert!(format!("{e:#}").contains("isn't pairing"), "the right code is refused too: {e:#}");
    let peers: Vec<PeerInfo> = tower.call(Request::RemotePeers).await.unwrap();
    assert!(peers.is_empty());
}
