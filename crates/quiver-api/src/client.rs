//! Async client for `quiverd`: this machine's, over its Unix socket (started if it isn't
//! running), or a paired machine's over TLS ([`Client::machine`], design/REMOTE.md §4).
//! Either way it survives the daemon restarting or the link dropping:
//!
//! - A request whose write fails never reached the daemon, so it is sent again once on a
//!   new connection.
//! - If the connection drops after a request was sent but before its reply, the request
//!   may have run, so it is not resent: the call fails with an error saying so, and the
//!   next call reconnects.
//! - Except when the daemon said it is restarting onto a new build: requests it refused
//!   (`code: restarting`) or never answered before its [`Reply::Closing`] did not run, so
//!   they are sent again on the new daemon for up to [`RESTART_RETRY`].
//! - A subscription is renewed on every new connection. Subscribers see
//!   [`Event::Disconnected`] when the daemon goes away and [`Event::Reconnected`] once
//!   events flow again (anything that happened in between was missed; reload state).
//!
//! A remote client never starts anything: while the machine can't be reached its calls
//! fail (after waiting out the same grace as a restart), and a subscription keeps
//! reconnecting with backoff. Each reconnect reads the machine's address and key from
//! `peers.json` again, so a revoked machine stops being reached and a moved one is found
//! at its new address.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{Envelope, Event, Reply, Request, socket_path};

/// A request's error: the daemon's message, and its code (`daemon::RESTARTING`).
struct Failure {
    error: String,
    code: Option<String>,
}

/// A request the daemon refused: its message, and its code when it gave one
/// (`daemon::RESTARTING`, `remote::LOCAL_ONLY`, `remote::NOT_IMPLEMENTED`, …). Displays as
/// the message, as errors from [`Client::call`] always have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    pub error: String,
    pub code: Option<String>,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for CallError {}

impl CallError {
    /// The daemon's code for a failed call, if it refused it with one.
    pub fn code_of(e: &anyhow::Error) -> Option<&str> {
        e.chain().find_map(|c| c.downcast_ref::<CallError>()).and_then(|c| c.code.as_deref())
    }
}

/// Where the daemon is.
enum Transport {
    /// This machine's, at a socket path.
    Unix(PathBuf),
    /// A paired machine's, with this machine's identity and peers in `dir`.
    Remote { dir: PathBuf, machine: quiver_remote::Machine },
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

impl Failure {
    fn restarting(&self) -> bool {
        self.code.as_deref() == Some(crate::daemon::RESTARTING)
    }
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, Failure>>>>;
type Subscriber = Arc<Mutex<Option<mpsc::UnboundedSender<Event>>>>;

/// How long a reconnect waits for a restarting daemon before starting one itself.
const RESTART_GRACE: Duration = Duration::from_secs(3);
/// Background reconnect backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// How long a call keeps retrying while the daemon restarts onto a new build.
pub const RESTART_RETRY: Duration = Duration::from_secs(30);
const RESTART_RETRY_EVERY: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

struct Shared {
    transport: Transport,
    /// Start the daemon when nobody is listening (off in tests, never for a remote one).
    autostart: bool,
    source: String,
    next_id: AtomicU64,
    /// The current connection; replaced when it has closed.
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
    events: Subscriber,
    /// Mod options events (`Reply::Options`), for clients that asked.
    options_events: Arc<Mutex<Option<mpsc::UnboundedSender<crate::options::OptionsEvent>>>>,
    /// Perf events (`Reply::Perf`), for clients that asked.
    perf_events: Arc<Mutex<Option<mpsc::UnboundedSender<crate::perf::PerfEvent>>>>,
    supervising: AtomicBool,
}

/// One socket connection to the daemon.
struct Conn {
    writer: tokio::sync::Mutex<Writer>,
    pending: Pending,
    /// Flips to true once the socket closed (the reader saw EOF or a write failed).
    closed: watch::Sender<bool>,
    /// `Subscribe` was sent on this connection.
    subscribed: AtomicBool,
    /// The daemon sent [`Reply::Closing`]: what it hasn't answered won't run.
    closing: AtomicBool,
}

impl Conn {
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Mark closed and fail every request still waiting for a reply: as "may have run"
    /// (dropped), or, after a [`Reply::Closing`], as refused, to be sent again.
    fn close(&self) {
        self.closed.send_replace(true);
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        if self.closing.load(Ordering::SeqCst) {
            for (_, tx) in pending {
                let _ = tx.send(Err(Failure { error: "quiverd restarted".into(), code: Some(crate::daemon::RESTARTING.into()) }));
            }
        }
    }
}

/// A request's method name (`install`, `status`, …) for messages.
fn method(request: &Request) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "request".into())
}

impl Client {
    /// Connect to the daemon, starting it if needed. `source` labels this client in the
    /// daemon's activity log and settings history (e.g. "gui", "cli", "agent").
    pub async fn connect(source: &str) -> Result<Self> {
        Self::connect_at(socket_path(), source, true).await
    }

    /// Connect to a running daemon only: never starts one, now or on a reconnect (after
    /// [`Event::Disconnected`], reconnects fail until a daemon is up again). For processes
    /// quiverd starts itself, such as `quiver-sight`.
    pub async fn connect_running(source: &str) -> Result<Self> {
        Self::connect_at(socket_path(), source, false).await
    }

    /// [`Self::connect_running`] on the socket at `path` (another daemon's, such as a
    /// scratch one in tests).
    pub async fn connect_running_at(path: PathBuf, source: &str) -> Result<Self> {
        Self::connect_at(path, source, false).await
    }

    /// Connect to a paired machine's quiverd over TLS, with this machine's identity and
    /// peers (`<data>/remote`). Fails if it can't be reached now; afterwards it reconnects
    /// as the module docs say. Requests arrive there as `remote:<this machine's name>`
    /// when `source` is the person (`cli`, `gui`), else as
    /// `remote:<this machine's name>/<source>` (`remote::remote_source_as`).
    pub async fn machine(machine: &quiver_remote::Machine, source: &str) -> Result<Self> {
        Self::machine_at(quiver_remote::remote_dir(), machine, source).await
    }

    /// [`Self::machine`] for the server peer `query` names (a name, id or unique id prefix).
    pub async fn machine_named(query: &str, source: &str) -> Result<Self> {
        let dir = quiver_remote::remote_dir();
        let peers = quiver_remote::Peers::load(&dir)?;
        let peer = peers.one(query, Some(quiver_remote::Role::Server))?;
        let machine = peer.machine().with_context(|| format!("{} has no known address", peer.name))?;
        Self::machine_at(dir, &machine, source).await
    }

    /// [`Self::machine`] with the identity and peers in `dir` (a scratch machine's, in tests).
    pub async fn machine_at(dir: PathBuf, machine: &quiver_remote::Machine, source: &str) -> Result<Self> {
        Self::open_with(Transport::Remote { dir, machine: machine.clone() }, source, false).await
    }

    /// The paired machine this client talks to, if it isn't this machine's daemon.
    pub fn remote_machine(&self) -> Option<&quiver_remote::Machine> {
        match &self.shared.transport {
            Transport::Remote { machine, .. } => Some(machine),
            Transport::Unix(_) => None,
        }
    }

    pub(crate) async fn connect_at(path: PathBuf, source: &str, autostart: bool) -> Result<Self> {
        Self::open_with(Transport::Unix(path), source, autostart).await
    }

    async fn open_with(transport: Transport, source: &str, autostart: bool) -> Result<Self> {
        let shared = Arc::new(Shared {
            transport,
            autostart,
            source: source.to_owned(),
            next_id: AtomicU64::new(1),
            conn: tokio::sync::Mutex::new(None),
            events: Arc::default(),
            options_events: Arc::default(),
            perf_events: Arc::default(),
            supervising: AtomicBool::new(false),
        });
        let conn = shared.open(Duration::ZERO).await?;
        *shared.conn.lock().await = Some(conn);
        Ok(Self { shared })
    }

    /// Send a request and decode its result. While the daemon restarts onto a new build,
    /// the request is sent again until the new one answers (see the module docs).
    pub async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let what = method(&request);
        let deadline = Instant::now() + RESTART_RETRY;
        let value = loop {
            let (_conn, rx) = self.shared.send(&request).await?;
            match rx.await {
                Err(_) => bail!("{} went away (restarted?) before answering '{what}'; it may have run, so check before retrying", self.shared.label()),
                Ok(Ok(v)) => break v,
                Ok(Err(f)) if f.restarting() && Instant::now() < deadline => {
                    tracing::debug!("{} is restarting; sending '{what}' again", self.shared.label());
                    tokio::time::sleep(RESTART_RETRY_EVERY).await;
                }
                Ok(Err(f)) => return Err(CallError { error: f.error, code: f.code }.into()),
            }
        };
        serde_json::from_value(value).with_context(|| format!("unexpected reply from {}", self.shared.label()))
    }

    /// Receive daemon events from now on, across daemon restarts (see the module docs).
    pub async fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.shared.events.lock().unwrap() = Some(tx);
        let conn = self.shared.subscribe_current().await?;
        if !self.shared.supervising.swap(true, Ordering::SeqCst) {
            tokio::spawn(supervise(Arc::downgrade(&self.shared), conn.closed.subscribe()));
        }
        Ok(rx)
    }

    /// Receive mod options events (`OptionsEvent`) on this connection from now on. Call
    /// [`Self::subscribe`] too to keep receiving them across daemon restarts.
    pub async fn subscribe_options(&self) -> Result<mpsc::UnboundedReceiver<crate::options::OptionsEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.shared.options_events.lock().unwrap() = Some(tx);
        self.shared.subscribe_current().await?;
        Ok(rx)
    }

    /// Receive perf events (`PerfEvent`: subscription updates, HUD models, perf broadcasts)
    /// on this connection from now on. Per-connection state (perf subscriptions, a HUD
    /// attachment) doesn't survive a daemon restart: call [`Self::subscribe`] too and set it
    /// up again on [`Event::Reconnected`](crate::Event::Reconnected).
    pub async fn subscribe_perf(&self) -> Result<mpsc::UnboundedReceiver<crate::perf::PerfEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.shared.perf_events.lock().unwrap() = Some(tx);
        self.shared.subscribe_current().await?;
        Ok(rx)
    }
}

impl Shared {
    /// "quiverd", or "quiverd on Tower" for a remote one (for messages).
    fn label(&self) -> String {
        match &self.transport {
            Transport::Unix(_) => "quiverd".into(),
            Transport::Remote { machine, .. } => format!("quiverd on {}", machine.name),
        }
    }

    /// The open connection, reconnecting (and starting the daemon if needed) if it closed.
    async fn connection(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed() && !c.closing.load(Ordering::SeqCst)) {
            return Ok(c.clone());
        }
        let c = self.open(RESTART_GRACE).await?;
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Write a request, reconnecting and writing it again once if the write fails (the
    /// daemon never saw it). Returns the connection it went out on and its reply channel.
    async fn send(&self, request: &Request) -> Result<(Arc<Conn>, oneshot::Receiver<Result<serde_json::Value, Failure>>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::to_vec(&Envelope { id, source: self.source.clone(), request: request.clone() })?;
        line.push(b'\n');
        let mut attempt = 0;
        loop {
            let conn = self.connection().await?;
            let (tx, rx) = oneshot::channel();
            conn.pending.lock().unwrap().insert(id, tx);
            let written = async {
                let mut w = conn.writer.lock().await;
                w.write_all(&line).await?;
                w.flush().await
            }
            .await;
            match written {
                Ok(()) => return Ok((conn, rx)),
                Err(e) => {
                    conn.pending.lock().unwrap().remove(&id);
                    conn.close();
                    attempt += 1;
                    if attempt > 1 {
                        return Err(anyhow!(e).context("quiverd connection lost"));
                    }
                    tracing::debug!("quiverd connection lost ({e}); reconnecting");
                }
            }
        }
    }

    /// Subscribe on the current connection (if not already) and return it.
    async fn subscribe_current(&self) -> Result<Arc<Conn>> {
        let conn = self.connection().await?;
        if conn.subscribed.load(Ordering::SeqCst) {
            return Ok(conn);
        }
        let (conn, rx) = self.send(&Request::Subscribe).await?;
        rx.await.map_err(|_| anyhow!("{} went away while subscribing", self.label()))?.map_err(|f| anyhow!(f.error))?;
        conn.subscribed.store(true, Ordering::SeqCst);
        Ok(conn)
    }

    fn emit(&self, e: Event) -> bool {
        match self.events.lock().unwrap().as_ref() {
            Some(tx) => tx.send(e).is_ok(),
            None => false,
        }
    }
}

/// Keep a subscription alive: when its connection closes, tell the subscriber, reconnect
/// with backoff and subscribe again. Ends when the client or the subscriber goes away.
/// Holds no strong references, so dropping the client still closes its socket.
async fn supervise(shared: Weak<Shared>, closed: watch::Receiver<bool>) {
    resubscribe_forever(&shared, closed).await;
    if let Some(s) = shared.upgrade() {
        s.supervising.store(false, Ordering::SeqCst);
    }
}

async fn resubscribe_forever(shared: &Weak<Shared>, mut closed: watch::Receiver<bool>) {
    loop {
        // An error means the connection itself is gone, which counts as closed.
        let _ = closed.wait_for(|c| *c).await;
        let Some(s) = shared.upgrade() else { return };
        if !s.emit(Event::Disconnected) {
            return;
        }
        drop(s);
        let mut delay = BACKOFF_MIN;
        loop {
            tokio::time::sleep(delay).await;
            let Some(s) = shared.upgrade() else { return };
            match s.subscribe_current().await {
                Ok(c) => {
                    tracing::info!("reconnected to {}", s.label());
                    if !s.emit(Event::Reconnected) {
                        return;
                    }
                    closed = c.closed.subscribe();
                    break;
                }
                Err(e) => {
                    tracing::debug!("{} still unavailable: {e:#}", s.label());
                    if s.events.lock().unwrap().as_ref().is_none_or(|tx| tx.is_closed()) {
                        return;
                    }
                    delay = (delay * 2).min(BACKOFF_MAX);
                }
            }
        }
    }
}

impl Shared {
    /// Connect to the daemon: its socket, or the paired machine ([`open_remote`]).
    async fn open(&self, grace: Duration) -> Result<Arc<Conn>> {
        let (r, w): (Reader, Writer) = match &self.transport {
            Transport::Unix(path) => {
                let (r, w) = self.open_unix(path, grace).await?.into_split();
                (Box::new(r), Box::new(w))
            }
            Transport::Remote { dir, machine } => {
                let (r, w) = tokio::io::split(open_remote(dir, machine, grace).await?);
                (Box::new(r), Box::new(w))
            }
        };
        Ok(self.start_conn(r, w))
    }

    /// Connect to the daemon's socket. If nobody is listening, wait up to `grace` for a
    /// restarting daemon to come up (so two daemons don't race for the socket), then start one.
    async fn open_unix(&self, path: &std::path::Path, grace: Duration) -> Result<UnixStream> {
        let start = Instant::now();
        let stream = loop {
            match UnixStream::connect(path).await {
                Ok(s) => break s,
                Err(_) if start.elapsed() < grace => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) if !self.autostart => bail!("quiverd is not running ({e})"),
                Err(_) => {
                    let how = {
                        let path = path.to_owned();
                        tokio::task::spawn_blocking(move || start_daemon(&path)).await.context("starting quiverd")??
                    };
                    let mut last = None;
                    let mut stream = None;
                    for _ in 0..100 {
                        match UnixStream::connect(path).await {
                            Ok(s) => {
                                stream = Some(s);
                                break;
                            }
                            Err(e) => last = Some(e),
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    // quiverd writes its log itself however it started; a unit's also has
                    // what it printed before it could (the journal).
                    let see = match how {
                        crate::service::Autostart::Unit | crate::service::Autostart::Transient => {
                            format!("{} (and journalctl --user -u quiverd)", daemon_log().display())
                        }
                        _ => daemon_log().display().to_string(),
                    };
                    break stream.ok_or_else(|| anyhow!("quiverd did not start ({last:?}); see {see}"))?;
                }
            }
        };
        Ok(stream)
    }

    /// Read replies and events off a new connection.
    fn start_conn(&self, r: Reader, w: Writer) -> Arc<Conn> {
        let conn = Arc::new(Conn {
            writer: tokio::sync::Mutex::new(w),
            pending: Mutex::default(),
            closed: watch::Sender::new(false),
            subscribed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        });
        // Weak: dropping the client drops the write half, the daemon hangs up, and this ends.
        let reader = Arc::downgrade(&conn);
        let events = self.events.clone();
        let options_events = self.options_events.clone();
        let perf_events = self.perf_events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(reader) = reader.upgrade() else { return };
                match serde_json::from_str::<Reply>(&line) {
                    Ok(Reply::Ok { id, result }) => {
                        if let Some(tx) = reader.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(Ok(result));
                        }
                    }
                    Ok(Reply::Err { id, error, code }) => {
                        if let Some(tx) = reader.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(Err(Failure { error, code }));
                        }
                    }
                    // The daemon is restarting onto a new build: new requests go to a new
                    // connection (the new daemon accepts it), and what this one leaves
                    // unanswered is sent again (`Conn::close`). Subscribers reconnect now.
                    Ok(Reply::Closing { reason }) => {
                        tracing::debug!("quiverd is closing this connection: {reason}");
                        reader.closing.store(true, Ordering::SeqCst);
                        reader.closed.send_replace(true);
                    }
                    // Only subscribed connections get events. Options and perf events come as
                    // `Event`s (or, from older daemons, as `Reply::Options`/`Reply::Perf`):
                    // either way they reach `subscribe` and their own channel.
                    Ok(r @ (Reply::Event(_) | Reply::Options(_) | Reply::Perf(_))) => {
                        let Reply::Event(e) = r.folded() else { continue };
                        if let Some(tx) = options_events.lock().unwrap().as_ref()
                            && let Some(o) = e.options()
                        {
                            let _ = tx.send(o);
                        }
                        if let Some(tx) = perf_events.lock().unwrap().as_ref()
                            && let Some(p) = e.perf()
                        {
                            let _ = tx.send(p);
                        }
                        if let Some(tx) = events.lock().unwrap().as_ref() {
                            let _ = tx.send(e);
                        }
                    }
                    Err(_) => {}
                }
            }
            // Closed: whatever is still waiting fails, and the next call reconnects.
            if let Some(reader) = reader.upgrade() {
                reader.close();
            }
        });
        conn
    }
}

/// Connect to a paired machine: its address and key read again from `dir`'s peers (so a
/// revoked machine isn't reached and a moved one is), TLS with the key pinned, the API
/// hello. Retries for up to `grace` while it can't be reached; a refusal (no longer
/// paired there) fails at once.
async fn open_remote(dir: &std::path::Path, given: &quiver_remote::Machine, grace: Duration) -> Result<quiver_remote::ApiStream> {
    let start = Instant::now();
    loop {
        let attempt = async {
            let me = quiver_remote::Identity::load(dir)?;
            let peers = quiver_remote::Peers::load(dir)?;
            let peer = peers
                .by_pin(&given.pin, quiver_remote::Role::Server)
                .with_context(|| format!("this machine isn't paired with {} any more", given.name))?;
            let machine = peer.machine().unwrap_or_else(|| given.clone());
            quiver_remote::connect(&me, &machine, quiver_remote::wire::Intent::Api).await.map(|(s, _)| s)
        };
        match attempt.await {
            Ok(s) => return Ok(s),
            Err(e) if quiver_remote::wire::refused_code(&e).is_some() => return Err(e),
            Err(e) if start.elapsed() >= grace => return Err(e.context(format!("{} can't be reached", given.name))),
            Err(e) => {
                tracing::debug!("{} not reachable yet: {e:#}", given.name);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

pub fn daemon_log() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join("quiver/quiverd.log")
}

/// Start the `quiverd` for `socket` (next to this executable, else from PATH): as the
/// systemd unit, a transient service or a scope of its own, never in the caller's cgroup
/// ([`crate::service::start_daemon`]).
fn start_daemon(socket: &std::path::Path) -> Result<crate::service::Autostart> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("quiverd")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("quiverd"));
    crate::service::start_daemon(&crate::service::Systemd::from_env(), socket, &exe)
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;
    use tokio::sync::broadcast;
    use tokio::task::JoinHandle;

    use super::*;

    /// A stand-in daemon on its own socket: answers every request with `{"method": …}`,
    /// except `shutdown`, which it reads and then hangs up on without replying (a request
    /// that ran but whose reply was lost). `stop` + `start` is a restart. With
    /// `restarting` set it refuses everything as a restarting daemon does; `goodbyes`
    /// requests are answered with `Reply::Closing` and a hang-up instead.
    struct Fake {
        path: PathBuf,
        events: broadcast::Sender<Event>,
        received: Arc<Mutex<Vec<String>>>,
        tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
        mode: Arc<Mode>,
    }

    #[derive(Default)]
    struct Mode {
        restarting: AtomicBool,
        goodbyes: AtomicU64,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("quiver-api-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let fake = Self {
                path: dir.join("d.sock"),
                events: broadcast::channel(16).0,
                received: Arc::default(),
                tasks: Arc::default(),
                mode: Arc::default(),
            };
            fake.start();
            fake
        }

        fn start(&self) {
            let _ = std::fs::remove_file(&self.path);
            let listener = UnixListener::bind(&self.path).unwrap();
            let (events, received, tasks, mode) = (self.events.clone(), self.received.clone(), self.tasks.clone(), self.mode.clone());
            let accept = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let conn = tokio::spawn(serve(stream, events.clone(), received.clone(), mode.clone()));
                    tasks.lock().unwrap().push(conn);
                }
            });
            self.tasks.lock().unwrap().push(accept);
        }

        /// The daemon exits: the socket stops accepting and every connection drops.
        async fn stop(&self) {
            let tasks: Vec<_> = self.tasks.lock().unwrap().drain(..).collect();
            for t in tasks {
                t.abort();
                let _ = t.await;
            }
            let _ = std::fs::remove_file(&self.path);
        }

        fn count(&self, method: &str) -> usize {
            self.received.lock().unwrap().iter().filter(|m| *m == method).count()
        }
    }

    async fn serve(stream: UnixStream, events: broadcast::Sender<Event>, received: Arc<Mutex<Vec<String>>>, mode: Arc<Mode>) {
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let mut sub: Option<broadcast::Receiver<Event>> = None;
        loop {
            let reply = tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { return };
                    let env: Envelope = serde_json::from_str(&line).unwrap();
                    let m = method(&env.request);
                    received.lock().unwrap().push(m.clone());
                    if mode.goodbyes.load(Ordering::SeqCst) > 0 {
                        mode.goodbyes.fetch_sub(1, Ordering::SeqCst);
                        let mut line = serde_json::to_vec(&Reply::Closing { reason: "test".into() }).unwrap();
                        line.push(b'\n');
                        let _ = w.write_all(&line).await;
                        return;
                    }
                    if mode.restarting.load(Ordering::SeqCst) {
                        let reply = Reply::Err { id: env.id, error: "restarting".into(), code: Some(crate::daemon::RESTARTING.into()) };
                        let mut line = serde_json::to_vec(&reply).unwrap();
                        line.push(b'\n');
                        let _ = w.write_all(&line).await;
                        continue;
                    }
                    match env.request {
                        Request::Shutdown => return,
                        Request::Subscribe => sub = Some(events.subscribe()),
                        _ => {}
                    }
                    Reply::Ok { id: env.id, result: serde_json::json!({ "method": m }) }
                }
                Ok(e) = async { sub.as_mut().unwrap().recv().await }, if sub.is_some() => Reply::Event(e),
            };
            let mut line = serde_json::to_vec(&reply).unwrap();
            line.push(b'\n');
            if w.write_all(&line).await.is_err() {
                return;
            }
        }
    }

    async fn status(c: &Client) -> Result<String> {
        let v: serde_json::Value = c.call(Request::Status).await?;
        Ok(v["method"].as_str().unwrap_or_default().to_owned())
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("no event within 10 s").expect("stream ended")
    }

    /// Send events until the subscriber sees one (a resubscribe races the first sends).
    async fn deliver(fake: &Fake, rx: &mut mpsc::UnboundedReceiver<Event>) {
        for _ in 0..100 {
            let _ = fake.events.send(Event::LibraryChanged);
            if let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                assert!(matches!(e, Event::LibraryChanged), "{e:?}");
                return;
            }
        }
        panic!("events never arrived");
    }

    #[tokio::test]
    async fn requests_after_a_restart_succeed() {
        let fake = Fake::new("restart");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        assert_eq!(status(&client).await.unwrap(), "status");

        fake.stop().await;
        fake.start();
        // The old socket is dead; the call reconnects (or resends after a failed write).
        assert_eq!(status(&client).await.unwrap(), "status");
        assert_eq!(status(&client).await.unwrap(), "status");
        assert_eq!(fake.count("status"), 3, "nothing sent twice");
    }

    /// A restarting daemon refuses with `code: restarting`: the call is sent again until a
    /// daemon answers.
    #[tokio::test]
    async fn refused_while_restarting_is_retried() {
        let fake = Fake::new("refused");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        fake.mode.restarting.store(true, Ordering::SeqCst);
        let call = tokio::spawn({
            let client = client.clone();
            async move { status(&client).await }
        });
        tokio::time::sleep(Duration::from_millis(700)).await;
        fake.mode.restarting.store(false, Ordering::SeqCst);
        assert_eq!(call.await.unwrap().unwrap(), "status");
        assert!(fake.count("status") >= 3, "refused, then sent again: {}", fake.count("status"));
        // Other errors aren't retried.
        assert_eq!(fake.count("status"), fake.received.lock().unwrap().len());
    }

    /// After `Reply::Closing`, what wasn't answered is sent again on a new connection; a
    /// hang-up without one still isn't (see `a_lost_reply_is_not_resent`).
    #[tokio::test]
    async fn unanswered_before_closing_is_resent() {
        let fake = Fake::new("closing");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        let mut events = client.subscribe().await.unwrap();
        fake.mode.goodbyes.store(1, Ordering::SeqCst);
        assert_eq!(status(&client).await.unwrap(), "status");
        assert_eq!(fake.count("status"), 2, "once on the closing connection, once on the new one");
        // Subscribers see the restart and are subscribed again.
        assert!(matches!(next(&mut events).await, Event::Disconnected));
        assert!(matches!(next(&mut events).await, Event::Reconnected));
        deliver(&fake, &mut events).await;
    }

    #[tokio::test]
    async fn a_call_waits_out_a_short_outage() {
        let fake = Fake::new("outage");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        fake.stop().await;
        let restart = {
            let path = fake.path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                path
            })
        };
        let call = tokio::spawn({
            let client = client.clone();
            async move { status(&client).await }
        });
        restart.await.unwrap();
        fake.start();
        assert_eq!(call.await.unwrap().unwrap(), "status");
    }

    #[tokio::test]
    async fn a_lost_reply_is_not_resent() {
        let fake = Fake::new("lost");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        let err = client.call::<serde_json::Value>(Request::Shutdown).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("'shutdown'") && msg.contains("may have run"), "{msg}");
        assert_eq!(fake.count("shutdown"), 1);
        // The next call reconnects.
        assert_eq!(status(&client).await.unwrap(), "status");
        assert_eq!(fake.count("shutdown"), 1);
    }

    #[tokio::test]
    async fn down_daemon_errors_without_autostart() {
        let fake = Fake::new("down");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        fake.stop().await;
        let t = Instant::now();
        let err = status(&client).await.unwrap_err();
        assert!(format!("{err:#}").contains("not running"), "{err:#}");
        // It waited for a restart first.
        assert!(t.elapsed() >= RESTART_GRACE - Duration::from_millis(200));
    }

    #[tokio::test]
    async fn subscription_survives_a_restart() {
        let fake = Fake::new("sub");
        let client = Client::connect_at(fake.path.clone(), "test", false).await.unwrap();
        let mut events = client.subscribe().await.unwrap();
        deliver(&fake, &mut events).await;

        fake.stop().await;
        assert!(matches!(next(&mut events).await, Event::Disconnected));
        tokio::time::sleep(Duration::from_millis(700)).await; // a few failed attempts
        fake.start();
        assert!(matches!(next(&mut events).await, Event::Reconnected));
        assert_eq!(fake.count("subscribe"), 2);
        deliver(&fake, &mut events).await;
        assert_eq!(status(&client).await.unwrap(), "status");

        // And again; a call that reconnects first doesn't stop the resubscribe.
        fake.stop().await;
        fake.start();
        assert_eq!(status(&client).await.unwrap(), "status");
        assert!(matches!(next(&mut events).await, Event::Disconnected));
        assert!(matches!(next(&mut events).await, Event::Reconnected));
        deliver(&fake, &mut events).await;
        assert_eq!(fake.count("subscribe"), 3, "one subscribe per connection");
    }
}
