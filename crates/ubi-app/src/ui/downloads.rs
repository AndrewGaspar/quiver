//! Downloads: the page (what is downloading, the queue, failures, what finished, storage
//! and download rules) and the install, update and verify dialogs, all on the daemon's
//! queue API (`ubi_api::downloads`).
//!
//! `app.rs` calls in through a few hooks (`start_downloads`, `downloads_event`,
//! `downloads_status`, `downloads_input`, `downloads_activate`, `render_dl_overlay`), and
//! the welcome flow (`welcome.rs`) shares the 1 s tick.
//!
//! Focus: every row of the page is one body row of [`crate::nav::Nav`]; Left/Right pick a
//! button within the row ([`DlState::col`]). On a queued item LT/RT move it up or down and
//! X pauses or resumes it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, Div, FontWeight, IntoElement, ParentElement, Stateful, Styled, Window, div, prelude::*, px};
use ubi_api::downloads::{DownloadSettings, InstallPreview, QueueEntry, QueueKind, QueueMove, QueueState, UpdatePreview, VerifyReport};
use ubi_api::exports::SteamExportResult;
use ubi_api::setup::SetupState;
use ubi_api::{Event, Job, JobKind, Request};

use super::game::checkbox;
use super::hero::{HeroTitle, TitleBox, title_element};
use super::{
    BOLD, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, art_fill, button, caption, card, command_line, cover, display,
    icon, mono, pill, progress, soft_shadow,
};
use crate::app::{Conn, InputMode, Quiver};
use crate::art::{ArtSlot, ArtState, Variant};
use crate::model::{actor, actor_label, ago, clean_name, gb, tilde, Actor};
use crate::nav::{Input, Page, Section, Target, Zone};
use crate::theme::{Theme, on_art, rgba};
use crate::err_text;

/// Commands other screens send here (`Cmd::Downloads`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DlCmd {
    /// The verify dialog (last report, verify, repair).
    Verify(u32),
    /// The welcome flow from its start, or at "Bring in existing installs".
    Welcome,
    Import,
}

/// A button on the Downloads page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    PauseAll,
    ResumeAll,
    Import,
    Pause(u32),
    Resume(u32),
    Remove(u32),
    Up(u32),
    Down(u32),
    Retry(u32),
    Open(u32),
    FinishSteam(u32),
    FinishDesktop(u32),
    ClearRecent,
    /// Bandwidth limit in MiB/s (0: none), like `quiver queue settings --limit 25M`.
    Limit(u64),
    MaxActive(usize),
    AutoUpdate,
}

/// One focusable row of the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Header,
    Active(u32),
    Finish(u32),
    Queued(u32),
    Failed(u32),
    Recent(u64),
    Limit,
    MaxActive,
    AutoUpdate,
}

/// Speed-limit choices, MiB/s (0: unlimited).
pub const LIMITS: [u64; 5] = [0, 10, 25, 50, 100];
const SAMPLES: usize = 60;

/// "When it finishes" for one install.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct After {
    pub steam: bool,
    pub desktop: bool,
}

#[derive(Clone)]
pub enum DlDialog {
    Install {
        id: u32,
        language: Option<String>,
        preview: Option<Result<InstallPreview, String>>,
        after: After,
    },
    Update {
        id: u32,
        preview: Option<Result<UpdatePreview, String>>,
    },
    Verify {
        id: u32,
        /// The last report (`None` inside: never verified).
        report: Option<Result<Option<VerifyReport>, String>>,
        /// A verify started from here.
        job: Option<u64>,
    },
}

/// A control in a dialog, in focus order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DCtl {
    Lang(usize),
    Steam,
    Desktop,
    Cancel,
    Confirm,
    Verify,
    Repair,
}

#[derive(Default)]
pub struct DlState {
    pub queue: Vec<QueueEntry>,
    pub settings: Option<DownloadSettings>,
    pub setup: Option<SetupState>,
    setup_at: Option<Instant>,
    /// Download speed samples per game (one a second while the page is open).
    pub speeds: HashMap<u32, VecDeque<u64>>,
    last_sample: HashMap<u32, Instant>,
    /// The button within the focused row.
    pub col: usize,
    pub dialog: Option<DlDialog>,
    /// The focused dialog control.
    pub dsel: usize,
    /// Exports to write when an install started here finishes.
    pub after: HashMap<u32, After>,
    /// Finished jobs hidden with "Clear".
    pub cleared: HashSet<u64>,
    /// Which left-column child each row is (for scrolling), as of the last render.
    row_child: Vec<Option<usize>>,
    loading: bool,
    started: bool,
}

/// "3 min 26 s", "1 h 04 min", "12 s".
pub fn eta(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// Sizes up to terabytes: "1.21 TB", "48.7 GB".
pub fn size(bytes: u64) -> String {
    if bytes >= 1_000_000_000_000 { format!("{:.2} TB", bytes as f64 / 1e12) } else { gb(bytes) }
}

/// "84.6 MB/s".
pub fn rate(bps: u64) -> String {
    let m = bps as f64 / 1e6;
    if m >= 100. { format!("{m:.0} MB/s") } else if m >= 1. { format!("{m:.1} MB/s") } else { format!("{:.0} KB/s", bps as f64 / 1e3) }
}

fn kind_word(k: QueueKind) -> &'static str {
    match k {
        QueueKind::Install => "Install",
        QueueKind::Update => "Update",
        QueueKind::Repair => "Repair",
    }
}

/// The source chip ("You · app", "claude-code", "Auto-update").
pub fn who(th: &Theme, source: &str) -> Div {
    let (c, i, label) = match source {
        "auto-update" => (th.who_sys, Icon::Cog, "Auto-update".to_owned()),
        s => match actor(s) {
            Actor::App => (th.who_gui, Icon::MousePointer, actor_label(s)),
            Actor::Cli => (th.who_cli, Icon::SquareTerminal, actor_label(s)),
            Actor::Agent => (th.who_agent, Icon::Bot, actor_label(s)),
            Actor::System => (th.who_sys, Icon::Cog, actor_label(s)),
        },
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .px(px(6.))
        .py(px(2.))
        .rounded(px(6.))
        .bg(c.opacity(0.13))
        .text_size(px(11.5))
        .font_weight(SEMIBOLD)
        .text_color(c)
        .child(icon(i, 12., c))
        .child(label)
}

/// A card cut into rows: `first` rounds the top, `last` the bottom; rows are separated by
/// a hairline.
const DL_SIDE_W: f32 = 340.;

fn seg(th: &Theme, first: bool, last: bool) -> Div {
    div()
        .flex()
        .bg(th.ink2)
        .border_l_1()
        .border_r_1()
        .border_t_1()
        .border_color(th.line1)
        .when(first, |d| d.rounded_tl(px(12.)).rounded_tr(px(12.)))
        .when(last, |d| d.border_b_1().rounded_bl(px(12.)).rounded_br(px(12.)))
}

/// A square icon button with the focus ring.
fn ibtn(id: impl Into<gpui::ElementId>, th: &Theme, i: Icon, color: Option<gpui::Hsla>, focused: bool) -> Stateful<Div> {
    let fg = color.unwrap_or(th.tx2);
    let hover = th.ink4;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(30.))
        .rounded(px(8.))
        .border_1()
        .border_color(th.line2)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon(i, 15., fg))
        .focus_ring(focused, th, 8.)
}

impl Quiver {
    // -----------------------------------------------------------------------------------------
    // Hooks called from app.rs

    /// Once, from `Quiver::new`: the 1 s tick for the page, dialogs and the welcome flow.
    pub fn start_downloads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dl.started {
            return;
        }
        self.dl.started = true;
        cx.spawn(async move |this, cx| {
            let mut n: u64 = 0;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                n += 1;
                if this.update(cx, |this, cx| this.downloads_tick(n, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        if self.demo {
            self.start_downloads_demo(window, cx);
        }
    }

    fn downloads_tick(&mut self, n: u64, cx: &mut Context<Self>) {
        if !matches!(self.conn, Conn::Ready) || !self.daemon_up() {
            return;
        }
        let page = self.nav.page == Page::Downloads;
        if page || self.dl.dialog.is_some() {
            self.load_queue(cx);
            if self.dl.settings.is_none() {
                self.load_dl_settings(None, None, None, cx);
            }
            if page && self.dl.setup_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
                self.load_setup(cx);
            }
            cx.notify();
        } else if n % 5 == 0 && !self.dl.queue.is_empty() {
            self.load_queue(cx);
        }
        self.welcome_tick(n, cx);
    }

    /// After the app gets a `Status` on (re)connecting.
    pub fn downloads_status(&mut self, cx: &mut Context<Self>) {
        self.dl.settings = None;
        self.load_queue(cx);
        self.load_setup(cx);

    }

    /// Every daemon event, before `app.rs` handles it.
    pub fn downloads_event(&mut self, e: &Event, cx: &mut Context<Self>) {
        match e {
            Event::Job(j) if matches!(j.kind, JobKind::Install | JobKind::Verify | JobKind::Import) => {
                let old = self.status.as_ref().and_then(|s| s.jobs.iter().find(|o| o.id == j.id)).map(|o| (o.status.clone(), o.done));
                if old.as_ref() != Some(&(j.status.clone(), j.done)) && !j.status.starts_with("Downloading") {
                    self.load_queue(cx);
                }
                if j.done && old.is_some_and(|(_, done)| !done) {
                    self.job_finished(j, cx);
                }
            }
            Event::Reconnected => {
                self.dl.settings = None;
                self.dl.setup_at = None;
                self.load_queue(cx);
                self.load_setup(cx);
            }
            _ => {}
        }
        self.welcome_event(e, cx);
    }

    fn job_finished(&mut self, j: &Job, cx: &mut Context<Self>) {
        let id = j.product_id;
        if let Some(DlDialog::Verify { id: vid, job: Some(job), .. }) = &self.dl.dialog
            && *vid == id
            && *job == j.id
        {
            self.load_verify_report(id, cx);
        }
        if j.kind != JobKind::Install || j.error.is_some() || !j.status.starts_with("Installed") {
            return;
        }
        let Some(after) = self.dl.after.remove(&id) else { return };
        let name = self.game_name(id);
        if after.desktop {
            self.call::<serde_json::Value, _>(Request::ExportDesktop { product_id: id, remove: false }, cx, move |this, r, cx| {
                if let Err(e) = r {
                    this.show_toast(format!(".desktop entry: {}", err_text(&e)), cx);
                }
                this.load_global_exports(cx);
            });
        }
        if after.steam {
            self.call::<SteamExportResult, _>(Request::ExportSteam { product_id: id, remove: false, steam_user: None }, cx, move |this, r, cx| {
                match r {
                    Ok(r) if r.staged => this.show_toast(
                        format!("{name} is installed. Its Steam shortcut shows after Steam restarts; Quiver won't restart Steam without asking"),
                        cx,
                    ),
                    Ok(_) => this.show_toast(format!("{name} is installed and in Steam"), cx),
                    Err(e) => this.show_toast(format!("{name} is installed. Add to Steam: {}", err_text(&e)), cx),
                }
                this.load_global_exports(cx);
            });
        } else {
            self.show_toast(format!("{name} is installed"), cx);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Loading

    pub fn load_queue(&mut self, cx: &mut Context<Self>) {
        if self.dl.loading || self.source.is_none() {
            return;
        }
        self.dl.loading = true;
        self.call::<Vec<QueueEntry>, _>(Request::Queue, cx, |this, r, _| {
            this.dl.loading = false;
            let Ok(q) = r else { return };
            let now = Instant::now();
            for e in q.iter().filter(|e| e.state.is_active()) {
                let due = this.dl.last_sample.get(&e.product_id).is_none_or(|t| now.duration_since(*t) >= Duration::from_millis(900));
                if due {
                    this.dl.last_sample.insert(e.product_id, now);
                    let s = this.dl.speeds.entry(e.product_id).or_default();
                    s.push_back(e.speed);
                    while s.len() > SAMPLES {
                        s.pop_front();
                    }
                }
            }
            this.dl.speeds.retain(|id, _| q.iter().any(|e| e.product_id == *id));
            this.dl.queue = q;
        });
    }

    fn load_dl_settings(&mut self, max_active: Option<usize>, bandwidth_limit: Option<u64>, auto_update: Option<bool>, cx: &mut Context<Self>) {
        let change = max_active.is_some() || bandwidth_limit.is_some() || auto_update.is_some();
        self.call::<DownloadSettings, _>(Request::DownloadSettings { max_active, bandwidth_limit, auto_update }, cx, move |this, r, cx| match r {
            Ok(s) => {
                if change {
                    let limit = s.bandwidth_limit.map(|b| format!("{} MB/s", b >> 20)).unwrap_or_else(|| "no limit".into());
                    this.show_toast(format!("Downloads: {} at a time, {limit}, auto-update {}", s.max_active, if s.auto_update { "on" } else { "off" }), cx);
                }
                this.dl.settings = Some(s);
            }
            Err(e) if change => this.show_toast(err_text(&e), cx),
            Err(_) => {}
        });
    }

    pub fn load_setup(&mut self, cx: &mut Context<Self>) {
        self.dl.setup_at = Some(Instant::now());
        self.call::<SetupState, _>(Request::Setup { library: None, prefixes: None, finish: None }, cx, |this, r, cx| {
            if let Ok(s) = r {
                this.dl.setup = Some(s);
                this.welcome_setup_loaded(cx);
            }
        });
    }

    // -----------------------------------------------------------------------------------------
    // Commands

    pub fn run_dl(&mut self, c: DlCmd, cx: &mut Context<Self>) {
        match c {
            DlCmd::Verify(id) => self.open_verify_dialog(id, cx),
            DlCmd::Welcome => self.open_welcome(crate::ui::welcome::Step::SignIn, cx),
            DlCmd::Import => self.open_import(cx),
        }
        cx.notify();
    }

    /// Send a queue request, then reload the queue.
    fn queue_req(&mut self, req: Request, ok: Option<String>, cx: &mut Context<Self>) {
        self.call::<serde_json::Value, _>(req, cx, move |this, r, cx| {
            match r {
                Ok(_) => {
                    if let Some(m) = ok {
                        this.show_toast(m, cx);
                    }
                }
                Err(e) => this.show_toast(err_text(&e), cx),
            }
            this.load_queue(cx);
        });
    }

    pub fn press(&mut self, b: Btn, cx: &mut Context<Self>) {
        let name = |this: &Self, id: u32| this.game_name(id);
        match b {
            Btn::PauseAll => {
                for e in self.dl.queue.clone() {
                    if e.state == QueueState::Queued || e.state.is_active() {
                        self.queue_req(Request::QueuePause { product_id: e.product_id }, None, cx);
                    }
                }
                self.show_toast("Pausing every download (each stops at a slice boundary)", cx);
            }
            Btn::ResumeAll => {
                for e in self.dl.queue.clone() {
                    if e.state == QueueState::Paused {
                        self.queue_req(Request::QueueResume { product_id: e.product_id }, None, cx);
                    }
                }
                self.show_toast("Resuming downloads", cx);
            }
            Btn::Import => self.open_import(cx),
            Btn::Pause(id) => {
                let m = format!("Pausing {}: it keeps its place", name(self, id));
                self.queue_req(Request::QueuePause { product_id: id }, Some(m), cx);
            }
            Btn::Resume(id) => self.queue_req(Request::QueueResume { product_id: id }, Some(format!("Resuming {}", name(self, id))), cx),
            Btn::Retry(id) => self.queue_req(Request::QueueResume { product_id: id }, Some(format!("Retrying {}", name(self, id))), cx),
            Btn::Remove(id) => {
                let m = format!("Removed {} from the queue; its files stay, so installing again resumes", name(self, id));
                self.queue_req(Request::QueueRemove { product_id: id }, Some(m), cx);
            }
            Btn::Up(id) | Btn::Down(id) => {
                let to = if matches!(b, Btn::Up(_)) { QueueMove::Up } else { QueueMove::Down };
                // Move now, so focus can follow; the daemon's order comes back with the reply.
                if let Some(i) = self.dl.queue.iter().position(|e| e.product_id == id) {
                    let j = if to == QueueMove::Up { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < self.dl.queue.len()) };
                    if let Some(j) = j {
                        self.dl.queue.swap(i, j);
                        self.follow_row(Row::Queued(id));
                    }
                }
                self.call::<Vec<QueueEntry>, _>(Request::QueueMove { product_id: id, to }, cx, |this, r, cx| match r {
                    Ok(q) => this.dl.queue = q,
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            Btn::Open(id) => self.open_game(id, cx),
            Btn::FinishSteam(id) | Btn::FinishDesktop(id) => {
                let a = self.dl.after.entry(id).or_default();
                if matches!(b, Btn::FinishSteam(_)) {
                    a.steam = !a.steam;
                } else {
                    a.desktop = !a.desktop;
                }
            }
            Btn::ClearRecent => {
                let ids: Vec<u64> = self.recent_jobs().iter().map(|j| j.id).collect();
                self.dl.cleared.extend(ids);
                self.nav.focus(&self.layout, Target::Body(0));
            }
            Btn::Limit(mb) => self.load_dl_settings(None, Some(mb << 20), None, cx),
            Btn::MaxActive(n) => self.load_dl_settings(Some(n), None, None, cx),
            Btn::AutoUpdate => {
                let on = self.dl.settings.as_ref().is_some_and(|s| s.auto_update);
                self.load_dl_settings(None, None, Some(!on), cx);
            }
        }
        cx.notify();
    }

    /// Keep focus on a row after the rows changed.
    fn follow_row(&mut self, row: Row) {
        if let Some(i) = self.dl_rows().iter().position(|(r, _)| *r == row) {
            let col = self.dl.col;
            self.nav.focus(&self.layout, Target::Body(i));
            self.dl.col = col;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Rows and focus

    fn dl_split(&self) -> (Vec<QueueEntry>, Vec<QueueEntry>, Vec<QueueEntry>) {
        let q = &self.dl.queue;
        (
            q.iter().filter(|e| e.state.is_active()).cloned().collect(),
            q.iter().filter(|e| matches!(e.state, QueueState::Queued | QueueState::Paused)).cloned().collect(),
            q.iter().filter(|e| e.state == QueueState::Failed).cloned().collect(),
        )
    }

    /// Finished installs, updates, repairs, verifies and imports this daemon run, newest first.
    fn recent_jobs(&self) -> Vec<Job> {
        let mut v: Vec<Job> = self
            .status
            .as_ref()
            .map(|s| {
                s.jobs
                    .iter()
                    .filter(|j| j.done && j.kind != JobKind::Play && !self.dl.cleared.contains(&j.id))
                    .filter(|j| !j.status.starts_with("Removed from queue"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|j| std::cmp::Reverse(j.id));
        v.truncate(6);
        v
    }

    fn job(&self, id: Option<u64>) -> Option<&Job> {
        let id = id?;
        self.status.as_ref()?.jobs.iter().find(|j| j.id == id)
    }

    /// The page's rows and each row's buttons, in focus order.
    pub fn dl_rows(&self) -> Vec<(Row, Vec<Btn>)> {
        let (active, waiting, failed) = self.dl_split();
        let mut rows = Vec::new();
        let mut head = Vec::new();
        if waiting.iter().any(|e| e.state == QueueState::Paused) && active.is_empty() {
            head.push(Btn::ResumeAll);
        } else if !active.is_empty() || waiting.iter().any(|e| e.state == QueueState::Queued) {
            head.push(Btn::PauseAll);
        }
        head.push(Btn::Import);
        rows.push((Row::Header, head));
        for e in &active {
            let id = e.product_id;
            rows.push((Row::Active(id), vec![Btn::Pause(id), Btn::Remove(id), Btn::Open(id)]));
            if e.kind == QueueKind::Install {
                rows.push((Row::Finish(id), vec![Btn::FinishSteam(id), Btn::FinishDesktop(id)]));
            }
        }
        for e in &waiting {
            let id = e.product_id;
            let toggle = if e.state == QueueState::Paused { Btn::Resume(id) } else { Btn::Pause(id) };
            rows.push((Row::Queued(id), vec![Btn::Up(id), Btn::Down(id), toggle, Btn::Remove(id)]));
        }
        for e in &failed {
            let id = e.product_id;
            rows.push((Row::Failed(id), vec![Btn::Retry(id), Btn::Remove(id)]));
        }
        for (i, j) in self.recent_jobs().iter().enumerate() {
            let mut b = vec![Btn::Open(j.product_id)];
            if i == 0 {
                b.push(Btn::ClearRecent);
            }
            rows.push((Row::Recent(j.id), b));
        }
        rows.push((Row::Limit, LIMITS.iter().map(|m| Btn::Limit(*m)).collect()));
        rows.push((Row::MaxActive, (1..=3).map(Btn::MaxActive).collect()));
        rows.push((Row::AutoUpdate, vec![Btn::AutoUpdate]));
        rows
    }

    pub fn downloads_body_len(&self) -> usize {
        self.dl_rows().len()
    }

    /// Whether button `col` of row `row` (a body index) has focus.
    fn btn_focused(&self, row: usize, col: usize, len: usize) -> bool {
        self.nav.page == Page::Downloads && self.nav.zone == Zone::Body && self.focused(Target::Body(row)) && self.dl.col.min(len.saturating_sub(1)) == col
    }

    /// A on a row: its focused button.
    pub fn downloads_activate(&mut self, i: usize, cx: &mut Context<Self>) {
        let rows = self.dl_rows();
        let Some((_, btns)) = rows.get(i) else { return };
        if let Some(b) = btns.get(self.dl.col.min(btns.len().saturating_sub(1))) {
            self.press(*b, cx);
        }
    }

    /// Scroll the focused row into view (left column only; the right one fits).
    pub fn downloads_scroll(&mut self) {
        if let Some(Some(ix)) = self.dl.row_child.get(self.nav.body_index()) {
            self.body_scroll.scroll_to_item(*ix);
        }
    }

    /// Input before [`crate::nav::Nav`] sees it: the welcome flow, dialogs, and the page's
    /// own buttons. Returns whether it was handled.
    pub fn downloads_input(&mut self, input: Input, cx: &mut Context<Self>) -> bool {
        if self.welcome_open() {
            return self.welcome_input(input, cx);
        }
        if self.dl.dialog.is_some() {
            let ctl = self.dl_dialog_controls();
            let n = ctl.len().max(1);
            match input {
                Input::Left | Input::Up => self.dl.dsel = (self.dl.dsel + n - 1) % n,
                Input::Right | Input::Down => self.dl.dsel = (self.dl.dsel + 1) % n,
                Input::Activate => {
                    if let Some(c) = ctl.get(self.dl.dsel.min(n - 1)).copied() {
                        self.dl_dialog_press(c, cx);
                    }
                }
                Input::Back => self.dl.dialog = None,
                _ => {}
            }
            return true;
        }
        if self.nav.page != Page::Downloads || self.nav.overlay.is_some() || self.ui.confirm.is_some() {
            return false;
        }
        if self.nav.zone != Zone::Body {
            return false;
        }
        let rows = self.dl_rows();
        let Some((row, btns)) = rows.get(self.nav.body_index().min(rows.len().saturating_sub(1))).cloned() else { return false };
        let col = self.dl.col.min(btns.len().saturating_sub(1));
        match input {
            Input::Left => {
                self.dl.col = col.saturating_sub(1);
                true
            }
            Input::Right => {
                self.dl.col = (col + 1).min(btns.len().saturating_sub(1));
                true
            }
            Input::Up | Input::Down => {
                // Keep the column where it means the same thing (the speed chips, say).
                self.dl.col = col;
                false
            }
            Input::PrevTab | Input::NextTab => {
                if let Row::Queued(id) = row {
                    self.press(if input == Input::PrevTab { Btn::Up(id) } else { Btn::Down(id) }, cx);
                }
                true
            }
            Input::PlayOptions => {
                let b = btns.iter().find(|b| matches!(b, Btn::Pause(_) | Btn::Resume(_) | Btn::Retry(_))).copied();
                if let Some(b) = b {
                    self.press(b, cx);
                }
                true
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------------------------
    // The page

    /// Width of the queue column.
    fn dl_main_w(&self) -> f32 {
        if self.bp().two_columns() { (self.viewport.0 - 2. * self.gutter() - DL_SIDE_W - 24.).max(420.) } else { self.viewport.0 - 2. * self.gutter() }
    }

    /// The queue column is too narrow for one-line rows: they wrap, and drop the grip and
    /// who-added column.
    fn dl_tight(&self) -> bool {
        self.dl_main_w() < 700.
    }

    pub fn render_downloads(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let rows = self.dl_rows();
        let (active, waiting, failed) = self.dl_split();
        let recent = self.recent_jobs();
        // Storage and rules sit beside the queue from Medium up, under it below.
        let two = self.bp().two_columns();
        let main_w = self.dl_main_w();
        let tight = self.dl_tight();
        let row_ix = |r: Row| rows.iter().position(|(x, _)| *x == r).unwrap_or(usize::MAX);
        let mut children: Vec<AnyElement> = Vec::new();
        let mut row_child: Vec<Option<usize>> = vec![None; rows.len()];

        // Header.
        let queued = waiting.iter().filter(|e| e.state == QueueState::Queued).count();
        let paused = waiting.len() - queued;
        let mut sub = vec![format!("{} active", active.len())];
        if queued > 0 {
            sub.push(format!("{queued} queued"));
        }
        if paused > 0 {
            sub.push(format!("{paused} paused"));
        }
        if !failed.is_empty() {
            sub.push(format!("{} failed", failed.len()));
        }
        let hi = row_ix(Row::Header);
        let head_btns = rows.get(hi).map(|r| r.1.clone()).unwrap_or_default();
        let mut head = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.))
            .pt(px(28.))
            .pb(px(18.))
            .child(display(22., FontWeight::BOLD, th.tx1).child("Downloads"))
            .child(div().text_color(th.tx3).child(sub.join(" · ")))
            .child(div().flex_1());
        for (c, b) in head_btns.iter().enumerate() {
            let f = self.btn_focused(hi, c, head_btns.len());
            let (i, label) = match b {
                Btn::PauseAll => (Icon::Pause, "Pause all"),
                Btn::ResumeAll => (Icon::Play, "Resume all"),
                _ => (Icon::FolderInput, "Import Connect downloads"),
            };
            let b = *b;
            head = head.child(
                button(("dl-head", c), th, Kind::Secondary, Size::Medium, Some(i), label)
                    .focus_ring(f, th, 8.)
                    .on_click(cx.listener(move |this, _, _, cx| this.press(b, cx))),
            );
        }
        row_child[hi] = Some(children.len());
        children.push(head.into_any_element());

        // What's running.
        for e in &active {
            let r = row_ix(Row::Active(e.product_id));
            let fr = row_ix(Row::Finish(e.product_id));
            let el = self.render_active(th, e, main_w, r, fr, &rows, cx);
            if r < row_child.len() {
                row_child[r] = Some(children.len());
            }
            if fr < row_child.len() {
                row_child[fr] = Some(children.len());
            }
            children.push(div().mb(px(20.)).child(el).into_any_element());
        }
        if active.is_empty() {
            children.push(
                card(th)
                    .mb(px(20.))
                    .p(px(22.))
                    .flex_row()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div().flex().items_center().justify_center().size(px(40.)).rounded(px(10.)).bg(th.ink4).child(icon(Icon::Download, 18., th.tx3)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(div().font_weight(SEMIBOLD).text_size(px(14.)).child(if waiting.is_empty() {
                                "Nothing downloading"
                            } else {
                                "Nothing downloading right now"
                            }))
                            .child(div().text_color(th.tx3).text_size(px(12.5)).child(if paused > 0 && queued == 0 {
                                "Paused items keep their place; resume one to continue where it stopped."
                            } else {
                                "Installs, updates and repairs you start from the app, the CLI or an agent show up here."
                            })),
                    )
                    .into_any_element(),
            );
        }

        // Up next.
        if !waiting.is_empty() {
            children.push(
                seg(th, true, false)
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(10.))
                    .gap_y(px(4.))
                    .px(px(16.))
                    .py(px(12.))
                    .child(div().text_size(px(13.5)).font_weight(SEMIBOLD).child("Up next"))
                    .child(div().text_color(th.tx3).text_size(px(12.)).child(format!("{} in order", waiting.len())))
                    .child(div().flex_1())
                    .child(div().when(tight, |d| d.w_full()).text_color(th.tx3).text_size(px(12.)).child(if self.mode == InputMode::Pad {
                        "LT / RT move the focused item"
                    } else {
                        "Reorder with the arrows; one runs at a time unless the rules say more"
                    }))
                    .into_any_element(),
            );
            for (i, e) in waiting.iter().enumerate() {
                let r = row_ix(Row::Queued(e.product_id));
                let el = self.render_queued(th, e, i + 1 == waiting.len(), r, &rows, cx);
                if r < row_child.len() {
                    row_child[r] = Some(children.len());
                }
                children.push(el);
            }
            children.push(div().h(px(20.)).into_any_element());
        }

        // Failed.
        if !failed.is_empty() {
            children.push(
                seg(th, true, false)
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(10.))
                    .gap_y(px(4.))
                    .px(px(16.))
                    .py(px(12.))
                    .child(icon(Icon::TriangleAlert, 15., th.bad))
                    .child(div().text_size(px(13.5)).font_weight(SEMIBOLD).child("Needs attention"))
                    .child(div().when(tight, |d| d.w_full()).text_color(th.tx3).text_size(px(12.)).child("Failed items keep their place; Retry continues from what is on disk"))
                    .into_any_element(),
            );
            for (i, e) in failed.iter().enumerate() {
                let r = row_ix(Row::Failed(e.product_id));
                let el = self.render_failed(th, e, i + 1 == failed.len(), r, &rows, cx);
                if r < row_child.len() {
                    row_child[r] = Some(children.len());
                }
                children.push(el);
            }
            children.push(div().h(px(20.)).into_any_element());
        }

        // Recent.
        if !recent.is_empty() {
            let r0 = row_ix(Row::Recent(recent[0].id));
            let clear_f = self.btn_focused(r0, 1, 2);
            children.push(
                seg(th, true, false)
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(10.))
                    .child(div().text_size(px(13.5)).font_weight(SEMIBOLD).child("Recent"))
                    .child(div().text_color(th.tx3).text_size(px(12.)).child("since quiverd started"))
                    .child(div().flex_1())
                    .child(
                        button("dl-clear", th, Kind::Ghost, Size::Small, None, "Clear")
                            .focus_ring(clear_f, th, 6.)
                            .on_click(cx.listener(|this, _, _, cx| this.press(Btn::ClearRecent, cx))),
                    )
                    .into_any_element(),
            );
            for (i, j) in recent.iter().enumerate() {
                let r = row_ix(Row::Recent(j.id));
                let el = self.render_recent(th, j, i + 1 == recent.len(), r, cx);
                if r < row_child.len() {
                    row_child[r] = Some(children.len());
                }
                children.push(el);
            }
        }
        self.dl.row_child = row_child;

        if !two {
            // One column: the side cards after the queue, in the same scroll.
            let g = self.gutter();
            return div().flex().flex_1().min_h_0().child(
                div()
                    .id("dl-main")
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.body_scroll)
                    .px(px(g))
                    .pb(px(40.))
                    .children(children)
                    .child(div().flex().flex_col().gap(px(20.)).pt(px(28.)).child(self.render_storage(th)).child(self.render_rules(th, &rows, cx))),
            );
        }
        let side = div()
            .id("dl-side")
            .w(px(DL_SIDE_W))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(20.))
            .overflow_y_scroll()
            .pt(px(76.))
            .pb(px(40.))
            .pr(px(self.gutter()))
            .child(self.render_storage(th))
            .child(self.render_rules(th, &rows, cx));

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap(px(24.))
            .child(
                div()
                    .id("dl-main")
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.body_scroll)
                    .pl(px(self.gutter()))
                    .pb(px(40.))
                    .children(children),
            )
            .child(side)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_active(&mut self, th: &Theme, e: &QueueEntry, w: f32, r: usize, fr: usize, rows: &[(Row, Vec<Btn>)], cx: &mut Context<Self>) -> AnyElement {
        let id = e.product_id;
        let g = self.game(id).cloned();
        let job = self.job(e.job_id).cloned();
        let (done, total) = match &job {
            Some(j) if j.total > 0 => (j.downloaded, j.total),
            _ => (e.done_bytes, e.total_bytes),
        };
        let f = if total > 0 { (done as f64 / total as f64).clamp(0., 1.) as f32 } else { 0. };
        let status = job.as_ref().map(|j| j.status.clone()).unwrap_or_default();
        let retrying = status.starts_with("Retrying");
        let verifying = e.state == QueueState::Verifying;
        let h = if e.kind == QueueKind::Install { 352. } else { 300. };
        // Tight: title and buttons on one line, the numbers under them, and the card grows
        // to fit (the art is cropped to a generous height).
        let tight = w < 700.;
        let name = g.as_ref().map(|g| clean_name(&g.entry.name)).unwrap_or_else(|| clean_name(&e.name));

        // Art: the hero, dimmed, under a scrim that fades to the card.
        let hero = match &g {
            Some(g) => self.art_for(g, ArtSlot::Hero, Variant::Original, cx),
            None => ArtState::Failed,
        };
        let title = match &g {
            Some(g) => match self.hero_title(g, cx) {
                HeroTitle::Baked => HeroTitle::Text,
                t => t,
            },
            None => HeroTitle::Text,
        };
        let title_el = title_element(&title, &name, if tight {
            TitleBox { max_w: 150., max_h: 48., max_scale: 1.0, text: 17., text_w: 150. }
        } else {
            TitleBox { max_w: 210., max_h: 62., max_scale: 1.0, text: 22., text_w: 260. }
        })
            .unwrap_or_else(|| div().into_any_element());

        let (pill_kind, label) = match (e.kind, verifying) {
            (_, true) => (PillKind::Accent, "Verifying"),
            (QueueKind::Install, _) => (PillKind::Accent, "Installing"),
            (QueueKind::Update, _) => (PillKind::Accent, "Updating"),
            (QueueKind::Repair, _) => (PillKind::Warn, "Repairing"),
        };
        let speed = if verifying { 0 } else { e.speed };
        let left = total.saturating_sub(done);
        let mut nums = div().flex().flex_wrap().items_center().gap_x(px(18.)).gap_y(px(2.)).text_size(px(13.)).text_color(on_art::tx1());
        let num = |b: String, s: &str| {
            div().flex().items_baseline().gap(px(5.)).child(div().font_weight(BOLD).child(b)).child(div().text_color(on_art::tx2()).child(s.to_owned()))
        };
        if total > 0 {
            let of = if verifying { "of {} read" } else { "of {}" };
            nums = nums.child(num(format!("{:.1}", done as f64 / 1e9), &of.replace("{}", &gb(total))));
        } else {
            nums = nums.child(div().text_color(on_art::tx2()).child(if status.is_empty() { "Starting…".into() } else { status.clone() }));
        }
        if speed > 0 {
            nums = nums.child(num(rate(speed), "network"));
            nums = nums.child(num(eta(left / speed.max(1)), "left"));
        }
        if e.retries > 0 {
            nums = nums.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_color(th.warn)
                    .child(icon(Icon::RefreshCw, 13., th.warn))
                    .child(format!("{} {}", e.retries, if e.retries == 1 { "retry" } else { "retries" })),
            );
        }

        let btns = rows.get(r).map(|x| x.1.clone()).unwrap_or_default();
        let pausing = status.starts_with("Pausing") || status.starts_with("Cancelling");
        let pause = button(("dl-pause", id as usize), th, Kind::OnArt, Size::Medium, Some(Icon::Pause), if pausing { "Pausing…" } else { "Pause" })
            .focus_ring(self.btn_focused(r, 0, btns.len()), th, 8.)
            .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Pause(id), cx)));
        let remove = button(("dl-x", id as usize), th, Kind::OnArt, Size::Medium, Some(Icon::X), "")
            .w(px(34.))
            .px(px(0.))
            .focus_ring(self.btn_focused(r, 1, btns.len()), th, 8.)
            .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Remove(id), cx)));
        let open = button(("dl-open", id as usize), th, Kind::OnArt, Size::Medium, Some(Icon::ChevronRight), "")
            .w(px(34.))
            .px(px(0.))
            .focus_ring(self.btn_focused(r, 2, btns.len()), th, 8.)
            .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Open(id), cx)));

        // Phases, like the installer reports them.
        let steps: Vec<(String, u8)> = match e.kind {
            QueueKind::Repair => vec![
                ("Verify files".into(), if verifying { 1 } else { 0 }),
                ("Re-download bad slices".into(), if verifying { 2 } else { 1 }),
                ("Done".into(), 2),
            ],
            _ => {
                let downloading = total > 0 && done < total;
                vec![
                    ("Manifest".into(), if total == 0 { 1 } else { 0 }),
                    (
                        if total > 0 { format!("Download · {:.0}%", (f * 100.).floor()) } else { "Download".into() },
                        if downloading { 1 } else if total == 0 { 2 } else { 0 },
                    ),
                    (if e.kind == QueueKind::Install { "Register, create compatibility folder".into() } else { "Register".into() }, if total > 0 && done >= total { 1 } else { 2 }),
                ]
            }
        };
        let phase_row = div().flex().flex_wrap().items_center().gap(px(6.)).text_size(px(11.5)).children(steps.iter().enumerate().flat_map(|(i, (s, st))| {
            let (c, ic) = match st {
                0 => (th.ok, Icon::CircleCheck),
                1 => (th.acc_hi, Icon::LoaderCircle),
                _ => (on_art::tx2().opacity(0.7), Icon::CircleDot),
            };
            let mut v: Vec<AnyElement> = Vec::new();
            if i > 0 {
                v.push(icon(Icon::ChevronRight, 11., on_art::tx2().opacity(0.6)).into_any_element());
            }
            v.push(div().flex().items_center().gap(px(5.)).text_color(c).child(icon(ic, 12., c)).child(s.clone()).into_any_element());
            v
        }));

        let chart = self.render_throughput(th, id, w - 44., verifying);
        let mut body = div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(14.))
            .px(px(22.))
            .py(px(20.))
            .child({
                let info = div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.))
                                    .child(pill(th, pill_kind, None, label))
                                    .child(who(th, &e.source))
                            .when(retrying, |d| d.child(pill(th, PillKind::Warn, Some(Icon::RefreshCw), status.clone())))
                            .when_some(e.job_id, |d, j| d.child(mono(11., on_art::tx2()).child(format!("job {j}")))),
                    )
                    .child(nums);
                if tight {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(div().flex_1().min_w_0().h(px(48.)).flex().items_center().overflow_hidden().child(title_el))
                                .child(pause)
                                .child(remove)
                                .child(open),
                        )
                        .child(info)
                } else {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(16.))
                        .child(div().w(px(210.)).h(px(62.)).flex().items_center().child(title_el))
                        .child(info)
                        .child(pause)
                        .child(remove)
                        .child(open)
                }
            })
            .child(progress(th, f, None, 8., None))
            .child(phase_row)
            .child(chart);
        if e.kind == QueueKind::Install {
            let after = self.dl.after.get(&id).copied().unwrap_or_default();
            let steam_running = matches!(&self.ui.steam, Some(Ok(s)) if s.running && s.root.is_some());
            let fb = rows.get(fr).map(|x| x.1.len()).unwrap_or(2);
            let check = |key: &'static str, on: bool, label: &str, f: bool, b: Btn, cx: &mut Context<Self>| {
                div()
                    .id((key, id as usize))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(4.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .child(checkbox(th, on))
                    .child(label.to_owned())
                    .focus_ring(f, th, 6.)
                    .on_click(cx.listener(move |this, _, _, cx| this.press(b, cx)))
            };
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(16.))
                    .gap_y(px(4.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(th.ink0.opacity(0.6))
                    .border_1()
                    .border_color(th.line1)
                    .text_size(px(12.5))
                    .child(caption("When it finishes", th.tx3))
                    .child(check("dl-f-steam", after.steam, "Add to Steam", self.btn_focused(fr, 0, fb), Btn::FinishSteam(id), cx))
                    .child(check("dl-f-desk", after.desktop, "Create .desktop entry", self.btn_focused(fr, 1, fb), Btn::FinishDesktop(id), cx))
                    .when(!tight, |d| d.child(div().flex_1()))
                    .child(div().text_size(px(11.5)).text_color(th.tx3).min_w_0().when(!tight, |d| d.truncate()).child(if after.steam && steam_running {
                        "Steam is running: the shortcut shows after it restarts, and Quiver asks first"
                    } else if after.steam || after.desktop {
                        "Quiver does this when the install finishes, while the app is open"
                    } else {
                        "Nothing ticked: add it later from the game page"
                    })),
            );
        }

        let mut c = card(th).relative().overflow_hidden().w_full();
        c = if tight { c.min_h(px(h)) } else { c.h(px(h)) };
        let art_h = if tight { h + 240. } else { h };
        if let ArtState::Ready(img) = &hero {
            c = c.child(div().absolute().top_0().left_0().size_full().overflow_hidden().opacity(0.5).child(cover(img, w, art_h, (0.6, 0.3))));
        }
        c.child(div().absolute().inset_0().bg(gpui::linear_gradient(
            90.,
            gpui::linear_color_stop(th.ink2, 0.3),
            gpui::linear_color_stop(th.ink2.opacity(0.3), 1.),
        )))
        .child(body)
        .into_any_element()
    }

    /// The last minute of download speed as bars.
    fn render_throughput(&self, th: &Theme, id: u32, w: f32, verifying: bool) -> AnyElement {
        let samples: Vec<u64> = self.dl.speeds.get(&id).map(|s| s.iter().copied().collect()).unwrap_or_default();
        let peak = samples.iter().copied().max().unwrap_or(0);
        let limit = self.dl.settings.as_ref().and_then(|s| s.bandwidth_limit);
        let top = peak.max(limit.unwrap_or(0)).max(1_000_000) as f32 * 1.1;
        let ch = 64.;
        let bw = ((w - 24.) / SAMPLES as f32 - 2.).max(1.);
        let mut bars = div().flex().items_end().gap(px(2.)).h(px(ch));
        for i in 0..SAMPLES {
            let v = samples.len().checked_sub(SAMPLES - i).and_then(|k| samples.get(k)).copied();
            let hgt = v.map(|v| (v as f32 / top * ch).max(2.)).unwrap_or(2.);
            let last = i + 1 == SAMPLES && v.is_some();
            bars = bars.child(
                div()
                    .w(px(bw))
                    .h(px(hgt))
                    .rounded_t(px(2.))
                    .bg(if v.is_none() { th.track } else if last { th.acc_hi } else { th.acc.opacity(0.75) }),
            );
        }
        let mut chart = div().relative().child(bars);
        if let Some(l) = limit {
            let y = ch - l as f32 / top * ch;
            chart = chart.child(div().absolute().left_0().right_0().top(px(y)).border_t_1().border_dashed().border_color(th.warn.opacity(0.7)));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(12.))
            .pt(px(10.))
            .pb(px(8.))
            .rounded(px(10.))
            .bg(th.ink0.opacity(0.6))
            .border_1()
            .border_color(th.line1)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_size(px(11.5))
                    .child(caption(if verifying { "Hashing" } else { "Throughput · last minute" }, th.tx3))
                    .child(div().flex_1())
                    .when(peak > 0, |d| d.child(div().text_color(th.tx3).child(format!("peak {}", rate(peak)))))
                    .when_some(limit, |d, l| {
                        d.child(div().flex().items_center().gap(px(5.)).text_color(th.warn).child(div().w(px(12.)).border_t_1().border_dashed().border_color(th.warn)).child(format!("limit {} MB/s", l >> 20)))
                    }),
            )
            .child(chart)
            .into_any_element()
    }

    fn thumb(&mut self, th: &Theme, id: u32, w: f32, h: f32, cx: &mut Context<Self>) -> AnyElement {
        let art = match self.game(id).cloned() {
            Some(g) => self.art_for(&g, ArtSlot::Tile, Variant::Tile, cx),
            None => ArtState::Failed,
        };
        div().w(px(w)).h(px(h)).flex_none().child(art_fill(th, &art, "", 6., false)).into_any_element()
    }

    fn render_queued(&mut self, th: &Theme, e: &QueueEntry, last: bool, r: usize, rows: &[(Row, Vec<Btn>)], cx: &mut Context<Self>) -> AnyElement {
        let id = e.product_id;
        let btns = rows.get(r).map(|x| x.1.clone()).unwrap_or_default();
        let n = btns.len();
        let paused = e.state == QueueState::Paused;
        let row_focus = self.focused(Target::Body(r));
        let job = self.job(e.job_id).cloned();
        let waiting = job.as_ref().is_some_and(|j| j.status.contains("waiting for the game"));
        let mut what = vec![kind_word(e.kind).to_owned()];
        if e.total_bytes > 0 {
            what.push(format!("{} of {}", gb(e.done_bytes), gb(e.total_bytes)));
        }
        if let Some(l) = &e.language {
            what.push(l.clone());
        }
        let state = if paused {
            pill(th, PillKind::Muted, Some(Icon::Pause), "Paused")
        } else if waiting {
            pill(th, PillKind::Warn, Some(Icon::Gamepad), "Waits for the game to exit")
        } else {
            pill(th, PillKind::Muted, None, format!("Queued · #{}", e.position))
        };
        let thumb = self.thumb(th, id, 34., 40., cx);
        let tight = self.dl_tight();
        let first_q = self.dl.queue.iter().filter(|x| matches!(x.state, QueueState::Queued | QueueState::Paused)).map(|x| x.product_id).next() == Some(id);
        let last_q = self.dl.queue.iter().filter(|x| matches!(x.state, QueueState::Queued | QueueState::Paused)).map(|x| x.product_id).last() == Some(id);
        let up = ibtn(("q-up", id as usize), th, Icon::ChevronUp, None, self.btn_focused(r, 0, n))
            .when(first_q, |d| d.opacity(0.4))
            .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Up(id), cx)));
        let down = ibtn(("q-down", id as usize), th, Icon::ChevronDown, None, self.btn_focused(r, 1, n))
            .when(last_q, |d| d.opacity(0.4))
            .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Down(id), cx)));
        let tb = if paused { Btn::Resume(id) } else { Btn::Pause(id) };
        let toggle = ibtn(("q-pp", id as usize), th, if paused { Icon::Play } else { Icon::Pause }, None, self.btn_focused(r, 2, n))
            .on_click(cx.listener(move |this, _, _, cx| this.press(tb, cx)));
        let remove = ibtn(("q-x", id as usize), th, Icon::X, None, self.btn_focused(r, 3, n)).on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Remove(id), cx)));
        seg(th, false, last)
            .id(("q-row", id as usize))
            .when(tight, |d| d.flex_wrap())
            .items_center()
            .gap_x(px(if tight { 10. } else { 14. }))
            .gap_y(px(8.))
            .px(px(16.))
            .py(px(10.))
            .when(row_focus, |d| d.bg(th.ink3))
            .when(!tight, |d| d.child(icon(Icon::GripVertical, 15., th.tx4)))
            .child(mono(12., th.tx3).w(px(16.)).child(e.position.to_string()))
            .child(thumb)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(if tight { 140. } else { 0. }))
                    .gap(px(2.))
                    .child(div().font_weight(SEMIBOLD).truncate().child(self.game_name(id)))
                    .child(div().text_size(px(12.)).text_color(th.tx3).truncate().child(what.join(" · "))),
            )
            .child(state)
            .when(!tight, |d| {
                d.child(div().flex().flex_col().items_end().gap(px(3.)).w(px(150.)).child(who(th, &e.source)).child(div().text_size(px(11.)).text_color(th.tx4).child(format!("added {}", ago(e.added)))))
            })
            .child(div().flex().gap(px(6.)).when(tight, |d| d.ml_auto()).child(up).child(down).child(toggle).child(remove))
            .into_any_element()
    }

    fn render_failed(&mut self, th: &Theme, e: &QueueEntry, last: bool, r: usize, rows: &[(Row, Vec<Btn>)], cx: &mut Context<Self>) -> AnyElement {
        let id = e.product_id;
        let n = rows.get(r).map(|x| x.1.len()).unwrap_or(2);
        let thumb = self.thumb(th, id, 34., 40., cx);
        let err = e.error.clone().unwrap_or_else(|| "Failed".into());
        let progress_txt = (e.total_bytes > 0).then(|| format!(" · stopped at {} of {}", gb(e.done_bytes), gb(e.total_bytes)));
        let tight = self.dl_tight();
        seg(th, false, last)
            .when(tight, |d| d.flex_wrap())
            .items_center()
            .gap_x(px(14.))
            .gap_y(px(8.))
            .px(px(16.))
            .py(px(10.))
            .when(self.focused(Target::Body(r)), |d| d.bg(th.ink3))
            .child(icon(Icon::CircleX, 16., th.bad))
            .child(thumb)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .when(tight, |d| d.min_w(px(160.)))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_x(px(8.))
                            .child(div().font_weight(SEMIBOLD).child(self.game_name(id)))
                            .child(div().text_color(th.tx3).text_size(px(12.)).child(format!("{} failed{}", kind_word(e.kind), progress_txt.unwrap_or_default()))),
                    )
                    .child(div().text_size(px(12.)).text_color(th.bad).truncate().child(err)),
            )
            .when(!tight, |d| d.child(who(th, &e.source)))
            .child(
                button(("f-retry", id as usize), th, Kind::Secondary, Size::Small, Some(Icon::RotateCcw), "Retry")
                    .when(tight, |d| d.ml_auto())
                    .focus_ring(self.btn_focused(r, 0, n), th, 6.)
                    .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Retry(id), cx))),
            )
            .child(
                button(("f-x", id as usize), th, Kind::Ghost, Size::Small, None, "Remove")
                    .focus_ring(self.btn_focused(r, 1, n), th, 6.)
                    .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Remove(id), cx))),
            )
            .into_any_element()
    }

    fn render_recent(&mut self, th: &Theme, j: &Job, last: bool, r: usize, cx: &mut Context<Self>) -> AnyElement {
        let id = j.product_id;
        let bad = j.error.is_some();
        let cancelled = j.status.starts_with("Cancelled");
        let (c, i) = if bad {
            (th.bad, Icon::CircleX)
        } else if cancelled {
            (th.tx3, Icon::CircleDot)
        } else {
            (th.ok, Icon::CircleCheck)
        };
        let what = match j.kind {
            JobKind::Verify => "Verify",
            JobKind::Import => "Import",
            _ => "Download",
        };
        let detail = match (&j.error, j.detail.len()) {
            (Some(e), _) => e.clone(),
            (None, 0) => String::new(),
            (None, n) if j.kind == JobKind::Import => format!("{n} other file{} left in place (mods, extras)", if n == 1 { "" } else { "s" }),
            (None, n) => format!("{n} file{} listed", if n == 1 { "" } else { "s" }),
        };
        let n = if r == self.dl_rows().iter().position(|(x, _)| matches!(x, Row::Recent(_))).unwrap_or(usize::MAX) { 2 } else { 1 };
        seg(th, false, last)
            .items_center()
            .gap(px(12.))
            .px(px(16.))
            .py(px(9.))
            .when(self.focused(Target::Body(r)) && self.dl.col == 0, |d| d.bg(th.ink3))
            .child(icon(i, 15., c))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_none().font_weight(MEDIUM).child(self.game_name(id)))
                            .child(div().min_w_0().text_color(th.tx3).truncate().child(format!("· {}", j.status))),
                    )
                    .when(!detail.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(if bad { th.bad } else { th.tx3 }).truncate().child(detail))),
            )
            .when(!self.dl_tight(), |d| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(what)))
            .when(j.total > 0, |d| d.child(mono(11.5, th.tx3).child(gb(j.downloaded.max(j.total.min(j.downloaded))))))
            .child(
                ibtn(("r-open", j.id as usize), th, Icon::ChevronRight, None, self.btn_focused(r, 0, n))
                    .on_click(cx.listener(move |this, _, _, cx| this.press(Btn::Open(id), cx))),
            )
            .into_any_element()
    }

    fn render_storage(&self, th: &Theme) -> AnyElement {
        let mut c = card(th).p(px(16.)).gap(px(12.)).child(
            div()
                .flex()
                .items_center()
                .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child("Storage"))
                .child(div().flex_1())
                .child(div().text_size(px(11.5)).text_color(th.tx3).child("free space on each disk")),
        );
        let Some(s) = &self.dl.setup else {
            return c.child(div().text_color(th.tx3).child("Asking quiverd…")).into_any_element();
        };
        let need: u64 = self.dl.queue.iter().filter(|e| e.state != QueueState::Failed).map(|e| e.total_bytes.saturating_sub(e.done_bytes)).sum();
        let disk = |label: &str, path: &std::path::Path, free: Option<u64>, total: Option<u64>| {
            let used = match (free, total) {
                (Some(f), Some(t)) if t > 0 => 1. - f as f32 / t as f32,
                _ => 0.,
            };
            let color = if used > 0.92 { th.bad } else if used > 0.8 { th.warn } else { th.acc };
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .child(div().text_color(th.tx2).font_weight(MEDIUM).child(label.to_owned()))
                        .child(div().flex_1())
                        .child(div().text_color(th.tx2).flex_none().child(free.map(|f| format!("{} free", size(f))).unwrap_or_else(|| "?".into()))),
                )
                .child(mono(11.5, th.tx3).truncate().child(tilde(path)))
                .child(progress(th, used, None, 8., Some(color)))
                .when_some(total, |d, t| d.child(div().text_size(px(11.)).text_color(th.tx4).child(format!("{:.0}% of {} used", used * 100., size(t)))))
        };
        c = c.child(disk("Games", &s.library, s.library_free, s.library_total));
        let same_disk = s.library_total == s.prefixes_total && s.library_free == s.prefixes_free;
        if same_disk {
            c = c.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(div().text_color(th.tx2).font_weight(MEDIUM).child("Compatibility folders"))
                            .child(div().flex_1())
                            .child(div().text_color(th.tx4).child("same disk")),
                    )
                    .child(mono(11.5, th.tx3).truncate().child(tilde(&s.prefixes))),
            );
        } else {
            c = c.child(disk("Compatibility folders", &s.prefixes, s.prefixes_free, s.prefixes_total));
        }
        c.child(div().h(px(1.)).bg(th.line1))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(px(12.))
                    .child(div().flex().child(div().text_color(th.tx3).flex_1().child("Installed by Quiver")).child(div().text_color(th.tx2).child(format!("{} games", s.installed))))
                    .child(div().flex().child(div().text_color(th.tx3).flex_1().child("Still to download in the queue")).child(div().text_color(th.tx2).child(if need > 0 { gb(need) } else { "—".into() }))),
            )
            .child(div().text_size(px(11.5)).text_color(th.tx4).child("A download won't start unless it fits with 1 GiB or 5% to spare."))
            .into_any_element()
    }

    fn render_rules(&mut self, th: &Theme, rows: &[(Row, Vec<Btn>)], cx: &mut Context<Self>) -> AnyElement {
        let s = self.dl.settings.clone();
        let limit_mb = s.as_ref().and_then(|s| s.bandwidth_limit).map(|b| b >> 20).unwrap_or(0);
        let max = s.as_ref().map(|s| s.max_active).unwrap_or(1);
        let auto = s.as_ref().is_some_and(|s| s.auto_update);
        let ri = |r: Row| rows.iter().position(|(x, _)| *x == r).unwrap_or(usize::MAX);
        let (lr, mr, ar) = (ri(Row::Limit), ri(Row::MaxActive), ri(Row::AutoUpdate));
        let chip = |key: &'static str, i: usize, label: String, on: bool, f: bool, b: Btn, cx: &mut Context<Self>| {
            let (bg, border, fg) = if on { (th.acc_soft, th.acc_line, th.acc_hi) } else { (th.ink3, th.line2, th.tx2) };
            div()
                .id((key, i))
                .flex()
                .items_center()
                .h(px(26.))
                .px(px(9.))
                .rounded(px(7.))
                .bg(bg)
                .border_1()
                .border_color(border)
                .text_size(px(12.))
                .text_color(fg)
                .cursor_pointer()
                .child(label)
                .focus_ring(f, th, 7.)
                .on_click(cx.listener(move |this, _, _, cx| this.press(b, cx)))
        };
        let limits = div().flex().flex_wrap().gap(px(6.)).children(LIMITS.iter().enumerate().map(|(i, m)| {
            let label = if *m == 0 { "Unlimited".to_owned() } else { format!("{m} MB/s") };
            chip("lim", i, label, *m == limit_mb, self.btn_focused(lr, i, LIMITS.len()), Btn::Limit(*m), cx).into_any_element()
        }));
        let actives = div().flex().gap(px(6.)).children((1..=3).map(|n| chip("max", n, n.to_string(), n == max, self.btn_focused(mr, n - 1, 3), Btn::MaxActive(n), cx).into_any_element()));
        let auto_t = div()
            .id("auto-up")
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .rounded(px(9.))
            .child(super::game::toggle(th, auto))
            .focus_ring(self.btn_focused(ar, 0, 1), th, 9.)
            .on_click(cx.listener(|this, _, _, cx| this.press(Btn::AutoUpdate, cx)));
        let label = |t: &str, help: &str| {
            div().flex().flex_col().gap(px(2.)).child(div().text_color(th.tx1).child(t.to_owned())).child(div().text_size(px(11.5)).text_color(th.tx3).child(help.to_owned()))
        };
        let limit_arg = if limit_mb == 0 { "off".to_owned() } else { format!("{limit_mb}M") };
        card(th)
            .p(px(16.))
            .gap(px(14.))
            .text_size(px(12.5))
            .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child("Download rules"))
            .child(div().flex().flex_col().gap(px(8.)).child(label("Speed limit", "Across every download")).child(limits))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(label("At the same time", "Downloads that run together").flex_1())
                    .child(actives),
            )
            .child(div().flex().items_center().gap(px(10.)).child(label("Auto-update", "Queue updates when the library shows one").flex_1()).child(auto_t))
            .child(command_line(th, format!("quiver queue settings --max-active {max} --limit {limit_arg} --auto-update {}", if auto { "on" } else { "off" })))
            .when(s.is_none(), |d| d.opacity(0.6))
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Dialogs

    pub fn open_install_dialog(&mut self, id: u32, cx: &mut Context<Self>) {
        if self.game(id).is_some_and(|g| g.installed()) {
            return self.open_update_dialog(id, cx);
        }
        // Steam installs (and updates) its own copies; there's no Ubisoft build to get.
        if let Some(g) = self.game(id).filter(|g| g.steam_only() && !g.entry.installable) {
            let name = crate::model::clean_name(&g.entry.name);
            return self.show_toast(format!("{name} is installed through Steam, which updates it"), cx);
        }
        let steam = self.dl.setup.as_ref().and_then(|s| s.first_run.as_ref()).is_none_or(|f| f.steam_on_install);
        let steam_there = matches!(&self.ui.steam, Some(Ok(s)) if s.root.is_some());
        self.nav.close_overlay();
        self.dl.dialog = Some(DlDialog::Install { id, language: None, preview: None, after: After { steam: steam && steam_there, desktop: true } });
        self.dl.dsel = usize::MAX;
        self.request_install_preview(id, None, cx);
    }

    fn request_install_preview(&mut self, id: u32, language: Option<String>, cx: &mut Context<Self>) {
        self.call::<InstallPreview, _>(Request::InstallPreview { product_id: id, language: language.clone() }, cx, move |this, r, _| {
            if let Some(DlDialog::Install { id: did, preview, language: lang, .. }) = &mut this.dl.dialog
                && *did == id
                && *lang == language
            {
                if let Ok(p) = &r {
                    *lang = Some(p.language.clone());
                }
                *preview = Some(r.map_err(|e| err_text(&e)));
                this.dl.dsel = usize::MAX;
            }
        });
    }

    pub fn open_update_dialog(&mut self, id: u32, cx: &mut Context<Self>) {
        self.nav.close_overlay();
        self.dl.dialog = Some(DlDialog::Update { id, preview: None });
        self.dl.dsel = usize::MAX;
        self.call::<UpdatePreview, _>(Request::UpdatePreview { product_id: id }, cx, move |this, r, _| {
            if let Some(DlDialog::Update { id: did, preview }) = &mut this.dl.dialog
                && *did == id
            {
                *preview = Some(r.map_err(|e| err_text(&e)));
                this.dl.dsel = usize::MAX;
            }
        });
    }

    pub fn open_verify_dialog(&mut self, id: u32, cx: &mut Context<Self>) {
        self.nav.close_overlay();
        // A verify already running for this game (from the CLI, say) shows its progress.
        let job = self.status.as_ref().and_then(|s| s.jobs.iter().filter(|j| j.kind == JobKind::Verify && j.product_id == id && !j.done).map(|j| j.id).max());
        self.dl.dialog = Some(DlDialog::Verify { id, report: None, job });
        self.dl.dsel = usize::MAX;
        self.load_verify_report(id, cx);
    }

    fn load_verify_report(&mut self, id: u32, cx: &mut Context<Self>) {
        self.call::<Option<VerifyReport>, _>(Request::VerifyResult { product_id: id }, cx, move |this, r, _| {
            if let Some(DlDialog::Verify { id: did, report, .. }) = &mut this.dl.dialog
                && *did == id
            {
                *report = Some(r.map_err(|e| err_text(&e)));
                this.dl.dsel = usize::MAX;
            }
        });
    }

    /// The dialog's controls in focus order; the primary one is last.
    fn dl_dialog_controls(&self) -> Vec<DCtl> {
        match &self.dl.dialog {
            None => vec![],
            Some(DlDialog::Install { preview, .. }) => {
                let mut v = vec![];
                if let Some(Ok(p)) = preview {
                    v.extend((0..p.languages.len()).map(DCtl::Lang));
                }
                v.extend([DCtl::Steam, DCtl::Desktop, DCtl::Cancel, DCtl::Confirm]);
                v
            }
            Some(DlDialog::Update { preview, .. }) => match preview {
                Some(Ok(p)) if p.up_to_date => vec![DCtl::Cancel, DCtl::Verify],
                _ => vec![DCtl::Cancel, DCtl::Confirm],
            },
            Some(DlDialog::Verify { report, job, .. }) => {
                let running = self.job(*job).is_some_and(|j| !j.done);
                let bad = matches!(report, Some(Ok(Some(r))) if !r.is_ok());
                let mut v = vec![DCtl::Cancel];
                if !running {
                    v.push(DCtl::Verify);
                    if bad {
                        v.push(DCtl::Repair);
                    }
                }
                v
            }
        }
    }

    fn dl_dialog_sel(&self) -> usize {
        let n = self.dl_dialog_controls().len();
        if self.dl.dsel == usize::MAX { n.saturating_sub(1) } else { self.dl.dsel.min(n.saturating_sub(1)) }
    }

    fn dl_dialog_press(&mut self, c: DCtl, cx: &mut Context<Self>) {
        let ctl = self.dl_dialog_controls();
        if let Some(i) = ctl.iter().position(|x| *x == c) {
            self.dl.dsel = i;
        }
        if c == DCtl::Cancel {
            self.dl.dialog = None;
            cx.notify();
            return;
        }
        // Change the dialog's own state here; requests go out below.
        enum Do {
            Nothing,
            Lang(u32, String),
            Install(u32, Option<String>, After),
            Update(u32),
            OpenVerify(u32),
            Verify(u32),
            Repair(u32),
        }
        let what = match (self.dl.dialog.as_mut(), c) {
            (Some(DlDialog::Install { id, language, preview, .. }), DCtl::Lang(i)) => {
                let code = match preview {
                    Some(Ok(p)) => p.languages.get(i).map(|l| l.code.clone()),
                    _ => None,
                };
                match code {
                    Some(l) if language.as_deref() != Some(l.as_str()) => {
                        *language = Some(l.clone());
                        *preview = None;
                        Do::Lang(*id, l)
                    }
                    _ => Do::Nothing,
                }
            }
            (Some(DlDialog::Install { after, .. }), DCtl::Steam) => {
                after.steam = !after.steam;
                Do::Nothing
            }
            (Some(DlDialog::Install { after, .. }), DCtl::Desktop) => {
                after.desktop = !after.desktop;
                Do::Nothing
            }
            (Some(DlDialog::Install { id, language, preview, after }), DCtl::Confirm) => match preview {
                Some(Ok(p)) if p.fits => Do::Install(*id, language.clone(), *after),
                _ => Do::Nothing,
            },
            (Some(DlDialog::Update { id, .. }), DCtl::Confirm) => Do::Update(*id),
            (Some(DlDialog::Update { id, .. }), DCtl::Verify) => Do::OpenVerify(*id),
            (Some(DlDialog::Verify { id, job, .. }), DCtl::Verify) => {
                *job = None;
                Do::Verify(*id)
            }
            (Some(DlDialog::Verify { id, .. }), DCtl::Repair) => Do::Repair(*id),
            _ => Do::Nothing,
        };
        match what {
            Do::Nothing => {}
            Do::Lang(id, l) => self.request_install_preview(id, Some(l), cx),
            Do::Install(id, language, after) => {
                let name = self.game_name(id);
                let busy = self.dl.queue.iter().any(|e| e.state.is_active());
                self.dl.dialog = None;
                self.call::<u64, _>(Request::Install { product_id: id, language }, cx, move |this, r, cx| match r {
                    Ok(_) => {
                        if after != After::default() {
                            this.dl.after.insert(id, after);
                        }
                        this.show_toast(if busy { format!("Queued {name}: it starts when the download ahead of it finishes") } else { format!("Installing {name}") }, cx);
                        this.load_queue(cx);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            Do::Update(id) => {
                let name = self.game_name(id);
                self.dl.dialog = None;
                self.call::<u64, _>(Request::Update { product_id: id }, cx, move |this, r, cx| match r {
                    Ok(_) => {
                        this.show_toast(format!("Updating {name}"), cx);
                        this.load_queue(cx);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            Do::OpenVerify(id) => self.open_verify_dialog(id, cx),
            Do::Verify(id) => {
                self.call::<u64, _>(Request::Verify { product_id: id, repair: false }, cx, move |this, r, cx| match r {
                    Ok(j) => {
                        if let Some(DlDialog::Verify { id: did, job, .. }) = &mut this.dl.dialog
                            && *did == id
                        {
                            *job = Some(j);
                            this.dl.dsel = 0;
                        }
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            Do::Repair(id) => {
                let name = self.game_name(id);
                self.dl.dialog = None;
                self.call::<u64, _>(Request::Verify { product_id: id, repair: true }, cx, move |this, r, cx| match r {
                    Ok(_) => {
                        this.show_toast(format!("Repair of {name} queued: it re-downloads only the slices that failed"), cx);
                        this.nav.open_section(Section::Downloads);
                        this.load_queue(cx);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
        }
        cx.notify();
    }

    /// The welcome flow and the dialogs, drawn over everything.
    pub fn render_dl_overlay(&mut self, th: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(w) = self.render_welcome(th, window, cx) {
            return Some(w);
        }
        self.dl.dialog.as_ref()?;
        Some(self.render_dl_dialog(th, cx))
    }

    fn render_dl_dialog(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ctl = self.dl_dialog_controls();
        let sel = self.dl_dialog_sel();
        let keys = self.mode != InputMode::Pointer;
        let f = |c: DCtl| keys && ctl.get(sel) == Some(&c);
        let dialog = self.dl.dialog.clone();
        let (id, title, sub, body, cli, primary): (u32, String, String, Vec<AnyElement>, Option<String>, Option<(Icon, String, bool)>) = match &dialog {
            Some(DlDialog::Install { id, language, preview, after }) => {
                let (id, after) = (*id, *after);
                let name = self.game_name(id);
                let lang = language.clone();
                let mut body = vec![];
                let (sub, ok) = match preview {
                    None => {
                        body.push(loading(th, "Reading the manifest: sizes, free space and languages…"));
                        ("Asking Ubisoft for the latest build…".to_owned(), false)
                    }
                    Some(Err(e)) => {
                        body.push(error_box(th, e));
                        ("Couldn't read the build".to_owned(), false)
                    }
                    Some(Ok(p)) => {
                        let p = p.clone();
                        body.push(self.install_preview_body(th, &p, &f, cx));
                        (format!("Version {}  ·  into {}", p.version.clone().unwrap_or_else(|| "latest".into()), tilde(&p.dest)), p.fits)
                    }
                };
                body.push(self.finish_choice(th, after, &f, cx));
                let cli = format!("quiver install {id}{}", lang.map(|l| format!(" --language {l}")).unwrap_or_default());
                (id, format!("Install {name}"), sub, body, Some(cli), Some((Icon::Download, "Install".into(), ok)))
            }
            Some(DlDialog::Update { id, preview }) => {
                let id = *id;
                let name = self.game_name(id);
                let mut body = vec![];
                let (sub, primary) = match preview {
                    None => {
                        body.push(loading(th, "Comparing the installed build with the latest…"));
                        ("Asking Ubisoft for the latest build…".to_owned(), Some((Icon::ArrowDownToLine, "Update".to_owned(), false)))
                    }
                    Some(Err(e)) => {
                        body.push(error_box(th, e));
                        ("Couldn't compare builds".to_owned(), Some((Icon::ArrowDownToLine, "Update".to_owned(), false)))
                    }
                    Some(Ok(p)) => {
                        let p = p.clone();
                        body.push(update_body(th, &p));
                        let v = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
                        if p.up_to_date {
                            (format!("Version {} is the latest", v(&p.installed_version)), Some((Icon::ShieldCheck, "Verify files…".to_owned(), true)))
                        } else {
                            (format!("{} → {}", v(&p.installed_version), v(&p.latest_version)), Some((Icon::ArrowDownToLine, "Update".to_owned(), true)))
                        }
                    }
                };
                (id, format!("Update {name}"), sub, body, Some(format!("quiver update {id} --preview")), primary)
            }
            Some(DlDialog::Verify { id, report, job }) => {
                let id = *id;
                let name = self.game_name(id);
                let running = self.job(*job).filter(|j| !j.done).cloned();
                let mut body = vec![div()
                    .text_size(px(12.5))
                    .line_height(px(19.))
                    .text_color(th.tx2)
                    .child("Hashes every file the manifest lists against Ubisoft's checksums, slice by slice. Files the game doesn't ship (mods, extra DLLs, configs) are never read or written; a repair re-downloads only the slices that fail.")
                    .into_any_element()];
                if let Some(j) = &running {
                    let f = if j.total > 0 { j.downloaded as f32 / j.total as f32 } else { 0. };
                    body.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(14.))
                            .rounded(px(10.))
                            .bg(th.ink1)
                            .border_1()
                            .border_color(th.line1)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(icon(Icon::LoaderCircle, 14., th.acc_hi))
                                    .child(div().font_weight(SEMIBOLD).child("Verifying…"))
                                    .child(div().flex_1())
                                    .child(div().text_color(th.tx3).text_size(px(12.)).child(if j.total > 0 { format!("{} of {} read", gb(j.downloaded), gb(j.total)) } else { j.status.clone() })),
                            )
                            .child(progress(th, f, None, 6., None))
                            .into_any_element(),
                    );
                }
                match report {
                    None => body.push(loading(th, "Reading the last report…")),
                    Some(Err(e)) => body.push(error_box(th, e)),
                    Some(Ok(None)) if running.is_none() => body.push(
                        div().text_color(th.tx3).text_size(px(12.5)).child("Not verified yet on this machine.").into_any_element(),
                    ),
                    Some(Ok(None)) => {}
                    Some(Ok(Some(r))) => body.push(verify_report(th, r, running.is_some())),
                }
                let sub = match report {
                    Some(Ok(Some(r))) => format!(
                        "Last checked {}{}",
                        r.at.map(ago).unwrap_or_else(|| "earlier".into()),
                        r.version.as_ref().map(|v| format!(" · version {v}")).unwrap_or_default()
                    ),
                    _ => "Check the install against its manifest".into(),
                };
                let bad = matches!(report, Some(Ok(Some(r))) if !r.is_ok());
                let primary = if running.is_some() {
                    None
                } else if bad {
                    let n = match report {
                        Some(Ok(Some(r))) => r.missing.len() + r.corrupt.len(),
                        _ => 0,
                    };
                    Some((Icon::Download, format!("Repair {n} file{}", if n == 1 { "" } else { "s" }), true))
                } else {
                    Some((Icon::ShieldCheck, if matches!(report, Some(Ok(Some(_)))) { "Verify again".into() } else { "Verify files".into() }, true))
                };
                (id, format!("Verify {name}"), sub, body, Some(format!("quiver verify {id}{}", if bad { " --repair" } else { "" })), primary)
            }
            None => return div().into_any_element(),
        };

        let thumb = self.thumb(th, id, 40., 47., cx);
        let width = 640.;
        let left = ((self.viewport.0 - width) / 2.).max(16.);
        let mut buttons = div().flex().items_center().justify_end().gap(px(8.));
        let cancel_label = if matches!(self.dl.dialog, Some(DlDialog::Verify { .. })) { "Close" } else { "Cancel" };
        buttons = buttons.child(
            button("dlg-cancel", th, Kind::Secondary, Size::Medium, None, cancel_label)
                .focus_ring(f(DCtl::Cancel), th, 8.)
                .on_click(cx.listener(|this, _, _, cx| this.dl_dialog_press(DCtl::Cancel, cx))),
        );
        // Verify: "Verify again" next to "Repair".
        if matches!(self.dl.dialog, Some(DlDialog::Verify { .. })) && ctl.contains(&DCtl::Repair) {
            buttons = buttons.child(
                button("dlg-verify", th, Kind::Secondary, Size::Medium, Some(Icon::ShieldCheck), "Verify again")
                    .focus_ring(f(DCtl::Verify), th, 8.)
                    .on_click(cx.listener(|this, _, _, cx| this.dl_dialog_press(DCtl::Verify, cx))),
            );
        }
        if let Some((i, label, enabled)) = primary {
            let c = *ctl.last().unwrap_or(&DCtl::Confirm);
            buttons = buttons.child(
                button("dlg-ok", th, Kind::Primary, Size::Medium, Some(i), label)
                    .when(!enabled, |d| d.opacity(0.5))
                    .focus_ring(f(c), th, 8.)
                    .on_click(cx.listener(move |this, _, _, cx| this.dl_dialog_press(c, cx))),
            );
        }
        div()
            .id("dl-scrim")
            .absolute()
            .inset_0()
            .bg(th.scrim)
            .on_click(cx.listener(|this, _, _, cx| {
                this.dl.dialog = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("dl-dialog")
                    .absolute()
                    .top(px((self.viewport.1 * 0.12).max(60.)))
                    .left(px(left))
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
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
                            .gap(px(14.))
                            .child(thumb)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(3.))
                                    .child(div().text_size(px(17.)).font_weight(BOLD).truncate().child(title))
                                    .child(div().text_size(px(12.5)).text_color(th.tx3).truncate().child(sub)),
                            ),
                    )
                    .children(body)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .pt(px(4.))
                            .when_some(cli, |d, c| d.child(command_line(th, c).flex_1().min_w_0()))
                            .child(buttons),
                    ),
            )
            .into_any_element()
    }

    fn install_preview_body(&mut self, th: &Theme, p: &InstallPreview, f: &dyn Fn(DCtl) -> bool, cx: &mut Context<Self>) -> AnyElement {
        let stat = |label: &str, value: String, note: Option<String>| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(3.))
                .p(px(12.))
                .rounded(px(10.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(caption(label, th.tx3))
                .child(div().text_size(px(18.)).font_weight(BOLD).child(value))
                .child(div().text_size(px(11.5)).text_color(th.tx3).child(note.unwrap_or_default()))
        };
        let resumed = p.remaining_download_bytes < p.download_bytes;
        let need = p.required_bytes + p.margin_bytes;
        let after = p.free_bytes.saturating_sub(p.required_bytes);
        let fit_color = if p.fits { th.ok } else { th.bad };
        let used = if p.free_bytes > 0 { (need as f32 / p.free_bytes as f32).min(1.) } else { 1. };
        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(stat("Download", gb(p.remaining_download_bytes), resumed.then(|| format!("{} already on disk", gb(p.download_bytes - p.remaining_download_bytes))).or(Some("from Ubisoft's CDN".into()))))
                    .child(stat("On disk", gb(p.install_bytes), Some(format!("{} to write now", gb(p.required_bytes)))))
                    .child(stat("Free", size(p.free_bytes), Some(if p.fits { format!("{} left after", size(after)) } else { "not enough".into() }))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(progress(th, used, None, 8., Some(fit_color)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .text_color(fit_color)
                            .child(icon(if p.fits { Icon::CircleCheck } else { Icon::TriangleAlert }, 13., fit_color))
                            .child(if p.fits {
                                format!("Fits, with {} kept free as a margin", gb(p.margin_bytes))
                            } else {
                                format!("Doesn't fit: needs {} plus a {} margin; {} is free", gb(p.required_bytes), gb(p.margin_bytes), size(p.free_bytes))
                            }),
                    ),
            );
        if p.languages.len() > 1 {
            let chips = div().flex().flex_wrap().gap(px(6.)).children(p.languages.iter().enumerate().map(|(i, l)| {
                let on = l.code == p.language;
                let (bg, border, fg) = if on { (th.acc_soft, th.acc_line, th.acc_hi) } else { (th.ink3, th.line2, th.tx2) };
                div()
                    .id(("lang", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(px(7.))
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .text_size(px(12.))
                    .text_color(fg)
                    .cursor_pointer()
                    .child(l.code.clone())
                    .when(l.download_bytes > 0, |d| d.child(div().text_color(th.tx3).child(format!("+{}", gb(l.download_bytes)))))
                    .focus_ring(f(DCtl::Lang(i)), th, 7.)
                    .on_click(cx.listener(move |this, _, _, cx| this.dl_dialog_press(DCtl::Lang(i), cx)))
                    .into_any_element()
            }));
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().flex().items_center().gap(px(8.)).child(caption("Language", th.tx3)).child(div().text_size(px(11.5)).text_color(th.tx4).child("voice and text; sizes are each language's own files")))
                    .child(chips),
            );
        }
        col.into_any_element()
    }

    fn finish_choice(&mut self, th: &Theme, after: After, f: &dyn Fn(DCtl) -> bool, cx: &mut Context<Self>) -> AnyElement {
        let steam_running = matches!(&self.ui.steam, Some(Ok(s)) if s.running && s.root.is_some());
        let line = |key: &'static str, on: bool, label: &str, c: DCtl, cx: &mut Context<Self>| {
            div()
                .id(key)
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(4.))
                .py(px(2.))
                .rounded(px(6.))
                .cursor_pointer()
                .child(checkbox(th, on))
                .child(label.to_owned())
                .focus_ring(f(c), th, 6.)
                .on_click(cx.listener(move |this, _, _, cx| this.dl_dialog_press(c, cx)))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(12.))
            .rounded(px(10.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line1)
            .text_size(px(12.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(caption("When it finishes", th.tx3))
                    .child(line("dlg-steam", after.steam, "Add to Steam", DCtl::Steam, cx))
                    .child(line("dlg-desk", after.desktop, "Create .desktop entry", DCtl::Desktop, cx)),
            )
            .when(after.steam, |d| {
                d.child(div().text_size(px(11.5)).text_color(th.tx3).child(if steam_running {
                    "Steam is running: the shortcut waits for Steam's next restart. Quiver never restarts Steam without asking."
                } else {
                    "Steam shows new shortcuts when it next starts. Quiver never restarts Steam without asking."
                }))
            })
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Demo setup (`QUIVER_DEMO_GLOBAL`)

    fn start_downloads_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let flags = crate::ui::global::demo_flags();
        if flags.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let ready = this.update_in(cx, |this, _, _| matches!(this.conn, Conn::Ready) && !this.games.is_empty()).unwrap_or(true);
                if ready {
                    break;
                }
            }
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let _ = this.update_in(cx, |this, _, cx| {
                for f in crate::ui::global::demo_flags() {
                    let (k, v) = f.split_once('=').unwrap_or((f.as_str(), ""));
                    let id = v.parse::<u32>().ok();
                    match (k, id) {
                        ("downloads", _) => {
                            this.nav.open_section(Section::Downloads);
                            this.load_queue(cx);
                            this.load_setup(cx);
                        }
                        ("install", Some(id)) => this.open_install_dialog(id, cx),
                        ("update", Some(id)) => this.open_update_dialog(id, cx),
                        ("verify", Some(id)) => this.open_verify_dialog(id, cx),
                        ("dl-focus", Some(i)) => {
                            this.nav.open_section(Section::Downloads);
                            this.nav.focus(&this.layout, Target::Body(i as usize));
                            this.mode = InputMode::Pad;
                        }
                        ("dl-col", Some(c)) => this.dl.col = c as usize,
                        ("after", Some(id)) => {
                            this.dl.after.insert(id, After { steam: true, desktop: true });
                        }
                        (k, _) => this.welcome_demo(k, v, cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn loading(th: &Theme, text: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .p(px(14.))
        .rounded(px(10.))
        .bg(th.ink1)
        .text_color(th.tx3)
        .child(icon(Icon::LoaderCircle, 14., th.tx3))
        .child(text.to_owned())
        .into_any_element()
}

fn error_box(th: &Theme, e: &str) -> AnyElement {
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .px(px(12.))
        .py(px(10.))
        .rounded(px(8.))
        .bg(th.bad_soft)
        .text_size(px(12.5))
        .text_color(th.bad)
        .child(div().pt(px(1.)).child(icon(Icon::CircleX, 14., th.bad)))
        .child(div().flex_1().min_w_0().child(e.to_owned()))
        .into_any_element()
}

fn file_list(th: &Theme, title: &str, color: gpui::Hsla, files: &[String], total: usize) -> Div {
    let more = total.saturating_sub(files.len());
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().flex().items_center().gap(px(8.)).child(caption(title, color)).child(div().text_size(px(11.5)).text_color(th.tx3).child(total.to_string())))
        .children(files.iter().map(|f| mono(11.5, th.tx2).truncate().child(f.clone())))
        .when(more > 0, |d| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("and {more} more"))))
}

fn update_body(th: &Theme, p: &UpdatePreview) -> AnyElement {
    if p.up_to_date {
        return div()
            .flex()
            .items_center()
            .gap(px(10.))
            .p(px(14.))
            .rounded(px(10.))
            .bg(th.ok_soft)
            .text_color(th.ok)
            .child(icon(Icon::CircleCheck, 16., th.ok))
            .child(format!("Up to date: all {} files match the latest build. Nothing to download.", p.unchanged))
            .into_any_element();
    }
    let stat = |label: &str, value: String| {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap(px(3.))
            .p(px(12.))
            .rounded(px(10.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line1)
            .child(caption(label, th.tx3))
            .child(div().text_size(px(18.)).font_weight(BOLD).child(value))
    };
    let take = |v: &[String]| v.iter().take(4).cloned().collect::<Vec<_>>();
    let mut lists = div().flex().gap(px(16.));
    if !p.changed.is_empty() {
        lists = lists.child(file_list(th, "Changed", th.acc_hi, &take(&p.changed), p.changed.len()).flex_1().min_w_0());
    }
    if !p.added.is_empty() {
        lists = lists.child(file_list(th, "Added", th.ok, &take(&p.added), p.added.len()).flex_1().min_w_0());
    }
    if !p.removed.is_empty() {
        lists = lists.child(file_list(th, "Removed", th.bad, &take(&p.removed), p.removed.len()).flex_1().min_w_0());
    }
    div()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(
            div()
                .flex()
                .gap(px(10.))
                .child(stat("Download", gb(p.download_bytes)))
                .child(stat("Rewritten", gb(p.write_bytes)))
                .child(stat("Files", format!("{} of {}", p.changed.len() + p.added.len(), p.changed.len() + p.added.len() + p.unchanged))),
        )
        .child(lists)
        .child(div().text_size(px(11.5)).text_color(th.tx3).child(format!(
            "{} files stay as they are. Slices already on disk are reused, so only what changed is downloaded.",
            p.unchanged
        )))
        .into_any_element()
}

fn verify_report(th: &Theme, r: &VerifyReport, stale: bool) -> AnyElement {
    let ok = r.is_ok();
    let (c, bg, i) = if ok { (th.ok, th.ok_soft, Icon::CircleCheck) } else { (th.bad, th.bad_soft, Icon::TriangleAlert) };
    let headline = if r.repaired_files > 0 {
        format!("Repaired {} files ({} downloaded)", r.repaired_files, gb(r.repaired_bytes))
    } else if ok {
        format!("All {} files OK · {} read", super::welcome::thousands(r.files), gb(r.bytes))
    } else {
        format!("{} missing, {} corrupt of {} files", r.missing.len(), r.corrupt.len(), super::welcome::thousands(r.files))
    };
    let corrupt: Vec<String> = r.corrupt.iter().take(4).map(|c| format!("{}  ({} bad slice{}, {})", c.path, c.bad_slices, if c.bad_slices == 1 { "" } else { "s" }, gb(c.bad_bytes))).collect();
    let missing: Vec<String> = r.missing.iter().take(4).cloned().collect();
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(14.))
        .rounded(px(10.))
        .bg(th.ink1)
        .border_1()
        .border_color(th.line1)
        .when(stale, |d| d.opacity(0.55))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(div().flex().items_center().justify_center().size(px(28.)).rounded(px(8.)).bg(bg).child(icon(i, 15., c)))
                .child(div().font_weight(SEMIBOLD).text_color(th.tx1).child(headline))
                .child(div().flex_1())
                .when_some(r.at, |d, t| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(ago(t)))),
        );
    if !r.corrupt.is_empty() {
        col = col.child(file_list(th, "Corrupt", th.bad, &corrupt, r.corrupt.len()));
    }
    if !r.missing.is_empty() {
        col = col.child(file_list(th, "Missing", th.warn, &missing, r.missing.len()));
    }
    if !r.unverified.is_empty() {
        col = col.child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("{} files carry no usable hash and were only size-checked.", r.unverified.len())));
    }
    if !ok {
        let bytes: u64 = r.corrupt.iter().map(|c| c.bad_bytes).sum();
        col = col.child(div().text_size(px(12.)).text_color(th.tx2).child(format!(
            "Repair downloads only what failed{}; mods and other files the game doesn't ship stay untouched.",
            if bytes > 0 { format!(" (about {} of slices, plus missing files)", gb(bytes)) } else { String::new() }
        )));
    }
    col.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(eta(206), "3 min 26 s");
        assert_eq!(eta(12), "12 s");
        assert_eq!(eta(3840), "1 h 04 min");
        assert_eq!(rate(84_600_000), "84.6 MB/s");
        assert_eq!(rate(212_000_000), "212 MB/s");
        assert_eq!(rate(500_000), "500 KB/s");
    }
}
