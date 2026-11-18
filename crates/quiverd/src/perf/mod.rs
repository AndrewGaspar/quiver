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
pub mod nvidia;
pub mod nvml;
pub mod panel;
pub mod registry;
pub mod rings;
pub mod sight;
pub mod sigbus;
pub mod stats;
pub mod steam;
pub mod system;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use quiver_api::{GameRef, Reply};
use quiver_api::perf::{
    HudConfig, HudModel, HudStatus, LayerInfo, MetricInfo, PerfEvent, PerfSessionInfo, PerfSummary, PerfUpdate, Presenter, Stat,
};
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::session::{SessionDir, SessionId};

use hud::ImageView;
use registry::Store;
use rings::LayerRing;

/// `/dev/shm`, or `QUIVER_SHM_ROOT` (tests, a scratch daemon).
pub fn shm_root() -> PathBuf {
    std::env::var_os("QUIVER_SHM_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| quiver_perf::session::SHM_ROOT.into())
}

/// The HUD image's buffer size: room for Full at 2× on a 1600p swapchain (≈ 1000 × 1000
/// px), within the 4 MiB cap; tmpfs allocates only the pages quiver-sight writes.
pub fn hud_buffer_bytes() -> u32 {
    quiver_perf::hud::buffer_bytes_for(1024, 1000).unwrap_or(quiver_perf::layout::HUD_MAX_BUFFER_BYTES)
}

const PUBLISH_EVERY: Duration = Duration::from_millis(100);
const MAX_SUBS_PER_CONN: usize = 8;
const MAX_METRICS_PER_SESSION: usize = quiver_perf::descriptor::limits::MAX_METRICS_PER_SESSION;

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
    /// The game: a Ubisoft product, or a Steam game started through `quiver steam-run`.
    pub game: GameRef,
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
    /// The layer's Reflex counters at the last drain (`nvidia::apply_layer`).
    nv_last: nvidia::Last,
}

impl Session {
    /// The Ubisoft product (0 for another store's game), as the wire's `product_id`.
    pub fn product_id(&self) -> u32 {
        self.game.as_ubisoft().unwrap_or(0)
    }

    /// `game` for the wire when it isn't a Ubisoft product (Ubisoft JSON stays as it was).
    fn other_store(&self) -> Option<GameRef> {
        (!self.game.is_ubisoft()).then_some(self.game)
    }

    /// The layer ring that's alive (the newest), for the presenter and status.
    pub fn live_layer(&self) -> Option<&LayerRing> {
        self.layers.iter().rev().find(|l| l.alive && !l.ring.starts_with("invalid")).or_else(|| self.layers.last())
    }

    /// The layer whose key presses count for the session: of the live ones, the one drawing
    /// on the largest swapchain (the game rather than a launcher), newest on ties.
    pub fn key_layer(&self) -> Option<&LayerRing> {
        let live = self.layers.iter().filter(|l| l.alive && !l.ring.starts_with("invalid") && l.status.is_some());
        live.max_by_key(|l| l.status.map_or(0, |s| s.extent_w as u64 * s.extent_h as u64))
    }

    pub fn layer_info(&self, now: i64) -> Option<LayerInfo> {
        let l = self.live_layer()?;
        let s = l.status.unwrap_or(quiver_perf::layout::LayerStatus::ZERO);
        // The keys from the layer that reads them for the session.
        let key_status = self.key_layer().and_then(|k| k.status).or(l.status);
        let (keys, keys_detail) = key_status.as_ref().map_or(("", ""), keybinds::layer_keys);
        Some(LayerInfo {
            pid: l.pid,
            alive: l.alive,
            heartbeat_age_ms: (l.heartbeat > 0).then(|| ((now - l.heartbeat).max(0) / 1_000_000) as u64),
            presents: s.presents,
            drawn: s.drawn,
            drawing: s.flags & quiver_perf::layout::LAYER_DRAWING != 0,
            draw_error: hud::draw_error_name(s.draw_error, s.flags).into(),
            extent: (s.extent_w > 0).then_some([s.extent_w, s.extent_h]),
            vk_format: s.vk_format,
            color_space: s.color_space,
            present_mode: s.present_mode,
            hud_published: s.hud_published,
            engine: quiver_perf::layout::LayerStatus::get_str(&s.engine_name).map(|x| quiver_perf::descriptor::sanitize_text(x, 32)).unwrap_or_default(),
            device: quiver_perf::layout::LayerStatus::get_str(&s.device_name).map(|x| quiver_perf::descriptor::sanitize_text(x, 64)).unwrap_or_default(),
            vendor_id: s.vendor_id,
            keys: keys.into(),
            keys_detail: keys_detail.into(),
            key_toggles: key_status.map_or(0, |k| k.key_toggles as u32),
            key_nexts: key_status.map_or(0, |k| k.key_nexts as u32),
            backdrop: key_status.as_ref().map(hud::backdrop_status).unwrap_or_default(),
        })
    }

    fn info(&self) -> PerfSessionInfo {
        PerfSessionInfo {
            session: self.id.clone(),
            product_id: self.product_id(),
            game: self.other_store(),
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
            product_id: self.product_id(),
            game: self.other_store(),
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
    /// `quiver-sight`, started for launches and HUD toggles ([`sight`]).
    pub sight: sight::Spawner,
    /// Perf sessions of `quiver steam-run` launches (`steam.rs`).
    steam: steam::SteamSessions,
    /// The Sight panel's last command and report ([`panel`]).
    panel: Mutex<panel::PanelState>,
}

/// What [`PerfHub::begin`] set up for a launch.
pub struct Began {
    pub plan: quiver_api::perf::LaunchPerfPlan,
}

impl PerfHub {
    /// A hub with its threads. `data_dir` is Quiver's (`hud/` settings and layouts go under it).
    pub fn start(data_dir: PathBuf, shm_root: PathBuf) -> Arc<PerfHub> {
        let hub = Self::new(data_dir, shm_root);
        match quiver_perf::session::sweep_stale(&hub.shm_root, |_| false) {
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
            sight: sight::Spawner::default(),
            steam: Default::default(),
            panel: Mutex::default(),
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
        game: GameRef,
        profile: Option<&str>,
        prefix: Option<&Path>,
        env: &BTreeMap<String, String>,
        wrappers: &[String],
        dev: bool,
    ) -> Result<Began> {
        let id = SessionId::parse(session_id).with_context(|| format!("bad session id {session_id:?}"))?;
        let mut cfg = self.hud.for_launch(&game, profile);
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
            if cfg.ngx_log
                && let Err(e) = std::fs::create_dir(dir.path().join(nvidia::NGX_DIR))
            {
                notes.push(format!("NGX log dir not created: {e}"));
            }
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
            store.declare(catalog::PRODUCER_LAYER, quiver_perf::descriptor::layer_present_descriptor());
        }
        // What the launch asks of NVIDIA's stack: a DLSS override (requested, not measured)
        // and Smooth Motion (frames generated above the layer).
        nvidia::apply_launch(&mut store, &nvidia::LaunchNv::from_env(env), now);
        let s = Session {
            id: session_id.to_owned(),
            game,
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
            nv_last: nvidia::Last::default(),
        };
        {
            let mut st = self.lock();
            if st.sessions.contains_key(session_id) {
                bail!("perf session {session_id} already exists");
            }
            st.sessions.insert(session_id.to_owned(), s);
        }
        tracing::info!(session = session_id, %game, hud = plan.hud.as_ref().is_some_and(|h| h.enabled), "perf session started");
        let other = (!game.is_ubisoft()).then_some(game);
        self.emit(PerfEvent::Session { session: session_id.to_owned(), product_id: game.as_ubisoft().unwrap_or(0), ended: false, summary: None, game: other });
        Ok(Began { plan })
    }

    /// Ends a session: its summary (when there was anything to summarize), the session dir
    /// removed, attached renderers told (an `on: false` model).
    pub fn end(&self, session_id: &str) -> Option<PerfSummary> {
        let mut s = self.lock().sessions.remove(session_id)?;
        self.panel_session_ended(session_id);
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
        self.emit(PerfEvent::Session { session: session_id.to_owned(), product_id: s.product_id(), ended: true, summary: summary.clone().map(Box::new), game: s.other_store() });
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

    fn pick<'a>(st: &'a State, session: Option<&str>, game: Option<GameRef>) -> Result<&'a str> {
        if let Some(id) = session {
            return st.sessions.get_key_value(id).map(|(k, _)| k.as_str()).with_context(|| format!("no perf session {id}"));
        }
        if let Some(g) = game {
            let what = g.as_ubisoft().map(|p| format!("product {p}")).unwrap_or_else(|| g.to_string());
            return st.sessions.values().rev().find(|s| s.game == g).map(|s| s.id.as_str()).with_context(|| format!("{what} isn't running"));
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
        self.running(&GameRef::ubisoft(product_id))
    }

    /// The game has a perf session.
    pub fn running(&self, game: &GameRef) -> bool {
        self.lock().sessions.values().any(|s| s.game == *game)
    }

    pub fn sessions(&self) -> Vec<PerfSessionInfo> {
        self.lock().sessions.values().map(Session::info).collect()
    }

    pub fn catalog(&self, session: Option<&str>, product_id: Option<GameRef>) -> Result<Vec<MetricInfo>> {
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

    /// Live consumers: (perf subscriptions, HUD attachments). Either keeps quiverd from
    /// restarting onto a new build.
    pub fn consumers(&self) -> (usize, usize) {
        let st = self.lock();
        (st.conns.values().map(|c| c.subs.len()).sum(), st.sessions.values().map(|s| s.attached.len()).sum())
    }

    /// The connection closed: its subscriptions and attachments go.
    pub fn disconnect(&self, conn: u64) {
        let mut st = self.lock();
        st.conns.remove(&conn);
        for s in st.sessions.values_mut() {
            s.attached.remove(&conn);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn subscribe(&self, c: &ConnCtx, session: Option<&str>, product_id: Option<GameRef>, select: Vec<String>, every_ms: u32, window_ms: u32, stats: Vec<Stat>) -> Result<u64> {
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

    /// Shows or hides a running game's HUD; remembered as the game's last state. Starts
    /// `quiver-sight` when the HUD comes on and it isn't running (§2.9).
    pub fn toggle(&self, session: Option<&str>, product_id: Option<GameRef>) -> Result<HudStatus> {
        let st = self.change(session, product_id, |s| {
            s.on = !s.on;
            (Some(s.on), None)
        })?;
        if st.on {
            self.sight.ensure(&self.data_dir, "HUD toggled on");
        }
        Ok(st)
    }

    /// The next preset; shows the HUD if it was hidden.
    pub fn next(&self, session: Option<&str>, product_id: Option<GameRef>) -> Result<HudStatus> {
        let st = self.change(session, product_id, |s| {
            let was_on = s.on;
            s.on = true;
            if was_on {
                s.layout = layout::next_preset(&s.layout).to_owned();
            }
            (Some(true), Some(s.layout.clone()))
        })?;
        self.sight.ensure(&self.data_dir, "HUD preset changed");
        Ok(st)
    }

    /// HUD settings changed at `scope`: running sessions it covers take the new look now
    /// (position, size, background, rate, …). `enabled` and `captures` decide the launch's
    /// layer setup, so they wait for the next launch; shown/hidden and the preset change live
    /// only when this change set them (`on`, `layout`), else they keep the session's state.
    pub fn settings_changed(&self, scope: &quiver_api::perf::HudScopeRef, set: &quiver_api::perf::HudScope) {
        let changed: Vec<(GameRef, String)> = {
            let mut st = self.lock();
            let mut out = Vec::new();
            for s in st.sessions.values_mut() {
                if scope.game.is_some_and(|g| g != s.game) {
                    continue;
                }
                // A profile's settings only reach sessions playing that profile.
                if scope.profile.is_some() && scope.profile != s.profile {
                    continue;
                }
                let fresh = self.hud.for_launch(&s.game, s.profile.as_deref());
                let c = &mut s.cfg;
                c.presenter = fresh.presenter;
                c.anchor = fresh.anchor;
                c.offset = fresh.offset;
                c.scale = fresh.scale;
                c.background = fresh.background;
                c.text_opacity = fresh.text_opacity;
                c.level_colors = fresh.level_colors;
                c.graph_span_ms = fresh.graph_span_ms;
                c.safe_area = fresh.safe_area;
                c.rate_hz = fresh.rate_hz;
                if let Some(on) = set.on {
                    s.on = on;
                }
                if let Some(l) = &set.layout {
                    s.layout = l.clone();
                }
                s.next_model_ns = 0;
                s.dirty = true;
                out.push((s.game, s.id.clone()));
            }
            out
        };
        if changed.is_empty() {
            return;
        }
        self.publish(mono_raw_ns());
        for (game, sid) in changed {
            self.emit(PerfEvent::HudChanged { product_id: game.as_ubisoft(), session: Some(sid) });
        }
    }

    fn change(&self, session: Option<&str>, product_id: Option<GameRef>, f: impl FnOnce(&mut Session) -> (Option<bool>, Option<String>)) -> Result<HudStatus> {
        let (product, remember, status, sid) = {
            let mut st = self.lock();
            let id = Self::pick(&st, session, product_id)?.to_owned();
            let s = st.sessions.get_mut(&id).unwrap();
            if !s.cfg.enabled && !s.dev {
                bail!("the HUD isn't enabled for this game (quiver hud enable {})", s.game.key());
            }
            let remember = f(s);
            s.dirty = true;
            (s.game, remember, s.status(mono_raw_ns()), id)
        };
        if let Err(e) = self.hud.remember(&product, remember.0, remember.1.as_deref()) {
            tracing::warn!("remembering the HUD state: {e:#}");
        }
        self.publish(mono_raw_ns());
        self.emit(PerfEvent::HudChanged { product_id: product.as_ubisoft(), session: Some(sid) });
        Ok(status)
    }

    pub fn status(&self, session: Option<&str>, product_id: Option<GameRef>) -> Result<HudStatus> {
        let st = self.lock();
        let id = Self::pick(&st, session, product_id)?;
        Ok(st.sessions[id].status(mono_raw_ns()))
    }

    /// The HUD's keys and, per running game with a layer, whether the layer reads them.
    pub fn keybinds(&self) -> quiver_api::perf::HudKeybinds {
        let st = self.lock();
        let games = st
            .sessions
            .values()
            .filter_map(|s| {
                let l = s.key_layer().or_else(|| s.live_layer())?;
                let status = l.status.unwrap_or(quiver_perf::layout::LayerStatus::ZERO);
                Some(keybinds::keys_state(&s.id, s.product_id(), s.other_store(), l.pid, &status))
            })
            .collect();
        st.binder.describe(games)
    }

    /// HUD chords the session's layer read in the game (`drain_once`): the same as
    /// `quiver hud toggle` / `quiver hud next` for that session, remembered as the game's
    /// last state. A burst is capped (the counters are a press count, not a queue); within
    /// one 50 ms drain, preset steps go before toggles.
    pub fn hud_keys(&self, session: &str, toggles: u32, nexts: u32) {
        const MAX_PER_DRAIN: u32 = 4;
        for _ in 0..nexts.min(MAX_PER_DRAIN) {
            match self.next(Some(session), None) {
                Ok(s) => tracing::info!(session, layout = %s.layout, "HUD key: next preset"),
                Err(e) => tracing::info!(session, "HUD key: next preset ignored: {e:#}"),
            }
        }
        for _ in 0..toggles.min(MAX_PER_DRAIN) {
            match self.toggle(Some(session), None) {
                Ok(s) => tracing::info!(session, on = s.on, "HUD key: toggle"),
                Err(e) => tracing::info!(session, "HUD key: toggle ignored: {e:#}"),
            }
        }
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
            let backdrop = hud::layer_backdrop(image.as_deref(), s.key_layer().or(s.live_layer()), now);
            let inp = hud::ModelInput {
                session: &s.id,
                product_id: s.product_id(),
                cfg: &s.cfg,
                on: s.on,
                layout: &layout,
                presenter: p,
                reason: &why,
                image: image.as_deref().filter(|_| p == Presenter::Game),
                monitor: s.monitor.as_deref(),
                backdrop,
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
            None => st.sessions.values().rev().find(|s| s.game == GameRef::ubisoft(product_id))?.id.clone(),
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
const HEADLINE: [&str; 14] = [
    "fps",
    "fps.base",
    "frametime",
    "fps.p1low",
    "latency.render",
    "latency.pc",
    "nv.reflex.mode",
    "gpu.load",
    "gpu.temp",
    "gpu.power",
    "cpu.load",
    "cpu.temp",
    "upscaler",
    "fg.ratio",
];

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
    PerfUpdate { sub: sub.id, session: s.id.clone(), product_id: s.product_id(), at: Utc::now(), values }
}

/// Built-in metrics, for a game that isn't running.
pub fn static_catalog() -> Vec<MetricInfo> {
    let mut s = Store::new(0, Utc::now());
    s.producer(catalog::PRODUCER_LAYER, "Sight layer", false);
    s.declare(catalog::PRODUCER_LAYER, quiver_perf::descriptor::layer_present_descriptor());
    s.producer(catalog::PRODUCER_SYSTEM, "System", false);
    for d in catalog::system() {
        s.declare(catalog::PRODUCER_SYSTEM, d);
    }
    s.producer(catalog::PRODUCER_DISPLAY, "Display", false);
    for d in catalog::display() {
        s.declare(catalog::PRODUCER_DISPLAY, d);
    }
    s.producer(nvidia::PRODUCER, "NVIDIA (Reflex, DLSS)", false);
    for d in nvidia::descriptors() {
        s.declare(nvidia::PRODUCER, d);
    }
    s.catalog(0).into_iter().map(|mut m| {
        m.state = quiver_api::perf::MetricState::Declared;
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
        if h.sight.refresh_due(h.panel_open()) {
            let h2 = h.clone();
            tokio::task::spawn_blocking(move || h2.sight.restart(h2.data_dir()));
        }
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
            let st = l.status.unwrap();
            s.store.set_layer_status(st);
            changed |= nvidia::apply_layer(&mut s.store, &st, &mut s.nv_last, now);
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
        let key_source = s.key_layer().map(|l| l.pid);
        let product = s.product_id();
        drop(st);
        if changed {
            h.catalog_changed(&id, product);
        }
        // One layer per session reads the keys for it: every layer polls the same X server
        // keymap, so a launcher and its game would both count each press.
        if let Some(pid) = key_source {
            let (t, n, p) = drained.keys.iter().filter(|k| k.0 == pid).fold((0, 0, 0), |a, k| (a.0 + k.1, a.1 + k.2, a.2 + k.3));
            h.hud_keys(&id, t, n);
            // Shift+F2: the Sight panel (SIGHT §14).
            h.panel_keys(&id, p);
        }
    }
}

/// The sensors thread: system and display producers at 1 Hz while a session runs.
fn spawn_sensors(hub: Weak<PerfHub>) {
    let _ = std::thread::Builder::new().name("quiverd-perf-sensors".into()).spawn(move || {
        let mut sys = system::SystemProducer::new(system::Roots::default());
        let sock = display::socket();
        let mut dlss: HashMap<String, nvidia::DlssScan> = HashMap::new();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(h) = hub.upgrade() else { return };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sense_once(&h, &mut sys, sock.as_deref());
                scan_dlss(&h, &mut dlss);
            }));
            if r.is_err() {
                tracing::error!("perf: sensors panicked; continuing");
            }
        }
    });
}

/// The DLSS DLLs mapped in each session's game processes and NGX's log in its session dir
/// (`nvidia::DlssScan`), outside the hub's lock.
pub fn scan_dlss(h: &PerfHub, scans: &mut HashMap<String, nvidia::DlssScan>) {
    let work: Vec<(String, Vec<u32>, Option<PathBuf>)> = {
        let st = h.lock();
        st.sessions
            .values()
            .map(|s| {
                let mut pids = s.pids.clone();
                pids.extend(s.layers.iter().filter(|l| l.alive).map(|l| l.pid));
                pids.sort_unstable();
                pids.dedup();
                (s.id.clone(), pids, s.dir.as_ref().map(|d| d.path().join(nvidia::NGX_DIR)).filter(|p| p.is_dir()))
            })
            .collect()
    };
    scans.retain(|id, _| work.iter().any(|w| &w.0 == id));
    let now = std::time::Instant::now();
    for (id, pids, ngx) in work {
        let Some(found) = scans.entry(id.clone()).or_default().scan(&pids, ngx.as_deref(), now) else { continue };
        let at = mono_raw_ns();
        let fresh = {
            let mut st = h.lock();
            let Some(s) = st.sessions.get_mut(&id) else { continue };
            (nvidia::apply_dlss(&mut s.store, &found, at), s.product_id())
        };
        if fresh.0 {
            h.catalog_changed(&id, fresh.1);
        }
    }
}

/// A session's id, its environment markers and its game pids (to recognize its windows).
type Markers = Vec<(String, Vec<String>, Vec<u32>)>;

pub fn sense_once(h: &PerfHub, sys: &mut system::SystemProducer, sock: Option<&Path>) {
    let (want, markers): (system::Want, Markers) = {
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
            s.store.push_text("display.blocked", now, v.blocked);
        }
    }
}

#[cfg(test)]
mod tests;
