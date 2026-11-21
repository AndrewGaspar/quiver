//! Moonlight compatibility in a scratch quiverd (design/REMOTE.md phase 1b): off by default,
//! on (on loopback, a scratch port), a pairing driven over HTTP the way Moonlight does it
//! with the PIN entered through the API, the paired client listed and unpaired, off again,
//! and on again after a restart. The scratch daemon has its own HOME, XDG dirs, socket, perf
//! shm root and broker port, mDNS off; nothing touches a real install or quiverd.
//!
//! No stream is started here (that needs the GPU and the real client: quiver-gamestream's
//! gated `moonlight_e2e`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quiver_api::moonlight::{MoonlightClient, MoonlightStatus};
use quiver_api::{Client, Event, Request};
use quiver_gamestream::crypto::{ecb_decrypt, ecb_encrypt};
use quiver_gamestream::pairing::{ClientCert, derive_key};
use serde_json::json;
use sha2::{Digest, Sha256};

const CLIENT_CERT: &str = include_str!("../../quiver-gamestream/tests/fixtures/client-cert.pem");
const CLIENT_KEY: &str = include_str!("../../quiver-gamestream/tests/fixtures/client-key.pem");

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Machine {
    root: PathBuf,
}

impl Machine {
    fn new(tmp: &Path, port: u16) -> Machine {
        let m = Machine { root: tmp.join("tower") };
        let data = m.dir("data/quiver");
        write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
        let cfg = json!({
            "library": m.dir("lib"), "prefixes": m.dir("pfx"), "proton": m.dir("proton"), "sdk_source": m.dir("sdk"),
            "save_backups": m.dir("backups"), "report_playtime": false, "steam_offers": false,
            "remote": { "moonlight": { "port": port } }
        });
        write(&data.join("config.json"), &cfg.to_string());
        write(&data.join("remote/machine.json"), &json!({ "name": "tower" }).to_string());
        m
    }

    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn sock(&self) -> PathBuf {
        self.dir("run/quiver/quiverd.sock")
    }

    fn daemon(&self) -> Daemon {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir("quiverd.log")).unwrap();
        let _ = std::fs::remove_file(self.sock());
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
            .env("RUST_LOG", "quiverd=info,quiver_gamestream=info")
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let d = Daemon(child);
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.sock()).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    async fn client(&self) -> Client {
        Client::connect_running_at(self.sock(), "cli").await.unwrap()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// An HTTP port whose whole GameStream layout (HTTPS −5, RTSP +21, UDP +9..+11) is free.
fn free_layout() -> u16 {
    for _ in 0..200 {
        let p = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        if !(1100..=65_000).contains(&p) {
            continue;
        }
        let tcp = [p - 5, p, p + 21].iter().all(|&q| std::net::TcpListener::bind(("127.0.0.1", q)).is_ok());
        let udp = [p + 9, p + 10, p + 11].iter().all(|&q| std::net::UdpSocket::bind(("127.0.0.1", q)).is_ok());
        if tcp && udp {
            return p;
        }
    }
    panic!("no free port layout");
}

/// One plain-HTTP GET, as Moonlight makes it; the body.
fn get(port: u16, path_and_query: &str) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(s, "GET {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default()
}

fn field(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let start = xml.find(&open).unwrap_or_else(|| panic!("no <{tag}> in {xml}")) + open.len();
    let end = xml[start..].find('<').unwrap() + start;
    xml[start..end].to_owned()
}

fn sign(data: &[u8]) -> Vec<u8> {
    let der = quiver_gamestream::identity::pem_body(CLIENT_KEY, "PRIVATE KEY").unwrap();
    let key = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), data, &mut sig).unwrap();
    sig
}

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
async fn moonlight_on_pair_unpair_off_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_layout();
    let m = Machine::new(tmp.path(), port);
    let mut daemon = m.daemon();
    let q = m.client().await;
    let mut events = q.subscribe().await.unwrap();

    // Off by default: nothing listens.
    let s: MoonlightStatus = q.call(Request::MoonlightStatus).await.unwrap();
    assert!(!s.enabled && s.listening.is_empty(), "{s:?}");
    assert_eq!(s.ports.iter().map(|p| p.port).collect::<Vec<_>>(), [port - 5, port, port + 21, port + 9, port + 10, port + 11]);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());

    // A bind that isn't an address or interface is refused, and nothing is remembered.
    let e = q.call::<MoonlightStatus>(Request::MoonlightEnable { bind: Some(vec!["no-such-if0".into()]), codecs: None, mdns: Some(false) }).await.unwrap_err();
    assert!(format!("{e:#}").contains("network interface"), "{e:#}");
    // On loopback only (as `tailscale0` would be), H.264 and HEVC.
    let s: MoonlightStatus = q
        .call(Request::MoonlightEnable { bind: Some(vec!["127.0.0.1".into()]), codecs: Some(vec!["hevc".into()]), mdns: Some(false) })
        .await
        .unwrap();
    assert_eq!((s.enabled, s.listening.as_slice(), s.codecs.as_slice()), (true, &["127.0.0.1".to_owned()][..], &["h264".to_owned(), "hevc".to_owned()][..]));
    let cfg = std::fs::read_to_string(m.dir("data/quiver/config.json")).unwrap();
    assert!(cfg.contains("\"moonlight\"") && cfg.contains("127.0.0.1"), "remembered: {cfg}");
    event(&mut events, "MoonlightChanged", |e| matches!(e, Event::MoonlightChanged)).await;

    // What Moonlight sees first.
    let info = get(port, "/serverinfo?uniqueid=0123456789ABCDEF&uuid=1");
    assert_eq!((field(&info, "hostname").as_str(), field(&info, "HttpsPort")), ("tower", (port - 5).to_string()));
    assert_eq!(field(&info, "ServerCodecModeSupport"), (0x1 | 0x100).to_string());

    // Pairing, Moonlight's side by hand: stage 1 waits for the PIN, entered through the API.
    let salt = [0x42u8; 16];
    let pin = "5172";
    let cert_hex = hex::encode(CLIENT_CERT);
    let stage1 = std::thread::spawn(move || {
        get(port, &format!("/pair?uniqueid=ABC&uuid=2&devicename=roth&updateState=1&phrase=getservercert&salt={}&clientcert={cert_hex}", hex::encode(salt)))
    });
    let e = event(&mut events, "MoonlightPairingRequested", |e| matches!(e, Event::MoonlightPairingRequested { .. })).await;
    let Event::MoonlightPairingRequested { device_name, address } = e else { unreachable!() };
    assert_eq!((device_name.as_str(), address.as_str()), ("roth", "127.0.0.1"));
    let s: MoonlightStatus = q.call(Request::MoonlightStatus).await.unwrap();
    assert_eq!(s.pending.as_ref().map(|p| p.device_name.as_str()), Some("roth"));
    let e = q.call::<MoonlightClient>(Request::MoonlightPin { pin: "12".into() }).await.unwrap_err();
    assert!(format!("{e:#}").contains("4 digits"), "{e:#}");
    let q2 = m.client().await;
    let pin_call = tokio::spawn(async move { q2.call::<MoonlightClient>(Request::MoonlightPin { pin: pin.into() }).await });

    let r1 = tokio::task::spawn_blocking(move || stage1.join().unwrap()).await.unwrap();
    assert_eq!(field(&r1, "paired"), "1", "{r1}");
    let server_cert = String::from_utf8(hex::decode(field(&r1, "plaincert")).unwrap()).unwrap();
    let server_cert = ClientCert::from_pem(&server_cert).unwrap();
    let key = derive_key(&salt, pin);
    let challenge = [7u8; 16];
    let r2 = tokio::task::spawn_blocking(move || get(port, &format!("/pair?uniqueid=ABC&uuid=3&devicename=roth&updateState=1&clientchallenge={}", hex::encode(ecb_encrypt(&key, &challenge).unwrap()))))
        .await
        .unwrap();
    let resp = ecb_decrypt(&key, &hex::decode(field(&r2, "challengeresponse")).unwrap()).unwrap();
    let (server_hash, server_challenge) = resp.split_at(32);
    let me = ClientCert::from_pem(CLIENT_CERT).unwrap();
    let secret = [9u8; 16];
    let ours: Vec<u8> = Sha256::new().chain_update(server_challenge).chain_update(&me.signature).chain_update(secret).finalize().to_vec();
    let r3 = tokio::task::spawn_blocking(move || get(port, &format!("/pair?uniqueid=ABC&uuid=4&devicename=roth&updateState=1&serverchallengeresp={}", hex::encode(ecb_encrypt(&key, &ours).unwrap()))))
        .await
        .unwrap();
    let ps = hex::decode(field(&r3, "pairingsecret")).unwrap();
    let (server_secret, server_sig) = ps.split_at(16);
    assert!(server_cert.verify(server_secret, server_sig), "the server's signature");
    let expect: Vec<u8> = Sha256::new().chain_update(challenge).chain_update(&server_cert.signature).chain_update(server_secret).finalize().to_vec();
    assert_eq!(expect, server_hash, "the server knew the PIN");
    let mut cps = secret.to_vec();
    cps.extend_from_slice(&sign(&secret));
    let r4 = tokio::task::spawn_blocking(move || get(port, &format!("/pair?uniqueid=ABC&uuid=5&devicename=roth&updateState=1&clientpairingsecret={}", hex::encode(cps)))).await.unwrap();
    assert_eq!(field(&r4, "paired"), "1", "{r4}");

    let paired = pin_call.await.unwrap().unwrap();
    assert_eq!((paired.name.as_str(), paired.fingerprint.as_str()), ("roth", me.fingerprint().as_str()));
    event(&mut events, "MoonlightPaired", |e| matches!(e, Event::MoonlightPaired { .. })).await;
    let clients: Vec<MoonlightClient> = q.call(Request::MoonlightClients).await.unwrap();
    assert_eq!(clients.len(), 1);
    let file = m.dir("data/quiver/remote/moonlight-clients.json");
    assert!(std::fs::read_to_string(&file).unwrap().contains(&paired.fingerprint), "pinned in its own file");
    assert!(!m.dir("data/quiver/remote/peers.json").exists() || !std::fs::read_to_string(m.dir("data/quiver/remote/peers.json")).unwrap().contains(&paired.fingerprint));

    // Unpair by a fingerprint prefix.
    let gone: MoonlightClient = q.call(Request::MoonlightUnpair { fingerprint: paired.fingerprint[..8].into() }).await.unwrap();
    assert_eq!(gone.fingerprint, paired.fingerprint);
    let clients: Vec<MoonlightClient> = q.call(Request::MoonlightClients).await.unwrap();
    assert!(clients.is_empty());

    // Off: nothing listens. On again, then a restart: it comes back on by itself.
    let () = q.call(Request::MoonlightDisable).await.unwrap();
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    let _: MoonlightStatus = q.call(Request::MoonlightEnable { bind: None, codecs: None, mdns: None }).await.unwrap();
    drop(q);
    drop(events);
    drop(daemon);
    daemon = m.daemon();
    let q = m.client().await;
    let t = Instant::now();
    loop {
        let s: MoonlightStatus = q.call(Request::MoonlightStatus).await.unwrap();
        if !s.listening.is_empty() {
            assert_eq!(s.bind, ["127.0.0.1"], "kept");
            assert_eq!(s.codecs, ["h264", "hevc"], "kept");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "not listening after a restart: {s:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(get(port, "/serverinfo").contains("<hostname>tower</hostname>"));
    let () = q.call(Request::MoonlightDisable).await.unwrap();
    drop(daemon);
}

/// Moonlight finds the server on the LAN by itself (`_nvstream._tcp` over mDNS), and not once
/// it's turned off. Runs in its own user and network namespace (re-running this test there):
/// only loopback, with multicast on, so nothing is announced on a real network.
#[tokio::test(flavor = "multi_thread")]
async fn moonlight_mdns_in_a_network_namespace() {
    const IN_NS: &str = "QUIVER_TEST_MOONLIGHT_NS";
    if std::env::var(IN_NS).is_err() {
        if Command::new("unshare").arg("--version").output().is_err() {
            return eprintln!("skipped: no unshare");
        }
        let out = Command::new("unshare")
            .args(["--user", "--map-root-user", "--net"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "moonlight_mdns_in_a_network_namespace", "--nocapture", "--test-threads=1"])
            .env(IN_NS, "1")
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if text.contains("Operation not permitted") && !out.status.success() {
            return eprintln!("skipped: no unprivileged namespaces: {text}");
        }
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        return;
    }
    // Inside: loopback up, with multicast.
    for args in [["link", "set", "lo", "up"], ["link", "set", "lo", "multicast"]] {
        let mut a: Vec<&str> = args.to_vec();
        if a[3] == "multicast" {
            a.push("on");
        }
        assert!(Command::new("ip").args(&a).status().unwrap().success());
    }
    let tmp = tempfile::tempdir().unwrap();
    let port = free_layout();
    let m = Machine::new(tmp.path(), port);
    let daemon = m.daemon();
    let q = m.client().await;
    let s: MoonlightStatus = q.call(Request::MoonlightEnable { bind: Some(vec!["127.0.0.1".into()]), codecs: None, mdns: None }).await.unwrap();
    assert!(s.mdns && s.advertised_on == ["lo"], "{s:?}");
    let browse = || tokio::task::spawn_blocking(|| quiver_remote::discovery::browse_service("_nvstream._tcp.local.", Duration::from_secs(3)).unwrap());
    let found = browse().await.unwrap();
    let ours = found.iter().find(|f| f.port == port).unwrap_or_else(|| panic!("not found: {found:?}"));
    assert!(ours.fullname.starts_with("tower._nvstream"), "{ours:?}");
    assert!(ours.addrs.contains(&"127.0.0.1".parse().unwrap()), "{ours:?}");

    // Advertising off: gone (the browse is fresh, so nothing cached answers).
    let s: MoonlightStatus = q.call(Request::MoonlightEnable { bind: None, codecs: None, mdns: Some(false) }).await.unwrap();
    assert!(!s.mdns && s.advertised_on.is_empty(), "{s:?}");
    let cfg: String = std::fs::read_to_string(m.dir("data/quiver/config.json")).unwrap().split_whitespace().collect();
    assert!(cfg.contains("\"mdns\":false"), "remembered: {cfg}");
    let found = browse().await.unwrap();
    assert!(!found.iter().any(|f| f.port == port), "still advertised: {found:?}");
    let () = q.call(Request::MoonlightDisable).await.unwrap();
    drop(daemon);
}

/// The real Moonlight client against a scratch quiverd: turned on and the PIN entered with the
/// CLI, then pair, list, stream quiverd's "Test pattern" (the stream host's NVENC pattern)
/// twice and quit, in quiver-gamestream's isolated namespace (`moonlight-e2e.sh`).
///
/// Gated like quiver-gamestream's: `QUIVER_E2E_MOONLIGHT=1`, and skipped without gamescope,
/// moonlight or the `quiver` CLI built next to quiverd (`cargo build -p quiver-cli`).
#[test]
fn moonlight_e2e_against_quiverd() {
    if std::env::var("QUIVER_E2E_MOONLIGHT").as_deref() != Ok("1") {
        return eprintln!("skipped: set QUIVER_E2E_MOONLIGHT=1");
    }
    let quiverd = PathBuf::from(env!("CARGO_BIN_EXE_quiverd"));
    let cli = quiverd.with_file_name("quiver");
    let have = |t: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(t).is_file()));
    if !cli.is_file() || !["gamescope", "moonlight", "unshare", "xprop", "prlimit", "ip"].iter().all(|t| have(t)) {
        return eprintln!("skipped: needs gamescope, moonlight and {}", cli.display());
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("moonlight-e2e-quiverd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../quiver-gamestream/tests/moonlight-e2e.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env("GS_QUIVERD", &quiverd)
        .env("GS_CLI", &cli)
        .env("GS_SERVER", "unused")
        .env("GS_STATE", &dir)
        .output()
        .unwrap();
    let steps = String::from_utf8_lossy(&out.stdout);
    let raw = std::fs::read_to_string(dir.join("server.log")).unwrap_or_default();
    // Without the log's colours.
    let mut log = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            let _ = it.by_ref().find(|c| c.is_ascii_alphabetic());
        } else {
            log.push(c);
        }
    }
    for step in ["STEP server ready", "STEP paired", "STEP listed", "STEP streamed stream-1392", "STEP streamed stream-auto", "STEP quit", "STEP done"] {
        assert!(steps.contains(step), "missing {step:?}\n{steps}\n{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(log.matches("control stream connected").count(), 2, "{log}");
    assert!(log.contains("advertised on mDNS"), "{log}");
    // Constant-bitrate audio: every 4-packet block had FEC.
    assert!(log.lines().filter(|l| l.contains("audio totals")).all(|l| l.contains("uneven_blocks=0")), "{log}");
    assert!(log.lines().filter(|l| l.contains("video totals")).all(|l| l.contains("send_errors=0")), "{log}");
}
