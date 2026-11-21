//! App-wide state and behaviour that belongs to no single screen: accounts and sessions
//! (now playing), the daemon link (reconnecting), the confirmation dialog, focus for the
//! palette and sign-in fields, live refreshes, and the demo-mode setup for these screens.
//!
//! `app.rs` calls in through a handful of hooks (`start_global`, `global_event`,
//! `global_status`, `global_input`, ...), so the screens in `settings.rs`,
//! `palette.rs`, `activity.rs` and `signin.rs` stay out of the shared files.

use quiver_ui::depth::DepthExt;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, EntityId, Focusable, IntoElement, KeyBinding, ParentElement, SharedString, Stateful,
    Styled, Window, actions, div, prelude::*, px,
};
use quiver_api::exports::{ExportKind, ExportStatus, SteamUsers};
use quiver_api::{ActiveSession, ActivityEntry, Event, Request, Status};

use super::{BOLD, FocusExt, Icon, Kind, PillKind, SEMIBOLD, Size, art_fill, button, icon, pill, soft_shadow};
use crate::app::{Conn, InputMode, NAV_CONTEXT, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::model::{Gid, Actor, actor, clean_name, clock, short_name};
use crate::nav::{Input, Overlay, Page, Section, SettingsSection};
use crate::text_input::TextInput;
use crate::ui::game::PlayAfter;
use crate::theme::{Theme, rgba};
use crate::err_text;

actions!(
    quiver_global,
    [
        PaletteUp,
        PaletteDown,
        PaletteRun,
        PaletteClose,
        PaletteToggle,
        OpenSettings,
        SubmitSignin,
        CancelSignin,
        NextField,
    ]
);

/// Key context while the command palette's search field has focus: letters go to the
/// field, arrows and Enter to the list.
pub const PALETTE_CONTEXT: &str = "QuiverPalette";
/// Key context of the sign-in screen (set by `app.rs`).
pub const LOGIN_CONTEXT: &str = "QuiverLogin";

pub fn bind_keys(cx: &mut App) {
    let p = Some(PALETTE_CONTEXT);
    let l = Some(LOGIN_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", PaletteUp, p),
        KeyBinding::new("down", PaletteDown, p),
        KeyBinding::new("ctrl-p", PaletteUp, p),
        KeyBinding::new("ctrl-n", PaletteDown, p),
        KeyBinding::new("enter", PaletteRun, p),
        KeyBinding::new("escape", PaletteClose, p),
        KeyBinding::new("ctrl-k", PaletteToggle, p),
        KeyBinding::new("ctrl-,", OpenSettings, Some(NAV_CONTEXT)),
        KeyBinding::new("enter", SubmitSignin, l),
        KeyBinding::new("escape", CancelSignin, l),
        KeyBinding::new("tab", NextField, l),
    ]);
}

/// Who an Activity entry came from, for the source filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    App,
    Cli,
    Agent,
    System,
}

impl SourceFilter {
    pub const ALL: [SourceFilter; 5] =
        [SourceFilter::All, SourceFilter::App, SourceFilter::Cli, SourceFilter::Agent, SourceFilter::System];

    pub fn label(self) -> &'static str {
        match self {
            SourceFilter::All => "Everyone",
            SourceFilter::App => "App",
            SourceFilter::Cli => "CLI",
            SourceFilter::Agent => "Agents",
            SourceFilter::System => "System",
        }
    }

    pub fn icon(self) -> Option<Icon> {
        match self {
            SourceFilter::All => None,
            SourceFilter::App => Some(Icon::MousePointer),
            SourceFilter::Cli => Some(Icon::SquareTerminal),
            SourceFilter::Agent => Some(Icon::Bot),
            SourceFilter::System => Some(Icon::Cog),
        }
    }

    pub fn matches(self, source: &str) -> bool {
        match self {
            SourceFilter::All => true,
            SourceFilter::App => actor(source) == Actor::App,
            SourceFilter::Cli => actor(source) == Actor::Cli,
            SourceFilter::Agent => actor(source) == Actor::Agent,
            SourceFilter::System => actor(source) == Actor::System,
        }
    }

    pub fn step(self, delta: isize) -> SourceFilter {
        let n = Self::ALL.len() as isize;
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0) as isize;
        Self::ALL[((i + delta).rem_euclid(n)) as usize]
    }
}

/// Commands from the palette and the global screens. `Copy`, so they fit in
/// [`crate::app::Cmd`]; accounts and exports are indexes into the current lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalCmd {
    /// Open the sign-in screen to add another account.
    AddAccount,
    /// Ask, then sign out `status.accounts[i]`.
    SignOut(usize),
    /// Make `status.accounts[i]` the default account.
    MakeDefault(usize),
    /// Ask, then remove `exports[i]`.
    RemoveExport(usize),
    /// Write staged Steam shortcut changes (only offered while Steam is closed).
    SteamApply,
    RescanProtons,
    Stop(Gid),
    /// Open Activity with this source filter.
    ActivityFrom(SourceFilter),
    /// The now-playing card.
    NowPlaying,
    /// Open Activity for one game.
    ActivityGame(Gid),
    /// Add a game to Steam (staged while Steam runs) or to the app launcher.
    ExportSteam(Gid),
    ExportDesktop(Gid),
    /// A Steam copy of a Ubisoft game (by Steam app id): start it through Quiver, or
    /// through Ubisoft Connect again.
    SteamPatch(u32),
    SteamUnpatch(u32),
    /// "Not now" on the offer for a newly installed Steam game.
    SteamDismiss(u32),
    /// Offer the switch for Ubisoft games newly installed in Steam: on/off.
    SteamOffers,
    /// Ask, then restart Steam so it picks up staged changes.
    SteamRestart,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    SignOut { user_id: String, name: String },
    RemoveExport { kind: ExportKind, game: Gid },
    /// Close Steam, write staged changes, start it again (`quiver steam restart`).
    RestartSteam,
    /// Start a game with unsaved launch-options edits: save them first (the action) or
    /// play without them (the alternative), keeping them staged.
    PlayUnsaved { game: Gid, play: PlayAfter },
}

impl ConfirmAction {
    /// Starts something rather than removing it: an accent button, not a red one.
    fn constructive(&self) -> bool {
        matches!(self, ConfirmAction::PlayUnsaved { .. })
    }
}

/// A question drawn over everything (sign out, remove a shortcut, play with unsaved edits):
/// Cancel, an optional alternative, and the action.
#[derive(Clone, Debug)]
pub struct Confirm {
    pub title: SharedString,
    pub body: SharedString,
    pub confirm: SharedString,
    /// A second way through, between Cancel and the action.
    pub alt: Option<SharedString>,
    pub cli: Option<SharedString>,
    pub action: ConfirmAction,
    /// The focused button, left to right: 0 Cancel, then the alternative (if any), then
    /// the action. Destructive questions start on Cancel.
    pub selected: usize,
}

/// A button of the confirmation dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmPick {
    Cancel,
    Alt,
    Action,
}

impl Confirm {
    pub fn buttons(&self) -> usize {
        if self.alt.is_some() { 3 } else { 2 }
    }

    /// The button at position `i` (left to right).
    pub fn pick(&self, i: usize) -> ConfirmPick {
        match i {
            0 => ConfirmPick::Cancel,
            i if i + 1 >= self.buttons() => ConfirmPick::Action,
            _ => ConfirmPick::Alt,
        }
    }
}

#[derive(Default)]
pub struct GlobalUi {
    pub sessions: Vec<ActiveSession>,
    pub protons: Option<Result<Vec<String>, String>>,
    pub exports: Option<Result<Vec<ExportStatus>, String>>,
    pub steam: Option<Result<SteamUsers, String>>,
    /// Ubisoft games installed through Steam (`steamgames.rs`).
    pub sg: super::steamgames::SteamGamesUi,
    /// When the daemon connection dropped (`Event::Disconnected`), for the banner.
    pub down_since: Option<Instant>,
    pub confirm: Option<Confirm>,
    pub now_playing_open: bool,
    pub palette_input: Option<Entity<TextInput>>,
    pub palette_query: String,
    pub activity_source: Option<SourceFilter>,
    pub activity_game: Option<Gid>,
    pub activity_games_open: bool,
    pub activity_checked: Option<Instant>,
    activity_loading: bool,
    pub login_adding: bool,
    pub login_email: Option<String>,
    login_focused: Option<EntityId>,
    /// Settings › Library and storage › Advanced is open.
    pub lib_advanced: bool,
    /// hyprpad seen running (checked when Appearance is shown).
    pub hyprpad: Option<(bool, Instant)>,
    /// Settings › Accounts › Epic Games.
    pub epic: super::settings::epic::EpicUi,
    /// Accounts across machines (Settings › Accounts, the game page's launch account).
    pub presence: super::settings::presence::PresenceUi,
}

impl GlobalUi {
    pub fn source_filter(&self) -> SourceFilter {
        self.activity_source.unwrap_or(SourceFilter::All)
    }
}

/// Flags for demo-mode screenshots: `QUIVER_DEMO_GLOBAL="settings=accounts,playing"`.
/// See `demo_global.rs` for the data side.
pub fn demo_flags() -> Vec<String> {
    std::env::var("QUIVER_DEMO_GLOBAL")
        .unwrap_or_default()
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn demo_flag(name: &str) -> Option<String> {
    demo_flags().into_iter().find_map(|f| match f.split_once('=') {
        Some((k, v)) if k == name => Some(v.to_owned()),
        None if f == name => Some(String::new()),
        _ => None,
    })
}

/// "1:12:08" / "4:07".
pub fn elapsed(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

impl Quiver {
    // -----------------------------------------------------------------------------------------
    // Hooks called from app.rs

    /// Once, from `Quiver::new`: key bindings, the 1 s tick (now-playing clock, Activity
    /// polling) and the demo-mode screen setup.
    pub fn start_global(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        bind_keys(cx);
        cx.spawn(async move |this, cx| {
            let mut n: u64 = 0;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                n += 1;
                let alive = this.update(cx, |this, cx| this.global_tick(n, cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        if self.demo {
            self.start_global_demo(window, cx);
        }
    }

    fn global_tick(&mut self, n: u64, cx: &mut Context<Self>) {
        if !matches!(self.conn, Conn::Ready) {
            return;
        }
        if !self.ui.sessions.is_empty() || self.ui.down_since.is_some() {
            cx.notify(); // clocks
        }
        if self.nav.page == Page::Activity {
            cx.notify(); // "checked 3 s ago"
            // No ActivityAppended event yet: poll while the page is open.
            if n % 4 == 0 {
                self.reload_activity(cx);
            }
        }
        if self.nav.page == Page::Settings && self.nav.settings == SettingsSection::Steam && n % 6 == 0 {
            self.load_global_exports(cx);
        }
    }

    /// After every `Status` the app gets on (re)connecting.
    pub fn global_status(&mut self, cx: &mut Context<Self>) {
        if matches!(self.conn, Conn::Ready) {
            self.ui.login_adding = false;
        }
        if let Some(s) = &self.status {
            // A sign-in started elsewhere (the CLI) is waiting for its code.
            if s.awaiting_2fa {
                if let Conn::Login(form) = &mut self.conn {
                    form.needs_code = true;
                }
            }
        }
        if matches!(self.conn, Conn::Ready) {
            self.load_sessions(cx);
            self.load_protons(cx);
            self.load_global_exports(cx);
            self.reload_activity(cx);
            self.load_epic_accounts(cx);
        }
    }

    /// Every daemon event, before `app.rs` handles it.
    pub fn global_event(&mut self, e: &Event, cx: &mut Context<Self>) {
        match e {
            Event::SessionChanged { signed_in } => {
                self.refresh_status(cx);
                if signed_in.is_some() && self.ui.login_adding {
                    self.ui.login_adding = false;
                    self.nav.open_settings(SettingsSection::Accounts);
                    let added = self.ui.login_email.take().unwrap_or_default();
                    self.show_toast(format!("Signed in {added}"), cx);
                } else if let (Some(name), Conn::Login(_)) = (signed_in, &self.conn) {
                    // The first account, from the library's card (or Settings): back to where
                    // the person was, with its games joining the library.
                    self.ui.login_email = None;
                    self.show_toast(format!("Signed in to Ubisoft as {name}. Its games are joining the library."), cx);
                }
            }
            Event::SessionsChanged { sessions } => {
                self.ui.sessions = sessions.clone();
                if self.ui.sessions.is_empty() {
                    self.ui.now_playing_open = false;
                }
            }
            Event::GameStarted { .. } | Event::GameStopped { .. } => {
                self.load_sessions(cx);
                if self.ui.presence.presence.is_some() {
                    self.load_presence(cx);
                }
            }
            Event::StoreAccountsChanged { .. } => {
                self.load_epic_accounts(cx);
                self.load_presence(cx);
            }
            Event::AccountsPresenceChanged => self.load_presence(cx),
            Event::Disconnected => {
                self.ui.down_since = Some(Instant::now());
                self.ui.confirm = None;
                return;
            }
            // `app.rs` reloads status and the library; the global screens reload their own.
            Event::Reconnected => {
                self.ui.down_since = None;
                self.load_sessions(cx);
                self.load_protons(cx);
            }
            Event::Job(j) if !j.done => return,
            _ => {}
        }
        self.steam_games_event(e, cx);
        // Anything that changes state may have logged an Activity entry.
        self.reload_activity(cx);
        if matches!(e, Event::LibraryChanged) {
            self.load_global_exports(cx);
        }
    }

    /// Input while one of the global overlays is up, or on the global pages, before
    /// [`crate::nav::Nav`] sees it. Returns whether it was handled.
    pub fn global_input(&mut self, input: Input, cx: &mut Context<Self>) -> bool {
        if let Some(c) = &mut self.ui.confirm {
            let n = c.buttons();
            match input {
                Input::Left | Input::Up => c.selected = (c.selected.min(n - 1) + n - 1) % n,
                Input::Right | Input::Down => c.selected = (c.selected.min(n - 1) + 1) % n,
                Input::Activate => {
                    let pick = c.pick(c.selected.min(n - 1));
                    self.confirm_pick(pick, cx);
                }
                Input::Back => self.ui.confirm = None,
                _ => {}
            }
            return true;
        }
        if self.ui.activity_games_open && input == Input::Back {
            self.ui.activity_games_open = false;
            return true;
        }
        if self.ui.now_playing_open {
            match input {
                Input::Back => {
                    self.ui.now_playing_open = false;
                    return true;
                }
                Input::Activate => {
                    self.ui.now_playing_open = false;
                    if let Some(s) = self.ui.sessions.iter().find(|s| !is_lab_session(s)).or(self.ui.sessions.first()) {
                        self.open_game(Gid::from(s.game_ref()), cx);
                    }
                    return true;
                }
                _ => self.ui.now_playing_open = false,
            }
        }
        if self.nav.overlay.is_none() && self.nav.page == Page::Activity {
            match input {
                // LT / RT: the source filter.
                Input::PrevTab | Input::NextTab => {
                    let d = if input == Input::NextTab { 1 } else { -1 };
                    self.set_activity_source(self.ui.source_filter().step(d));
                    return true;
                }
                // Y: only this entry's game (again: every game).
                Input::Menu => {
                    let game = self.activity_rows().get(self.nav.body_index()).and_then(|a| a.game.map(Gid));
                    self.ui.activity_game = if self.ui.activity_game.is_some() { None } else { game };
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Run a [`GlobalCmd`] (palette entry or a focused control).
    pub fn run_global(&mut self, cmd: GlobalCmd, cx: &mut Context<Self>) {
        match cmd {
            GlobalCmd::AddAccount => self.start_add_account(cx),
            GlobalCmd::SignOut(i) => {
                let Some(a) = self.status.as_ref().and_then(|s| s.accounts.get(i)).cloned() else { return };
                let playing: Vec<String> = self
                    .ui
                    .sessions
                    .iter()
                    .filter(|s| s.account == a.user_id && !is_lab_session(s))
                    .map(|s| self.game_name(Gid::from(s.game_ref())))
                    .collect();
                let only: usize = self
                    .games
                    .iter()
                    .filter(|g| g.entry.owned_by.len() == 1 && g.entry.owned_by[0] == a.user_id)
                    .count();
                let mut body = String::new();
                if !playing.is_empty() {
                    body.push_str(&format!("{} is running on this account; quiverd will ask you to stop it first. ", playing.join(", ")));
                }
                if only > 0 {
                    body.push_str(&format!(
                        "{only} game{} owned only by {} will leave the library until you sign in again. ",
                        if only == 1 { "" } else { "s" },
                        a.name
                    ));
                }
                if a.default {
                    body.push_str("The next account becomes the default. ");
                }
                body.push_str("Its session ticket is removed from the keyring; installs and saves stay.");
                self.ui.confirm = Some(Confirm {
                    title: format!("Sign out {}?", a.name).into(),
                    body: body.into(),
                    confirm: "Sign out".into(),
                    cli: Some(format!("quiver logout {}", a.name).into()),
                    action: ConfirmAction::SignOut { user_id: a.user_id, name: a.name },
                    alt: None,
                    selected: 0,
                });
            }
            GlobalCmd::MakeDefault(i) => {
                let Some(a) = self.status.as_ref().and_then(|s| s.accounts.get(i)).cloned() else { return };
                self.call::<(), _>(Request::AccountDefault { account: a.user_id.clone() }, cx, move |this, r, cx| match r {
                    Ok(()) => {
                        this.show_toast(format!("{} is now the default account", a.name), cx);
                        this.refresh_status(cx);
                    }
                    Err(e) => this.show_error(&e, cx),
                });
            }
            GlobalCmd::RemoveExport(i) => {
                let Some(Ok(list)) = &self.ui.exports else { return };
                let Some(x) = list.get(i).cloned() else { return };
                let r = &x.record;
                let (what, cli, body) = match r.kind {
                    ExportKind::Desktop => (
                        ".desktop entry",
                        format!("quiver export desktop {} --remove", r.game.key()),
                        format!("Deletes {} from your app launcher.", crate::model::tilde(&r.path)),
                    ),
                    ExportKind::Steam => {
                        let running = matches!(&self.ui.steam, Some(Ok(s)) if s.running);
                        (
                            "Steam shortcut",
                            format!("quiver export steam {} --remove", r.game.key()),
                            if running {
                                "Steam is running, so the removal is staged and shows after Steam restarts. Quiver doesn't restart Steam for you.".to_owned()
                            } else {
                                "Removes the shortcut and its artwork from Steam's shortcuts.vdf.".to_owned()
                            },
                        )
                    }
                };
                self.ui.confirm = Some(Confirm {
                    title: format!("Remove the {what} for {}?", short_name(&r.name)).into(),
                    body: body.into(),
                    confirm: "Remove".into(),
                    cli: Some(cli.into()),
                    action: ConfirmAction::RemoveExport { kind: r.kind, game: Gid(r.game) },
                    alt: None,
                    selected: 0,
                });
            }
            GlobalCmd::SteamApply => {
                self.call::<serde_json::Value, _>(Request::SteamApplyStaged, cx, |this, r, cx| {
                    match r {
                        Ok(v) => {
                            let n = v.get("applied").and_then(|n| n.as_u64()).unwrap_or(0);
                            this.show_toast(format!("Wrote {n} staged shortcut change{}", if n == 1 { "" } else { "s" }), cx);
                        }
                        Err(e) => this.show_error(&e, cx),
                    }
                    this.load_global_exports(cx);
                });
            }
            GlobalCmd::RescanProtons => {
                self.load_protons(cx);
                self.show_toast("Looking for Proton builds…", cx);
            }
            GlobalCmd::Stop(id) => {
                let name = self.game_name(id);
                self.call::<(), _>(Request::Stop { game: id.0.into() }, cx, move |this, r, cx| match r {
                    Ok(()) => this.show_toast(format!("Stopping {name}"), cx),
                    Err(e) => this.show_error(&e, cx),
                });
            }
            GlobalCmd::ActivityFrom(f) => {
                self.nav.open_section(Section::Activity);
                self.set_activity_source(f);
                self.ui.activity_game = None;
            }
            GlobalCmd::NowPlaying => self.ui.now_playing_open = !self.ui.sessions.is_empty(),
            GlobalCmd::ActivityGame(id) => {
                self.nav.open_section(Section::Activity);
                self.set_activity_source(SourceFilter::All);
                self.ui.activity_game = Some(id);
            }
            GlobalCmd::ExportSteam(id) => {
                let name = self.game_name(id);
                let req = Request::ExportSteam { game: id.0.into(), remove: false, steam_user: None };
                self.call::<quiver_api::exports::SteamExportResult, _>(req, cx, move |this, r, cx| {
                    match r {
                        Ok(r) if r.staged => this.show_toast(
                            format!("{name} is staged for Steam: it shows after Steam restarts (quiver steam restart asks first)"),
                            cx,
                        ),
                        Ok(_) => this.show_toast(format!("Added {name} to Steam"), cx),
                        Err(e) => this.show_error(&e, cx),
                    }
                    this.load_global_exports(cx);
                });
            }
            GlobalCmd::SteamPatch(app_id) => self.steam_patch(app_id, false, cx),
            GlobalCmd::SteamUnpatch(app_id) => self.steam_patch(app_id, true, cx),
            GlobalCmd::SteamDismiss(app_id) => self.steam_dismiss(app_id, cx),
            GlobalCmd::SteamOffers => {
                let on = !self.steam_offers_enabled();
                self.set_steam_offers(on, cx);
            }
            GlobalCmd::SteamRestart => self.ask_restart_steam(cx),
            GlobalCmd::ExportDesktop(id) => {
                let name = self.game_name(id);
                let req = Request::ExportDesktop { game: id.0.into(), remove: false };
                self.call::<serde_json::Value, _>(req, cx, move |this, r, cx| {
                    match r {
                        Ok(_) => this.show_toast(format!("Added {name} to the app launcher"), cx),
                        Err(e) => this.show_error(&e, cx),
                    }
                    this.load_global_exports(cx);
                });
            }
        }
        cx.notify();
    }

    pub fn close_confirm(&mut self, run: bool, cx: &mut Context<Self>) {
        self.confirm_pick(if run { ConfirmPick::Action } else { ConfirmPick::Cancel }, cx);
    }

    pub fn confirm_pick(&mut self, pick: ConfirmPick, cx: &mut Context<Self>) {
        let Some(c) = self.ui.confirm.take() else { return };
        if pick == ConfirmPick::Cancel {
            return;
        }
        if let ConfirmAction::PlayUnsaved { game, play } = c.action {
            match pick {
                ConfirmPick::Action => self.launch_save(game, Some(play), cx),
                _ => self.play_after(game, play, cx),
            }
            cx.notify();
            return;
        }
        if pick == ConfirmPick::Alt {
            return;
        }
        match c.action {
            ConfirmAction::PlayUnsaved { .. } => {}
            ConfirmAction::SignOut { user_id, name } => {
                self.call::<(), _>(Request::Logout { account: Some(user_id) }, cx, move |this, r, cx| match r {
                    Ok(()) => {
                        this.show_toast(format!("Signed out {name}"), cx);
                        this.refresh_status(cx);
                    }
                    Err(e) => this.show_error(&e, cx),
                });
            }
            ConfirmAction::RestartSteam => self.restart_steam(cx),
            ConfirmAction::RemoveExport { kind, game } => {
                let req = match kind {
                    ExportKind::Desktop => Request::ExportDesktop { game: game.0.into(), remove: true },
                    ExportKind::Steam => Request::ExportSteam { game: game.0.into(), remove: true, steam_user: None },
                };
                self.call::<serde_json::Value, _>(req, cx, move |this, r, cx| {
                    match r {
                        Ok(v) if v.get("staged").and_then(|s| s.as_bool()) == Some(true) => {
                            this.show_toast("Removal staged: Steam shows it after its next restart", cx)
                        }
                        Ok(_) => this.show_toast("Removed", cx),
                        Err(e) => this.show_error(&e, cx),
                    }
                    this.load_global_exports(cx);
                });
            }
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // Loading

    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.call::<Status, _>(Request::Status, cx, |this, r, cx| {
            if let Ok(s) = r {
                // Keep job progress from events; the daemon's list is as fresh.
                this.status = Some(s);
                this.global_status(cx);
            }
        });
    }

    pub fn load_sessions(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<ActiveSession>, _>(Request::Sessions, cx, |this, r, _| {
            if let Ok(s) = r {
                this.ui.sessions = s;
                if this.ui.sessions.is_empty() {
                    this.ui.now_playing_open = false;
                }
            }
        });
    }

    pub fn load_protons(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<String>, _>(Request::ProtonList, cx, |this, r, _| {
            this.ui.protons = Some(r.map_err(|e| err_text(&e)));
        });
    }

    pub fn load_global_exports(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<ExportStatus>, _>(Request::ExportList, cx, |this, r, _| {
            this.ui.exports = Some(r.map_err(|e| err_text(&e)));
        });
        self.call::<SteamUsers, _>(Request::SteamUsers, cx, |this, r, _| {
            this.ui.steam = Some(r.map_err(|e| err_text(&e)));
        });
        self.load_steam_games(cx);
    }

    /// Activity with a bigger window than the library's first load, at most one request at
    /// a time.
    pub fn reload_activity(&mut self, cx: &mut Context<Self>) {
        if self.ui.activity_loading {
            return;
        }
        self.ui.activity_loading = true;
        self.call::<Vec<ActivityEntry>, _>(Request::Activity { limit: 500 }, cx, |this, r, _| {
            this.ui.activity_loading = false;
            if let Ok(a) = r {
                this.activity = a;
                this.ui.activity_checked = Some(Instant::now());
            }
        });
        if self.source.is_none() {
            self.ui.activity_loading = false;
        }
    }

    pub fn set_activity_source(&mut self, f: SourceFilter) {
        self.ui.activity_source = Some(f);
        self.nav.focus(&self.layout, crate::nav::Target::Body(0));
        self.nav.zone = crate::nav::Zone::Body;
        self.body_scroll.set_offset(gpui::point(px(0.), px(0.)));
    }

    /// A game's short name; for one not in the library (a Ubisoft game while signed out),
    /// the name its download or job carries.
    pub fn game_name(&self, id: Gid) -> String {
        if let Some(g) = self.game(id) {
            return short_name(&g.entry.name);
        }
        let queued = id.ubisoft().and_then(|p| self.dl.queue.iter().find(|e| e.product_id == p)).map(|e| e.name.as_str());
        let job = || self.status.as_ref().and_then(|s| s.jobs.iter().rev().find(|j| Gid(j.game) == id)).map(|j| j.name.as_str());
        match queued.or_else(job).filter(|n| !n.is_empty()) {
            Some(n) => short_name(&clean_name(n)),
            None => format!("Game {id}"),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Sign in again / another account

    /// The sign-in screen: Ubisoft's first account (from the library's sign-in card,
    /// Settings or the palette) or another one. A sign-in started elsewhere that waits for
    /// its code opens at the code.
    pub fn start_add_account(&mut self, cx: &mut Context<Self>) {
        self.nav.close_overlay();
        self.ui.now_playing_open = false;
        self.ui.login_adding = self.ubisoft_signed_in();
        self.ui.login_email = None;
        self.show_login(None, cx);
        if self.status.as_ref().is_some_and(|s| s.awaiting_2fa)
            && let Conn::Login(form) = &mut self.conn
        {
            form.needs_code = true;
        }
        cx.notify();
    }

    /// Leave the sign-in screen without signing in (back to the library, or wherever the
    /// person was).
    pub fn cancel_add_account(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.conn, Conn::Login(_)) {
            return;
        }
        self.ui.login_adding = false;
        self.conn = Conn::Ready;
        cx.notify();
    }

    /// Submit the sign-in form, remembering the email for the 2FA step's "code for …".
    pub fn submit_signin(&mut self, cx: &mut Context<Self>) {
        if let Conn::Login(form) = &self.conn {
            if !form.needs_code {
                let email = form.email.read(cx).text().trim().to_owned();
                if !email.is_empty() {
                    self.ui.login_email = Some(email);
                }
            }
        }
        self.submit_login(cx);
    }

    /// Back from the code step to email and password ("Not you?").
    pub fn restart_signin(&mut self, cx: &mut Context<Self>) {
        self.ui.login_email = None;
        self.show_login(None, cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // Rendering glue

    /// The root key context: the palette's while its field has focus.
    pub fn root_context(&self) -> &'static str {
        if matches!(self.nav.overlay, Some(Overlay::Palette { .. })) && self.ui.palette_input.is_some() {
            PALETTE_CONTEXT
        } else {
            NAV_CONTEXT
        }
    }

    /// Move keyboard focus to wherever text goes now (start of every render): the palette's
    /// field while it is open, back to the app when it closes, the sign-in fields.
    pub fn sync_global_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let palette_open = matches!(self.conn, Conn::Ready) && matches!(self.nav.overlay, Some(Overlay::Palette { .. }));
        match (palette_open, self.ui.palette_input.is_some()) {
            (true, false) => {
                let input = cx.new(|cx| TextInput::new(cx, "Type a command or a game", false).bare());
                cx.observe(&input, |this, input, cx| {
                    let q = input.read(cx).text().to_owned();
                    if q != this.ui.palette_query {
                        this.ui.palette_query = q;
                        if let Some(Overlay::Palette { selected }) = &mut this.nav.overlay {
                            *selected = 0;
                        }
                    }
                    cx.notify();
                })
                .detach();
                if let Some(q) = demo_flag("palette").filter(|q| !q.is_empty() && self.demo) {
                    let q = q.replace('+', " ");
                    input.update(cx, |i, cx| i.set_text(&q, cx));
                    self.ui.palette_query = q;
                }
                let h = input.read(cx).focus_handle(cx);
                window.focus(&h, cx);
                self.ui.palette_input = Some(input);
            }
            (false, true) => {
                self.ui.palette_input = None;
                self.ui.palette_query.clear();
                if matches!(self.conn, Conn::Ready) {
                    window.focus(&self.focus, cx);
                }
            }
            _ => {}
        }
        if let Conn::Login(form) = &self.conn {
            let target = if form.needs_code { &form.code } else { &form.email };
            if self.ui.login_focused != Some(target.entity_id()) {
                self.ui.login_focused = Some(target.entity_id());
                let h = target.read(cx).focus_handle(cx);
                window.focus(&h, cx);
            }
        } else if self.ui.login_focused.take().is_some() && matches!(self.conn, Conn::Ready) {
            window.focus(&self.focus, cx);
        }
    }

    /// Action handlers the global screens need on the app's root element.
    pub fn global_actions(&self, root: Stateful<gpui::Div>, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        root.on_action(cx.listener(|this, _: &PaletteUp, _, cx| this.input(Input::Up, InputMode::Keyboard, cx)))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| this.input(Input::Down, InputMode::Keyboard, cx)))
            .on_action(cx.listener(|this, _: &PaletteRun, _, cx| this.input(Input::Activate, InputMode::Keyboard, cx)))
            .on_action(cx.listener(|this, _: &PaletteClose, _, cx| this.input(Input::Back, InputMode::Keyboard, cx)))
            .on_action(cx.listener(|this, _: &PaletteToggle, _, cx| this.input(Input::Palette, InputMode::Keyboard, cx)))
            .on_action(cx.listener(|this, _: &crate::text_input::Copy, _, cx| this.copy_palette_cli(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.nav.open_settings(this.nav.settings);
                this.mode = InputMode::Keyboard;
                cx.notify();
            }))
    }

    /// Things drawn over the page: the now-playing card and the confirmation dialog.
    pub fn render_global_overlay(&mut self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.ui.confirm.is_some() {
            return Some(self.render_confirm(th, cx));
        }
        if self.ui.now_playing_open && !self.ui.sessions.is_empty() {
            return Some(self.render_now_playing(th, cx));
        }
        None
    }

    fn render_confirm(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(c) = self.ui.confirm.clone() else { return div().into_any_element() };
        let keys = self.mode != InputMode::Pointer;
        let width = self.dialog_width(460.);
        let left = ((self.viewport.0 - width) / 2.).max(16.);
        let cancel = button("confirm-cancel", th, Kind::Secondary, Size::Medium, None, "Cancel")
            .focus_ring(keys && c.selected == 0, th, 8.)
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_confirm(false, cx);
                cx.notify();
            }));
        let alt = c.alt.clone().map(|label| {
            button("confirm-alt", th, Kind::Secondary, Size::Medium, None, label)
                .focus_ring(keys && c.selected == 1, th, 8.)
                .on_click(cx.listener(|this, _, _, cx| this.confirm_pick(ConfirmPick::Alt, cx)))
        });
        let last = c.buttons() - 1;
        let constructive = c.action.constructive();
        let (bad, white) = if constructive { (th.acc, th.acc_ink) } else { (th.bad, rgba(0xffffff, 1.0)) };
        let ok = div()
            .id("confirm-ok")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.))
            .h(px(32.))
            .px(px(13.))
            .rounded(px(8.))
            .bg(bad)
            .text_color(white)
            .font_weight(SEMIBOLD)
            .cursor_pointer()
            .hover(move |s| s.bg(bad.opacity(0.85)))
            .child(icon(
                match c.action {
                    ConfirmAction::SignOut { .. } => Icon::LogOut,
                    ConfirmAction::RestartSteam => Icon::RefreshCw,
                    ConfirmAction::RemoveExport { .. } => Icon::Trash,
                    ConfirmAction::PlayUnsaved { .. } => Icon::Check,
                },
                15.,
                white,
            ))
            .child(c.confirm.clone())
            .focus_ring(keys && c.selected == last, th, 8.)
            .on_click(cx.listener(|this, _, _, cx| this.close_confirm(true, cx)));
        div()
            .id("confirm-scrim")
            .absolute()
            .inset_0()
            .bg(th.scrim)
            .on_click(cx.listener(|this, _, _, cx| {
                this.ui.confirm = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("confirm")
                    .depth_overlay()
                    .absolute()
                    .top(px((self.viewport.1 * 0.28).max(80.)))
                    .left(px(left))
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .p(px(22.))
                    .rounded(px(14.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line2)
                    .shadow(vec![soft_shadow(rgba(0x000000, 0.7), 30., 90.)])
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(34.))
                                    .rounded(px(10.))
                                    .bg(if constructive { th.warn_soft } else { th.bad_soft })
                                    .child(icon(Icon::TriangleAlert, 16., if constructive { th.warn } else { th.bad })),
                            )
                            .child(div().text_size(px(16.)).font_weight(BOLD).child(c.title.clone())),
                    )
                    .child(div().text_color(th.tx2).text_size(px(12.5)).line_height(px(19.)).child(c.body.clone()))
                    .when_some(c.cli.clone(), |d, cli| d.child(super::command_line(th, cli)))
                    .child(div().flex().flex_wrap().justify_end().gap(px(8.)).child(cancel).children(alt).child(ok)),
            )
            .into_any_element()
    }

    fn render_now_playing(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        /// How a session was started, from who holds it.
        fn launched_how(launcher: bool, steam: bool, bridge: bool) -> &'static str {
            match (launcher, steam, bridge) {
                (true, _, true) => "Started by Quiver · bridge connected",
                (true, _, false) => "Started by Quiver",
                (false, true, _) => "From Steam, through Quiver",
                (false, false, true) => "Through a Quiver bridge (container or another launcher)",
                _ => "Tracked by quiverd",
            }
        }
        // The person's games first, then lab instances (agents' headless runs) under their own heading.
        let (play, lab): (Vec<_>, Vec<_>) = self.ui.sessions.iter().cloned().partition(|s| !is_lab_session(s));
        let only_lab = play.is_empty();
        let sessions: Vec<_> = play.into_iter().chain(lab).collect();
        let now = chrono::Utc::now();
        let mut cards: Vec<AnyElement> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            let lab_run = is_lab_session(s);
            if lab_run && !only_lab && (i == 0 || !is_lab_session(&sessions[i - 1])) {
                cards.push(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(14.))
                        .pt(px(10.))
                        .border_t_1()
                        .border_color(th.line1)
                        .child(icon(Icon::Bot, 12., th.tx3))
                        .child(super::caption("Lab instances", th.tx3))
                        .into_any_element(),
                );
            }
            let g = self.game(Gid::from(s.game_ref())).cloned();
            let art = match &g {
                Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Tile, cx),
                None => crate::art::ArtState::Failed,
            };
            let name = g.as_ref().map(|g| clean_name(&g.entry.name)).unwrap_or_else(|| s.game_ref().to_string());
            let launcher = s.holders.iter().any(|h| h == "launcher");
            let bridge = s.holders.iter().any(|h| h == "bridge");
            let steam = s.steam_app_id.is_some() || s.holders.iter().any(|h| h == "steam");
            let how = if lab_run { "Headless lab instance (quiver lab), run by an agent" } else { launched_how(launcher, steam, bridge) };
            let inst = lab_instance(s);
            let id = s.game_ref();
            let row = |label: &str, value: AnyElement| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(div().w(px(92.)).flex_none().text_color(th.tx3).child(label.to_owned()))
                    .child(div().flex_1().min_w_0().child(value))
            };
            cards.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(14.))
                    .when(i > 0 && !(lab_run && !only_lab && !is_lab_session(&sessions[i - 1])), |d| d.border_t_1().border_color(th.line1))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(div().w(px(40.)).h(px(47.)).flex_none().child(art_fill(th, &art, "", 6., false)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(div().font_weight(BOLD).text_size(px(14.)).truncate().child(name))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(th.tx3)
                                            .child(match &inst {
                                                Some(inst) => format!("Lab {inst} · since {}", clock(s.started)),
                                                None if lab_run => format!("Lab run · since {}", clock(s.started)),
                                                None => format!("Since {} · {}", clock(s.started), self.account_name(&s.account)),
                                            }),
                                    ),
                            )
                            .child(
                                super::mono(20., th.tx1)
                                    .font_weight(BOLD)
                                    .child(elapsed((now - s.started).num_seconds())),
                            ),
                    )
                    .child(row("Play time", if lab_run {
                        pill(th, PillKind::Muted, Some(Icon::Bot), "Not counted: a lab run").into_any_element()
                    } else if s.reported {
                        pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Reported to Ubisoft").into_any_element()
                    } else {
                        pill(th, PillKind::Muted, None, "Not reported to Ubisoft").into_any_element()
                    }))
                    .child(row("Launched", div().text_color(th.tx2).child(how).into_any_element()))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .pt(px(2.))
                            .child(
                                button(("np-open", i), th, Kind::Secondary, Size::Small, Some(Icon::Info), "Open game")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ui.now_playing_open = false;
                                        this.open_game(Gid::from(id), cx);
                                        cx.notify();
                                    })),
                            )
                            .when_some(inst.clone().filter(|_| lab_run), |d, inst| d.child(div().flex_1().min_w_0().truncate().child(super::mono(11., th.tx3).child(format!("quiver lab status {inst}")))))
                            .when(launcher && !lab_run, |d| {
                                d.child(
                                    button(("np-stop", i), th, Kind::Secondary, Size::Small, Some(Icon::Square), "Stop game")
                                        .text_color(th.bad)
                                        .on_click(cx.listener(move |this, _, _, cx| this.run_global(GlobalCmd::Stop(Gid::from(id)), cx))),
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id("np-scrim")
            .absolute()
            .inset_0()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ui.now_playing_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("now-playing")
                    .depth_overlay()
                    .absolute()
                    .top(px(58.))
                    // Under the pill on wide windows; at the right edge once it moves.
                    .right(px(if self.bp() == super::responsive::Bp::Wide { 588. } else { 16. }))
                    .w(px(self.dialog_width(420.)))
                    .flex()
                    .flex_col()
                    .rounded(px(14.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line2)
                    .shadow(vec![soft_shadow(rgba(0x000000, 0.6), 24., 70.)])
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(14.))
                            .pt(px(12.))
                            .pb(px(4.))
                            .when(!only_lab, |d| d.child(div().size(px(7.)).rounded_full().bg(th.ok)).child(super::caption("Now playing", th.tx3)))
                            .when(only_lab, |d| d.child(icon(Icon::Bot, 12., th.tx3)).child(super::caption("Lab instances", th.tx3)))
                            .child(div().flex_1())
                            .child(super::mono(11., th.tx4).child("quiver sessions")),
                    )
                    .children(cards),
            )
            .into_any_element()
    }

    /// The top bar's now-playing pill (any launch path, containers included).
    /// `compact`: icon and clock only (narrower windows), the name as a tooltip.
    pub fn render_now_playing_pill(&mut self, th: &Theme, compact: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        // The person's games lead; lab instances alone get a quiet pill of their own.
        let play: Vec<_> = self.ui.sessions.iter().filter(|s| !is_lab_session(s)).cloned().collect();
        let labs: Vec<_> = self.ui.sessions.iter().filter(|s| is_lab_session(s)).cloned().collect();
        let open = self.ui.now_playing_open;
        if play.is_empty() {
            let first = labs.first()?;
            let label = match (labs.len(), lab_instance(first)) {
                (1, Some(inst)) => format!("Lab · {inst}"),
                (1, None) => "Lab run".to_owned(),
                (n, _) => format!("Lab · {n} runs"),
            };
            return Some(
                div()
                    .id("now-playing-pill")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(7.))
                    .h(px(32.))
                    .px(px(10.))
                    .rounded_full()
                    .bg(th.ink3)
                    .border_1()
                    .border_color(th.line2.opacity(if open { 1.0 } else { 0.7 }))
                    .text_size(px(12.))
                    .text_color(th.tx2)
                    .cursor_pointer()
                    .hover(|s| s.border_color(th.line2))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.now_playing_open = !this.ui.now_playing_open;
                        cx.notify();
                    }))
                    .child(icon(Icon::Bot, 13., th.tx3))
                    .when(compact, |d| d.tooltip(super::responsive::tip(th, label.clone())))
                    .when(!compact, |d| d.child(div().max_w(px(150.)).truncate().child(label)))
                    .into_any_element(),
            );
        }
        let first = play[0].clone();
        let n = play.len();
        let label = if n == 1 { self.game_name(Gid::from(first.game_ref())) } else { format!("{n} games") };
        let secs = (chrono::Utc::now() - first.started).num_seconds();
        let reported = play.iter().all(|s| s.reported);
        Some(
            div()
                .id("now-playing-pill")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .h(px(32.))
                .pl(px(10.))
                .pr(px(12.))
                .rounded_full()
                .bg(th.ok_soft)
                .border_1()
                .border_color(th.ok.opacity(if open { 0.7 } else { 0.35 }))
                .text_size(px(12.5))
                .cursor_pointer()
                .hover(|s| s.border_color(th.ok.opacity(0.7)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ui.now_playing_open = !this.ui.now_playing_open;
                    cx.notify();
                }))
                .child(icon(Icon::Gamepad, 14., th.ok))
                .when(compact, |d| d.pr(px(10.)).tooltip(super::responsive::tip(th, label.clone())))
                .when(!compact || n > 1, |d| {
                    let label = if compact { n.to_string() } else { label.clone() };
                    d.child(div().font_weight(SEMIBOLD).text_color(th.tx1).max_w(px(150.)).truncate().child(label))
                })
                .when(n == 1, |d| d.child(super::mono(12., th.tx2).child(elapsed(secs))))
                .when(reported && !compact, |d| d.child(icon(Icon::Cloud, 13., th.ok)))
                .into_any_element(),
        )
    }

    /// Whether the daemon connection is up (the client reconnects on its own).
    pub fn daemon_up(&self) -> bool {
        self.link != crate::app::Link::Reconnecting
    }

    /// Under the top bar while the daemon is gone; the client retries on its own.
    pub fn render_link_banner(&mut self, th: &Theme, _cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.daemon_up() {
            return None;
        }
        let secs = self.ui.down_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .h(px(38.))
                .px(px(self.gutter()))
                .bg(th.warn_soft)
                .border_b_1()
                .border_color(th.warn.opacity(0.3))
                .text_size(px(12.5))
                .child(icon(Icon::RefreshCw, 14., th.warn))
                .child(div().font_weight(SEMIBOLD).text_color(th.warn).child("Reconnecting to quiverd…"))
                .child(div().text_color(th.tx2).truncate().child(format!(
                    "The connection dropped {} ago; Quiver retries on its own and reloads when it's back",
                    if secs < 60 { format!("{secs} s") } else { format!("{} min", secs / 60) },
                )))
                .child(div().flex_1())
                .child(div().text_color(th.tx3).child("Showing what Quiver knew; actions wait for the daemon"))
                .into_any_element(),
        )
    }

    // -----------------------------------------------------------------------------------------
    // Demo setup

    fn start_global_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let flags = demo_flags();
        if flags.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let ready = this
                    .update_in(cx, |this, _, _| match this.conn {
                        Conn::Ready => !this.games.is_empty(),
                        Conn::Login(_) | Conn::Failed(_) => true,
                        Conn::Connecting => false,
                    })
                    .unwrap_or(true);
                if ready {
                    break;
                }
            }
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let _ = this.update_in(cx, |this, _, cx| {
                for f in demo_flags() {
                    let (k, v) = f.split_once('=').unwrap_or((f.as_str(), ""));
                    match k {
                        "settings" => {
                            let s = SettingsSection::ALL
                                .into_iter()
                                .find(|s| s.label().to_lowercase().starts_with(&v.to_lowercase()))
                                .unwrap_or(SettingsSection::Accounts);
                            this.nav.open_settings(s);
                        }
                        "activity" => this.nav.open_section(Section::Activity),
                        "activity-source" => {
                            this.nav.open_section(Section::Activity);
                            let f = SourceFilter::ALL
                                .into_iter()
                                .find(|s| s.label().to_lowercase().starts_with(&v.to_lowercase()))
                                .unwrap_or(SourceFilter::All);
                            this.set_activity_source(f);
                        }
                        "activity-game" => {
                            this.nav.open_section(Section::Activity);
                            this.ui.activity_game = v.parse().ok();
                        }
                        "game" => {
                            if let Ok(id) = v.parse() {
                                this.open_game(id, cx);
                            }
                        }
                        // A game page's tab by its label (`game=3539,tab=profiles`).
                        "tab" => {
                            if let Some(t) = crate::nav::Tab::ALL.into_iter().find(|t| t.label().to_lowercase().starts_with(&v.to_lowercase())) {
                                this.nav.tab = t;
                            }
                        }
                        // With `game=` and `tab=profiles`: open the New profile dialog once the
                        // game's profiles have loaded (`dialog=new-profile`).
                        "dialog" if v == "new-profile" => {
                            cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(Duration::from_millis(1500)).await;
                                let _ = this.update(cx, |this, cx| {
                                    if let Page::Game(id) = this.nav.page {
                                        this.new_profile_dialog(id, cx);
                                        cx.notify();
                                    }
                                });
                            })
                            .detach();
                        }
                        // The library's store filter (`store=steam`).
                        "store" => this.store_filter = quiver_api::StoreId::parse(v),
                        // Unsaved launch-options edits on a game (`unsaved=3539`).
                        "unsaved" => {
                            if let Ok(id) = v.parse() {
                                this.demo_unsaved(id, cx);
                            }
                        }
                        "palette" => this.nav.open_palette(),
                        // A toast that stays (`toast=Some text`), for screenshots.
                        "toast" => this.toast_for(v.to_owned().into(), false, 600_000, cx),
                        "add-account" => this.start_add_account(cx),
                        // With `signed-out`: sign in from the library's card after N s.
                        "autosignin" => this.demo_auto_signin(v.parse().unwrap_or(4), cx),
                        "confirm-signout" => this.run_global(GlobalCmd::SignOut(1), cx),
                        "now-playing" => this.ui.now_playing_open = true,
                        "reconnecting" => {
                            this.link = crate::app::Link::Reconnecting;
                            this.ui.down_since = Some(Instant::now() - Duration::from_secs(14));
                        }
                        "signin-error" => {
                            if let Conn::Login(form) = &mut this.conn {
                                form.error = Some("That code didn't work. Codes change every 30 seconds.".into());
                            }
                        }
                        "pad" => this.mode = InputMode::Pad,
                        "keys" => this.mode = InputMode::Keyboard,
                        "focus-body" => {
                            this.nav.zone = crate::nav::Zone::Body;
                            this.mode = InputMode::Keyboard;
                        }
                        _ => {}
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Demo (`autosignin=N`): after N seconds, sign in the way a person would from the
    /// library's card: open the form, type an email and password, continue, type the code,
    /// verify. Checks the signed-out start end to end (and gives screenshots of both).
    fn demo_auto_signin(&mut self, after_secs: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(after_secs)).await;
            let steps: [fn(&mut Self, &mut Context<Self>); 3] = [
                |this, cx| this.start_add_account(cx),
                |this, cx| {
                    if let Conn::Login(f) = &this.conn {
                        let (e, p) = (f.email.clone(), f.password.clone());
                        e.update(cx, |i, cx| i.set_text("player.one@example.com", cx));
                        p.update(cx, |i, cx| i.set_text("hunter2", cx));
                    }
                    this.submit_signin(cx);
                },
                |this, cx| {
                    if let Conn::Login(f) = &this.conn {
                        let c = f.code.clone();
                        c.update(cx, |i, cx| i.set_text("246810", cx));
                    }
                    this.submit_signin(cx);
                },
            ];
            for step in steps {
                if this.update(cx, |this, cx| {
                    step(this, cx);
                    cx.notify();
                })
                .is_err()
                {
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(900)).await;
            }
        })
        .detach();
    }

    /// Ctrl C in the palette with nothing selected in the field: copy the highlighted
    /// entry's `quiver` command.
    fn copy_palette_cli(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::Palette { selected }) = self.nav.overlay else { return };
        if let Some(cli) = self.palette_commands().get(selected).and_then(|c| c.cli.clone()) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(cli.to_string()));
            self.show_toast(format!("Copied: {cli}"), cx);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks() {
        assert_eq!(elapsed(4 * 60 + 7), "4:07");
        assert_eq!(elapsed(3600 + 12 * 60 + 8), "1:12:08");
        assert_eq!(elapsed(-5), "0:00");
    }

    #[test]
    fn confirm_buttons_left_to_right() {
        let mut c = Confirm {
            title: "t".into(),
            body: "b".into(),
            confirm: "Remove".into(),
            alt: None,
            cli: None,
            action: ConfirmAction::RestartSteam,
            selected: 0,
        };
        assert_eq!(c.buttons(), 2);
        assert_eq!((c.pick(0), c.pick(1)), (ConfirmPick::Cancel, ConfirmPick::Action));
        assert!(!c.action.constructive(), "destructive questions get the red button");
        c.alt = Some("Play without them".into());
        c.action = ConfirmAction::PlayUnsaved { game: Gid::from(1), play: PlayAfter::Primary };
        assert_eq!(c.buttons(), 3);
        assert_eq!((c.pick(0), c.pick(1), c.pick(2)), (ConfirmPick::Cancel, ConfirmPick::Alt, ConfirmPick::Action));
        assert!(c.action.constructive());
    }

    #[test]
    fn source_filters() {
        assert!(SourceFilter::Agent.matches("aco-dlss"));
        assert!(SourceFilter::Agent.matches("claude-code"));
        assert!(!SourceFilter::Agent.matches("gui"));
        assert!(SourceFilter::Cli.matches("cli"));
        assert!(SourceFilter::System.matches("system"));
        assert!(SourceFilter::All.matches("anything"));
        assert_eq!(SourceFilter::All.step(-1), SourceFilter::System);
        assert_eq!(SourceFilter::System.step(1), SourceFilter::All);
    }
}

/// A headless lab instance's run (`quiver lab`), not the person playing: held by the lab only,
/// or a `lab-<instance>-r<N>` session. It counts for nothing (play time, reports).
pub fn is_lab_session(s: &quiver_api::ActiveSession) -> bool {
    (!s.holders.is_empty() && s.holders.iter().all(|h| h == "lab")) || s.session.as_deref().is_some_and(|x| x.starts_with("lab-"))
}

/// The lab instance a lab session runs in (`lab-awr-sight-r3` → `awr-sight`).
pub fn lab_instance(s: &quiver_api::ActiveSession) -> Option<String> {
    let id = s.session.as_deref()?.strip_prefix("lab-")?;
    let (inst, run) = id.rsplit_once("-r")?;
    run.chars().all(|c| c.is_ascii_digit()).then(|| inst.to_owned())
}

#[cfg(test)]
mod lab_session_tests {
    use super::{is_lab_session, lab_instance};
    use quiver_api::ActiveSession;

    fn s(holders: &[&str], session: Option<&str>) -> ActiveSession {
        ActiveSession { holders: holders.iter().map(|h| h.to_string()).collect(), session: session.map(str::to_owned), ..Default::default() }
    }

    #[test]
    fn lab_runs_are_told_apart_from_play() {
        assert!(is_lab_session(&s(&["lab"], None)));
        assert!(is_lab_session(&s(&["bridge"], Some("lab-awr-sight-r3"))));
        assert!(!is_lab_session(&s(&["launcher"], Some("s-jcf3jqgzlc"))));
        assert!(!is_lab_session(&s(&["lab", "launcher"], None)), "the person playing too is play");
        assert!(!is_lab_session(&s(&[], None)));
        assert_eq!(lab_instance(&s(&["lab"], Some("lab-awr-sight-r3"))).as_deref(), Some("awr-sight"));
        assert_eq!(lab_instance(&s(&["lab"], Some("lab-hudmock-rabbids-r12"))).as_deref(), Some("hudmock-rabbids"));
        assert_eq!(lab_instance(&s(&["lab"], Some("s-abc"))), None);
    }
}
