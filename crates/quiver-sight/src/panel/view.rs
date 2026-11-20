//! The panel's GPUI view: the full-output surface over the game (SIGHT §9 "Panel"). A
//! backdrop (a light tint over the compositor's blur, or a plain dim) under a centred card
//! with a header (game, profile, clock, battery), the section list, the section's rows and a
//! hint bar. Everything it changes goes to quiverd through the link ([`LinkReq`]); values show
//! what was asked at once and what came of it when quiverd answers, never optimism.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use chrono::Utc;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    AnyElement, Bounds, Context, Div, FocusHandle, Hsla, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, ScrollHandle, SharedString, Styled, Window, canvas,
    div, prelude::*, px,
};
use quiver_api::options::{OptionOp, OptionState, OptionsChange};
use quiver_api::perf::{HudScope, PerfValue, Stat};
use quiver_api::sight::{Backdrop, PanelSection};
use quiver_ui::assets::{DISPLAY_FONT, MONO_FONT, UI_FONT};
use quiver_ui::components::{FocusExt, Icon, MEDIUM, PillKind, SEMIBOLD, caption, glyph, icon, pill};
use quiver_ui::input::Input;
use quiver_ui::options::{Tone, apply_badge, draft_value, outcome_text, pending_text, short_value};
use quiver_ui::theme::Theme;
use serde_json::Value;

use super::state::{ANCHORS, Button, Effect, HudRow, Load, Nav, PRESETS, PanelData, Row, Zone, adjustable, cycle, key_input, rows_nav, step_scale, step_value};
use crate::link::{Change, LinkReq, Loaded, Msg};

/// Widest the card gets (ultra-wide outputs), logical px (SIGHT §9).
const MAX_W: f32 = 1400.0;
/// Safe area on every side, a fraction of the output (TVs).
const SAFE: f32 = 0.035;
/// Edits to a row wait this long for the next step before going to quiverd (one revision
/// per burst of presses, not one per press).
const DEBOUNCE: Duration = Duration::from_millis(350);

/// What the view needs from the rest of quiver-sight. All optional, so fixtures render it
/// off-screen without quiverd.
#[derive(Clone, Default)]
pub struct Wiring {
    pub link: Option<tokio::sync::mpsc::UnboundedSender<LinkReq>>,
    /// To quiver-sight's own loop (close requests).
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
    /// The card (the panel's depth).
    pub card: Option<Bounds<Pixels>>,
    /// The focused row or section.
    pub focus: Option<Bounds<Pixels>>,
    /// A dialog's box, and its focused button.
    pub dialog: Option<Bounds<Pixels>>,
    pub dialog_focus: Option<Bounds<Pixels>>,
}

#[derive(Clone, Copy)]
enum Mark {
    Card,
    Focus,
    Dialog,
    DialogFocus,
}

impl DepthMarks {
    /// Each part with its rung (thousandths, `quiver_perf::stereo::rung`), back to front.
    pub fn parts(&self) -> impl Iterator<Item = (Bounds<Pixels>, u16)> + '_ {
        use quiver_perf::stereo::rung;
        [(self.card, rung::PANEL), (self.focus, rung::FOCUS), (self.dialog, rung::DIALOG), (self.dialog_focus, rung::DIALOG_FOCUS)]
            .into_iter()
            .filter_map(|(b, d)| b.map(|b| (b, d)))
    }

    fn slot(&mut self, m: Mark) -> &mut Option<Bounds<Pixels>> {
        match m {
            Mark::Card => &mut self.card,
            Mark::Focus => &mut self.focus,
            Mark::Dialog => &mut self.dialog,
            Mark::DialogFocus => &mut self.dialog_focus,
        }
    }
}

/// Background around a marked part, logical px: covers a focus ring (5 px outside the
/// element) and a pixel of what's behind it, which a lift's moved-off strip is filled from.
const MARK_PAD: f32 = 6.0;

/// The focused part's depth marker, for the sections that draw their own rows
/// (`sections::session`, `sections::achievements`).
pub fn focus_mark(marks: &Rc<RefCell<DepthMarks>>) -> impl IntoElement {
    mark(marks, Mark::Focus, MARK_PAD)
}

/// An invisible child covering its (relative) parent that records the parent's bounds, padded
/// and clipped to the content mask, as part `m`.
fn mark(marks: &Rc<RefCell<DepthMarks>>, m: Mark, pad: f32) -> impl IntoElement {
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

pub struct PanelView {
    pub session: String,
    pub data: PanelData,
    pub nav: Nav,
    pub backdrop: Backdrop,
    pub footer: Footer,
    /// Values asked for and not yet confirmed, by (surface, id) (tweaks: ("tweaks", id)).
    drafts: BTreeMap<(String, String), Value>,
    edit_gen: u64,
    /// What came of the last change per row.
    outcomes: BTreeMap<(String, String), (String, Tone)>,
    hud_draft: BTreeMap<HudRow, String>,
    hud_note: Option<(String, Tone)>,
    /// A line over the hint bar ("Quit sent", errors).
    flash: Option<(String, Tone)>,
    wiring: Wiring,
    pub focus: FocusHandle,
    scroll: ScrollHandle,
    side_scroll: ScrollHandle,
    /// The parts at their own depth in the last frame laid out ([`DepthMarks`]).
    pub marks: Rc<RefCell<DepthMarks>>,
    /// What came of the last screenshot (the Session section shows it under the button).
    pub shot: Option<(String, Tone)>,
    /// The Achievements list's own scroll.
    pub ach_scroll: ScrollHandle,
}

fn tone_color(th: &Theme, t: Tone) -> Hsla {
    match t {
        Tone::Ok => th.ok,
        Tone::Warn => th.warn,
        Tone::Bad => th.bad,
        Tone::Accent => th.acc_hi,
        Tone::Muted => th.tx3,
    }
}

fn tone_pill(t: Tone) -> PillKind {
    match t {
        Tone::Ok => PillKind::Ok,
        Tone::Warn => PillKind::Warn,
        Tone::Bad => PillKind::Bad,
        Tone::Accent => PillKind::Accent,
        Tone::Muted => PillKind::Muted,
    }
}

fn battery() -> Option<(u32, bool)> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for e in dir.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type")).ok()?.trim() != "Battery" {
            continue;
        }
        let cap: u32 = std::fs::read_to_string(p.join("capacity")).ok()?.trim().parse().ok()?;
        let charging = std::fs::read_to_string(p.join("status")).is_ok_and(|s| matches!(s.trim(), "Charging" | "Full"));
        return Some((cap, charging));
    }
    None
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
            edit_gen: 0,
            outcomes: BTreeMap::new(),
            hud_draft: BTreeMap::new(),
            hud_note: None,
            flash: None,
            wiring,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            side_scroll: ScrollHandle::new(),
            marks: Rc::default(),
            shot: None,
            ach_scroll: ScrollHandle::new(),
        }
    }

    fn send(&self, r: LinkReq) {
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

    pub fn rows(&self) -> Vec<Row> {
        rows_nav(&self.nav, &self.data, Utc::now())
    }

    /// The value a row shows: what was asked for, else the option's own.
    pub fn draft_of(&self, row: &Row) -> Option<Value> {
        let st = self.option_state(row)?;
        Some(Self::row_key(row).and_then(|k| self.drafts.get(&k).cloned()).unwrap_or_else(|| draft_value(st, None)))
    }

    pub fn option_state(&self, row: &Row) -> Option<&OptionState> {
        match row {
            Row::Opt { surface, id } => self.data.options.ready()?.iter().find(|s| &s.key == surface)?.options.get(id),
            Row::Tweak { id } => self.data.tweaks.ready()?.options.get(id),
            _ => None,
        }
    }

    fn row_key(row: &Row) -> Option<(String, String)> {
        match row {
            Row::Opt { surface, id } => Some((surface.clone(), id.clone())),
            Row::Tweak { id } => Some(("tweaks".into(), id.clone())),
            _ => None,
        }
    }

    pub fn can_adjust(&self, row: &Row) -> bool {
        match row {
            Row::Opt { .. } | Row::Tweak { .. } => self.option_state(row).is_some_and(adjustable),
            Row::Hud(_) => self.data.hud.ready().is_some(),
            Row::Pref(_) | Row::AchFilter => true,
            _ => false,
        }
    }

    /// One input from the keyboard, the controller (gilrs, or hyprpad's keys) or a click.
    pub fn input(&mut self, i: Input, cx: &mut Context<Self>) {
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

    fn scroll_to_focus(&self) {
        let rows = self.rows();
        if let Some(r) = self.nav.focused_row(&rows) {
            match self.nav.section {
                // These draw their rows in cards: the Achievements list scrolls on its own.
                PanelSection::Achievements => {
                    if let Some(p) = super::sections::achievements::list_pos(&rows, r) {
                        self.ach_scroll.scroll_to_item(p);
                    }
                }
                PanelSection::Session => {}
                _ => self.scroll.scroll_to_item(r),
            }
        }
        self.side_scroll.scroll_to_item(self.nav.section.index());
    }

    fn effect(&mut self, e: Effect, cx: &mut Context<Self>) {
        match e {
            Effect::None | Effect::Moved | Effect::Reveal(_) => {}
            Effect::Close => self.close(),
            Effect::Adjust(row, dir) => self.adjust(row, dir, cx),
            Effect::Confirmed(b) => self.confirmed(b),
        }
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
            Button::Screenshot => {
                self.send(LinkReq::Screenshot { session: self.session.clone() });
                self.shot = Some(("Taking a screenshot…".into(), Tone::Muted));
            }
        }
    }

    fn adjust(&mut self, row: Row, dir: i32, cx: &mut Context<Self>) {
        match &row {
            Row::Opt { .. } | Row::Tweak { .. } => {
                let Some(key) = Self::row_key(&row) else { return };
                let Some(st) = self.option_state(&row) else { return };
                let draft = self.drafts.get(&key).cloned().unwrap_or_else(|| draft_value(st, None));
                let Some(v) = step_value(st, &draft, dir) else { return };
                self.drafts.insert(key.clone(), v.clone());
                self.outcomes.remove(&key);
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
            Row::Hud(h) => self.adjust_hud(*h, dir),
            Row::Pref(p) => {
                super::sections::prefs::step(&mut self.data.prefs, *p, dir);
                if let Err(e) = super::sections::prefs::save_to(&super::sections::prefs::path(), &self.data.prefs, *p) {
                    self.flash = Some((format!("Couldn't save the setting: {e}"), Tone::Bad));
                }
            }
            Row::AchFilter => self.nav.ach_filter = self.nav.ach_filter.step(dir),
            _ => {}
        }
    }

    /// Sends every staged value (one request per row: outcomes are per row).
    fn flush(&mut self, _cx: &mut Context<Self>) {
        let Some(view) = self.data.session.ready().cloned() else { return };
        for ((surface, id), v) in &self.drafts {
            let unchanged = if surface == "tweaks" {
                self.data.tweaks.ready().and_then(|s| s.options.get(id)).is_some_and(|st| quiver_api::options::same_value(&st.value, v))
            } else {
                self.data.options.ready().and_then(|l| l.iter().find(|s| &s.key == surface)).and_then(|s| s.options.get(id)).is_some_and(|st| quiver_api::options::same_value(&st.value, v))
            };
            if unchanged || self.outcomes.contains_key(&(surface.clone(), id.clone())) {
                continue;
            }
            if surface == "tweaks" {
                let op = OptionOp::Set { surface: self.data.tweaks.ready().map(|s| s.key.clone()).unwrap_or_default(), id: id.clone(), value: v.clone() };
                self.send(LinkReq::SetTweak { view: view.clone(), op, id: id.clone() });
            } else {
                let op = OptionOp::Set { surface: surface.clone(), id: id.clone(), value: v.clone() };
                self.send(LinkReq::SetOption { view: view.clone(), op, key: (surface.clone(), id.clone()) });
            }
            self.outcomes.insert((surface.clone(), id.clone()), ("Saving…".into(), Tone::Muted));
        }
    }

    pub fn hud_value(&self, h: HudRow) -> String {
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
            Loaded::Options(o) => {
                self.data.options = o;
                self.drafts.retain(|k, _| k.0 == "tweaks");
            }
            Loaded::Tweaks(t) => {
                self.data.tweaks = t;
                self.drafts.retain(|k, _| k.0 != "tweaks");
            }
            Loaded::Hud(h) => {
                self.data.hud = h;
                self.hud_draft.clear();
            }
        }
    }

    /// quiverd's answer to a change: the row's outcome, and a reload of what it changed.
    pub fn done(&mut self, change: Change, result: Result<Option<OptionsChange>, String>) {
        let part = match &change {
            Change::Option(..) => super::data::Part::Options,
            Change::Tweak(_) => super::data::Part::Tweaks,
            Change::Hud => super::data::Part::Hud,
            Change::Stop => {
                self.flash = Some(match result {
                    Ok(_) => ("Stopped the game".into(), Tone::Ok),
                    Err(e) => (format!("Couldn't stop the game: {e}"), Tone::Bad),
                });
                return;
            }
            Change::Screenshot(path) => {
                self.shot = Some(match result {
                    Ok(_) => (format!("Saved {}", crate::panel::sections::session::home_relative(path)), Tone::Ok),
                    Err(e) => (format!("No screenshot: {e}"), Tone::Bad),
                });
                return;
            }
        };
        let key = match &change {
            Change::Option(s, id) => Some((s.clone(), id.clone())),
            Change::Tweak(id) => Some(("tweaks".into(), id.clone())),
            _ => None,
        };
        let text = match (&result, &key) {
            (Err(e), _) => (format!("Not saved: {e}"), Tone::Bad),
            (Ok(Some(c)), Some((_, id))) => c.outcomes.iter().find(|o| &o.id == id).map(|o| outcome_text(&o.outcome)).unwrap_or(("Saved".into(), Tone::Ok)),
            (Ok(_), _) => {
                let live = self.data.hud.ready().is_some_and(|h| h.live());
                (if live { "Applied live".into() } else { "Saved · applies next launch".into() }, if live { Tone::Ok } else { Tone::Muted })
            }
        };
        match key {
            Some(k) => {
                self.outcomes.insert(k, text);
            }
            None => self.hud_note = Some(text),
        }
        self.send(LinkReq::Load { session: self.session.clone(), part, view: self.data.session.ready().cloned() });
    }

    pub fn key_down(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.key == "f2" && k.modifiers.shift {
            self.close();
            return;
        }
        if let Some(i) = key_input(&k.key, k.modifiers.shift, k.modifiers.control) {
            self.input(i, cx);
        }
    }

    // ---- Rendering --------------------------------------------------------------------------

    fn header(&self, th: &Theme) -> Div {
        let (name, sub) = match self.data.session.ready() {
            Some(s) => (s.name.clone(), format!("{} · {}", s.profile_name.clone().unwrap_or_else(|| "Vanilla".into()), s.copy)),
            None => ("Quiver".into(), String::new()),
        };
        let clock = chrono::Local::now().format("%H:%M").to_string();
        let bat = battery().map(|(c, charging)| {
            let col = if c <= 15 && !charging { th.bad } else { th.tx2 };
            div().flex().items_center().gap(px(6.)).text_color(col).child(div().font_family(MONO_FONT).child(format!("{c}%{}", if charging { " ⚡" } else { "" })))
        });
        div()
            .flex()
            .items_center()
            .gap(px(16.))
            .px(px(28.))
            .py(px(18.))
            .border_b_1()
            .border_color(th.line1)
            .child(div().flex_none().size(px(34.)).rounded(px(9.)).bg(th.acc_soft).flex().items_center().justify_center().child(icon(Icon::Gamepad, 18., th.acc_hi)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().font_family(DISPLAY_FONT).text_size(px(20.)).font_weight(SEMIBOLD).text_color(th.tx1).truncate().child(name))
                    .child(div().text_size(px(13.)).text_color(th.tx3).child(sub)),
            )
            .child(div().font_family(MONO_FONT).text_size(px(18.)).text_color(th.tx1).child(clock))
            .when_some(bat, |d, b| d.child(b))
    }

    fn side(&self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.nav.zone == Zone::Side && self.nav.confirm.is_none();
        div()
            .id("sections")
            .flex_none()
            .w(px(230.))
            .h_full()
            .py(px(14.))
            .px(px(12.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .border_r_1()
            .border_color(th.line1)
            .overflow_y_scroll()
            .track_scroll(&self.side_scroll)
            .children(PanelSection::ALL.iter().map(|s| {
                let on = *s == self.nav.section;
                let ic = match s {
                    PanelSection::Session => Icon::Gamepad,
                    PanelSection::Mods => Icon::Package,
                    PanelSection::Tweaks => Icon::SlidersHorizontal,
                    PanelSection::Performance => Icon::Activity,
                    PanelSection::Achievements => Icon::Trophy,
                };
                let s = *s;
                div()
                    .id(SharedString::from(format!("section-{}", s.label())))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(44.))
                    .px(px(12.))
                    .rounded(px(10.))
                    .text_size(px(15.))
                    .font_weight(if on { SEMIBOLD } else { MEDIUM })
                    .text_color(if on { th.tx1 } else { th.tx2 })
                    .when(on, |d| d.bg(th.ink3))
                    .cursor_pointer()
                    .hover(|d| d.bg(th.ink3))
                    .child(icon(ic, 17., if on { th.acc_hi } else { th.tx3 }))
                    .child(s.label())
                    .focus_ring(on && focused, th, 10.)
                    .when(on && focused, |d| d.child(mark(&self.marks, Mark::Focus, MARK_PAD)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.nav.select_section(s);
                        this.nav.zone = Zone::Side;
                        cx.notify();
                    }))
            }))
    }

    fn stepper(&self, th: &Theme, i: usize, text: String, on: Option<bool>, can: bool, cx: &mut Context<Self>) -> Div {
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
            Some(b) => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(38.))
                        .h(px(22.))
                        .rounded(px(11.))
                        .bg(if b { th.acc } else { th.ink5 })
                        .flex()
                        .items_center()
                        .when(b, |d| d.justify_end())
                        .px(px(3.))
                        .child(div().size(px(16.)).rounded(px(8.)).bg(if b { th.acc_ink } else { th.tx2 })),
                )
                .child(div().w(px(36.)).child(if b { "On" } else { "Off" })),
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

    fn option_row(&self, th: &Theme, i: usize, row: &Row, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let (Some(st), Some(key)) = (self.option_state(row), Self::row_key(row)) else { return div().into_any_element() };
        let id = &key.1;
        let v = self.drafts.get(&key).cloned().unwrap_or_else(|| draft_value(st, None));
        let can = adjustable(st);
        let on = (st.def.ty == quiver_api::options::OptionType::Bool).then(|| v.as_bool().unwrap_or(false));
        let control = self.stepper(th, i, short_value(&st.def, &v), on, can, cx);
        let line = self
            .outcomes
            .get(&key)
            .cloned()
            .or_else(|| pending_text(&st.pending).map(|t| (t, Tone::Warn)))
            .or_else(|| st.def.danger.as_ref().map(|d| (format!("Change it in the app: {d}"), Tone::Muted)))
            .or_else(|| st.tweak.as_ref().and_then(|t| t.overridden_by).map(|by| (format!("The {} scope sets it too, so this change doesn't reach the game", by.label().to_lowercase()), Tone::Warn)));
        self.control_row(th, i, focused, st.def.label_or(id).to_owned(), st.def.help.clone(), control, Some(apply_badge(&st.def)), line, cx)
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

    fn render_row(&self, th: &Theme, i: usize, row: &Row, focused: bool, cx: &mut Context<Self>) -> AnyElement {
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
            Row::Hud(h) => self.hud_row(th, i, *h, focused, cx),
            Row::Metrics => self.metrics(th),
            Row::Button(b) => {
                let (label, help, col, ic) = match b {
                    Button::Quit => ("Quit game", "Asks the game to close, like its window's close button", th.tx1, Icon::LogOut),
                    Button::ForceQuit => ("Force quit", "Stops the game's processes at once; unsaved progress is lost", th.bad, Icon::CircleX),
                    Button::Screenshot => ("Screenshot", "The game's next frame, saved under Pictures › Quiver", th.tx1, Icon::Image),
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
            // Drawn by the sections that have them (`sections::*`).
            Row::Pref(_) | Row::AchFilter => div().into_any_element(),
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

    fn footer(&self, th: &Theme) -> Div {
        let hint = |g: Vec<&'static str>, label: &str| div().flex().items_center().gap(px(7.)).children(g.into_iter().map(|g| glyph(th, g, false))).child(label.to_owned());
        let rows = self.rows();
        let focused = self.nav.focused_row(&rows).and_then(|i| rows.get(i));
        let mut hints = vec![];
        if self.nav.confirm.is_some() {
            hints.push(hint(vec!["A"], "Select"));
            hints.push(hint(vec!["B"], "Cancel"));
        } else {
            match focused {
                Some(r) if self.can_adjust(r) => {
                    hints.push(hint(vec!["◀", "▶"], "Change"));
                    hints.push(hint(vec!["A"], "Next value"));
                }
                Some(_) => hints.push(hint(vec!["A"], "Select")),
                None => hints.push(hint(vec!["A"], "Open")),
            }
            if self.nav.section == PanelSection::Achievements {
                if let Some(Row::Achievement(k)) = focused
                    && self.data.achievements.ready().and_then(|a| a.list.get(*k)).is_some_and(|a| super::sections::achievements::masked(a, self.data.prefs.show_hidden, self.nav.revealed.contains(k)))
                {
                    hints.push(hint(vec!["X"], "Reveal"));
                }
                hints.push(hint(vec!["LT", "RT"], "Filter"));
            }
            hints.push(hint(vec!["B"], if self.nav.zone == Zone::Body { "Back" } else { "Close" }));
            hints.push(hint(vec!["LB", "RB"], "Section"));
        }
        div()
            .flex()
            .items_center()
            .gap(px(22.))
            .px(px(28.))
            .py(px(14.))
            .border_t_1()
            .border_color(th.line1)
            .text_size(px(13.))
            .text_color(th.tx2)
            .children(hints)
            .child(div().flex_1())
            .when_some(self.flash.clone().or(self.hud_note.clone().filter(|_| self.nav.section == PanelSection::Performance)), |d, (t, tone)| {
                d.child(div().text_color(tone_color(th, tone)).child(t))
            })
            .when(self.footer.pad_shared, |d| d.child(div().text_color(th.warn).child("The game sees the controller too (no hyprpad)")))
            .child(div().text_color(th.tx3).child("Shift+F2 · Guide+Start"))
    }

    fn confirm(&self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = self.nav.confirm?;
        let game = self.data.session.ready().map(|s| s.name.clone()).unwrap_or_else(|| "the game".into());
        let (title, body, yes) = match c.what {
            Button::Quit => (format!("Quit {game}?"), "Asks the game to close, as its window's close button does. The game may ask you first; anything unsaved may be lost.", "Quit"),
            Button::ForceQuit => (format!("Force quit {game}?"), "Stops the game's processes at once. Anything unsaved is lost.", "Force quit"),
            Button::Screenshot => return None,
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
        let vp = window.viewport_size();
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let (scrim, card) = backdrop_colors(&th, self.backdrop);
        let rows = self.rows();
        let focused = self.nav.focused_row(&rows);
        let dialog = self.nav.confirm.is_some();
        let f = focused.filter(|_| !dialog);
        let body: Vec<AnyElement> = match self.nav.section {
            // The sections laid out as cards draw their own rows.
            PanelSection::Session => vec![super::sections::session::body(self, &th, &rows, f, cx)],
            PanelSection::Achievements => vec![super::sections::achievements::body(self, &th, &rows, f, cx)],
            _ => rows.iter().enumerate().map(|(i, r)| self.render_row(&th, i, r, focused == Some(i) && !dialog, cx)).collect(),
        };
        let confirm = self.confirm(&th, cx);
        div()
            .id("panel")
            .size_full()
            .bg(scrim)
            .font_family(UI_FONT)
            .text_color(th.tx1)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key_down(ev, cx)))
            .flex()
            .items_center()
            .justify_center()
            .px(px(vw * SAFE))
            .py(px(vh * SAFE))
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(px(MAX_W))
                    .h_full()
                    .flex()
                    .flex_col()
                    .rounded(px(18.))
                    .bg(card)
                    .border_1()
                    .border_color(th.line2)
                    .overflow_hidden()
                    .child(mark(&self.marks, Mark::Card, 0.))
                    .child(self.header(&th))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .child(self.side(&th, cx))
                            .child(
                                div()
                                    .id("body")
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .px(px(28.))
                                    .py(px(16.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.))
                                    .children(body),
                            ),
                    )
                    .child(self.footer(&th))
                    .when_some(confirm, |d, c| d.child(c)),
            )
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
