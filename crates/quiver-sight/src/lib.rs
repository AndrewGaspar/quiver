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
//! - shows the toast lane: achievements unlocked and mod-setting outcomes of Quiver launches;
//! - exits when quiverd goes away or a few seconds after the last session ends.
//!
//! Threads: GPUI's main thread (all rendering, windows, the heartbeat) and one tokio thread
//! for the quiverd connection ([`link`]), joined by a channel.
//!
//! ```text
//! quiver-sight                      # what quiverd runs
//! quiver-sight --demo toasts        # sample toasts on this desktop, then exit
//! quiver-sight --demo desktop [--seconds N]      # a sample desktop HUD
//! quiver-sight --demo image --image PATH [--extent WxH] [--layout standard]
//!                                   # render a sample model into a hud.image file
//! ```

pub mod demo;
pub mod desktop;
pub mod game;
pub mod hud_view;
pub mod link;
pub mod place;
pub mod toasts;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::App;
use quiver_api::perf::HudModel;
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
    layer_shell: bool,
    ticks: u32,
}

impl Sight {
    fn new(layer_shell: bool) -> Self {
        Self { huds: HashMap::new(), toasts: toasts::ToastLane::new(layer_shell), layer_shell, ticks: 0 }
    }

    fn handle(&mut self, msg: Msg, cx: &mut App) {
        match msg {
            Msg::Model(m) => self.model(*m, cx),
            Msg::Ended(s) => {
                if let Some(mut h) = self.huds.remove(&s) {
                    let (rss, hwm) = game::rss_mib();
                    tracing::info!(session = %s, rss_mib = rss, peak_mib = hwm, "HUD closed: {}", h.game.stats.line());
                    h.game.close(cx);
                    h.desktop.close(cx);
                }
                self.place_toasts();
            }
            Msg::Toast(t) => {
                self.place_toasts();
                self.toasts.push(t, cx);
            }
            Msg::Quit(why) => {
                tracing::info!("quitting: {why}");
                for (_, mut h) in self.huds.drain() {
                    h.game.close(cx);
                    h.desktop.close(cx);
                }
                self.toasts.close(cx);
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
        }
    }

    fn model(&mut self, m: HudModel, cx: &mut App) {
        let h = self.huds.entry(m.session.clone()).or_insert_with(|| SessionHud { model: None, game: Default::default(), desktop: Default::default() });
        if h.model.as_ref().is_some_and(|old| old.rev >= m.rev) {
            return;
        }
        if let Err(e) = h.game.update(&m, cx) {
            tracing::warn!(session = %m.session, "HUD image: {e:#}");
        }
        if let Err(e) = h.desktop.update(&m, self.layer_shell, cx) {
            tracing::warn!(session = %m.session, "desktop HUD: {e:#}");
        }
        if h.model.as_ref().is_none_or(|old| old.presenter != m.presenter) {
            tracing::info!(session = %m.session, presenter = ?m.presenter, reason = %m.presenter_reason, "HUD presenter");
        }
        h.model = Some(m);
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
        if self.ticks.is_multiple_of(HEARTBEAT_EVERY) {
            let now = quiver_perf::clock::mono_raw_ns();
            for h in self.huds.values() {
                h.game.heartbeat(now);
            }
        }
        for h in self.huds.values_mut() {
            h.game.tick(cx);
        }
        self.toasts.tick(cx);
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
    seconds: f32,
    image: Option<PathBuf>,
    extent: (u32, u32),
    layout: String,
    headless: bool,
}

fn args() -> anyhow::Result<Args> {
    let mut a = Args { demo: None, seconds: 6.0, image: None, extent: (2560, 1600), layout: "standard".into(), headless: false };
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
            "--headless" => a.headless = true,
            "-V" | "--version" => {
                println!("quiver-sight {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("quiver-sight: Sight's HUD renderer and toast lane. quiverd starts it; it takes no arguments.\n  --demo toasts|desktop|image  try it without quiverd (see the source)");
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
    let _lock = if args.demo.is_none() {
        match single_instance()? {
            Some(l) => Some(l),
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
    match args.demo.clone() {
        None => {
            std::thread::Builder::new().name("quiver-sight-link".into()).spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
                rt.block_on(link::run(tx));
            })?;
        }
        Some(d) => demo::feed(&d, &args, tx)?,
    }
    tracing::info!(pid = std::process::id(), layer_shell, headless, "quiver-sight starting");
    // Windows come and go (toasts, the desktop HUD, off-screen HUDs): quit only when told.
    app.with_quit_mode(gpui::QuitMode::Explicit).with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        let dir = theme::omarchy_state_dir();
        quiver_ui::init(cx, theme::load(&dir));
        let sight = Rc::new(RefCell::new(Sight::new(layer_shell)));
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
