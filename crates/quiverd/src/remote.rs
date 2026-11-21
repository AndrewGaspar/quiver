//! Remote play's control plane in quiverd (design/REMOTE.md §3, §4): the TLS listener (off
//! by default), pairing, paired machines and their reachability.
//!
//! - **The listener** binds `remote.listen` (default `0.0.0.0:47990`) while
//!   `remote.enabled` is set in config.json. Each connection is TLS with this machine's
//!   certificate; the client's key decides what it gets ([`serve_incoming`]): a paired
//!   client's `api` connection is served by the same handler as the Unix socket
//!   (`serve_conn`), as `remote:<peer name>`, with [`remote::local_only`] requests refused;
//!   an unpaired client can only pair, and only while a window is open.
//! - **Revoking** a peer drops its connections at once ([`Remote::kick`]).
//! - **Servers this machine paired with** are checked every [`PROBE_EVERY`] (a TLS `ping`),
//!   so `RemotePeers` and the app's status dots say whether each is up; changes go out as
//!   `PeerOnline`/`PeerOffline`.
//!
//! The listening socket goes to the next build on a restart like the Unix one
//! (`lifecycle::exec`, name `remote`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use quiver_api::remote::{self, PairCode, PairingState, PeerInfo, PeerState, RemoteStatus};
use quiver_api::{Event, Request};
use quiver_core::launcher::{self, Config};
use quiver_remote::pair::PairingWindow;
use quiver_remote::wire::{self, Answer, Hello, Intent, Welcome};
use quiver_remote::{Identity, Peer, Peers, Role};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::sync::watch;

use crate::Daemon;

/// How often servers this machine paired with are checked.
const PROBE_EVERY: Duration = Duration::from_secs(20);
/// How long a check waits for an answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// `last_seen` is written to peers.json at most this often per peer.
const LAST_SEEN_EVERY: chrono::Duration = chrono::Duration::minutes(10);

/// An error with a `Reply::Err::code` (`remote::NOT_IMPLEMENTED`, …).
#[derive(Debug)]
pub(crate) struct Coded {
    pub code: &'static str,
    pub error: String,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for Coded {}

/// The code an error from the handler carries, if any.
pub(crate) fn code_of(e: &anyhow::Error) -> Option<String> {
    e.chain().find_map(|c| c.downcast_ref::<Coded>()).map(|c| c.code.to_owned())
}

/// A paired client on a connection: requests are its, as `remote:<name>`.
#[derive(Clone)]
pub(crate) struct RemotePeer {
    pub name: String,
    /// Flips to true when the connection must close (revoked, listener off).
    pub kick: watch::Receiver<bool>,
}

impl RemotePeer {
    pub fn source(&self) -> String {
        remote::remote_source(&self.name)
    }
}

struct LiveConn {
    peer_id: String,
    kick: watch::Sender<bool>,
}

struct Listening {
    addr: SocketAddr,
    listener: Arc<tokio::net::TcpListener>,
    task: tokio::task::JoinHandle<()>,
    advert: Option<quiver_remote::discovery::Advert>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Remote state in the daemon.
pub(crate) struct Remote {
    dir: PathBuf,
    identity: Mutex<Option<Arc<Identity>>>,
    peers: Mutex<Peers>,
    window: Arc<Mutex<Option<PairingWindow>>>,
    /// Which window a timer belongs to (a new window replaces the old one's timer).
    window_gen: AtomicU64,
    listening: Mutex<Option<Listening>>,
    error: Mutex<Option<String>>,
    conns: Mutex<HashMap<u64, LiveConn>>,
    next_conn: AtomicU64,
    /// Servers' last check: reached or not.
    reach: Mutex<HashMap<String, bool>>,
}

impl Remote {
    pub fn new() -> Remote {
        let dir = launcher::data_dir().join("remote");
        let peers = Peers::load(&dir).unwrap_or_else(|e| {
            tracing::warn!("remote: {e:#}; starting with no peers");
            Peers::empty(&dir)
        });
        Remote {
            dir,
            identity: Mutex::new(None),
            peers: Mutex::new(peers),
            window: Arc::default(),
            window_gen: AtomicU64::new(0),
            listening: Mutex::new(None),
            error: Mutex::new(None),
            conns: Mutex::default(),
            next_conn: AtomicU64::new(1),
            reach: Mutex::default(),
        }
    }

    /// This machine's identity, made the first time it is needed.
    fn identity(&self) -> Result<Arc<Identity>> {
        let mut slot = self.identity.lock().unwrap();
        if let Some(i) = slot.as_ref() {
            return Ok(i.clone());
        }
        let i = Arc::new(Identity::load_or_create(&self.dir)?);
        *slot = Some(i.clone());
        Ok(i)
    }

    /// The listening socket's fd, for handing to the next build.
    pub fn listener_fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.listening.lock().unwrap().as_ref().map(|l| l.listener.as_raw_fd())
    }

    /// Close every connection of peer `id` (`None`: every remote connection).
    fn kick(&self, id: Option<&str>) {
        for c in self.conns.lock().unwrap().values().filter(|c| id.is_none_or(|id| c.peer_id == id)) {
            c.kick.send_replace(true);
        }
    }

    fn connections_of(&self, id: &str) -> usize {
        self.conns.lock().unwrap().values().filter(|c| c.peer_id == id).count()
    }

    fn peer_info(&self, p: &Peer) -> PeerInfo {
        let connections = if p.role == Role::Client { self.connections_of(&p.id) } else { 0 };
        let reached = match p.role {
            Role::Client => connections > 0,
            Role::Server => self.reach.lock().unwrap().get(&p.id).copied().unwrap_or(false),
        };
        let state = if reached {
            PeerState::Online
        } else if p.mac.is_some() {
            PeerState::Asleep
        } else {
            PeerState::Offline
        };
        PeerInfo {
            id: p.id.clone(),
            name: p.name.clone(),
            role: p.role,
            addr: p.addr.clone(),
            mac: p.mac.clone(),
            paired_at: p.paired_at,
            last_seen: p.last_seen,
            state,
            connections,
        }
    }

    fn peer_list(&self) -> Vec<PeerInfo> {
        let peers = self.peers.lock().unwrap().list().to_vec();
        peers.iter().map(|p| self.peer_info(p)).collect()
    }

    fn save_peers(&self) -> Result<()> {
        self.peers.lock().unwrap().save()
    }

    /// Note that a peer was seen now (written at most every [`LAST_SEEN_EVERY`]).
    fn seen(&self, pin: &quiver_remote::Pin, role: Role) {
        let mut peers = self.peers.lock().unwrap();
        let now = chrono::Utc::now();
        let Some(p) = peers.by_pin_mut(pin, role) else { return };
        let stale = p.last_seen.is_none_or(|t| now - t > LAST_SEEN_EVERY);
        p.last_seen = Some(now);
        if stale && let Err(e) = peers.save() {
            tracing::warn!("remote: saving peers: {e:#}");
        }
    }
}

/// Start what config.json says: the listener if `remote.enabled` (on the socket the last
/// build handed over, if it did), and the checks of paired servers.
pub(crate) fn start(daemon: &Arc<Daemon>, inherited: Option<std::net::TcpListener>) {
    let settings = daemon.cfg.remote.clone();
    if settings.enabled {
        let d = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = listen(&d, &settings.listen(), inherited).await {
                tracing::warn!("remote: not listening: {e:#}");
                *d.remote.error.lock().unwrap() = Some(format!("{e:#}"));
            }
        });
    }
    let weak = Arc::downgrade(daemon);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        loop {
            let Some(d) = weak.upgrade() else { return };
            probe_servers(&d).await;
            drop(d);
            tokio::time::sleep(PROBE_EVERY).await;
        }
    });
}

/// Bind `addr` (or take `inherited`) and serve it; mDNS too unless `QUIVER_REMOTE_MDNS=0`.
async fn listen(daemon: &Arc<Daemon>, addr: &str, inherited: Option<std::net::TcpListener>) -> Result<SocketAddr> {
    let me = daemon.remote.identity()?;
    let acceptor = quiver_remote::tls::Acceptor::new(&me)?;
    let listener = match inherited {
        Some(l) => {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)?
        }
        None => tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?,
    };
    let bound = listener.local_addr()?;
    let listener = Arc::new(listener);
    let advert = if std::env::var("QUIVER_REMOTE_MDNS").is_ok_and(|v| v == "0") {
        None
    } else {
        match quiver_remote::discovery::Advert::start(me.name(), &me.id(), bound.port()) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("remote: not advertising on mDNS: {e:#}");
                None
            }
        }
    };
    let weak = Arc::downgrade(daemon);
    let l = listener.clone();
    let task = tokio::spawn(async move {
        loop {
            let (tcp, from) = match l.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("remote accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Some(d) = weak.upgrade() else { return };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_incoming(d, acceptor, tcp).await {
                    tracing::info!("remote: connection from {from}: {e:#}");
                }
            });
        }
    });
    *daemon.remote.listening.lock().unwrap() = Some(Listening { addr: bound, listener, task, advert });
    *daemon.remote.error.lock().unwrap() = None;
    tracing::info!("remote: listening on {bound} as {} ({})", me.name(), me.id());
    Ok(bound)
}

/// Stop listening and drop every remote connection.
fn stop_listening(daemon: &Daemon) {
    let old = daemon.remote.listening.lock().unwrap().take();
    drop(old);
    daemon.remote.kick(None);
}

/// One connection on the listener: TLS, the hello, then pairing, a ping or the API. Boxed:
/// the handler it runs can start a listener, which spawns this.
fn serve_incoming(
    daemon: Arc<Daemon>,
    acceptor: quiver_remote::tls::Acceptor,
    tcp: tokio::net::TcpStream,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(serve_incoming_inner(daemon, acceptor, tcp))
}

async fn serve_incoming_inner(daemon: Arc<Daemon>, acceptor: quiver_remote::tls::Acceptor, tcp: tokio::net::TcpStream) -> Result<()> {
    let _ = tcp.set_nodelay(true);
    let local = tcp.local_addr().ok().map(|a| a.ip());
    let me = daemon.remote.identity()?;
    let (tls, cert) = acceptor.accept(tcp).await?;
    let mut stream = BufReader::new(tls);
    let hello: Hello = tokio::time::timeout(quiver_remote::tls::CONNECT_TIMEOUT, wire::recv(&mut stream)).await.context("no hello")??;
    if hello.quiver_remote != wire::VERSION {
        wire::send(&mut stream, &Answer::refuse(wire::code::VERSION, format!("{} speaks remote version {}", me.name(), wire::VERSION))).await?;
        bail!("remote version {}", hello.quiver_remote);
    }
    if hello.intent == Intent::Pair {
        return pair_incoming(&daemon, &me, &mut stream, &hello, cert, local).await;
    }
    let peer = daemon.remote.peers.lock().unwrap().by_pin(&cert.pin, Role::Client).cloned();
    let Some(peer) = peer else {
        wire::send(&mut stream, &Answer::refuse(wire::code::NOT_PAIRED, format!("{} isn't paired with this machine", me.name()))).await?;
        bail!("not paired ({})", cert.pin.id());
    };
    wire::send(&mut stream, &Answer { ok: Some(Welcome { name: me.name().to_owned(), id: me.id() }), ..Default::default() }).await?;
    daemon.remote.seen(&cert.pin, Role::Client);
    if hello.intent == Intent::Ping {
        return Ok(());
    }
    let (kick_tx, kick_rx) = watch::channel(false);
    let id = daemon.remote.next_conn.fetch_add(1, Ordering::Relaxed);
    let first = {
        let mut conns = daemon.remote.conns.lock().unwrap();
        let first = !conns.values().any(|c| c.peer_id == peer.id);
        conns.insert(id, LiveConn { peer_id: peer.id.clone(), kick: kick_tx });
        first
    };
    if first {
        daemon.emit(Event::PeerOnline { id: peer.id.clone(), name: peer.name.clone() });
    }
    tracing::info!("remote: {} ({}) connected", peer.name, peer.id);
    let ctx = RemotePeer { name: peer.name.clone(), kick: kick_rx };
    crate::serve_conn(daemon.clone(), stream, Some(ctx)).await;
    let last = {
        let mut conns = daemon.remote.conns.lock().unwrap();
        conns.remove(&id);
        !conns.values().any(|c| c.peer_id == peer.id)
    };
    if last {
        daemon.emit(Event::PeerOffline { id: peer.id.clone(), name: peer.name.clone() });
    }
    tracing::info!("remote: {} ({}) disconnected", peer.name, peer.id);
    Ok(())
}

/// The server's side of pairing, for an unpaired client while a window is open.
async fn pair_incoming(
    daemon: &Arc<Daemon>,
    me: &Identity,
    stream: &mut BufReader<quiver_remote::tls::ServerStream>,
    hello: &Hello,
    cert: quiver_remote::tls::PeerCert,
    local: Option<std::net::IpAddr>,
) -> Result<()> {
    let mac = quiver_remote::wol::local_mac(local);
    let r = &daemon.remote;
    match quiver_remote::pair::serve(stream, me, hello, cert, &r.window, mac).await {
        Ok(paired) => {
            let peer = Peer {
                id: paired.pin.id(),
                name: paired.name.clone(),
                pin: paired.pin,
                role: Role::Client,
                addr: None,
                mac: None,
                paired_at: chrono::Utc::now(),
                last_seen: Some(chrono::Utc::now()),
            };
            r.peers.lock().unwrap().upsert(peer.clone());
            r.save_peers()?;
            daemon.log_activity("system", format!("remote: paired with {} ({})", peer.name, peer.id), None);
            daemon.emit(Event::PairingEnded { outcome: "paired".into(), id: Some(peer.id), name: Some(peer.name) });
            daemon.emit(Event::RemoteChanged);
            Ok(())
        }
        Err(e) => {
            if wire::refused_code(&e) == Some(wire::code::WRONG_CODE) {
                daemon.log_activity("system", format!("remote: a pairing try from {} had the wrong code", hello.name), None);
            }
            // Three wrong tries closed the window.
            if wire::refused_code(&e) != Some(wire::code::NO_PAIRING) && r.window.lock().unwrap().is_none() {
                daemon.log_activity("system", "remote: pairing closed after 3 wrong codes".into(), None);
                daemon.emit(Event::PairingEnded { outcome: "failed".into(), id: None, name: None });
                daemon.emit(Event::RemoteChanged);
            }
            Err(e)
        }
    }
}

/// Check each server this machine paired with; report changes.
async fn probe_servers(daemon: &Arc<Daemon>) {
    let servers: Vec<Peer> = daemon.remote.peers.lock().unwrap().list().iter().filter(|p| p.role == Role::Server).cloned().collect();
    if servers.is_empty() {
        return;
    }
    let Ok(me) = daemon.remote.identity() else { return };
    let checks = servers.into_iter().map(|p| {
        let me = me.clone();
        async move {
            let ok = match p.machine() {
                Some(m) => matches!(tokio::time::timeout(PROBE_TIMEOUT, quiver_remote::connect(&me, &m, Intent::Ping)).await, Ok(Ok(_))),
                None => false,
            };
            (p, ok)
        }
    });
    for (p, ok) in futures_join(checks).await {
        set_reach(daemon, &p, ok);
    }
}

/// Every future, run concurrently.
async fn futures_join<F: std::future::Future + Send + 'static>(fs: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let handles: Vec<_> = fs.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

fn set_reach(daemon: &Daemon, p: &Peer, ok: bool) {
    let before = daemon.remote.reach.lock().unwrap().insert(p.id.clone(), ok);
    if ok {
        daemon.remote.seen(&p.pin, Role::Server);
    }
    if before != Some(ok) && (before.is_some() || ok) {
        daemon.emit(if ok { Event::PeerOnline { id: p.id.clone(), name: p.name.clone() } } else { Event::PeerOffline { id: p.id.clone(), name: p.name.clone() } });
    }
}

fn status(daemon: &Daemon) -> Result<RemoteStatus> {
    let me = daemon.remote.identity()?;
    let cfg = Config::load().map(|c| c.remote).unwrap_or_default();
    let listening = daemon.remote.listening.lock().unwrap();
    let pairing = daemon.remote.window.lock().unwrap().as_ref().filter(|w| w.is_open()).map(|w| PairingState {
        expires_at: w.expires_at(),
        tries_left: quiver_remote::pair::MAX_FAILURES - w.failures(),
    });
    Ok(RemoteStatus {
        name: me.name().to_owned(),
        id: me.id(),
        enabled: cfg.enabled,
        listen: cfg.listen(),
        listening: listening.as_ref().map(|l| l.addr.to_string()),
        error: daemon.remote.error.lock().unwrap().clone(),
        discoverable: listening.as_ref().is_some_and(|l| l.advert.is_some()),
        pairing,
        peers: daemon.remote.peer_list(),
        local_only: remote::local_only_list(),
    })
}

/// The addresses to give the other machine: the listener's, or with it on every address,
/// this machine's LAN and VPN addresses.
fn reach_addrs(daemon: &Daemon) -> Vec<String> {
    let Some(addr) = daemon.remote.listening.lock().unwrap().as_ref().map(|l| l.addr) else { return Vec::new() };
    if !addr.ip().is_unspecified() {
        return vec![addr.to_string()];
    }
    let port = addr.port();
    let ips = quiver_remote::wol::local_ips();
    let suffix = |ip: &std::net::IpAddr| if port == quiver_remote::DEFAULT_PORT { ip.to_string() } else { SocketAddr::new(*ip, port).to_string() };
    ips.iter().filter(|ip| ip.is_ipv4() || addr.is_ipv6()).map(suffix).collect()
}

fn save_settings(f: impl FnOnce(&mut remote::RemoteSettings)) -> Result<()> {
    let mut cfg = Config::load()?;
    f(&mut cfg.remote);
    cfg.save()
}

/// A request's `Request::Remote*` and `StartStream` handling.
pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let r = &daemon.remote;
    Ok(match req {
        Request::RemoteStatus => json!(status(daemon)?),
        Request::RemoteEnable { listen } => {
            let current = Config::load()?.remote;
            let addr = listen.clone().unwrap_or_else(|| current.listen());
            let want: SocketAddr = addr.parse().with_context(|| format!("'{addr}' isn't an address to listen on (host:port, such as 0.0.0.0:47990)"))?;
            let same = r.listening.lock().unwrap().as_ref().is_some_and(|l| l.addr == want);
            if !same {
                stop_listening(daemon);
                listen_and_record(daemon, &addr).await?;
            }
            save_settings(|s| {
                s.enabled = true;
                if listen.is_some() {
                    s.listen = Some(addr.clone()).filter(|a| a != quiver_remote::DEFAULT_LISTEN);
                }
            })?;
            daemon.log_activity(source, format!("remote: on ({addr})"), None);
            daemon.emit(Event::RemoteChanged);
            json!(status(daemon)?)
        }
        Request::RemoteDisable => {
            stop_listening(daemon);
            *r.window.lock().unwrap() = None;
            *r.error.lock().unwrap() = None;
            save_settings(|s| s.enabled = false)?;
            daemon.log_activity(source, "remote: off".into(), None);
            daemon.emit(Event::RemoteChanged);
            Value::Null
        }
        Request::RemotePairBegin => {
            if r.listening.lock().unwrap().is_none() {
                bail!("remote is off on this machine; turn it on first (`quiver remote enable`)");
            }
            let me = r.identity()?;
            let window = PairingWindow::open();
            let code = PairCode { code: window.code().to_owned(), name: me.name().to_owned(), expires_at: window.expires_at(), addrs: reach_addrs(daemon) };
            *r.window.lock().unwrap() = Some(window);
            let generation = r.window_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let weak = Arc::downgrade(daemon);
            tokio::spawn(async move {
                tokio::time::sleep(quiver_remote::pair::WINDOW).await;
                let Some(d) = weak.upgrade() else { return };
                if d.remote.window_gen.load(Ordering::SeqCst) == generation && d.remote.window.lock().unwrap().take().is_some() {
                    d.emit(Event::PairingEnded { outcome: "expired".into(), id: None, name: None });
                    d.emit(Event::RemoteChanged);
                }
            });
            daemon.log_activity(source, "remote: pairing open for 2 minutes".into(), None);
            daemon.emit(Event::RemoteChanged);
            json!(code)
        }
        Request::RemotePairEnd => {
            if r.window.lock().unwrap().take().is_some() {
                daemon.emit(Event::PairingEnded { outcome: "cancelled".into(), id: None, name: None });
                daemon.emit(Event::RemoteChanged);
            }
            Value::Null
        }
        Request::RemotePairWith { addr, code } => {
            let me = r.identity()?;
            let addr = quiver_remote::tls::with_default_port(&addr);
            let paired = quiver_remote::pair::pair_with(&me, &addr, &code).await.map_err(|e| match wire::refused_code(&e) {
                Some(wire::code::WRONG_CODE) => anyhow::anyhow!("wrong code for {addr} (three wrong codes end its pairing window)"),
                _ => e,
            })?;
            let peer = Peer {
                id: paired.id.clone(),
                name: paired.name.clone(),
                pin: paired.pin,
                role: Role::Server,
                addr: Some(addr.clone()),
                mac: paired.mac.clone(),
                paired_at: chrono::Utc::now(),
                last_seen: Some(chrono::Utc::now()),
            };
            r.peers.lock().unwrap().upsert(peer.clone());
            r.save_peers()?;
            set_reach(daemon, &peer, true);
            daemon.log_activity(source, format!("remote: paired with {} ({}) at {addr}", peer.name, peer.id), None);
            daemon.emit(Event::RemoteChanged);
            json!(r.peer_info(&peer))
        }
        Request::RemotePeers => json!(r.peer_list()),
        Request::RemoteRevoke { id } => {
            let gone = {
                let mut peers = r.peers.lock().unwrap();
                let id = peers.one(&id, None)?.id.clone();
                let gone = peers.remove(&id);
                peers.save()?;
                gone
            };
            let Some(first) = gone.first() else { bail!("no paired machine {id}") };
            r.kick(Some(&first.id));
            r.reach.lock().unwrap().remove(&first.id);
            daemon.log_activity(source, format!("remote: revoked {} ({})", first.name, first.id), None);
            daemon.emit(Event::PeerOffline { id: first.id.clone(), name: first.name.clone() });
            daemon.emit(Event::RemoteChanged);
            json!(gone.iter().map(|p| r.peer_info(p)).collect::<Vec<_>>())
        }
        Request::RemoteRename { id: None, name } => {
            let me = r.identity()?;
            let mut renamed = Identity::load_or_create(&r.dir)?;
            renamed.rename(&name)?;
            let old = me.name().to_owned();
            *r.identity.lock().unwrap() = Some(Arc::new(renamed));
            // The LAN sees the new name.
            let relisten = r.listening.lock().unwrap().as_ref().map(|l| l.addr);
            if let Some(addr) = relisten {
                let me = r.identity()?;
                if let Some(l) = r.listening.lock().unwrap().as_mut()
                    && l.advert.is_some()
                {
                    l.advert = None;
                    l.advert = quiver_remote::discovery::Advert::start(me.name(), &me.id(), addr.port()).ok();
                }
            }
            daemon.log_activity(source, format!("remote: this machine is now {} (was {old})", name.trim()), None);
            daemon.emit(Event::RemoteChanged);
            json!(status(daemon)?)
        }
        Request::RemoteRename { id: Some(id), name } => {
            {
                let mut peers = r.peers.lock().unwrap();
                let id = peers.one(&id, None)?.id.clone();
                peers.rename(&id, &name)?;
                peers.save()?;
            }
            daemon.log_activity(source, format!("remote: renamed {id} to {}", name.trim()), None);
            daemon.emit(Event::RemoteChanged);
            json!(r.peer_list())
        }
        Request::RemoteWake { id } => {
            let peer = r.peers.lock().unwrap().one(&id, None)?.clone();
            let mac = peer.mac.as_deref().with_context(|| format!("{}'s MAC isn't known (it's learned when pairing over a LAN)", peer.name))?;
            quiver_remote::wol::wake(mac, peer.addr.as_deref()).await?;
            daemon.log_activity(source, format!("remote: sent a wake-up to {}", peer.name), None);
            Value::Null
        }
        Request::RemoteDiscover { wait_ms } => {
            let wait = Duration::from_millis(wait_ms.unwrap_or(1500).min(10_000));
            let mine = r.identity().map(|i| i.id()).unwrap_or_default();
            let found = tokio::task::spawn_blocking(move || quiver_remote::discovery::browse(wait)).await??;
            json!(found.into_iter().filter(|d| d.id != mine).collect::<Vec<_>>())
        }
        Request::StartStream { .. } => {
            return Err(Coded { code: remote::NOT_IMPLEMENTED, error: "streaming isn't implemented yet".into() }.into());
        }
        other => bail!("not a remote request: {other:?}"),
    })
}

async fn listen_and_record(daemon: &Arc<Daemon>, addr: &str) -> Result<()> {
    match listen(daemon, addr, None).await {
        Ok(_) => Ok(()),
        Err(e) => {
            *daemon.remote.error.lock().unwrap() = Some(format!("{e:#}"));
            Err(e)
        }
    }
}
