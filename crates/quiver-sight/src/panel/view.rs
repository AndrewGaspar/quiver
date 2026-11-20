//! The panel's GPUI view (SIGHT §9 "Panel", the SightSession / SightMods / docked mock-ups).
//! Two layouts, per section ([`super::state::DockPrefs`]):
//!
//! - **Centred**: over the blurred (or dimmed) game, a tab row on top ("Sight", LB, the
//!   sections, RB; the game, profile, clock and battery on the right), the section's body
//!   (its cards), and the hint bar along the bottom.
//! - **Docked** (Mods and Tweaks by default): a 500 px column on the left with no blur and no
//!   dim, so the game shows unobstructed beside it (and the HUD stays visible); its own header
//!   (the section, its icon tabs between LB and RB, "Docked R3"), the body stacked, and the hint
//!   bar. A change shows a chip at the top of the game area ([`super::sections::mods::Chip`]).
//!
//! R3 (hyprpad types Tab) swaps the two. Everything the view changes goes to quiverd through
//! the link ([`LinkReq`]); values show what was asked at once and what came of it when quiverd
//! answers, never optimism. Each section supplies its hints ([`PanelView::section_hints`],
//! [`super::hints`]).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    AnyElement, Bounds, Context, Div, FocusHandle, Hsla, IntoElement, KeyDownEvent, KeyUpEvent, ParentElement, Pixels, Render, ScrollHandle, SharedString, Styled, Window,
    canvas, div, prelude::*, px,
};
use quiver_api::options::{Live, OptionOp, OptionState, OptionsChange, SurfaceState};
use quiver_api::perf::{HudScope, PerfValue, Stat};
use quiver_api::sight::{Backdrop, PanelSection};
use quiver_ui::assets::{DISPLAY_FONT, MONO_FONT, UI_FONT};
use quiver_ui::components::{FocusExt, Icon, MEDIUM, PillKind, SEMIBOLD, caption, glyph, icon, pill};
use quiver_ui::input::Input;
use quiver_ui::options::{Tone, apply_badge, draft_value, pending_text, short_value};
use quiver_ui::theme::Theme;
use serde_json::Value;

use super::hints::{Glyph, Hints};
use super::sections::mods::{self, Chip, ChipWhen, Compare, SessionLog, TWEAKS};
use super::sections::performance::{self as perf, PerfSection};
use super::state::{ANCHORS, Act, Button, Effect, HudRow, Key, Load, Nav, PRESETS, PanelData, Row, Zone, adjustable, cycle, panel_key, step_scale, step_value};
use crate::link::{Change, LinkReq, Loaded, Msg, Why};

/// Widest the centred panel gets (ultra-wide outputs), logical px (SIGHT §9).
const MAX_W: f32 = 1400.0;
/// Safe area on every side, a fraction of the output (TVs).
const SAFE: f32 = 0.028;
/// The docked column: its width and its margin from the output's edges, logical px.
pub const DOCK_W: f32 = 500.0;
pub const DOCK_MARGIN: f32 = 24.0;
/// The centred layout's hint bar.
const BAR_H: f32 = 56.0;
/// Edits to a row wait this long for the next step before going to quiverd (one revision
/// per burst of presses, not one per press).
const DEBOUNCE: Duration = Duration::from_millis(350);
/// A Y release followed this soon by a press is key repeat, not a new press.
const Y_REPEAT_GAP: Duration = Duration::from_millis(60);

/// What the view needs from the rest of quiver-sight. All optional, so fixtures render it
/// off-screen without quiverd.
#[derive(Clone, Default)]
pub struct Wiring {
    pub link: Option<tokio::sync::mpsc::UnboundedSender<LinkReq>>,
    /// To quiver-sight's own loop (close requests, the layout).
    pub ui: Option<UnboundedSender<Msg>>,
    /// Graceful quit: closes the game's window (Hyprland).
    pub quit: Option<Rc<dyn Fn() -> Result<(), String>>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Footer {
    /// The controller in words ("Steam Controller", "no controller").
    pub pad: String,
    /// The game also sees the pad (no hyprpad to neutralise it).
    pub pad_shared: bool,
}

/// Where the panel's parts that float at their own depth are, for a stereo frame
/// (design/STEREO-UI.md): window logical px, padded by the background around each, clipped to
/// what's visible. Marker canvases fill them while the frame is laid out; the in-game publisher
/// turns them into the image's lifts. Back to front.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DepthMarks {
    /// The panel: the centred layout's content column, or the docked column.
    pub card: Option<Bounds<Pixels>>,
    /// The change chip over the game (docked), at the panel's depth.
    pub chip: Option<Bounds<Pixels>>,
    /// The focused row, surface or button.
    pub focus: Option<Bounds<Pixels>>,
    /// A dialog's box, and its focused button.
    pub dialog: Option<Bounds<Pixels>>,
    pub dialog_focus: Option<Bounds<Pixels>>,
}

#[derive(Clone, Copy)]
pub(crate) enum Mark {
    Card,
    Chip,
    Focus,
    Dialog,
    DialogFocus,
}

impl DepthMarks {
    /// Each part with its rung (thousandths, `quiver_perf::stereo::rung`), back to front. The
    /// chip gives way to a dialog (four lifts at most).
    pub fn parts(&self) -> impl Iterator<Item = (Bounds<Pixels>, u16)> + '_ {
        use quiver_perf::stereo::rung;
        let chip = self.chip.filter(|_| self.dialog.is_none());
        [(self.card, rung::PANEL), (chip, rung::PANEL), (self.focus, rung::FOCUS), (self.dialog, rung::DIALOG), (self.dialog_focus, rung::DIALOG_FOCUS)]
            .into_iter()
            .filter_map(|(b, d)| b.map(|b| (b, d)))
    }

    fn slot(&mut self, m: Mark) -> &mut Option<Bounds<Pixels>> {
        match m {
            Mark::Card => &mut self.card,
            Mark::Chip => &mut self.chip,
            Mark::Focus => &mut self.focus,
            Mark::Dialog => &mut self.dialog,
            Mark::DialogFocus => &mut self.dialog_focus,
        }
    }
}

/// Background around a marked part, logical px: covers a focus ring (5 px outside the
/// element) and a pixel of what's behind it, which a lift's moved-off strip is filled from.
pub(crate) const MARK_PAD: f32 = 6.0;

/// An invisible child covering its (relative) parent that records the parent's bounds, padded
/// and clipped to the content mask, as part `m`.
pub(crate) fn mark(marks: &Rc<RefCell<DepthMarks>>, m: Mark, pad: f32) -> impl IntoElement {
    let marks = marks.clone();
    canvas(
        move |bounds, window, _| {
            let padded = bounds.dilate(px(pad));
            let b = padded.intersect(&window.content_mask().bounds);
            if b.size.width > px(0.) && b.size.height > px(0.) {
                *marks.borrow_mut().slot(m) = Some(b);
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Y held down: since when, and which press.
#[derive(Debug, Clone, Copy)]
struct YDown {
    since: Instant,
    press: u64,
}

pub struct PanelView {
    pub session: String,
    pub data: PanelData,
    pub nav: Nav,
    pub backdrop: Backdrop,
    pub footer: Footer,
    /// Values asked for and not yet confirmed, by (surface, id) (tweaks: ("tweaks", id)).
    pub(crate) drafts: BTreeMap<(String, String), Value>,
    /// Presets picked and not yet sent, by surface key: the preset's index.
    pub(crate) preset_drafts: BTreeMap<String, usize>,
    edit_gen: u64,
    /// A row's change on its way to quiverd ("Saving…"), or why it wasn't saved.
    pub(crate) saving: BTreeMap<(String, String), (String, Tone)>,
    /// The session's changes made in game (kept across opens by the controller).
    pub log: SessionLog,
    /// The change chip over the game (docked).
    pub chip: Option<Chip>,
    chip_ticking: bool,
    /// Y: held, or just let go (a press right after is key repeat).
    y_down: Option<YDown>,
    y_up: Option<(YDown, Instant)>,
    y_gen: u64,
    /// An A/B compare while Y is held.
    pub compare: Option<Compare>,
    hud_draft: BTreeMap<HudRow, String>,
    hud_note: Option<(String, Tone)>,
    /// A line in the hint bar ("Quit sent", errors).
    pub(crate) flash: Option<(String, Tone)>,
    wiring: Wiring,
    pub focus: FocusHandle,
    pub(crate) scroll: ScrollHandle,
    /// The layout last told to the controller (docked).
    told_docked: Option<bool>,
    /// The readouts last subscribed to.
    subscribed: Vec<String>,
    /// The parts at their own depth in the last frame laid out ([`DepthMarks`]).
    pub marks: Rc<RefCell<DepthMarks>>,
    /// What's pinned to the quick menu for this game and profile (Y toggles a row's pin;
    /// `crate::quick::pin_row`).
    pub pinned: Vec<quiver_api::sight::QuickPin>,
    /// The Performance section's own state (`sections::performance`).
    pub perf: PerfSection,
    /// Its layout editor's preview, a HUD view of the edited layout.
    pub(crate) perf_preview: Option<perf::view::Preview>,
    /// The section body's room in the last frame (logical px), for its layout.
    pub(crate) room: perf::view::Room,
}

pub(crate) fn tone_color(th: &Theme, t: Tone) -> Hsla {
    match t {
        Tone::Ok => th.ok,
        Tone::Warn => th.warn,
        Tone::Bad => th.bad,
        Tone::Accent => th.acc_hi,
        Tone::Muted => th.tx3,
    }
}

pub(crate) fn tone_pill(t: Tone) -> PillKind {
    match t {
        Tone::Ok => PillKind::Ok,
        Tone::Warn => PillKind::Warn,
        Tone::Bad => PillKind::Bad,
        Tone::Accent => PillKind::Accent,
        Tone::Muted => PillKind::Muted,
    }
}

/// The battery's charge and whether it's on the charger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Charging,
    /// Full or held (not charging) on the charger.
    PluggedIn,
    Battery,
}

/// "58% · charging", "100% · plugged in", "41%".
pub fn battery_text(cap: u32, p: Power) -> String {
    match p {
        Power::Charging => format!("{cap}% · charging"),
        Power::PluggedIn => format!("{cap}% · plugged in"),
        Power::Battery => format!("{cap}%"),
    }
}

/// The battery's `status` file in words.
pub fn power_of(status: &str) -> Power {
    match status.trim() {
        "Charging" => Power::Charging,
        "Full" | "Not charging" => Power::PluggedIn,
        _ => Power::Battery,
    }
}

fn battery() -> Option<(u32, Power)> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for e in dir.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type")).ok()?.trim() != "Battery" {
            continue;
        }
        let cap: u32 = std::fs::read_to_string(p.join("capacity")).ok()?.trim().parse().ok()?;
        let power = std::fs::read_to_string(p.join("status")).map(|s| power_of(&s)).unwrap_or(Power::Battery);
        return Some((cap, power));
    }
    None
}

pub(crate) fn section_icon(s: PanelSection) -> Icon {
    match s {
        PanelSection::Session => Icon::Gamepad,
        PanelSection::Mods => Icon::Package,
        PanelSection::Tweaks => Icon::SlidersHorizontal,
        PanelSection::Performance => Icon::Activity,
        PanelSection::Achievements => Icon::Trophy,
    }
}

impl PanelView {
    pub fn new(session: String, data: PanelData, nav: Nav, backdrop: Backdrop, wiring: Wiring, cx: &mut Context<Self>) -> Self {
        Self {
            session,
            data,
            nav,
            backdrop,
            footer: Footer::default(),
            drafts: BTreeMap::new(),
            preset_drafts: BTreeMap::new(),
            edit_gen: 0,
            saving: BTreeMap::new(),
            log: SessionLog::default(),
            chip: None,
            chip_ticking: false,
            y_down: None,
            y_up: None,
            y_gen: 0,
            compare: None,
            hud_draft: BTreeMap::new(),
            hud_note: None,
            flash: None,
            wiring,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            told_docked: None,
            subscribed: Vec::new(),
            marks: Rc::default(),
            pinned: Vec::new(),
            perf: PerfSection::default(),
            perf_preview: None,
            room: perf::view::Room { w: 900.0, h: 500.0 },
        }
    }

    pub(crate) fn send(&self, r: LinkReq) {
        if let Some(l) = &self.wiring.link {
            let _ = l.send(r);
        }
    }

    fn close(&self) {
        if let Some(ui) = &self.wiring.ui {
            let _ = ui.unbounded_send(Msg::Panel(quiver_api::sight::PanelCommand {
                seq: 0,
                action: quiver_api::sight::PanelAction::Close,
                session: None,
                section: None,
                monitor: None,
                by: "panel".into(),
                at: Utc::now(),
            }));
        }
    }

    /// The current section's rows, in focus order.
    pub fn rows(&self) -> Vec<Row> {
        super::sections::rows(&self.nav, &self.data, &self.log, Utc::now())
    }

    /// The section's panel is docked now.
    pub fn docked(&self) -> bool {
        self.nav.docked()
    }

    pub(crate) fn surface_of(&self, row: &Row) -> Option<&SurfaceState> {
        match row {
            Row::Opt { surface, .. } | Row::Preset { surface } => {
                self.data.options.ready().and_then(|l| l.iter().find(|s| &s.key == surface)).or_else(|| self.data.tweaks.ready().filter(|t| &t.key == surface))
            }
            Row::Tweak { .. } => self.data.tweaks.ready(),
            _ => None,
        }
    }

    pub(crate) fn option_state(&self, row: &Row) -> Option<&OptionState> {
        match row {
            Row::Opt { id, .. } | Row::Tweak { id } => self.surface_of(row)?.options.get(id),
            _ => None,
        }
    }

    pub(crate) fn row_key(row: &Row) -> Option<(String, String)> {
        match row {
            Row::Opt { surface, id } => Some((surface.clone(), id.clone())),
            Row::Tweak { id } => Some((TWEAKS.into(), id.clone())),
            _ => None,
        }
    }

    /// The value a row shows: what was asked, else what quiverd says.
    pub(crate) fn shown_value(&self, row: &Row) -> Option<Value> {
        let st = self.option_state(row)?;
        Some(Self::row_key(row).and_then(|k| self.drafts.get(&k).cloned()).unwrap_or_else(|| draft_value(st, None)))
    }

    fn is_tweaks_surface(&self, key: &str) -> bool {
        self.data.tweaks.ready().is_some_and(|t| t.key == key)
    }

    fn can_adjust(&self, row: &Row) -> bool {
        match row {
            Row::Opt { .. } | Row::Tweak { .. } => self.option_state(row).is_some_and(adjustable),
            Row::Hud(_) => self.data.hud.ready().is_some(),
            Row::Preset { .. } => self.surface_of(row).is_some_and(|s| !s.presets.is_empty()),
            _ => false,
        }
    }

    /// What Y would flip the focused row to while held (Mods' live options).
    pub(crate) fn compare_target(&self, row: &Row) -> Option<Value> {
        let Row::Opt { surface, id } = row else { return None };
        let (st, s) = (self.option_state(row)?, self.surface_of(row)?);
        mods::compare_with(st, s, self.log.get(false, surface, id), &self.shown_value(row)?)
    }

    /// One input from the keyboard, the controller (gilrs, or hyprpad's keys) or a click.
    pub fn input(&mut self, i: Input, cx: &mut Context<Self>) {
        // The Performance section's body has its own controls and editor.
        if self.perf_has_input() {
            let cap = self.cap_value();
            if let perf::Outcome::Done(acts) = self.perf.input(i, &self.data, &cap) {
                self.perf_acts(acts, cx);
                cx.notify();
                return;
            }
        }
        self.nav.pages.surfaces = self.data.options.ready().map_or(0, |l| l.len());
        let rows = self.rows();
        let e = {
            let this = &*self;
            let mut nav = this.nav.clone();
            let e = nav.apply(i, &rows, |r| this.can_adjust(r));
            self.nav = nav;
            e
        };
        self.effect(e, cx);
        self.scroll_to_focus();
        cx.notify();
    }

    /// A key of the panel's own ([`Key`]): `held` is key repeat.
    pub fn key(&mut self, k: Key, held: bool, cx: &mut Context<Self>) {
        match k {
            Key::Input(i) => self.input(i, cx),
            Key::Dock if !held => self.toggle_dock(cx),
            Key::Dock => {}
            // Y saves the layout editor's preset (no A/B compare there).
            Key::Y if self.perf_has_input() => {
                if !held {
                    self.input(Input::Menu, cx);
                }
            }
            Key::Y => self.y_pressed(held, cx),
        }
    }

    /// R3 / Tab: docks or centres the section (remembered per section).
    pub fn toggle_dock(&mut self, cx: &mut Context<Self>) {
        if self.nav.confirm.is_some() {
            return;
        }
        let d = self.nav.toggle_dock();
        tracing::info!(section = self.nav.section.label(), docked = d, "panel layout");
        cx.notify();
    }

    fn scroll_to_focus(&self) {
        if let Some(r) = self.nav.focused_row(&self.rows()) {
            self.scroll.scroll_to_item(r);
        }
    }

    fn effect(&mut self, e: Effect, cx: &mut Context<Self>) {
        match e {
            Effect::None | Effect::Moved | Effect::Reveal(_) | Effect::Surface => {}
            Effect::Close => self.close(),
            Effect::Adjust(row, dir) => self.adjust(row, dir, cx),
            Effect::Confirmed(b) => self.confirmed(b),
            Effect::Tab(d) => self.tab(d),
            Effect::X(row) => self.x(row),
            Effect::Pin(row) => self.pin(row, cx),
            Effect::Act(a) => self.act(a),
        }
    }

    /// LT / RT: Mods' and Tweaks' pages; elsewhere, the sections.
    fn tab(&mut self, d: i32) {
        let key_n = match self.nav.section {
            PanelSection::Mods => mods::surface(&self.data, &self.nav).map(|s| (s.key.clone(), mods::pages(s).len())),
            PanelSection::Tweaks => self.data.tweaks.ready().map(|s| (s.key.clone(), mods::pages(s).len())),
            _ => {
                self.nav.step_section(d);
                return;
            }
        };
        if let Some((key, n)) = key_n
            && self.nav.pages.step(&key, n, d)
        {
            self.nav.zone = Zone::Body;
            self.nav.set_focus_index(0);
        }
    }

    /// X: Mods and Tweaks put the focused option back to its default.
    fn x(&mut self, row: Option<Row>) {
        let Some(row) = row else { return };
        if !matches!(self.nav.section, PanelSection::Mods | PanelSection::Tweaks) {
            return;
        }
        let (Some(st), Some(key), Some(s)) = (self.option_state(&row), Self::row_key(&row), self.surface_of(&row)) else { return };
        let Some(view) = self.data.session.ready().cloned() else { return };
        let surface = s.key.clone();
        let tweaks = matches!(row, Row::Tweak { .. });
        let Some(op) = mods::reset_op(&surface, &key.1, st) else {
            self.flash = Some((format!("{} is at its default", st.def.label_or(&key.1)), Tone::Muted));
            return;
        };
        // Session only: the default for this session, not saved.
        let (op, live) = if !tweaks && self.log.session_only {
            let d = st.def.default.clone().filter(|v| !v.is_null()).unwrap_or_else(|| st.default.clone());
            (OptionOp::Set { surface, id: key.1.clone(), value: d }, Live::Only)
        } else {
            (op, Live::IfRunning)
        };
        self.drafts.remove(&key);
        self.saving.insert(key, ("Resetting…".into(), Tone::Muted));
        self.send(LinkReq::OptionOps { view, tweaks, ops: vec![op], live, why: Why::Reset });
    }

    /// Y tapped on a row: pin it to the quick menu, or unpin it if it's there
    /// (`crate::quick::pin_row`; per game and profile). (A held Y compares instead:
    /// [`Self::y_pressed`].)
    pub fn pin(&mut self, row: Option<Row>, cx: &mut Context<Self>) {
        let Some(row) = row else { return };
        match crate::quick::pin_row(&self.data, &row, &self.pinned) {
            Ok(p) => {
                self.flash = Some((p.said.clone(), Tone::Ok));
                // Shown at once; quiverd's answer (the settings) confirms it.
                if p.on {
                    self.pinned.push(p.pin.clone());
                } else {
                    self.pinned.retain(|x| !x.same_item(&p.pin));
                }
                self.send(LinkReq::Pin { scope: p.scope, pin: p.pin, on: p.on });
            }
            Err(why) => self.flash = Some((why, Tone::Muted)),
        }
        cx.notify();
    }

    /// The Mods / Tweaks buttons.
    fn act(&mut self, a: Act) {
        let Some(view) = self.data.session.ready().cloned() else { return };
        match a {
            Act::UndoRestart { tweaks } => {
                let ops = self.log.undo_ops(tweaks);
                if !ops.is_empty() {
                    self.flash = Some(("Undoing…".into(), Tone::Muted));
                    self.send(LinkReq::OptionOps { view, tweaks, ops, live: Live::IfRunning, why: Why::Undo });
                }
            }
            Act::RevertSession => {
                self.flash = Some(("Putting back what the session started with…".into(), Tone::Muted));
                self.send(LinkReq::RevertSession { view });
            }
            Act::SessionOnly => {
                if self.log.session_only {
                    self.log.session_only = false;
                    self.flash = Some(("Changes are saved to the profile again".into(), Tone::Ok));
                    return;
                }
                let (back, live) = self.log.session_only_ops();
                if back.is_empty() && live.is_empty() {
                    self.log.session_only = true;
                    self.flash = Some(("Session only: changes from now on aren't saved".into(), Tone::Ok));
                } else {
                    self.flash = Some(("Keeping the changes for this session only…".into(), Tone::Muted));
                    self.send(LinkReq::SessionOnly { view, back, live });
                }
            }
        }
    }

    // ---- Y: tap pins, hold compares ---------------------------------------------------------

    fn y_pressed(&mut self, held: bool, cx: &mut Context<Self>) {
        // A release and a press this close together is key repeat (hyprpad holds the key).
        if let Some((y, _)) = self.y_up.take() {
            self.y_down = Some(y);
            return;
        }
        if held || self.y_down.is_some() || self.nav.confirm.is_some() {
            return;
        }
        self.y_gen += 1;
        let press = self.y_gen;
        self.y_down = Some(YDown { since: Instant::now(), press });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(mods::HOLD).await;
            let _ = this.update(cx, |this, cx| {
                if this.y_down.is_some_and(|y| y.press == press) && this.compare.is_none() {
                    this.begin_compare(cx);
                }
            });
        })
        .detach();
    }

    fn y_released(&mut self, cx: &mut Context<Self>) {
        let Some(y) = self.y_down.take() else { return };
        self.y_up = Some((y, Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Y_REPEAT_GAP).await;
            let _ = this.update(cx, |this, cx| {
                if let Some((u, at)) = this.y_up.filter(|(u, _)| u.press == y.press) {
                    this.y_up = None;
                    this.y_finished(at.saturating_duration_since(u.since), cx);
                }
            });
        })
        .detach();
    }

    fn y_finished(&mut self, held: Duration, cx: &mut Context<Self>) {
        if self.compare.is_some() {
            self.end_compare(cx);
            return;
        }
        let rows = self.rows();
        let focused = self.nav.focused_row(&rows).and_then(|i| rows.get(i)).cloned();
        let comparable = focused.as_ref().is_some_and(|r| self.compare_target(r).is_some());
        if mods::y_press(held, comparable) == mods::YPress::Pin {
            self.pin(focused, cx);
        }
        cx.notify();
    }

    /// Y held on a live option: the game shows the value to compare with (this session only)
    /// until Y is let go.
    fn begin_compare(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows();
        let Some(row) = self.nav.focused_row(&rows).and_then(|i| rows.get(i)).cloned() else { return };
        let Row::Opt { surface, id } = &row else { return };
        let (Some(target), Some(current), Some(st), Some(view)) = (self.compare_target(&row), self.shown_value(&row), self.option_state(&row), self.data.session.ready().cloned()) else { return };
        let label = st.def.label_or(id).to_owned();
        let (from, to) = (short_value(&st.def, &current), short_value(&st.def, &target));
        self.send(LinkReq::OptionOps { view, tweaks: false, ops: vec![OptionOp::Set { surface: surface.clone(), id: id.clone(), value: target.clone() }], live: Live::Only, why: Why::Compare });
        tracing::info!(%surface, %id, %from, %to, "A/B compare");
        self.compare = Some(Compare { surface: surface.clone(), id: id.clone(), showing: target, back_to: current });
        self.show_chip(Chip { label, before: from, after: to, when: ChipWhen::Live { session_only: true }, at: Instant::now(), comparing: true }, cx);
        cx.notify();
    }

    fn end_compare(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.compare.take() else { return };
        let Some(view) = self.data.session.ready().cloned() else { return };
        let row = Row::Opt { surface: c.surface.clone(), id: c.id.clone() };
        let (label, from, to) = match self.option_state(&row) {
            Some(st) => (st.def.label_or(&c.id).to_owned(), short_value(&st.def, &c.showing), short_value(&st.def, &c.back_to)),
            None => (c.id.clone(), String::new(), String::new()),
        };
        self.send(LinkReq::OptionOps { view, tweaks: false, ops: vec![OptionOp::Set { surface: c.surface, id: c.id, value: c.back_to }], live: Live::Only, why: Why::Compare });
        self.show_chip(Chip { label, before: from, after: to, when: ChipWhen::Live { session_only: false }, at: Instant::now(), comparing: false }, cx);
        cx.notify();
    }

    /// Shows the chip, redrawing while it ages and fades.
    pub fn show_chip(&mut self, chip: Chip, cx: &mut Context<Self>) {
        self.chip = Some(chip);
        if self.chip_ticking {
            return;
        }
        self.chip_ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let go_on = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        let alive = this.chip.as_ref().is_some_and(|c| c.alive(Instant::now()));
                        if !alive {
                            this.chip = None;
                            this.chip_ticking = false;
                        }
                        alive
                    })
                    .unwrap_or(false);
                if !go_on {
                    break;
                }
            }
        })
        .detach();
    }

    fn confirmed(&mut self, b: Button) {
        match b {
            Button::Quit => match self.wiring.quit.as_ref().map(|q| q()) {
                Some(Ok(())) => {
                    self.flash = Some(("Asked the game to quit".into(), Tone::Ok));
                    self.close();
                }
                Some(Err(e)) => self.flash = Some((format!("Couldn't ask the game to quit: {e}. Force quit stops it."), Tone::Bad)),
                None => self.flash = Some(("No game window to close: Force quit stops it.".into(), Tone::Warn)),
            },
            Button::ForceQuit => match self.data.session.ready() {
                Some(s) => {
                    self.send(LinkReq::Stop { game: s.game });
                    self.flash = Some(("Stopping the game…".into(), Tone::Warn));
                }
                None => self.flash = Some(("The session isn't loaded yet.".into(), Tone::Warn)),
            },
        }
    }

    pub(crate) fn adjust(&mut self, row: Row, dir: i32, cx: &mut Context<Self>) {
        match &row {
            Row::Opt { .. } | Row::Tweak { .. } => {
                let Some(key) = Self::row_key(&row) else { return };
                let Some(st) = self.option_state(&row) else { return };
                let draft = self.drafts.get(&key).cloned().unwrap_or_else(|| draft_value(st, None));
                let Some(v) = step_value(st, &draft, dir) else { return };
                self.drafts.insert(key.clone(), v);
                self.saving.remove(&key);
                self.debounce(cx);
            }
            Row::Preset { surface } => {
                let Some(s) = self.surface_of(&row) else { return };
                let n = s.presets.len();
                if n == 0 {
                    return;
                }
                let cur = self.preset_drafts.get(surface).copied().or_else(|| mods::preset_now(s));
                let next = match cur {
                    Some(i) => (i as i32 + if dir == 0 { 1 } else { dir }).rem_euclid(n as i32) as usize,
                    None if dir < 0 => n - 1,
                    None => 0,
                };
                self.preset_drafts.insert(surface.clone(), next);
                self.debounce(cx);
            }
            Row::Hud(h) => self.adjust_hud(*h, dir),
            _ => {}
        }
    }

    // ---- The Performance section's glue (`sections::performance`) ---------------------------

    /// The Performance section's body has the focus (its controls take the input first).
    fn perf_has_input(&self) -> bool {
        self.nav.section == PanelSection::Performance && self.nav.zone == Zone::Body && self.nav.confirm.is_none()
    }

    /// What the section's input asked for.
    fn perf_acts(&mut self, acts: Vec<perf::Act>, cx: &mut Context<Self>) {
        for a in acts {
            match a {
                perf::Act::Send(req) => self.send(LinkReq::Perf { session: self.session.clone(), req }),
                perf::Act::SetTweak(id, v) => {
                    let key = (TWEAKS.to_owned(), id);
                    self.drafts.insert(key.clone(), v);
                    self.saving.remove(&key);
                    self.debounce(cx);
                }
                perf::Act::Note(t, tone) => self.hud_note = Some((t, tone)),
            }
        }
    }

    /// quiverd answered one of the section's requests.
    pub fn perf_reply(&mut self, r: perf::Reply, _cx: &mut Context<Self>) {
        let opening = self.perf.editor.is_none();
        let (note, reload) = self.perf.reply(r, &self.data);
        if let Some(n) = note {
            self.flash = Some(n);
        }
        if reload {
            self.send(LinkReq::Load { session: self.session.clone(), part: super::data::Part::Hud, view: self.data.session.ready().cloned() });
        }
        // The editor just opened (or took another preset): its preview and metrics now.
        if let Some(e) = self.perf.editor.as_ref().filter(|e| opening || e.preview.is_none()) {
            let reqs = [perf::Req::Preview(e.layout.clone()), perf::Req::Catalog];
            for req in reqs {
                self.send(LinkReq::Perf { session: self.session.clone(), req });
            }
        }
    }

    /// Once a second while the panel shows: the section's graph (and the editor's preview).
    pub fn tick(&mut self, _cx: &mut Context<Self>) {
        if self.nav.section == PanelSection::Performance {
            for req in self.perf.refresh(&self.data) {
                self.send(LinkReq::Perf { session: self.session.clone(), req });
            }
        }
    }

    /// A click on one of the section's controls focuses it.
    pub(crate) fn perf_focus(&mut self, c: perf::Ctl, cx: &mut Context<Self>) {
        if let Some(i) = self.perf.ctls(&self.data).iter().position(|x| *x == c) {
            self.nav.zone = Zone::Body;
            self.perf.focus = i;
            cx.notify();
        }
    }

    /// The frame-rate cap as asked (a draft) or as set.
    pub(crate) fn cap_value(&self) -> Value {
        let key = (TWEAKS.to_owned(), perf::CAP_TWEAK.to_owned());
        self.drafts.get(&key).cloned().or_else(|| self.data.tweaks.ready().and_then(|t| t.options.get(perf::CAP_TWEAK)).map(|st| draft_value(st, None))).unwrap_or(Value::from(0))
    }

    /// What came of the last change of a tweak ("Saving…", or why it wasn't saved).
    pub(crate) fn tweak_outcome(&self, id: &str) -> Option<(String, Tone)> {
        self.saving.get(&(TWEAKS.to_owned(), id.to_owned())).cloned()
    }

    fn debounce(&mut self, cx: &mut Context<Self>) {
        self.edit_gen += 1;
        let edit_gen = self.edit_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.edit_gen == edit_gen {
                    this.flush(cx);
                }
            });
        })
        .detach();
    }

    /// Sends every staged value (one request per row: outcomes are per row) and preset.
    fn flush(&mut self, _cx: &mut Context<Self>) {
        let Some(view) = self.data.session.ready().cloned() else { return };
        let live = if self.log.session_only { Live::Only } else { Live::IfRunning };
        for ((surface, id), v) in &self.drafts {
            let row = if surface == TWEAKS { Row::Tweak { id: id.clone() } } else { Row::Opt { surface: surface.clone(), id: id.clone() } };
            let unchanged = self.option_state(&row).is_some_and(|st| quiver_api::options::same_value(&st.value, v));
            let key = (surface.clone(), id.clone());
            if unchanged || self.saving.contains_key(&key) {
                continue;
            }
            if surface == TWEAKS {
                let op = OptionOp::Set { surface: self.data.tweaks.ready().map(|s| s.key.clone()).unwrap_or_default(), id: id.clone(), value: v.clone() };
                self.send(LinkReq::SetTweak { view: view.clone(), op, id: id.clone() });
            } else {
                let op = OptionOp::Set { surface: surface.clone(), id: id.clone(), value: v.clone() };
                self.send(LinkReq::SetOption { view: view.clone(), op, key: key.clone(), live });
            }
            self.saving.insert(key, ("Saving…".into(), Tone::Muted));
        }
        for (surface, i) in std::mem::take(&mut self.preset_drafts) {
            let tweaks = self.is_tweaks_surface(&surface);
            let s = if tweaks { self.data.tweaks.ready() } else { self.data.options.ready().and_then(|l| l.iter().find(|x| x.key == surface)) };
            let Some(p) = s.and_then(|s| s.presets.get(i)) else { continue };
            if s.and_then(mods::preset_now) == Some(i) {
                continue;
            }
            self.flash = Some((format!("Preset {}…", p.label), Tone::Muted));
            let ops = vec![OptionOp::Preset { surface: surface.clone(), id: p.id.clone() }];
            self.send(LinkReq::OptionOps { view: view.clone(), tweaks, ops, live: if tweaks { Live::IfRunning } else { live }, why: Why::Preset });
        }
    }

    pub(crate) fn hud_value(&self, h: HudRow) -> String {
        if let Some(v) = self.hud_draft.get(&h) {
            return v.clone();
        }
        let Some(s) = self.data.hud.ready() else { return "—".into() };
        let c = &s.settings.effective;
        match h {
            HudRow::Enable => if c.enabled { "on" } else { "off" }.into(),
            HudRow::Shown => if s.status.as_ref().is_some_and(|x| x.on) { "on" } else { "off" }.into(),
            HudRow::Preset => s.status.as_ref().filter(|_| s.live()).map(|x| x.layout.clone()).unwrap_or_else(|| c.layout.clone()),
            HudRow::Position => serde_json::to_value(c.anchor).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(),
            HudRow::Scale => format!("{:.2}", c.scale),
        }
    }

    fn adjust_hud(&mut self, h: HudRow, dir: i32) {
        let Some(view) = self.data.session.ready().cloned() else { return };
        let Some(s) = self.data.hud.ready() else { return };
        let c = s.settings.effective.clone();
        let cur = self.hud_value(h);
        match h {
            HudRow::Shown => {
                self.send(LinkReq::HudToggle { session: self.session.clone() });
                self.hud_draft.insert(h, if cur == "on" { "off" } else { "on" }.into());
            }
            HudRow::Enable => {
                let on = cur != "on";
                self.send(LinkReq::HudSet { view, set: HudScope { enabled: Some(on), ..Default::default() } });
                self.hud_draft.insert(h, if on { "on" } else { "off" }.into());
            }
            HudRow::Preset => {
                let ids = PRESETS.map(|p| p.0.to_owned());
                let next = cycle(&ids, &cur, dir);
                self.send(LinkReq::HudSet { view, set: HudScope { layout: Some(next.clone()), on: s.live().then_some(true), ..Default::default() } });
                self.hud_draft.insert(h, next);
            }
            HudRow::Position => {
                let all = ANCHORS.map(|a| a.0);
                let next = cycle(&all, &c.anchor, dir);
                self.send(LinkReq::HudSet { view, set: HudScope { anchor: Some(next), ..Default::default() } });
                self.hud_draft.insert(h, serde_json::to_value(next).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default());
            }
            HudRow::Scale => {
                let cur: f32 = cur.parse().unwrap_or(c.scale);
                let next = step_scale(cur, dir);
                if (next - cur).abs() < 1e-3 {
                    return;
                }
                self.send(LinkReq::HudSet { view, set: HudScope { scale: Some(next), ..Default::default() } });
                self.hud_draft.insert(h, format!("{next:.2}"));
            }
        }
        self.hud_note = Some(("Saving…".into(), Tone::Muted));
    }

    /// quiverd's answer to a load.
    pub fn loaded(&mut self, data: Loaded) {
        match data {
            Loaded::All(d) => {
                let metrics = std::mem::take(&mut self.data.metrics);
                self.data = *d;
                self.data.metrics = metrics;
                self.drafts.clear();
                self.hud_draft.clear();
            }
            Loaded::Options(o, readouts) => {
                self.data.options = o;
                self.data.readouts = readouts;
                self.drafts.retain(|k, _| k.0 == TWEAKS);
            }
            Loaded::Tweaks(t) => {
                self.data.tweaks = t;
                self.drafts.retain(|k, _| k.0 != TWEAKS);
            }
            Loaded::Hud(h) => {
                self.data.hud = h;
                self.hud_draft.clear();
            }
        }
        // The mods' readouts come with the panel's metrics.
        let extra: Vec<String> = self.data.readouts.values().flatten().map(|(id, _)| id.clone()).collect();
        if extra != self.subscribed {
            self.subscribed = extra.clone();
            self.send(LinkReq::Subscribe { session: self.session.clone(), extra });
        }
    }

    /// The label and definition of an option quiverd answered about.
    fn def_of(&self, tweaks: bool, surface: &str, id: &str) -> Option<(String, quiver_api::options::OptionDef)> {
        let s = if tweaks { self.data.tweaks.ready() } else { self.data.options.ready().and_then(|l| l.iter().find(|s| s.key == surface)) }?;
        let st = s.options.get(id)?;
        Some((st.def.label_or(id).to_owned(), st.def.clone()))
    }

    /// Notes what came of each change in the session's log, and shows the last one's chip.
    fn note(&mut self, tweaks: bool, c: &OptionsChange, session_only: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut chip = None;
        for o in &c.outcomes {
            let Some((label, def)) = self.def_of(tweaks, &o.surface, &o.id) else { continue };
            if let Some(ch) = self.log.record(tweaks, &label, &def, o, session_only, now) {
                chip = Some(ch);
            }
        }
        if let Some(ch) = chip {
            self.show_chip(ch, cx);
        }
    }

    /// quiverd's answer to a change: the row's outcome, the session's log and chip, and a
    /// reload of what it changed.
    pub fn done(&mut self, change: Change, result: Result<Option<OptionsChange>, String>, cx: &mut Context<Self>) {
        let part = match &change {
            Change::Option(..) => super::data::Part::Options,
            Change::Tweak(_) => super::data::Part::Tweaks,
            Change::Ops { tweaks: true, .. } => super::data::Part::Tweaks,
            Change::Ops { .. } => super::data::Part::Options,
            Change::Hud => super::data::Part::Hud,
            Change::Stop => {
                self.flash = Some(match result {
                    Ok(_) => ("Stopped the game".into(), Tone::Ok),
                    Err(e) => (format!("Couldn't stop the game: {e}"), Tone::Bad),
                });
                return;
            }
        };
        match (&change, &result) {
            (Change::Option(s, id), Err(e)) => {
                self.saving.insert((s.clone(), id.clone()), (format!("Not saved: {e}"), Tone::Bad));
            }
            (Change::Tweak(id), Err(e)) => {
                self.saving.insert((TWEAKS.into(), id.clone()), (format!("Not saved: {e}"), Tone::Bad));
            }
            (Change::Option(s, id), Ok(c)) => {
                self.saving.remove(&(s.clone(), id.clone()));
                if let Some(c) = c {
                    self.note(false, c, self.log.session_only, cx);
                }
            }
            (Change::Tweak(id), Ok(c)) => {
                self.saving.remove(&(TWEAKS.to_owned(), id.clone()));
                if let Some(c) = c {
                    self.note(true, c, false, cx);
                }
            }
            (Change::Ops { why, .. }, Err(e)) => {
                self.saving.retain(|_, v| v.0 != "Resetting…");
                self.flash = Some((format!("Not done ({}): {e}", why_word(*why)), Tone::Bad));
            }
            (Change::Ops { tweaks, why }, Ok(c)) => {
                self.saving.retain(|_, v| v.0 != "Resetting…");
                match why {
                    Why::Compare => {}
                    Why::Revert => {
                        self.log.reverted();
                        self.flash = Some(("Back to what the session started with".into(), Tone::Ok));
                    }
                    Why::SessionOnly => {
                        self.log.made_session_only();
                        self.flash = Some(("This session's changes are for this session only; the profile is as it was".into(), Tone::Ok));
                    }
                    Why::Undo | Why::Reset | Why::Preset => {
                        if let Some(c) = c {
                            let so = !*tweaks && self.log.session_only;
                            self.note(*tweaks, c, so, cx);
                        }
                        if *why != Why::Reset {
                            self.flash = c.as_ref().map(|c| (quiver_ui::options::outcome_summary(c.revision, &c.outcomes), Tone::Muted));
                        }
                    }
                }
            }
            (Change::Hud, r) => {
                let live = self.data.hud.ready().is_some_and(|h| h.live());
                self.hud_note = Some(match r {
                    Err(e) => (format!("Not saved: {e}"), Tone::Bad),
                    Ok(_) if live => ("Applied live".into(), Tone::Ok),
                    Ok(_) => ("Saved · applies next launch".into(), Tone::Muted),
                });
            }
            _ => {}
        }
        // An A/B flip doesn't change what's saved: no reload (it would drop the drafts).
        if change != (Change::Ops { tweaks: false, why: Why::Compare }) {
            self.send(LinkReq::Load { session: self.session.clone(), part, view: self.data.session.ready().cloned() });
        }
    }

    pub fn key_down(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.key == "f2" && k.modifiers.shift {
            self.close();
            return;
        }
        if let Some(key) = panel_key(&k.key, k.modifiers.shift, k.modifiers.control) {
            self.key(key, ev.is_held, cx);
        }
    }

    pub fn key_up(&mut self, ev: &KeyUpEvent, cx: &mut Context<Self>) {
        if ev.keystroke.key == "y" {
            self.y_released(cx);
        }
    }

    // ---- Hints --------------------------------------------------------------------------

    /// What the section's buttons do now ([`Hints`]): one arm per section. A section's module
    /// adds its own; the shell then adds B, LB / RB and R3 ([`Hints::finish`]).
    pub fn section_hints(&self, h: &mut Hints) {
        match self.nav.section {
            PanelSection::Mods | PanelSection::Tweaks => super::sections::mods_view::hints(self, h),
            PanelSection::Performance if self.nav.zone == Zone::Body => {
                for (g, t) in self.perf.hints(&self.data) {
                    h.add(g, t);
                }
            }
            _ => self.default_hints(h),
        }
    }

    /// The hints of a section without its own: adjust or select the focused row.
    fn default_hints(&self, h: &mut Hints) {
        let rows = self.rows();
        match self.nav.focused_row(&rows).and_then(|i| rows.get(i)) {
            Some(r) if self.can_adjust(r) => {
                h.add([Glyph::LeftRight], "Change").add([Glyph::A], "Next value");
            }
            Some(_) => {
                h.add([Glyph::A], "Select");
            }
            None => {}
        }
    }

    /// The whole bar's hints this frame: a dialog's, else the section's and the shell's.
    pub fn hints(&self) -> Hints {
        let mut h = Hints::default();
        if self.nav.confirm.is_some() {
            h.add([Glyph::A], "Select").add([Glyph::B], "Cancel").without_shell();
            return h;
        }
        self.section_hints(&mut h);
        let back = if self.nav.section == PanelSection::Mods && self.nav.zone == Zone::Body && self.nav.pages.surfaces > 0 { "Back" } else { "Close" };
        h.finish(back, Some(if self.docked() { "Centre" } else { "Dock" }));
        h
    }

    // ---- Rendering --------------------------------------------------------------------------

    /// The right side of the tab row: "AC Origins · Profile DLSS · 21:16", the battery.
    fn status_line(&self, th: &Theme) -> Div {
        let clock = chrono::Local::now().format("%H:%M").to_string();
        let mut parts = Vec::new();
        if let Some(s) = self.data.session.ready() {
            parts.push(s.name.clone());
            parts.push(format!("Profile {}", s.profile_name.clone().unwrap_or_else(|| "Vanilla".into())));
        }
        parts.push(clock);
        let bat = battery().map(|(c, p)| {
            let col = if c <= 15 && p == Power::Battery { th.bad } else { th.tx2 };
            div().flex().items_center().gap(px(6.)).text_color(col).when(p != Power::Battery, |d| d.child(icon(Icon::PlugZap, 14., col))).child(battery_text(c, p))
        });
        div()
            .flex()
            .min_w_0()
            .flex_shrink_1()
            .items_center()
            .gap(px(14.))
            .text_size(px(14.))
            .text_color(th.tx2)
            .child(div().min_w_0().max_w(px(520.)).truncate().child(parts.join(" · ")))
            .when_some(bat, |d, b| d.child(b))
    }

    /// The centred layout's tab row: "Sight", LB, the sections, RB; the status on the right.
    fn tab_row(&self, th: &Theme, cx: &mut Context<Self>) -> Div {
        let pad = |id: &'static str, g: &'static str, d: i32| {
            div().id(id).cursor_pointer().child(glyph(th, g, false)).on_click(cx.listener(move |this, _, _, cx| {
                this.nav.step_section(d);
                cx.notify();
            }))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .mr(px(10.))
                    .child(icon(Icon::CircleDot, 24., th.acc_hi))
                    .child(div().font_family(DISPLAY_FONT).text_size(px(18.)).font_weight(gpui::FontWeight::BOLD).text_color(th.tx1).child("Sight")),
            )
            .child(pad("tab-lb", "LB", -1))
            .child(div().flex().items_center().gap(px(4.)).children(PanelSection::ALL.iter().map(|s| {
                let on = *s == self.nav.section;
                let s = *s;
                div()
                    .id(SharedString::from(format!("tab-{}", s.label())))
                    .h(px(36.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .rounded(px(9.))
                    .text_size(px(14.))
                    .font_weight(SEMIBOLD)
                    .text_color(if on { th.tx1 } else { th.tx2 })
                    .when(on, |d| d.bg(th.acc.opacity(0.16)).border_b_2().border_color(th.acc))
                    .cursor_pointer()
                    .hover(|d| d.bg(th.ink3))
                    .child(s.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.nav.select_section(s);
                        cx.notify();
                    }))
            })))
            .child(pad("tab-rb", "RB", 1))
            .child(div().flex_1())
            .child(self.status_line(th))
    }

    /// The docked column's header: the section, its icon tabs between LB and RB, "Docked R3".
    fn dock_header(&self, th: &Theme, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .pt(px(14.))
            .pb(px(10.))
            .child(icon(Icon::CircleDot, 20., th.acc_hi))
            .child(div().text_size(px(16.)).font_weight(gpui::FontWeight::BOLD).text_color(th.tx1).mr(px(6.)).child(self.nav.section.label()))
            .child(glyph(th, "LB", false))
            .child(div().flex().gap(px(2.)).children(PanelSection::ALL.iter().map(|s| {
                let on = *s == self.nav.section;
                let s = *s;
                div()
                    .id(SharedString::from(format!("dtab-{}", s.label())))
                    .size(px(34.))
                    .rounded(px(9.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(on, |d| d.bg(th.acc.opacity(0.18)).border_b_2().border_color(th.acc))
                    .cursor_pointer()
                    .hover(|d| d.bg(th.ink3))
                    .child(icon(section_icon(s), 18., if on { th.tx1 } else { th.tx2 }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.nav.select_section(s);
                        cx.notify();
                    }))
            })))
            .child(glyph(th, "RB", false))
            .child(div().flex_1())
            .child(
                div()
                    .id("dock-toggle")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(24.))
                    .px(px(9.))
                    .rounded(px(12.))
                    .bg(th.ink4)
                    .text_size(px(11.5))
                    .font_weight(SEMIBOLD)
                    .text_color(th.tx2)
                    .cursor_pointer()
                    .child("Docked")
                    .child(glyph(th, "R3", false))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_dock(cx))),
            )
    }

    /// The hint bar: the hints, then the flash or the focused thing's detail, then the chord.
    fn hint_bar(&self, th: &Theme, docked: bool) -> Div {
        let h = self.hints();
        let items = h.items.iter().map(|x| div().flex().items_center().gap(px(7.)).children(x.glyphs.iter().map(|g| glyph(th, g.label(), false))).child(x.label.clone()));
        let note = self.flash.clone().or(self.hud_note.clone().filter(|_| self.nav.section == PanelSection::Performance));
        let bar = div().flex().flex_wrap().items_center().text_color(th.tx2).children(items);
        if docked {
            return bar
                .flex_none()
                .gap_x(px(16.))
                .gap_y(px(10.))
                .px(px(16.))
                .py(px(12.))
                .border_t_1()
                .border_color(th.line1)
                .text_size(px(12.))
                .when_some(note.or(h.detail.map(|d| (d, Tone::Muted))), |d, (t, tone)| d.child(div().w_full().text_color(tone_color(th, tone)).child(t)));
        }
        bar.flex_none()
            .h(px(BAR_H))
            .gap(px(22.))
            .px(px(40.))
            .bg(th.ink0.opacity(0.86))
            .border_t_1()
            .border_color(th.line1)
            .text_size(px(13.))
            .child(div().flex_1())
            .when_some(note, |d, (t, tone)| d.child(div().text_color(tone_color(th, tone)).child(t)))
            .when_some(h.detail, |d, t| d.child(div().text_color(th.tx3).child(t)))
            .when(self.footer.pad_shared, |d| d.child(div().text_color(th.warn).child("The game sees the controller too (no hyprpad)")))
            .child(div().text_color(th.tx3).child("Shift+F2 · Guide+Start"))
    }

    pub(crate) fn stepper(&self, th: &Theme, i: usize, text: String, on: Option<bool>, can: bool, cx: &mut Context<Self>) -> Div {
        let arrow = |id: String, g: &'static str, dir: i32| {
            div()
                .id(SharedString::from(id))
                .flex_none()
                .size(px(28.))
                .rounded(px(7.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(th.tx2)
                .cursor_pointer()
                .hover(|d| d.bg(th.ink4))
                .child(g)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let rows = this.rows();
                    this.nav.focus_row(&rows, i);
                    if let Some(r) = rows.get(i).cloned() {
                        this.adjust(r, dir, cx);
                    }
                    cx.notify();
                }))
        };
        let value = match on {
            Some(b) => div().flex().items_center().gap(px(8.)).child(switch(th, b)).child(div().w(px(36.)).child(if b { "On" } else { "Off" })),
            None => div().min_w(px(150.)).flex().justify_center().font_family(MONO_FONT).child(text),
        };
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(14.))
            .text_color(if can { th.tx1 } else { th.tx3 })
            .when(can && on.is_none(), |d| d.child(arrow(format!("l-{i}"), "◀", -1)))
            .child(value)
            .when(can && on.is_none(), |d| d.child(arrow(format!("r-{i}"), "▶", 1)))
    }

    /// A focusable row's shell: label, help, the control, a badge, and a line under it.
    #[allow(clippy::too_many_arguments)]
    fn control_row(&self, th: &Theme, i: usize, focused: bool, label: String, help: Option<String>, control: Div, badge: Option<(String, Tone)>, line: Option<(String, Tone)>, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(SharedString::from(format!("row-{i}")))
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(16.))
            .py(px(12.))
            .rounded(px(12.))
            .bg(if focused { th.ink3 } else { th.ink2 })
            .border_1()
            .border_color(if focused { th.line2 } else { th.line1 })
            .cursor_pointer()
            .focus_ring(focused, th, 12.)
            .when(focused, |d| d.child(mark(&self.marks, Mark::Focus, MARK_PAD)))
            .on_click(cx.listener(move |this, _, _, cx| {
                let rows = this.rows();
                this.nav.focus_row(&rows, i);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(15.)).font_weight(MEDIUM).text_color(th.tx1).truncate().child(label))
                            .when_some(help, |d, h| d.child(div().text_size(px(12.)).text_color(th.tx3).line_clamp(1).child(h))),
                    )
                    .child(control)
                    .child(div().w(px(150.)).flex().justify_end().when_some(badge, |d, (t, tone)| d.child(pill(th, tone_pill(tone), None, t)))),
            )
            .when_some(line, |d, (t, tone)| d.child(div().text_size(px(12.)).text_color(tone_color(th, tone)).child(t)))
            .into_any_element()
    }

    /// An option row of a section without its own layout (the Mods and Tweaks sections draw
    /// theirs in `sections::mods_view`).
    fn option_row(&self, th: &Theme, i: usize, row: &Row, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let (Some(st), Some(key), Some(v)) = (self.option_state(row), Self::row_key(row), self.shown_value(row)) else { return div().into_any_element() };
        let can = adjustable(st);
        let on = (st.def.ty == quiver_api::options::OptionType::Bool).then(|| v.as_bool().unwrap_or(false));
        let control = self.stepper(th, i, short_value(&st.def, &v), on, can, cx);
        let line = self.saving.get(&key).cloned().or_else(|| pending_text(&st.pending).map(|t| (t, Tone::Warn)));
        self.control_row(th, i, focused, st.def.label_or(&key.1).to_owned(), st.def.help.clone(), control, Some(apply_badge(&st.def)), line, cx)
    }

    fn hud_row(&self, th: &Theme, i: usize, h: HudRow, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let v = self.hud_value(h);
        let live = self.data.hud.ready().is_some_and(|x| x.live());
        let (label, help, text, on) = match h {
            HudRow::Enable => ("Sight layer".to_owned(), Some("In the game's launches (on unless turned off or anti-cheat)".to_owned()), v.clone(), Some(v == "on")),
            HudRow::Shown => ("Show the HUD".into(), Some("Shift+F3 in game".into()), v.clone(), Some(v == "on")),
            HudRow::Preset => ("Preset".into(), Some("Ctrl+Shift+F3 in game".into()), PRESETS.iter().find(|p| p.0 == v).map(|p| p.1.to_owned()).unwrap_or(v.clone()), None),
            HudRow::Position => {
                let a = ANCHORS.iter().find(|(a, _)| serde_json::to_value(a).ok().and_then(|x| x.as_str().map(str::to_owned)).as_deref() == Some(v.as_str()));
                ("Position".into(), None, a.map(|a| a.1.to_owned()).unwrap_or(v.clone()), None)
            }
            HudRow::Scale => ("Size".into(), None, format!("{}×", v.trim_end_matches('0').trim_end_matches('.')), None),
        };
        let badge = if h == HudRow::Enable || !live { ("NEXT LAUNCH".to_owned(), Tone::Muted) } else { ("LIVE".to_owned(), Tone::Ok) };
        let control = self.stepper(th, i, text, on, true, cx);
        self.control_row(th, i, focused, label, help, control, Some(badge), None, cx)
    }

    fn metric(&self, id: &str, stat: Stat) -> Option<&PerfValue> {
        let m = self.data.metrics.get(id)?;
        m.get(&stat).or_else(|| m.get(&Stat::Last)).or_else(|| m.values().next()).filter(|v| !v.stale)
    }

    fn metrics(&self, th: &Theme) -> AnyElement {
        let tile = |label: &str, v: Option<&PerfValue>, big: bool| {
            let (text, unit, col) = match v {
                Some(v) => (v.text.clone(), v.unit.clone(), crate::hud_view::level_color(th, v.level, true)),
                None => ("—".into(), String::new(), th.tx4),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .px(px(16.))
                .py(px(12.))
                .min_w(px(if big { 150. } else { 120. }))
                .rounded(px(12.))
                .bg(th.ink2)
                .border_1()
                .border_color(th.line1)
                .child(caption(label, th.tx3))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(4.))
                        .child(div().font_family(MONO_FONT).text_size(px(if big { 30. } else { 20. })).font_weight(SEMIBOLD).text_color(col).child(text))
                        .child(div().font_family(MONO_FONT).text_size(px(13.)).text_color(th.tx3).pb(px(3.)).child(unit)),
                )
        };
        let any = !self.data.metrics.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(10.))
                    .child(tile("FPS", self.metric("fps", Stat::Mean), true))
                    .child(tile("1% LOW", self.metric("fps", Stat::P1low), true))
                    .when(self.data.metrics.contains_key("fps.base"), |d| d.child(tile("BASE FPS", self.metric("fps.base", Stat::Mean), true)))
                    .child(tile("FRAME TIME", self.metric("frametime", Stat::Mean), false))
                    .child(tile("GPU", self.metric("gpu.load", Stat::Last), false))
                    .child(tile("GPU TEMP", self.metric("gpu.temp", Stat::Last), false))
                    .child(tile("GPU POWER", self.metric("gpu.power", Stat::Last), false))
                    .child(tile("VRAM", self.metric("gpu.vram.used", Stat::Last), false))
                    .child(tile("CPU", self.metric("cpu.load", Stat::Last), false))
                    .child(tile("RAM", self.metric("mem.used", Stat::Last), false)),
            )
            .when(!any, |d| d.child(div().text_size(px(12.)).text_color(th.tx3).child("Waiting for numbers… Frame rates need the Sight layer in the launch (Quiver's HUD set up).")))
            .into_any_element()
    }

    /// One row of a section without its own layout (Session, Performance, Achievements).
    pub(crate) fn render_row(&self, th: &Theme, i: usize, row: &Row, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Heading { title, detail } => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .pt(px(14.))
                .pb(px(2.))
                .child(div().text_size(px(13.)).font_weight(SEMIBOLD).text_color(th.tx2).child(title.to_uppercase()))
                .when_some(detail.clone(), |d, t| {
                    let tone = if t == "live" { PillKind::Ok } else { PillKind::Muted };
                    d.child(pill(th, tone, None, t.to_uppercase()))
                })
                .into_any_element(),
            Row::Fact { label, value } => div()
                .flex()
                .gap(px(16.))
                .py(px(4.))
                .text_size(px(15.))
                .child(div().w(px(120.)).flex_none().text_color(th.tx3).child(label.clone()))
                .child(div().flex_1().min_w_0().text_color(th.tx1).child(value.clone()))
                .into_any_element(),
            Row::Note(t) => div().py(px(4.)).text_size(px(13.)).text_color(th.tx3).child(t.clone()).into_any_element(),
            Row::Opt { .. } | Row::Tweak { .. } => self.option_row(th, i, row, focused, cx),
            Row::Preset { .. } | Row::Act(_) | Row::Performance => div().into_any_element(),
            Row::Hud(h) => self.hud_row(th, i, *h, focused, cx),
            Row::Metrics => self.metrics(th),
            Row::Button(b) => {
                let (label, help, col, ic) = match b {
                    Button::Quit => ("Quit game", "Asks the game to close, like its window's close button", th.tx1, Icon::LogOut),
                    Button::ForceQuit => ("Force quit", "Stops the game's processes at once; unsaved progress is lost", th.bad, Icon::CircleX),
                };
                div()
                    .id(SharedString::from(format!("row-{i}")))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .mt(px(if *b == Button::Quit { 18. } else { 0. }))
                    .px(px(16.))
                    .py(px(12.))
                    .rounded(px(12.))
                    .bg(if focused { th.ink3 } else { th.ink2 })
                    .border_1()
                    .border_color(th.line1)
                    .cursor_pointer()
                    .focus_ring(focused, th, 12.)
                    .when(focused, |d| d.child(mark(&self.marks, Mark::Focus, MARK_PAD)))
                    .child(icon(ic, 18., col))
                    .child(div().flex().flex_col().gap(px(2.)).child(div().text_size(px(15.)).font_weight(MEDIUM).text_color(col).child(label)).child(div().text_size(px(12.)).text_color(th.tx3).child(help)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let rows = this.rows();
                        this.nav.focus_row(&rows, i);
                        this.input(Input::Activate, cx);
                    }))
                    .into_any_element()
            }
            Row::Achievement(k) => {
                let Some(a) = self.data.achievements.ready().and_then(|a| a.list.get(*k)) else { return div().into_any_element() };
                let masked = a.hidden && !a.unlocked && !self.nav.revealed.contains(k);
                let (name, desc) = if masked { ("Hidden achievement".to_owned(), "A to reveal".to_owned()) } else { (a.name.clone(), a.description.clone()) };
                let when = a.unlocked_at.map(|t| t.with_timezone(&chrono::Local).format("%-d %b %Y").to_string());
                div()
                    .id(SharedString::from(format!("row-{i}")))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(10.))
                    .rounded(px(12.))
                    .bg(if focused { th.ink3 } else { th.ink2 })
                    .focus_ring(focused, th, 12.)
                    .when(focused, |d| d.child(mark(&self.marks, Mark::Focus, MARK_PAD)))
                    .child(div().flex_none().size(px(34.)).rounded(px(9.)).bg(if a.unlocked { th.acc_soft } else { th.ink4 }).flex().items_center().justify_center().child(icon(
                        if masked { Icon::Lock } else { Icon::Trophy },
                        17.,
                        if a.unlocked { th.acc_hi } else { th.tx4 },
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(15.)).font_weight(MEDIUM).text_color(if a.unlocked { th.tx1 } else { th.tx2 }).truncate().child(name))
                            .child(div().text_size(px(12.)).text_color(th.tx3).line_clamp(1).child(desc)),
                    )
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(when.map(|w| format!("Earned {w}")).unwrap_or_else(|| if a.unlocked { "Earned".into() } else { String::new() })))
                    .into_any_element()
            }
        }
    }

    /// The body of a section without its own layout: its rows in one card, scrolling.
    fn rows_body(&self, th: &Theme, docked: bool, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.rows();
        let focused = self.nav.focused_row(&rows);
        let dialog = self.nav.confirm.is_some();
        let body: Vec<AnyElement> = rows.iter().enumerate().map(|(i, r)| self.render_row(th, i, r, focused == Some(i) && !dialog, cx)).collect();
        let list = div()
            .id("body")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(body);
        if docked {
            return list.px(px(12.)).py(px(8.)).into_any_element();
        }
        card(th).size_full().p(px(20.)).child(list).into_any_element()
    }

    /// The section's body: Mods and Tweaks lay out their own; the others list their rows.
    fn section_body(&self, th: &Theme, docked: bool, cx: &mut Context<Self>) -> AnyElement {
        match self.nav.section {
            PanelSection::Mods => super::sections::mods_view::mods_body(self, th, docked, cx),
            PanelSection::Tweaks => super::sections::mods_view::tweaks_body(self, th, docked, cx),
            PanelSection::Performance => perf::view::body(self, th, docked, cx),
            _ => self.rows_body(th, docked, cx),
        }
    }

    /// The chip over the game, beside the dock.
    fn chip_el(&self, th: &Theme) -> Option<AnyElement> {
        let c = self.chip.as_ref()?;
        let now = Instant::now();
        let o = c.opacity(now);
        if o <= 0.0 {
            return None;
        }
        let tone = tone_color(th, c.when.tone());
        Some(
            div()
                .absolute()
                .top(px(DOCK_MARGIN + 4.))
                .left(px(DOCK_MARGIN + DOCK_W + 36.))
                .opacity(o)
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(34.))
                .px(px(14.))
                .rounded(px(10.))
                .bg(th.ink0.opacity(0.78))
                .border_1()
                .border_color(if c.comparing { th.acc } else { th.line2 })
                .text_size(px(13.))
                .child(mark(&self.marks, Mark::Chip, MARK_PAD))
                .child(div().font_weight(SEMIBOLD).text_color(th.tx1).child(c.label.clone()))
                .when(!c.before.is_empty(), |d| d.child(div().text_color(th.tx2).child(c.before.clone())).child(icon(if c.comparing { Icon::ArrowDownUp } else { Icon::ArrowRight }, 14., th.tx2)))
                .child(div().font_weight(SEMIBOLD).text_color(tone).child(c.after.clone()))
                .child(div().text_color(th.tx3).child(c.tail(now)))
                .into_any_element(),
        )
    }

    fn confirm(&self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = self.nav.confirm?;
        let game = self.data.session.ready().map(|s| s.name.clone()).unwrap_or_else(|| "the game".into());
        let (title, body, yes) = match c.what {
            Button::Quit => (format!("Quit {game}?"), "Asks the game to close, as its window's close button does. The game may ask you first; anything unsaved may be lost.", "Quit"),
            Button::ForceQuit => (format!("Force quit {game}?"), "Stops the game's processes at once. Anything unsaved is lost.", "Force quit"),
        };
        let btn = |id: &'static str, label: &'static str, on: bool, danger: bool, yes: bool| {
            div()
                .id(id)
                .px(px(20.))
                .h(px(42.))
                .flex()
                .items_center()
                .rounded(px(10.))
                .text_size(px(15.))
                .font_weight(MEDIUM)
                .bg(if on && danger { th.bad } else if on { th.ink5 } else { th.ink3 })
                .text_color(if on && danger { th.ink0 } else { th.tx1 })
                .cursor_pointer()
                .focus_ring(on, th, 10.)
                .when(on, |d| d.child(mark(&self.marks, Mark::DialogFocus, MARK_PAD)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(c) = &mut this.nav.confirm {
                        c.yes = yes;
                    }
                    this.input(Input::Activate, cx);
                }))
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(th.scrim.opacity(0.5))
                .child(
                    div()
                        .relative()
                        .w(px(520.))
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(mark(&self.marks, Mark::Dialog, 4.))
                        .p(px(24.))
                        .rounded(px(16.))
                        .bg(th.ink2)
                        .border_1()
                        .border_color(th.line2)
                        .child(div().font_family(DISPLAY_FONT).text_size(px(19.)).font_weight(SEMIBOLD).text_color(th.tx1).child(title))
                        .child(div().text_size(px(14.)).text_color(th.tx2).child(body))
                        .child(div().flex().justify_end().gap(px(10.)).pt(px(6.)).child(btn("cancel", "Cancel", !c.yes, false, false)).child(btn("yes", yes, c.yes, c.what == Button::ForceQuit, true))),
                )
                .into_any_element(),
        )
    }

    /// Tells the controller when the layout changes (the backdrop follows, the choice is
    /// saved).
    fn tell_layout(&mut self) {
        let docked = self.docked();
        if self.told_docked == Some(docked) {
            return;
        }
        self.told_docked = Some(docked);
        if let Some(ui) = &self.wiring.ui {
            let _ = ui.unbounded_send(Msg::PanelLayout { docked, prefs: self.nav.dock.to_map() });
        }
    }
}

/// A card of the centred layout (the mock-ups' `.card`).
pub(crate) fn card(th: &Theme) -> Div {
    div().relative().rounded(px(14.)).bg(th.ink1.opacity(0.92)).border_1().border_color(th.line1)
}

/// An on / off switch.
pub(crate) fn switch(th: &Theme, on: bool) -> Div {
    div()
        .flex_none()
        .w(px(38.))
        .h(px(22.))
        .rounded(px(11.))
        .bg(if on { th.acc } else { th.ink5 })
        .flex()
        .items_center()
        .when(on, |d| d.justify_end())
        .px(px(3.))
        .child(div().size(px(16.)).rounded(px(8.)).bg(if on { th.acc_ink } else { th.tx1 }))
}

fn why_word(w: Why) -> &'static str {
    match w {
        Why::Undo => "undo",
        Why::Reset => "reset",
        Why::Preset => "preset",
        Why::Revert => "revert this session",
        Why::SessionOnly => "session only",
        Why::Compare => "compare",
    }
}

/// The backdrop and card colours: over a blur (the Sight layer's in the game, or the
/// compositor's) a light tint (dark themes: ink-0 at 35%) and a translucent card; without one
/// the plain dim of SIGHT §9 (ink-0 at 55%).
pub fn backdrop_colors(th: &Theme, b: Backdrop) -> (Hsla, Hsla) {
    if b.blurred() { (th.ink0.opacity(0.35), th.ink1.opacity(0.78)) } else { (th.ink0.opacity(0.55), th.ink1.opacity(0.94)) }
}

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        // Filled again as this frame is laid out.
        *self.marks.borrow_mut() = DepthMarks::default();
        self.nav.pages.surfaces = self.data.options.ready().map_or(0, |l| l.len());
        self.tell_layout();
        let vp = window.viewport_size();
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let docked = self.docked();
        // The section body's room (approximate: the panel's padding, the tab row or the dock's
        // header, the hint bar), for the Performance section's layout.
        self.room = if docked {
            perf::view::Room { w: DOCK_W.min(vw - 2.0 * DOCK_MARGIN) - 24.0, h: vh - 2.0 * DOCK_MARGIN - 64.0 - BAR_H - 16.0 }
        } else {
            perf::view::Room { w: (vw - 2.0 * (vw * SAFE).max(24.0)).min(MAX_W), h: vh - (vh * SAFE).max(20.0) - 20.0 - 44.0 - 20.0 - BAR_H }
        };
        perf::view::sync_preview(self, cx);
        let (scrim, _) = backdrop_colors(&th, self.backdrop);
        let confirm = self.confirm(&th, cx);
        let root = div()
            .id("panel")
            .relative()
            .size_full()
            .font_family(UI_FONT)
            .text_color(th.tx1)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key_down(ev, cx)))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| this.key_up(ev, cx)));
        if docked {
            // No scrim, no blur: the game in full view beside the column.
            let chip = self.chip_el(&th);
            let column = div()
                .absolute()
                .left(px(DOCK_MARGIN))
                .top(px(DOCK_MARGIN))
                .bottom(px(DOCK_MARGIN))
                .w(px(DOCK_W.min(vw - 2.0 * DOCK_MARGIN)))
                .flex()
                .flex_col()
                .rounded(px(16.))
                .bg(th.ink1.opacity(0.95))
                .border_1()
                .border_color(th.line2)
                .overflow_hidden()
                .child(mark(&self.marks, Mark::Card, 0.))
                .child(self.dock_header(&th, cx))
                .child(div().flex_1().min_h_0().flex().flex_col().child(self.section_body(&th, true, cx)))
                .child(self.hint_bar(&th, true));
            return root.child(column).when_some(chip, |d, c| d.child(c)).when_some(confirm, |d, c| d.child(c));
        }
        let pad_x = (vw * SAFE).max(24.0);
        let pad_y = (vh * SAFE).max(20.0);
        root.bg(scrim)
            .flex()
            .flex_col()
            .child(
                div().flex_1().min_h_0().flex().justify_center().px(px(pad_x)).pt(px(pad_y)).pb(px(20.)).child(
                    div()
                        .relative()
                        .w_full()
                        .max_w(px(MAX_W))
                        .h_full()
                        .flex()
                        .flex_col()
                        .gap(px(20.))
                        .child(mark(&self.marks, Mark::Card, 0.))
                        .child(self.tab_row(&th, cx))
                        .child(div().flex_1().min_h_0().flex().flex_col().child(self.section_body(&th, false, cx))),
                ),
            )
            .child(self.hint_bar(&th, false))
            .when_some(confirm, |d, c| d.child(c))
    }
}

/// `Load` in a word, for logs.
pub fn load_word<T>(l: &Load<T>) -> &'static str {
    match l {
        Load::Loading => "loading",
        Load::Ready(_) => "ready",
        Load::Failed(_) => "failed",
        Load::Unavailable(_) => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_words() {
        assert_eq!(power_of("Charging\n"), Power::Charging);
        assert_eq!(power_of("Full"), Power::PluggedIn);
        assert_eq!(power_of("Not charging"), Power::PluggedIn);
        assert_eq!(power_of("Discharging"), Power::Battery);
        assert_eq!(battery_text(58, Power::PluggedIn), "58% · plugged in");
        assert_eq!(battery_text(41, Power::Battery), "41%");
        assert_eq!(battery_text(70, Power::Charging), "70% · charging");
    }

    #[test]
    fn the_chip_gives_way_to_a_dialog() {
        let b = |x: f32| Bounds { origin: gpui::point(px(x), px(0.)), size: gpui::size(px(10.), px(10.)) };
        let mut m = DepthMarks { card: Some(b(0.)), chip: Some(b(1.)), focus: Some(b(2.)), ..Default::default() };
        assert_eq!(m.parts().count(), 3);
        m.dialog = Some(b(3.));
        m.dialog_focus = Some(b(4.));
        let parts: Vec<_> = m.parts().map(|(b, _)| f32::from(b.origin.x)).collect();
        assert_eq!(parts, [0., 2., 3., 4.], "four lifts at most: the chip goes");
    }
}
