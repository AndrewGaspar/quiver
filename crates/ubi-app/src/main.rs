//! Quiver's desktop app: a GPUI front end for the `quiverd` daemon (see `ubi-api`). It
//! shows the daemon's state and sends it requests; installs, launches and settings all live
//! in the daemon, so the CLI and agents see and drive the same thing.
//!
//! `QUIVER_DEMO=1` runs on built-in fixture data instead of the daemon (see `demo.rs`).

mod app;
mod art;
mod assets;
mod demo;
mod gamepad;
mod model;
mod nav;
mod palette;
mod source;
mod text_input;
mod theme;
mod ui;

use std::future::Future;
use std::sync::OnceLock;

use gpui::{App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

// ---------------------------------------------------------------------------------------------
// ubi-api's client is tokio-based; GPUI has its own executor. Calls run on a tokio runtime and
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,ubi_app=info".into()),
        )
        .init();
    let demo = source::demo_requested();
    if demo {
        tracing::info!("demo mode: fixture data, no daemon");
    }
    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        assets::register_fonts(cx);
        cx.set_global(theme::load(&theme::omarchy_state_dir()));
        text_input::bind_keys(cx);
        app::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let title = if demo { "Quiver (demo)" } else { "Quiver" };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
                app_id: Some("quiver".into()),
                window_min_size: Some(size(px(960.), px(600.))),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| app::Quiver::new(demo, window, cx)),
        )
        .expect("open window");
        cx.activate(true);
    });
}
