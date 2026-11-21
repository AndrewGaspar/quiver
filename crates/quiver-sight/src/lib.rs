//! `quiver-sight`: Sight's UI process (design/SIGHT.md §5, design/SIGHT-HUD.md §2.9).
//!
//! quiverd starts it for Quiver launches (and HUD toggles); one instance serves every running
//! session. It
//!
//! - attaches to each session whose HUD is enabled (`HudAttach`) and follows its `HudModel`;
//! - renders the HUD off-screen with GPUI (`quiver-ui`'s theme and fonts) at the scale the
//!   game's swapchain implies and publishes it into the session's `hud.image`, which the
//!   Sight layer composites in the game (the game presenter), re-rendering only when the
//!   model changes and heartbeating twice a second;
//! - or, when the model's presenter is `desktop`, shows the same view on a click-through
//!   layer-shell surface on the game's monitor;
//! - shows the toast lane: achievements unlocked, mod-setting outcomes, notices (saves in the
//!   cloud, a mod's hotkey, agents' and mods' notes) and finished downloads, drawn in the game
//!   where its layer can ([`overlay`], with the status chip and the first-launch hint), else
//!   on the desktop; do-not-disturb holds all but errors (design/SIGHT.md §16);
//! - opens on the Quick tab for replay, screenshots and pins ([`quick`]);
//! - shows the Sight panel over the game when summoned ([`panel`]: Shift+F2, a hyprpad
//!   chord, `quiver sight toggle`), driven with the controller;
//! - ties the panel and the desktop HUD to the game's window ([`front`], from the
//!   compositor's toplevel list, [`toplevels`], and the game's Sight layer): hidden while
//!   the game isn't in front, back when it is (toasts stay up wherever the user is);
//! - exits when quiverd goes away or a few seconds after the last session ends.
//!
//! Threads: GPUI's main thread (all rendering, windows, the heartbeat) and one tokio thread
//! for the quiverd connection ([`link`]), joined by a channel.
//!
//! ```text
//! quiver-sight                      # what quiverd runs
//! quiver-sight --demo toasts [--variant more]   # sample toasts on this desktop, then exit
//! quiver-sight --demo desktop [--seconds N]      # a sample desktop HUD
//! quiver-sight --demo image --image PATH [--extent WxH] [--layout standard] [--variant long]
//!                                   # render a sample model into a hud.image file
//! quiver-sight --demo panel [--section mods] [--seconds N] [--namespace quiver-sight-test]
//!                                   # the panel with sample data on this desktop
//! ```

pub mod demo;
pub mod desktop;
pub mod front;
pub mod game;
mod glue;
pub mod hud_view;
pub mod hypr;
pub mod lab;
pub mod link;
pub mod overlay;
pub mod panel;
pub mod place;
pub mod quick;
pub mod replay;
pub mod screenshot;
pub mod toasts;
pub mod toplevels;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::App;
use quiver_api::perf::{HudModel, Presenter};
use quiver_api::sight::{PanelAction, PanelCommand, SightSettings};
use quiver_ui::theme;

use link::Msg;

/// The heartbeat (≥ 1/s, §3.8) and toast expiry.
const TICK: Duration = Duration::from_millis(250);
const HEARTBEAT_EVERY: u32 = 2;

struct SessionHud {
    model: Option<HudModel>,
    game: game::GameHud,
    desktop: desktop::DesktopHud,
}

pub struct Sight {
    huds: HashMap<String, SessionHud>,
    toasts: toasts::ToastLane,
    pub panel: panel::PanelCtl,
    layer_shell: bool,
    ticks: u32,
    /// Is each game in front: the panel and the desktop HUD are tied to it (SIGHT §14).
    front: front::Front,
    /// When Hyprland was last asked for a session's window (at most once a second).
    hinted: HashMap<String, Instant>,
    /// To our own loop (re-checks after a timer).
    tx: Option<mpsc::UnboundedSender<Msg>>,
    recheck: Option<Instant>,
    // ---- design/SIGHT.md §16 (`glue`)
    /// To quiverd (`None`: the demo).
    link: Option<tokio::sync::mpsc::UnboundedSender<link::LinkReq>>,
    settings: SightSettings,
    replay_enabled: bool,
    /// The toasts, the chip and the hint drawn in the game.
    overlay: overlay::Overlay,
    /// Sessions whose game didn't show the overlay lately (toasts on the desktop meanwhile).
    overlay_failed: HashMap<String, Instant>,
    recorders: HashMap<String, replay::Recorder>,
    quick_data: HashMap<String, panel::state::PanelData>,
    /// The last FPS and the start of each session (the chip).
    fps: HashMap<String, f64>,
    started: HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// When each session was first seen (the hint waits a little for the game to draw).
    started_at: HashMap<String, Instant>,
    hint_done: bool,
}

/// How often Hyprland may be asked for a session's window while it isn't known.
const HINT_EVERY: Duration = Duration::from_secs(1);

impl Sight {
    fn new(layer_shell: bool, panel: panel::PanelCtl, tx: Option<mpsc::UnboundedSender<Msg>>, link: Option<tokio::sync::mpsc::UnboundedSender<link::LinkReq>>, settings: SightSettings, replay_enabled: bool) -> Self {
        Self {
            huds: HashMap::new(),
            toasts: toasts::ToastLane::new(layer_shell),
            panel,
            layer_shell,
            ticks: 0,
            front: front::Front::new(),
            hinted: HashMap::new(),
            tx,
            recheck: None,
            link,
            settings,
            replay_enabled,
            overlay: Default::default(),
            overlay_failed: HashMap::new(),
            recorders: HashMap::new(),
            quick_data: HashMap::new(),
            fps: HashMap::new(),
            started: HashMap::new(),
            started_at: HashMap::new(),
            hint_done: false,
        }
    }

    /// A panel command (after the quick menu had its say): ties the game's toplevel, opens hidden
    /// if the game isn't in front.
    fn panel_command(&mut self, c: PanelCommand, cx: &mut App) {
        let opening = matches!(c.action, PanelAction::Open | PanelAction::Quick) || (c.action == PanelAction::Toggle && !self.panel.is_open());
        let mut hidden = None;
        if opening
            && let Some(s) = c.session.clone()
            && self.layer_shell
        {
            self.hint(&s);
            // Shift+F2 came from the game's own window, so the active toplevel is the
            // game's (the demo stands in for a game the same way).
            if (c.by == "keys" || c.by == "demo") && self.front.bind_active(&s, Instant::now()) {
                tracing::info!(session = %s, by = %c.by, "the game's toplevel: the one active when it was summoned");
            }
            // Summoned while the game isn't in front (`quiver sight open` from a
            // terminal): open hidden, show when the game is.
            if let v @ front::Verdict::Away(_) = self.front.verdict(&s, false) {
                hidden = Some(v.why().to_owned());
            }
        }
        // The panel takes the game's image over from the overlay.
        if opening {
            self.overlay.close(cx);
        }
        let was_open = self.panel.is_open();
        let session = c.session.clone().or_else(|| self.panel.session().map(str::to_owned));
        self.panel.command(c, hidden, cx);
        self.refresh_quick(cx);
        self.apply_front(cx);
        if was_open && !self.panel.is_open() {
            self.chip(session.as_deref(), false, cx);
        }
        self.sync_overlay(cx);
    }

    /// The sessions something tied to the game is shown (or hidden) for: the panel's, and
    /// those whose HUD the desktop presents.
    fn tied_sessions(&self) -> Vec<String> {
        let mut v: Vec<String> = self.huds.iter().filter(|(_, h)| h.model.as_ref().is_some_and(|m| m.on && m.presenter == Presenter::Desktop)).map(|(s, _)| s.clone()).collect();
        if let Some(s) = self.panel.session()
            && !v.iter().any(|x| x == s)
        {
            v.push(s.to_owned());
        }
        v
    }

    /// Finds the game's toplevel through Hyprland when it isn't known (rate-limited).
    fn hint(&mut self, session: &str) {
        if !self.front.wants_hint(session) || self.hinted.get(session).is_some_and(|t| t.elapsed() < HINT_EVERY) {
            return;
        }
        self.hinted.insert(session.to_owned(), Instant::now());
        if let Some(w) = self.panel.hypr_game_window(session)
            && self.front.bind_hint(session, &w.class, &w.title)
        {
            tracing::info!(session, class = %w.class, title = %w.title, "the game's toplevel: Hyprland's window for the session");
        }
    }

    /// Applies "is the game in front" to the panel and the desktop HUDs.
    fn apply_front(&mut self, cx: &mut App) {
        if !self.layer_shell {
            return;
        }
        // Learned whatever is shown, so a panel summoned later from elsewhere knows the game.
        for s in self.front.learn(Instant::now()) {
            if let Some((t, _)) = self.front.bound(&s) {
                tracing::info!(session = %s, app_id = %t.app_id, title = %t.title, "the game's toplevel: learned from its Sight layer");
            }
        }
        let sessions = self.tied_sessions();
        if sessions.is_empty() {
            return;
        }
        for s in &sessions {
            self.hint(s);
        }
        let describe = |f: &front::Front, s: &str| f.bound(s).map(|(t, how)| format!("{} “{}” ({})", if t.app_id.is_empty() { "(no app id)" } else { &t.app_id }, t.title, how.as_str()));
        let mut recheck = None;
        if let Some(s) = self.panel.session().map(str::to_owned) {
            self.front.observe_panel(&s, self.panel.keyboard_since(), Instant::now());
            let v = self.front.verdict(&s, self.panel.has_keyboard());
            recheck = self.panel.front(&v, describe(&self.front, &s), cx);
        }
        for (s, h) in &mut self.huds {
            if !h.model.as_ref().is_some_and(|m| m.on && m.presenter == Presenter::Desktop) {
                continue;
            }
            let away = self.front.verdict(s, false).away();
            if let Err(e) = h.desktop.set_away(away, h.model.as_ref(), self.layer_shell, cx) {
                tracing::warn!(session = %s, "desktop HUD: {e:#}");
            }
        }
        // A pending hide or the end of the panel's keyboard grace: ask again then.
        if let (Some(after), Some(tx)) = (recheck, self.tx.clone()) {
            let at = Instant::now() + after;
            if self.recheck.is_none_or(|r| r <= Instant::now() || at < r) {
                self.recheck = Some(at);
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(after).await;
                    let _ = tx.unbounded_send(Msg::Front);
                })
                .detach();
            }
        }
    }

    fn handle(&mut self, msg: Msg, cx: &mut App) {
        match msg {
            Msg::Model(m) => self.model(*m, cx),
            Msg::Ended(s) => {
                self.panel.ended(&s, cx);
                self.panel.layer_backdrop(&s, None, cx);
                self.panel.set_stereo(&s, None, cx);
                self.front.ended(&s);
                self.hinted.remove(&s);
                self.recorders.remove(&s);
                self.quick_session_ended(&s, cx);
                if let Some(mut h) = self.huds.remove(&s) {
                    let (rss, hwm) = game::rss_mib();
                    tracing::info!(session = %s, rss_mib = rss, peak_mib = hwm, "HUD closed: {}", h.game.stats.line());
                    h.game.close(cx);
                    h.desktop.close(cx);
                }
                self.place_toasts();
            }
            Msg::Toast(t) => self.toast(t, cx),
            Msg::Notice(n) => self.notice(*n, cx),
            Msg::NoticeAnswered(id) => {
                self.toasts.dismiss_notice(&id, cx);
                self.refresh_quick(cx);
                self.sync_overlay(cx);
            }
            Msg::Settings(s) => self.settings_changed(*s, cx),
            Msg::PinFailed { scope, reason } => {
                self.panel_pins(cx);
                self.panel.pin_failed(&scope, reason, cx);
            }
            Msg::Replay { session, action } => self.apply_replay(&session, action, cx),
            Msg::ReplayFeedback { session, result } => self.replay_feedback(&session, result, cx),
            Msg::QuickAction { session, tile } => self.quick_apply(&session, &tile, cx),
            Msg::Quit(why) => {
                tracing::info!("quitting: {why}");
                for (_, mut h) in self.huds.drain() {
                    h.game.close(cx);
                    h.desktop.close(cx);
                }
                self.recorders.clear();
                self.toasts.close(cx);
                self.overlay.close(cx);
                self.panel.close(cx);
                // Leave once the windows are gone. Not through GPUI's teardown: dropping the
                // app at exit can touch thread-locals that are already destroyed and abort
                // ("cannot access a Thread Local Storage value during or after destruction").
                // Nothing needs flushing: hud.image is a shared mapping, logs go to stderr.
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_millis(150)).await;
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    // SAFETY: ends the process; no Rust state is used afterwards.
                    unsafe { libc::_exit(0) }
                })
                .detach();
            }
            Msg::Panel(c) if c.action == PanelAction::Chip => self.chip(c.session.as_deref(), true, cx),
            Msg::Panel(c) => self.panel_command(c, cx),
            Msg::Loaded { session, data } => {
                self.data_seen(&session, &data, cx);
                self.panel.loaded(&session, *data, cx)
            }
            Msg::Done { change, result } => self.panel.done(change, result, cx),
            Msg::Metrics(u) => {
                self.metrics_seen(&u);
                self.panel.metrics(u, cx)
            }
            Msg::OptionsChanged { session } => self.panel.options_changed(&session),
            Msg::Pad(e) => self.panel.pad(e, cx),
            Msg::Toplevels(e) => {
                if let toplevels::Event::Unavailable(why) = &e {
                    tracing::info!("no toplevel list: {why}; the panel and the desktop HUD follow the game's Sight layer where it reports focus");
                }
                self.front.toplevels_event(e, Instant::now());
                self.apply_front(cx);
            }
            Msg::PanelKeyboard { map_gen, active } => {
                self.panel.keyboard(map_gen, active);
                self.apply_front(cx);
            }
            Msg::Front => {
                self.recheck = None;
                self.apply_front(cx);
            }
            Msg::GameInput { session, events } => self.panel.game_input(&session, &events, cx),
            Msg::Perf { session, reply } => self.panel.perf_reply(&session, reply, cx),
            Msg::PanelLayout { section, docked, prefs } => self.panel.layout(section, docked, prefs, cx),
        }
    }

    fn model(&mut self, m: HudModel, cx: &mut App) {
        if self.huds.get(&m.session).and_then(|h| h.model.as_ref()).is_some_and(|old| old.rev >= m.rev) {
            return;
        }
        // Whether the game is in front before the desktop HUD shows, so it doesn't flash up
        // over another window when its first model comes.
        self.front.game_focus(&m.session, m.game_focus, Instant::now());
        let desktop = m.on && m.presenter == Presenter::Desktop && self.layer_shell;
        if desktop {
            self.hint(&m.session);
        }
        let away = desktop && self.front.verdict(&m.session, false).away();
        let h = self.huds.entry(m.session.clone()).or_insert_with(|| SessionHud { model: None, game: Default::default(), desktop: Default::default() });
        if let Err(e) = h.game.update(&m, cx) {
            tracing::warn!(session = %m.session, "HUD image: {e:#}");
        }
        let _ = h.desktop.set_away(away, None, self.layer_shell, cx);
        if let Err(e) = h.desktop.update(&m, self.layer_shell, cx) {
            tracing::warn!(session = %m.session, "desktop HUD: {e:#}");
        }
        if h.model.as_ref().is_none_or(|old| old.presenter != m.presenter) {
            tracing::info!(session = %m.session, presenter = ?m.presenter, reason = %m.presenter_reason, "HUD presenter");
        }
        let (session, backdrop, stereo) = (m.session.clone(), m.backdrop.as_deref().cloned(), m.stereo.as_ref().map(|s| s.stereo()));
        h.model = Some(m);
        self.panel.layer_backdrop(&session, backdrop, cx);
        self.panel.set_stereo(&session, stereo, cx);
        self.apply_front(cx);
        self.place_toasts();
    }

    /// Toasts go to the game's monitor, away from a HUD in the top-right corner.
    fn place_toasts(&mut self) {
        let shown: Vec<&HudModel> = self.huds.values().filter_map(|h| h.model.as_ref()).filter(|m| m.on).collect();
        self.toasts.corner = place::toast_corner(shown.iter().map(|m| &m.anchor));
        self.toasts.monitor = shown.iter().find_map(|m| m.monitor.clone()).or_else(|| self.huds.values().find_map(|h| h.model.as_ref()?.monitor.clone()));
    }

    fn tick(&mut self, cx: &mut App) {
        self.ticks = self.ticks.wrapping_add(1);
        // The panel drawn in the game holds the game's input only while this keeps renewing it
        // (a quiver-sight that hangs gives it back within a second).
        self.panel.renew_in_game();
        if self.ticks.is_multiple_of(HEARTBEAT_EVERY) {
            let now = quiver_perf::clock::mono_raw_ns();
            for h in self.huds.values() {
                h.game.heartbeat(now);
            }
            // The open panel's request to the Sight layer to blur the game.
            self.panel.keepalive(cx);
        }
        for h in self.huds.values_mut() {
            h.game.tick(cx);
        }
        // Toast expiry, the overlay's heartbeat and the first-launch hint.
        self.tick_overlay(cx);
        // The panel's clock and play time.
        if self.ticks.is_multiple_of(4) {
            self.poll_replays(cx);
            self.panel.tick(cx);
        }
        // Learning which toplevel is the game's needs time to pass with nothing happening.
        if self.ticks.is_multiple_of(2) {
            self.apply_front(cx);
        }
    }
}

/// One `quiver-sight` per user session: a lock next to quiverd's socket.
fn single_instance() -> anyhow::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let dir = quiver_api::socket_path().parent().map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir)?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("sight.lock"))?;
    // SAFETY: flock on a file descriptor we own.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok((r == 0).then_some(f))
}

pub struct Args {
    demo: Option<String>,
    section: Option<String>,
    namespace: Option<String>,
    seconds: f32,
    image: Option<PathBuf>,
    extent: (u32, u32),
    layout: String,
    variant: String,
    headless: bool,
    /// `--lab DIR`: a `quiver lab` instance's run (`lab`).
    lab: Option<lab::LabArgs>,
}

fn args() -> anyhow::Result<Args> {
    let mut a = Args { demo: None, section: None, namespace: None, seconds: 6.0, image: None, extent: (2560, 1600), layout: "standard".into(), variant: "base".into(), headless: false, lab: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or_else(|| anyhow::anyhow!("{arg} needs a value"));
        match arg.as_str() {
            "--demo" => a.demo = Some(val()?),
            "--seconds" => a.seconds = val()?.parse()?,
            "--image" => a.image = Some(PathBuf::from(val()?)),
            "--extent" => {
                let v = val()?;
                let (w, h) = v.split_once('x').ok_or_else(|| anyhow::anyhow!("--extent WxH"))?;
                a.extent = (w.parse()?, h.parse()?);
            }
            "--layout" => a.layout = val()?,
            "--variant" => a.variant = val()?,
            "--headless" => a.headless = true,
            "--lab" => a.lab.get_or_insert_with(Default::default).dir = PathBuf::from(val()?),
            "--lab-status" => a.lab.get_or_insert_with(Default::default).status = Some(PathBuf::from(val()?)),
            "--title" => a.lab.get_or_insert_with(Default::default).title = val()?,
            "--game" => a.lab.get_or_insert_with(Default::default).game = val()?,
            "--section" => a.section = Some(val()?),
            "--namespace" => a.namespace = Some(val()?),
            "-V" | "--version" => {
                println!("quiver-sight {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("quiver-sight: Sight's HUD renderer and toast lane. quiverd starts it; it takes no arguments.\n  --demo toasts|desktop|image|panel  try it without quiverd (see the source)");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other} (try --help)"),
        }
    }
    Ok(a)
}

/// The `quiver-sight` binary.
pub fn run() -> anyhow::Result<()> {
    // Its own variable: quiverd's RUST_LOG (inherited) is about quiverd.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("QUIVER_SIGHT_LOG").unwrap_or_else(|_| "quiver_sight=info,warn".into()))
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    let args = args()?;
    let _lock = if args.demo.is_none() && args.lab.is_none() {
        match single_instance()? {
            Some(l) => {
                // A panel state file left by a quiver-sight that died.
                panel::write_state(None);
                Some(l)
            }
            None => {
                tracing::info!("another quiver-sight is running; exiting");
                return Ok(());
            }
        }
    } else {
        None
    };
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let headless = args.headless || args.demo.as_deref() == Some("image") || (!wayland && std::env::var_os("DISPLAY").is_none());
    // Layer-shell needs Wayland; off-screen rendering works anywhere.
    let layer_shell = !headless && wayland;
    let app = if headless { gpui_platform::headless() } else { gpui_platform::application() };
    let (tx, rx) = mpsc::unbounded::<Msg>();
    let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel::<link::LinkReq>();
    let mut panel = panel::PanelCtl::new(layer_shell, None, Some(tx.clone()));
    // The demo has no quiverd; a lab instance's link refuses settings writes.
    let sight_link = (args.demo.is_none()).then(|| req_tx.clone());
    let settings = if args.lab.is_some() || args.demo.is_some() { SightSettings::default() } else { quiver_api::sight::load_settings_at(&quiver_api::sight::settings_path()) };
    if let Some(ns) = &args.namespace {
        panel.namespace = ns.clone();
    }
    match (args.demo.clone(), args.lab.clone()) {
        (_, Some(l)) => {
            // A lab instance's run: no quiverd (design/LAB.md §12).
            panel = panel::PanelCtl::new(layer_shell, Some(req_tx), Some(tx.clone()));
            lab::start(l, tx.clone(), req_rx)?;
        }
        (None, None) => {
            panel = panel::PanelCtl::new(layer_shell, Some(req_tx), Some(tx.clone()));
            // The real one keeps the sections' layout in sight.toml.
            panel.persist = true;
            let tx = tx.clone();
            std::thread::Builder::new().name("quiver-sight-link".into()).spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
                rt.block_on(link::run(tx, req_rx));
            })?;
        }
        (Some(d), None) => demo::feed(&d, &args, tx.clone())?,
    }
    // The controller's long-press fallback has to listen from the start.
    if panel.pad_chord() && args.demo.is_none() && args.lab.is_none() {
        panel.start_pad();
    }
    // The compositor's toplevels: what ties the panel and the desktop HUD to the game.
    if layer_shell {
        let t = tx.clone();
        toplevels::spawn(move |e| {
            let _ = t.unbounded_send(Msg::Toplevels(e));
        });
    }
    let ui_tx = tx.clone();
    tracing::info!(pid = std::process::id(), layer_shell, headless, "quiver-sight starting");
    // Windows come and go (toasts, the desktop HUD, off-screen HUDs): quit only when told.
    app.with_quit_mode(gpui::QuitMode::Explicit).with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        let dir = theme::omarchy_state_dir();
        quiver_ui::init(cx, theme::load(&dir));
        let sight = Rc::new(RefCell::new(Sight::new(layer_shell, panel, Some(ui_tx), sight_link, settings, args.demo.is_none() && args.lab.is_none())));
        // Theme switches: reload, and re-render what's shown (by replaying the models).
        let (ttx, mut trx) = mpsc::unbounded::<()>();
        theme::watch(dir.clone(), move || {
            let _ = ttx.unbounded_send(());
        });
        let s = sight.clone();
        cx.spawn(async move |cx| {
            while trx.next().await.is_some() {
                let dir = dir.clone();
                let s = s.clone();
                cx.update(move |cx| {
                    cx.set_global(theme::load(&dir));
                    let models: Vec<HudModel> = s.borrow_mut().huds.values_mut().filter_map(|h| h.model.take()).collect();
                    for m in models {
                        s.borrow_mut().model(m, cx);
                    }
                });
            }
        })
        .detach();
        let s = sight.clone();
        let mut rx = rx;
        cx.spawn(async move |cx| {
            while let Some(msg) = rx.next().await {
                let s = s.clone();
                cx.update(move |cx| s.borrow_mut().handle(msg, cx));
            }
        })
        .detach();
        let s = sight.clone();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let s = s.clone();
                cx.update(move |cx| s.borrow_mut().tick(cx));
            }
        })
        .detach();
    });
    Ok(())
}
