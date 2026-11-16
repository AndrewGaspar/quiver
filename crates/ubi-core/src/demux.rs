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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;
use ubi_proto::demux;

pub const DEFAULT_HOST: &str = "dmx.upc.ubisoft.com";
/// Client version sent in `ClientVersionPush`; matches Connect's `version.txt`.
pub const CLIENT_VERSION: u32 = 13416;
const MAX_FRAME: usize = 64 << 20;

type Stream = TlsStream<TcpStream>;

struct Inner {
    writer: tokio::sync::Mutex<WriteHalf<Stream>>,
    next_request_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<demux::Rsp>>>,
    connections: Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>,
    timeout: Duration,
}

#[derive(Clone)]
pub struct Demux {
    inner: Arc<Inner>,
}

impl Demux {
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
        let (reader, writer) = tokio::io::split(tls);
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(writer),
            next_request_id: AtomicU32::new(0),
            pending: Mutex::default(),
            connections: Mutex::default(),
            timeout: Duration::from_secs(15),
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
        self.inner.connections.lock().unwrap().insert(open.connection_id, tx);
        Ok(ServiceConnection {
            demux: self.clone(),
            id: open.connection_id,
            rx,
            next_request_id: 1,
            pushes: VecDeque::new(),
        })
    }
}

async fn read_loop(mut reader: ReadHalf<Stream>, inner: std::sync::Weak<Inner>) {
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
                        Some(tx) => {
                            let _ = tx.send(data.data);
                        }
                        None => tracing::debug!(id = data.connection_id, "data for unknown connection"),
                    }
                }
                if let Some(closed) = push.connection_closed {
                    tracing::warn!(id = closed.connection_id, code = ?closed.error_code, "demux closed connection");
                    inner.connections.lock().unwrap().remove(&closed.connection_id);
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
        inner.pending.lock().unwrap().clear();
        inner.connections.lock().unwrap().clear();
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
        service!(@impl $ty, $name, $module, ubi_proto::$module::Push, |d: ubi_proto::$module::Downstream| (d.response, d.push));
    };
    ($ty:ident, $name:literal, $module:ident) => {
        service!(@impl $ty, $name, $module, (), |d: ubi_proto::$module::Downstream| (d.response, None));
    };
    (@impl $ty:ident, $name:literal, $module:ident, $push:ty, $split:expr) => {
        pub struct $ty;
        impl Service for $ty {
            const NAME: &'static str = $name;
            type Up = ubi_proto::$module::Upstream;
            type Down = ubi_proto::$module::Downstream;
            type Req = ubi_proto::$module::Req;
            type Rsp = ubi_proto::$module::Rsp;
            type Push = $push;
            fn wrap(req: Self::Req) -> Self::Up {
                ubi_proto::$module::Upstream { request: Some(req) }
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

pub struct ServiceConnection<S: Service> {
    demux: Demux,
    id: u32,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    next_request_id: u32,
    pushes: VecDeque<S::Push>,
}

impl<S: Service> ServiceConnection<S> {
    pub async fn request(&mut self, mut req: S::Req) -> Result<S::Rsp> {
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
        self.demux.inner.connections.lock().unwrap().remove(&self.id);
    }
}
