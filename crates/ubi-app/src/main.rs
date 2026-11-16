//! Ubisoft Library: GPUI front end. A client of the `ubid` daemon (see `ubi-api`): it shows
//! the daemon's state and sends it requests; installs, launches and settings all live in
//! the daemon, so the CLI and agents see and drive the same thing.

mod text_input;

use std::future::Future;
use std::sync::OnceLock;

use futures::StreamExt;
use gpui::{
    App, Application, Bounds, Context, Entity, SharedString, Stateful, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use text_input::TextInput;
use ubi_api::{Client, Event, Job, JobKind, LibraryEntry, LoginResult, Request, Status};

// ---------------------------------------------------------------------------------------------
// ubi-api's client is tokio-based; GPUI has its own executor. Calls run on a tokio runtime and
// hand results back through oneshot channels, which GPUI tasks can await.

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

fn err_text(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

// ---------------------------------------------------------------------------------------------

struct Row {
    entry: LibraryEntry,
    /// Latest install/launch job for this game.
    job: Option<Job>,
}

struct LoginForm {
    email: Entity<TextInput>,
    password: Entity<TextInput>,
    code: Entity<TextInput>,
    needs_code: bool,
    busy: bool,
    error: Option<String>,
}

enum Screen {
    Loading(String),
    Login(LoginForm),
    Library,
}

struct Launcher {
    client: Option<Client>,
    screen: Screen,
    account: Option<String>,
    rows: Vec<Row>,
    error: Option<String>,
}

impl Launcher {
    fn new(cx: &mut Context<Self>) -> Self {
        let this = Self {
            client: None,
            screen: Screen::Loading("Connecting to ubid…".into()),
            account: None,
            rows: Vec::new(),
            error: None,
        };
        let rx = on_tokio(async {
            let client = Client::connect("gui").await?;
            let status: Status = client.call(Request::Status).await?;
            // Forward daemon events into a channel GPUI can await.
            let mut events = client.subscribe().await?;
            let (tx, rx) = futures::channel::mpsc::unbounded();
            tokio::spawn(async move {
                while let Some(e) = events.recv().await {
                    if tx.unbounded_send(e).is_err() {
                        break;
                    }
                }
            });
            anyhow::Ok((client, status, rx))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let events = this
                .update(cx, |this, cx| {
                    let out = match result {
                        Ok(Ok((client, status, events))) => {
                            this.client = Some(client);
                            match status.signed_in {
                                Some(name) => {
                                    this.account = Some(name);
                                    this.load_library(false, cx);
                                }
                                None => this.show_login(None, cx),
                            }
                            Some(events)
                        }
                        Ok(Err(e)) => {
                            this.screen = Screen::Loading(format!("Could not reach ubid: {}", err_text(&e)));
                            None
                        }
                        Err(_) => None,
                    };
                    cx.notify();
                    out
                })
                .ok()
                .flatten();
            let Some(mut events) = events else { return };
            while let Some(e) = events.next().await {
                if this.update(cx, |this, cx| this.on_event(e, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn on_event(&mut self, e: Event, cx: &mut Context<Self>) {
        match e {
            Event::Job(job) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.entry.product_id == job.product_id) {
                    row.job = Some(job);
                }
            }
            Event::GameStarted { product_id } | Event::GameStopped { product_id } => {
                let running = matches!(e, Event::GameStarted { .. });
                if let Some(row) = self.rows.iter_mut().find(|r| r.entry.product_id == product_id) {
                    row.entry.running = running;
                }
            }
            Event::SessionChanged { signed_in: Some(name) } => self.account = Some(name),
            Event::SessionChanged { signed_in: None } => {
                self.account = None;
                self.rows.clear();
                self.show_login(None, cx);
            }
            Event::LibraryChanged => self.load_library(false, cx),
            Event::SettingsChanged { .. } => {}
        }
        cx.notify();
    }

    /// Send a request to the daemon; `done` runs on the UI thread with the result.
    fn call<T, F>(&self, req: Request, cx: &mut Context<Self>, done: F)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    {
        let Some(client) = self.client.clone() else { return };
        let rx = on_tokio(async move { client.call::<T>(req).await });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("cancelled")));
            this.update(cx, |this, cx| {
                done(this, result, cx);
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
            needs_code: false,
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
        let req = if form.needs_code {
            Request::Login2fa { code: form.code.read(cx).text().trim().to_owned() }
        } else {
            Request::Login {
                email: form.email.read(cx).text().trim().to_owned(),
                password: form.password.read(cx).text().to_owned(),
            }
        };
        self.call::<LoginResult, _>(req, cx, |this, result, cx| {
            let Screen::Login(form) = &mut this.screen else { return };
            form.busy = false;
            match result {
                Ok(LoginResult::SignedIn { name }) => {
                    this.account = Some(name);
                    this.load_library(false, cx);
                }
                Ok(LoginResult::TwoFactorRequired) => form.needs_code = true,
                Err(e) => form.error = Some(err_text(&e)),
            }
        });
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.call::<(), _>(Request::Logout, cx, |_, _, _| {});
    }

    fn load_library(&mut self, refresh: bool, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            self.screen = Screen::Loading("Loading your library…".into());
        }
        self.call::<Vec<LibraryEntry>, _>(Request::Library { refresh }, cx, |this, result, _| {
            match result {
                Ok(entries) => {
                    let old: Vec<Row> = std::mem::take(&mut this.rows);
                    this.rows = entries
                        .into_iter()
                        .map(|entry| {
                            let job = old.iter().find(|r| r.entry.product_id == entry.product_id).and_then(|r| r.job.clone());
                            Row { entry, job }
                        })
                        .collect();
                    this.error = None;
                }
                Err(e) => this.error = Some(err_text(&e)),
            }
            this.screen = Screen::Library;
        });
    }

    fn install(&mut self, product_id: u32, cx: &mut Context<Self>) {
        self.call::<u64, _>(Request::Install { product_id, language: None }, cx, |this, r, _| {
            if let Err(e) = r {
                this.error = Some(err_text(&e));
            }
        });
    }

    fn play(&mut self, product_id: u32, cx: &mut Context<Self>) {
        self.call::<u64, _>(Request::Play { product_id }, cx, |this, r, _| {
            if let Err(e) = r {
                this.error = Some(err_text(&e));
            }
        });
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
        let two_factor = form.needs_code;
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
        let e = &row.entry;
        let id = e.product_id;
        let active = row.job.as_ref().filter(|j| !j.done);
        let installing = active.filter(|j| j.kind == JobKind::Install);
        let launching = active.filter(|j| j.kind == JobKind::Play).is_some() || e.running;
        let mut right = div().flex().flex_none().items_center().gap_3();
        if let Some(j) = installing {
            let frac = if j.total > 0 { j.downloaded as f32 / j.total as f32 } else { 0. };
            let job_id = j.id;
            right = right.child(progress_bar(frac)).child(
                button(format!("cancel-{id}"), "Cancel", false, j.status != "Cancelling…").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.call::<(), _>(Request::Cancel { job_id, discard: false }, cx, |this, r, _| {
                            if let Err(e) = r {
                                this.error = Some(err_text(&e));
                            }
                        });
                    },
                )),
            );
        } else if launching {
            right = right.child(button(format!("stop-{id}"), "Stop", false, true).on_click(cx.listener(move |this, _, _, cx| {
                this.call::<(), _>(Request::Stop { product_id: id }, cx, |_, _, _| {});
            })));
        } else {
            let (label, primary, is_play) = match (&e.installed, e.update_available) {
                (None, _) => ("Install", false, false),
                (Some(_), true) => ("Update", false, false),
                (Some(_), false) => ("Play", true, true),
            };
            right = right.child(button(format!("act-{id}"), label, primary, true).on_click(cx.listener(
                move |this, _, _, cx| {
                    if is_play { this.play(id, cx) } else { this.install(id, cx) }
                },
            )));
        }
        let sub = match (&row.job, &e.installed) {
            (Some(j), _) if !j.done || j.error.is_some() => match &j.error {
                Some(err) => format!("{}: {err}", j.status),
                None => j.status.clone(),
            },
            (_, Some(_)) if e.update_available => "Update available".into(),
            (_, Some(_)) => "Installed".into(),
            _ => String::new(),
        };
        let mut left = div().flex().flex_col().flex_1().min_w_0().gap_1().child(div().truncate().child(e.name.clone()));
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
                    .child(button("refresh", "Refresh", false, true).on_click(cx.listener(|this, _, _, cx| this.load_library(true, cx))))
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
                titlebar: Some(TitlebarOptions { title: Some("Quiver".into()), ..Default::default() }),
                app_id: Some("quiver".into()),
                ..Default::default()
            },
            |_, cx| cx.new(Launcher::new),
        )
        .expect("open window");
        cx.activate(true);
    });
}
