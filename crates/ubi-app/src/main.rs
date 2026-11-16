//! Ubisoft Library: a GPUI front end for ubi-core. Sign in, list owned games,
//! install/update them, and play them through ubi-bridge in per-game Proton prefixes.

mod text_input;

use std::future::Future;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui::{
    App, Application, Bounds, Context, Entity, SharedString, Stateful, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use text_input::TextInput;
use ubi_core::auth::{Auth, LoginOutcome, TwoFactorChallenge};
use ubi_core::install::Progress;
use ubi_core::launcher::{self, Config, Game, Games, LaunchEvent};
use ubi_core::library::Library;

const LANGUAGE: &str = "en-US";

// ---------------------------------------------------------------------------------------------
// ubi-core is tokio-based; GPUI has its own executor. Core work runs on a tokio runtime and
// hands results back through oneshot channels, which GPUI tasks can await.

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime"))
}

fn on_tokio<F>(f: F) -> futures::channel::oneshot::Receiver<F::Output>
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

/// Like `on_tokio`, for futures that aren't `Send` (rustc can't prove it for some borrowing
/// stream combinators in ubi-core): the future is built and driven on a blocking thread.
fn on_tokio_local<F, Fut>(f: F) -> futures::channel::oneshot::Receiver<Fut::Output>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future,
    Fut::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    rt().spawn_blocking(move || {
        let _ = tx.send(tokio::runtime::Handle::current().block_on(f()));
    });
    rx
}

fn err_text(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
enum RowState {
    NotInstalled,
    Installed { update: bool },
    Installing(Arc<Progress>),
    Running,
}

struct Row {
    product_id: u32,
    name: String,
    game: Option<Game>,
    state: RowState,
    /// Latest status line from an install/launch, shared with the worker.
    status: Arc<Mutex<Option<String>>>,
}

impl Row {
    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = Some(s.into());
    }
}

struct LoginForm {
    email: Entity<TextInput>,
    password: Entity<TextInput>,
    code: Entity<TextInput>,
    challenge: Option<TwoFactorChallenge>,
    busy: bool,
    error: Option<String>,
}

enum Screen {
    Loading(String),
    Login(LoginForm),
    Library,
}

struct Launcher {
    auth: Arc<Auth>,
    cfg: Option<Arc<Config>>,
    broker: Option<String>,
    screen: Screen,
    account: Option<String>,
    rows: Vec<Row>,
    error: Option<String>,
}

impl Launcher {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            auth: Arc::new(Auth::default()),
            cfg: None,
            broker: None,
            screen: Screen::Loading("Starting…".into()),
            account: None,
            rows: Vec::new(),
            error: None,
        };
        match Config::load() {
            Ok(c) => this.cfg = Some(Arc::new(c)),
            Err(e) => this.error = Some(format!("Configuration: {}", err_text(&e))),
        }
        this.start_broker(cx);
        this.resume_session(cx);
        // Repaint while background work (downloads, launches) updates shared state.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    /// The broker serves Denuvo token requests from every running game's bridge.
    fn start_broker(&mut self, cx: &mut Context<Self>) {
        let rx = on_tokio(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?.to_string();
            tokio::spawn(ubi_core::broker::Broker::new(Auth::default()).serve(listener));
            anyhow::Ok(addr)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(addr)) => this.broker = Some(addr),
                    Ok(Err(e)) => this.error = Some(format!("Broker: {}", err_text(&e))),
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn resume_session(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Loading("Signing in…".into());
        let auth = self.auth.clone();
        let rx = on_tokio(async move { ubi_core::store::current(&auth).await });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(session)) => {
                        this.account = Some(session.name);
                        this.load_library(cx);
                    }
                    _ => this.show_login(None, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn show_login(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.screen = Screen::Login(LoginForm {
            email: cx.new(|cx| TextInput::new(cx, "Email", false)),
            password: cx.new(|cx| TextInput::new(cx, "Password", true)),
            code: cx.new(|cx| TextInput::new(cx, "Authenticator code", false)),
            challenge: None,
            busy: false,
            error,
        });
    }

    fn submit_login(&mut self, cx: &mut Context<Self>) {
        let Screen::Login(form) = &mut self.screen else { return };
        if form.busy {
            return;
        }
        form.busy = true;
        form.error = None;
        let auth = self.auth.clone();
        let rx = match form.challenge.clone() {
            Some(challenge) => {
                let code = form.code.read(cx).text().trim().to_owned();
                on_tokio(async move { auth.complete_2fa(&challenge, &code).await.map(LoginOutcome::Done) })
            }
            None => {
                let email = form.email.read(cx).text().trim().to_owned();
                let password = form.password.read(cx).text().to_owned();
                on_tokio(async move { auth.login(&email, &password).await })
            }
        };
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                let Screen::Login(form) = &mut this.screen else { return };
                form.busy = false;
                match result {
                    Ok(Ok(LoginOutcome::Done(session))) => {
                        if let Err(e) = ubi_core::store::save(&session) {
                            form.error = Some(err_text(&e));
                        } else {
                            this.account = Some(session.name);
                            this.load_library(cx);
                        }
                    }
                    Ok(Ok(LoginOutcome::TwoFactor(c))) => form.challenge = Some(c),
                    Ok(Err(e)) => form.error = Some(err_text(&e)),
                    Err(_) => form.error = Some("cancelled".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        let _ = ubi_core::store::clear();
        self.rows.clear();
        self.account = None;
        self.show_login(None, cx);
        cx.notify();
    }

    fn load_library(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Loading("Loading your library…".into());
        let auth = self.auth.clone();
        let rx = on_tokio(async move {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let lib = Library::fetch(&demux).await?;
            let installed = Games::load()?;
            let mut rows: Vec<(u32, String, Option<Game>, bool)> = lib
                .products
                .iter()
                .filter(|p| p.owned && p.is_installable() && p.product_type == 0 && !p.locked_by_subscription)
                .map(|p| {
                    let game = installed.get(p.product_id).cloned();
                    let update = game.as_ref().is_some_and(|g| launcher::update_available(g, &lib));
                    (p.product_id, p.name.clone().unwrap_or_else(|| p.product_id.to_string()), game, update)
                })
                .collect();
            rows.sort_by(|a, b| b.2.is_some().cmp(&a.2.is_some()).then_with(|| a.1.cmp(&b.1)));
            anyhow::Ok(rows)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(rows)) => {
                        this.rows = rows
                            .into_iter()
                            .map(|(product_id, name, game, update)| Row {
                                product_id,
                                name,
                                state: match &game {
                                    Some(_) => RowState::Installed { update },
                                    None => RowState::NotInstalled,
                                },
                                game,
                                status: Arc::default(),
                            })
                            .collect();
                        this.screen = Screen::Library;
                    }
                    Ok(Err(e)) => {
                        this.error = Some(err_text(&e));
                        this.screen = Screen::Library;
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn row_mut(&mut self, product_id: u32) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.product_id == product_id)
    }

    fn install(&mut self, product_id: u32, cx: &mut Context<Self>) {
        let Some(cfg) = self.cfg.clone() else { return };
        let auth = self.auth.clone();
        let Some(row) = self.row_mut(product_id) else { return };
        let progress = Arc::new(Progress::default());
        row.state = RowState::Installing(progress.clone());
        row.set_status("Starting…");
        let status = row.status.clone();
        let rx = on_tokio_local(move || async move {
            launcher::install_game(&cfg, &auth, product_id, LANGUAGE, &progress, |s| {
                *status.lock().unwrap() = Some(s);
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                if let Some(row) = this.row_mut(product_id) {
                    match result {
                        Ok(Ok(game)) => {
                            row.game = Some(game);
                            row.state = RowState::Installed { update: false };
                            row.set_status("Installed");
                        }
                        Ok(Err(e)) => {
                            row.state = match row.game {
                                Some(_) => RowState::Installed { update: true },
                                None => RowState::NotInstalled,
                            };
                            row.set_status(format!("Install failed: {}", err_text(&e)));
                        }
                        Err(_) => {}
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn play(&mut self, product_id: u32, cx: &mut Context<Self>) {
        let (Some(cfg), Some(broker)) = (self.cfg.clone(), self.broker.clone()) else { return };
        let auth = self.auth.clone();
        let Some(row) = self.row_mut(product_id) else { return };
        let Some(game) = row.game.clone() else { return };
        row.state = RowState::Running;
        row.set_status("Starting…");
        let status = row.status.clone();
        let rx = on_tokio_local(move || async move {
            launcher::play(&cfg, &auth, &game, &broker, |e| {
                let text = match e {
                    LaunchEvent::Status(s) => s,
                    LaunchEvent::Running => "Running".into(),
                    LaunchEvent::Exited(_) => "Game exited".into(),
                };
                *status.lock().unwrap() = Some(text);
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                if let Some(row) = this.row_mut(product_id) {
                    row.state = RowState::Installed { update: false };
                    if let Ok(Err(e)) = result {
                        row.set_status(format!("Launch failed: {}", err_text(&e)));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering

const BG: u32 = 0x121218;
const PANEL: u32 = 0x1a1a22;
const BORDER: u32 = 0x2c2c38;
const TEXT: u32 = 0xe8e8ee;
const MUTED: u32 = 0x8a8a9a;
const ACCENT: u32 = 0x2f6fed;
const DANGER: u32 = 0xe5484d;

fn button(id: impl Into<SharedString>, label: impl Into<SharedString>, primary: bool, enabled: bool) -> Stateful<gpui::Div> {
    let base = div()
        .id(gpui::ElementId::Name(id.into()))
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .child(label.into());
    let base = if primary { base.bg(rgb(ACCENT)).text_color(rgb(0xffffff)) } else { base.bg(rgb(BORDER)).text_color(rgb(TEXT)) };
    if enabled {
        base.cursor_pointer().hover(|s| s.opacity(0.85))
    } else {
        base.opacity(0.4)
    }
}

fn progress_bar(fraction: f32) -> gpui::Div {
    div()
        .w(px(160.))
        .h(px(6.))
        .rounded_full()
        .bg(rgb(BORDER))
        .child(div().h_full().rounded_full().bg(rgb(ACCENT)).w(px(160. * fraction.clamp(0., 1.))))
}

impl Launcher {
    fn render_login(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let Screen::Login(form) = &self.screen else { return div() };
        let two_factor = form.challenge.is_some();
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(360.))
            .p_6()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(div().text_xl().child("Sign in to Ubisoft"));
        if two_factor {
            col = col
                .child(div().text_sm().text_color(rgb(MUTED)).child("Enter the code from your authenticator app."))
                .child(form.code.clone());
        } else {
            col = col.child(form.email.clone()).child(form.password.clone());
        }
        if let Some(e) = &form.error {
            col = col.child(div().text_sm().text_color(rgb(DANGER)).child(e.clone()));
        }
        let label = match (form.busy, two_factor) {
            (true, _) => "Signing in…",
            (false, true) => "Verify",
            (false, false) => "Sign in",
        };
        col.child(
            button("login", label, true, !form.busy)
                .on_click(cx.listener(|this, _, _, cx| this.submit_login(cx))),
        )
    }

    fn render_row(&self, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let id = row.product_id;
        let status = row.status.lock().unwrap().clone();
        let (label, detail): (Option<(&str, bool)>, Option<gpui::AnyElement>) = match &row.state {
            RowState::NotInstalled => (Some(("Install", false)), None),
            RowState::Installed { update: true } => (Some(("Update", false)), None),
            RowState::Installed { update: false } => (Some(("Play", true)), None),
            RowState::Running => (None, None),
            RowState::Installing(p) => {
                let total = p.total.load(Relaxed).max(1) as f32;
                let frac = p.downloaded.load(Relaxed) as f32 / total;
                (None, Some(progress_bar(frac).into_any_element()))
            }
        };
        let mut right = div().flex().flex_none().items_center().gap_3();
        if let Some(d) = detail {
            right = right.child(d);
        }
        if let Some((label, primary)) = label {
            let is_play = label == "Play";
            right = right.child(button(format!("act-{id}"), label, primary, true).on_click(cx.listener(
                move |this, _, _, cx| {
                    if is_play { this.play(id, cx) } else { this.install(id, cx) }
                    cx.notify();
                },
            )));
        }
        let mut left = div().flex().flex_col().flex_1().min_w_0().gap_1().child(div().truncate().child(row.name.clone()));
        let sub = match (&row.state, &status) {
            (_, Some(s)) => s.clone(),
            (RowState::Installed { update: true }, None) => "Update available".into(),
            (RowState::Installed { .. }, None) => "Installed".into(),
            _ => String::new(),
        };
        if !sub.is_empty() {
            left = left.child(div().text_sm().text_color(rgb(MUTED)).truncate().child(sub));
        }
        div()
            .flex()
            .items_center()
            .gap_4()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(left)
            .child(right)
    }

    fn render_library(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_3()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().text_lg().child("Ubisoft Library"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_sm().text_color(rgb(MUTED)).child(self.account.clone().unwrap_or_default()))
                    .child(button("refresh", "Refresh", false, true).on_click(cx.listener(|this, _, _, cx| this.load_library(cx))))
                    .child(button("signout", "Sign out", false, true).on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))),
            );
        let rows: Vec<_> = self.rows.iter().map(|r| self.render_row(r, cx).into_any_element()).collect();
        let mut body = div().id("games").flex_1().overflow_y_scroll().children(rows);
        if let Some(e) = &self.error {
            body = body.child(div().p_4().text_color(rgb(DANGER)).child(e.clone()));
        }
        div().flex().flex_col().size_full().child(header).child(body)
    }
}

impl Render for Launcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().size_full().bg(rgb(BG)).text_color(rgb(TEXT)).font_family("Inter");
        match &self.screen {
            Screen::Loading(msg) => root
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(rgb(MUTED)).child(msg.clone())),
            Screen::Login(_) => root.flex().items_center().justify_center().child(self.render_login(cx)),
            Screen::Library => root.child(self.render_library(cx)),
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    Application::new().run(|cx: &mut App| {
        text_input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(880.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Ubisoft Library".into()), ..Default::default() }),
                app_id: Some("ubisoft-linux".into()),
                ..Default::default()
            },
            |_, cx| cx.new(Launcher::new),
        )
        .expect("open window");
        cx.activate(true);
    });
}
