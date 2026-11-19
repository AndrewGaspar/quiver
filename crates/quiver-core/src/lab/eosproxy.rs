//! The lab's EOS proxy (design/LAB.md section 3.1): where an instance's `api.epicgames.dev`
//! goes. It runs in the instance's supervisor, on the host's loopback; pasta forwards the
//! container's port 443 to it, and the container resolves Epic's names to 127.0.0.1
//! ([`super::podman`]). Per connection:
//!
//! - a line `QUIVER-LAB-PROBE <nonce>` instead of TLS is the launch's check that the redirect
//!   works from inside the container (answered `QUIVER-LAB-PROXY ok`);
//! - TLS for `api.epicgames.dev` only, with a certificate from the instance's own CA ([`Ca`],
//!   trusted only in the instance's cloned prefix, [`super::wincert`]); any other name (Epic's
//!   sinkholed hosts) is refused. ALPN offers `http/1.1` only;
//! - HTTP/1.1 (keep-alive and pipelining by hyper): each request against
//!   [`super::eos::ALLOWLIST`]. Allowed ones go to the real `api.epicgames.dev` (resolved on the
//!   host, never to a local address, verified against the web PKI), one upstream connection per
//!   game connection; everything else, protocol upgrades (websockets) included, gets a 403 and
//!   never leaves the proxy.
//!
//! Every handshake and decision is a line in the current run's `eos-filter.log` (method, path,
//! the query's keys; never headers, bodies or query values). If the game doesn't trust the
//! lab's CA (bundled roots only, or a pinned key), its TLS fails and EOS is offline for the
//! run: fail closed, and the log says so.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper::{Request, Response, StatusCode, Uri, header};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio_rustls::rustls::{self, AlertDescription};

use super::eos::{self, FORWARDED_HOST};
use super::wincert::CA_NAME;

/// What the launch's probe sends, and what the proxy answers.
pub const PROBE: &str = "QUIVER-LAB-PROBE";
pub const PROBE_REPLY: &str = "QUIVER-LAB-PROXY ok";

/// The instance's CA: made once per instance, kept in `<instance>/eos-proxy/` (not in the
/// mounted lab folder, so nothing in the container can read its key), and trusted only in
/// the instance's cloned prefix. It may only sign `api.epicgames.dev` (a name constraint).
#[derive(Debug, Clone)]
pub struct Ca {
    pub cert_pem: String,
    key_pem: String,
    pub der: Vec<u8>,
}

/// A critical KeyUsage extension as a one-byte bit string. rcgen always writes nine bits (two
/// bytes); Wine's chain engine reads only the last byte (dlls/crypt32/chain.c,
/// `CRYPT_KeyUsageValid`), so it never saw keyCertSign and rejected the chain
/// (`CERT_E_WRONG_USAGE`). `bits`: the first byte (0x80 digitalSignature, 0x04 keyCertSign,
/// 0x02 cRLSign); `unused`: trailing unused bits.
fn key_usage(bits: u8, unused: u8) -> rcgen::CustomExtension {
    let mut e = rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 15], vec![0x03, 0x02, unused, bits]);
    e.set_criticality(true);
    e
}

fn now_minus_a_day() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() - time::Duration::days(1)
}

impl Ca {
    pub fn generate(instance: &str) -> Result<Ca> {
        use rcgen::{BasicConstraints, CertificateParams, DnType, GeneralSubtree, IsCa, KeyPair, NameConstraints};
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.distinguished_name.push(DnType::CommonName, format!("{CA_NAME} ({instance})"));
        p.distinguished_name.push(DnType::OrganizationName, "Quiver lab: this instance's prefix only");
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        // digitalSignature, keyCertSign, cRLSign (bits 0, 5, 6; one unused bit).
        p.custom_extensions = vec![key_usage(0x86, 1)];
        p.name_constraints = Some(NameConstraints { permitted_subtrees: vec![GeneralSubtree::DnsName(FORWARDED_HOST.into())], excluded_subtrees: vec![] });
        p.not_before = now_minus_a_day();
        p.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3650);
        let key = KeyPair::generate()?;
        let cert = p.self_signed(&key)?;
        Ok(Ca { cert_pem: cert.pem(), key_pem: key.serialize_pem(), der: cert.der().to_vec() })
    }

    /// The CA in `dir` (`ca.pem`, `ca.key`), made if missing.
    pub fn load_or_create(dir: &Path, instance: &str) -> Result<Ca> {
        let (cert, key) = (dir.join("ca.pem"), dir.join("ca.key"));
        if let (Ok(cert_pem), Ok(key_pem)) = (std::fs::read_to_string(&cert), std::fs::read_to_string(&key)) {
            let parsed = pem_der(&cert_pem).context("the instance's EOS proxy CA")?;
            rcgen::KeyPair::from_pem(&key_pem).context("the instance's EOS proxy CA key")?;
            return Ok(Ca { cert_pem, key_pem, der: parsed });
        }
        let ca = Self::generate(instance)?;
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        crate::private::write_private(&key, ca.key_pem.as_bytes())?;
        std::fs::write(&cert, &ca.cert_pem)?;
        Ok(ca)
    }

    /// A fresh certificate for `api.epicgames.dev` from this CA: the chain and its key.
    pub fn leaf(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair};
        let ca_key = KeyPair::from_pem(&self.key_pem)?;
        let issuer = CertificateParams::from_ca_cert_pem(&self.cert_pem)?.self_signed(&ca_key)?;
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(vec![FORWARDED_HOST.to_owned()])?;
        p.distinguished_name.push(DnType::CommonName, FORWARDED_HOST);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        // digitalSignature only (bit 0; seven unused bits).
        p.custom_extensions = vec![key_usage(0x80, 7)];
        p.use_authority_key_identifier_extension = true;
        p.not_before = now_minus_a_day();
        p.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);
        let cert = p.signed_by(&key, &issuer, &ca_key)?;
        Ok((vec![cert.der().clone(), CertificateDer::from(self.der.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()))))
    }
}

fn pem_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(body.trim())?)
}

/// Where allowed requests go.
#[derive(Debug, Clone)]
pub enum Upstream {
    /// The real `api.epicgames.dev`: resolved on the host, port 443.
    Dns,
    /// A fixed address (tests: a local stand-in, never Epic).
    Fixed(SocketAddr),
}

pub struct Config {
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    upstream: Upstream,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl Config {
    /// `roots`: what the upstream must chain to (default: the web PKI).
    pub fn new(ca: &Ca, roots: Option<rustls::RootCertStore>, upstream: Upstream) -> Result<Config> {
        let (chain, key) = ca.leaf()?;
        let mut server = rustls::ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions()?.with_no_client_auth().with_single_cert(chain, key)?;
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let roots = roots.unwrap_or_else(|| rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() });
        let mut client = rustls::ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions()?.with_root_certificates(roots).with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Config { server: Arc::new(server), client: Arc::new(client), upstream })
    }
}

/// Counts since the last [`Shared::reset`] (a launch).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub probes: u64,
    pub connections: u64,
    pub tls_ok: u64,
    pub tls_failed: u64,
    pub untrusted: u64,
    pub refused: u64,
    pub allowed: u64,
    pub blocked: u64,
    pub upstream_errors: u64,
}

#[derive(Default)]
struct Counters {
    probes: AtomicU64,
    connections: AtomicU64,
    tls_ok: AtomicU64,
    tls_failed: AtomicU64,
    untrusted: AtomicU64,
    refused: AtomicU64,
    allowed: AtomicU64,
    blocked: AtomicU64,
    upstream_errors: AtomicU64,
}

/// What the proxy and the supervisor share: the log it writes to, the counts, the probe
/// nonces it answered, and the real host's addresses (to recognise bypasses).
pub struct Shared {
    log: Mutex<PathBuf>,
    counters: Counters,
    conn_ids: AtomicU64,
    probes: Mutex<Vec<String>>,
    epic_addrs: Mutex<BTreeSet<IpAddr>>,
}

fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

impl Shared {
    pub fn new(log: PathBuf) -> Arc<Shared> {
        Arc::new(Shared { log: Mutex::new(log), counters: Counters::default(), conn_ids: AtomicU64::new(0), probes: Mutex::new(Vec::new()), epic_addrs: Mutex::new(BTreeSet::new()) })
    }

    /// Log to this file from now on (a launch's run folder), with the counts reset.
    pub fn start_run(&self, log: PathBuf, header: &str) {
        *self.log.lock().unwrap() = log;
        let c = &self.counters;
        for a in [&c.probes, &c.connections, &c.tls_ok, &c.tls_failed, &c.untrusted, &c.refused, &c.allowed, &c.blocked, &c.upstream_errors] {
            a.store(0, Ordering::Relaxed);
        }
        self.log(header);
    }

    pub fn log(&self, line: &str) {
        let path = self.log.lock().unwrap().clone();
        let text = format!("{} {line}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = f.write_all(text.as_bytes());
        }
    }

    pub fn stats(&self) -> Stats {
        let c = &self.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Stats {
            probes: g(&c.probes),
            connections: g(&c.connections),
            tls_ok: g(&c.tls_ok),
            tls_failed: g(&c.tls_failed),
            untrusted: g(&c.untrusted),
            refused: g(&c.refused),
            allowed: g(&c.allowed),
            blocked: g(&c.blocked),
            upstream_errors: g(&c.upstream_errors),
        }
    }

    /// Did a probe with this nonce reach the proxy?
    pub fn probed(&self, nonce: &str) -> bool {
        self.probes.lock().unwrap().iter().any(|n| n == nonce)
    }

    /// `api.epicgames.dev`'s addresses as the host resolves them (what a bypass would go to).
    pub fn epic_addrs(&self) -> BTreeSet<IpAddr> {
        self.epic_addrs.lock().unwrap().clone()
    }
}

/// A local address: a resolution that would make the proxy forward to itself.
fn local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback() || a.is_unspecified() || a.is_private() || a.is_link_local(),
        IpAddr::V6(a) => a.is_loopback() || a.is_unspecified() || (a.segments()[0] & 0xfe00) == 0xfc00 || (a.segments()[0] & 0xffc0) == 0xfe80 || a.to_ipv4_mapped().is_some_and(|v| local(IpAddr::V4(v))),
    }
}

/// The real host's addresses (and remember them for bypass checks).
pub async fn resolve_epic(shared: &Shared) -> Result<Vec<SocketAddr>> {
    let all: Vec<SocketAddr> = tokio::net::lookup_host((FORWARDED_HOST, 443)).await.with_context(|| format!("resolve {FORWARDED_HOST}"))?.collect();
    let real: Vec<SocketAddr> = all.iter().copied().filter(|a| !local(a.ip())).collect();
    if real.is_empty() {
        bail!("{FORWARDED_HOST} resolves only to local addresses on this host ({all:?}); refusing to forward there (it would loop back)");
    }
    shared.epic_addrs.lock().unwrap().extend(real.iter().map(|a| a.ip()));
    Ok(real)
}

/// A listener for the proxy on the host's loopback: `prefer`'s port if it's free, else any.
pub fn listen(prefer: Option<u16>) -> Result<std::net::TcpListener> {
    if let Some(p) = prefer
        && let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", p))
    {
        return Ok(l);
    }
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?)
}

/// Serve on `listener` in a thread of its own (a small runtime), for the supervisor.
pub fn spawn(listener: std::net::TcpListener, cfg: Config, shared: Arc<Shared>) -> Result<()> {
    listener.set_nonblocking(true)?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("eos-proxy").enable_all().build()?;
    std::thread::Builder::new().name("eos-proxy".into()).spawn(move || {
        rt.block_on(async move {
            match TcpListener::from_std(listener) {
                Ok(l) => serve(l, Arc::new(cfg), shared).await,
                Err(e) => shared.log(&format!("--- EOS proxy couldn't listen: {e}")),
            }
        })
    })?;
    Ok(())
}

/// Accept connections until the runtime ends.
pub async fn serve(listener: TcpListener, cfg: Arc<Config>, shared: Arc<Shared>) {
    if matches!(cfg.upstream, Upstream::Dns) {
        let s = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = resolve_epic(&s).await {
                s.log(&format!("--- {e:#}"));
            }
        });
    }
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let (cfg, shared) = (cfg.clone(), shared.clone());
        tokio::spawn(async move { connection(sock, cfg, shared).await });
    }
}

/// Why a handshake failed, for the log; and whether the game rejected our certificate.
fn handshake_error(e: &std::io::Error) -> (String, bool) {
    if let Some(r) = e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
        return match r {
            rustls::Error::AlertReceived(
                a @ (AlertDescription::UnknownCA
                | AlertDescription::BadCertificate
                | AlertDescription::CertificateUnknown
                | AlertDescription::UnsupportedCertificate
                | AlertDescription::CertificateExpired
                | AlertDescription::AccessDenied
                | AlertDescription::DecryptError),
            ) => (format!("the game rejected the lab's certificate (alert {a:?}): it doesn't trust the lab's CA (bundled roots only, or a pinned key), so EOS is offline for this run (fail closed)"), true),
            rustls::Error::AlertReceived(a) => (format!("the game ended the handshake (alert {a:?})"), false),
            rustls::Error::NoApplicationProtocol => ("the game offered no ALPN protocol the proxy speaks (it speaks http/1.1 only)".into(), false),
            other => (format!("{other}"), false),
        };
    }
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        return ("the game closed the connection during the handshake (often: it doesn't trust the lab's certificate); EOS is offline for this run (fail closed)".into(), true);
    }
    (e.to_string(), false)
}

async fn connection(mut sock: TcpStream, cfg: Arc<Config>, shared: Arc<Shared>) {
    let id = shared.conn_ids.fetch_add(1, Ordering::Relaxed) + 1;
    let mut first = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(30), sock.peek(&mut first)).await {
        Ok(Ok(1)) => {}
        _ => return,
    }
    if first[0] != 0x16 {
        // Not TLS: the launch's probe, or something to refuse.
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while buf.len() < 128 && !buf.ends_with(b"\n") {
            match tokio::time::timeout(Duration::from_secs(3), sock.read(&mut byte)).await {
                Ok(Ok(1)) => buf.push(byte[0]),
                _ => break,
            }
        }
        let line = String::from_utf8_lossy(&buf).trim().to_owned();
        if let Some(nonce) = line.strip_prefix(PROBE).map(str::trim).filter(|n| !n.is_empty() && n.len() <= 64) {
            shared.probes.lock().unwrap().push(nonce.to_owned());
            inc(&shared.counters.probes);
            shared.log("probe ok (the container's api.epicgames.dev:443 reaches this proxy)");
            let _ = sock.write_all(format!("{PROBE_REPLY}\n").as_bytes()).await;
        } else {
            inc(&shared.counters.refused);
            shared.log(&format!("refused conn#{id} -- not TLS"));
        }
        return;
    }
    inc(&shared.counters.connections);
    let start = match tokio::time::timeout(Duration::from_secs(20), tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), sock)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            inc(&shared.counters.tls_failed);
            shared.log(&format!("tls failed conn#{id} -- not a TLS ClientHello ({e})"));
            return;
        }
        Err(_) => return,
    };
    let hello = start.client_hello();
    let sni = hello.server_name().map(str::to_ascii_lowercase);
    let alpn: Vec<String> = hello.alpn().map(|a| a.map(|p| String::from_utf8_lossy(p).into_owned()).collect()).unwrap_or_default();
    if sni.as_deref() != Some(FORWARDED_HOST) {
        inc(&shared.counters.refused);
        let what = sni.as_deref().unwrap_or("(no server name)");
        let why = if eos::SINKHOLED_HOSTS.contains(&what) { "a sinkholed Epic host" } else { "only api.epicgames.dev goes through" };
        shared.log(&format!("refused {what} conn#{id} -- {why}"));
        return;
    }
    let tls = match tokio::time::timeout(Duration::from_secs(20), start.into_stream(cfg.server.clone())).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            let (why, untrusted) = handshake_error(&e);
            inc(&shared.counters.tls_failed);
            if untrusted {
                inc(&shared.counters.untrusted);
            }
            let offered = if alpn.is_empty() { String::new() } else { format!(" (ALPN offered: {})", alpn.join(", ")) };
            shared.log(&format!("tls failed {FORWARDED_HOST} conn#{id} -- {why}{offered}"));
            return;
        }
        Err(_) => {
            inc(&shared.counters.tls_failed);
            shared.log(&format!("tls failed {FORWARDED_HOST} conn#{id} -- the handshake timed out"));
            return;
        }
    };
    let proto = tls.get_ref().1.alpn_protocol().map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_else(|| "no ALPN".into());
    let version = tls.get_ref().1.protocol_version().map(|v| format!("{v:?}")).unwrap_or_default();
    inc(&shared.counters.tls_ok);
    shared.log(&format!("tls ok {FORWARDED_HOST} conn#{id} ({version}, {proto}{})", if alpn.iter().any(|a| a == "h2") { "; h2 was offered, http/1.1 chosen" } else { "" }));
    let requests = Arc::new(AtomicU64::new(0));
    let upstream: Arc<tokio::sync::Mutex<Option<SendRequest<Incoming>>>> = Arc::new(tokio::sync::Mutex::new(None));
    let svc = {
        let (cfg, shared, requests) = (cfg.clone(), shared.clone(), requests.clone());
        hyper::service::service_fn(move |req| {
            let (cfg, shared, requests, upstream) = (cfg.clone(), shared.clone(), requests.clone(), upstream.clone());
            async move {
                requests.fetch_add(1, Ordering::Relaxed);
                Ok::<_, Infallible>(request(req, &cfg, &shared, &upstream).await)
            }
        })
    };
    let served = hyper::server::conn::http1::Builder::new().preserve_header_case(true).pipeline_flush(true).serve_connection(TokioIo::new(tls), svc).await;
    let n = requests.load(Ordering::Relaxed);
    if n == 0 {
        shared.log(&format!("closed conn#{id} after the handshake without a request (an SDK that pins Epic's key gives up here; see the game's log){}", served.err().map(|e| format!(": {e}")).unwrap_or_default()));
    } else {
        shared.log(&format!("closed conn#{id} after {n} request(s)"));
    }
}

type Body = BoxBody<Bytes, hyper::Error>;

/// Epic's error shape, so the SDK reports an ordinary service error.
fn blocked_response() -> Response<Body> {
    let body = r#"{"errorCode":"errors.com.quiver.lab.eos_blocked","errorMessage":"Blocked by the Quiver lab's EOS proxy: a lab instance never reports to Epic","numericErrorCode":403,"originatingService":"quiver-lab","intent":"prod"}"#;
    let mut r = Response::new(Full::new(Bytes::from_static(body.as_bytes())).map_err(|n: Infallible| match n {}).boxed());
    *r.status_mut() = StatusCode::FORBIDDEN;
    r.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    r
}

fn gateway_error() -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from_static(b"{\"errorCode\":\"errors.com.quiver.lab.upstream\",\"numericErrorCode\":502}")).map_err(|n: Infallible| match n {}).boxed());
    *r.status_mut() = StatusCode::BAD_GATEWAY;
    r
}

const HOP_BY_HOP: &[header::HeaderName] = &[header::CONNECTION, header::PROXY_AUTHENTICATE, header::PROXY_AUTHORIZATION, header::TE, header::TRAILER, header::TRANSFER_ENCODING, header::UPGRADE];

fn strip_hop_by_hop(h: &mut header::HeaderMap) {
    let named: Vec<header::HeaderName> = h.get_all(header::CONNECTION).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).filter_map(|n| header::HeaderName::from_bytes(n.trim().as_bytes()).ok()).collect();
    for n in HOP_BY_HOP.iter().chain(&named) {
        h.remove(n);
    }
    h.remove("keep-alive");
    h.remove("proxy-connection");
}

async fn request(req: Request<Incoming>, cfg: &Config, shared: &Shared, upstream: &tokio::sync::Mutex<Option<SendRequest<Incoming>>>) -> Response<Body> {
    let method = req.method().as_str().to_owned();
    let target = req.uri().path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_else(|| req.uri().to_string());
    let shown = eos::shown_target(&target);
    let upgrade = req.headers().contains_key(header::UPGRADE)
        || method == "CONNECT"
        || req.headers().get_all(header::CONNECTION).iter().any(|v| v.to_str().is_ok_and(|v| v.to_ascii_lowercase().contains("upgrade")));
    if upgrade {
        inc(&shared.counters.blocked);
        shared.log(&format!("block {method} {shown} -- a protocol upgrade (websocket) (403)"));
        return blocked_response();
    }
    let Some(rule) = eos::rule(&method, req.uri().path()) else {
        inc(&shared.counters.blocked);
        shared.log(&format!("block {method} {shown} -- not on the allowlist (403)"));
        return blocked_response();
    };
    let (mut parts, body) = req.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.insert(header::HOST, header::HeaderValue::from_static(FORWARDED_HOST));
    parts.uri = target.parse::<Uri>().unwrap_or_else(|_| Uri::from_static("/"));
    let req = Request::from_parts(parts, body);
    let mut guard = upstream.lock().await;
    let fresh = match guard.as_mut() {
        Some(s) => s.ready().await.is_err(),
        None => true,
    };
    if fresh {
        match connect_upstream(cfg, shared).await {
            Ok(s) => *guard = Some(s),
            Err(e) => {
                inc(&shared.counters.upstream_errors);
                shared.log(&format!("upstream error {method} {shown} -- {e:#} (502)"));
                return gateway_error();
            }
        }
    }
    let sender = guard.as_mut().expect("connected");
    match sender.send_request(req).await {
        Ok(resp) => {
            inc(&shared.counters.allowed);
            shared.log(&format!("allow {method} {shown} -- {} -> {}", rule.why, resp.status().as_u16()));
            let (mut parts, body) = resp.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            Response::from_parts(parts, body.boxed())
        }
        Err(e) => {
            *guard = None;
            inc(&shared.counters.upstream_errors);
            shared.log(&format!("upstream error {method} {shown} -- {e} (502)"));
            gateway_error()
        }
    }
}

async fn connect_upstream(cfg: &Config, shared: &Shared) -> Result<SendRequest<Incoming>> {
    let addrs = match &cfg.upstream {
        Upstream::Fixed(a) => vec![*a],
        Upstream::Dns => resolve_epic(shared).await?,
    };
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addrs[..])).await.context("connecting timed out")??;
    let name = ServerName::try_from(FORWARDED_HOST)?;
    let tls = tokio::time::timeout(Duration::from_secs(10), tokio_rustls::TlsConnector::from(cfg.client.clone()).connect(name, tcp)).await.context("the TLS handshake timed out")??;
    let (sender, conn) = hyper::client::conn::http1::Builder::new().preserve_header_case(true).handshake(TokioIo::new(tls)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ca_signs_only_for_epic_and_persists() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("eos-proxy");
        let ca = Ca::load_or_create(&dir, "awr").unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("ca.key")).unwrap().permissions().mode() & 0o777, 0o600);
        let again = Ca::load_or_create(&dir, "awr").unwrap();
        assert_eq!(ca.der, again.der);
        let marker = format!("{CA_NAME} (awr)");
        assert!(ca.der.windows(marker.len()).any(|w| w == marker.as_bytes()));
        let (chain, _) = ca.leaf().unwrap();
        assert_eq!(chain.len(), 2);
        assert!(Config::new(&ca, None, Upstream::Dns).is_ok());
    }

    #[test]
    fn local_addresses_are_refused() {
        for a in ["127.0.0.1", "10.0.0.1", "192.168.1.2", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "0.0.0.0"] {
            assert!(local(a.parse().unwrap()), "{a}");
        }
        for a in ["104.18.124.108", "2606:4700::6812:7c6c"] {
            assert!(!local(a.parse().unwrap()), "{a}");
        }
    }
}
