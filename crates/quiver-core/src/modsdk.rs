//! quiverd's side of the mod SDK (`sdk/quiver_mod.h`): which mods are connected to which
//! running game, what they may do, and where their messages go.
//!
//! A mod inside the game opens the pipe named by `QUIVER_MODS_PIPE`; `quiver-bridge` relays it
//! over a broker connection that starts with `BrokerRequest::ModChannel` (`quiver_ipc::modrelay`);
//! the broker hands that connection to [`ModRegistry::serve`]. From then on every message is
//! validated here (the game process is untrusted) and dispatched to two sinks:
//!
//! - [`ModOptionsSink`]: the Options agent's live-apply path (MOD-CONFIG §5). It supplies the
//!   `welcome` snapshot and hears `changed`; the other direction is
//!   [`ModRegistry::set_options`], which returns a per-option [`LiveOutcome`] once the mod
//!   acked, so `applied_live` is only ever reported for values a mod confirmed.
//! - [`ModMetricsSink`]: the perf hub (SIGHT-HUD §3.11). It gets validated descriptors with
//!   their full ids and indices, and validated samples and marks.
//!
//! [`LogOptionsSink`] and [`LogMetricsSink`] are the stand-ins until those land.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::Utc;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;
use quiver_api::mods::{ModSdkConnection, ModSdkMetric};
use quiver_ipc::modrelay::read_line_capped;
use quiver_perf::control::PerfMessage;
use quiver_perf::descriptor::{self as pd, CheckedValue, MetricDescriptor, MetricKind, SampleValue, limits as pl};
use quiver_proto::modsdk::{self as wire, FromMod, Hello, OptionValue, ToMod, Values};

/// A fresh session id (`QUIVER_SESSION`, the mod pipe's suffix): `s-` and 10 base-36 digits.
pub fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    // RandomState is seeded randomly per process; mix in a counter and the clock.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.write_u32(std::process::id());
    let mut v = h.finish();
    let mut s = String::from("s-");
    for _ in 0..10 {
        s.push(char::from_digit((v % 36) as u32, 36).unwrap());
        v /= 36;
    }
    s
}

/// Who a channel belongs to, from the bridge's `ModChannel` request.
#[derive(Debug, Clone, Default)]
pub struct ChannelOrigin {
    pub product_id: u32,
    pub account: Option<String>,
    pub session: Option<String>,
    pub profile: Option<String>,
    pub bridge_conn: u32,
    pub client_pid: Option<u32>,
}

/// Option values as a mod reported them (`changed`): id → text as the file would hold it.
pub type ChangedValues = BTreeMap<String, String>;

/// Where mod channels meet the Options backend (MOD-CONFIG §5).
pub trait ModOptionsSink: Send + Sync {
    /// The values `welcome` carries: the surface's effective values for the session's
    /// profile. Called once per connection, after `hello`.
    fn snapshot(&self, _m: &ModSdkConnection) -> Values {
        Values::new()
    }
    /// The live options a connection applies (`m.options`) were set or changed.
    fn handles(&self, _m: &ModSdkConnection) {}
    /// The mod sent its schema inline in `hello` (TOML or JSON, at most a frame's size,
    /// unvalidated). Called before [`snapshot`](Self::snapshot).
    fn inline_schema(&self, _m: &ModSdkConnection, _schema: &str) {}
    /// The mod changed values itself: its own menu (`persist`) or runtime state such as a
    /// hotkey flip (not persisted). Ids are syntax-checked, texts sanitized; checking them
    /// against the schema is the sink's job.
    fn changed(&self, m: &ModSdkConnection, values: &ChangedValues, persist: bool);
    fn connected(&self, _m: &ModSdkConnection) {}
    fn disconnected(&self, _m: &ModSdkConnection) {}
}

/// A metric a mod declared, validated, under its full id.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredMetric {
    pub index: u16,
    /// `<surface>.<name>` (= `def.id`).
    pub id: String,
    pub def: MetricDescriptor,
}

/// One validated value from an `m` message (`quiver_perf::descriptor::check_sample`: a state
/// is an index into `def.values`).
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub index: u16,
    pub id: String,
    pub kind: MetricKind,
    pub value: CheckedValue,
}

/// Where mod channels meet the perf hub (SIGHT-HUD §3.11).
pub trait ModMetricsSink: Send + Sync {
    /// Metrics were declared or updated (only the ones in this message).
    fn declared(&self, _m: &ModSdkConnection, _metrics: &[DeclaredMetric]) {}
    /// Values at `at` (QPC ticks as the mod reported them; 0 = not given).
    fn samples(&self, _m: &ModSdkConnection, _at: i64, _samples: &[Sample]) {}
    /// A mark on an `event` metric.
    fn mark(&self, _m: &ModSdkConnection, _metric: &DeclaredMetric, _at: i64, _dur: Option<i64>, _label: Option<&str>) {}
    fn disconnected(&self, _m: &ModSdkConnection) {}
}

/// Logs what a real Options backend would act on.
pub struct LogOptionsSink;

impl ModOptionsSink for LogOptionsSink {
    fn changed(&self, m: &ModSdkConnection, values: &ChangedValues, persist: bool) {
        tracing::info!(product_id = m.product_id, surface = %m.surface, persist, ?values, "mod changed its own options");
    }
    fn handles(&self, m: &ModSdkConnection) {
        tracing::info!(product_id = m.product_id, surface = %m.surface, options = ?m.options, "mod applies these options live");
    }
}

/// Logs declarations; samples go only to the registry's status (`quiver mods GAME sdk`).
pub struct LogMetricsSink;

impl ModMetricsSink for LogMetricsSink {
    fn declared(&self, m: &ModSdkConnection, metrics: &[DeclaredMetric]) {
        let ids: Vec<&str> = metrics.iter().map(|d| d.id.as_str()).collect();
        tracing::info!(product_id = m.product_id, surface = %m.surface, ?ids, "mod declared metrics");
    }
    fn mark(&self, m: &ModSdkConnection, metric: &DeclaredMetric, _at: i64, dur: Option<i64>, label: Option<&str>) {
        tracing::debug!(product_id = m.product_id, metric = %metric.id, ?dur, ?label, "mod mark");
    }
}

/// What became of one option in [`ModRegistry::set_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveOutcome {
    /// Every connection that handles it applied it and acked.
    Applied,
    /// A connection refused it (the mod's reason).
    Rejected(String),
    /// Connected, but no connection of the surface applies this option live.
    NotHandled,
    /// No connection of the surface is up.
    NoConnection,
    /// Sent, no ack in time.
    Timeout,
    /// The connection was busy or went away before acking.
    Dropped,
}

/// A connection came or went.
#[derive(Debug, Clone)]
pub struct ModEvent {
    pub connected: bool,
    pub conn: ModSdkConnection,
}

/// Limits beyond the wire's own (quiver_proto::modsdk).
const MAX_CONNECTIONS: usize = 64;
const MAX_PER_GAME: usize = 16;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REFUSED: u64 = 200;
const OUTBOX: usize = 64;

struct Pending {
    ids: Vec<String>,
    tx: oneshot::Sender<BTreeMap<String, String>>,
}

struct Conn {
    info: ModSdkConnection,
    tx: mpsc::Sender<ToMod>,
    next_seq: u64,
    pending: HashMap<u64, Pending>,
}

pub struct ModRegistry {
    options: RwLock<Arc<dyn ModOptionsSink>>,
    metrics: RwLock<Arc<dyn ModMetricsSink>>,
    conns: Mutex<BTreeMap<u64, Conn>>,
    next_id: AtomicU64,
    events: broadcast::Sender<ModEvent>,
}

/// A per-second message budget.
struct Window {
    start: Instant,
    used: u32,
    limit: u32,
}

impl Window {
    fn new(limit: u32) -> Self {
        Self { start: Instant::now(), used: 0, limit }
    }
    fn take(&mut self) -> bool {
        if self.start.elapsed() >= Duration::from_secs(1) {
            self.start = Instant::now();
            self.used = 0;
        }
        self.used += 1;
        self.used <= self.limit
    }
}

/// A connection's own state, owned by its task.
struct Session {
    id: u64,
    surface: String,
    defs: Vec<DeclaredMetric>,
    by_id: HashMap<String, u16>,
    last_total: HashMap<u16, u64>,
    m: Window,
    marks: Window,
    logs: Window,
    declares: Window,
    refused: u64,
}

impl Session {
    /// A declared metric by the index mods use: 1-based (`metrics_ok` hands out 1..=64);
    /// 0 is never valid.
    fn def(&self, index: u16) -> Option<&DeclaredMetric> {
        def_at(&self.defs, index)
    }
}

fn def_at(defs: &[DeclaredMetric], index: u16) -> Option<&DeclaredMetric> {
    defs.get(usize::from(index).checked_sub(1)?)
}

impl ModRegistry {
    pub fn new(options: Arc<dyn ModOptionsSink>, metrics: Arc<dyn ModMetricsSink>) -> Arc<Self> {
        Arc::new(Self {
            options: RwLock::new(options),
            metrics: RwLock::new(metrics),
            conns: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            events: broadcast::channel(64).0,
        })
    }

    /// The stand-in sinks: log, keep status.
    pub fn logging() -> Arc<Self> {
        Self::new(Arc::new(LogOptionsSink), Arc::new(LogMetricsSink))
    }

    /// Install the Options backend (connections made from now on use it).
    pub fn set_options_sink(&self, sink: Arc<dyn ModOptionsSink>) {
        *self.options.write().unwrap() = sink;
    }

    /// Install the perf hub.
    pub fn set_metrics_sink(&self, sink: Arc<dyn ModMetricsSink>) {
        *self.metrics.write().unwrap() = sink;
    }

    fn options_sink(&self) -> Arc<dyn ModOptionsSink> {
        self.options.read().unwrap().clone()
    }

    fn metrics_sink(&self) -> Arc<dyn ModMetricsSink> {
        self.metrics.read().unwrap().clone()
    }

    /// Connections come and go.
    pub fn subscribe(&self) -> broadcast::Receiver<ModEvent> {
        self.events.subscribe()
    }

    /// Connected mods, for one game or all, oldest first.
    pub fn connections(&self, product_id: Option<u32>) -> Vec<ModSdkConnection> {
        let conns = self.conns.lock().unwrap();
        conns.values().filter(|c| product_id.is_none_or(|p| c.info.product_id == p)).map(|c| c.info.clone()).collect()
    }

    /// The options some connection of `surface` applies live.
    pub fn live_options(&self, product_id: u32, surface: &str) -> BTreeSet<String> {
        let conns = self.conns.lock().unwrap();
        conns
            .values()
            .filter(|c| c.info.product_id == product_id && c.info.surface == surface)
            .flat_map(|c| c.info.options.iter().cloned())
            .collect()
    }

    /// Push changed values to the running mod(s) of `surface` and wait (at most `timeout`)
    /// for their acks. Each connection gets the values it said it handles; the outcome is
    /// per option.
    pub async fn set_options(&self, product_id: u32, surface: &str, values: &Values, timeout: Duration) -> BTreeMap<String, LiveOutcome> {
        let mut out: BTreeMap<String, LiveOutcome> = BTreeMap::new();
        let mut waits = Vec::new();
        {
            let mut conns = self.conns.lock().unwrap();
            let mine: Vec<&mut Conn> = conns.values_mut().filter(|c| c.info.product_id == product_id && c.info.surface == surface).collect();
            if mine.is_empty() {
                return values.keys().map(|k| (k.clone(), LiveOutcome::NoConnection)).collect();
            }
            for k in values.keys() {
                out.insert(k.clone(), LiveOutcome::NotHandled);
            }
            for c in mine {
                let subset: Values = values.iter().filter(|(k, _)| c.info.options.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
                if subset.is_empty() {
                    continue;
                }
                let ids: Vec<String> = subset.keys().cloned().collect();
                c.next_seq += 1;
                let seq = c.next_seq;
                if c.tx.try_send(ToMod::Set { seq, values: subset }).is_err() {
                    for id in ids {
                        out.insert(id, LiveOutcome::Dropped);
                    }
                    continue;
                }
                for id in &ids {
                    // Applied until some connection says otherwise.
                    out.entry(id.clone()).and_modify(|o| {
                        if *o == LiveOutcome::NotHandled {
                            *o = LiveOutcome::Applied;
                        }
                    });
                }
                let (tx, rx) = oneshot::channel();
                c.pending.insert(seq, Pending { ids: ids.clone(), tx });
                waits.push((ids, rx));
            }
        }
        let deadline = Instant::now() + timeout;
        for (ids, rx) in waits {
            let outcome = match tokio::time::timeout_at(deadline, rx).await {
                Ok(Ok(rejected)) => Some(rejected),
                Ok(Err(_)) => {
                    for id in &ids {
                        demote(&mut out, id, LiveOutcome::Dropped);
                    }
                    None
                }
                Err(_) => {
                    for id in &ids {
                        demote(&mut out, id, LiveOutcome::Timeout);
                    }
                    None
                }
            };
            if let Some(rejected) = outcome {
                for (id, reason) in rejected {
                    if ids.contains(&id) {
                        out.insert(id, LiveOutcome::Rejected(reason));
                    }
                }
            }
        }
        out
    }

    /// Run an action on every connection of `surface` that has the `actions` capability.
    /// Returns how many got it.
    pub fn invoke_action(&self, product_id: u32, surface: &str, id: &str, arg: Option<&str>) -> Result<usize> {
        if !wire::is_option_id(id) {
            bail!("'{id}' is not an action id");
        }
        let conns = self.conns.lock().unwrap();
        let mut n = 0;
        for c in conns.values().filter(|c| c.info.product_id == product_id && c.info.surface == surface) {
            if c.info.caps.iter().any(|x| x == "actions")
                && c.tx.try_send(ToMod::Action { id: id.to_owned(), arg: arg.map(str::to_owned) }).is_ok()
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Take mod channels on a listener of its own, as the broker would (tests, tools): each
    /// connection starts with a `BrokerRequest::ModChannel` line, answered `Ok`.
    pub async fn serve_listener(self: Arc<Self>, listener: tokio::net::TcpListener) -> Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let this = self.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut r = tokio::io::BufReader::new(r);
                let first = match read_line_capped(&mut r, 64 * 1024).await {
                    Ok(Some(l)) => l,
                    _ => return,
                };
                let Ok(quiver_ipc::BrokerRequest::ModChannel { uplay_id, account_id, session, profile, conn, client_pid }) = serde_json::from_slice(&first)
                else {
                    return;
                };
                if w.write_all(b"\"ok\"\n").await.is_err() {
                    return;
                }
                let origin = ChannelOrigin { product_id: uplay_id, account: account_id, session, profile, bridge_conn: conn, client_pid };
                if let Err(e) = this.serve(origin, r, w).await {
                    tracing::info!("mod channel ended: {e:#}");
                }
            });
        }
    }

    /// Serve one mod channel (the broker already answered `Ok`): `hello` → `welcome`, then
    /// messages both ways until either side closes.
    pub async fn serve<R, W>(self: &Arc<Self>, origin: ChannelOrigin, mut reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let first = match tokio::time::timeout(HELLO_TIMEOUT, read_line_capped(&mut reader, wire::MAX_LINE)).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => bail!("no hello in time"),
        };
        let hello = match serde_json::from_slice::<FromMod>(&first) {
            Ok(FromMod::Hello(h)) => h,
            Ok(_) => bail!("the first message was not hello"),
            Err(e) => bail!("bad hello: {e}"),
        };
        let (info, granted) = match self.admit(&origin, &hello) {
            Ok(x) => x,
            Err(reason) => {
                tracing::warn!(product_id = origin.product_id, "mod channel refused: {reason}");
                send(&mut writer, &ToMod::Bye { reason: Some(reason.clone()) }).await?;
                bail!(reason);
            }
        };
        let id = info.conn;
        let (tx, mut rx) = mpsc::channel(OUTBOX);
        let values = if granted.iter().any(|c| c == "options") {
            let sink = self.options_sink();
            if let Some(schema) = &hello.schema {
                sink.inline_schema(&info, schema);
            }
            sink.snapshot(&info)
        } else {
            Values::new()
        };
        let welcome = ToMod::Welcome(wire::Welcome {
            proto: info.proto,
            session: info.session.clone(),
            profile: info.profile.clone(),
            values,
            caps: granted.clone(),
            // No rings yet (SIGHT-HUD phase 2), so no session dir to name; metrics are JSON
            // with QPC timestamps.
            perf: None,
            limits: (info.proto >= 2).then(wire::Limits::current),
        });
        self.conns.lock().unwrap().insert(id, Conn { info: info.clone(), tx, next_seq: 0, pending: HashMap::new() });
        tracing::info!(
            product_id = info.product_id,
            surface = %info.surface,
            conn = id,
            proto = info.proto,
            sdk = %info.sdk,
            caps = ?granted,
            "mod connected through the SDK"
        );
        self.options_sink().connected(&info);
        let _ = self.events.send(ModEvent { connected: true, conn: info.clone() });

        let mut s = Session {
            id,
            surface: info.surface.clone(),
            defs: Vec::new(),
            by_id: HashMap::new(),
            last_total: HashMap::new(),
            // A little slack over the SDK's own coalescing, for jitter.
            m: Window::new(pl::MAX_JSON_HZ + 5),
            marks: Window::new(pl::MAX_MARKS_PER_SEC),
            logs: Window::new(10),
            declares: Window::new(4),
            refused: 0,
        };
        let result: Result<()> = async {
            send(&mut writer, &welcome).await?;
            loop {
                tokio::select! {
                    line = read_line_capped(&mut reader, wire::MAX_LINE) => {
                        let Some(line) = line? else { return Ok(()) };
                        match parse(&line) {
                            Ok(Incoming::Mod(FromMod::Bye)) => return Ok(()),
                            Ok(Incoming::Mod(msg)) => {
                                if let Some(reply) = self.handle(&mut s, msg) {
                                    send(&mut writer, &reply).await?;
                                }
                            }
                            Ok(Incoming::Perf(msg)) => {
                                if let Some(reply) = self.handle_perf(&mut s, msg) {
                                    send(&mut writer, &reply).await?;
                                }
                            }
                            Err(e) => {
                                tracing::debug!(conn = id, "malformed mod message: {e}");
                                s.refused += 1;
                            }
                        }
                        if s.refused > MAX_REFUSED {
                            send(&mut writer, &ToMod::Bye { reason: Some("too many refused messages".into()) }).await?;
                            bail!("too many refused messages");
                        }
                    }
                    Some(out) = rx.recv() => send(&mut writer, &out).await?,
                }
            }
        }
        .await;
        let gone = self.conns.lock().unwrap().remove(&id);
        if let Some(c) = gone {
            // Pending acks resolve as Dropped (their senders drop here).
            tracing::info!(product_id = c.info.product_id, surface = %c.info.surface, conn = id, refused = s.refused, "mod disconnected: {result:?}");
            self.options_sink().disconnected(&c.info);
            self.metrics_sink().disconnected(&c.info);
            let _ = self.events.send(ModEvent { connected: false, conn: c.info });
        }
        result
    }

    fn admit(&self, origin: &ChannelOrigin, hello: &Hello) -> std::result::Result<(ModSdkConnection, Vec<String>), String> {
        if hello.proto == 0 {
            return Err("proto must be at least 1".into());
        }
        if !wire::is_surface_id(&hello.surface) {
            return Err("`mod` must be a surface id ([a-z][a-z0-9_-], at most 48)".into());
        }
        let proto = hello.proto.min(wire::PROTOCOL);
        // Protocol 1 SDKs had options (and readouts) only.
        let wanted: Vec<&str> = if proto < 2 { vec!["options", "log"] } else { hello.caps.iter().map(String::as_str).collect() };
        let granted: Vec<String> = wire::CAPS.iter().filter(|c| wanted.contains(c)).map(|c| (*c).to_owned()).collect();
        let module = hello.module.as_deref().map(|m| wire::sanitize(m.rsplit(['\\', '/']).next().unwrap_or(m), 64));
        let conns = self.conns.lock().unwrap();
        if conns.len() >= MAX_CONNECTIONS || conns.values().filter(|c| c.info.product_id == origin.product_id).count() >= MAX_PER_GAME {
            return Err("too many mod connections".into());
        }
        let info = ModSdkConnection {
            conn: self.next_id.fetch_add(1, Ordering::Relaxed),
            product_id: origin.product_id,
            surface: hello.surface.clone(),
            sdk: wire::sanitize(&hello.sdk, 64),
            proto,
            caps: granted.clone(),
            pid: hello.pid,
            client_pid: origin.client_pid,
            module,
            version: hello.version.as_deref().map(|v| wire::sanitize(v, 32)),
            session: origin.session.clone(),
            profile: origin.profile.clone(),
            account: origin.account.clone(),
            connected: Utc::now(),
            inline_schema: hello.schema.is_some(),
            ..Default::default()
        };
        Ok((info, granted))
    }

    /// Update the shared view of a connection.
    fn with_info(&self, id: u64, f: impl FnOnce(&mut ModSdkConnection)) -> Option<ModSdkConnection> {
        let mut conns = self.conns.lock().unwrap();
        let c = conns.get_mut(&id)?;
        f(&mut c.info);
        Some(c.info.clone())
    }

    fn info(&self, id: u64) -> Option<ModSdkConnection> {
        self.with_info(id, |_| {})
    }

    fn has_cap(&self, id: u64, cap: &str) -> bool {
        self.conns.lock().unwrap().get(&id).is_some_and(|c| c.info.caps.iter().any(|x| x == cap))
    }

    fn refuse(&self, s: &mut Session, why: &str) {
        tracing::debug!(conn = s.id, "refused mod message: {why}");
        s.refused += 1;
        self.with_info(s.id, |i| i.refused = s.refused);
    }

    /// Handle one options message after `hello`; maybe a reply.
    fn handle(&self, s: &mut Session, msg: FromMod) -> Option<ToMod> {
        match msg {
            FromMod::Hello(_) => self.refuse(s, "second hello"),
            FromMod::Options { ids } => {
                if ids.len() > wire::MAX_OPTIONS || !ids.iter().all(|i| wire::is_option_id(i)) {
                    self.refuse(s, "bad option ids");
                    return None;
                }
                let ids: BTreeSet<String> = ids.into_iter().collect();
                if let Some(info) = self.with_info(s.id, |i| i.options = ids.into_iter().collect()) {
                    self.options_sink().handles(&info);
                }
            }
            FromMod::Ack { seq, rejected } => {
                let mut conns = self.conns.lock().unwrap();
                let p = conns.get_mut(&s.id).and_then(|c| c.pending.remove(&seq))?;
                drop(conns);
                let rejected: BTreeMap<String, String> = rejected
                    .into_iter()
                    .filter(|(k, _)| p.ids.contains(k))
                    .map(|(k, v)| (k, wire::sanitize(&v, 200)))
                    .collect();
                let _ = p.tx.send(rejected);
            }
            FromMod::Changed { values, persist } => {
                if values.len() > wire::MAX_OPTIONS || !values.keys().all(|k| wire::is_option_id(k)) {
                    self.refuse(s, "bad option ids in changed");
                    return None;
                }
                let values: ChangedValues = values.iter().map(|(k, v)| (k.clone(), wire::sanitize(v.text(), wire::MAX_OPTION_TEXT))).collect();
                if let Some(info) = self.info(s.id) {
                    self.options_sink().changed(&info, &values, persist);
                }
            }
            FromMod::Readout { values } => {
                // Protocol 1: each id becomes a `text` metric (SIGHT-HUD §3.13).
                if !s.m.take() {
                    self.refuse(s, "readouts over the rate");
                    return None;
                }
                let mut samples = Vec::new();
                let mut new = Vec::new();
                for (name, text) in values.into_iter().take(pl::MAX_METRICS_PER_PRODUCER) {
                    let id = pd::qualify(&s.surface, &name);
                    let index = match s.by_id.get(&id) {
                        Some(&i) => i,
                        None => match self.add_metric(s, MetricDescriptor::new(id.clone(), MetricKind::Text)) {
                            Ok(d) => {
                                new.push(d.clone());
                                d.index
                            }
                            Err(_) => continue,
                        },
                    };
                    let Some(d) = s.def(index) else { continue };
                    if let Ok(value) = pd::check_sample(&d.def, &SampleValue::Text(text)) {
                        samples.push(Sample { index, id: d.id.clone(), kind: d.def.kind, value });
                    }
                }
                self.publish_defs(s, &new);
                self.deliver(s, 0, samples);
            }
            FromMod::Log { level, text } => {
                if !s.logs.take() {
                    s.refused += 1;
                    return None;
                }
                let text = wire::sanitize(&text, wire::MAX_LOG_TEXT);
                match level {
                    0 => tracing::error!(target: "quiver::mod", surface = %s.surface, "{text}"),
                    1 => tracing::warn!(target: "quiver::mod", surface = %s.surface, "{text}"),
                    2 => tracing::info!(target: "quiver::mod", surface = %s.surface, "{text}"),
                    _ => tracing::debug!(target: "quiver::mod", surface = %s.surface, "{text}"),
                }
            }
            FromMod::Bye | FromMod::Unknown => {}
        }
        None
    }

    /// Handle one perf message (`quiver_perf::control`); maybe a reply.
    fn handle_perf(&self, s: &mut Session, msg: PerfMessage<serde_json::Value>) -> Option<PerfMessage<MetricDescriptor>> {
        if !self.has_cap(s.id, "metrics") {
            self.refuse(s, "metrics without the capability");
            return None;
        }
        match msg {
            PerfMessage::Metrics { defs } => {
                if !s.declares.take() {
                    let rejected = defs.iter().enumerate().map(|(i, d)| (def_key(d, i), "too many declarations; retry in a second".to_owned())).collect();
                    return Some(PerfMessage::MetricsOk { ids: BTreeMap::new(), rejected });
                }
                return Some(self.declare(s, defs));
            }
            PerfMessage::M { ts, v } => {
                if !s.m.take() {
                    self.refuse(s, "m over the rate");
                    return None;
                }
                let mut samples = Vec::with_capacity(v.len());
                for (index, val) in v.iter().take(pl::MAX_METRICS_PER_PRODUCER) {
                    let Some(d) = def_at(&s.defs, *index) else {
                        s.refused += 1;
                        continue;
                    };
                    let Ok(value) = pd::check_sample(&d.def, val) else {
                        s.refused += 1;
                        continue;
                    };
                    if let CheckedValue::Counter(t) = value {
                        let last = s.last_total.insert(d.index, t);
                        if last.is_some_and(|l| t < l) {
                            tracing::debug!(metric = %d.id, "counter went backwards (the mod restarted it)");
                        }
                    }
                    samples.push(Sample { index: d.index, id: d.id.clone(), kind: d.def.kind, value });
                }
                self.deliver(s, ts, samples);
            }
            PerfMessage::Mark { index, ts, dur, label } => {
                if !s.marks.take() {
                    self.refuse(s, "marks over the rate");
                    return None;
                }
                let Some(d) = s.def(index).filter(|d| d.def.kind == MetricKind::Event).cloned() else {
                    self.refuse(s, "mark on a metric that isn't an event");
                    return None;
                };
                let label = label.map(|l| pd::sanitize_text(&l, pl::MAX_TEXT_BYTES));
                let dur = dur.filter(|d| *d >= 0);
                if let Some(info) = self.with_info(s.id, |i| {
                    if let Some(m) = i.metrics.iter_mut().find(|m| m.index == u32::from(index)) {
                        m.marks += 1;
                        m.updated = Some(Utc::now());
                        if let Some(l) = &label {
                            m.value = Some(serde_json::Value::String(l.clone()));
                        }
                    }
                }) {
                    self.metrics_sink().mark(&info, &d, ts, dur, label.as_deref());
                }
            }
            // Rings are SIGHT-HUD phase 2; the rest only ever go to the mod.
            PerfMessage::RingReq { .. } => self.refuse(s, "no rings yet"),
            PerfMessage::MetricsOk { .. } | PerfMessage::Ring { .. } | PerfMessage::PerfCtl { .. } => self.refuse(s, "not a mod's message"),
        }
        None
    }

    /// Validate and add (or update) one runtime declaration (`quiver_perf::descriptor`'s rules,
    /// plus phase 1's: no ring kinds but events, which also travel as JSON marks).
    fn add_metric(&self, s: &mut Session, mut def: MetricDescriptor) -> std::result::Result<DeclaredMetric, String> {
        def.sanitize();
        def.validate(Some(&s.surface)).map_err(|e| e.to_string())?;
        if matches!(def.kind, MetricKind::Series | MetricKind::Frame) {
            return Err(format!("{} metrics need a shared-memory ring, which this Quiver doesn't offer yet", kind_name(def.kind)));
        }
        if let Some(&i) = s.by_id.get(&def.id) {
            let old = &mut s.defs[usize::from(i) - 1];
            if old.def.kind != def.kind {
                return Err(format!("already declared as {}", kind_name(old.def.kind)));
            }
            old.def = def;
            return Ok(old.clone());
        }
        if s.defs.len() >= pl::MAX_METRICS_PER_PRODUCER {
            return Err(format!("more than {} metrics", pl::MAX_METRICS_PER_PRODUCER));
        }
        // 1-based: index 0 is never valid in ring records (quiver_perf::layout), so the
        // indices mods get here are the ones their rings will use.
        let d = DeclaredMetric { index: s.defs.len() as u16 + 1, id: def.id.clone(), def };
        s.by_id.insert(d.id.clone(), d.index);
        s.defs.push(d.clone());
        Ok(d)
    }

    fn declare(&self, s: &mut Session, defs: Vec<serde_json::Value>) -> PerfMessage<MetricDescriptor> {
        let mut ids = BTreeMap::new();
        let mut rejected = BTreeMap::new();
        let mut accepted = Vec::new();
        for (i, raw) in defs.into_iter().enumerate().take(pl::MAX_METRICS_PER_PRODUCER * 2) {
            let key = def_key(&raw, i);
            match serde_json::from_value::<MetricDescriptor>(raw).map_err(|e| format!("malformed: {e}")).and_then(|d| self.add_metric(s, d)) {
                Ok(d) => {
                    ids.insert(d.id.clone(), d.index);
                    accepted.push(d);
                }
                Err(why) => {
                    rejected.insert(key, why);
                }
            }
        }
        self.publish_defs(s, &accepted);
        PerfMessage::MetricsOk { ids, rejected }
    }

    /// Show newly declared metrics in the status, and tell the metrics sink.
    fn publish_defs(&self, s: &Session, accepted: &[DeclaredMetric]) {
        if accepted.is_empty() {
            return;
        }
        let defs = &s.defs;
        if let Some(info) = self.with_info(s.id, |i| {
            for d in defs {
                let unit = serde_json::to_value(d.def.unit).ok().and_then(|u| u.as_str().map(str::to_owned)).unwrap_or_default();
                match i.metrics.iter_mut().find(|m| m.index == u32::from(d.index)) {
                    Some(m) => {
                        m.unit = unit;
                        m.label = d.def.label.clone();
                    }
                    None => i.metrics.push(ModSdkMetric {
                        index: d.index.into(),
                        id: d.id.clone(),
                        kind: kind_name(d.def.kind),
                        unit,
                        label: d.def.label.clone(),
                        ..Default::default()
                    }),
                }
            }
        }) {
            self.metrics_sink().declared(&info, accepted);
        }
    }

    fn deliver(&self, s: &Session, ts: i64, samples: Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        let now = Utc::now();
        let shown = |smp: &Sample| -> serde_json::Value {
            match &smp.value {
                CheckedValue::Gauge(x) => serde_json::json!(x),
                CheckedValue::Counter(n) => serde_json::json!(n),
                CheckedValue::State(i) => serde_json::json!(s.def(smp.index).and_then(|d| d.def.values.get(*i)).map(|v| v.value.clone())),
                CheckedValue::Text(t) => serde_json::json!(t),
                CheckedValue::Histogram(h) => serde_json::json!({ "edges": h.edges, "counts": h.counts }),
            }
        };
        let Some(info) = self.with_info(s.id, |i| {
            i.refused = s.refused;
            for smp in &samples {
                if let Some(m) = i.metrics.iter_mut().find(|m| m.index == u32::from(smp.index)) {
                    m.value = Some(shown(smp));
                    m.updated = Some(now);
                }
            }
        }) else {
            return;
        };
        self.metrics_sink().samples(&info, ts, &samples);
    }
}

/// A message from a mod: the options protocol's, or a perf one (`quiver_perf::control`).
enum Incoming {
    Mod(FromMod),
    Perf(PerfMessage<serde_json::Value>),
}

fn parse(line: &[u8]) -> serde_json::Result<Incoming> {
    let v: serde_json::Value = serde_json::from_slice(line)?;
    if v.get("t").and_then(|t| t.as_str()).is_some_and(wire::is_perf_message) {
        Ok(Incoming::Perf(serde_json::from_value(v)?))
    } else {
        Ok(Incoming::Mod(serde_json::from_value(v)?))
    }
}

/// `gauge`, `state`, … as on the wire.
fn kind_name(k: MetricKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// A descriptor's id for `metrics_ok.rejected`, even when it doesn't parse.
fn def_key(raw: &serde_json::Value, i: usize) -> String {
    raw.get("id")
        .and_then(|n| n.as_str())
        .map(|n| pd::sanitize_text(n, pl::MAX_ID_LEN))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("#{i}"))
}

fn demote(out: &mut BTreeMap<String, LiveOutcome>, id: &str, to: LiveOutcome) {
    if let Some(o) = out.get_mut(id)
        && matches!(o, LiveOutcome::Applied)
    {
        *o = to;
    }
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, msg: &impl serde::Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// Convenience for sinks and tests: a value whose text and JSON agree.
pub fn value(text: &str, json: serde_json::Value) -> OptionValue {
    OptionValue { text: text.to_owned(), json }
}

#[cfg(test)]
mod tests;
