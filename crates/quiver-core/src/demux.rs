//! Client for Ubisoft Connect's "demux" protocol on `dmx.upc.ubisoft.com:443`.
//!
//! Framing: every message is a 4-byte big-endian length followed by a protobuf
//! `demux.Upstream` (client→server) or `demux.Downstream` (server→client).
//! The first upstream message must be a `ClientVersionPush`.
//!
//! Services (ownership, download, denuvo, ...) are reached either with a one-shot
//! `ServiceReq` or, more commonly, by opening a *connection*: after
//! `OpenConnectionReq`, service messages travel inside `Push.data` with the
//! connection id, each itself length-prefixed.
//!
//! Service connections go stale in two ways, neither of which the socket itself reports:
//! the server may close one (`ConnectionClosedPush` with its id; [`ServiceConnection::is_open`]
//! turns false), and `ownership_service` only answers the most recently opened (and
//! initialized) ownership connection on a socket. Older ones stay "open" but every request
//! on them goes unanswered until it times out ([`ServiceConnection::is_latest`]).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::rustls;
use quiver_proto::demux;

pub const DEFAULT_HOST: &str = "dmx.upc.ubisoft.com";
/// Client version sent in `ClientVersionPush`; matches Connect's `version.txt`.
pub const CLIENT_VERSION: u32 = 13416;
const MAX_FRAME: usize = 64 << 20;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// Shared between a [`ServiceConnection`] and the reader's routing table.
struct ConnState {
    service: &'static str,
    /// Unique per opened connection (server connection ids may be reused).
    serial: u64,
    /// Set when the server closes the connection or the socket ends.
    closed: AtomicBool,
}

struct Route {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    state: Arc<ConnState>,
}

struct Inner {
    writer: tokio::sync::Mutex<Writer>,
    next_request_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<demux::Rsp>>>,
    connections: Mutex<HashMap<u32, Route>>,
    /// Serial of the most recently opened connection per service.
    latest: Mutex<HashMap<&'static str, u64>>,
    next_serial: AtomicU64,
    timeout: Duration,
    closed: AtomicBool,
    /// Serializes sessions of services that only answer their newest connection
    /// (`ownership_service`): see [`Demux::exclusive`].
    exclusive: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub struct Demux {
    inner: Arc<Inner>,
}

impl Demux {
    /// Hold this while opening, initializing or using a connection of a service that only
    /// answers the most recently opened connection on the socket (`ownership_service`).
    /// Without it, a session opened by another task (a game launch's broker, a library
    /// refresh) between a request and its answer silently swallows the answer.
    pub async fn exclusive(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.exclusive.lock().await
    }

    pub async fn connect() -> Result<Self> {
        Self::connect_to(DEFAULT_HOST, CLIENT_VERSION).await
    }

    pub async fn connect_to(host: &str, client_version: u32) -> Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = TcpStream::connect((host, 443))
            .await
            .with_context(|| format!("connect to {host}:443"))?;
        tcp.set_nodelay(true)?;
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .context("demux TLS handshake")?;
        Self::over(tls, client_version, DEFAULT_TIMEOUT).await
    }

    /// Speak demux over an established stream (TLS in production, a pipe in tests).
    pub(crate) async fn over<S>(stream: S, client_version: u32, timeout: Duration) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            next_request_id: AtomicU32::new(0),
            pending: Mutex::default(),
            connections: Mutex::default(),
            latest: Mutex::default(),
            next_serial: AtomicU64::new(1),
            timeout,
            closed: AtomicBool::new(false),
            exclusive: tokio::sync::Mutex::new(()),
        });
        tokio::spawn(read_loop(reader, Arc::downgrade(&inner)));
        let demux = Self { inner };
        demux
            .push(demux::Push {
                client_version: Some(demux::ClientVersionPush { version: client_version }),
                ..Default::default()
            })
            .await?;
        Ok(demux)
    }

    async fn write_frame(&self, payload: &[u8]) -> Result<()> {
        let mut w = self.inner.writer.lock().await;
        w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
        w.write_all(payload).await?;
        w.flush().await?;
        Ok(())
    }

    /// False once the server closed the socket (e.g. another login replaced this one).
    pub fn is_alive(&self) -> bool {
        !self.inner.closed.load(Ordering::Relaxed)
    }

    pub async fn push(&self, push: demux::Push) -> Result<()> {
        let up = demux::Upstream { request: None, push: Some(push) };
        self.write_frame(&up.encode_to_vec()).await
    }

    /// Send a top-level request; `request_id` is filled in automatically.
    pub async fn request(&self, mut req: demux::Req) -> Result<demux::Rsp> {
        let id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        req.request_id = id;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let up = demux::Upstream { request: Some(req), push: None };
        if let Err(e) = self.write_frame(&up.encode_to_vec()).await {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(self.inner.timeout, rx).await {
            Ok(Ok(rsp)) => Ok(rsp),
            Ok(Err(_)) => bail!("demux connection closed while waiting for request {id}"),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                // The demux server never returns errors; a malformed or unauthorized
                // request simply goes unanswered.
                bail!("demux request {id} timed out (server sends no errors; likely rejected)")
            }
        }
    }

    /// Authenticate the socket with a UbiServices ticket.
    pub async fn authenticate(&self, ubi_ticket: &str) -> Result<()> {
        let rsp = self
            .request(demux::Req {
                authenticate_req: Some(demux::AuthenticateReq {
                    token: demux::Token {
                        ubi_ticket: Some(ubi_ticket.to_owned()),
                        ..Default::default()
                    },
                    send_keep_alive: Some(false),
                    client_id: Some("uplay_pc".into()),
                    logout_push_group_id: None,
                }),
                ..Default::default()
            })
            .await?;
        let auth = rsp.authenticate_rsp.context("no authenticate_rsp")?;
        if auth.banned == Some(true) {
            bail!("demux authentication: account is banned");
        }
        if auth.expired == Some(true) {
            bail!("demux authentication: ticket expired");
        }
        if !auth.success {
            bail!("demux authentication failed");
        }
        Ok(())
    }

    pub async fn patch_info(&self) -> Result<demux::GetPatchInfoRsp> {
        let rsp = self
            .request(demux::Req {
                get_patch_info_req: Some(demux::GetPatchInfoReq {
                    patch_track_id: "DEFAULT".into(),
                    test_config: false,
                    track_type: Some(0),
                }),
                ..Default::default()
            })
            .await?;
        rsp.get_patch_info_rsp.context("no get_patch_info_rsp")
    }

    pub async fn open<S: Service>(&self) -> Result<ServiceConnection<S>> {
        let rsp = self
            .request(demux::Req {
                open_connection_req: Some(demux::OpenConnectionReq { service_name: S::NAME.into() }),
                ..Default::default()
            })
            .await?;
        let open = rsp.open_connection_rsp.context("no open_connection_rsp")?;
        if !open.success {
            bail!("demux refused to open {}", S::NAME);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(ConnState {
            service: S::NAME,
            serial: self.inner.next_serial.fetch_add(1, Ordering::Relaxed),
            closed: AtomicBool::new(false),
        });
        let route = Route { tx, state: state.clone() };
        if let Some(old) = self.inner.connections.lock().unwrap().insert(open.connection_id, route) {
            // The server reused an id, so the connection that had it is gone.
            old.state.closed.store(true, Ordering::Relaxed);
        }
        self.inner.latest.lock().unwrap().insert(S::NAME, state.serial);
        Ok(ServiceConnection {
            demux: self.clone(),
            id: open.connection_id,
            state,
            rx,
            next_request_id: 1,
            pushes: VecDeque::new(),
        })
    }
}

async fn read_loop<R: AsyncRead + Unpin>(mut reader: R, inner: std::sync::Weak<Inner>) {
    let result: Result<()> = async {
        loop {
            let len = reader.read_u32().await? as usize;
            if len > MAX_FRAME {
                bail!("demux frame of {len} bytes exceeds limit");
            }
            let mut buf = vec![0; len];
            reader.read_exact(&mut buf).await?;
            let Some(inner) = inner.upgrade() else { return Ok(()) };
            let down = demux::Downstream::decode(buf.as_slice())?;
            if let Some(rsp) = down.response {
                if let Some(tx) = inner.pending.lock().unwrap().remove(&rsp.request_id) {
                    let _ = tx.send(rsp);
                } else {
                    tracing::debug!(id = rsp.request_id, "unsolicited demux response");
                }
            }
            if let Some(push) = down.push {
                if let Some(data) = push.data {
                    let conns = inner.connections.lock().unwrap();
                    match conns.get(&data.connection_id) {
                        Some(route) => {
                            let _ = route.tx.send(data.data);
                        }
                        None => tracing::debug!(id = data.connection_id, "data for unknown connection"),
                    }
                }
                if let Some(closed) = push.connection_closed {
                    // Dropping the route ends the connection's channel, so a request
                    // waiting on it fails at once instead of timing out.
                    let route = inner.connections.lock().unwrap().remove(&closed.connection_id);
                    let service = route.as_ref().map_or("unknown", |r| r.state.service);
                    tracing::warn!(id = closed.connection_id, service, code = ?closed.error_code, "demux closed connection");
                    if let Some(route) = route {
                        route.state.closed.store(true, Ordering::Relaxed);
                    }
                    if closed.error_code == Some(demux::connection_closed_push::ConnectionErrorCode::ConnectionMultipleLogin as i32) {
                        tracing::warn!("another Ubisoft login replaced this one");
                        inner.closed.store(true, Ordering::Relaxed);
                    }
                }
                if push.client_outdated.is_some() {
                    tracing::warn!("demux says client version {CLIENT_VERSION} is outdated");
                }
            }
        }
    }
    .await;
    if let Err(e) = result {
        tracing::debug!("demux reader stopped: {e:#}");
    }
    if let Some(inner) = inner.upgrade() {
        inner.closed.store(true, Ordering::Relaxed);
        inner.pending.lock().unwrap().clear();
        for (_, route) in inner.connections.lock().unwrap().drain() {
            route.state.closed.store(true, Ordering::Relaxed);
        }
    }
}

/// A demux service whose envelope follows the usual shape:
/// `Upstream { request: Req { request_id, .. } }`, `Downstream { response: Rsp, push? }`.
pub trait Service {
    const NAME: &'static str;
    type Up: Message + Default;
    type Down: Message + Default;
    type Req: Default;
    type Rsp;
    type Push;
    fn wrap(req: Self::Req) -> Self::Up;
    fn set_request_id(req: &mut Self::Req, id: u32);
    fn response_id(rsp: &Self::Rsp) -> u32;
    fn split(down: Self::Down) -> (Option<Self::Rsp>, Option<Self::Push>);
}

macro_rules! service {
    ($ty:ident, $name:literal, $module:ident, push) => {
        service!(@impl $ty, $name, $module, quiver_proto::$module::Push, |d: quiver_proto::$module::Downstream| (d.response, d.push));
    };
    ($ty:ident, $name:literal, $module:ident) => {
        service!(@impl $ty, $name, $module, (), |d: quiver_proto::$module::Downstream| (d.response, None));
    };
    (@impl $ty:ident, $name:literal, $module:ident, $push:ty, $split:expr) => {
        pub struct $ty;
        impl Service for $ty {
            const NAME: &'static str = $name;
            type Up = quiver_proto::$module::Upstream;
            type Down = quiver_proto::$module::Downstream;
            type Req = quiver_proto::$module::Req;
            type Rsp = quiver_proto::$module::Rsp;
            type Push = $push;
            fn wrap(req: Self::Req) -> Self::Up {
                quiver_proto::$module::Upstream { request: Some(req) }
            }
            fn set_request_id(req: &mut Self::Req, id: u32) {
                req.request_id = id;
            }
            fn response_id(rsp: &Self::Rsp) -> u32 {
                rsp.request_id
            }
            fn split(down: Self::Down) -> (Option<Self::Rsp>, Option<Self::Push>) {
                ($split)(down)
            }
        }
    };
}

service!(Ownership, "ownership_service", ownership, push);
service!(Denuvo, "denuvo_service", denuvo_service);
service!(Cloudsave, "cloudsave_service", cloudsave_service);
service!(Download, "download_service", download_service);

/// `playtime_service` (package `mg.playtime`): proto2 with an optional request id.
pub struct Playtime;
impl Service for Playtime {
    const NAME: &'static str = "playtime_service";
    type Up = quiver_proto::playtime::Upstream;
    type Down = quiver_proto::playtime::Downstream;
    type Req = quiver_proto::playtime::Req;
    type Rsp = quiver_proto::playtime::Rsp;
    type Push = ();
    fn wrap(req: Self::Req) -> Self::Up {
        quiver_proto::playtime::Upstream { request: Some(req) }
    }
    fn set_request_id(req: &mut Self::Req, id: u32) {
        req.request_id = Some(id);
    }
    fn response_id(rsp: &Self::Rsp) -> u32 {
        rsp.request_id.unwrap_or_default()
    }
    fn split(down: Self::Down) -> (Option<Self::Rsp>, Option<Self::Push>) {
        (down.response, None)
    }
}

pub struct ServiceConnection<S: Service> {
    demux: Demux,
    id: u32,
    state: Arc<ConnState>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    next_request_id: u32,
    pushes: VecDeque<S::Push>,
}

impl<S: Service> ServiceConnection<S> {
    /// The server's id for this connection.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// False once the server closed this connection (or the whole socket).
    pub fn is_open(&self) -> bool {
        self.demux.is_alive() && !self.state.closed.load(Ordering::Relaxed)
    }

    /// Whether this is the most recently opened connection to its service on this socket.
    /// `ownership_service` only answers that one.
    pub fn is_latest(&self) -> bool {
        self.demux.inner.latest.lock().unwrap().get(S::NAME) == Some(&self.state.serial)
    }

    pub async fn request(&mut self, mut req: S::Req) -> Result<S::Rsp> {
        if !self.is_open() {
            bail!("{} connection {} was closed by the server", S::NAME, self.id);
        }
        let id = self.next_request_id;
        self.next_request_id += 1;
        S::set_request_id(&mut req, id);
        let up = S::wrap(req).encode_to_vec();
        let mut data = Vec::with_capacity(up.len() + 4);
        data.extend_from_slice(&(up.len() as u32).to_be_bytes());
        data.extend_from_slice(&up);
        self.demux
            .push(demux::Push {
                data: Some(demux::DataMessage { connection_id: self.id, data }),
                ..Default::default()
            })
            .await?;
        let deadline = tokio::time::Instant::now() + self.demux.inner.timeout;
        loop {
            let frame = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .map_err(|_| anyhow!("{} request {id} timed out (likely rejected)", S::NAME))?
                .with_context(|| format!("{} connection closed", S::NAME))?;
            let body = frame.get(4..).context("short service frame")?;
            let (rsp, push) = S::split(S::Down::decode(body)?);
            if let Some(push) = push {
                self.pushes.push_back(push);
            }
            match rsp {
                Some(rsp) if S::response_id(&rsp) == id => return Ok(rsp),
                Some(rsp) => tracing::debug!(got = S::response_id(&rsp), want = id, "stale response"),
                None => {}
            }
        }
    }

    /// Pushes received so far (e.g. ownership changes).
    pub fn drain_pushes(&mut self) -> impl Iterator<Item = S::Push> + '_ {
        self.pushes.drain(..)
    }
}

impl<S: Service> Drop for ServiceConnection<S> {
    fn drop(&mut self) {
        let mut conns = self.demux.inner.connections.lock().unwrap();
        // Only our own route: the server may have given the id to a newer connection.
        if conns.get(&self.id).is_some_and(|r| Arc::ptr_eq(&r.state, &self.state)) {
            conns.remove(&self.id);
        }
    }
}

/// An in-process demux server for tests: answers `OpenConnectionReq` with fresh ids and
/// hands service data to a handler; tests can also inject downstream messages.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    /// `handler(service, connection id, service frame body)` returns a reply body, if any.
    pub type Handler = Box<dyn FnMut(&str, u32, &[u8]) -> Option<Vec<u8>> + Send>;

    pub struct Server {
        pub demux: Demux,
        /// Sends a downstream message as the server.
        pub inject: mpsc::UnboundedSender<demux::Downstream>,
    }

    pub async fn start(timeout: Duration, mut handler: Handler) -> Server {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (mut r, mut w) = tokio::io::split(server);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<demux::Downstream>();
        tokio::spawn(async move {
            while let Some(down) = out_rx.recv().await {
                let buf = down.encode_to_vec();
                if w.write_all(&(buf.len() as u32).to_be_bytes()).await.is_err() || w.write_all(&buf).await.is_err() {
                    return;
                }
            }
        });
        let inject = out_tx.clone();
        tokio::spawn(async move {
            let mut services: HashMap<u32, String> = HashMap::new();
            let mut next_conn = 1;
            loop {
                let Ok(len) = r.read_u32().await else { return };
                let mut buf = vec![0; len as usize];
                if r.read_exact(&mut buf).await.is_err() {
                    return;
                }
                let up = demux::Upstream::decode(buf.as_slice()).unwrap();
                if let Some(open) = up.request.as_ref().and_then(|r| r.open_connection_req.as_ref()) {
                    let id = next_conn;
                    next_conn += 1;
                    services.insert(id, open.service_name.clone());
                    let rsp = demux::Rsp {
                        request_id: up.request.as_ref().unwrap().request_id,
                        open_connection_rsp: Some(demux::OpenConnectionRsp { connection_id: id, success: true }),
                        ..Default::default()
                    };
                    let _ = out_tx.send(demux::Downstream { response: Some(rsp), push: None });
                }
                if let Some(data) = up.push.and_then(|p| p.data) {
                    let service = services.get(&data.connection_id).cloned().unwrap_or_default();
                    if let Some(body) = handler(&service, data.connection_id, &data.data[4..]) {
                        let _ = out_tx.send(data_push(data.connection_id, &body));
                    }
                }
            }
        });
        let demux = Demux::over(client, CLIENT_VERSION, timeout).await.unwrap();
        Server { demux, inject }
    }

    /// A `Push.data` carrying one length-prefixed service frame.
    pub fn data_push(connection_id: u32, body: &[u8]) -> demux::Downstream {
        let mut data = (body.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(body);
        demux::Downstream {
            response: None,
            push: Some(demux::Push { data: Some(demux::DataMessage { connection_id, data }), ..Default::default() }),
        }
    }

    pub fn closed_push(connection_id: u32) -> demux::Downstream {
        demux::Downstream {
            response: None,
            push: Some(demux::Push {
                connection_closed: Some(demux::ConnectionClosedPush { connection_id, error_code: None }),
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_proto::denuvo_service;

    /// A denuvo service that answers every request with an empty success.
    fn answer_all() -> fake::Handler {
        Box::new(|_, _, body| {
            let req = denuvo_service::Upstream::decode(body).ok()?.request?;
            let rsp = denuvo_service::Rsp { request_id: req.request_id, ..Default::default() };
            Some(denuvo_service::Downstream { response: Some(rsp) }.encode_to_vec())
        })
    }

    async fn until(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn closed_push_marks_connection_closed() {
        let server = fake::start(Duration::from_secs(10), answer_all()).await;
        let mut a = server.demux.open::<Denuvo>().await.unwrap();
        let mut b = server.demux.open::<Denuvo>().await.unwrap();
        assert!(a.request(Default::default()).await.is_ok());
        assert!(a.is_open() && b.is_open());

        server.inject.send(fake::closed_push(a.id())).unwrap();
        until(|| !a.is_open()).await;
        assert!(!a.is_open());
        assert!(b.is_open(), "only the named connection closes");
        assert!(server.demux.is_alive(), "a service close isn't a socket close");

        // Fails at once rather than after the 10 s timeout.
        let t = std::time::Instant::now();
        let err = a.request(Default::default()).await.unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(format!("{err:#}").contains("closed"), "{err:#}");
        assert!(b.request(Default::default()).await.is_ok());
    }

    #[tokio::test]
    async fn closed_push_wakes_a_waiting_request() {
        // A service that never answers.
        let server = fake::start(Duration::from_secs(10), Box::new(|_, _, _| None)).await;
        let mut a = server.demux.open::<Denuvo>().await.unwrap();
        let (id, inject) = (a.id(), server.inject.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            inject.send(fake::closed_push(id)).unwrap();
        });
        let t = std::time::Instant::now();
        let err = a.request(Default::default()).await.unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(2), "waited {:?}", t.elapsed());
        assert!(format!("{err:#}").contains("closed"), "{err:#}");
    }

    #[tokio::test]
    async fn newest_connection_per_service_is_latest() {
        let server = fake::start(Duration::from_secs(10), answer_all()).await;
        let a = server.demux.open::<Denuvo>().await.unwrap();
        assert!(a.is_latest());
        let b = server.demux.open::<Denuvo>().await.unwrap();
        assert!(!a.is_latest() && b.is_latest());
        let _c = server.demux.open::<Cloudsave>().await.unwrap();
        assert!(b.is_latest(), "other services don't count");
    }
}
