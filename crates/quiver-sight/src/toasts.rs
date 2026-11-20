//! The toast lane (SIGHT §9, §3.2, §16): short notes over the game, top-right of the game's
//! monitor (bottom-right when the HUD owns the top-right corner), timed, never taking input.
//! At most three, newest on top; achievements 6 s, notices with buttons 12 s, the rest 4–5 s.
//!
//! **Kinds** (design/SIGHT.md §16, the mock-up's `Main.dc.html`): an achievement unlocked; a
//! mod setting's outcome; "Saved to the cloud" (quiverd's notice after a session's saves went
//! up); a mod's hotkey flipping one of its options ("Frame Generation off · You pressed F9 ·
//! this session only"); an agent's or a mod's notice, maybe with buttons (answered from the
//! quick menu, whose first tiles they become while they're up); a download finished; errors.
//!
//! **Do not disturb** (`toasts.dnd`): only errors show ([`allowed`]).
//!
//! **Where**: drawn in the game by the Sight layer when the game's layer can show an image
//! without taking input (`crate::overlay`, in each eye on a stereo frame, lifted to the
//! toast rung), else on a layer-shell surface (namespace `quiver-sight-toast`: `quiver-sight`
//! is the panel's, which hyprpad and the blur rule match exactly), unmapped when the lane is
//! empty so it costs scan-out only while a toast is up. The lane keeps the list; whichever
//! presenter shows it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{App, Context, Div, Hsla, IntoElement, ParentElement, Render, Styled, Window, WindowHandle, canvas, div, img, prelude::*, px};
use quiver_api::options::{OpOutcome, OptionsEvent, Outcome};
use quiver_api::perf::Level;
use quiver_api::sight::{NoticeButton, NoticeKind, NoticeLevel, SightNotice};
use quiver_api::{Event, Job, JobKind};
use quiver_ui::assets::UI_FONT;
use quiver_ui::components::{Icon, SEMIBOLD, icon, kbd, soft_shadow};
use quiver_ui::theme::Theme;

use crate::desktop::open_layer;
use crate::place::ToastCorner;

pub const NAMESPACE: &str = "quiver-sight-toast";
pub const MAX_SHOWN: usize = 3;
/// A toast's width; the surface adds room for the shadow on every side.
pub const WIDTH: f32 = 380.0;
const PAD: f32 = 10.0;
/// From the screen edge (or a bar's exclusive zone) to the toast, shadow room included.
const MARGIN: f32 = 16.0 - PAD;
/// In a body, where the key goes as a keycap ("You pressed {key} · this session only").
pub const KEY_SLOT: &str = "{key}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastKind {
    Achievement,
    #[default]
    Options,
    CloudSaved,
    ModHotkey,
    /// An agent's or a mod's notice.
    Notice,
    Download,
    Error,
    /// The first-launch hint, where the game can't show it itself.
    Hint,
    Screenshot,
}

/// One toast, as the lane shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToastData {
    pub kind: ToastKind,
    /// The small caption ("Achievement unlocked", "DLSS", "Agent · aco-dlss"); empty: none.
    pub caption: String,
    pub title: String,
    pub body: Option<String>,
    /// A keycap in the body, at [`KEY_SLOT`] (else after it).
    pub key: Option<String>,
    /// An image on this machine (an achievement's icon).
    pub image: Option<PathBuf>,
    pub level: Level,
    pub duration: Duration,
    /// The running session it's about (`None`: whichever game is in front).
    pub session: Option<String>,
    /// A notice's id and buttons (chosen from the quick menu).
    pub notice: Option<String>,
    pub buttons: Vec<NoticeButton>,
    /// Shown in do-not-disturb too.
    pub urgent: bool,
    /// `achievements.on_unlock = toast_screenshot`: the screenshot was taken (it goes first,
    /// so the toast isn't in it).
    pub shot: bool,
}

impl Default for ToastData {
    fn default() -> Self {
        ToastData {
            kind: ToastKind::Options,
            caption: String::new(),
            title: String::new(),
            body: None,
            key: None,
            image: None,
            level: Level::Normal,
            duration: Duration::from_secs(4),
            session: None,
            notice: None,
            buttons: Vec::new(),
            urgent: false,
            shot: false,
        }
    }
}

/// Do not disturb lets only urgent toasts (errors) through.
pub fn allowed(t: &ToastData, dnd: bool) -> bool {
    !dnd || t.urgent
}

/// `AchievementUnlocked` → a toast.
pub fn from_event(e: &Event) -> Option<ToastData> {
    let Event::AchievementUnlocked { achievement_id, name, description, image, key, .. } = e else { return None };
    Some(ToastData {
        kind: ToastKind::Achievement,
        caption: "Achievement unlocked".into(),
        title: name.clone().filter(|n| !n.trim().is_empty()).or_else(|| key.clone()).unwrap_or_else(|| format!("Achievement {achievement_id}")),
        body: description.clone().filter(|d| !d.trim().is_empty()),
        image: image.as_ref().map(PathBuf::from).filter(|p| p.is_file()),
        level: Level::Accent,
        duration: Duration::from_secs(6),
        ..Default::default()
    })
}

/// A notice (quiverd's own, an agent's, a mod's) → a toast.
pub fn from_notice(n: &SightNotice) -> ToastData {
    let s = &n.spec;
    let kind = match s.kind {
        NoticeKind::CloudSaved => ToastKind::CloudSaved,
        NoticeKind::ModHotkey => ToastKind::ModHotkey,
        NoticeKind::Download => ToastKind::Download,
        NoticeKind::Error => ToastKind::Error,
        NoticeKind::Notice => ToastKind::Notice,
    };
    let level = match s.level {
        NoticeLevel::Info => Level::Normal,
        NoticeLevel::Success => Level::Good,
        NoticeLevel::Warning => Level::Warn,
        NoticeLevel::Error => Level::Bad,
    };
    ToastData {
        kind,
        caption: s.caption.clone().unwrap_or_default(),
        title: s.title.clone(),
        body: s.body.clone(),
        key: s.key.clone(),
        image: None,
        level,
        duration: Duration::from_secs(s.secs() as u64),
        session: s.session.clone(),
        notice: Some(n.id.clone()),
        buttons: s.buttons.clone(),
        urgent: s.urgent(),
        shot: false,
    }
}

/// An install job that just finished (or failed) → a toast. `None` for anything else.
pub fn from_job(j: &Job) -> Option<ToastData> {
    if !j.done || j.kind != JobKind::Install {
        return None;
    }
    Some(match &j.error {
        None => ToastData {
            kind: ToastKind::Download,
            caption: "Downloads".into(),
            title: "Download finished".into(),
            body: Some(format!("{} is ready to play", j.name)),
            level: Level::Good,
            duration: Duration::from_secs(5),
            ..Default::default()
        },
        Some(e) => ToastData {
            kind: ToastKind::Error,
            caption: "Downloads".into(),
            title: format!("{} didn't finish downloading", j.name),
            body: Some(e.lines().next().unwrap_or(e).chars().take(160).collect()),
            level: Level::Bad,
            duration: Duration::from_secs(6),
            urgent: true,
            ..Default::default()
        },
    })
}

fn rank(o: &Outcome) -> (u8, Level) {
    match o {
        Outcome::Rejected { .. } => (5, Level::Bad),
        Outcome::TimedOut | Outcome::Dormant { .. } => (4, Level::Warn),
        Outcome::SavedRestart { .. } => (3, Level::Warn),
        Outcome::SavedPending { .. } | Outcome::SavedAt { .. } => (2, Level::Accent),
        Outcome::AppliedLive { .. } => (1, Level::Good),
        Outcome::Unchanged => (0, Level::Dim),
    }
}

fn title_of(o: &Outcome) -> String {
    match o {
        Outcome::AppliedLive { .. } => "Applied live".into(),
        Outcome::SavedAt { at } => format!("Saved · applies at {at}"),
        Outcome::SavedRestart { running: true } => "Saved · needs a restart".into(),
        Outcome::SavedRestart { running: false } => "Saved · applies next launch".into(),
        Outcome::SavedPending { .. } => "Saved · applies later".into(),
        Outcome::Dormant { .. } => "Kept, not applied".into(),
        Outcome::Rejected { .. } => "Rejected".into(),
        Outcome::TimedOut => "Saved · the mod didn't confirm".into(),
        Outcome::Unchanged => "Unchanged".into(),
    }
}

fn detail_of(o: &OpOutcome) -> Option<String> {
    match &o.outcome {
        Outcome::Rejected { reason } | Outcome::Dormant { reason } => Some(reason.clone()),
        Outcome::SavedPending { detail } => Some(detail.clone()),
        Outcome::AppliedLive { latency: Some(l), .. } => Some(format!("the mod picks it up within {l}")),
        _ => None,
    }
}

/// An options `Outcome` event → a toast: the worst outcome sets the colour and title, the
/// changes (or the option and its new value) the body.
pub fn from_options(e: &OptionsEvent) -> Option<ToastData> {
    let OptionsEvent::Outcome { outcomes, changes, session, .. } = e else { return None };
    let worst = outcomes.iter().filter(|o| o.outcome != Outcome::Unchanged).max_by_key(|o| rank(&o.outcome).0)?;
    let (_, level) = rank(&worst.outcome);
    let n = outcomes.iter().filter(|o| o.outcome != Outcome::Unchanged).count();
    let mut title = title_of(&worst.outcome);
    if n > 1 && outcomes.iter().any(|o| o.outcome != worst.outcome && o.outcome != Outcome::Unchanged) {
        title = format!("{title} (and {} more)", n - 1);
    }
    let what = if changes.is_empty() {
        outcomes
            .iter()
            .filter(|o| o.outcome != Outcome::Unchanged)
            .map(|o| match &o.after {
                Some(v) => format!("{} → {}", o.id, v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())),
                None => o.id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        changes.join("; ")
    };
    let body = match detail_of(worst) {
        Some(d) if !what.is_empty() => format!("{what}\n{d}"),
        Some(d) => d,
        None => what,
    };
    Some(ToastData {
        kind: ToastKind::Options,
        caption: "Mod setting".into(),
        title,
        body: Some(body).filter(|b| !b.is_empty()),
        level,
        duration: Duration::from_secs(if level == Level::Bad { 6 } else { 4 }),
        session: Some(session.clone()),
        urgent: level == Level::Bad,
        ..Default::default()
    })
}

/// The first-launch hint as a toast (the desktop fallback; in the game it's its own bar).
pub fn hint_toast() -> ToastData {
    ToastData {
        kind: ToastKind::Hint,
        caption: "Sight".into(),
        title: "Open Sight: Shift+F2 or Guide+Start".into(),
        body: Some("Hold them for the quick menu · shown on the first launch only".into()),
        level: Level::Accent,
        duration: Duration::from_secs(8),
        ..Default::default()
    }
}

#[derive(Clone)]
pub struct Shown {
    pub id: u64,
    pub data: ToastData,
    pub until: Instant,
}

pub struct ToastView {
    toasts: Vec<(u64, ToastData)>,
}

impl ToastView {
    pub fn new(toasts: Vec<ToastData>) -> Self {
        Self { toasts: toasts.into_iter().enumerate().map(|(i, t)| (i as u64, t)).collect() }
    }
}

/// A toast's colours and glyph: (caption and icon colour, the badge's tint, icon).
fn look(th: &Theme, t: &ToastData) -> (Hsla, Hsla, Icon) {
    let by_level = match t.level {
        Level::Good => (th.ok, th.ok_soft, Icon::CircleCheck),
        Level::Warn => (th.warn, th.warn_soft, Icon::TriangleAlert),
        Level::Bad => (th.bad, th.bad_soft, Icon::CircleX),
        Level::Accent => (th.acc_hi, th.acc_soft, Icon::Info),
        _ => (th.tx2, th.ink4, Icon::Info),
    };
    match t.kind {
        ToastKind::Achievement => (th.warn, th.warn_soft, Icon::Trophy),
        ToastKind::Options => by_level,
        ToastKind::CloudSaved => (th.ok, th.ok_soft, Icon::CloudUpload),
        ToastKind::ModHotkey => (th.acc_hi, th.acc_soft, Icon::SlidersHorizontal),
        ToastKind::Notice if t.level == Level::Bad || t.level == Level::Warn => (by_level.0, by_level.1, Icon::Bot),
        ToastKind::Notice => (th.who_agent, th.who_agent.opacity(0.16), Icon::Bot),
        ToastKind::Download => (th.ok, th.ok_soft, Icon::Download),
        ToastKind::Error => (th.bad, th.bad_soft, Icon::CircleX),
        ToastKind::Hint => (th.acc_hi, th.acc_soft, Icon::Gamepad),
        ToastKind::Screenshot => (th.acc_hi, th.acc_soft, Icon::Image),
    }
}

/// The body, with the key as a keycap where the text says ([`KEY_SLOT`]).
fn body(th: &Theme, text: &str, key: Option<&str>) -> Div {
    let line = |s: &str| div().child(s.to_owned());
    let base = div().text_size(px(13.)).line_height(px(18.)).text_color(th.tx2);
    let Some(_) = key else { return base.w_full().line_clamp(3).child(text.to_owned()) };
    let d = base.flex().flex_wrap().items_center().gap(px(4.));
    match key {
        Some(k) if text.contains(KEY_SLOT) => {
            let (a, b) = text.split_once(KEY_SLOT).unwrap_or((text, ""));
            d.when(!a.trim().is_empty(), |d| d.child(line(a.trim_end())))
                .child(kbd(th, k.to_owned()).text_size(px(12.)).text_color(th.tx1))
                .when(!b.trim().is_empty(), |d| d.child(line(b.trim_start())))
        }
        Some(k) => d.child(line(text)).child(kbd(th, k.to_owned())),
        None => d,
    }
}

/// One toast (the desktop surface and the in-game overlay both draw these).
pub fn toast(th: &Theme, t: &ToastData) -> Div {
    let (fg, soft, glyph) = look(th, t);
    let badge = div().flex_none().size(px(40.)).rounded(px(10.)).bg(soft).flex().items_center().justify_center().overflow_hidden();
    let badge = match &t.image {
        Some(p) => badge.child(img(p.clone()).size(px(40.))),
        None => badge.child(icon(glyph, 22., fg)),
    };
    let border = match t.kind {
        ToastKind::Achievement => th.warn.opacity(0.45),
        ToastKind::Error => th.bad.opacity(0.45),
        _ => th.line2,
    };
    let buttons = (!t.buttons.is_empty()).then(|| {
        div().flex().gap(px(8.)).mt(px(8.)).children(t.buttons.iter().enumerate().map(|(i, b)| {
            let primary = b.primary || (i == 0 && !t.buttons.iter().any(|x| x.primary));
            let d = div().h(px(30.)).px(px(12.)).rounded(px(8.)).flex().items_center().text_size(px(13.)).font_weight(SEMIBOLD).child(b.label.clone());
            if primary { d.bg(th.acc).text_color(th.acc_ink) } else { d.border_1().border_color(th.line2).text_color(th.tx1) }
        }))
    });
    div()
        .w_full()
        .flex()
        .gap(px(14.))
        .items_start()
        .px(px(16.))
        .py(px(14.))
        .rounded(px(14.))
        .bg(th.ink1.opacity(0.92))
        .border_1()
        .border_color(border)
        .shadow(vec![soft_shadow(th.scrim, 12., 32.)])
        .font_family(UI_FONT)
        .child(badge)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .when(!t.caption.is_empty(), |d| d.child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(fg).child(t.caption.to_uppercase())))
                .child(div().text_size(px(14.)).font_weight(SEMIBOLD).text_color(th.tx1).child(t.title.clone()))
                .when_some(t.body.clone(), |d, b| d.child(body(th, &b, t.key.as_deref())))
                .when_some(buttons, |d, b| d.child(b))
                .when(!t.buttons.is_empty(), |d| d.child(div().text_size(px(11.)).text_color(th.tx3).child("Hold Shift+F2 (Guide+Start) to answer"))),
        )
}

impl Render for ToastView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        let measure = canvas(
            |bounds, window, cx| {
                // The surface follows the stack's height.
                let want = gpui::size(px(WIDTH + 2.0 * PAD), px(f32::from(bounds.size.height).ceil().max(1.0)));
                let have = window.viewport_size();
                if (f32::from(have.height) - f32::from(want.height)).abs() > 0.5 {
                    window.defer(cx, move |w, _| w.resize(want));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        div().w(px(WIDTH + 2.0 * PAD)).flex().flex_col().items_start().child(
            div()
                .relative()
                .w_full()
                .p(px(PAD))
                .flex()
                .flex_col()
                .gap(px(10.))
                .children(self.toasts.iter().map(|(_, t)| toast(&th, t)))
                .child(measure),
        )
    }
}

/// The lane: what's shown, and the desktop surface showing it when the game doesn't.
#[derive(Default)]
pub struct ToastLane {
    shown: Vec<Shown>,
    next_id: u64,
    window: Option<(WindowHandle<ToastView>, ToastCorner, Option<String>)>,
    pub corner: ToastCorner,
    /// The game's monitor, when a HUD model said which.
    pub monitor: Option<String>,
    /// Layer-shell is there (the desktop presenter can show).
    pub enabled: bool,
    /// The game shows the lane itself (`crate::overlay`): no desktop surface.
    pub in_game: bool,
}

impl ToastLane {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, ..Default::default() }
    }

    /// What's up now, newest first.
    pub fn list(&self) -> Vec<(u64, ToastData)> {
        self.shown.iter().map(|s| (s.id, s.data.clone())).collect()
    }

    /// Notices with buttons that are up now (the quick menu offers their buttons).
    pub fn answerable(&self) -> Vec<ToastData> {
        self.shown.iter().filter(|s| s.data.notice.is_some() && !s.data.buttons.is_empty()).map(|s| s.data.clone()).collect()
    }

    /// The session the newest toast is about.
    pub fn session(&self) -> Option<String> {
        self.shown.iter().find_map(|s| s.data.session.clone())
    }

    /// Adds a toast (a notice with the same id as one up replaces it). Returns false when
    /// nothing can show it (no layer-shell and the game isn't showing the lane).
    pub fn push(&mut self, t: ToastData, cx: &mut App) -> bool {
        if !self.enabled && !self.in_game {
            tracing::info!(title = %t.title, "toast (no layer-shell, and not in the game: not shown)");
            return false;
        }
        tracing::info!(title = %t.title, caption = %t.caption, kind = ?t.kind, in_game = self.in_game, "toast");
        if let Some(id) = &t.notice {
            self.shown.retain(|s| s.data.notice.as_ref() != Some(id));
        }
        self.next_id += 1;
        let until = Instant::now() + t.duration;
        self.shown.insert(0, Shown { id: self.next_id, data: t, until });
        self.shown.truncate(MAX_SHOWN);
        self.sync(cx);
        true
    }

    /// Drops a notice's toast (it was answered).
    pub fn dismiss_notice(&mut self, id: &str, cx: &mut App) {
        let n = self.shown.len();
        self.shown.retain(|s| s.data.notice.as_deref() != Some(id));
        if self.shown.len() != n {
            self.sync(cx);
        }
    }

    /// Drops expired toasts (called a few times a second). Returns whether the list changed.
    pub fn tick(&mut self, cx: &mut App) -> bool {
        let now = Instant::now();
        let n = self.shown.len();
        self.shown.retain(|s| s.until > now);
        let changed = self.shown.len() != n;
        if changed {
            self.sync(cx);
        }
        changed
    }

    /// Switches between the game showing the lane and the desktop surface.
    pub fn set_in_game(&mut self, on: bool, cx: &mut App) {
        if self.in_game != on {
            self.in_game = on;
            self.sync(cx);
        }
    }

    fn sync(&mut self, cx: &mut App) {
        if self.shown.is_empty() || self.in_game || !self.enabled {
            self.close(cx);
            return;
        }
        let toasts = self.list();
        if let Some((h, corner, monitor)) = &self.window
            && *corner == self.corner
            && *monitor == self.monitor
        {
            let t = toasts.clone();
            if h.update(cx, |v, _, cx| {
                v.toasts = t;
                cx.notify();
            })
            .is_ok()
            {
                return;
            }
        }
        self.close(cx);
        if let Err(e) = self.open(toasts, cx) {
            tracing::warn!("toast surface: {e:#}");
            self.shown.clear();
        }
    }

    fn open(&mut self, toasts: Vec<(u64, ToastData)>, cx: &mut App) -> Result<()> {
        use gpui::layer_shell::Anchor as L;
        let (edges, margin) = match self.corner {
            ToastCorner::TopRight => (L::TOP | L::RIGHT, (MARGIN, MARGIN, 0.0, 0.0)),
            ToastCorner::BottomRight => (L::BOTTOM | L::RIGHT, (0.0, MARGIN, MARGIN, 0.0)),
        };
        let h = open_layer(cx, NAMESPACE, edges, margin, (WIDTH + 2.0 * PAD, 2.0 * PAD + 90.0 * toasts.len() as f32), self.monitor.as_deref(), ToastView { toasts })?;
        self.window = Some((h, self.corner, self.monitor.clone()));
        Ok(())
    }

    pub fn close(&mut self, cx: &mut App) {
        if let Some((h, _, _)) = self.window.take() {
            let _ = h.update(cx, |_, w, _| w.remove_window());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::options::LiveChannel;
    use quiver_api::sight::NoticeSpec;
    use serde_json::json;

    fn op(id: &str, outcome: Outcome) -> OpOutcome {
        OpOutcome { surface: "m1/aco-dlss".into(), id: id.into(), outcome, before: None, after: Some(json!("on")) }
    }

    fn ev(outcomes: Vec<OpOutcome>, changes: Vec<&str>) -> OptionsEvent {
        OptionsEvent::Outcome { session: "s-1".into(), product_id: 1, profile: "dlss".into(), by: "sight".into(), outcomes, changes: changes.into_iter().map(String::from).collect() }
    }

    #[test]
    fn options_outcomes_become_toasts() {
        let t = from_options(&ev(vec![op("fg.enabled", Outcome::AppliedLive { channel: LiveChannel::Sdk, latency: None })], vec!["DLSS fg.enabled off → on"])).unwrap();
        assert_eq!((t.title.as_str(), t.level), ("Applied live", Level::Good));
        assert_eq!(t.body.as_deref(), Some("DLSS fg.enabled off → on"));
        assert_eq!(t.session.as_deref(), Some("s-1"));
        let t = from_options(&ev(vec![op("fg.mode", Outcome::SavedAt { at: "swapchain creation".into() })], vec![])).unwrap();
        assert_eq!(t.title, "Saved · applies at swapchain creation");
        assert_eq!(t.body.as_deref(), Some("fg.mode → on"));
        // The worst outcome wins, with a count; a rejection is urgent.
        let t = from_options(&ev(
            vec![op("a", Outcome::AppliedLive { channel: LiveChannel::File, latency: None }), op("b", Outcome::Rejected { reason: "out of range".into() })],
            vec!["DLSS a 1 → 2"],
        ))
        .unwrap();
        assert_eq!((t.title.as_str(), t.level, t.urgent), ("Rejected (and 1 more)", Level::Bad, true));
        assert_eq!(t.body.as_deref(), Some("DLSS a 1 → 2\nout of range"));
        // Nothing to say.
        assert!(from_options(&ev(vec![op("a", Outcome::Unchanged)], vec![])).is_none());
    }

    #[test]
    fn achievements_become_toasts() {
        let e: Event = serde_json::from_value(json!({
            "type": "achievement_unlocked", "product_id": 3539, "account": "a", "achievement_id": 12,
            "name": "Mysteries of Egypt", "description": "Find all the tombs.", "image": "/nonexistent.png",
            "unlocked_at": "2026-11-17T12:00:00Z", "report": "pending"
        }))
        .unwrap();
        let t = from_event(&e).unwrap();
        assert_eq!((t.title.as_str(), t.body.as_deref(), t.kind), ("Mysteries of Egypt", Some("Find all the tombs."), ToastKind::Achievement));
        assert_eq!(t.image, None, "a missing icon file falls back to the trophy");
        assert_eq!(t.duration, Duration::from_secs(6));
        assert!(from_event(&Event::LibraryChanged).is_none());
        // An Epic unlock (polled from Epic's store): named, keyed by the store's id.
        let e: Event = serde_json::from_value(json!({
            "type": "achievement_unlocked", "product_id": 0, "account": "8645b494", "achievement_id": 0,
            "name": null, "unlocked_at": "2026-11-18T19:40:00Z", "report": "by_game", "game": "epic:Wren", "key": "DEEP_DIVE"
        }))
        .unwrap();
        assert_eq!(from_event(&e).unwrap().title, "DEEP_DIVE");
    }

    fn notice(spec: NoticeSpec) -> SightNotice {
        SightNotice { id: "n-4".into(), from: "quiverd".into(), at: chrono::Utc::now(), spec }
    }

    #[test]
    fn every_notice_kind() {
        // Saved to the cloud.
        let t = from_notice(&notice(NoticeSpec { kind: NoticeKind::CloudSaved, level: NoticeLevel::Success, title: "Saved to the cloud".into(), body: Some("Ubisoft Connect · slot 3".into()), session: Some("s-1".into()), ..Default::default() }));
        assert_eq!((t.kind, t.level, t.session.as_deref(), t.urgent), (ToastKind::CloudSaved, Level::Good, Some("s-1"), false));
        // A mod's hotkey: the key as a keycap in the body.
        let t = from_notice(&notice(NoticeSpec { kind: NoticeKind::ModHotkey, caption: Some("DLSS".into()), title: "Frame Generation off".into(), body: Some("You pressed {key} · this session only".into()), key: Some("F9".into()), ..Default::default() }));
        assert_eq!((t.kind, t.caption.as_str(), t.key.as_deref()), (ToastKind::ModHotkey, "DLSS", Some("F9")));
        // An agent's, with buttons: longer, answerable.
        let t = from_notice(&notice(NoticeSpec {
            title: "DLSS v4 is ready".into(),
            caption: Some("Agent · aco-dlss".into()),
            buttons: vec![NoticeButton { id: "keep_v3".into(), label: "Keep v3 for now".into(), primary: true }, NoticeButton { id: "notes".into(), label: "Notes".into(), primary: false }],
            ..Default::default()
        }));
        assert_eq!((t.kind, t.notice.as_deref(), t.buttons.len(), t.duration), (ToastKind::Notice, Some("n-4"), 2, Duration::from_secs(12)));
        // An error is urgent.
        let t = from_notice(&notice(NoticeSpec { kind: NoticeKind::Error, title: "The mod crashed".into(), ..Default::default() }));
        assert!(t.urgent && t.kind == ToastKind::Error);
    }

    #[test]
    fn downloads_finished_or_failed() {
        let job = |done: bool, error: Option<&str>, kind: JobKind| Job {
            id: 1,
            kind,
            game: quiver_api::GameRef::ubisoft(5408),
            name: "Rabbids".into(),
            status: String::new(),
            downloaded: 0,
            total: 0,
            done,
            error: error.map(String::from),
            detail: Vec::new(),
        };
        let t = from_job(&job(true, None, JobKind::Install)).unwrap();
        assert_eq!((t.kind, t.title.as_str(), t.body.as_deref()), (ToastKind::Download, "Download finished", Some("Rabbids is ready to play")));
        let t = from_job(&job(true, Some("disk full\nmore"), JobKind::Install)).unwrap();
        assert!(t.urgent && t.body.as_deref() == Some("disk full"));
        assert!(from_job(&job(false, None, JobKind::Install)).is_none(), "still going");
        assert!(from_job(&job(true, None, JobKind::Play)).is_none(), "not a download");
    }

    #[test]
    fn do_not_disturb_lets_only_errors_through() {
        let calm = ToastData { title: "Saved to the cloud".into(), ..Default::default() };
        let err = ToastData { title: "Rejected".into(), urgent: true, ..Default::default() };
        assert!(allowed(&calm, false) && allowed(&err, false));
        assert!(!allowed(&calm, true) && allowed(&err, true));
        assert!(!allowed(&from_event(&serde_json::from_value(json!({"type": "achievement_unlocked", "product_id": 1, "account": "a", "achievement_id": 1, "name": "x", "unlocked_at": "2026-11-17T12:00:00Z", "report": "pending"})).unwrap()).unwrap(), true));
    }
}
