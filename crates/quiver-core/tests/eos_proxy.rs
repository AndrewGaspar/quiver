//! The lab's EOS proxy (`quiver_core::lab::eosproxy`, design/LAB.md section 3.1) against a
//! local stand-in for `api.epicgames.dev` (its own test CA, on 127.0.0.1; nothing here talks
//! to Epic): allowed requests are forwarded, blocked ones get a 403 and never reach the
//! upstream, keep-alive and pipelining work, ALPN is http/1.1 only, other names and upgrades
//! are refused, a client that doesn't trust the lab CA fails closed, and the log says each.
//!
//! `filter_under_wine` (opt-in) does the same from a Windows client that trusts what Unreal's
//! SSL code trusts, the ROOT store (`tests/eos_proxy/client.c`), with the lab CA put in a
//! scratch prefix's registry by `lab::wincert`, for both architectures, and without it:
//!
//! ```sh
//! QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 \
//!   cargo test -p quiver-core --test eos_proxy -- --ignored --nocapture
//! ```
//!
//! Headless (no display), in a scratch prefix kept in `target/quiver-eos-proxy-wine`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use quiver_core::lab::eos;
use quiver_core::lab::eosproxy::{self, Ca, Config, Shared, Upstream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::{self, pki_types::ServerName};

/// The stand-in upstream: every request it receives ("METHOD target body"), answered 200.
struct Upstream0 {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    roots: rustls::RootCertStore,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

async fn upstream() -> Upstream0 {
    let ca = Ca::generate("test upstream").unwrap();
    let (chain, key) = ca.leaf().unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(chain[1].clone()).unwrap();
    let cfg = Arc::new(rustls::ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(chain, key).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (sock, _) = l.accept().await.unwrap();
            let (cfg, seen) = (cfg.clone(), s2.clone());
            tokio::spawn(async move {
                let Ok(tls) = tokio_rustls::TlsAcceptor::from(cfg).accept(sock).await else { return };
                let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let seen = seen.clone();
                    async move {
                        let line = format!("{} {} host={}", req.method(), req.uri(), req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or("-"));
                        let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                        seen.lock().unwrap().push(format!("{line} body={}", String::from_utf8_lossy(&body)));
                        let path = line.split(' ').nth(1).unwrap_or("?").to_owned();
                        Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from(format!("ok {path}")))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tls), svc).await;
            });
        }
    });
    Upstream0 { addr, seen, roots }
}

struct Proxy {
    port: u16,
    shared: Arc<Shared>,
    ca: Ca,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

async fn proxy(up: &Upstream0) -> Proxy {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::load_or_create(&dir.path().join("eos-proxy"), "t").unwrap();
    let log = dir.path().join("eos-filter.log");
    let shared = Shared::new(dir.path().join("before.log"));
    shared.start_run(log.clone(), "--- EOS proxy for run r1 (test)");
    let cfg = Config::new(&ca, Some(up.roots.clone()), Upstream::Fixed(up.addr)).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(eosproxy::serve(l, Arc::new(cfg), shared.clone()));
    Proxy { port, shared, ca, log, _dir: dir }
}

fn client_config(roots: rustls::RootCertStore, alpn: &[&str]) -> Arc<rustls::ClientConfig> {
    let mut c = rustls::ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth();
    c.alpn_protocols = alpn.iter().map(|a| a.as_bytes().to_vec()).collect();
    Arc::new(c)
}

fn lab_roots(p: &Proxy) -> rustls::RootCertStore {
    let mut r = rustls::RootCertStore::empty();
    r.add(p.ca.der.clone().into()).unwrap();
    r
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

async fn connect(p: &Proxy, name: &str, cfg: Arc<rustls::ClientConfig>) -> std::io::Result<Tls> {
    let tcp = TcpStream::connect(("127.0.0.1", p.port)).await?;
    tokio_rustls::TlsConnector::from(cfg).connect(ServerName::try_from(name.to_owned()).unwrap(), tcp).await
}

/// Read `n` responses (status line and body each).
async fn responses(tls: &mut Tls, n: usize) -> Vec<(u16, String)> {
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    while out.len() < n {
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
                    let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
                    buf.drain(..end + 4 + len);
                    out.push((status, body));
                    break;
                }
            }
            let k = tokio::time::timeout(Duration::from_secs(10), tls.read(&mut chunk)).await.expect("a response in time").unwrap();
            assert!(k > 0, "the connection closed after {} response(s)", out.len());
            buf.extend_from_slice(&chunk[..k]);
        }
    }
    out
}

async fn log_of(p: &Proxy, until: &str) -> String {
    for _ in 0..100 {
        let text = std::fs::read_to_string(&p.log).unwrap_or_default();
        if text.contains(until) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("no {until:?} in the log:\n{}", std::fs::read_to_string(&p.log).unwrap_or_default());
}

const H: &str = "Host: api.epicgames.dev\r\n";

#[tokio::test]
async fn allowed_go_through_blocked_never_leave() {
    let up = upstream().await;
    let p = proxy(&up).await;
    let mut tls = connect(&p, "api.epicgames.dev", client_config(lab_roots(&p), &["http/1.1"])).await.unwrap();
    tls.write_all(format!("POST /epic/oauth/v1/token HTTP/1.1\r\n{H}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: 19\r\n\r\ngrant_type=exchange").as_bytes()).await.unwrap();
    assert_eq!(responses(&mut tls, 1).await, [(200, "ok /epic/oauth/v1/token".to_owned())]);
    // Keep-alive: a cloud save upload on the same connection.
    tls.write_all(format!("PUT /playerdata/v1/dep/puid/data/save1.sav HTTP/1.1\r\n{H}Content-Length: 13\r\n\r\nSECRET-SAVE-1").as_bytes()).await.unwrap();
    let r = responses(&mut tls, 1).await;
    assert_eq!(r[0].0, 403);
    assert!(r[0].1.contains("errors.com.quiver.lab.eos_blocked"), "{r:?}");
    // And the connection carries on.
    tls.write_all(format!("GET /sdk/v1/default?platformId=WIN HTTP/1.1\r\n{H}\r\n").as_bytes()).await.unwrap();
    assert_eq!(responses(&mut tls, 1).await[0], (200, "ok /sdk/v1/default?platformId=WIN".into()));
    // A chunked body for an allowed request arrives whole.
    tls.write_all(format!("POST /auth/v1/oauth/token HTTP/1.1\r\n{H}Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").as_bytes()).await.unwrap();
    assert_eq!(responses(&mut tls, 1).await[0].0, 200);
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [
            "POST /epic/oauth/v1/token host=api.epicgames.dev body=grant_type=exchange",
            "GET /sdk/v1/default?platformId=WIN host=api.epicgames.dev body=",
            "POST /auth/v1/oauth/token host=api.epicgames.dev body=hello world",
        ]
    );
    let log = log_of(&p, "allow POST /auth/v1/oauth/token").await;
    for want in [
        "tls ok api.epicgames.dev conn#1",
        "allow POST /epic/oauth/v1/token -- Epic account sign-in (token) -> 200",
        "block PUT /playerdata/v1/dep/puid/data/save1.sav -- not on the allowlist (403)",
        "allow GET /sdk/v1/default?platformId=\u{2026} -- SDK config (default) -> 200",
    ] {
        assert!(log.contains(want), "no {want:?} in\n{log}");
    }
    for never in ["SECRET", "grant_type", "WIN", "hello"] {
        assert!(!log.contains(never), "{never:?} in the log:\n{log}");
    }
    let st = p.shared.stats();
    assert_eq!((st.tls_ok, st.allowed, st.blocked), (1, 3, 1));
    let s = eos::summarize(&log);
    assert!(s.verdict().contains("3 allowed, 1 blocked"), "{}", s.verdict());
}

#[tokio::test]
async fn pipelined_requests_answer_in_order() {
    let up = upstream().await;
    let p = proxy(&up).await;
    let mut tls = connect(&p, "api.epicgames.dev", client_config(lab_roots(&p), &["http/1.1"])).await.unwrap();
    let three = format!(
        "GET /sdk/v1/product/prod1 HTTP/1.1\r\n{H}\r\n\
         POST /achievements/v1/dep/achievements/puid/unlock HTTP/1.1\r\n{H}Content-Length: 12\r\n\r\nUNLOCK-ACH-1\
         GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=ns:item HTTP/1.1\r\n{H}\r\n"
    );
    tls.write_all(three.as_bytes()).await.unwrap();
    let r = responses(&mut tls, 3).await;
    assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), [200, 403, 200], "{r:?}");
    assert_eq!(r[2].1, "ok /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=ns:item");
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(!seen.iter().any(|s| s.contains("achievements") || s.contains("UNLOCK")));
}

#[tokio::test]
async fn alpn_is_http1_only() {
    let up = upstream().await;
    let p = proxy(&up).await;
    let tls = connect(&p, "api.epicgames.dev", client_config(lab_roots(&p), &["h2", "http/1.1"])).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    // Only h2: no protocol in common, no connection.
    assert!(connect(&p, "api.epicgames.dev", client_config(lab_roots(&p), &["h2"])).await.is_err());
    let log = log_of(&p, "tls failed").await;
    assert!(log.contains("h2 was offered, http/1.1 chosen"), "{log}");
    assert!(log.contains("no ALPN protocol the proxy speaks") && log.contains("(ALPN offered: h2)"), "{log}");
}

#[tokio::test]
async fn other_names_upgrades_and_untrusting_clients_get_nothing() {
    let up = upstream().await;
    let p = proxy(&up).await;
    // A sinkholed Epic name: refused at the handshake.
    assert!(connect(&p, "api-gamedev.ol.epicgames.net", client_config(lab_roots(&p), &[])).await.is_err());
    // A websocket upgrade on an allowed path.
    let mut tls = connect(&p, "api.epicgames.dev", client_config(lab_roots(&p), &[])).await.unwrap();
    tls.write_all(format!("GET /sdk/v1/default HTTP/1.1\r\n{H}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").as_bytes()).await.unwrap();
    assert_eq!(responses(&mut tls, 1).await[0].0, 403);
    // A client that doesn't trust the lab CA (here: the web PKI only, as with bundled roots).
    let webpki = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    assert!(connect(&p, "api.epicgames.dev", client_config(webpki, &[])).await.is_err());
    let log = log_of(&p, "rejected the lab's certificate").await;
    assert!(log.contains("refused api-gamedev.ol.epicgames.net conn#1 -- a sinkholed Epic host"), "{log}");
    assert!(log.contains("block GET /sdk/v1/default -- a protocol upgrade (websocket) (403)"), "{log}");
    assert!(log.contains("fail closed"), "{log}");
    assert!(up.seen.lock().unwrap().is_empty());
    assert_eq!(p.shared.stats().untrusted, 1);
    let s = eos::summarize(&log);
    assert_eq!((s.refused, s.untrusted, s.blocked), (1, 1, 1));
}

#[tokio::test]
async fn the_launch_probe() {
    let up = upstream().await;
    let p = proxy(&up).await;
    let mut tcp = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
    tcp.write_all(format!("{} n0nce\n", eosproxy::PROBE).as_bytes()).await.unwrap();
    let mut got = String::new();
    tcp.read_to_string(&mut got).await.unwrap();
    assert_eq!(got.trim(), eosproxy::PROBE_REPLY);
    assert!(p.shared.probed("n0nce") && !p.shared.probed("other"));
    // Anything else that isn't TLS is refused.
    let mut tcp = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
    tcp.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut rest = Vec::new();
    let _ = tcp.read_to_end(&mut rest).await;
    assert!(rest.is_empty());
    let log = log_of(&p, "-- not TLS").await;
    assert!(log.contains("probe ok"), "{log}");
}

/// The SDK checks certificates with OpenSSL (1.1.1, statically linked): OpenSSL must accept
/// the lab CA's chain for api.epicgames.dev (name constraints, key usage and all).
#[tokio::test]
async fn openssl_trusts_the_chain() {
    let up = upstream().await;
    let p = proxy(&up).await;
    let ca = p._dir.path().join("eos-proxy/ca.pem");
    let port = p.port;
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("openssl")
            .args(["s_client", "-connect", &format!("127.0.0.1:{port}"), "-servername", "api.epicgames.dev", "-verify_hostname", "api.epicgames.dev", "-verify_return_error", "-alpn", "h2,http/1.1", "-CAfile"])
            .arg(&ca)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("openssl")
    })
    .await
    .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("Verify return code: 0 (ok)"), "{text}");
    assert!(text.contains("ALPN protocol: http/1.1"), "{text}");
    assert!(!text.contains("verify error"), "{text}");
}

// --- the opt-in Wine test ---

fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

struct Wine {
    proton: PathBuf,
    prefix: PathBuf,
    home: PathBuf,
}

impl Wine {
    fn run(&self, exe: &Path, scenario: &Path) -> String {
        let out = std::process::Command::new(self.proton.join("files/bin/wine"))
            .arg(exe)
            .arg(win(scenario))
            .env("WINEPREFIX", &self.prefix)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .expect("wine");
        // wincert edits system.reg: only with no wineserver running.
        let _ = std::process::Command::new(self.proton.join("files/bin/wineserver")).arg("-w").env("WINEPREFIX", &self.prefix).status();
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
    }
}

fn results(out: &str) -> Vec<&str> {
    out.lines().filter(|l| ["roots", "connect", "write", "read", "close"].iter().any(|p| l.starts_with(p))).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs QUIVER_TEST_PROTON and mingw-w64; see the module docs"]
async fn filter_under_wine() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/quiver-eos-proxy-wine");
    std::fs::create_dir_all(keep.join("home")).unwrap();
    let wine = Wine { proton, prefix: keep.join("pfx"), home: keep.join("home") };
    let t = tempfile::tempdir().unwrap();
    let up = upstream().await;
    let p = proxy(&up).await;
    let ca_name = quiver_core::lab::wincert::CA_NAME;
    let mut exes = Vec::new();
    for arch in ["x86_64", "i686"] {
        let exe = t.path().join(format!("client-{arch}.exe"));
        let out = std::process::Command::new(format!("{arch}-w64-mingw32-gcc"))
            .args(["-std=gnu11", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&exe)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eos_proxy/client.c"))
            .args(["-lsecur32", "-lws2_32", "-lcrypt32"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        exes.push((arch, exe));
    }
    // A prefix to put the CA in (Wine makes it on the first run).
    let init = t.path().join("init.txt");
    std::fs::write(&init, format!("roots {ca_name}\n")).unwrap();
    wine.run(&exes[0].1, &init);
    let did = quiver_core::lab::wincert::install(&wine.prefix, &p.ca.der).unwrap();
    println!("wincert: {did:?}");

    let port = p.port;
    let scen = t.path().join("scenario.txt");
    std::fs::write(
        &scen,
        [
            format!("roots {ca_name}"),
            format!("connect {port} api.epicgames.dev"),
            r"write POST /epic/oauth/v1/token HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 2\r\n\r\nhi".into(),
            "read".into(),
            r"write PUT /playerdata/v1/dep/puid/data/save1.sav HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 13\r\n\r\nSECRET-SAVE-1".into(),
            "read".into(),
            // Pipelined in one write: an achievement and an ownership check.
            r"write POST /achievements/v1/dep/achievements/puid/unlock HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 12\r\n\r\nUNLOCK-ACH-1GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership HTTP/1.1\r\nHost: api.epicgames.dev\r\n\r\n".into(),
            "read".into(),
            "read".into(),
            format!("connect {port} api-gamedev.ol.epicgames.net"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    for (arch, exe) in &exes {
        up.seen.lock().unwrap().clear();
        let out = wine.run(exe, &scen);
        println!("--- {arch}:\n{out}");
        let r = results(&out);
        assert_eq!(r[0], "roots 1", "{arch}: the lab CA is in the ROOT store");
        assert_eq!(r[1], "connect ok verified", "{arch}: the proxy's certificate chains to it, for api.epicgames.dev");
        assert_eq!(r[2], "write ok");
        assert_eq!(r[3], "read HTTP/1.1 200 OK body=ok /epic/oauth/v1/token");
        assert_eq!(r[4], "write ok");
        assert!(r[5].starts_with("read HTTP/1.1 403 Forbidden body={\"errorCode\":\"errors.com.quiver.lab.eos_blocked\""), "{arch}: {}", r[5]);
        assert_eq!(r[6], "write ok");
        assert!(r[7].starts_with("read HTTP/1.1 403"), "{arch}: {}", r[7]);
        assert_eq!(r[8], "read HTTP/1.1 200 OK body=ok /epic/ecom/v1/platforms/EPIC/identities/abc/ownership");
        assert!(r[9].starts_with("connect handshake-failed"), "{arch}: {}", r[9]);
        let seen = up.seen.lock().unwrap().clone();
        assert_eq!(seen, ["POST /epic/oauth/v1/token host=api.epicgames.dev body=hi", "GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership host=api.epicgames.dev body="], "{arch}");
        // Wine's root sync (32-bit and 64-bit) kept the CA.
        assert!(quiver_core::lab::wincert::installed(&wine.prefix, &p.ca.der), "{arch}: Wine dropped the lab CA");
    }
    let log = std::fs::read_to_string(&p.log).unwrap();
    println!("--- the proxy's log:\n{log}");
    assert!(!log.contains("SECRET") && !log.contains("UNLOCK"));

    // Without the CA in the prefix: the same client doesn't trust the proxy (fail closed).
    assert!(quiver_core::lab::wincert::remove(&wine.prefix, &p.ca.der).unwrap());
    let ctl = t.path().join("control.txt");
    std::fs::write(&ctl, format!("roots {ca_name}\nconnect {port} api.epicgames.dev\n")).unwrap();
    let out = wine.run(&exes[0].1, &ctl);
    println!("--- without the CA:\n{out}");
    let r = results(&out);
    assert_eq!(r[0], "roots 0");
    assert!(r[1].starts_with("connect untrusted"), "{}", r[1]);
}
