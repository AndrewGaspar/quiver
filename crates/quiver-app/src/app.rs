//! The app view: daemon state, navigation and input. Screens are drawn by `ui::*`, which
//! extend [`Quiver`] with render methods.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, KeyBinding, ScrollHandle, SharedString,
    Window, actions, div, prelude::*, px,
};
use quiver_api::{ActivityEntry, Event, Job, LibraryEntry, LoginResult, Playtime, Request, Revision, Status};

use crate::art::{ArtSource, ArtState, ArtStore, CustomArt, Fetcher, Variant};
use crate::assets::UI_FONT;
use crate::gamepad::PadEvent;
use crate::model::{Gid, GameRow, clean_name};
use crate::nav::{Effect, Input, Layout, Nav, Page, Section, Tab, Target, Zone};
use crate::source::Source;
use crate::text_input::TextInput;
use crate::theme::{self, Theme, theme};
use crate::ui::game::PlayAfter;
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
    Open(Gid),
    Primary(Gid),
    Tab(Gid, Tab),
    CopyCli(Gid),
    Refresh,
    NotYet,
    /// Game page: play once with play-menu entry `i` (0: vanilla); Steam/.desktop exports.
    PlayWith(Gid, usize),
    /// Game page: make profile entry `i` (0: vanilla) the default for Play; open that menu.
    DefaultProfile(Gid, usize),
    DefaultMenu(Gid),
    Export(Gid, crate::ui::game::ExportAction),
    /// Settings at a section.
    Settings(crate::nav::SettingsSection),
    /// Accounts, exports, sessions, Activity filters (`ui/global.rs`).
    Global(crate::ui::global::GlobalCmd),
    /// The verify dialog, the welcome flow (`ui/downloads.rs`).
    Downloads(crate::ui::downloads::DlCmd),
}

pub struct Quiver {
    pub source: Option<Source>,
    pub demo: bool,
    pub conn: Conn,
    pub status: Option<Status>,
    pub games: Vec<GameRow>,
    /// The library has answered at least once (an empty library isn't "loading").
    pub library_loaded: bool,
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
    /// quiverd said it is restarting onto this build (`Event::DaemonRestarting`): shown
    /// once it is back.
    daemon_update: Option<String>,
    pub side_filter: SideFilter,
    pub lib_filter: crate::ui::library::LibFilter,
    /// Only one store's games in the grid (`None`: every store).
    pub store_filter: Option<quiver_api::StoreId>,
    /// What's on screen, as of the last render. Input is resolved against it.
    pub layout: Layout,
    pub viewport: (f32, f32),
    /// Relaunch on update (`selfupdate.rs`).
    pub relaunch: crate::selfupdate::Relaunch,
    /// The game page's own state (`ui::game`).
    pub gp: crate::ui::game::GamePage,
    /// State of the global screens: Settings, palette, Activity, sign-in, now playing.
    pub ui: crate::ui::global::GlobalUi,
    /// Downloads page and dialogs (`ui/downloads.rs`), and the welcome flow (`ui/welcome.rs`).
    pub dl: crate::ui::downloads::DlState,
    pub wc: crate::ui::welcome::Welcome,
    /// Performance and the HUD (`ui/perf.rs`, `ui/game/perf.rs`).
    pub perf: crate::ui::perf::PerfUi,
    /// Quiver Tweaks: Settings → Tweaks and the game page's Tweaks tab.
    pub tw: crate::ui::game::tweaks::TweaksUi,
}

impl Quiver {
    pub fn new(demo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let art = cx.new(|_| ArtStore::new(Fetcher::new(crate::art::default_cache_dir(), crate::art::CDN)));
        cx.observe(&art, |_, _, cx| cx.notify()).detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut this = Self {
            source: None,
            demo,
            conn: Conn::Connecting,
            status: None,
            games: Vec::new(),
            library_loaded: false,
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
            daemon_update: None,
            side_filter: SideFilter::All,
            lib_filter: crate::ui::library::LibFilter::All,
            store_filter: None,
            layout: Layout::default(),
            viewport: (1440., 900.),
            relaunch: Default::default(),
            gp: Default::default(),
            ui: Default::default(),
            dl: Default::default(),
            wc: Default::default(),
            perf: Default::default(),
            tw: Default::default(),
        };
        this.connect(cx);
        this.start_theme_watch(cx);
        this.start_gamepad(window, cx);
        this.start_global(window, cx);
        this.start_downloads(window, cx);
        if demo {
            this.start_demo_script(window, cx);
            this.start_options_demo(window, cx);
            this.start_tweaks_demo(window, cx);
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
                            this.start_perf_events(cx);
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

    /// Connected: the library opens whether or not a Ubisoft account is signed in (other
    /// stores' games show either way; the library offers Ubisoft's sign-in).
    fn on_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = Some(status);
        self.conn = Conn::Ready;
        self.load_library(false, cx);
        self.global_status(cx);
        self.downloads_status(cx);
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

    /// [`Self::call`], failing with "no answer" after `limit` (the request may still run).
    pub fn call_within<T, F>(&self, req: Request, limit: std::time::Duration, cx: &mut Context<Self>, done: F)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    {
        let Some(source) = self.source.clone() else { return };
        let rx = on_tokio(async move {
            match tokio::time::timeout(limit, source.call::<T>(req)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("quiverd didn't answer in {} s (Ubisoft may be slow); try again later", limit.as_secs())),
            }
        });
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
        self.downloads_event(&e, cx);
        self.global_event(&e, cx);
        self.perf_on_event(&e, cx);
        match e {
            Event::Job(job) => self.apply_job(job),
            Event::GameStarted { product_id } | Event::GameStopped { product_id } => {
                let running = matches!(e, Event::GameStarted { .. });
                if let Some(g) = self.game_mut(Gid::from(product_id)) {
                    g.entry.running = running;
                }
                if !running {
                    self.load_details(Gid::from(product_id), cx);
                }
                self.options_game_state(Gid::from(product_id), running, cx);
            }
            // Signed in (here, from the CLI, or another account): its games join.
            Event::SessionChanged { signed_in: Some(name) } => {
                if let Some(s) = &mut self.status {
                    s.signed_in = Some(name);
                }
                self.conn = Conn::Ready;
                self.load_library(false, cx);
            }
            // Signed out of Ubisoft: its games leave the library, the rest stays. A sign-in
            // form the person has open stays open.
            Event::SessionChanged { signed_in: None } => {
                if let Some(s) = &mut self.status {
                    s.signed_in = None;
                }
                self.load_library(false, cx);
            }
            // Steam games change Steam copies in the library (installed, switched to Quiver).
            Event::LibraryChanged => self.load_library(false, cx),
            Event::SteamGamesChanged { .. } => {
                self.load_library(false, cx);
                // A Steam game's page shows its hook (`quiver steam hook` may have changed it).
                if let Page::Game(id) = self.nav.page
                    && id.0.as_steam().is_some()
                {
                    self.load_store_detail(id, true, cx);
                }
            }
            Event::SettingsChanged { product_id, .. } => self.load_details(Gid::from(product_id), cx),
            // Another store's game (a Steam game): its mods, profiles, `steam_profile` or
            // options changed; the open page reloads them.
            Event::ModsChanged { game: Some(g), .. } => {
                let id = Gid(g);
                if self.nav.page == Page::Game(id) && self.gp.id == id {
                    self.load_mods(id, cx);
                    self.options_mods_changed(id, cx);
                }
            }
            Event::ModsChanged { product_id, game: None } => self.load_page(Gid::from(product_id), cx),
            // The open page's achievements show it (Sight will toast it).
            // Another store's game (Epic): its page reloads its achievements.
            Event::AchievementUnlocked { game: Some(g), .. } => {
                let id = Gid(g);
                if self.nav.page == Page::Game(id) && self.gp.id == id {
                    self.load_store_detail(id, true, cx);
                }
            }
            Event::AchievementUnlocked { product_id, .. } => {
                if self.nav.page == Page::Game(Gid::from(product_id)) && self.gp.id == product_id {
                    self.load_progress(Gid::from(product_id), false, cx);
                }
            }
            // A Steam copy shows as running while Steam plays it (quiverd's library says so).
            Event::SessionsChanged { sessions } => {
                let steam_now = sessions.iter().any(|s| self.game(Gid::from(s.product_id)).is_some_and(|g| g.steam_only()));
                let steam_before = self.games.iter().any(|g| g.steam_only() && g.entry.running);
                if steam_now || steam_before {
                    self.load_library(false, cx);
                }
            }
            Event::Disconnected => {
                self.link = Link::Reconnecting;
                self.link_gen += 1;
            }
            Event::Reconnected => self.on_reconnected(cx),
            Event::DaemonRestarting { to, .. } => self.daemon_update = Some(to.unwrap_or_else(|| "a new build".into())),
            // Mod options (the open game's Mods tab follows them).
            Event::OptionsChanged { .. } | Event::OptionsLive { .. } | Event::OptionsCrash { .. } | Event::OptionsOutcome { .. } | Event::ModChannel { .. } => {
                self.options_event(&e, cx)
            }
            // Quiver Tweaks (Settings → Tweaks and the Tweaks tab follow them).
            Event::TweaksChanged { .. } | Event::TweaksEffect { .. } => self.tweaks_event(&e, cx),
            // Events this app doesn't show (perf updates, HUD models) and ones added later.
            _ => return,
        }
        cx.notify();
    }

    /// The daemon is back (likely restarted): events in between were missed and its jobs
    /// may be gone, so reload status, jobs and the library.
    fn on_reconnected(&mut self, cx: &mut Context<Self>) {
        self.link = Link::Reconnected;
        self.link_gen += 1;
        if let Some(build) = self.daemon_update.take() {
            self.show_toast(format!("quiverd updated to {build}"), cx);
        }
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
                // Keep a half-filled sign-in form.
                (false, Conn::Login(_)) => {
                    this.status = Some(status);
                    this.load_library(false, cx);
                }
                _ => {
                    this.status = Some(status);
                    this.conn = Conn::Ready;
                    this.load_library(false, cx);
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
        if let Some(g) = self.game_mut(Gid(job.game)) {
            // Keep an unfinished install visible over a finished launch, and vice versa.
            let keep_old = g.job.as_ref().is_some_and(|old| old.id != job.id && !old.done && job.done);
            if !keep_old {
                g.job = Some(job);
            }
        }
    }

    pub fn load_library(&mut self, refresh: bool, cx: &mut Context<Self>) {
        self.call::<Vec<LibraryEntry>, _>(Request::Library { refresh, all_stores: true }, cx, |this, result, cx| match result {
            Ok(entries) => {
                let mut old: HashMap<Gid, GameRow> = this.games.drain(..).map(|g| (g.id(), g)).collect();
                this.games = entries
                    .into_iter()
                    .map(|mut entry| {
                        entry.name = clean_name(&entry.name);
                        match old.remove(&Gid(entry.game)) {
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
                // Steam copies too: their sessions are recorded when Steam starts them through Quiver.
                let ids: Vec<Gid> = this.games.iter().filter(|g| g.on_disk()).map(|g| g.id()).collect();
                for id in ids {
                    this.load_playtime(id, cx);
                }
                // Games with a Steam copy: their `art` setting picks whose art shows.
                let steam: Vec<Gid> = this.games.iter().filter(|g| g.steam().is_some() && g.settings.is_none()).map(|g| g.id()).collect();
                for id in steam {
                    this.load_settings(id, cx);
                }
                if let Page::Game(id) = this.nav.page {
                    if this.game(id).is_none() && id.is_ubisoft() && !this.ubisoft_signed_in() {
                        // Signed out of Ubisoft: its games left the library.
                        this.nav.open_section(Section::Library);
                    } else {
                        this.load_details(id, cx);
                    }
                }
                // A store filter whose store left (signed out of Ubisoft) would hide everything,
                // with no chip left to clear it.
                if let Some(s) = this.store_filter
                    && !this.games.iter().any(|g| g.entry.stores().contains(&s))
                {
                    this.store_filter = None;
                }
                this.library_loaded = true;
            }
            // Signed out of Ubisoft with no other store's games, quiverd answers with
            // Ubisoft's "not signed in": that's an empty library, not a failure.
            Err(_) if !this.ubisoft_signed_in() => {
                this.games.clear();
                this.library_loaded = true;
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

    fn load_playtime(&mut self, id: Gid, cx: &mut Context<Self>) {
        self.call::<Playtime, _>(Request::Playtime { game: id.0.into() }, cx, move |this, r, cx| {
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
    pub fn load_details(&mut self, id: Gid, cx: &mut Context<Self>) {
        self.load_playtime(id, cx);
        self.load_settings(id, cx);
        self.load_page(id, cx);
    }

    fn load_settings(&mut self, id: Gid, cx: &mut Context<Self>) {
        // A Steam copy's settings apply when Steam starts it through Quiver; other stores'
        // games have no Quiver settings (their client launches them).
        if !id.is_ubisoft() || !self.game(id).is_some_and(|g| g.installed() || g.steam_only()) {
            return;
        }
        // The daemon replies `{revision, settings}`.
        self.call::<serde_json::Value, _>(Request::SettingsGet { game: id.pid().into() }, cx, move |this, r, _| {
            if let (Ok(v), Some(g)) = (r, this.game_mut(id)) {
                g.settings = Some(crate::ui::game::settings_from_reply(v).0);
            }
        });
        self.call::<Vec<Revision>, _>(Request::SettingsHistory { game: id.pid().into() }, cx, move |this, r, _| {
            if let (Ok(h), Some(g)) = (r, this.game_mut(id)) {
                g.history = Some(h);
            }
        });
    }

    /// A Ubisoft account is signed in (the default one, as the daemon reports it).
    pub fn ubisoft_signed_in(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.signed_in.is_some())
    }

    pub fn game(&self, id: Gid) -> Option<&GameRow> {
        self.games.iter().find(|g| g.id() == id)
    }

    fn game_mut(&mut self, id: Gid) -> Option<&mut GameRow> {
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
                    if !this.ui.login_adding {
                        this.show_toast(format!("Signed in to Ubisoft as {name}. Its games are joining the library."), cx);
                    }
                    if let Some(s) = &mut this.status {
                        s.signed_in.get_or_insert(name);
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

    /// The game's `art` setting (Ubisoft's art first unless it says `steam`).
    pub fn art_pref(&self, g: &GameRow) -> crate::art::ArtPreference {
        crate::art::ArtPreference::from_setting(g.settings.as_ref().and_then(|s| s.art.as_deref()))
    }

    /// The image for a game's slot: the first of its candidates (`art::candidates`: the
    /// user's file, Ubisoft's config, Ubisoft's games API, Steam's cache, Steam's CDN) that
    /// hasn't failed, and which one that is.
    pub fn art_pick(
        &self,
        g: &GameRow,
        slot: crate::art::ArtSlot,
        variant: Variant,
        cx: &mut Context<Self>,
    ) -> (ArtState, Option<crate::art::Candidate>) {
        let cands = crate::art::candidates(&g.entry, g.id().ubisoft().and_then(|p| self.custom_art.get(&p)), slot, self.art_pref(g));
        let (state, c) = crate::art::pick(&cands, |src| self.art.update(cx, |a, cx| a.get(src, variant, cx)));
        (state, c.cloned())
    }

    pub fn art_for(&self, g: &GameRow, slot: crate::art::ArtSlot, variant: Variant, cx: &mut Context<Self>) -> ArtState {
        self.art_pick(g, slot, variant, cx).0
    }

    /// Steam's art for a Steam install that isn't in the library.
    pub fn art_for_steam_game(&self, g: &quiver_api::exports::SteamGame, slot: crate::art::ArtSlot, variant: Variant, cx: &mut Context<Self>) -> ArtState {
        let entry = LibraryEntry {
            game: quiver_api::GameRef::steam(g.app_id),
            name: g.name.clone(),
            installed: None,
            update_available: false,
            running: false,
            art: Default::default(),
            owned_by: Vec::new(),
            installable: false,
            steam: Some(g.clone()),
            space_id: None,
            client: None,
            records: Vec::new(),
        };
        let cands = crate::art::candidates(&entry, None, slot, crate::art::ArtPreference::Steam);
        crate::art::pick(&cands, |src| self.art.update(cx, |a, cx| a.get(src, variant, cx))).0
    }

    // -----------------------------------------------------------------------------------------
    // Library order and layout

    /// The library in display order: installed and active games by last played, then the
    /// rest by name.
    pub fn ordered(&self) -> Vec<&GameRow> {
        let mut v: Vec<&GameRow> = self.games.iter().collect();
        v.sort_by(|a, b| {
            let group = |g: &GameRow| if g.on_disk() || g.install_job().is_some() { 0 } else { 1 };
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
            .filter(|g| self.side_filter == SideFilter::All || g.on_disk() || g.install_job().is_some())
            .collect()
    }

    /// The game the library hero features: the most recently played installed game.
    pub fn hero_game(&self) -> Option<&GameRow> {
        self.games.iter().filter(|g| g.on_disk() && g.last_played().is_some()).max_by_key(|g| g.last_played())
    }

    /// Things waiting on the user, for the hero's "Needs attention" panel.
    pub fn attention(&self) -> Vec<(Gid, Attention)> {
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

    /// Grid columns and tile width for the current window.
    pub fn grid_metrics(&self) -> (usize, f32) {
        let avail = (self.viewport.0 - 2. * self.gutter()).max(160.);
        let cols = (((avail + Self::GRID_GAP) / (150. + Self::GRID_GAP)).floor() as usize).clamp(2, 12);
        let w = (avail - Self::GRID_GAP * (cols as f32 - 1.)) / cols as f32;
        (cols, w)
    }

    pub fn compute_layout(&self, cx: &App) -> Layout {
        let grid: Vec<Gid> = self.grid_games().iter().map(|g| g.id()).collect();
        let side: Vec<Gid> = self.side_games().iter().map(|g| g.id()).collect();
        let hero_buttons = if self.hero_game().is_some() { 2 + self.attention().len() } else { 0 };
        let body = match self.nav.page {
            Page::Downloads => self.downloads_body_len(),
            Page::Activity => self.activity_rows().len(),
            Page::Settings => self.settings_actions().len(),
            Page::Game(id) => self.page_body_len(id, self.nav.tab, cx),
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

    // -----------------------------------------------------------------------------------------
    // Commands

    pub fn palette_commands(&self) -> Vec<Command> {
        self.global_palette_commands()
    }

    fn primary_command(&self, g: &GameRow) -> Command {
        let (icon, label, cli) = if g.running() {
            (crate::ui::Icon::Square, "Stop", format!("quiver stop {}", g.id()))
        } else if g.steam_only() {
            (crate::ui::Icon::Play, "Play", format!("quiver play {}", g.id()))
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

    pub fn menu_commands(&self, id: Gid) -> Vec<Command> {
        if let Some(v) = self.play_menu_commands(id) {
            return v;
        }
        let Some(g) = self.game(id) else { return vec![] };
        let mut v = vec![self.primary_command(g)];
        // Another store's game: its client installs and runs it; the page and the command.
        if g.client().is_some() {
            v.push(Command {
                icon: crate::ui::Icon::Info,
                label: "Game details".into(),
                detail: None,
                cli: Some(format!("quiver info {id}").into()),
                action: Cmd::Open(id),
                future: false,
            });
            v.push(Command {
                icon: crate::ui::Icon::Copy,
                label: "Copy play command".into(),
                detail: Some(format!("{} starts it", g.entry.store().label()).into()),
                cli: Some(format!("quiver play {id}").into()),
                action: Cmd::CopyCli(id),
                future: false,
            });
            return v;
        }
        v.push(Command {
            icon: crate::ui::Icon::Info,
            label: "Game details".into(),
            detail: None,
            cli: Some(format!("quiver show {id}").into()),
            action: Cmd::Open(id),
            future: false,
        });
        // A Steam copy: Steam installs it, so no shortcuts or install actions; the switch
        // between Quiver and Ubisoft Connect instead.
        if let Some(s) = g.steam().filter(|_| g.steam_only()) {
            let s = self.steam_copy(id.pid()).unwrap_or_else(|| s.clone());
            let app_id = s.app_id;
            match crate::ui::steamgames::control(&s) {
                Some(true) => v.push(Command {
                    icon: crate::ui::Icon::Gamepad,
                    label: "Start it from Steam through Quiver".into(),
                    detail: Some("Instead of Ubisoft Connect; undo any time".into()),
                    cli: Some(format!("quiver steam patch {app_id}").into()),
                    action: Cmd::Global(crate::ui::global::GlobalCmd::SteamPatch(app_id)),
                    future: false,
                }),
                Some(false) => v.push(Command {
                    icon: crate::ui::Icon::Undo,
                    label: "Start it from Steam through Ubisoft Connect again".into(),
                    detail: None,
                    cli: Some(format!("quiver steam unpatch {app_id}").into()),
                    action: Cmd::Global(crate::ui::global::GlobalCmd::SteamUnpatch(app_id)),
                    future: false,
                }),
                None => {}
            }
            v.push(Command {
                icon: crate::ui::Icon::SlidersHorizontal,
                label: "Launch options…".into(),
                detail: Some("Applied when Steam starts it through Quiver".into()),
                cli: Some(format!("quiver history {id}").into()),
                action: Cmd::Tab(id, Tab::LaunchOptions),
                future: false,
            });
            return v;
        }
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
            Cmd::DefaultProfile(id, i) => self.set_default_profile(id, i, cx),
            Cmd::DefaultMenu(id) => {
                (self.gp.play_menu, self.gp.default_menu) = (true, true);
                self.nav.open_menu(id);
            }
            Cmd::Export(id, a) => self.run_export(id, a, cx),
            Cmd::Settings(s) => self.nav.open_settings(s),
            Cmd::Global(g) => self.run_global(g, cx),
            Cmd::Downloads(d) => self.run_dl(d, cx),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Actions

    pub fn open_game(&mut self, id: Gid, cx: &mut Context<Self>) {
        self.nav.open_game(id);
        self.load_details(id, cx);
    }

    /// Play, or whatever stands in for it: Stop while running, Install / Update otherwise.
    pub fn primary(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some(g) = self.game(id) else { return };
        let name = g.entry.name.clone();
        if g.running() && g.steam_only() {
            self.show_toast(format!("{name} is running from Steam; quit it from the game or Steam"), cx);
        } else if g.running() {
            self.call::<(), _>(Request::Stop { game: id.0.into() }, cx, |this, r, cx| {
                if let Err(e) = r {
                    this.show_toast(err_text(&e), cx);
                }
            });
        } else if let Some(f) = g.installing() {
            self.show_toast(format!("{name} is installing ({:.0}%)", (f * 100.).floor()), cx);
        } else if g.queued() {
            self.show_toast(format!("{name} is queued"), cx);
        } else if let Some(label) = g.client().map(|_| g.entry.store().label()) {
            // Another store's game: its client starts it.
            self.call::<u64, _>(Request::Play { game: id.0.into(), profile: None }, cx, move |this, r, cx| match r {
                Ok(_) => this.show_toast(format!("Asked {label} to start {name}"), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            });
        } else if g.steam_only() {
            // Steam starts it (through Quiver, once it's switched).
            if self.ui.sessions.iter().any(|s| s.product_id == id) {
                return self.show_toast(format!("{name} is running; quit it from the game"), cx);
            }
            let via_quiver = g.steam().is_some_and(|s| s.state.uses_quiver());
            // Steam starts it through Quiver with the saved launch options.
            if self.launch_guard(id, PlayAfter::Primary, cx) {
                return;
            }
            self.call::<u64, _>(Request::Play { game: id.0.into(), profile: None }, cx, move |this, r, cx| match r {
                Ok(_) if via_quiver => this.show_toast(format!("Asked Steam to start {name}"), cx),
                Ok(_) => this.show_toast(format!("Asked Steam to start {name}; it opens Ubisoft Connect first"), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            });
        } else if !id.is_ubisoft() && !g.entry.on_disk() && !g.entry.installable {
            // Another store's game that isn't installed: its provider says why it can't
            // play yet (an Epic game before Epic installs; a Ubisoft game bought on Epic).
            self.call::<u64, _>(Request::Play { game: id.0.into(), profile: None }, cx, |this, r, cx| {
                if let Err(e) = r {
                    this.show_toast(err_text(&e), cx);
                }
            });
        } else if !g.installed() {
            self.open_install_dialog(id, cx);
        } else if g.entry.update_available {
            self.open_update_dialog(id, cx);
        } else {
            if self.launch_guard(id, PlayAfter::Primary, cx) {
                return;
            }
            // No profile named: the daemon runs the game's default.
            let with = self.default_profile(id).and_then(|p| self.gp.profiles().into_iter().find(|x| x.id == p)).map(|p| format!(" · {}", p.name));
            self.call::<u64, _>(Request::Play { game: id.0.into(), profile: None }, cx, move |this, r, cx| match r {
                Ok(_) => this.show_toast(format!("Starting {name}{}…", with.unwrap_or_default()), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            });
        }
    }

    /// A game its store's client updates (Steam): ask quiverd to open the client's
    /// downloads, where the update is (Quiver never writes the client's files).
    pub fn update_in_client(&mut self, id: Gid, cx: &mut Context<Self>) {
        let label = id.0.store.label();
        self.call::<u64, _>(Request::Update { game: id.0.into() }, cx, move |this, r, cx| match r {
            Ok(_) => this.show_toast(format!("Opened {label}'s downloads: {label} updates it"), cx),
            Err(e) => this.show_toast(err_text(&e), cx),
        });
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
                    // The profile chip sets `default_menu` before this; the arrow clears it.
                    self.gp.play_menu = i == ACTION_OPTIONS;
                    self.gp.default_menu &= self.gp.play_menu;
                    self.nav.open_menu(id);
                }
                // Shortcuts, files and launch options are for Quiver's own launches.
                ACTION_STEAM | ACTION_FILES | ACTION_LAUNCH if !id.is_ubisoft() => {}
                ACTION_STEAM => self.steam_button(id, cx),
                ACTION_FILES => self.nav.select_tab(Tab::Files),
                ACTION_LAUNCH => self.nav.select_tab(Tab::LaunchOptions),
                _ => {}
            },
            Effect::Body(i) => match self.nav.page {
                Page::Downloads => self.downloads_activate(i, cx),
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
        if self.downloads_input(input, cx) || self.global_input(input, cx) || self.page_input(input, cx) {
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
            (Page::Downloads, Zone::Body) => self.downloads_scroll(),
            (Page::Activity | Page::Settings, _) => self.scroll_global_focus(),
            _ => {}
        }
    }

    /// Index of a game's row among the side list's children (group headers included).
    pub fn side_child_index(&self, id: Gid) -> Option<usize> {
        let games = self.side_games();
        let pos = games.iter().position(|g| g.id() == id)?;
        let first_rest = games.iter().position(|g| !(g.on_disk() || g.install_job().is_some()));
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
        let y = (f32::from(o.y) - dy).clamp(-f32::from(max.y), 0.);
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
                cx.update(|cx| {
                    cx.set_global(t);
                    cx.refresh_windows();
                });
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
                        // The app asks for no chord; gilrs not starting is a log line already.
                        PadEvent::Chord | PadEvent::Unavailable(_) => {}
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
                    // The right stick: scroll the page down 400 px.
                    "scroll" => {
                        let _ = this.update_in(cx, |this, _, cx| {
                            this.scroll_by(400.);
                            cx.notify();
                        });
                        cx.background_executor().timer(Duration::from_millis(250)).await;
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
        self.layout = self.compute_layout(cx);

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
                .when_some(self.render_dl_overlay(&th, window, cx), |d, o| d.child(o))
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
        let offer = if ready { self.render_steam_offer(&th, update.is_some(), cx) } else { None };
        screen
            .when_some(offer, |d, n| d.child(n))
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
