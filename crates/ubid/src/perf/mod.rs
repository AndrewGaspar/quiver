//! quiverd's perf hub (design/SIGHT-HUD.md §3.11, phase 1).
//!
//! Per running session: a [`registry::Store`] (catalog, roles, values), the HUD's state
//! (settings resolved at launch, shown/hidden, layout, presenter, model revision) and the
//! session dir with `hud.image` when the HUD is enabled for the game. Producers:
//!
//! - the Sight layer's ring, drained every 50 ms on its own thread ([`rings`]);
//! - the system and display producers, polled at 1 Hz on another ([`system`], [`display`]),
//!   only while a session runs;
//! - mods, over the SDK pipe through the mod registry ([`mods::PerfModSink`]).
//!
//! Consumers: per-connection subscriptions (`PerfEvent::Update`), `quiver-sight`
//! connections attached to a session (`PerfEvent::HudModel`), and the summary recorded
//! with the play session. A tokio task publishes both every 100 ms.

pub mod api;
pub mod catalog;
pub mod display;
pub mod format;
pub mod hud;
pub mod hudcfg;
pub mod keybinds;
pub mod launch;
pub mod layout;
pub mod mods;
pub mod nvml;
pub mod registry;
pub mod rings;
pub mod sigbus;
pub mod stats;
pub mod system;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use ubi_api::Reply;
use ubi_api::perf::{
    HudConfig, HudModel, HudStatus, LayerInfo, MetricInfo, PerfEvent, PerfSessionInfo, PerfSummary, PerfUpdate, Presenter, Stat,
};
use ubi_perf::clock::mono_raw_ns;
use ubi_perf::session::{SessionDir, SessionId};

use hud::ImageView;
use registry::Store;
use rings::LayerRing;

/// `/dev/shm`, or `QUIVER_SHM_ROOT` (tests, a scratch daemon).
pub fn shm_root() -> PathBuf {
    std::env::var_os("QUIVER_SHM_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| ubi_perf::session::SHM_ROOT.into())
}

/// The HUD image's buffer size: room for Full at 2× on a 1600p swapchain (≈ 1000 × 1000
/// px), within the 4 MiB cap; tmpfs allocates only the pages quiver-sight writes.
pub fn hud_buffer_bytes() -> u32 {
    ubi_perf::hud::buffer_bytes_for(1024, 1000).unwrap_or(ubi_perf::layout::HUD_MAX_BUFFER_BYTES)
}

const PUBLISH_EVERY: Duration = Duration::from_millis(100);
const MAX_SUBS_PER_CONN: usize = 8;
const MAX_METRICS_PER_SESSION: usize = ubi_perf::descriptor::limits::MAX_METRICS_PER_SESSION;

struct Sub {
    id: u64,
    session: String,
    select: Vec<String>,
    every_ns: i64,
    window_ns: i64,
    stats: Vec<Stat>,
    next_ns: i64,
}

struct Conn {
    tx: mpsc::UnboundedSender<Reply>,
    subs: Vec<Sub>,
}

/// A connection, as the hub sees it.
#[derive(Clone)]
pub struct ConnCtx {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Reply>,
}

pub struct Session {
    pub id: String,
    pub product_id: u32,
    pub profile: Option<String>,
    pub dev: bool,
    pub dir: Option<SessionDir>,
    pub image: Option<PathBuf>,
    pub store: Store,
    pub cfg: HudConfig,
    pub on: bool,
    pub layout: String,
    pub presenter: Option<(Presenter, String)>,
    pub layers: Vec<LayerRing>,
    pub image_view: Option<ImageView>,
    pub model: Option<HudModel>,
    pub model_rev: u64,
    next_model_ns: i64,
    dirty: bool,
    pub attached: BTreeSet<u64>,
    /// Game processes (Linux pids), from the launcher.
    pub pids: Vec<u32>,
    pub prefix: Option<PathBuf>,
    pub last_error: Option<String>,
    /// Mod connections per surface (a surface's producer disconnects with its last one).
    pub mod_conns: HashMap<String, BTreeSet<u64>>,
    /// The game is flagged anti-cheat: no layer, desktop presenter (decision 16). No source
    /// of the flag yet; kept for the presenter rule.
    pub anti_cheat: bool,
    pub monitor: Option<String>,
    /// The presenter while shown, as last accounted for the summary.
    noted: Option<Presenter>,
}

impl Session {
    /// The layer ring that's alive (the newest), for the presenter and status.
    pub fn live_layer(&self) -> Option<&LayerRing> {
        self.layers.iter().rev().find(|l| l.alive && !l.ring.starts_with("invalid")).or_else(|| self.layers.last())
    }

    pub fn layer_info(&self, now: i64) -> Option<LayerInfo> {
        let l = self.live_layer()?;
        let s = l.status.unwrap_or(ubi_perf::layout::LayerStatus::ZERO);
        Some(LayerInfo {
            pid: l.pid,
            alive: l.alive,
            heartbeat_age_ms: (l.heartbeat > 0).then(|| ((now - l.heartbeat).max(0) / 1_000_000) as u64),
            presents: s.presents,
            drawn: s.drawn,
            drawing: s.flags & ubi_perf::layout::LAYER_DRAWING != 0,
            draw_error: hud::draw_error_name(s.draw_error, s.flags).into(),
            extent: (s.extent_w > 0).then_some([s.extent_w, s.extent_h]),
            vk_format: s.vk_format,
            color_space: s.color_space,
            present_mode: s.present_mode,
            hud_published: s.hud_published,
            engine: ubi_perf::layout::LayerStatus::get_str(&s.engine_name).map(|x| ubi_perf::descriptor::sanitize_text(x, 32)).unwrap_or_default(),
            device: ubi_perf::layout::LayerStatus::get_str(&s.device_name).map(|x| ubi_perf::descriptor::sanitize_text(x, 64)).unwrap_or_default(),
            vendor_id: s.vendor_id,
        })
    }

    fn info(&self) -> PerfSessionInfo {
        PerfSessionInfo {
            session: self.id.clone(),
            product_id: self.product_id,
            profile: self.profile.clone(),
            started: self.store.started_at,
            dir: self.dir.as_ref().map(|d| d.path().to_owned()),
            hud_enabled: self.cfg.enabled,
            producers: self.store.producer_infos(),
            presenter: self.cfg.enabled.then(|| self.presenter.as_ref().map(|p| p.0)).flatten(),
            dev: self.dev,
        }
    }

    fn status(&self, now: i64) -> HudStatus {
        HudStatus {
            session: self.id.clone(),
            product_id: self.product_id,
            enabled: self.cfg.enabled,
            on: self.on,
            layout: self.layout.clone(),
            presenter: self.presenter.as_ref().map(|p| p.0),
            presenter_reason: self.presenter.as_ref().map(|p| p.1.clone()).unwrap_or_else(|| if self.cfg.enabled { String::new() } else { "the HUD isn't enabled for this game".into() }),
            image: self.image_view.as_ref().map(|i| i.status(now)),
            layer: self.layer_info(now),
            renderer_attached: !self.attached.is_empty(),
            model_rev: self.model_rev,
            last_error: self.last_error.clone(),
            captures: self.cfg.captures,
        }
    }
}

struct State {
    sessions: BTreeMap<String, Session>,
    conns: HashMap<u64, Conn>,
    binder: keybinds::Binder,
    /// The session whose game window has focus.
    focused: Option<String>,
}

pub struct PerfHub {
    pub hud: hudcfg::HudSettings,
    shm_root: PathBuf,
    data_dir: PathBuf,
    state: Mutex<State>,
    pub broadcast: broadcast::Sender<PerfEvent>,
    next_sub: AtomicU64,
}

/// What [`PerfHub::begin`] set up for a launch.
pub struct Began {
    pub plan: ubi_api::perf::LaunchPerfPlan,
}

impl PerfHub {
    /// A hub with its threads. `data_dir` is Quiver's (`hud/` settings and layouts go under it).
    pub fn start(data_dir: PathBuf, shm_root: PathBuf) -> Arc<PerfHub> {
        let hub = Self::new(data_dir, shm_root);
        match ubi_perf::session::sweep_stale(&hub.shm_root, |_| false) {
            Ok(v) if !v.is_empty() => tracing::info!("perf: removed {} stale session dir(s)", v.len()),
            Ok(_) => {}
            Err(e) => tracing::warn!("perf: sweeping stale session dirs: {e}"),
        }
        sigbus::install();
        spawn_rings(Arc::downgrade(&hub));
        spawn_sensors(Arc::downgrade(&hub));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(publish_loop(Arc::downgrade(&hub)));
        }
        hub
    }

    /// Without threads (tests drive [`Self::publish`] themselves).
    pub fn new(data_dir: PathBuf, shm_root: PathBuf) -> Arc<PerfHub> {
        Arc::new(PerfHub {
            hud: hudcfg::HudSettings::new(data_dir.join("hud")),
            shm_root,
            data_dir,
            state: Mutex::new(State { sessions: BTreeMap::new(), conns: HashMap::new(), binder: keybinds::Binder::dry_run(), focused: None }),
            broadcast: broadcast::channel(64).0,
            next_sub: AtomicU64::new(1),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn emit(&self, e: PerfEvent) {
        let _ = self.broadcast.send(e);
    }

    /// Starts a session's perf state for a launch (or a dev session). With the HUD enabled
    /// (or `dev`), creates the session dir and `hud.image` and returns the launch's
    /// environment additions; otherwise only the registry and sensors run.
    #[allow(clippy::too_many_arguments)]
    pub fn begin(
        &self,
        session_id: &str,
        product_id: u32,
        profile: Option<&str>,
        prefix: Option<&Path>,
        env: &BTreeMap<String, String>,
        wrappers: &[String],
        dev: bool,
    ) -> Result<Began> {
        let id = SessionId::parse(session_id).with_context(|| format!("bad session id {session_id:?}"))?;
        let mut cfg = self.hud.for_launch(product_id, profile);
        // A dev session plays a launch with the HUD on (its env is what a fake layer needs).
        cfg.enabled |= dev;
        let mut notes = Vec::new();
        let (dir, image) = if cfg.enabled {
            let dir = SessionDir::create(&self.shm_root, &id).context("creating the perf session dir")?;
            let image = match dir.create_hud_image(hud_buffer_bytes()) {
                Ok(p) => Some(p),
                Err(e) => {
                    notes.push(format!("hud.image not created: {e}"));
                    None
                }
            };
            (Some(dir), image)
        } else {
            (None, None)
        };
        let session_env: Vec<(&str, String)> = dir.as_ref().map(|d| d.env().to_vec()).unwrap_or_default();
        let mut plan = launch::plan(&cfg, &session_env, env, wrappers, launch::installed_layer_dir(&self.data_dir).as_deref());
        plan.notes.extend(notes);
        let now = mono_raw_ns();
        let mut store = Store::new(now, Utc::now());
        store.producer(catalog::PRODUCER_SYSTEM, "System", true);
        for d in catalog::system() {
            store.declare(catalog::PRODUCER_SYSTEM, d);
        }
        store.producer(catalog::PRODUCER_DISPLAY, "Display", true);
        for d in catalog::display() {
            store.declare(catalog::PRODUCER_DISPLAY, d);
        }
        if dir.is_some() {
            // Declared up front: the layer's ring shows up when the game makes a swapchain.
            store.producer(catalog::PRODUCER_LAYER, "Sight layer", false).info.ring = "none".into();
            store.declare(catalog::PRODUCER_LAYER, ubi_perf::descriptor::layer_present_descriptor());
        }
        let s = Session {
            id: session_id.to_owned(),
            product_id,
            profile: profile.map(str::to_owned),
            dev,
            dir,
            image,
            store,
            on: cfg.on,
            layout: cfg.layout.clone(),
            cfg,
            presenter: None,
            layers: Vec::new(),
            image_view: None,
            model: None,
            model_rev: 0,
            next_model_ns: 0,
            dirty: true,
            attached: BTreeSet::new(),
            pids: Vec::new(),
            prefix: prefix.map(Path::to_path_buf),
            last_error: None,
            mod_conns: HashMap::new(),
            anti_cheat: false,
            monitor: None,
            noted: None,
        };
        {
            let mut st = self.lock();
            if st.sessions.contains_key(session_id) {
                bail!("perf session {session_id} already exists");
            }
            st.sessions.insert(session_id.to_owned(), s);
        }
        tracing::info!(session = session_id, product_id, hud = plan.hud.as_ref().is_some_and(|h| h.enabled), "perf session started");
        self.emit(PerfEvent::Session { session: session_id.to_owned(), product_id, ended: false, summary: None });
        Ok(Began { plan })
    }

    /// Ends a session: its summary (when there was anything to summarize), the session dir
    /// removed, attached renderers told (an `on: false` model).
    pub fn end(&self, session_id: &str) -> Option<PerfSummary> {
        let mut s = self.lock().sessions.remove(session_id)?;
        let now = mono_raw_ns();
        s.store.note_presenter(None, now);
        let summary = s.store.summary(now);
        let has_data = summary.frames > 0 || summary.sensor_seconds > 0.0;
        if let Some(mut m) = s.model.take() {
            m.on = false;
            m.rev = s.model_rev + 1;
            self.send_to(&s.attached, PerfEvent::HudModel(m));
        }
        if let Some(dir) = s.dir.take()
            && let Err(e) = dir.remove()
        {
            tracing::warn!(session = session_id, "removing the perf session dir: {e}");
        }
        tracing::info!(session = session_id, frames = summary.frames, "perf session ended");
        let summary = has_data.then_some(summary);
        self.emit(PerfEvent::Session { session: session_id.to_owned(), product_id: s.product_id, ended: true, summary: summary.clone().map(Box::new) });
        summary
    }

    pub fn set_pids(&self, session_id: &str, pids: Vec<u32>) {
        if let Some(s) = self.lock().sessions.get_mut(session_id) {
            s.pids = pids;
        }
    }

    fn send_to(&self, conns: &BTreeSet<u64>, e: PerfEvent) {
        let st = self.lock();
        for c in conns {
            if let Some(c) = st.conns.get(c) {
                let _ = c.tx.send(Reply::Perf(e.clone()));
            }
        }
    }

    fn pick<'a>(st: &'a State, session: Option<&str>, product_id: Option<u32>) -> Result<&'a str> {
        if let Some(id) = session {
            return st.sessions.get_key_value(id).map(|(k, _)| k.as_str()).with_context(|| format!("no perf session {id}"));
        }
        if let Some(p) = product_id {
            return st.sessions.values().rev().find(|s| s.product_id == p).map(|s| s.id.as_str()).with_context(|| format!("product {p} isn't running"));
        }
        if let Some(f) = &st.focused
            && st.sessions.contains_key(f)
        {
            return Ok(f.as_str());
        }
        match st.sessions.len() {
            0 => bail!("no game is running"),
            1 => Ok(st.sessions.keys().next().unwrap().as_str()),
            _ => bail!("several games are running; name one"),
        }
    }

    pub fn product_running(&self, product_id: u32) -> bool {
        self.lock().sessions.values().any(|s| s.product_id == product_id)
    }

    pub fn sessions(&self) -> Vec<PerfSessionInfo> {
        self.lock().sessions.values().map(Session::info).collect()
    }

    pub fn catalog(&self, session: Option<&str>, product_id: Option<u32>) -> Result<Vec<MetricInfo>> {
        let now = mono_raw_ns();
        let st = self.lock();
        match Self::pick(&st, session, product_id) {
            Ok(id) => Ok(st.sessions[id].store.catalog(now)),
            Err(e) if session.is_none() && product_id.is_some() => {
                drop(st);
                tracing::debug!("static catalog ({e})");
                Ok(static_catalog())
            }
            Err(e) => Err(e),
        }
    }

    /// Registers a connection for per-connection events.
    pub fn connect(&self, c: &ConnCtx) {
        self.lock().conns.entry(c.id).or_insert_with(|| Conn { tx: c.tx.clone(), subs: Vec::new() });
    }

    /// The connection closed: its subscriptions and attachments go.
    pub fn disconnect(&self, conn: u64) {
        let mut st = self.lock();
        st.conns.remove(&conn);
        for s in st.sessions.values_mut() {
            s.attached.remove(&conn);
        }
    }

    pub fn subscribe(&self, c: &ConnCtx, session: Option<&str>, product_id: Option<u32>, select: Vec<String>, every_ms: u32, window_ms: u32, stats: Vec<Stat>) -> Result<u64> {
        self.connect(c);
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        let mut st = self.lock();
        let session = Self::pick(&st, session, product_id)?.to_owned();
        let conn = st.conns.get_mut(&c.id).context("connection gone")?;
        if conn.subs.len() >= MAX_SUBS_PER_CONN {
            bail!("at most {MAX_SUBS_PER_CONN} perf subscriptions per connection");
        }
        conn.subs.push(Sub {
            id,
            session,
            select,
            every_ns: every_ms.max(100) as i64 * 1_000_000,
            window_ns: if window_ms == 0 { registry::WINDOW_SESSION } else { window_ms.clamp(250, 10_000) as i64 * 1_000_000 },
            stats,
            next_ns: 0,
        });
        Ok(id)
    }

    pub fn unsubscribe(&self, conn: u64, sub: u64) -> Result<()> {
        let mut st = self.lock();
        let c = st.conns.get_mut(&conn).context("no such subscription")?;
        let n = c.subs.len();
        c.subs.retain(|s| s.id != sub);
        if c.subs.len() == n {
            bail!("no such subscription");
        }
        Ok(())
    }

    /// `quiver-sight`: this connection gets the session's HUD models.
    pub fn attach(&self, c: &ConnCtx, session: &str) -> Result<HudModel> {
        self.connect(c);
        {
            let mut st = self.lock();
            let s = st.sessions.get_mut(session).with_context(|| format!("no perf session {session}"))?;
            if !s.cfg.enabled && !s.dev {
                bail!("the HUD isn't enabled for this game");
            }
            s.attached.insert(c.id);
            s.dirty = true;
        }
        self.publish(mono_raw_ns());
        let st = self.lock();
        st.sessions.get(session).and_then(|s| s.model.clone()).context("no HUD model yet")
    }

    /// Shows or hides a running game's HUD; remembered as the game's last state.
    pub fn toggle(&self, session: Option<&str>, product_id: Option<u32>) -> Result<HudStatus> {
        self.change(session, product_id, |s| {
            s.on = !s.on;
            (Some(s.on), None)
        })
    }

    /// The next preset; shows the HUD if it was hidden.
    pub fn next(&self, session: Option<&str>, product_id: Option<u32>) -> Result<HudStatus> {
        self.change(session, product_id, |s| {
            let was_on = s.on;
            s.on = true;
            if was_on {
                s.layout = layout::next_preset(&s.layout).to_owned();
            }
            (Some(true), Some(s.layout.clone()))
        })
    }

    fn change(&self, session: Option<&str>, product_id: Option<u32>, f: impl FnOnce(&mut Session) -> (Option<bool>, Option<String>)) -> Result<HudStatus> {
        let (product, remember, status, sid) = {
            let mut st = self.lock();
            let id = Self::pick(&st, session, product_id)?.to_owned();
            let s = st.sessions.get_mut(&id).unwrap();
            if !s.cfg.enabled && !s.dev {
                bail!("the HUD isn't enabled for this game (quiver hud enable {})", s.product_id);
            }
            let remember = f(s);
            s.dirty = true;
            (s.product_id, remember, s.status(mono_raw_ns()), id)
        };
        if let Err(e) = self.hud.remember(product, remember.0, remember.1.as_deref()) {
            tracing::warn!("remembering the HUD state: {e:#}");
        }
        self.publish(mono_raw_ns());
        self.emit(PerfEvent::HudChanged { product_id: Some(product), session: Some(sid) });
        Ok(status)
    }

    pub fn status(&self, session: Option<&str>, product_id: Option<u32>) -> Result<HudStatus> {
        let st = self.lock();
        let id = Self::pick(&st, session, product_id)?;
        Ok(st.sessions[id].status(mono_raw_ns()))
    }

    pub fn keybinds(&self) -> ubi_api::perf::HudKeybinds {
        self.lock().binder.describe()
    }

    /// One publish pass: presenters, HUD models to attached renderers, subscription updates.
    pub fn publish(&self, now: i64) {
        let mut st = self.lock();
        let mut out: Vec<(u64, PerfEvent)> = Vec::new();
        let hud_dir = self.hud.dir().to_owned();
        for s in st.sessions.values_mut() {
            if !(s.cfg.enabled || s.dev) {
                continue;
            }
            let age = now - s.store.started_ns;
            let (p, why) = hud::choose_presenter(s.cfg.presenter, s.anti_cheat, s.live_layer(), s.image_view.as_ref(), age, now);
            if s.presenter.as_ref().is_none_or(|(op, ow)| *op != p || *ow != why) {
                if s.presenter.as_ref().is_some_and(|(op, _)| *op != p) {
                    tracing::info!(session = %s.id, ?p, "HUD presenter: {why}");
                }
                s.presenter = Some((p, why));
                s.dirty = true;
            }
            // Presenter time while shown, for the summary.
            let shown = s.on.then_some(p);
            if shown != s.noted {
                s.store.note_presenter(shown, now);
                s.noted = shown;
            }
            if !s.dirty && now < s.next_model_ns {
                continue;
            }
            s.dirty = false;
            s.next_model_ns = now + 1_000_000_000 / s.cfg.rate_hz.max(1) as i64;
            let layout = match layout::load(&hud_dir, &s.layout) {
                Ok(l) => l,
                Err(e) => {
                    s.last_error = Some(format!("layout {}: {e}; using compact", s.layout));
                    layout::builtin("compact").unwrap()
                }
            };
            let (p, why) = s.presenter.clone().unwrap();
            let image = s.image.clone();
            let inp = hud::ModelInput {
                session: &s.id,
                product_id: s.product_id,
                cfg: &s.cfg,
                on: s.on,
                layout: &layout,
                presenter: p,
                reason: &why,
                image: image.as_deref().filter(|_| p == Presenter::Game),
                monitor: s.monitor.as_deref(),
            };
            let mut m = hud::build_model(&inp, &s.store, now);
            if s.model.as_ref().is_some_and(|old| hud::same(old, &m)) {
                continue;
            }
            s.model_rev += 1;
            m.rev = s.model_rev;
            for c in &s.attached {
                out.push((*c, PerfEvent::HudModel(m.clone())));
            }
            s.model = Some(m);
        }
        // Subscriptions.
        let sessions = &st.sessions;
        let mut updates = Vec::new();
        for (cid, c) in &st.conns {
            for sub in &c.subs {
                if now < sub.next_ns {
                    continue;
                }
                let Some(s) = sessions.get(&sub.session) else { continue };
                updates.push((*cid, sub.id, update(s, sub, now)));
            }
        }
        for (cid, sid, u) in updates {
            if let Some(c) = st.conns.get_mut(&cid)
                && let Some(sub) = c.subs.iter_mut().find(|x| x.id == sid)
            {
                sub.next_ns = now + sub.every_ns;
                out.push((cid, PerfEvent::Update(u)));
            }
        }
        let mut dead = Vec::new();
        for (cid, e) in out {
            if let Some(c) = st.conns.get(&cid)
                && c.tx.send(Reply::Perf(e)).is_err()
            {
                dead.push(cid);
            }
        }
        for d in dead {
            st.conns.remove(&d);
        }
    }

    /// Mutate a session (producers use this).
    pub fn with_session<T>(&self, session: Option<&str>, product_id: u32, f: impl FnOnce(&mut Session) -> T) -> Option<T> {
        let mut st = self.lock();
        let id = match session.filter(|s| st.sessions.contains_key(*s)) {
            Some(s) => s.to_owned(),
            None => st.sessions.values().rev().find(|s| s.product_id == product_id)?.id.clone(),
        };
        st.sessions.get_mut(&id).map(f)
    }

    pub fn catalog_changed(&self, session: &str, product_id: u32) {
        self.emit(PerfEvent::CatalogChanged { session: session.to_owned(), product_id });
    }

    /// Room for more metrics in a session (≤ 512, §3.16).
    pub fn session_has_room(s: &Session) -> bool {
        s.store.ids().len() < MAX_METRICS_PER_SESSION
    }
}

/// The headline metrics a subscription without `select` gets.
const HEADLINE: [&str; 12] = ["fps", "fps.base", "frametime", "fps.p1low", "latency.render", "gpu.load", "gpu.temp", "gpu.power", "cpu.load", "cpu.temp", "upscaler", "fg.ratio"];

fn update(s: &Session, sub: &Sub, now: i64) -> PerfUpdate {
    let mut ids: Vec<String> = Vec::new();
    let sel: Vec<String> = if sub.select.is_empty() { HEADLINE.iter().map(|x| x.to_string()).collect() } else { sub.select.clone() };
    for x in &sel {
        for id in s.store.select(x) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let mut values = BTreeMap::new();
    for id in ids {
        let Some(desc) = s.store.descriptor(&id) else { continue };
        let stats = if sub.stats.is_empty() { vec![format::default_stat(desc)] } else { sub.stats.clone() };
        let mut per = BTreeMap::new();
        for stat in stats {
            if let Some(v) = s.store.formatted(&id, stat, sub.window_ns, now) {
                per.insert(stat, v);
            }
        }
        values.insert(id, per);
    }
    PerfUpdate { sub: sub.id, session: s.id.clone(), product_id: s.product_id, at: Utc::now(), values }
}

/// Built-in metrics, for a game that isn't running.
pub fn static_catalog() -> Vec<MetricInfo> {
    let mut s = Store::new(0, Utc::now());
    s.producer(catalog::PRODUCER_LAYER, "Sight layer", false);
    s.declare(catalog::PRODUCER_LAYER, ubi_perf::descriptor::layer_present_descriptor());
    s.producer(catalog::PRODUCER_SYSTEM, "System", false);
    for d in catalog::system() {
        s.declare(catalog::PRODUCER_SYSTEM, d);
    }
    s.producer(catalog::PRODUCER_DISPLAY, "Display", false);
    for d in catalog::display() {
        s.declare(catalog::PRODUCER_DISPLAY, d);
    }
    s.catalog(0).into_iter().map(|mut m| {
        m.state = ubi_api::perf::MetricState::Declared;
        m.last = None;
        m
    }).collect()
}

async fn publish_loop(hub: Weak<PerfHub>) {
    let mut tick = tokio::time::interval(PUBLISH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let Some(h) = hub.upgrade() else { return };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.publish(mono_raw_ns()))).is_err() {
            tracing::error!("perf: publishing panicked; continuing");
        }
    }
}

/// The ring thread: drains every session dir's rings every 50 ms.
fn spawn_rings(hub: Weak<PerfHub>) {
    let _ = std::thread::Builder::new().name("quiverd-perf-rings".into()).spawn(move || {
        let mut watched: HashMap<String, rings::SessionRings> = HashMap::new();
        loop {
            std::thread::sleep(rings::DRAIN_EVERY);
            let Some(h) = hub.upgrade() else { return };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain_once(&h, &mut watched)));
            if r.is_err() {
                tracing::error!("perf: ring drain panicked; dropping the readers and continuing");
                watched.clear();
            }
        }
    });
}

/// One drain pass (also driven directly by tests).
pub fn drain_once(h: &PerfHub, watched: &mut HashMap<String, rings::SessionRings>) {
    let dirs: Vec<(String, SessionDir)> = h.lock().sessions.values().filter_map(|s| s.dir.clone().map(|d| (s.id.clone(), d))).collect();
    watched.retain(|id, _| dirs.iter().any(|(d, _)| d == id));
    for (id, dir) in dirs {
        let w = watched.entry(id.clone()).or_insert_with(|| rings::SessionRings::new(dir));
        let drained = w.drain();
        let now = mono_raw_ns();
        let image = w.image(now);
        let mut st = h.lock();
        let Some(s) = st.sessions.get_mut(&id) else { continue };
        let mut changed = false;
        for (sc, t) in &drained.presents {
            s.store.present(*sc, *t, now);
        }
        let alive = drained.layers.iter().any(|l| l.alive && !l.ring.starts_with("invalid"));
        if let Some(l) = drained.layers.iter().rev().find(|l| l.alive && l.status.is_some()) {
            s.store.set_layer_status(l.status.unwrap());
        }
        let p = s.store.producer(catalog::PRODUCER_LAYER, "Sight layer", alive);
        let (records, lost, rejected) = drained.layers.iter().fold((0, 0, 0), |a, l| (a.0 + l.records, a.1 + l.lost, a.2 + l.rejected));
        let ring = drained.layers.last().map(|l| l.ring.clone()).unwrap_or_else(|| "none".into());
        let detail = drained.layers.last().map(|l| format!("pid {}{}", l.pid, if l.alive { "" } else { " (gone)" })).unwrap_or_default();
        if p.info.ring != ring || p.info.detail != detail {
            changed = true;
        }
        p.info.records = records;
        p.info.lost = lost;
        p.info.rejected = rejected;
        p.info.ring = ring;
        p.info.detail = detail;
        s.layers = drained.layers;
        s.image_view = image;
        let product = s.product_id;
        drop(st);
        if changed {
            h.catalog_changed(&id, product);
        }
    }
}

/// The sensors thread: system and display producers at 1 Hz while a session runs.
fn spawn_sensors(hub: Weak<PerfHub>) {
    let _ = std::thread::Builder::new().name("quiverd-perf-sensors".into()).spawn(move || {
        let mut sys = system::SystemProducer::new(system::Roots::default());
        let sock = display::socket();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(h) = hub.upgrade() else { return };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sense_once(&h, &mut sys, sock.as_deref())));
            if r.is_err() {
                tracing::error!("perf: sensors panicked; continuing");
            }
        }
    });
}

pub fn sense_once(h: &PerfHub, sys: &mut system::SystemProducer, sock: Option<&Path>) {
    let (want, markers): (system::Want, Vec<(String, Vec<String>, Vec<u32>)>) = {
        let st = h.lock();
        if st.sessions.is_empty() {
            drop(st);
            sys.idle();
            return;
        }
        let mut want = system::Want::default();
        let mut markers = Vec::new();
        for s in st.sessions.values() {
            let mut pids: Vec<u32> = s.pids.clone();
            pids.extend(s.layers.iter().filter(|l| l.alive).map(|l| l.pid));
            want.pids.extend(pids.iter().copied());
            if want.vendor.is_none() {
                want.vendor = s.store.layer_status.map(|l| l.vendor_id).filter(|v| *v != 0);
            }
            let mut m = vec![format!("QUIVER_SESSION={}", s.id)];
            if let Some(p) = &s.prefix {
                m.push(format!("WINEPREFIX={}", p.display()));
            }
            markers.push((s.id.clone(), m, pids));
        }
        want.pids.sort_unstable();
        want.pids.dedup();
        (want, markers)
    };
    let reading = sys.sample(&want);
    let disp = sock.and_then(display::DisplayState::poll);
    let proc_root = Path::new("/proc");
    let focused = disp.as_ref().and_then(|d| d.active.as_ref()).and_then(|a| {
        markers.iter().find(|(_, m, pids)| pids.contains(&(a.pid as u32)) || display::pid_in_session(proc_root, a.pid, m)).map(|(id, _, _)| id.clone())
    });
    let now = mono_raw_ns();
    let mut st = h.lock();
    st.binder.focus(focused.is_some());
    st.focused = focused.clone();
    for s in st.sessions.values_mut() {
        for (id, v) in &reading.nums {
            s.store.push_num(id, now, *v);
        }
        for (id, v) in &reading.texts {
            s.store.push_text(id, now, v.clone());
        }
        for (id, v) in &reading.states {
            s.store.push_state_value(id, now, v);
        }
        if let Some(d) = &disp
            && let Some(m) = d.game_monitor(focused.as_deref() == Some(s.id.as_str()))
        {
            let v = display::values(m);
            s.monitor = Some(m.name.clone());
            s.store.push_text("display.mode", now, v.mode);
            s.store.push_num("display.refresh", now, v.refresh);
            s.store.push_state_value("display.vrr", now, v.vrr);
            s.store.push_state_value("display.tearing", now, v.tearing);
            s.store.push_text("display.path", now, v.path);
        }
    }
}

#[cfg(test)]
mod tests;
