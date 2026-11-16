//! The app view: daemon state, navigation and input. Screens are drawn by `ui::*`, which
//! extend [`Quiver`] with render methods.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, KeyBinding, ScrollHandle, SharedString,
    Window, actions, div, prelude::*, px,
};
use ubi_api::{ActivityEntry, Event, Job, JobKind, LibraryEntry, LoginResult, Playtime, Request, Revision, Status};

use crate::art::{ArtSource, ArtState, ArtStore, CustomArt, Fetcher, Variant};
use crate::assets::UI_FONT;
use crate::gamepad::PadEvent;
use crate::model::{GameRow, clean_name};
use crate::nav::{Effect, Input, Layout, Nav, Page, Section, Tab, Target, Zone};
use crate::source::Source;
use crate::text_input::TextInput;
use crate::theme::{self, Theme, theme};
use crate::{err_text, on_tokio};

actions!(
    quiver,
    [
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        Activate,
        Back,
        PrevSection,
        NextSection,
        PrevTab,
        NextTab,
        OpenPalette,
        OpenMenu,
        PlayFocused,
        GoLibrary,
        GoDownloads,
        GoActivity,
        Refresh,
    ]
);

/// Key context for everything except text fields, so letters typed into a field never
/// trigger navigation.
pub const NAV_CONTEXT: &str = "QuiverNav";

pub fn bind_keys(cx: &mut App) {
    // Not while typing in a field on a page (GPUI's `!` checks the whole context stack).
    let c = Some("QuiverNav && !TextInput");
    cx.bind_keys([
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("k", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("j", MoveDown, c),
        KeyBinding::new("left", MoveLeft, c),
        KeyBinding::new("right", MoveRight, c),
        KeyBinding::new("enter", Activate, c),
        KeyBinding::new("space", Activate, c),
        KeyBinding::new("escape", Back, c),
        KeyBinding::new("backspace", Back, c),
        KeyBinding::new("ctrl-k", OpenPalette, c),
        KeyBinding::new("m", OpenMenu, c),
        KeyBinding::new("p", PlayFocused, c),
        KeyBinding::new("[", PrevTab, c),
        KeyBinding::new("]", NextTab, c),
        KeyBinding::new("ctrl-pageup", PrevTab, c),
        KeyBinding::new("ctrl-pagedown", NextTab, c),
        KeyBinding::new("g l", GoLibrary, c),
        KeyBinding::new("g d", GoDownloads, c),
        KeyBinding::new("g a", GoActivity, c),
        KeyBinding::new("ctrl-r", Refresh, c),
        KeyBinding::new("f5", Refresh, c),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Pointer,
    Keyboard,
    Pad,
}

pub struct LoginForm {
    pub email: Entity<TextInput>,
    pub password: Entity<TextInput>,
    pub code: Entity<TextInput>,
    pub needs_code: bool,
    pub busy: bool,
    pub error: Option<String>,
}

pub enum Conn {
    Connecting,
    Failed(String),
    Login(LoginForm),
    Ready,
}

/// The link to `quiverd` once connected. The client reconnects on its own when the daemon
/// restarts; this is only what the top bar says about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Up,
    /// The daemon went away; the client is retrying with backoff.
    Reconnecting,
    /// Back, shown briefly.
    Reconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideFilter {
    All,
    Installed,
}

/// An entry in the command palette or a menu: a label and the `quiver` command it mirrors.
#[derive(Clone)]
pub struct Command {
    pub icon: crate::ui::Icon,
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub cli: Option<SharedString>,
    pub action: Cmd,
    /// "Not wired yet": shown, but only explains itself when run.
    pub future: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Go(Section),
    Open(u32),
    Primary(u32),
    Tab(u32, Tab),
    CopyCli(u32),
    Refresh,
    NotYet,
    /// Game page: play with play-menu entry `i` (0: vanilla); Steam/.desktop exports.
    PlayWith(u32, usize),
    Export(u32, crate::ui::game::ExportAction),
    /// Settings at a section.
    Settings(crate::nav::SettingsSection),
    /// Accounts, exports, sessions, Activity filters (`ui/global.rs`).
    Global(crate::ui::global::GlobalCmd),
}

pub struct Quiver {
    pub source: Option<Source>,
    pub demo: bool,
    pub conn: Conn,
    pub status: Option<Status>,
    pub games: Vec<GameRow>,
    pub activity: Vec<ActivityEntry>,
    pub nav: Nav,
    pub mode: InputMode,
    pub pad: Option<String>,
    pub art: Entity<ArtStore>,
    pub custom_art: HashMap<u32, CustomArt>,
    pub focus: FocusHandle,
    pub lib_scroll: ScrollHandle,
    pub side_scroll: ScrollHandle,
    pub body_scroll: ScrollHandle,
    pub toast: Option<SharedString>,
    toast_gen: u64,
    pub link: Link,
    link_gen: u64,
    pub side_filter: SideFilter,
    pub lib_filter: crate::ui::library::LibFilter,
    /// What's on screen, as of the last render. Input is resolved against it.
    pub layout: Layout,
    pub viewport: (f32, f32),
    /// Relaunch on update (`selfupdate.rs`).
    pub relaunch: crate::selfupdate::Relaunch,
    /// The game page's own state (`ui::game`).
    pub gp: crate::ui::game::GamePage,
    /// State of the global screens: Settings, palette, Activity, sign-in, now playing.
    pub ui: crate::ui::global::GlobalUi,
}

impl Quiver {
    pub fn new(demo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let art = cx.new(|_| ArtStore::new(Fetcher::new(crate::art::default_cache_dir(), crate::art::CDN)));
        cx.observe(&art, |_, _, cx| cx.notify()).detach();
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut this = Self {
            source: None,
            demo,
            conn: Conn::Connecting,
            status: None,
            games: Vec::new(),
            activity: Vec::new(),
            nav: Nav::new(),
            mode: InputMode::Pointer,
            pad: None,
            art,
            custom_art: crate::art::load_custom_art(&crate::art::custom_art_path()),
            focus,
            lib_scroll: ScrollHandle::new(),
            side_scroll: ScrollHandle::new(),
            body_scroll: ScrollHandle::new(),
            toast: None,
            toast_gen: 0,
            link: Link::Up,
            link_gen: 0,
            side_filter: SideFilter::All,
            lib_filter: crate::ui::library::LibFilter::All,
            layout: Layout::default(),
            viewport: (1440., 900.),
            relaunch: Default::default(),
            gp: Default::default(),
            ui: Default::default(),
        };
        this.connect(cx);
        this.start_theme_watch(cx);
        this.start_gamepad(window, cx);
        this.start_global(window, cx);
        if demo {
            this.start_demo_script(window, cx);
        }
        this
    }

    // -----------------------------------------------------------------------------------------
    // Daemon

    fn connect(&self, cx: &mut Context<Self>) {
        let demo = self.demo;
        let rx = on_tokio(async move {
            let (source, status, mut events) = Source::connect(demo).await?;
            // Forward daemon events into a channel GPUI can await.
            let (tx, rx) = futures::channel::mpsc::unbounded();
            tokio::spawn(async move {
                while let Some(e) = events.recv().await {
                    if tx.unbounded_send(e).is_err() {
                        break;
                    }
                }
            });
            anyhow::Ok((source, status, rx))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let events = this
                .update(cx, |this, cx| {
                    let out = match result {
                        Ok(Ok((source, status, events))) => {
                            this.source = Some(source);
                            this.on_status(status, cx);
                            Some(events)
                        }
                        Ok(Err(e)) => {
                            this.conn = Conn::Failed(err_text(&e));
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
    }

    pub fn retry_connect(&mut self, cx: &mut Context<Self>) {
        self.conn = Conn::Connecting;
        self.connect(cx);
        cx.notify();
    }

    fn on_status(&mut self, status: Status, cx: &mut Context<Self>) {
        let signed_in = status.signed_in.is_some();
        self.status = Some(status);
        if signed_in {
            self.conn = Conn::Ready;
            self.load_library(false, cx);
        } else {
            self.show_login(None, cx);
        }
        self.global_status(cx);
    }

    /// Send a request; `done` runs on the UI thread with the result.
    pub fn call<T, F>(&self, req: Request, cx: &mut Context<Self>, done: F)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    {
        let Some(source) = self.source.clone() else { return };
        let rx = on_tokio(async move { source.call::<T>(req).await });
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

    fn on_event(&mut self, e: Event, cx: &mut Context<Self>) {
        self.global_event(&e, cx);
        match e {
            Event::Job(job) => self.apply_job(job),
            Event::GameStarted { product_id } | Event::GameStopped { product_id } => {
                let running = matches!(e, Event::GameStarted { .. });
                if let Some(g) = self.game_mut(product_id) {
                    g.entry.running = running;
                }
                if !running {
                    self.load_details(product_id, cx);
                }
            }
            Event::SessionChanged { signed_in: Some(name) } => {
                if let Some(s) = &mut self.status {
                    s.signed_in = Some(name);
                }
                if !matches!(self.conn, Conn::Ready) {
                    self.conn = Conn::Ready;
                    self.load_library(false, cx);
                }
            }
            Event::SessionChanged { signed_in: None } => {
                if let Some(s) = &mut self.status {
                    s.signed_in = None;
                }
                self.games.clear();
                self.show_login(None, cx);
            }
            Event::LibraryChanged => self.load_library(false, cx),
            Event::SettingsChanged { product_id, .. } => self.load_details(product_id, cx),
            Event::ModsChanged { product_id } => self.load_page(product_id, cx),
            Event::SessionsChanged { .. } => {}
            Event::Disconnected => {
                self.link = Link::Reconnecting;
                self.link_gen += 1;
            }
            Event::Reconnected => self.on_reconnected(cx),
        }
        cx.notify();
    }

    /// The daemon is back (likely restarted): events in between were missed and its jobs
    /// may be gone, so reload status, jobs and the library.
    fn on_reconnected(&mut self, cx: &mut Context<Self>) {
        self.link = Link::Reconnected;
        self.link_gen += 1;
        let generation = self.link_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(3200)).await;
            this.update(cx, |this, cx| {
                if this.link_gen == generation {
                    this.link = Link::Up;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        self.call::<Status, _>(Request::Status, cx, |this, r, cx| {
            let status = match r {
                Ok(s) => s,
                Err(e) => return this.show_toast(format!("Could not reload status: {}", err_text(&e)), cx),
            };
            // Job ids restart with the daemon; take its list as the whole truth.
            for g in &mut this.games {
                g.job = None;
            }
            match (status.signed_in.is_some(), &this.conn) {
                (true, _) => {
                    this.status = Some(status);
                    this.conn = Conn::Ready;
                    this.load_library(false, cx);
                }
                // Keep a half-filled sign-in form.
                (false, Conn::Login(_)) => this.status = Some(status),
                (false, _) => {
                    this.status = Some(status);
                    this.games.clear();
                    this.show_login(None, cx);
                }
            }
        });
    }

    fn apply_job(&mut self, job: Job) {
        if let Some(s) = &mut self.status {
            match s.jobs.iter_mut().find(|j| j.id == job.id) {
                Some(j) => *j = job.clone(),
                None => s.jobs.push(job.clone()),
            }
        }
        if let Some(g) = self.game_mut(job.product_id) {
            // Keep an unfinished install visible over a finished launch, and vice versa.
            let keep_old = g.job.as_ref().is_some_and(|old| old.id != job.id && !old.done && job.done);
            if !keep_old {
                g.job = Some(job);
            }
        }
    }

    pub fn load_library(&mut self, refresh: bool, cx: &mut Context<Self>) {
        self.call::<Vec<LibraryEntry>, _>(Request::Library { refresh }, cx, |this, result, cx| match result {
            Ok(entries) => {
                let mut old: HashMap<u32, GameRow> = this.games.drain(..).map(|g| (g.id(), g)).collect();
                this.games = entries
                    .into_iter()
                    .map(|mut entry| {
                        entry.name = clean_name(&entry.name);
                        match old.remove(&entry.product_id) {
                            Some(mut g) => {
                                g.entry = entry;
                                g
                            }
                            None => GameRow::new(entry),
                        }
                    })
                    .collect();
                let jobs = this.status.as_ref().map(|s| s.jobs.clone()).unwrap_or_default();
                for j in jobs {
                    this.apply_job(j);
                }
                let ids: Vec<u32> = this.games.iter().filter(|g| g.installed()).map(|g| g.id()).collect();
                for id in ids {
                    this.load_playtime(id, cx);
                }
                if let Page::Game(id) = this.nav.page {
                    this.load_details(id, cx);
                }
            }
            Err(e) => this.show_toast(format!("Could not load the library: {}", err_text(&e)), cx),
        });
        self.refresh_activity(cx);
    }

    fn refresh_activity(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<ActivityEntry>, _>(Request::Activity { limit: 100 }, cx, |this, r, _| {
            if let Ok(a) = r {
                this.activity = a;
            }
        });
    }

    fn load_playtime(&mut self, id: u32, cx: &mut Context<Self>) {
        self.call::<Playtime, _>(Request::Playtime { product_id: id }, cx, move |this, r, cx| {
            if let (Ok(p), Some(g)) = (r, this.game_mut(id)) {
                g.playtime = Some(p);
            }
            // The library hero shows the featured game's launch options too.
            let hero = this.hero_game().filter(|g| g.settings.is_none()).map(|g| g.id());
            if hero == Some(id) {
                this.load_settings(id, cx);
            }
        });
    }

    /// Everything the game page shows that the library doesn't carry.
    pub fn load_details(&mut self, id: u32, cx: &mut Context<Self>) {
        self.load_playtime(id, cx);
        self.load_settings(id, cx);
        self.load_page(id, cx);
    }

    fn load_settings(&mut self, id: u32, cx: &mut Context<Self>) {
        if !self.game(id).is_some_and(|g| g.installed()) {
            return;
        }
        // The daemon replies `{revision, settings}`.
        self.call::<serde_json::Value, _>(Request::SettingsGet { product_id: id }, cx, move |this, r, _| {
            if let (Ok(v), Some(g)) = (r, this.game_mut(id)) {
                g.settings = Some(crate::ui::game::settings_from_reply(v).0);
            }
        });
        self.call::<Vec<Revision>, _>(Request::SettingsHistory { product_id: id }, cx, move |this, r, _| {
            if let (Ok(h), Some(g)) = (r, this.game_mut(id)) {
                g.history = Some(h);
            }
        });
    }

    pub fn game(&self, id: u32) -> Option<&GameRow> {
        self.games.iter().find(|g| g.id() == id)
    }

    fn game_mut(&mut self, id: u32) -> Option<&mut GameRow> {
        self.games.iter_mut().find(|g| g.id() == id)
    }

    // -----------------------------------------------------------------------------------------
    // Sign in

    pub fn show_login(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.conn = Conn::Login(LoginForm {
            email: cx.new(|cx| TextInput::new(cx, "name@example.com", false)),
            password: cx.new(|cx| TextInput::new(cx, "Password", true)),
            code: cx.new(|cx| TextInput::new(cx, "123456", false)),
            needs_code: false,
            busy: false,
            error,
        });
    }

    pub fn submit_login(&mut self, cx: &mut Context<Self>) {
        let Conn::Login(form) = &mut self.conn else { return };
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
            let Conn::Login(form) = &mut this.conn else { return };
            form.busy = false;
            match result {
                Ok(LoginResult::SignedIn { name }) => {
                    if let Some(s) = &mut this.status {
                        s.signed_in = Some(name);
                    }
                    this.conn = Conn::Ready;
                    this.load_library(false, cx);
                }
                Ok(LoginResult::TwoFactorRequired) => form.needs_code = true,
                Err(e) => form.error = Some(err_text(&e)),
            }
        });
    }

    // -----------------------------------------------------------------------------------------
    // Art

    pub fn art_state(&self, src: Option<ArtSource>, variant: Variant, cx: &mut Context<Self>) -> ArtState {
        match src {
            Some(src) => self.art.update(cx, |a, cx| a.get(&src, variant, cx)),
            None => ArtState::Failed,
        }
    }

    pub fn art_for(&self, g: &GameRow, slot: crate::art::ArtSlot, variant: Variant, cx: &mut Context<Self>) -> ArtState {
        let src = crate::art::resolve(&g.entry.art, self.custom_art.get(&g.id()), slot);
        self.art_state(src, variant, cx)
    }

    // -----------------------------------------------------------------------------------------
    // Library order and layout

    /// The library in display order: installed and active games by last played, then the
    /// rest by name.
    pub fn ordered(&self) -> Vec<&GameRow> {
        let mut v: Vec<&GameRow> = self.games.iter().collect();
        v.sort_by(|a, b| {
            let group = |g: &GameRow| if g.installed() || g.install_job().is_some() { 0 } else { 1 };
            group(a)
                .cmp(&group(b))
                .then_with(|| b.last_played().cmp(&a.last_played()))
                .then_with(|| a.install_job().is_none().cmp(&b.install_job().is_none()))
                .then_with(|| a.queued().cmp(&b.queued()))
                .then_with(|| a.entry.name.cmp(&b.entry.name))
        });
        v
    }

    pub fn side_games(&self) -> Vec<&GameRow> {
        self.ordered()
            .into_iter()
            .filter(|g| self.side_filter == SideFilter::All || g.installed() || g.install_job().is_some())
            .collect()
    }

    /// The game the library hero features: the most recently played installed game.
    pub fn hero_game(&self) -> Option<&GameRow> {
        self.games.iter().filter(|g| g.installed() && g.last_played().is_some()).max_by_key(|g| g.last_played())
    }

    /// Things waiting on the user, for the hero's "Needs attention" panel.
    pub fn attention(&self) -> Vec<(u32, Attention)> {
        let mut out = vec![];
        for g in self.ordered() {
            if let Some(e) = g.failed() {
                out.push((g.id(), Attention::Failed(e.to_owned())));
            } else if let Some(f) = g.installing() {
                out.push((g.id(), Attention::Installing(f)));
            } else if g.installed() && g.entry.update_available {
                out.push((g.id(), Attention::Update));
            }
        }
        out.truncate(4);
        out
    }

    pub const GRID_GAP: f32 = 18.;
    pub const GUTTER: f32 = 40.;

    /// Grid columns and tile width for the current window.
    pub fn grid_metrics(&self) -> (usize, f32) {
        let avail = (self.viewport.0 - 2. * Self::GUTTER).max(160.);
        let cols = (((avail + Self::GRID_GAP) / (150. + Self::GRID_GAP)).floor() as usize).clamp(2, 12);
        let w = (avail - Self::GRID_GAP * (cols as f32 - 1.)) / cols as f32;
        (cols, w)
    }

    pub fn compute_layout(&self) -> Layout {
        let grid: Vec<u32> = self.grid_games().iter().map(|g| g.id()).collect();
        let side: Vec<u32> = self.side_games().iter().map(|g| g.id()).collect();
        let hero_buttons = if self.hero_game().is_some() { 2 + self.attention().len() } else { 0 };
        let body = match self.nav.page {
            Page::Downloads => self.download_jobs().len(),
            Page::Activity => self.activity_rows().len(),
            Page::Settings => self.settings_actions().len(),
            Page::Game(id) => self.page_body_len(id, self.nav.tab),
            Page::Library => 0,
        };
        Layout {
            grid,
            columns: self.grid_metrics().0,
            hero_buttons,
            side,
            actions: GAME_ACTIONS,
            body,
            palette_items: self.palette_commands().len(),
            menu_items: match self.nav.overlay {
                Some(crate::nav::Overlay::Menu { game, .. }) => self.menu_commands(game).len(),
                _ => 0,
            },
        }
    }

    /// Install jobs for the Downloads page: active first, then finished.
    pub fn download_jobs(&self) -> Vec<Job> {
        let mut jobs: Vec<Job> = self
            .status
            .as_ref()
            .map(|s| s.jobs.iter().filter(|j| j.kind != JobKind::Play).cloned().collect())
            .unwrap_or_default();
        jobs.sort_by_key(|j| (j.done, j.status.starts_with("Queued"), std::cmp::Reverse(j.id)));
        jobs
    }

    // -----------------------------------------------------------------------------------------
    // Commands

    pub fn palette_commands(&self) -> Vec<Command> {
        self.global_palette_commands()
    }

    fn primary_command(&self, g: &GameRow) -> Command {
        let (icon, label, cli) = if g.running() {
            (crate::ui::Icon::Square, "Stop", format!("quiver stop {}", g.id()))
        } else if !g.installed() {
            (crate::ui::Icon::Download, "Install", format!("quiver install {}", g.id()))
        } else if g.entry.update_available {
            (crate::ui::Icon::ArrowDownToLine, "Update", format!("quiver install {}", g.id()))
        } else {
            (crate::ui::Icon::Play, "Play", format!("quiver play {}", g.id()))
        };
        Command {
            icon,
            label: format!("{label} {}", g.entry.name).into(),
            detail: None,
            cli: Some(cli.into()),
            action: Cmd::Primary(g.id()),
            future: false,
        }
    }

    pub fn menu_commands(&self, id: u32) -> Vec<Command> {
        if let Some(v) = self.play_menu_commands(id) {
            return v;
        }
        let Some(g) = self.game(id) else { return vec![] };
        let mut v = vec![self.primary_command(g)];
        v.push(Command {
            icon: crate::ui::Icon::Info,
            label: "Game details".into(),
            detail: None,
            cli: Some(format!("quiver show {id}").into()),
            action: Cmd::Open(id),
            future: false,
        });
        v.push(Command {
            icon: crate::ui::Icon::SlidersHorizontal,
            label: "Launch options…".into(),
            detail: None,
            cli: Some(format!("quiver history {id}").into()),
            action: Cmd::Tab(id, Tab::LaunchOptions),
            future: false,
        });
        v.extend(self.export_commands(id));
        v.push(Command {
            icon: crate::ui::Icon::Copy,
            label: "Copy play command".into(),
            detail: None,
            cli: Some(format!("quiver play {id}").into()),
            action: Cmd::CopyCli(id),
            future: false,
        });
        v
    }

    fn run_command(&mut self, cmd: &Command, cx: &mut Context<Self>) {
        match cmd.action {
            Cmd::Go(s) => self.nav.open_section(s),
            Cmd::Open(id) => self.open_game(id, cx),
            Cmd::Primary(id) => self.primary(id, cx),
            Cmd::Tab(id, t) => {
                self.open_game(id, cx);
                self.nav.select_tab(t);
                self.nav.zone = Zone::Tabs;
            }
            Cmd::CopyCli(id) => {
                let text = format!("quiver play {id}");
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                self.show_toast(format!("Copied: {text}"), cx);
            }
            Cmd::Refresh => {
                self.load_library(true, cx);
                self.show_toast("Refreshing the library…", cx);
            }
            Cmd::NotYet => self.show_toast(format!("{}: not wired yet", cmd.label), cx),
            Cmd::PlayWith(id, i) => self.play_with(id, i, cx),
            Cmd::Export(id, a) => self.run_export(id, a, cx),
            Cmd::Settings(s) => self.nav.open_settings(s),
            Cmd::Global(g) => self.run_global(g, cx),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Actions

    pub fn open_game(&mut self, id: u32, cx: &mut Context<Self>) {
        self.nav.open_game(id);
        self.load_details(id, cx);
    }

    /// Play, or whatever stands in for it: Stop while running, Install / Update otherwise.
    pub fn primary(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(g) = self.game(id) else { return };
        let name = g.entry.name.clone();
        if g.running() {
            self.call::<(), _>(Request::Stop { product_id: id }, cx, |this, r, cx| {
                if let Err(e) = r {
                    this.show_toast(err_text(&e), cx);
                }
            });
        } else if let Some(f) = g.installing() {
            self.show_toast(format!("{name} is installing ({:.0}%)", (f * 100.).floor()), cx);
        } else if g.queued() {
            self.show_toast(format!("{name} is queued"), cx);
        } else if !g.installed() || g.entry.update_available {
            let verb = if g.installed() { "Updating" } else { "Installing" };
            self.call::<u64, _>(Request::Install { product_id: id, language: None }, cx, move |this, r, cx| match r {
                Ok(_) => this.show_toast(format!("{verb} {name}"), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            });
        } else {
            let profile = self.gp.play_profile(id);
            self.call::<u64, _>(Request::Play { product_id: id, profile }, cx, move |this, r, cx| match r {
                Ok(_) => this.show_toast(format!("Starting {name}…"), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            });
        }
    }

    pub fn show_toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some(text.into());
        self.toast_gen += 1;
        let generation = self.toast_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(3200)).await;
            this.update(cx, |this, cx| {
                if this.toast_gen == generation {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn run_effect(&mut self, effect: Effect, cx: &mut Context<Self>) {
        match effect {
            Effect::None | Effect::Moved => {}
            Effect::Play(id) => self.primary(id, cx),
            Effect::Hero(i) => {
                let Some(hero) = self.hero_game().map(|g| g.id()) else { return };
                match i {
                    0 => self.primary(hero, cx),
                    1 => self.open_game(hero, cx),
                    i => {
                        if let Some((id, a)) = self.attention().get(i - 2).cloned() {
                            match a {
                                Attention::Update => self.primary(id, cx),
                                Attention::Installing(_) => self.nav.open_section(Section::Downloads),
                                Attention::Failed(_) => self.open_game(id, cx),
                            }
                        }
                    }
                }
            }
            Effect::Action(id, i) => match i {
                ACTION_PRIMARY => self.primary(id, cx),
                ACTION_OPTIONS | ACTION_MORE => {
                    self.gp.play_menu = i == ACTION_OPTIONS;
                    self.nav.open_menu(id);
                }
                ACTION_STEAM => self.steam_button(id, cx),
                ACTION_FILES => self.nav.select_tab(Tab::Files),
                ACTION_LAUNCH => self.nav.select_tab(Tab::LaunchOptions),
                _ => {}
            },
            Effect::Body(i) => match self.nav.page {
                Page::Downloads => {
                    if let Some(j) = self.download_jobs().get(i) {
                        let id = j.product_id;
                        self.open_game(id, cx);
                    }
                }
                Page::Activity => self.activity_open(i, cx),
                Page::Settings => self.settings_activate(i, cx),
                Page::Game(id) => self.page_body_activate(id, i, cx),
                _ => {}
            },
            Effect::Palette(i) => {
                if let Some(c) = self.palette_commands().get(i).cloned() {
                    self.run_command(&c, cx);
                }
            }
            Effect::Menu(id, i) => {
                if let Some(c) = self.menu_commands(id).get(i).cloned() {
                    self.run_command(&c, cx);
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Input

    pub fn input(&mut self, input: Input, mode: InputMode, cx: &mut Context<Self>) {
        if !matches!(self.conn, Conn::Ready) {
            return;
        }
        self.mode = mode;
        if self.global_input(input, cx) || self.page_input(input, cx) {
            cx.notify();
            return;
        }
        let page_before = self.nav.page;
        let effect = self.nav.apply(input, &self.layout);
        self.run_effect(effect, cx);
        if let (Page::Game(id), true) = (self.nav.page, self.nav.page != page_before) {
            self.load_details(id, cx);
        }
        if mode != InputMode::Pointer {
            self.scroll_focus_into_view(page_before);
        }
        cx.notify();
    }

    /// A click: focus the target, then activate it.
    pub fn click(&mut self, target: Target, cx: &mut Context<Self>) {
        self.nav.focus(&self.layout, target);
        self.input(Input::Activate, InputMode::Pointer, cx);
    }

    fn scroll_focus_into_view(&mut self, page_before: Page) {
        let page_changed = std::mem::discriminant(&page_before) != std::mem::discriminant(&self.nav.page);
        match (self.nav.page, self.nav.zone) {
            (Page::Library, Zone::Grid) => {
                let cols = self.layout.columns.max(1);
                // Children of the scroll view: hero, header, then one per grid row.
                self.lib_scroll.scroll_to_item(2 + self.nav.grid_index() / cols);
            }
            (Page::Library, _) => self.lib_scroll.set_offset(gpui::point(px(0.), px(0.))),
            (Page::Game(id), _) => {
                if let Some(i) = self.side_child_index(id) {
                    self.side_scroll.scroll_to_item(i);
                }
                if page_changed || self.nav.zone != Zone::Body {
                    self.body_scroll.set_offset(gpui::point(px(0.), px(0.)));
                }
            }
            (Page::Downloads, Zone::Body) => {
                // Children: title, then one per row.
                self.body_scroll.scroll_to_item(1 + self.nav.body_index());
            }
            (Page::Activity | Page::Settings, _) => self.scroll_global_focus(),
            _ => {}
        }
    }

    /// Index of a game's row among the side list's children (group headers included).
    pub fn side_child_index(&self, id: u32) -> Option<usize> {
        let games = self.side_games();
        let pos = games.iter().position(|g| g.id() == id)?;
        let first_rest = games.iter().position(|g| !(g.installed() || g.install_job().is_some()));
        Some(match first_rest {
            Some(r) if pos >= r => pos + 2,
            _ => pos + 1,
        })
    }

    fn scroll_by(&mut self, dy: f32) {
        let handle = match self.nav.page {
            Page::Library => &self.lib_scroll,
            _ => &self.body_scroll,
        };
        let o = handle.offset();
        let max = handle.max_offset();
        let y = (f32::from(o.y) - dy).clamp(-f32::from(max.height), 0.);
        handle.set_offset(gpui::point(o.x, px(y)));
    }

    // -----------------------------------------------------------------------------------------
    // Background sources: theme, gamepad, demo script

    fn start_theme_watch(&self, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        theme::watch(theme::omarchy_state_dir(), move || {
            let _ = tx.unbounded_send(());
        });
        cx.spawn(async move |_, cx| {
            while rx.next().await.is_some() {
                let t = theme::load(&theme::omarchy_state_dir());
                tracing::info!("theme changed: {}", t.name);
                if cx
                    .update(|cx| {
                        cx.set_global(t);
                        cx.refresh_windows();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_gamepad(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<PadEvent>();
        crate::gamepad::spawn(move |e| {
            let _ = tx.unbounded_send(e);
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some(e) = rx.next().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    // gilrs reads the pad whether or not we have focus; only act on it
                    // while Quiver's window is active (hyprpad forwards it then).
                    if !window.is_window_active() && !matches!(e, PadEvent::Connected(_) | PadEvent::Disconnected) {
                        return;
                    }
                    match e {
                        PadEvent::Input(i) => this.input(i, InputMode::Pad, cx),
                        PadEvent::Scroll(dy) => {
                            this.scroll_by(dy);
                            cx.notify();
                        }
                        PadEvent::Connected(name) => {
                            tracing::info!("gamepad: {name}");
                            this.pad = Some(name);
                        }
                        PadEvent::Disconnected => {
                            this.pad = None;
                            if this.mode == InputMode::Pad {
                                this.mode = InputMode::Pointer;
                            }
                            cx.notify();
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Demo mode only: `QUIVER_DEMO_INPUT="pad down right a"` replays inputs after the
    /// library loads, so screens can be driven for screenshots without a device. Tokens:
    /// `pad`/`key` (input mode), `up down left right a b x y lb rb lt rt start p`,
    /// `wait` (an extra pause).
    fn start_demo_script(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(script) = std::env::var("QUIVER_DEMO_INPUT") else { return };
        let tokens: Vec<String> = script.split_whitespace().map(str::to_owned).collect();
        cx.spawn_in(window, async move |this, cx| {
            // Wait until the library is on screen.
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                let ready = this.update_in(cx, |this, _, _| !this.layout.grid.is_empty()).unwrap_or(false);
                if ready {
                    break;
                }
            }
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let mut mode = InputMode::Keyboard;
            for t in tokens {
                let input = match t.as_str() {
                    "pad" => {
                        mode = InputMode::Pad;
                        None
                    }
                    "key" => {
                        mode = InputMode::Keyboard;
                        None
                    }
                    "wait" => {
                        cx.background_executor().timer(Duration::from_millis(1000)).await;
                        None
                    }
                    "up" => Some(Input::Up),
                    "down" => Some(Input::Down),
                    "left" => Some(Input::Left),
                    "right" => Some(Input::Right),
                    "a" | "enter" => Some(Input::Activate),
                    "b" | "esc" => Some(Input::Back),
                    "x" => Some(Input::PlayOptions),
                    "y" => Some(Input::Menu),
                    "lb" => Some(Input::PrevSection),
                    "rb" => Some(Input::NextSection),
                    "lt" => Some(Input::PrevTab),
                    "rt" => Some(Input::NextTab),
                    "start" => Some(Input::Palette),
                    "p" => Some(Input::Play),
                    other => {
                        tracing::warn!("QUIVER_DEMO_INPUT: unknown token {other:?}");
                        None
                    }
                };
                if let Some(i) = input {
                    if this.update_in(cx, |this, _, cx| this.input(i, mode, cx)).is_err() {
                        return;
                    }
                    cx.background_executor().timer(Duration::from_millis(250)).await;
                } else if mode == InputMode::Pad {
                    let _ = this.update_in(cx, |this, _, cx| {
                        this.mode = InputMode::Pad;
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    // -----------------------------------------------------------------------------------------
    // Key actions

    fn key(&mut self, i: Input, cx: &mut Context<Self>) {
        self.input(i, InputMode::Keyboard, cx);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Attention {
    Update,
    Installing(f32),
    Failed(String),
}

/// The game page's action bar, in focus order.
pub const ACTION_PRIMARY: usize = 0;
pub const ACTION_OPTIONS: usize = 1;
pub const ACTION_STEAM: usize = 2;
pub const ACTION_FILES: usize = 3;
pub const ACTION_LAUNCH: usize = 4;
pub const ACTION_MORE: usize = 5;
pub const GAME_ACTIONS: usize = 6;

impl Focusable for Quiver {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Quiver {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th: Theme = theme(cx).clone();
        tracing::trace!("render");
        let vp = window.viewport_size();
        self.viewport = (f32::from(vp.width), f32::from(vp.height));
        self.sync_global_focus(window, cx);
        self.layout = self.compute_layout();

        let root = div()
            .id("quiver")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(th.ink1)
            .text_color(th.tx1)
            .font_family(UI_FONT)
            .text_size(px(13.))
            .line_height(px(18.));

        let ready = matches!(self.conn, Conn::Ready);
        let screen = match &self.conn {
            Conn::Connecting => root.child(self.render_splash(&th, None, cx)),
            Conn::Failed(e) => {
                let e = e.clone();
                root.child(self.render_splash(&th, Some(e), cx))
            }
            Conn::Login(_) => root.key_context(crate::ui::global::LOGIN_CONTEXT).child(self.render_login(&th, cx)),
            Conn::Ready => root
                .track_focus(&self.focus)
                .key_context(self.root_context())
                .map(|d| self.global_actions(d, cx))
                .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.key(Input::Up, cx)))
                .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.key(Input::Down, cx)))
                .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.key(Input::Left, cx)))
                .on_action(cx.listener(|this, _: &MoveRight, _, cx| this.key(Input::Right, cx)))
                .on_action(cx.listener(|this, _: &Activate, _, cx| this.key(Input::Activate, cx)))
                .on_action(cx.listener(|this, _: &Back, _, cx| this.key(Input::Back, cx)))
                .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.key(Input::PrevTab, cx)))
                .on_action(cx.listener(|this, _: &NextTab, _, cx| this.key(Input::NextTab, cx)))
                .on_action(cx.listener(|this, _: &PrevSection, _, cx| this.key(Input::PrevSection, cx)))
                .on_action(cx.listener(|this, _: &NextSection, _, cx| this.key(Input::NextSection, cx)))
                .on_action(cx.listener(|this, _: &OpenPalette, _, cx| this.key(Input::Palette, cx)))
                .on_action(cx.listener(|this, _: &OpenMenu, _, cx| this.key(Input::Menu, cx)))
                .on_action(cx.listener(|this, _: &PlayFocused, _, cx| this.key(Input::Play, cx)))
                .on_action(cx.listener(|this, _: &GoLibrary, _, cx| {
                    this.nav.open_section(Section::Library);
                    this.mode = InputMode::Keyboard;
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &GoDownloads, _, cx| {
                    this.nav.open_section(Section::Downloads);
                    this.mode = InputMode::Keyboard;
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &GoActivity, _, cx| {
                    this.nav.open_section(Section::Activity);
                    this.mode = InputMode::Keyboard;
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                    this.load_library(true, cx);
                    this.show_toast("Refreshing the library…", cx);
                }))
                // Any pointer movement hides keyboard/controller affordances again.
                .on_mouse_move(cx.listener(|this, _, _, cx| {
                    if this.mode != InputMode::Pointer {
                        this.mode = InputMode::Pointer;
                        cx.notify();
                    }
                }))
                .child(self.render_topbar(&th, cx))
                .when_some(self.render_link_banner(&th, cx), |d, b| d.child(b))
                .child(self.render_page(&th, window, cx))
                .when(self.mode == InputMode::Pad, |d| d.child(self.render_hintbar(&th)))
                .when_some(self.render_overlay(&th, cx), |d, o| d.child(o))
                .when_some(self.render_global_overlay(&th, cx), |d, o| d.child(o))
                .when_some(self.toast.clone(), |d, t| d.child(crate::ui::overlay::toast(&th, t))),
        };
        // The top bar shows the daemon link; other screens get a notice instead.
        let notice = match self.link {
            _ if ready => None,
            Link::Up => None,
            Link::Reconnecting => Some("Reconnecting to quiverd…"),
            Link::Reconnected => Some("Reconnected to quiverd"),
        };
        let update = self.render_update_notice(&th, cx);
        screen
            .when_some(notice, |d, n| d.child(crate::ui::overlay::toast(&th, n.into())))
            .when_some(update, |d, n| d.child(n))
    }
}

impl Quiver {
    fn render_page(&mut self, th: &Theme, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.nav.page {
            Page::Library => self.render_library(th, cx).into_any_element(),
            Page::Game(id) => self.render_game(th, id, window, cx).into_any_element(),
            Page::Downloads => self.render_downloads(th, cx).into_any_element(),
            Page::Activity => self.render_activity(th, cx).into_any_element(),
            Page::Settings => self.render_settings(th, cx).into_any_element(),
        }
    }

    pub fn focused(&self, t: Target) -> bool {
        self.mode != InputMode::Pointer && self.nav.overlay.is_none() && self.ui.confirm.is_none() && self.nav.is_focused(&self.layout, t)
    }
}
