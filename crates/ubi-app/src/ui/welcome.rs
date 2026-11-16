//! The welcome flow: shown when quiverd has no first-run record (`Request::Setup`), and
//! again from Settings. Seven short steps, each skippable: sign in (the sign-in screen
//! itself), where games are installed (the per-game compatibility folders are under
//! "Advanced"), importing Ubisoft Connect downloads from elsewhere, Steam (Ubisoft games
//! bought on Steam starting through Quiver, and shortcuts), the controller, recommended
//! fixes for unmodded games, and the theme. `quiver setup` walks the same choices in a
//! terminal.
//!
//! When setup was finished but "recommended fixes" was left at "ask me later", only that
//! question comes back, once per app start. Skipping setup doesn't ask again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{AnyElement, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, Styled, Window, div, prelude::*, px};
use ubi_api::exports::{ExportKind, ExportState, SteamExportResult};
use ubi_api::setup::{FinishSetup, FixesChoice, FoundInstall, InstallSource, SetupState};
use ubi_api::mods::{ModPrefs, ModsRequest};
use ubi_api::{Event, Job, JobKind, Request};

use super::downloads::who;
use super::game::toggle;
use super::settings::brand_mark;
use super::{BOLD, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, art_fill, button, caption, command_line, display, glyph, icon, mono, pill, progress, soft_shadow};
use crate::app::{Conn, InputMode, Quiver};
use crate::art::{ArtSlot, ArtState, Variant};
use crate::model::{clean_name, gb, tilde};
use crate::nav::Input;
use crate::text_input::TextInput;
use crate::theme::{Theme, rgba};
use crate::err_text;

/// Below this content width, the welcome flow's rows of cards stack.
const WC_ROW_MIN: f32 = 520.;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    #[default]
    SignIn,
    Storage,
    Import,
    Steam,
    Controller,
    Fixes,
    Theme,
}

impl Step {
    pub const ALL: [Step; 7] = [Step::SignIn, Step::Storage, Step::Import, Step::Steam, Step::Controller, Step::Fixes, Step::Theme];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            Step::SignIn => "Sign in",
            Step::Storage => "Where games live",
            Step::Import => "Connect downloads",
            Step::Steam => "Steam",
            Step::Controller => "Controller",
            Step::Fixes => "Recommended fixes",
            Step::Theme => "Theme",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Step::SignIn => "Your Ubisoft account",
            Step::Storage => "The install folder",
            Step::Import => "Already downloaded elsewhere",
            Step::Steam => "Your Steam games, shortcuts",
            Step::Controller => "Pads, hyprpad, button hints",
            Step::Fixes => "For unmodded games",
            Step::Theme => "Follows Omarchy",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Step::SignIn => "Sign in with your Ubisoft account",
            Step::Storage => "Where games are installed",
            Step::Import => "Ubisoft Connect games you already downloaded",
            Step::Steam => "Steam and your Ubisoft games",
            Step::Controller => "Playing with a controller",
            Step::Fixes => "Recommended fixes for unmodded games",
            Step::Theme => "Your theme, already",
        }
    }

    fn sub(self) -> &'static str {
        match self {
            Step::SignIn => "Quiver signs in to Ubisoft the way Ubisoft Connect does, then shows every game you own.",
            Step::Storage => "Each game gets its own folder here. Pick the place before anything is installed; most people keep the default.",
            Step::Import => "Games you downloaded with Ubisoft Connect (under Lutris, Wine, or inside a Steam game) can join your library without downloading again. Ubisoft games you bought on Steam are handled under Steam instead.",
            Step::Steam => "Ubisoft games you bought on Steam can start through Quiver instead of Ubisoft Connect, and games Quiver installs can show up in your Steam library.",
            Step::Controller => "The whole app works from a pad, next to hyprpad. Nothing to set up; here is what the buttons do.",
            Step::Fixes => "Small, audited tweaks that make games behave on a PC. You decide whether unmodded games get them.",
            Step::Theme => "Quiver takes its colors from Omarchy and restyles the moment you switch themes.",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Step::SignIn => Icon::User,
            Step::Storage => Icon::HardDrive,
            Step::Import => Icon::FolderInput,
            Step::Steam => Icon::Gamepad,
            Step::Controller => Icon::Gamepad,
            Step::Fixes => Icon::Sparkles,
            Step::Theme => Icon::Palette,
        }
    }

    fn step(self, d: isize) -> Step {
        let i = (self.index() as isize + d).clamp(0, Self::ALL.len() as isize - 1);
        Self::ALL[i as usize]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// All the steps.
    #[default]
    Full,
    /// Only "recommended fixes" (it was left at "ask me later").
    FixesOnly,
    /// Only "Connect downloads" (Downloads' and the game list's "Import Connect downloads").
    ImportOnly,
}

/// A focusable control of the welcome flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WCtl {
    AddAccount,
    /// Show or hide the compatibility folder (Storage › Advanced).
    Advanced,
    ApplyPaths,
    Rescan,
    Import(usize),
    SteamOnInstall,
    SteamUser(usize),
    SteamAdd(u32),
    SteamAddAll,
    /// A Ubisoft game in Steam (by app id): start it through Quiver, or undo that.
    SteamPatch(u32),
    SteamUndo(u32),
    SteamPatchAll,
    /// Offer the switch for later Steam installs.
    SteamOffers,
    /// Restart Steam: ask (inline), then "Not now" or "Restart Steam".
    SteamRestart,
    SteamRestartNo,
    SteamRestartYes,
    Fix(FixesChoice),
    Skip,
    Back,
    Next,
}

#[derive(Default)]
pub struct Welcome {
    pub open: bool,
    pub mode: Mode,
    pub step: Step,
    /// The focused control (`usize::MAX`: the primary one).
    pub focus: usize,
    /// The startup check ran.
    startup: bool,
    /// The flow waits for sign-in (the sign-in screen shows step 1).
    pub after_signin: bool,
    pub scan: Option<Result<Vec<FoundInstall>, String>>,
    scanning: bool,
    /// Imports started here: folder -> job.
    imports: HashMap<PathBuf, u64>,
    pub fixes: Option<FixesChoice>,
    steam_on_install: Option<bool>,
    steam_user: Option<String>,
    steam_busy: HashSet<u32>,
    lib_input: Option<Entity<TextInput>>,
    pre_input: Option<Entity<TextInput>>,
    /// Storage › Advanced is open.
    advanced: bool,
    /// The inline "Restart Steam now?" question is showing.
    restart_ask: bool,
    /// Paths changed: quiverd is restarting.
    restarting: bool,
    /// Width of the step body, as of the last render (rows of cards share it).
    cw: f32,
}

impl Quiver {
    // -----------------------------------------------------------------------------------------
    // Hooks (from downloads.rs)

    pub fn welcome_open(&self) -> bool {
        self.wc.open && matches!(self.conn, Conn::Ready)
    }

    pub fn welcome_tick(&mut self, _n: u64, cx: &mut Context<Self>) {
        if self.wc.after_signin && matches!(self.conn, Conn::Ready) {
            self.wc.after_signin = false;
            self.open_welcome(Step::Storage, cx);
            cx.notify();
        }
    }

    /// `Request::Setup` answered (at startup, or refreshed).
    pub fn welcome_setup_loaded(&mut self, cx: &mut Context<Self>) {
        self.wc.restarting = self.wc.restarting && self.dl.setup.as_ref().is_some_and(|s| s.restarting);
        if self.wc.startup {
            return;
        }
        self.wc.startup = true;
        let Some(s) = self.dl.setup.clone() else { return };
        if crate::ui::global::demo_flag("welcome").is_some() || crate::ui::global::demo_flag("welcome-fixes").is_some() {
            return; // the demo opens it itself
        }
        match &s.first_run {
            None if self.signed_in() => self.open_welcome(Step::Storage, cx),
            None => self.wc.after_signin = true,
            Some(f) if !f.skipped && s.fixes_on_vanilla.is_none() => self.open_welcome_mode(Mode::FixesOnly, Step::Fixes, cx),
            Some(_) => {}
        }
        cx.notify();
    }

    pub fn welcome_event(&mut self, e: &Event, cx: &mut Context<Self>) {
        match e {
            Event::Reconnected if self.wc.restarting => {
                self.wc.restarting = false;
                self.wc.lib_input = None;
                self.wc.pre_input = None;
                self.load_setup(cx);
                self.show_toast("quiverd is back with the new folders", cx);
            }
            Event::Job(j) if j.kind == JobKind::Import && j.done && self.wc.imports.values().any(|id| *id == j.id) => {
                let name = self.game_name(j.product_id);
                match &j.error {
                    Some(e) => self.show_toast(format!("Import of {name} failed: {e}"), cx),
                    None => self.show_toast(format!("{name}: {}", j.status), cx),
                }
                self.load_setup(cx);
            }
            _ => {}
        }
    }

    /// Demo flags (`QUIVER_DEMO_GLOBAL`): `welcome[=step]`, `welcome-fixes`,
    /// `welcome-import`, `wfocus=N` (focus control N), `wfix=yes|no|later`, `wadv` (Storage ›
    /// Advanced open), `wrestart` (the Steam step asking to restart Steam).
    pub fn welcome_demo(&mut self, k: &str, v: &str, cx: &mut Context<Self>) {
        match k {
            "welcome" => {
                let step = Step::ALL.into_iter().find(|s| !v.is_empty() && s.label().to_lowercase().replace(' ', "-").starts_with(&v.to_lowercase())).unwrap_or(Step::Storage);
                self.open_welcome(step, cx);
            }
            "welcome-fixes" => self.open_welcome_mode(Mode::FixesOnly, Step::Fixes, cx),
            "welcome-import" => self.open_import(cx),
            "wfocus" => self.wc.focus = v.parse().unwrap_or(usize::MAX),
            "wfix" => self.wc.fixes = v.parse().ok(),
            "wadv" => self.wc.advanced = true,
            "wrestart" => self.wc.restart_ask = true,
            _ => {}
        }
    }

    fn signed_in(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.signed_in.is_some())
    }

    // -----------------------------------------------------------------------------------------
    // Opening and closing

    pub fn open_welcome(&mut self, step: Step, cx: &mut Context<Self>) {
        self.open_welcome_mode(Mode::Full, step, cx);
    }

    /// "Import Connect downloads": just that step.
    pub fn open_import(&mut self, cx: &mut Context<Self>) {
        self.open_welcome_mode(Mode::ImportOnly, Step::Import, cx);
    }

    fn open_welcome_mode(&mut self, mode: Mode, step: Step, cx: &mut Context<Self>) {
        self.nav.close_overlay();
        self.dl.dialog = None;
        self.wc.open = true;
        self.wc.mode = mode;
        if self.wc.fixes.is_none() {
            self.wc.fixes = self.dl.setup.as_ref().and_then(|s| s.fixes_on_vanilla).map(|v| FixesChoice::of(Some(v)));
        }
        if self.wc.steam_on_install.is_none() {
            self.wc.steam_on_install = Some(self.dl.setup.as_ref().and_then(|s| s.first_run.as_ref()).is_none_or(|f| f.steam_on_install));
        }
        self.go_step(step, cx);
    }

    fn go_step(&mut self, step: Step, cx: &mut Context<Self>) {
        // Signed in: the first step is done; its page only offers adding an account.
        self.wc.step = step;
        self.wc.focus = usize::MAX;
        match step {
            Step::Import if self.wc.scan.is_none() => self.start_scan(cx),
            Step::Storage => {
                self.load_setup(cx);
                self.wc.lib_input = None;
                self.wc.pre_input = None;
            }
            Step::Steam => {
                self.wc.restart_ask = false;
                self.load_global_exports(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    fn close_welcome(&mut self, cx: &mut Context<Self>) {
        self.wc.open = false;
        self.wc.lib_input = None;
        self.wc.pre_input = None;
        cx.notify();
    }

    fn finish_welcome(&mut self, skipped: bool, cx: &mut Context<Self>) {
        let steam_on_install = self.wc.steam_on_install.unwrap_or(true);
        self.call::<SetupState, _>(Request::Setup { library: None, prefixes: None, finish: Some(FinishSetup { skipped, steam_on_install }) }, cx, move |this, r, cx| {
            match r {
                Ok(s) => {
                    this.dl.setup = Some(s);
                    this.show_toast(
                        if skipped { "Setup skipped. Run it again any time from Settings › Library and storage" } else { "You're set. Enjoy your games" },
                        cx,
                    );
                }
                Err(e) => this.show_toast(err_text(&e), cx),
            }
        });
        self.close_welcome(cx);
    }

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.wc.scanning {
            return;
        }
        self.wc.scanning = true;
        self.call::<Vec<FoundInstall>, _>(Request::ScanInstalls, cx, |this, r, _| {
            this.wc.scanning = false;
            this.wc.scan = Some(r.map_err(|e| err_text(&e)));
        });
    }

    fn set_fixes(&mut self, c: FixesChoice, cx: &mut Context<Self>) {
        self.wc.fixes = Some(c);
        // Mods v2's app-level preference. "Later" leaves it undecided, so nothing is sent.
        let Some(v) = c.value() else { return };
        self.call::<ModPrefs, _>(Request::Mods(ModsRequest::Prefs { fixes_on_vanilla: Some(v) }), cx, |this, r, cx| match r {
            Ok(p) => {
                if let Some(s) = &mut this.dl.setup {
                    s.fixes_on_vanilla = p.fixes_on_vanilla;
                }
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    // -----------------------------------------------------------------------------------------
    // Controls and input

    fn steam_games(&self) -> Vec<(u32, String, Option<ExportState>)> {
        let exports = match &self.ui.exports {
            Some(Ok(l)) => l.clone(),
            _ => vec![],
        };
        self.ordered()
            .into_iter()
            .filter(|g| g.installed())
            .map(|g| {
                let st = exports.iter().find(|x| x.record.kind == ExportKind::Steam && x.record.product_id == g.id() && x.record.profile_id.is_none()).map(|x| x.state);
                (g.id(), clean_name(&g.entry.name), st)
            })
            .collect()
    }

    fn steam_missing(&self) -> Vec<u32> {
        self.steam_games().into_iter().filter(|(_, _, s)| matches!(s, None | Some(ExportState::Missing) | Some(ExportState::Changed))).map(|(id, ..)| id).collect()
    }

    /// Steam users to choose from (several, none chosen yet).
    fn steam_choice(&self) -> Vec<(String, String)> {
        match &self.ui.steam {
            Some(Ok(s)) if s.selected.is_none() && s.users.len() > 1 => {
                s.users.iter().map(|u| (u.id.clone(), u.persona_name.clone().or_else(|| u.account_name.clone()).unwrap_or_else(|| u.id.clone()))).collect()
            }
            _ => vec![],
        }
    }

    fn found(&self) -> Vec<FoundInstall> {
        match &self.wc.scan {
            Some(Ok(v)) => v.clone(),
            _ => vec![],
        }
    }

    fn import_job(&self, path: &PathBuf) -> Option<Job> {
        let id = self.wc.imports.get(path)?;
        self.status.as_ref()?.jobs.iter().find(|j| j.id == *id).cloned()
    }

    pub fn welcome_controls(&self) -> Vec<WCtl> {
        let mut v = vec![];
        match self.wc.step {
            Step::SignIn => v.push(WCtl::AddAccount),
            Step::Storage => {
                v.push(WCtl::Advanced);
                if self.dl.setup.as_ref().is_some_and(|s| !s.paths_locked) {
                    v.push(WCtl::ApplyPaths);
                }
            }
            Step::Import => {
                v.push(WCtl::Rescan);
                for (i, f) in self.found().iter().enumerate() {
                    if f.importable && self.import_job(&f.path).is_none() {
                        v.push(WCtl::Import(i));
                    }
                }
            }
            Step::Steam => {
                if matches!(&self.ui.steam, Some(Ok(s)) if s.root.is_some()) {
                    if self.steam_switchable().len() > 1 {
                        v.push(WCtl::SteamPatchAll);
                    }
                    for g in self.steam_copies() {
                        if let Some(c) = steam_copy_control(&g) {
                            v.push(c);
                        }
                    }
                    v.push(WCtl::SteamOffers);
                    v.push(WCtl::SteamOnInstall);
                    v.extend((0..self.steam_choice().len()).map(WCtl::SteamUser));
                    let missing = self.steam_missing();
                    if missing.len() > 1 {
                        v.push(WCtl::SteamAddAll);
                    }
                    v.extend(missing.into_iter().map(WCtl::SteamAdd));
                    if self.steam_waiting() && self.steam_running() {
                        if self.wc.restart_ask {
                            v.extend([WCtl::SteamRestartNo, WCtl::SteamRestartYes]);
                        } else {
                            v.push(WCtl::SteamRestart);
                        }
                    }
                }
            }
            Step::Controller | Step::Theme => {}
            Step::Fixes => v.extend([WCtl::Fix(FixesChoice::Yes), WCtl::Fix(FixesChoice::No), WCtl::Fix(FixesChoice::Later)]),
        }
        match self.wc.mode {
            Mode::Full => {
                v.push(WCtl::Skip);
                if self.wc.step != Step::SignIn {
                    v.push(WCtl::Back);
                }
                v.push(WCtl::Next);
            }
            Mode::FixesOnly | Mode::ImportOnly => v.push(WCtl::Next),
        }
        v
    }

    fn welcome_sel(&self) -> usize {
        let n = self.welcome_controls().len();
        if self.wc.focus == usize::MAX { n.saturating_sub(1) } else { self.wc.focus.min(n.saturating_sub(1)) }
    }

    fn wfocused(&self, c: WCtl) -> bool {
        self.mode != InputMode::Pointer && self.welcome_controls().get(self.welcome_sel()) == Some(&c)
    }

    pub fn welcome_input(&mut self, input: Input, cx: &mut Context<Self>) -> bool {
        let ctl = self.welcome_controls();
        let n = ctl.len().max(1);
        let sel = self.welcome_sel();
        match input {
            Input::Up | Input::Left => self.wc.focus = sel.saturating_sub(1),
            Input::Down | Input::Right => self.wc.focus = (sel + 1).min(n - 1),
            Input::Activate => {
                if let Some(c) = ctl.get(sel).copied() {
                    self.welcome_press(c, cx);
                }
            }
            Input::Back => match self.wc.mode {
                Mode::Full if self.wc.step.index() > 1 || (self.wc.step == Step::Storage && !self.signed_in()) => {
                    let s = self.wc.step.step(-1);
                    self.go_step(s, cx);
                }
                Mode::Full => {}
                Mode::FixesOnly => {
                    // Leaving without an answer is "ask me later".
                    self.set_fixes(FixesChoice::Later, cx);
                    self.close_welcome(cx);
                }
                Mode::ImportOnly => self.close_welcome(cx),
            },
            Input::PrevTab | Input::NextTab if self.wc.mode == Mode::Full => {
                let s = self.wc.step.step(if input == Input::NextTab { 1 } else { -1 });
                if s != Step::SignIn || !self.signed_in() || self.wc.step != Step::Storage {
                    self.go_step(s, cx);
                }
            }
            _ => {}
        }
        cx.notify();
        true
    }

    fn welcome_press(&mut self, c: WCtl, cx: &mut Context<Self>) {
        if let Some(i) = self.welcome_controls().iter().position(|x| *x == c) {
            self.wc.focus = i;
        }
        match c {
            WCtl::AddAccount => self.start_add_account(cx),
            WCtl::Advanced => self.wc.advanced = !self.wc.advanced,
            WCtl::ApplyPaths => {
                let read = |e: &Option<Entity<TextInput>>, cx: &Context<Self>| e.as_ref().map(|e| e.read(cx).text().trim().to_owned()).filter(|t| !t.is_empty()).map(PathBuf::from);
                let (library, prefixes) = (read(&self.wc.lib_input, cx), read(&self.wc.pre_input, cx));
                let cur = self.dl.setup.clone();
                let library = library.filter(|p| cur.as_ref().is_none_or(|s| *p != s.library));
                let prefixes = prefixes.filter(|p| cur.as_ref().is_none_or(|s| *p != s.prefixes));
                if library.is_none() && prefixes.is_none() {
                    return self.show_toast("Those are the folders already in use", cx);
                }
                self.call::<SetupState, _>(Request::Setup { library, prefixes, finish: None }, cx, |this, r, cx| match r {
                    Ok(s) => {
                        this.wc.restarting = s.restarting;
                        this.dl.setup = Some(s);
                        this.show_toast("Saved. quiverd restarts to use the new folders", cx);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            WCtl::Rescan => {
                self.wc.scan = None;
                self.start_scan(cx);
            }
            WCtl::Import(i) => {
                let Some(f) = self.found().get(i).cloned() else { return };
                let Some(product_id) = f.product_id else { return };
                let path = f.path.clone();
                self.call::<u64, _>(Request::Import { product_id, path: f.path.clone(), language: f.language.clone() }, cx, move |this, r, cx| match r {
                    Ok(job) => {
                        this.wc.imports.insert(path, job);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            WCtl::SteamOnInstall => self.wc.steam_on_install = Some(!self.wc.steam_on_install.unwrap_or(true)),
            WCtl::SteamUser(i) => self.wc.steam_user = self.steam_choice().get(i).map(|u| u.0.clone()),
            WCtl::SteamAdd(id) => self.welcome_add_steam(vec![id], cx),
            WCtl::SteamAddAll => {
                let ids = self.steam_missing();
                self.welcome_add_steam(ids, cx);
            }
            WCtl::SteamPatch(app_id) => self.steam_patch(app_id, false, cx),
            WCtl::SteamUndo(app_id) => self.steam_patch(app_id, true, cx),
            WCtl::SteamPatchAll => self.steam_patch_all(cx),
            WCtl::SteamOffers => {
                let on = !self.steam_offers_enabled();
                self.set_steam_offers(on, cx);
            }
            WCtl::SteamRestart => self.wc.restart_ask = true,
            WCtl::SteamRestartNo => {
                self.wc.restart_ask = false;
                self.show_toast("Not restarting Steam. The changes apply the next time Steam starts", cx);
            }
            WCtl::SteamRestartYes => {
                self.wc.restart_ask = false;
                self.restart_steam(cx);
            }
            WCtl::Fix(choice) => {
                self.set_fixes(choice, cx);
                if self.wc.mode == Mode::FixesOnly {
                    self.close_welcome(cx);
                    self.show_toast(
                        match choice {
                            FixesChoice::Yes => "Recommended fixes are on for unmodded games",
                            FixesChoice::No => "Unmodded games run exactly as shipped",
                            FixesChoice::Later => "Quiver asks again next time",
                        },
                        cx,
                    );
                }
            }
            WCtl::Skip => self.finish_welcome(true, cx),
            WCtl::Back => {
                let s = self.wc.step.step(-1);
                self.go_step(s, cx);
            }
            WCtl::Next => match (self.wc.mode, self.wc.step) {
                (Mode::Full, Step::Theme) => self.finish_welcome(false, cx),
                (Mode::Full, s) => {
                    if s == Step::Fixes && self.wc.fixes.is_none() {
                        self.set_fixes(FixesChoice::Later, cx);
                    }
                    self.go_step(s.step(1), cx);
                }
                (Mode::FixesOnly, _) => {
                    if self.wc.fixes.is_none() {
                        self.set_fixes(FixesChoice::Later, cx);
                    }
                    self.close_welcome(cx);
                }
                (Mode::ImportOnly, _) => self.close_welcome(cx),
            },
        }
        cx.notify();
    }

    fn welcome_add_steam(&mut self, ids: Vec<u32>, cx: &mut Context<Self>) {
        if !self.steam_choice().is_empty() && self.wc.steam_user.is_none() {
            return self.show_toast("Pick which Steam user gets the shortcuts first", cx);
        }
        for id in ids {
            self.wc.steam_busy.insert(id);
            let user = self.wc.steam_user.clone();
            self.call::<SteamExportResult, _>(Request::ExportSteam { product_id: id, remove: false, steam_user: user }, cx, move |this, r, cx| {
                this.wc.steam_busy.remove(&id);
                if let Err(e) = r {
                    this.show_toast(err_text(&e), cx);
                }
                this.load_global_exports(cx);
            });
        }
    }

    // -----------------------------------------------------------------------------------------
    // Rendering

    /// Step 1 of the flow on the sign-in screen.
    pub fn welcome_signin_banner(&self, th: &Theme) -> Option<AnyElement> {
        if !self.wc.after_signin {
            return None;
        }
        let dots = div().flex().items_center().gap(px(5.)).children(Step::ALL.iter().map(|s| {
            let on = *s == Step::SignIn;
            div().h(px(6.)).w(px(if on { 22. } else { 6. })).rounded_full().bg(if on { th.acc } else { th.ink5 })
        }));
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .w(px(480.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(10.))
                .bg(th.acc_soft)
                .border_1()
                .border_color(th.acc_line)
                .text_size(px(12.5))
                .child(icon(Icon::Sparkles, 14., th.acc_hi))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(1.))
                        .child(div().font_weight(SEMIBOLD).text_color(th.acc_hi).child("Welcome to Quiver · step 1 of 7"))
                        .child(div().text_color(th.tx2).child("Next: install folder, Connect downloads, Steam, controller, fixes, theme")),
                )
                .child(dots)
                .into_any_element(),
        )
    }

    pub fn render_welcome(&mut self, th: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.wc.after_signin && matches!(self.conn, Conn::Ready) {
            self.wc.after_signin = false;
            self.open_welcome(Step::Storage, cx);
        }
        if !self.welcome_open() {
            return None;
        }
        let _ = window;
        if self.wc.step == Step::Storage {
            self.ensure_path_inputs(cx);
        }
        let body = self.welcome_body(th, cx);
        let footer = self.welcome_footer(th, cx);
        let step = self.wc.step;
        let compact = self.wc.mode != Mode::Full;
        let (vw, vh) = self.viewport;
        // Below Medium the step rail gives way to a "Step N of 7" header with a bar of
        // steps, and narrow windows get the dialog nearly edge to edge.
        let bp = self.bp();
        let rail = !compact && bp.two_columns();
        let narrow = bp.narrow();
        let margin = if narrow { 8. } else { 24. };
        let side = if narrow { 18. } else { 32. };
        let pw = if compact { 760f32.min(vw - 2. * margin) } else { 1100f32.min(vw - 2. * margin) };
        self.wc.cw = pw - if rail { 280. } else { 0. } - 2. * side;
        let ph = match self.wc.mode {
            Mode::FixesOnly => (vh - 120.).min(560.),
            Mode::ImportOnly => (vh - 120.).min(720.),
            Mode::Full if narrow => (vh - 2. * margin - 40.).min(900.),
            Mode::Full => (vh - 72.).min(760.),
        };
        let steps_bar = (!rail && self.wc.mode == Mode::Full).then(|| {
            let cur = step.index();
            div().flex().gap(px(4.)).pt(px(4.)).pb(px(2.)).children(Step::ALL.iter().map(|s| {
                let i = s.index();
                div().flex_1().h(px(4.)).rounded(px(2.)).bg(if i < cur { th.acc.opacity(0.5) } else if i == cur { th.acc } else { th.track })
            }))
        });

        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(side))
                    .pt(px(if narrow { 20. } else { 28. }))
                    .pb(px(18.))
                    .children(steps_bar)
                    .child(div().flex().items_center().gap(px(7.)).child(icon(step.icon(), 13., th.acc_hi)).child(caption(
                        &match self.wc.mode {
                            Mode::Full => format!("Step {} of 7 · {}", step.index() + 1, step.label()),
                            Mode::FixesOnly => "One more thing".to_owned(),
                            Mode::ImportOnly => "Import".to_owned(),
                        },
                        th.acc_hi,
                    )))
                    .child(display(if narrow { 21. } else { 24. }, FontWeight::BOLD, th.tx1).child(step.title()))
                    .child(div().text_color(th.tx2).text_size(px(13.)).line_height(px(19.)).child(step.sub())),
            )
            .child(div().id("wc-body").flex_1().min_h_0().overflow_y_scroll().px(px(side)).pb(px(24.)).child(div().flex().flex_col().gap(px(14.)).w(px(self.wc.cw)).children(body)))
            .child(footer);

        let mut panel = div()
            .id("welcome")
            .w(px(pw))
            .h(px(ph))
            .flex()
            .rounded(px(16.))
            .overflow_hidden()
            .bg(th.ink2)
            .border_1()
            .border_color(th.line2)
            .shadow(vec![soft_shadow(th.acc.opacity(0.10), 0., 180.), soft_shadow(rgba(0x000000, 0.6), 30., 80.)])
            .on_click(|_, _, cx| cx.stop_propagation());
        if rail {
            panel = panel.child(self.welcome_rail(th, cx));
        }
        panel = panel.child(content);

        let mut root = div().id("welcome-root").absolute().inset_0();
        if compact {
            root = root.bg(th.scrim);
        } else {
            root = root.bg(th.ink0);
            if let Some(m) = self.render_signin_mosaic(th, cx) {
                root = root.child(m);
            }
        }
        Some(
            root.child(div().absolute().inset_0().flex().flex_col().items_center().justify_center().gap(px(14.)).child(panel).when(!compact, |d| {
                d.child(
                    div()
                        .max_w(px(pw))
                        .px(px(margin))
                        .text_center()
                        .text_size(px(11.5))
                        .text_color(th.tx3)
                        .child("Quiver is a third-party launcher for your Ubisoft library. Not affiliated with or endorsed by Ubisoft."),
                )
            }))
            .into_any_element(),
        )
    }

    fn welcome_rail(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let cur = self.wc.step;
        let signed = self.signed_in();
        let items = Step::ALL.iter().map(|&s| {
            let done = s.index() < cur.index() || (s == Step::SignIn && signed);
            let on = s == cur;
            let mark = if done && !on {
                div().flex().items_center().justify_center().size(px(22.)).rounded_full().bg(th.ok_soft).child(icon(Icon::Check, 13., th.ok))
            } else if on {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded_full()
                    .bg(th.acc)
                    .text_color(th.acc_ink)
                    .text_size(px(11.))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .child((s.index() + 1).to_string())
            } else {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded_full()
                    .border_1()
                    .border_color(th.line2)
                    .text_color(th.tx3)
                    .text_size(px(11.))
                    .child((s.index() + 1).to_string())
            };
            div()
                .id(("wc-step", s.index()))
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(10.))
                .py(px(8.))
                .rounded(px(10.))
                .cursor_pointer()
                .when(on, |d| d.bg(th.sel))
                .hover(|d| d.bg(th.ink3))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if s != Step::SignIn || !this.signed_in() {
                        this.go_step(s, cx);
                    } else {
                        this.go_step(Step::SignIn, cx);
                    }
                }))
                .child(mark)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(div().font_weight(if on { SEMIBOLD } else { MEDIUM }).text_color(if on { th.tx1 } else if done { th.tx2 } else { th.tx3 }).child(s.label()))
                        .child(div().text_size(px(11.5)).text_color(th.tx4).child(s.hint())),
                )
                .into_any_element()
        });
        div()
            .w(px(280.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(20.))
            .bg(th.ink1)
            .border_r_1()
            .border_color(th.line1)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(6.))
                    .pb(px(14.))
                    .child(brand_mark(th, 24.))
                    .child(display(19., FontWeight::EXTRA_BOLD, th.tx1).child("Quiver")),
            )
            .child(div().px(px(6.)).text_size(px(13.)).font_weight(SEMIBOLD).child("Welcome"))
            .child(div().px(px(6.)).pb(px(12.)).text_size(px(12.)).line_height(px(17.)).text_color(th.tx3).child("A few choices, each one skippable. Everything here can change later in Settings."))
            .children(items)
            .child(div().flex_1())
            .child(div().px(px(6.)).text_size(px(11.5)).text_color(th.tx4).pb(px(6.)).child("The same choices in a terminal:"))
            .child(command_line(th, "quiver setup"))
            .into_any_element()
    }

    fn welcome_footer(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let pad = self.mode == InputMode::Pad;
        let mode = self.wc.mode;
        let step = self.wc.step;
        let next_label = match (mode, step) {
            (Mode::Full, Step::Theme) => "Finish",
            (Mode::Full, Step::Fixes) if self.wc.fixes.is_none() => "Decide later",
            (Mode::Full, _) => "Continue",
            (Mode::FixesOnly, _) if self.wc.fixes.is_none() => "Ask me later",
            (Mode::FixesOnly | Mode::ImportOnly, _) => "Done",
        };
        let side = if self.bp().narrow() { 18. } else { 32. };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .px(px(side))
            .py(px(16.))
            .border_t_1()
            .border_color(th.line1)
            .bg(th.ink1.opacity(0.5));
        if mode == Mode::Full {
            row = row.child(
                button("wc-skip", th, Kind::Ghost, Size::Medium, None, "Skip setup")
                    .focus_ring(self.wfocused(WCtl::Skip), th, 8.)
                    .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::Skip, cx))),
            );
        }
        if pad {
            let hint = |g: Vec<&'static str>, label: &str| div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(th.tx3).children(g.into_iter().map(|g| glyph(th, g, false))).child(label.to_owned());
            row = row.child(hint(vec!["A"], "Select")).child(hint(vec!["B"], "Back"));
            if mode == Mode::Full {
                row = row.child(hint(vec!["LT", "RT"], "Steps"));
            }
        }
        row = row.child(div().flex_1());
        if mode == Mode::Full && step != Step::SignIn {
            row = row.child(
                button("wc-back", th, Kind::Secondary, Size::Medium, Some(Icon::ArrowLeft), "Back")
                    .focus_ring(self.wfocused(WCtl::Back), th, 8.)
                    .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::Back, cx))),
            );
        }
        row.child(
            button("wc-next", th, Kind::Primary, Size::Medium, (step == Step::Theme && mode == Mode::Full).then_some(Icon::Check), next_label)
                .font_weight(SEMIBOLD)
                .min_w(px(120.))
                .focus_ring(self.wfocused(WCtl::Next), th, 8.)
                .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::Next, cx)))
                .when(step != Step::Theme || mode != Mode::Full, |d| d.child(icon(Icon::ArrowRight, 15., th.acc_ink))),
        )
        .into_any_element()
    }

    fn welcome_body(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match self.wc.step {
            Step::SignIn => self.wc_signin(th, cx),
            Step::Storage => self.wc_storage(th, cx),
            Step::Import => self.wc_import(th, cx),
            Step::Steam => self.wc_steam(th, cx),
            Step::Controller => self.wc_controller(th),
            Step::Fixes => self.wc_fixes(th, cx),
            Step::Theme => self.wc_theme(th),
        }
    }

    // ---- Step 1

    fn wc_signin(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let accounts = self.status.as_ref().map(|s| s.accounts.clone()).unwrap_or_default();
        let rows = accounts.iter().enumerate().map(|(i, a)| {
            let (c1, c2) = super::settings::account_colors(i);
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(10.))
                .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                .child(super::avatar(&crate::model::initials(&a.name), c1, c2, th.ink2))
                .child(div().font_weight(SEMIBOLD).child(a.name.clone()))
                .when(a.default, |d| d.child(pill(th, PillKind::Accent, None, "Default")))
                .child(div().flex_1())
                .child(pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Signed in"))
                .into_any_element()
        });
        vec![
            panel(th).children(rows).into_any_element(),
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(12.))
                .child(
                    button("wc-add", th, Kind::Secondary, Size::Medium, Some(Icon::Plus), "Add another account")
                        .focus_ring(self.wfocused(WCtl::AddAccount), th, 8.)
                        .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::AddAccount, cx))),
                )
                .child(div().flex_1().min_w(px(200.)).text_size(px(12.)).text_color(th.tx3).child("Games from every account show up in one library; shared games launch with the default account."))
                .into_any_element(),
        ]
    }

    // ---- Step 2

    fn ensure_path_inputs(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.dl.setup.clone() else { return };
        if s.paths_locked || self.wc.lib_input.is_some() {
            return;
        }
        let mk = |path: &std::path::Path, cx: &mut Context<Self>| {
            let text = path.display().to_string();
            cx.new(|cx| {
                let mut t = TextInput::new(cx, "/path/to/folder", false).compact(true);
                t.set_text(text, cx);
                t
            })
        };
        self.wc.lib_input = Some(mk(&s.library, cx));
        self.wc.pre_input = Some(mk(&s.prefixes, cx));
    }

    fn wc_storage(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(s) = self.dl.setup.clone() else { return vec![loading_row(th, "Asking quiverd…")] };
        let folder = |key: &'static str, label: &str, help: &str, path: &std::path::Path, input: Option<Entity<TextInput>>, free: Option<u64>, total: Option<u64>| {
            let used = match (free, total) {
                (Some(f), Some(t)) if t > 0 => 1. - f as f32 / t as f32,
                _ => 0.,
            };
            let field: AnyElement = match input {
                Some(i) => div().flex_1().min_w_0().child(i).into_any_element(),
                None => div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_1()
                    .min_w_0()
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(px(8.))
                    .bg(th.ink1)
                    .border_1()
                    .border_color(th.line1)
                    .child(icon(Icon::Lock, 13., th.tx4))
                    .child(mono(12., th.tx2).truncate().child(tilde(path)))
                    .into_any_element(),
            };
            div()
                .id(key)
                .flex()
                .gap(px(16.))
                .p(px(16.))
                .rounded(px(12.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(div().flex().items_center().justify_center().flex_none().size(px(40.)).rounded(px(10.)).bg(th.ink4).child(icon(Icon::HardDrive, 18., th.tx2)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(8.))
                        .child(div().flex().items_baseline().gap(px(8.)).child(div().font_weight(SEMIBOLD).text_size(px(14.)).child(label.to_owned())).child(div().text_size(px(12.)).text_color(th.tx3).child(help.to_owned())))
                        .child(field)
                        .child(progress(th, used, None, 6., Some(if used > 0.9 { th.bad } else { th.acc })))
                        .child(div().text_size(px(12.)).text_color(th.tx3).child(match (free, total) {
                            (Some(f), Some(t)) => format!("{} free of {}", super::downloads::size(f), super::downloads::size(t)),
                            (Some(f), None) => format!("{} free", gb(f)),
                            _ => "Free space unknown".into(),
                        })),
                )
        };
        let mut v = vec![folder("wc-lib", "Games", "each game in its own folder", &s.library, self.wc.lib_input.clone(), s.library_free, s.library_total).into_any_element()];
        // The prefixes folder is a detail most people never need: behind "Advanced".
        let open = self.wc.advanced;
        v.push(
            div()
                .id("wc-adv")
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(4.))
                .py(px(2.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(th.tx2)
                .child(icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }, 14., th.tx3))
                .child(div().font_weight(MEDIUM).child("Advanced"))
                .child(div().text_color(th.tx3).child("Windows compatibility folders"))
                .focus_ring(self.wfocused(WCtl::Advanced), th, 6.)
                .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::Advanced, cx)))
                .into_any_element(),
        );
        if open {
            v.push(
                folder(
                    "wc-pre",
                    "Compatibility folders",
                    "one per game (Proton prefixes)",
                    &s.prefixes,
                    self.wc.pre_input.clone(),
                    s.prefixes_free,
                    s.prefixes_total,
                )
                .into_any_element(),
            );
            v.push(
                div()
                    .px(px(4.))
                    .text_size(px(12.))
                    .line_height(px(17.))
                    .text_color(th.tx3)
                    .child("Each game runs in its own small Windows setup: settings, Ubisoft's files and anything else a Windows game expects. Quiver keeps one per game, so games never affect each other. Leave this as it is unless you want them on another disk.")
                    .into_any_element(),
            );
        }
        if s.paths_locked {
            v.push(note(
                th,
                Icon::Lock,
                &if s.installed > 0 {
                    format!("{} game{} installed here already, so these folders stay as they are. Moving installs comes later.", s.installed, if s.installed == 1 { " is" } else { "s are" })
                } else {
                    "Something is downloading or importing, so these folders stay as they are for now.".to_owned()
                },
            ));
        } else {
            v.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        button("wc-apply", th, Kind::Secondary, Size::Medium, Some(Icon::Check), if self.wc.restarting { "Restarting…" } else if open { "Use these folders" } else { "Use this folder" })
                            .focus_ring(self.wfocused(WCtl::ApplyPaths), th, 8.)
                            .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::ApplyPaths, cx))),
                    )
                    .child(div().flex_1().text_size(px(12.)).text_color(th.tx3).child("Nothing is installed yet, so changing this moves nothing. Quiver's background service restarts to use it."))
                    .into_any_element(),
            );
        }
        v.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(th.tx3)
                .child(icon(Icon::Info, 13., th.tx3))
                .child(div().flex_1().min_w(px(200.)).child("Quiver's own state (accounts, settings history, save backups) stays in"))
                .child(mono(11.5, th.tx2).child(ubi_api::client::daemon_log().parent().map(tilde).unwrap_or_default()))
                .into_any_element(),
        );
        v
    }

    // ---- Step 3

    fn wc_import(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let found = match &self.wc.scan {
            None => {
                return vec![loading_row(th, "Looking for Ubisoft Connect downloads in Lutris, ~/Games and Steam games (read-only)…")];
            }
            Some(Err(e)) => {
                return vec![note(th, Icon::CircleX, e), self.rescan_button(th, cx)];
            }
            Some(Ok(v)) => v.clone(),
        };
        let count = |s: InstallSource| found.iter().filter(|f| f.source == s).count();
        let prefixes = |s: InstallSource| {
            let mut p: Vec<&PathBuf> = found.iter().filter(|f| f.source == s).map(|f| &f.prefix).collect();
            p.dedup();
            p.len()
        };
        // One per row when three don't fit.
        let stack = self.wc.cw < WC_ROW_MIN;
        let third = if stack { self.wc.cw } else { (self.wc.cw - 24.) / 3. };
        let source_card = |i: Icon, name: &str, detail: String, n: usize| {
            div()
                .flex()
                .w(px(third))
                .items_center()
                .gap(px(12.))
                .p(px(12.))
                .rounded(px(12.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(div().flex().items_center().justify_center().flex_none().size(px(36.)).rounded(px(10.)).bg(th.ink4).child(icon(i, 17., th.tx2)))
                .child(div().flex().flex_col().flex_1().min_w_0().gap(px(1.)).child(div().font_weight(SEMIBOLD).child(name.to_owned())).child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(detail)))
                .child(pill(th, if n > 0 { PillKind::Accent } else { PillKind::Muted }, None, format!("{n} found")))
        };
        let lutris_names: Vec<String> = {
            let mut v: Vec<String> = found.iter().filter(|f| f.source == InstallSource::Lutris).flat_map(|f| f.lutris_games.clone()).collect();
            v.sort();
            v.dedup();
            v
        };
        let sources = div()
            .flex()
            .when(stack, |d| d.flex_col())
            .gap(px(12.))
            .child(source_card(
                Icon::Layers,
                "Lutris",
                if lutris_names.is_empty() { "games that run Ubisoft Connect".into() } else { lutris_names.join(", ") },
                count(InstallSource::Lutris),
            ))
            .child(source_card(Icon::FolderOpen, "Wine", format!("~/Games · {} folder{}", prefixes(InstallSource::Wine), if prefixes(InstallSource::Wine) == 1 { "" } else { "s" }), count(InstallSource::Wine)))
            .child(source_card(Icon::Gamepad, "Inside Steam", "Connect in a Steam game".into(), count(InstallSource::Steam)));

        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, f) in found.iter().enumerate() {
            rows.push(self.found_row(th, i, f, cx));
        }
        if rows.is_empty() {
            rows.push(div().p(px(18.)).text_color(th.tx3).child("No Ubisoft Connect downloads found. Games you install with Quiver go straight into your library.").into_any_element());
        }
        vec![
            sources.into_any_element(),
            panel(th).children(rows).into_any_element(),
            div()
                .flex()
                .items_start()
                .gap(px(14.))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .items_start()
                        .gap(px(10.))
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(th.tx3)
                        .child(div().pt(px(1.)).child(icon(Icon::ShieldCheck, 14., th.ok)))
                        .child("Importing copies only the files the game's manifest lists, as reflinks: instant and free of extra space on btrfs and xfs. The original install is never changed, and anything else in its folder (mods, DLSS swaps, configs) is listed afterwards, not copied."),
                )
                .child(self.rescan_button(th, cx))
                .into_any_element(),
        ]
    }

    fn rescan_button(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        button("wc-rescan", th, Kind::Secondary, Size::Small, Some(Icon::RefreshCw), if self.wc.scanning { "Scanning…" } else { "Scan again" })
            .focus_ring(self.wfocused(WCtl::Rescan), th, 6.)
            .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::Rescan, cx)))
            .into_any_element()
    }

    fn found_row(&mut self, th: &Theme, i: usize, f: &FoundInstall, cx: &mut Context<Self>) -> AnyElement {
        let g = f.product_id.and_then(|id| self.game(id)).cloned();
        let art = match &g {
            Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Tile, cx),
            None => ArtState::Failed,
        };
        let job = self.import_job(&f.path);
        let mut facts = vec![format!("{} · {} files", gb(f.size_bytes), thousands(f.files))];
        if let Some(v) = &f.version {
            facts.push(format!("version {v}"));
        }
        if let Some(l) = &f.language {
            facts.push(l.clone());
        }
        // The Lutris entry that launches this game, if there is one ("Origins (DLSS)").
        let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect::<String>();
        let launcher = f.lutris_games.iter().find(|g| norm(g).contains(&norm(&clean_name(&f.name))));
        let source = match launcher {
            Some(g) => format!("{} · in Lutris as “{g}”", f.source_label),
            None => f.source_label.clone(),
        };
        let right: AnyElement = match &job {
            Some(j) if !j.done => div()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(5.))
                .w(px(190.))
                .child(div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(th.acc_hi).child(icon(Icon::LoaderCircle, 13., th.acc_hi)).child(j.status.clone()))
                .child(progress(th, import_fraction(&j.status), Some(190.), 5., None))
                .into_any_element(),
            Some(j) if j.error.is_some() => div()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(3.))
                .max_w(px(260.))
                .child(pill(th, PillKind::Bad, Some(Icon::CircleX), "Import failed"))
                .child(div().text_size(px(11.5)).text_color(th.bad).truncate().child(j.error.clone().unwrap_or_default()))
                .into_any_element(),
            Some(j) => div()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(3.))
                .max_w(px(300.))
                .child(pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Imported"))
                .when(!j.detail.is_empty(), |d| {
                    let shown: Vec<String> = j.detail.iter().take(3).map(|p| { let p = p.trim_end_matches('/'); p.rsplit('/').next().unwrap_or(p).to_owned() }).collect();
                    let more = j.detail.len().saturating_sub(3);
                    d.child(div().text_size(px(11.5)).text_color(th.warn).truncate().child(format!(
                        "Left in place: {}{}",
                        shown.join(", "),
                        if more > 0 { format!(" +{more}") } else { String::new() }
                    )))
                })
                .into_any_element(),
            None if f.importable => button(("wc-imp", i), th, Kind::Primary, Size::Small, Some(Icon::FolderInput), "Import")
                .focus_ring(self.wfocused(WCtl::Import(i)), th, 6.)
                .on_click(cx.listener(move |this, _, _, cx| this.welcome_press(WCtl::Import(i), cx)))
                .into_any_element(),
            None if f.installed => pill(th, PillKind::Ok, Some(Icon::CircleCheck), "In Quiver").into_any_element(),
            None => pill(th, PillKind::Muted, None, if f.product_id.is_none() { "Not matched" } else { "Can't import" }).into_any_element(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(14.))
            .px(px(14.))
            .py(px(11.))
            .when(i > 0, |d| d.border_t_1().border_color(th.line1))
            .child(div().w(px(34.)).h(px(40.)).flex_none().child(art_fill(th, &art, "", 5., g.is_none())))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().font_weight(SEMIBOLD).truncate().child(clean_name(&f.name)))
                            .when_some(f.product_id, |d, id| d.child(mono(11., th.tx4).child(id.to_string()))),
                    )
                    .child(div().text_size(px(12.)).text_color(th.tx3).truncate().child(source))
                    .child(mono(11., th.tx4).truncate().child(short_path(f)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(11.5))
                            .text_color(th.tx3)
                            .child(facts.join(" · "))
                            .when(job.is_none(), |d| d.when_some(f.note.clone(), |d, n| d.child(div().text_color(if f.installed { th.tx3 } else { th.warn }).truncate().child(format!("· {n}"))))),
                    ),
            )
            .child(right)
            .into_any_element()
    }

    // ---- Step 4

    fn wc_steam(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let info = match &self.ui.steam {
            None => return vec![loading_row(th, "Looking for Steam…")],
            Some(Err(e)) => return vec![note(th, Icon::CircleX, e)],
            Some(Ok(s)) => s.clone(),
        };
        if info.root.is_none() {
            return vec![note(th, Icon::Info, "Steam isn't installed (no ~/.local/share/Steam). Skip this step; you can add games to Steam later from each game's page.")];
        }
        let on = self.wc.steam_on_install.unwrap_or(true);
        let user = info
            .users
            .iter()
            .find(|u| Some(&u.id) == info.selected.as_ref())
            .map(|u| u.persona_name.clone().unwrap_or_else(|| u.id.clone()));
        // 1. Ubisoft games bought on Steam: start them through Quiver.
        let mut v = self.wc_steam_copies(th, cx);
        // 2. Shortcuts for games Quiver installs.
        v.push(
            div()
                .pt(px(10.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(caption("Shortcuts for games Quiver installs", th.tx3))
                .child(div().text_size(px(12.5)).text_color(th.tx2).child("Games you install with Quiver can show up in your Steam library, with Ubisoft's art, for Big Picture and the Steam Deck UI."))
                .into_any_element(),
        );
        v.push(
            panel(th)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(12.))
                        .child(icon(Icon::Gamepad, 16., th.tx2))
                        .child(div().flex().flex_col().flex_1().gap(px(1.)).child(div().font_weight(SEMIBOLD).child("Steam")).child(div().text_size(px(12.)).text_color(th.tx3).child(match &user {
                            Some(u) => format!("Shortcuts go to {u}"),
                            None if info.users.len() > 1 => format!("{} Steam users: pick one below", info.users.len()),
                            None => "No Steam user yet".into(),
                        })))
                        .child(if info.running { pill(th, PillKind::Warn, None, "Running") } else { pill(th, PillKind::Muted, None, "Not running") }),
                )
                .child(
                    div()
                        .id("wc-steam-on")
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(12.))
                        .border_t_1()
                        .border_color(th.line1)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamOnInstall, cx)))
                        .child(div().flex().flex_col().flex_1().gap(px(1.)).child(div().font_weight(SEMIBOLD).child("Offer “Add to Steam” when an install finishes")).child(div().text_size(px(12.)).text_color(th.tx3).child("Ticked in the install dialog; untick it per game")))
                        .child(div().rounded(px(9.)).child(toggle(th, on)).focus_ring(self.wfocused(WCtl::SteamOnInstall), th, 9.)),
                )
                .into_any_element(),
        );
        let choice = self.steam_choice();
        if !choice.is_empty() {
            let chips = div().flex().gap(px(8.)).children(choice.iter().enumerate().map(|(i, (id, name))| {
                let on = self.wc.steam_user.as_deref() == Some(id.as_str());
                let (bg, border, fg) = if on { (th.acc_soft, th.acc_line, th.acc_hi) } else { (th.ink3, th.line2, th.tx2) };
                div()
                    .id(("wc-su", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(30.))
                    .px(px(11.))
                    .rounded(px(8.))
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .text_color(fg)
                    .cursor_pointer()
                    .child(icon(Icon::User, 13., fg))
                    .child(name.clone())
                    .focus_ring(self.wfocused(WCtl::SteamUser(i)), th, 8.)
                    .on_click(cx.listener(move |this, _, _, cx| this.welcome_press(WCtl::SteamUser(i), cx)))
                    .into_any_element()
            }));
            v.push(div().flex().items_center().gap(px(12.)).child(caption("Which Steam user?", th.tx3)).child(chips).into_any_element());
        }
        let games = self.steam_games();
        let missing = self.steam_missing();
        let rows: Vec<AnyElement> = games
            .iter()
            .enumerate()
            .map(|(i, (id, name, st))| {
                let id = *id;
                let busy = self.wc.steam_busy.contains(&id);
                let right: AnyElement = match st {
                    _ if busy => pill(th, PillKind::Muted, Some(Icon::LoaderCircle), "Adding…").into_any_element(),
                    Some(ExportState::Ok) => pill(th, PillKind::Ok, Some(Icon::CircleCheck), "In Steam").into_any_element(),
                    Some(ExportState::WaitingForSteam) => pill(th, PillKind::Warn, Some(Icon::Clock), "Shows after Steam restarts").into_any_element(),
                    _ => button(("wc-sa", id as usize), th, Kind::Secondary, Size::Small, Some(Icon::Plus), "Add")
                        .focus_ring(self.wfocused(WCtl::SteamAdd(id)), th, 6.)
                        .on_click(cx.listener(move |this, _, _, cx| this.welcome_press(WCtl::SteamAdd(id), cx)))
                        .into_any_element(),
                };
                let g = self.game(id).cloned();
                let art = match &g {
                    Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Tile, cx),
                    None => ArtState::Failed,
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(8.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .child(div().w(px(26.)).h(px(30.)).flex_none().child(art_fill(th, &art, "", 4., false)))
                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                    .child(right)
                    .into_any_element()
            })
            .collect();
        v.push(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(caption("Installed games", th.tx3))
                .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{} of {} not in Steam", missing.len(), games.len())))
                .child(div().flex_1())
                .when(missing.len() > 1, |d| {
                    d.child(
                        button("wc-sall", th, Kind::Secondary, Size::Small, Some(Icon::Plus), format!("Add all {}", missing.len()))
                            .focus_ring(self.wfocused(WCtl::SteamAddAll), th, 6.)
                            .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamAddAll, cx))),
                    )
                })
                .into_any_element(),
        );
        if rows.is_empty() {
            v.push(div().text_color(th.tx3).text_size(px(12.5)).child("Nothing installed yet. New installs offer it when they finish.").into_any_element());
        } else {
            v.push(panel(th).children(rows).into_any_element());
        }
        if self.steam_waiting() && self.steam_running() {
            v.push(self.wc_steam_restart(th, cx));
        } else {
            v.push(note(
                th,
                Icon::RefreshCw,
                "Steam reads shortcuts and launch options only when it starts. While it runs, changes wait for its next restart. Quiver never restarts Steam on its own: it asks you every time.",
            ));
        }
        v
    }

    /// "Ubisoft games in Steam": each one, and starting it through Quiver.
    fn wc_steam_copies(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = vec![];
        let list = match &self.ui.sg.list {
            None => return vec![loading_row(th, "Looking for Ubisoft games in Steam…")],
            Some(Err(e)) => return vec![note(th, Icon::CircleX, e)],
            Some(Ok(s)) => s.clone(),
        };
        let switchable = self.steam_switchable();
        let using = list.games.iter().filter(|g| g.state.uses_quiver()).count();
        v.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(caption("Ubisoft games in Steam", th.tx3))
                .when(!list.games.is_empty(), |d| {
                    d.child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{using} of {} start through Quiver", list.games.len())))
                })
                .child(div().flex_1())
                .when(switchable.len() > 1, |d| {
                    d.child(
                        button("wc-sg-all", th, Kind::Secondary, Size::Small, Some(Icon::Check), format!("Use Quiver for all {}", switchable.len()))
                            .focus_ring(self.wfocused(WCtl::SteamPatchAll), th, 6.)
                            .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamPatchAll, cx))),
                    )
                })
                .into_any_element(),
        );
        v.push(
            div()
                .text_size(px(12.5))
                .line_height(px(18.))
                .text_color(th.tx2)
                .child("Started through Quiver, they skip the Ubisoft Connect window and its second sign-in, and your play time still counts. Nothing in the game changes, and you can undo it any time.")
                .into_any_element(),
        );
        let mut rows: Vec<AnyElement> = vec![];
        for (i, g) in list.games.iter().enumerate() {
            let busy = self.ui.sg.busy.contains(&g.app_id);
            let lib = g.product_id.and_then(|p| self.game(p)).cloned();
            let art = match &lib {
                Some(l) => self.art_for(l, ArtSlot::Tile, Variant::Tile, cx),
                None => ArtState::Failed,
            };
            let id = g.app_id;
            let right: AnyElement = match steam_copy_control(g) {
                _ if busy => super::steamgames::state_pill(th, g, true),
                Some(WCtl::SteamPatch(_)) => button(("wc-sg", id as usize), th, Kind::Primary, Size::Small, Some(Icon::Check), "Use Quiver")
                    .focus_ring(self.wfocused(WCtl::SteamPatch(id)), th, 6.)
                    .on_click(cx.listener(move |this, _, _, cx| this.welcome_press(WCtl::SteamPatch(id), cx)))
                    .into_any_element(),
                Some(WCtl::SteamUndo(_)) => div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(super::steamgames::state_pill(th, g, false))
                    .child(
                        button(("wc-sgu", id as usize), th, Kind::Ghost, Size::Small, Some(Icon::Undo), "Undo")
                            .focus_ring(self.wfocused(WCtl::SteamUndo(id)), th, 6.)
                            .on_click(cx.listener(move |this, _, _, cx| this.welcome_press(WCtl::SteamUndo(id), cx))),
                    )
                    .into_any_element(),
                _ => super::steamgames::state_pill(th, g, false),
            };
            rows.push(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(12.))
                    .gap_y(px(6.))
                    .px(px(14.))
                    .py(px(8.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .child(div().w(px(26.)).h(px(30.)).flex_none().child(art_fill(th, &art, "", 4., lib.is_none())))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(160.))
                            .gap(px(1.))
                            .child(div().truncate().child(clean_name(&g.name)))
                            .child(div().text_size(px(11.5)).text_color(if g.note.is_some() && !g.patchable { th.warn } else { th.tx3 }).truncate().child(super::steamgames::state_line(g))),
                    )
                    .child(right)
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px(px(14.))
                    .py(px(12.))
                    .text_size(px(12.5))
                    .text_color(th.tx3)
                    .child(if list.offers {
                        "No Ubisoft games are installed through Steam. If you install one later, Quiver offers this then."
                    } else {
                        "No Ubisoft games are installed through Steam."
                    })
                    .into_any_element(),
            );
        }
        let offers = list.offers;
        rows.push(
            div()
                .id("wc-sg-offers")
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(10.))
                .border_t_1()
                .border_color(th.line1)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamOffers, cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(px(1.))
                        .child(div().font_weight(SEMIBOLD).child("Ask about games installed later"))
                        .child(div().text_size(px(12.)).text_color(th.tx3).child("When a Ubisoft game is installed through Steam, Quiver offers this once")),
                )
                .child(div().rounded(px(9.)).child(toggle(th, offers)).focus_ring(self.wfocused(WCtl::SteamOffers), th, 9.))
                .into_any_element(),
        );
        v.push(panel(th).children(rows).into_any_element());
        v
    }

    /// Steam is running with changes waiting: offer (and confirm) a restart.
    fn wc_steam_restart(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ask = self.wc.restart_ask;
        let mut b = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(10.))
            .bg(th.warn_soft)
            .border_1()
            .border_color(th.warn.opacity(0.3))
            .text_size(px(12.5))
            .line_height(px(18.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .child(div().pt(px(1.)).child(icon(Icon::RefreshCw, 14., th.warn)))
                    .child(div().flex_1().min_w_0().text_color(th.tx2).child(if ask {
                        "Close Steam and start it again now? Quiver closes it cleanly and writes the changes while it's closed. Close any game you started from Steam first."
                    } else {
                        "Steam is running, so these changes wait for it to restart: Steam only reads them when it starts. Quiver won't restart Steam unless you ask."
                    })),
            );
        let row = div().flex().justify_end().gap(px(8.));
        b = b.child(if ask {
            row.child(
                button("wc-rs-no", th, Kind::Secondary, Size::Small, None, "Not now")
                    .focus_ring(self.wfocused(WCtl::SteamRestartNo), th, 6.)
                    .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamRestartNo, cx))),
            )
            .child(
                button("wc-rs-yes", th, Kind::Primary, Size::Small, Some(Icon::RefreshCw), "Restart Steam")
                    .focus_ring(self.wfocused(WCtl::SteamRestartYes), th, 6.)
                    .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamRestartYes, cx))),
            )
        } else {
            row.child(
                button("wc-rs", th, Kind::Secondary, Size::Small, Some(Icon::RefreshCw), "Restart Steam…")
                    .focus_ring(self.wfocused(WCtl::SteamRestart), th, 6.)
                    .on_click(cx.listener(|this, _, _, cx| this.welcome_press(WCtl::SteamRestart, cx))),
            )
        });
        b.into_any_element()
    }

    // ---- Step 5

    fn wc_controller(&mut self, th: &Theme) -> Vec<AnyElement> {
        let hyprpad = self.hyprpad_running();
        let pad = self.pad.clone();
        let status = |i: Icon, label: &str, ok: bool, on: &'static str, off: &'static str, detail: String| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(12.))
                .child(div().flex().items_center().justify_center().size(px(34.)).rounded(px(9.)).bg(if ok { th.ok_soft } else { th.ink4 }).child(icon(i, 16., if ok { th.ok } else { th.tx3 })))
                .child(div().flex().flex_col().flex_1().min_w_0().gap(px(1.)).child(div().font_weight(SEMIBOLD).child(label.to_owned())).child(div().text_size(px(12.)).text_color(th.tx3).child(detail)))
                .child(if ok { pill(th, PillKind::Ok, Some(Icon::CircleCheck), on) } else { pill(th, PillKind::Muted, None, off) })
        };
        let map: [(&[&'static str], &str); 8] = [
            (&["A"], "Select, or play"),
            (&["B"], "Back"),
            (&["X"], "Play options (profile, launch once with…)"),
            (&["Y"], "Menu for the focused game"),
            (&["LB", "RB"], "Library · Downloads · Activity"),
            (&["LT", "RT"], "Tabs, sections, filters"),
            (&["≡"], "Command palette (Start)"),
            (&["R"], "Right stick scrolls"),
        ];
        let grid = div().flex().flex_wrap().gap(px(10.)).children(map.iter().map(|(g, label)| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .w(px(if self.wc.cw < WC_ROW_MIN { self.wc.cw } else { (self.wc.cw - 10.) / 2. }))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(10.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(div().flex().gap(px(4.)).w(px(60.)).children(g.iter().map(|g| glyph(th, g, false))))
                .child(div().flex_1().min_w_0().text_size(px(12.5)).text_color(th.tx2).child(label.to_string()))
        }));
        vec![
            panel(th)
                .child(status(
                    Icon::Gamepad,
                    "Gamepad",
                    pad.is_some(),
                    "Connected",
                    "None seen",
                    pad.clone().unwrap_or_else(|| "Plug one in or turn it on; Quiver notices it on its own".into()),
                ))
                .child(div().border_t_1().border_color(th.line1).child(status(
                    Icon::Command,
                    "hyprpad",
                    hyprpad,
                    "Running",
                    "Not running",
                    if hyprpad { "It forwards the pad to Quiver while Quiver's window has focus".into() } else { "Optional: Quiver reads the pad itself either way".into() },
                )))
                .into_any_element(),
            div().pt(px(4.)).child(caption("Buttons", th.tx3)).into_any_element(),
            grid.into_any_element(),
            note(th, Icon::Info, "Hints like these appear when you use a pad and hide again on mouse or keyboard input. The Guide button is never used: hyprpad keeps it for desktop chords. Settings › Appearance has the details."),
        ]
    }

    // ---- Step 6

    fn wc_fixes(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let sel = self.wc.fixes;
        let stack = self.wc.cw < WC_ROW_MIN;
        let third = if stack { self.wc.cw } else { (self.wc.cw - 20.) / 3. };
        let example = |i: Icon, t: &str, d: &str| {
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .w(px(third))
                .p(px(12.))
                .rounded(px(10.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(div().pt(px(1.)).child(icon(i, 15., th.acc_hi)))
                .child(div().flex().flex_col().flex_1().min_w_0().gap(px(2.)).child(div().font_weight(SEMIBOLD).text_size(px(12.5)).child(t.to_owned())).child(div().text_size(px(12.)).line_height(px(17.)).text_color(th.tx3).child(d.to_owned())))
        };
        let option = |c: FixesChoice, title: &str, d: &str, cx: &mut Context<Self>| {
            let on = sel == Some(c);
            div()
                .id(match c {
                    FixesChoice::Yes => "fix-yes",
                    FixesChoice::No => "fix-no",
                    FixesChoice::Later => "fix-later",
                })
                .flex()
                .items_start()
                .gap(px(12.))
                .w(px(third))
                .p(px(14.))
                .rounded(px(12.))
                .bg(if on { th.acc_soft } else { th.ink1 })
                .border_1()
                .border_color(if on { th.acc_line } else { th.line1 })
                .cursor_pointer()
                .hover(|d| d.border_color(th.line2))
                .child(
                    div()
                        .mt(px(1.))
                        .size(px(18.))
                        .flex_none()
                        .rounded_full()
                        .border(px(if on { 5. } else { 1.5 }))
                        .border_color(if on { th.acc } else { th.tx4 }),
                )
                .child(div().flex().flex_col().flex_1().min_w_0().gap(px(3.)).child(div().font_weight(SEMIBOLD).child(title.to_owned())).child(div().text_size(px(12.)).line_height(px(17.)).text_color(th.tx3).child(d.to_owned())))
                .focus_ring(self.wfocused(WCtl::Fix(c)), th, 12.)
                .on_click(cx.listener(move |this, _, _, cx| this.welcome_press(WCtl::Fix(c), cx)))
        };
        vec![
            div()
                .flex()
                .when(stack, |d| d.flex_col())
                .gap(px(10.))
                .child(example(Icon::Play, "Skip intro videos", "No more logo reels and studio intros before the menu."))
                .child(example(Icon::MousePointer, "Mouse fixes", "Raw input, no forced acceleration or smoothing, where a game needs it."))
                .child(example(Icon::BookOpen, "Audited, with a source", "Each one comes from PCGamingWiki, checked before it becomes part of Quiver."))
                .into_any_element(),
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(10.))
                .bg(th.ok_soft)
                .text_size(px(12.5))
                .text_color(th.ok)
                .child(icon(Icon::ShieldCheck, 15., th.ok))
                .child(div().flex_1().min_w_0().child("They never change gameplay or your saves, and nothing is downloaded from third parties."))
                .into_any_element(),
            div()
                .flex()
                .when(stack, |d| d.flex_col())
                .gap(px(10.))
                .child(option(FixesChoice::Yes, "Yes, apply them", "Unmodded games get the recommended fixes. Mod profiles choose their own.", cx))
                .child(option(FixesChoice::No, "No, keep games as shipped", "Unmodded games run exactly as Ubisoft ships them.", cx))
                .child(option(FixesChoice::Later, "Ask me later", "Nothing changes now; Quiver asks again next time.", cx))
                .into_any_element(),
            div().text_size(px(12.)).text_color(th.tx3).child("Change it any time in Settings › Library and storage, or with `quiver setup`.").into_any_element(),
        ]
    }

    // ---- Step 7

    fn wc_theme(&mut self, th: &Theme) -> Vec<AnyElement> {
        let builtin = th.name.as_ref() == "Quiver dark";
        let source = tilde(&crate::theme::omarchy_state_dir().join("theme/colors.toml"));
        let sw = |c: gpui::Hsla, l: &str| div().flex().flex_col().gap(px(4.)).items_center().child(div().w(px(44.)).h(px(32.)).rounded(px(7.)).bg(c).border_1().border_color(th.line2)).child(mono(10., th.tx3).child(l.to_owned()));
        let preview = panel(th)
            .p(px(18.))
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.))
                    .child(icon(Icon::Palette, 18., th.acc_hi))
                    .child(div().text_size(px(16.)).font_weight(BOLD).child(th.name.to_string()))
                    .child(pill(th, PillKind::Muted, None, if th.dark { "dark" } else { "light" }))
                    .child(div().flex_1())
                    .child(if builtin { pill(th, PillKind::Muted, None, "Built-in palette") } else { pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Following Omarchy, live") }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(10.))
                    .child(sw(th.ink0, "ink-0"))
                    .child(sw(th.ink2, "ink-2"))
                    .child(sw(th.ink4, "ink-4"))
                    .child(sw(th.tx1, "text"))
                    .child(sw(th.acc, "accent"))
                    .child(sw(th.ok, "ok"))
                    .child(sw(th.warn, "warn"))
                    .child(sw(th.bad, "bad")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .child(button("wc-t1", th, Kind::Primary, Size::Medium, Some(Icon::Play), "Play"))
                    .child(button("wc-t2", th, Kind::Secondary, Size::Medium, None, "Game details"))
                    .child(pill(th, PillKind::Ok, None, "In sync"))
                    .child(pill(th, PillKind::Warn, None, "Update"))
                    .child(who(th, "gui"))
                    .child(who(th, "claude-code"))
                    .child(div().flex_1())
                    .child(div().w(px(160.)).child(progress(th, 0.64, None, 6., None))),
            )
            .child(div().flex().items_center().gap(px(8.)).text_size(px(12.)).text_color(th.tx3).child("Source").child(mono(11.5, th.tx2).flex_1().min_w_0().truncate().child(source)));
        let mut v = vec![
            preview.into_any_element(),
            note(
                th,
                Icon::Info,
                if builtin {
                    "No Omarchy theme was found, so Quiver uses its built-in palette. When Omarchy is there, Quiver follows it."
                } else {
                    "Switch themes in Omarchy and Quiver restyles itself at once. Nothing to set here."
                },
            ),
        ];
        v.push(div().pt(px(6.)).child(caption("Your choices", th.tx3)).into_any_element());
        v.push(self.welcome_summary(th));
        v
    }

    fn welcome_summary(&self, th: &Theme) -> AnyElement {
        let s = self.dl.setup.clone();
        let accounts: Vec<String> = self.status.as_ref().map(|s| s.accounts.iter().map(|a| a.name.clone()).collect()).unwrap_or_default();
        let imported = self.wc.imports.keys().filter(|p| self.import_job(p).is_some_and(|j| j.done && j.error.is_none())).count();
        let row = |k: &str, v: String| div().flex().items_center().gap(px(10.)).px(px(14.)).py(px(8.)).text_size(px(12.5)).child(div().w(px(150.)).text_color(th.tx3).child(k.to_owned())).child(div().text_color(th.tx1).truncate().child(v));
        panel(th)
            .child(row("Signed in", if accounts.is_empty() { "No account".into() } else { accounts.join(", ") }))
            .child(row("Games go to", s.as_ref().map(|s| tilde(&s.library)).unwrap_or_default()))
            .child(row("Imported", if imported == 0 { "Nothing".into() } else { format!("{imported} game{}", if imported == 1 { "" } else { "s" }) }))
            .child(row("Add to Steam", if self.wc.steam_on_install.unwrap_or(true) { "Offered when installs finish".into() } else { "Not offered".into() }))
            .child(row(
                "Recommended fixes",
                match self.wc.fixes {
                    Some(FixesChoice::Yes) => "On for unmodded games".into(),
                    Some(FixesChoice::No) => "Off: games run as shipped".into(),
                    _ => "Ask me later".into(),
                },
            ))
            .into_any_element()
    }

    /// Settings › Library and storage: the setup choices and "run it again".
    pub fn settings_setup_group(&mut self, th: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let s = self.dl.setup.clone();
        let when = s.as_ref().and_then(|s| s.first_run.as_ref()).map(|f| {
            format!("{} {}", if f.skipped { "Skipped" } else { "Done" }, crate::model::day(f.at))
        });
        let fixes = match s.as_ref().and_then(|s| s.fixes_on_vanilla) {
            Some(true) => "On for unmodded games",
            Some(false) => "Off: games run as shipped",
            None => "Not decided yet",
        };
        super::card(th)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_b_1()
                    .border_color(th.line1)
                    .child(div().text_size(px(13.5)).font_weight(SEMIBOLD).child("Setup"))
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(when.unwrap_or_else(|| "Not done yet".into())))
                    .child(div().flex_1())
                    .child(
                        button("set-welcome", th, Kind::Secondary, Size::Small, Some(Icon::Sparkles), "Run setup again")
                            .focus_ring(focused, th, 6.)
                            .on_click(cx.listener(|this, _, _, cx| this.run_dl(super::downloads::DlCmd::Welcome, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(12.))
                    .text_size(px(12.5))
                    .child(div().w(px(180.)).text_color(th.tx2).child("Recommended fixes"))
                    .child(div().flex_1().min_w(px(140.)).text_color(th.tx1).child(fixes))
                    .child(command_line(th, "quiver setup").w(px(160.))),
            )
            .into_any_element()
    }
}

/// The button a Ubisoft game in Steam gets: switch to Quiver, or undo.
fn steam_copy_control(g: &ubi_api::exports::SteamGame) -> Option<WCtl> {
    super::steamgames::control(g).map(|on| if on { WCtl::SteamPatch(g.app_id) } else { WCtl::SteamUndo(g.app_id) })
}

/// Copy progress from an import job's status ("Copying files 51/1204…").
fn import_fraction(status: &str) -> f32 {
    let nums: Vec<f32> = status
        .split(|c: char| !c.is_ascii_digit() && c != '/')
        .find(|p| p.contains('/'))
        .map(|p| p.split('/').filter_map(|n| n.parse().ok()).collect())
        .unwrap_or_default();
    match nums.as_slice() {
        [a, b] if *b > 0. => (a / b).clamp(0., 1.),
        _ => 0.05,
    }
}

/// "3,412".
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A found install's folder, short: `~/Games/ubisoft-connect › games/Far Cry 5` for the
/// usual Connect layout.
fn short_path(f: &FoundInstall) -> String {
    let connect = f.prefix.join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher");
    match f.path.strip_prefix(&connect) {
        Ok(rest) => format!("{} › {}", tilde(&f.prefix), rest.display()),
        Err(_) => tilde(&f.path),
    }
}

fn panel(th: &Theme) -> gpui::Div {
    div().flex().flex_col().rounded(px(12.)).bg(th.ink1).border_1().border_color(th.line1).overflow_hidden()
}

fn note(th: &Theme, i: Icon, text: &str) -> AnyElement {
    div()
        .flex()
        .items_start()
        .gap(px(10.))
        .px(px(14.))
        .py(px(11.))
        .rounded(px(10.))
        .bg(th.ink3)
        .border_1()
        .border_color(th.line1)
        .text_size(px(12.5))
        .line_height(px(18.))
        .text_color(th.tx2)
        .child(div().pt(px(1.)).child(icon(i, 14., th.tx2)))
        .child(div().flex_1().min_w_0().child(text.to_owned()))
        .into_any_element()
}

fn loading_row(th: &Theme, text: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .p(px(16.))
        .rounded(px(10.))
        .bg(th.ink1)
        .text_color(th.tx3)
        .child(icon(Icon::LoaderCircle, 14., th.tx3))
        .child(text.to_owned())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_progress() {
        assert_eq!(Step::Storage.step(-1), Step::SignIn);
        assert_eq!(Step::Theme.step(1), Step::Theme);
        assert_eq!(Step::ALL.len(), 7);
        assert!((import_fraction("Copying files 51/204…") - 0.25).abs() < 0.01);
        assert_eq!(import_fraction("Reading the manifest…"), 0.05);
        assert_eq!(thousands(3412), "3,412");
        assert_eq!(thousands(412), "412");
        assert_eq!(thousands(1_204_000), "1,204,000");
    }
}
