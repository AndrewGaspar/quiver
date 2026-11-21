//! Pairing two machines with a 6-digit code (design/REMOTE.md §3).
//!
//! The server opens a [`PairingWindow`] (`quiver remote pair`): a code, good for
//! [`WINDOW`] and [`MAX_FAILURES`] wrong tries. The client connects with TLS without
//! pinning anything yet, and the two run SPAKE2 (Ed25519 group) over the code:
//!
//! ```text
//! C → S  hello  {intent: pair, name, pake: A}                 A = SPAKE2 msg (start_a)
//! S → C         {pake: B, name, id, mac, confirm: Ms}          B = SPAKE2 msg (start_b)
//! C → S         {confirm: Mc}            (or {error} when Ms is wrong: a wrong code)
//! S → C         {paired: {name, id}}     (or {error, code: wrong_code})
//! ```
//!
//! Both derive K = SPAKE2(code, A, B), and from it a key per side:
//! `Ks = HMAC(K, "quiver-pair v1 server")`, `Kc = HMAC(K, "quiver-pair v1 client")`. Each
//! side's confirmation is an HMAC-SHA256 under its key of the transcript
//! `T = SHA256("quiver-pair v1" ‖ A ‖ B ‖ hash(client cert) ‖ hash(server cert))`, with the
//! certificate hashes as each side saw them in this TLS session. A wrong code gives a
//! different K, so the confirmations fail; a machine in the middle (two TLS sessions, its
//! own certificates) gives different hashes, so they fail too. After both confirmations
//! each side pins the other's key.
//!
//! Every attempt that gets as far as the PAKE counts as a wrong try until it succeeds, so
//! an attacker gets [`MAX_FAILURES`] guesses at a million codes per window; a success
//! closes the window.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use ring::hmac;
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity as PakeId, Password, Spake2};
use tokio::io::{AsyncBufRead, AsyncWrite};

use crate::identity::{Identity, Pin};
use crate::tls::PeerCert;
use crate::wire::{self, Hello, Intent, Refused, code};

/// How long a pairing window stays open.
pub const WINDOW: Duration = Duration::from_secs(120);
/// Wrong tries before a window closes.
pub const MAX_FAILURES: u32 = 3;
/// How long each side waits for the other's next pairing message.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

const CLIENT_ID: &[u8] = b"quiver client";
const SERVER_ID: &[u8] = b"quiver server";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// An open pairing window on the server. `Debug` leaves the code out.
pub struct PairingWindow {
    code: String,
    opened: Instant,
    expires_at: DateTime<Utc>,
    failures: u32,
}

impl fmt::Debug for PairingWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingWindow").field("expires_at", &self.expires_at).field("failures", &self.failures).finish()
    }
}

impl PairingWindow {
    /// A new window with a random code.
    pub fn open() -> PairingWindow {
        PairingWindow {
            code: random_code(),
            opened: Instant::now(),
            expires_at: Utc::now() + chrono::Duration::from_std(WINDOW).unwrap_or_default(),
            failures: 0,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Still taking tries: not expired, and fewer than [`MAX_FAILURES`] wrong ones.
    pub fn is_open(&self) -> bool {
        self.opened.elapsed() < WINDOW && self.failures < MAX_FAILURES
    }
}

/// Six random digits, uniform.
fn random_code() -> String {
    use ring::rand::SecureRandom as _;
    let rng = ring::rand::SystemRandom::new();
    loop {
        let mut b = [0u8; 4];
        rng.fill(&mut b).expect("system randomness");
        let n = u32::from_le_bytes(b);
        // Reject the top sliver so every code is equally likely.
        if n < u32::MAX - (u32::MAX % 1_000_000) {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

/// A code as typed: digits only (spaces and dashes dropped), six of them.
pub fn clean_code(code: &str) -> Result<String> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        bail!("a pairing code is 6 digits");
    }
    Ok(digits)
}

struct Keys {
    server: hmac::Key,
    client: hmac::Key,
    transcript: [u8; 32],
}

fn keys(k: &[u8], a: &[u8], b: &[u8], client_cert: &[u8; 32], server_cert: &[u8; 32]) -> Keys {
    let root = hmac::Key::new(hmac::HMAC_SHA256, k);
    let derive = |label: &[u8]| hmac::Key::new(hmac::HMAC_SHA256, hmac::sign(&root, label).as_ref());
    let mut t = b"quiver-pair v1".to_vec();
    for part in [a, b, client_cert, server_cert] {
        t.extend_from_slice(&(part.len() as u32).to_be_bytes());
        t.extend_from_slice(part);
    }
    Keys { server: derive(b"quiver-pair v1 server"), client: derive(b"quiver-pair v1 client"), transcript: crate::identity::sha256(&t) }
}

#[derive(Serialize, Deserialize)]
struct ServerPake {
    pake: String,
    name: String,
    id: String,
    #[serde(default)]
    mac: Option<String>,
    confirm: String,
}

#[derive(Serialize, Deserialize)]
struct ClientConfirm {
    confirm: String,
}

#[derive(Serialize, Deserialize)]
struct PairedMsg {
    paired: crate::wire::Welcome,
}

/// The client that just paired, as the server sees it.
#[derive(Debug, Clone)]
pub struct PairedClient {
    pub name: String,
    pub pin: Pin,
}

/// The server that just paired, as the client sees it.
#[derive(Debug, Clone)]
pub struct PairedServer {
    pub name: String,
    pub id: String,
    pub pin: Pin,
    pub mac: Option<String>,
}

/// The server's side of a pairing, after the client's `hello` (intent `pair`) on a TLS
/// connection where it presented `client`. Takes a try from `window`; a success closes it.
pub async fn serve<S: AsyncBufRead + AsyncWrite + Unpin>(
    stream: &mut S,
    me: &Identity,
    hello: &Hello,
    client: PeerCert,
    window: &Mutex<Option<PairingWindow>>,
    mac: Option<String>,
) -> Result<PairedClient> {
    let refuse = |code: &str, msg: &str| Refused { code: code.into(), error: msg.into() };
    let taken = {
        let mut w = window.lock().unwrap();
        match w.as_mut() {
            Some(open) if open.is_open() => {
                open.failures += 1;
                Some(open.code.clone())
            }
            _ => {
                *w = None;
                None
            }
        }
    };
    let Some(pairing_code) = taken else {
        let r = refuse(code::NO_PAIRING, &format!("{} isn't pairing right now (run `quiver remote pair` there)", me.name()));
        let _ = wire::send(stream, &wire::Answer::refuse(&r.code, &r.error)).await;
        return Err(r.into());
    };
    let close_if_spent = || {
        let mut w = window.lock().unwrap();
        if w.as_ref().is_some_and(|o| !o.is_open()) {
            tracing::warn!("pairing window closed after {MAX_FAILURES} wrong codes");
            *w = None;
        }
    };
    let a = hello.pake.as_deref().and_then(|p| b64().decode(p).ok()).context("pairing without a PAKE message")?;
    let (state, b) = Spake2::<Ed25519Group>::start_b(&Password::new(pairing_code.as_bytes()), &PakeId::new(CLIENT_ID), &PakeId::new(SERVER_ID));
    let k = match state.finish(&a) {
        Ok(k) => k,
        Err(e) => {
            close_if_spent();
            bail!("bad PAKE message: {e:?}");
        }
    };
    let keys = keys(&k, &a, &b, &client.cert_hash, &me.cert_hash());
    let confirm = hmac::sign(&keys.server, &keys.transcript);
    let msg = ServerPake { pake: b64().encode(&b), name: me.name().to_owned(), id: me.id(), mac, confirm: b64().encode(confirm.as_ref()) };
    wire::send(stream, &msg).await?;
    let reply: Result<ClientConfirm> = tokio::time::timeout(STEP_TIMEOUT, wire::recv(stream)).await.unwrap_or_else(|_| Err(anyhow!("timed out")));
    let ok = match &reply {
        Ok(c) => b64().decode(&c.confirm).ok().is_some_and(|m| hmac::verify(&keys.client, &keys.transcript, &m).is_ok()),
        Err(_) => false,
    };
    if !ok {
        close_if_spent();
        let r = refuse(code::WRONG_CODE, "wrong pairing code");
        let _ = wire::send(stream, &wire::Answer::refuse(&r.code, &r.error)).await;
        return Err(match reply {
            Err(e) => e.context("pairing failed"),
            Ok(_) => r.into(),
        });
    }
    *window.lock().unwrap() = None;
    let name = crate::identity::clean_name(&hello.name).unwrap_or_else(|_| client.pin.id());
    wire::send(stream, &PairedMsg { paired: crate::wire::Welcome { name: me.name().to_owned(), id: me.id() } }).await?;
    Ok(PairedClient { name, pin: client.pin })
}

/// The client's side: pair with the machine at `addr` using `code`. On success the
/// caller pins the returned server (adds it to its peers).
pub async fn pair_with(me: &Identity, addr: &str, code: &str) -> Result<PairedServer> {
    let code = clean_code(code)?;
    let addr = crate::tls::with_default_port(addr);
    let (tls, server) = crate::tls::connect(me, &addr, None).await?;
    let mut stream = tokio::io::BufReader::new(tls);
    let (state, a) = Spake2::<Ed25519Group>::start_a(&Password::new(code.as_bytes()), &PakeId::new(CLIENT_ID), &PakeId::new(SERVER_ID));
    let hello = Hello { quiver_remote: wire::VERSION, intent: Intent::Pair, name: me.name().to_owned(), pake: Some(b64().encode(&a)) };
    wire::send(&mut stream, &hello).await?;
    let msg: ServerPake = tokio::time::timeout(STEP_TIMEOUT, wire::recv(&mut stream)).await.context("pairing timed out")??;
    let b = b64().decode(&msg.pake).context("bad PAKE message")?;
    let k = state.finish(&b).map_err(|e| anyhow!("bad PAKE message: {e:?}"))?;
    let keys = keys(&k, &a, &b, &me.cert_hash(), &server.cert_hash);
    let theirs = b64().decode(&msg.confirm).unwrap_or_default();
    if hmac::verify(&keys.server, &keys.transcript, &theirs).is_err() {
        let _ = wire::send(&mut stream, &wire::Answer::refuse(code::WRONG_CODE, "wrong pairing code")).await;
        return Err(Refused { code: code::WRONG_CODE.into(), error: "wrong pairing code".into() }.into());
    }
    if msg.id != server.pin.id() {
        bail!("the machine's id doesn't match its key");
    }
    let mine = hmac::sign(&keys.client, &keys.transcript);
    wire::send(&mut stream, &ClientConfirm { confirm: b64().encode(mine.as_ref()) }).await?;
    let _done: PairedMsg = tokio::time::timeout(STEP_TIMEOUT, wire::recv(&mut stream)).await.context("pairing timed out")??;
    let name = crate::identity::clean_name(&msg.name).unwrap_or_else(|_| msg.id.clone());
    Ok(PairedServer { name, id: msg.id, pin: server.pin, mac: msg.mac.filter(|m| crate::wol::parse_mac(m).is_ok()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let c = random_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|c| c.is_ascii_digit()));
        }
        assert_eq!(clean_code(" 123-456 ").unwrap(), "123456");
        assert!(clean_code("12345").is_err() && clean_code("12345a").is_err());
        let w = PairingWindow::open();
        assert!(w.is_open() && !format!("{w:?}").contains(w.code()));
    }

    /// A server that pairs once per connection, as quiverd does.
    async fn server(me: Arc<Identity>, window: Arc<Mutex<Option<PairingWindow>>>) -> (String, tokio::task::JoinHandle<Vec<Result<PairedClient>>>) {
        let acceptor = crate::tls::Acceptor::new(&me).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let mut out = Vec::new();
            while let Ok(Ok((tcp, _))) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await {
                let (tls, peer) = acceptor.accept(tcp).await.unwrap();
                let mut s = tokio::io::BufReader::new(tls);
                let hello: Hello = wire::recv(&mut s).await.unwrap();
                out.push(serve(&mut s, &me, &hello, peer, &window, Some("aa:bb:cc:dd:ee:ff".into())).await);
            }
            out
        });
        (addr, task)
    }

    #[tokio::test]
    async fn pairing_pins_both_sides() {
        let t = tempfile::tempdir().unwrap();
        let s = Arc::new(Identity::load_or_create(&t.path().join("s")).unwrap());
        let c = Identity::load_or_create(&t.path().join("c")).unwrap();
        let window = Arc::new(Mutex::new(Some(PairingWindow::open())));
        let code = window.lock().unwrap().as_ref().unwrap().code().to_owned();
        let (addr, task) = server(s.clone(), window.clone()).await;
        let paired = pair_with(&c, &addr, &code).await.unwrap();
        assert_eq!((paired.pin, paired.id.as_str(), paired.mac.as_deref()), (s.pin(), s.id().as_str(), Some("aa:bb:cc:dd:ee:ff")));
        assert!(window.lock().unwrap().is_none(), "a success closes the window");
        // The window is closed now: the same code doesn't pair again.
        let e = pair_with(&c, &addr, &code).await.unwrap_err();
        assert_eq!(wire::refused_code(&e), Some(code::NO_PAIRING), "{e:#}");
        let results = task.await.unwrap();
        let first = results[0].as_ref().unwrap();
        assert_eq!(first.pin, c.pin());
        assert_eq!(first.name, c.name());
    }

    #[tokio::test]
    async fn three_wrong_codes_close_the_window() {
        let t = tempfile::tempdir().unwrap();
        let s = Arc::new(Identity::load_or_create(&t.path().join("s")).unwrap());
        let c = Identity::load_or_create(&t.path().join("c")).unwrap();
        let window = Arc::new(Mutex::new(Some(PairingWindow::open())));
        let code = window.lock().unwrap().as_ref().unwrap().code().to_owned();
        let wrong = format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000);
        let (addr, task) = server(s.clone(), window.clone()).await;
        for i in 0..MAX_FAILURES {
            let e = pair_with(&c, &addr, &wrong).await.unwrap_err();
            assert_eq!(wire::refused_code(&e), Some(code::WRONG_CODE), "try {i}: {e:#}");
        }
        // Even the right code is refused now.
        let e = pair_with(&c, &addr, &code).await.unwrap_err();
        assert_eq!(wire::refused_code(&e), Some(code::NO_PAIRING), "{e:#}");
        assert!(window.lock().unwrap().is_none(), "closed after three");
        let results = task.await.unwrap();
        assert!(results.iter().all(|r| r.is_err()));
    }

    #[test]
    fn transcripts_bind_the_certificates() {
        let k = [1u8; 32];
        let a = keys(&k, b"A", b"B", &[1; 32], &[2; 32]);
        let b = keys(&k, b"A", b"B", &[1; 32], &[3; 32]);
        assert_ne!(a.transcript, b.transcript);
        let m = hmac::sign(&a.server, &a.transcript);
        assert!(hmac::verify(&b.server, &b.transcript, m.as_ref()).is_err());
        assert!(hmac::verify(&a.client, &a.transcript, m.as_ref()).is_err(), "the sides' keys differ");
    }
}
