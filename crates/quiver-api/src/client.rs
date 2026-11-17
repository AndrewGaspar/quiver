//! Async client for `quiverd`. Starts the daemon if it isn't running, and survives the
//! daemon restarting:
//!
//! - A request whose write fails never reached the daemon, so it is sent again once on a
//!   new connection.
//! - If the connection drops after a request was sent but before its reply, the request
//!   may have run, so it is not resent: the call fails with an error saying so, and the
//!   next call reconnects.
//! - A subscription is renewed on every new connection. Subscribers see
//!   [`Event::Disconnected`] when the daemon goes away and [`Event::Reconnected`] once
//!   events flow again (anything that happened in between was missed; reload state).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{Envelope, Event, Reply, Request, socket_path};

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>;
type Subscriber = Arc<Mutex<Option<mpsc::UnboundedSender<Event>>>>;

/// How long a reconnect waits for a restarting daemon before starting one itself.
const RESTART_GRACE: Duration = Duration::from_secs(3);
/// Background reconnect backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

struct Shared {
    path: PathBuf,
    /// Start the daemon when nobody is listening (off in tests).
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
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Pending,
    /// Flips to true once the socket closed (the reader saw EOF or a write failed).
    closed: watch::Sender<bool>,
    /// `Subscribe` was sent on this connection.
    subscribed: AtomicBool,
}

impl Conn {
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Mark closed and fail every request still waiting for a reply.
    fn close(&self) {
        self.closed.send_replace(true);
        self.pending.lock().unwrap().clear();
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

    pub(crate) async fn connect_at(path: PathBuf, source: &str, autostart: bool) -> Result<Self> {
        let shared = Arc::new(Shared {
            path,
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

    /// Send a request and decode its result.
    pub async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let what = method(&request);
        let (_conn, rx) = self.shared.send(request).await?;
        let value = rx
            .await
            .map_err(|_| {
                anyhow!("quiverd went away (restarted?) before answering '{what}'; it may have run, so check before retrying")
            })?
            .map_err(|e| anyhow!(e))?;
        serde_json::from_value(value).context("unexpected reply from quiverd")
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
    /// The open connection, reconnecting (and starting the daemon if needed) if it closed.
    async fn connection(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let c = self.open(RESTART_GRACE).await?;
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Write a request, reconnecting and writing it again once if the write fails (the
    /// daemon never saw it). Returns the connection it went out on and its reply channel.
    async fn send(
        &self,
        request: Request,
    ) -> Result<(Arc<Conn>, oneshot::Receiver<Result<serde_json::Value, String>>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::to_vec(&Envelope { id, source: self.source.clone(), request })?;
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
        let (conn, rx) = self.send(Request::Subscribe).await?;
        rx.await.map_err(|_| anyhow!("quiverd went away while subscribing"))?.map_err(|e| anyhow!(e))?;
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
                    tracing::info!("reconnected to quiverd");
                    if !s.emit(Event::Reconnected) {
                        return;
                    }
                    closed = c.closed.subscribe();
                    break;
                }
                Err(e) => {
                    tracing::debug!("quiverd still unavailable: {e:#}");
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
    /// Connect to the daemon's socket. If nobody is listening, wait up to `grace` for a
    /// restarting daemon to come up (so two daemons don't race for the socket), then start one.
    async fn open(&self, grace: Duration) -> Result<Arc<Conn>> {
        let path = &self.path;
        let start = Instant::now();
        let stream = loop {
            match UnixStream::connect(path).await {
                Ok(s) => break s,
                Err(_) if start.elapsed() < grace => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) if !self.autostart => bail!("quiverd is not running ({e})"),
                Err(_) => {
                    spawn_daemon()?;
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
                    break stream
                        .ok_or_else(|| anyhow!("quiverd did not start ({last:?}); see {}", daemon_log().display()))?;
                }
            }
        };
        let (r, w) = stream.into_split();
        let conn = Arc::new(Conn {
            writer: tokio::sync::Mutex::new(w),
            pending: Mutex::default(),
            closed: watch::Sender::new(false),
            subscribed: AtomicBool::new(false),
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
                    Ok(Reply::Err { id, error }) => {
                        if let Some(tx) = reader.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(Err(error));
                        }
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
        Ok(conn)
    }
}

pub fn daemon_log() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join("quiver/quiverd.log")
}

/// Start `quiverd` in the background: next to this executable, else from PATH.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("quiverd")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("quiverd"));
    let log = daemon_log();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.stdin(std::process::Stdio::null()).stdout(out.try_clone()?).stderr(out);
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the client's terminal closing
    }
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(e) => bail!("could not start {}: {e}", exe.display()),
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;
    use tokio::sync::broadcast;
    use tokio::task::JoinHandle;

    use super::*;

    /// A stand-in daemon on its own socket: answers every request with `{"method": …}`,
    /// except `shutdown`, which it reads and then hangs up on without replying (a request
    /// that ran but whose reply was lost). `stop` + `start` is a restart.
    struct Fake {
        path: PathBuf,
        events: broadcast::Sender<Event>,
        received: Arc<Mutex<Vec<String>>>,
        tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
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
            };
            fake.start();
            fake
        }

        fn start(&self) {
            let _ = std::fs::remove_file(&self.path);
            let listener = UnixListener::bind(&self.path).unwrap();
            let (events, received, tasks) = (self.events.clone(), self.received.clone(), self.tasks.clone());
            let accept = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let conn = tokio::spawn(serve(stream, events.clone(), received.clone()));
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

    async fn serve(stream: UnixStream, events: broadcast::Sender<Event>, received: Arc<Mutex<Vec<String>>>) {
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
