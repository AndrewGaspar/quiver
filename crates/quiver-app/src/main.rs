//! Quiver's desktop app: a GPUI front end for the `quiverd` daemon (see `quiver-api`). It
//! shows the daemon's state and sends it requests; installs, launches and settings all live
//! in the daemon, so the CLI and agents see and drive the same thing.
//!
//! Arguments: `--page library|downloads|activity` opens on that page, or turns the Quiver
//! that is already open to it and exits (`handoff.rs`).
//!
//! Environment:
//! - `QUIVER_DEMO=1`: built-in fixture data instead of the daemon (`demo.rs`); never
//!   connects to or starts `quiverd`.
//! - `QUIVER_DEMO_INPUT="pad down a"`: demo mode only, replays inputs once the library is
//!   on screen (for screenshots without a device); `scroll` scrolls the page down 400 px.
//! - `QUIVER_DEMO_GLOBAL="settings=accounts,playing"`: demo mode only, opens a global screen
//!   or sets up a state for screenshots (see `ui/global.rs` and `demo_global.rs`).
//! - `QUIVER_DEMO_LIBRARY=FILE`: demo mode only, the library is the entries in FILE (the
//!   shape `quiver library --json` prints) instead of the built-in fixtures, for
//!   screenshots of a real library's grid.
//! - `QUIVER_THEME=builtin`: ignore Omarchy and use "Quiver dark".
//! - `QUIVER_OMARCHY_STATE=DIR`: read the theme from DIR instead of
//!   `~/.local/state/omarchy/current` (tests, screenshots).
//! - `QUIVER_AUTO_RELAUNCH=0`: don't relaunch when the binary is replaced by a new build
//!   (`selfupdate.rs`). `QUIVER_RESUME` is set by a relaunch for the new process.
//! - `XDG_CACHE_HOME`: art cache root (`quiver/art`); `XDG_CONFIG_HOME`: where
//!   `quiver/custom-art.json` (per-game image files) is read from.

mod app;
mod art;
mod demo;
mod gamepad;
mod handoff;
mod machines;
mod model;
mod nav;
mod selfupdate;
mod source;
mod stereo_ui;
mod streaming;
mod ui;

// The design system lives in `quiver-ui`; these keep the `crate::theme::…` paths the screens use.
use quiver_ui::{assets, text_input, theme};

use std::future::Future;
use std::sync::OnceLock;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

// ---------------------------------------------------------------------------------------------
// quiver-api's client is tokio-based; GPUI has its own executor. Calls run on a tokio runtime and
// hand results back through oneshot channels, which GPUI tasks can await.

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime"))
}

pub fn on_tokio<F>(f: F) -> futures::channel::oneshot::Receiver<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    rt().spawn(async move {
        let _ = tx.send(f.await);
    });
    rx
}

pub fn err_text(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

fn main() {
    // SAFETY: first thing in main; no other thread exists yet.
    unsafe { selfupdate::forget_hyprland_tokens() };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,quiver_app=info".into()),
        )
        .init();
    if let Some(dir) = std::env::var_os("QUIVER_STEREO_SHOT") {
        stereo_ui::shot::run(dir.into());
        return;
    }
    let page = handoff::requested_page(std::env::args().skip(1));
    let resume = selfupdate::take_resume();
    let resumed = resume.is_some();
    let demo = source::demo_requested();
    // `--page`: an open Quiver turns to it, and this launch is done.
    if let Some(p) = page.filter(|_| !resumed && !demo) {
        if handoff::forward(p) {
            return;
        }
    }
    if demo {
        tracing::info!("demo mode: fixture data, no daemon");
    }
    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        assets::register_fonts(cx);
        cx.set_global(theme::load(&theme::omarchy_state_dir()));
        text_input::bind_keys(cx);
        app::bind_keys(cx);
        let bounds = WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx));
        let bounds = selfupdate::window_bounds(resume.as_ref(), bounds);
        let title = if demo { "Quiver (demo)" } else { "Quiver" };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
                app_id: Some("quiver".into()),
                window_min_size: Some(size(px(360.), px(400.))),
                ..Default::default()
            },
            move |window, cx| {
                // gpui 0.2's Wayland backend ignores `titlebar.title` at creation (only the
                // app id reaches the compositor), so set it once the window exists.
                window.set_window_title(title);
                cx.new(|cx| {
                    let mut q = app::Quiver::new(demo, window, cx);
                    if let Some(p) = page.filter(|_| !resumed) {
                        q.nav.open_section(p);
                    }
                    q.start_self_update(resume, window, cx);
                    if !demo {
                        cx.spawn_in(window, async move |this, cx| handoff::listen(resumed, this, cx).await).detach();
                    }
                    q
                })
            },
        )
        .expect("open window");
        cx.activate(true);
    });
}
