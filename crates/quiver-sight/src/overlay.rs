//! What Sight shows over the game without taking its input (design/SIGHT.md §16): the toast
//! lane, the status chip ("Sight · 118 fps · 21:14 · 1 h 12 m", top-left, a few seconds when
//! the panel or the quick menu closes, or on `quiver sight chip`; the `status_chip.enabled`
//! setting, off by default) and the first-launch hint ("Open Sight: Shift+F2 or Guide+Start",
//! bottom centre, once per install; `hint.shown`, which `quiver sight hint --reset` clears).
//!
//! Drawn in the game by the Sight layer like the panel (`panel::ingame`), through the same
//! `panel.image`, but shown without capture (`quiver_perf::panel::PANEL_SHOW`): the game keeps
//! its keyboard and mouse, and a preload from before reads it as closed. Only where the game's
//! layer says it draws such images (`PANEL_LAYER_CAN_SHOW`); the panel and the quick menu take
//! the image over while they're open. On a stereo frame it's laid out for one eye and drawn in
//! each, the toasts and the hint at the panel's rung, the chip at the HUD's
//! (design/STEREO-UI.md §12). Elsewhere the toasts go to their layer-shell surface and the hint
//! becomes a toast; the chip is only drawn in the game.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use gpui::{App, AppContext, Bounds, Context, Entity, FocusHandle, IntoElement, ParentElement, Pixels, Render, Styled, Window, canvas, div, prelude::*, px};
use quiver_perf::panel::PanelInput;
use quiver_perf::shm::Mapping;
use quiver_perf::stereo::{Stereo, rung};
use quiver_ui::assets::UI_FONT;
use quiver_ui::components::{SEMIBOLD, glyph, kbd};
use quiver_ui::theme::Theme;

use crate::panel::ingame::{self, DepthParts, InGame, InGameView};
use crate::place::ToastCorner;
use crate::toasts::{WIDTH, toast, ToastData};

/// How long the chip and the hint stay.
pub const CHIP_FOR: Duration = Duration::from_secs(4);
pub const HINT_FOR: Duration = Duration::from_secs(9);

/// What the chip says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chip {
    pub fps: Option<f64>,
    /// The session's start (its play time).
    pub started: Option<DateTime<Utc>>,
}

/// "1 h 12 m", "12 m", "40 s".
pub fn play_time(secs: i64) -> String {
    let s = secs.max(0);
    match (s / 3600, (s % 3600) / 60) {
        (0, 0) => format!("{s} s"),
        (0, m) => format!("{m} m"),
        (h, m) => format!("{h} h {m} m"),
    }
}

/// The chip's parts after "Sight": fps (when known), the clock, the play time (when known).
pub fn chip_parts(c: &Chip, now: DateTime<Utc>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(f) = c.fps.filter(|f| f.is_finite() && *f > 0.0) {
        v.push(format!("{} fps", f.round() as i64));
    }
    v.push(now.with_timezone(&Local).format("%H:%M").to_string());
    if let Some(s) = c.started {
        v.push(play_time((now - s).num_seconds()));
    }
    v
}

/// Whether the first-launch hint shows now, and where (`Some(true)`: in the game; `Some(false)`:
/// as a toast, the game's layer not drawing after [`HINT_WAIT`]): once per install
/// (`hint.shown`), once per run.
pub fn hint_due(s: &quiver_api::sight::SightSettings, done: bool, in_game: bool, waited: Duration) -> Option<bool> {
    if s.hint.shown || done {
        return None;
    }
    (in_game || waited >= HINT_WAIT).then_some(in_game)
}

/// How long the hint waits for the game's layer to draw before it's a toast instead.
pub const HINT_WAIT: Duration = Duration::from_secs(20);

pub struct OverlayView {
    pub toasts: Vec<(u64, ToastData)>,
    pub corner: ToastCorner,
    pub chip: Option<Chip>,
    pub hint: bool,
    focus: FocusHandle,
    parts: Rc<RefCell<Vec<(Bounds<Pixels>, u16)>>>,
}

impl OverlayView {
    pub fn new(cx: &mut Context<Self>) -> OverlayView {
        OverlayView { toasts: Vec::new(), corner: ToastCorner::TopRight, chip: None, hint: false, focus: cx.focus_handle(), parts: Rc::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty() && self.chip.is_none() && !self.hint
    }
}

impl InGameView for OverlayView {
    fn depth_parts(&self) -> DepthParts {
        let p = self.parts.clone();
        Rc::new(move || p.borrow().clone())
    }
    fn focus_handle(&self) -> Option<FocusHandle> {
        let _ = &self.focus;
        None
    }
}

/// Records its parent's bounds (padded) as a part at `depth`.
fn part(parts: &Rc<RefCell<Vec<(Bounds<Pixels>, u16)>>>, depth: u16) -> impl IntoElement {
    let parts = parts.clone();
    canvas(move |bounds, _, _| parts.borrow_mut().push((bounds.dilate(px(6.)), depth)), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
}

/// The status chip.
pub fn chip(th: &Theme, c: &Chip, now: DateTime<Utc>) -> gpui::Div {
    let dot = || div().text_color(th.tx3).child("·");
    let mut d = div().flex().gap(px(10.)).items_center().px(px(12.)).py(px(6.)).rounded_full().bg(th.ink0.opacity(0.62)).text_size(px(12.)).font_weight(SEMIBOLD).text_color(th.tx1);
    d = d.child(div().text_color(th.acc_hi).child("Sight"));
    for p in chip_parts(c, now) {
        d = d.child(dot()).child(div().child(p));
    }
    d
}

/// The first-launch hint bar.
pub fn hint_bar(th: &Theme) -> gpui::Div {
    let sep = |s: &'static str| div().text_color(th.tx3).child(s);
    div()
        .flex()
        .gap(px(12.))
        .items_center()
        .px(px(16.))
        .py(px(10.))
        .rounded(px(12.))
        .bg(th.ink0.opacity(0.72))
        .text_size(px(13.))
        .text_color(th.tx1)
        .child("Open Sight")
        .child(kbd(th, "Shift").text_size(px(12.)).text_color(th.tx1))
        .child(sep("+"))
        .child(kbd(th, "F2").text_size(px(12.)).text_color(th.tx1))
        .child(sep("or"))
        .child(glyph(th, "Guide", false))
        .child(sep("+"))
        .child(glyph(th, "Start", false))
        .child(sep("· choose a Quick action, then press A or Enter"))
}

impl Render for OverlayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        self.parts.borrow_mut().clear();
        let now = Utc::now();
        let stack = (!self.toasts.is_empty()).then(|| {
            let d = div().absolute().right(px(28.)).w(px(WIDTH)).flex().flex_col().gap(px(10.));
            let d = match self.corner {
                ToastCorner::TopRight => d.top(px(24.)),
                ToastCorner::BottomRight => d.bottom(px(24.)),
            };
            d.children(self.toasts.iter().map(|(_, t)| toast(&th, t))).child(part(&self.parts, rung::PANEL))
        });
        let chip = self.chip.as_ref().map(|c| div().absolute().left(px(24.)).top(px(20.)).child(div().relative().child(chip(&th, c, now)).child(part(&self.parts, rung::HUD))));
        let hint = self.hint.then(|| div().absolute().left_0().right_0().bottom(px(28.)).flex().justify_center().child(div().relative().child(hint_bar(&th)).child(part(&self.parts, rung::PANEL))));
        div().size_full().relative().font_family(UI_FONT).when_some(chip, |d, c| d.child(c)).when_some(stack, |d, s| d.child(s)).when_some(hint, |d, h| d.child(h))
    }
}

/// Where the overlay can go: a session whose layer shows images without capture.
#[derive(Clone)]
pub struct Target {
    pub session: String,
    pub input: Arc<PanelInput<Mapping>>,
    pub dir: std::path::PathBuf,
    pub extent: (u32, u32),
    pub stereo: Stereo,
    /// The game's monitor's logical size, when known.
    pub monitor: Option<(f32, f32)>,
}

impl Target {
    fn key(&self) -> (String, (u32, u32), Stereo) {
        (self.session.clone(), self.extent, self.stereo)
    }
}

/// The overlay drawn in a game.
#[derive(Default)]
pub struct Overlay {
    shown: Option<(InGame, Entity<OverlayView>, (String, (u32, u32), Stereo))>,
    /// The chip, until when.
    pub chip: Option<(Chip, Instant)>,
    /// The hint, until when.
    pub hint: Option<Instant>,
}

impl Overlay {
    /// The session it's drawn in now.
    pub fn session(&self) -> Option<&str> {
        self.shown.as_ref().map(|(g, _, _)| g.session.as_str())
    }

    pub fn show_chip(&mut self, c: Chip) {
        self.chip = Some((c, Instant::now() + CHIP_FOR));
    }

    pub fn show_hint(&mut self) {
        self.hint = Some(Instant::now() + HINT_FOR);
    }

    /// Shows `toasts` (and the chip and hint while they last) in `target`'s game, or nothing
    /// (`None`: the game can't, or the panel holds the image). Returns whether it's drawn.
    pub fn sync(&mut self, target: Option<Target>, toasts: Vec<(u64, ToastData)>, corner: ToastCorner, cx: &mut App) -> Result<bool> {
        let now = Instant::now();
        if self.chip.as_ref().is_some_and(|(_, t)| *t <= now) {
            self.chip = None;
        }
        if self.hint.is_some_and(|t| t <= now) {
            self.hint = None;
        }
        let chip = self.chip.as_ref().map(|(c, _)| c.clone());
        let hint = self.hint.is_some();
        let empty = toasts.is_empty() && chip.is_none() && !hint;
        let Some(target) = target.filter(|_| !empty) else {
            self.close(cx);
            return Ok(false);
        };
        // Another view took the image over (the panel, the quick menu): let go of it.
        if self.shown.as_ref().is_some_and(|(g, _, k)| g.superseded() || *k != target.key()) {
            self.close(cx);
        }
        if let Some((_, v, _)) = &self.shown {
            v.update(cx, |v, cx| {
                v.toasts = toasts;
                v.corner = corner;
                v.chip = chip;
                v.hint = hint;
                cx.notify();
            });
            return Ok(true);
        }
        let view = cx.new(|cx| {
            let mut v = OverlayView::new(cx);
            (v.toasts, v.corner, v.chip, v.hint) = (toasts, corner, chip, hint);
            v
        });
        let stereo = target.stereo.fits(target.extent).then_some(target.stereo).unwrap_or_default();
        let factor = ingame::factor(stereo.logical_extent(target.extent), target.monitor);
        let g = InGame::show(&target.session, target.input.clone(), &target.dir, target.extent, stereo, factor, view.clone(), cx)?;
        self.shown = Some((g, view, target.key()));
        Ok(true)
    }

    /// Every tick: the image's heartbeat.
    pub fn renew(&mut self) {
        if let Some((g, _, _)) = &mut self.shown {
            g.renew();
        }
    }

    /// The layer hasn't drawn it within [`ingame::CONFIRM_WITHIN`] (the game isn't
    /// presenting, or it can't after all).
    pub fn failed(&self) -> bool {
        self.shown.as_ref().is_some_and(|(g, _, _)| !g.confirmed && g.opened.elapsed() > ingame::CONFIRM_WITHIN)
    }

    pub fn close(&mut self, cx: &mut App) {
        if let Some((g, _, _)) = self.shown.take() {
            g.close(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_shows_once() {
        let mut s = quiver_api::sight::SightSettings::default();
        // Not drawing yet: wait, then a toast.
        assert_eq!(hint_due(&s, false, false, Duration::from_secs(1)), None);
        assert_eq!(hint_due(&s, false, false, HINT_WAIT), Some(false));
        assert_eq!(hint_due(&s, false, true, Duration::ZERO), Some(true));
        // Shown this run, or on this install: never again (until `hint.shown` is reset).
        assert_eq!(hint_due(&s, true, true, Duration::ZERO), None);
        s.set("hint.shown", &serde_json::json!(true)).unwrap();
        assert!(s.hint.shown_at.is_some());
        assert_eq!(hint_due(&s, false, true, Duration::ZERO), None);
        s.set("hint.shown", &serde_json::json!(false)).unwrap();
        assert_eq!(hint_due(&s, false, true, Duration::ZERO), Some(true));
    }

    #[test]
    fn chip_reads_like_the_mock() {
        let now = Utc::now();
        let c = Chip { fps: Some(117.6), started: Some(now - chrono::Duration::seconds(72 * 60 + 5)) };
        let p = chip_parts(&c, now);
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].as_str(), p[2].as_str()), ("118 fps", "1 h 12 m"));
        assert_eq!(p[1].len(), 5, "HH:MM");
        // Nothing known but the clock.
        assert_eq!(chip_parts(&Chip::default(), now).len(), 1);
        assert_eq!((play_time(40), play_time(12 * 60), play_time(-3)), ("40 s".into(), "12 m".into(), "0 s".into()));
    }
}
