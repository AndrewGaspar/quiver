//! The toast lane (SIGHT §9, §3.2): small layer-shell toasts, top-right of the game's monitor
//! (bottom-right when the HUD owns the top-right corner), timed, click-through, never taking
//! focus. At most three, newest on top; achievements 6 s, everything else 4 s. One surface
//! (namespace `quiver-sight`) holds the stack and is unmapped when the lane is empty, so it
//! costs scan-out only while a toast is up.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{App, Context, Div, Hsla, IntoElement, ParentElement, Render, Styled, Window, WindowHandle, canvas, div, img, prelude::*, px};
use quiver_api::Event;
use quiver_api::options::{OpOutcome, OptionsEvent, Outcome};
use quiver_api::perf::Level;
use quiver_ui::assets::UI_FONT;
use quiver_ui::components::{Icon, SEMIBOLD, caption, icon, soft_shadow};
use quiver_ui::theme::Theme;

use crate::desktop::open_layer;
use crate::place::ToastCorner;

pub const NAMESPACE: &str = "quiver-sight";
pub const MAX_SHOWN: usize = 3;
const WIDTH: f32 = 360.0;
const MARGIN: f32 = 16.0;

#[derive(Debug, Clone, PartialEq)]
pub enum ToastKind {
    Achievement,
    Options,
}

/// One toast, as the lane shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToastData {
    pub kind: ToastKind,
    /// The small caption ("Achievement unlocked", "Mod setting").
    pub caption: String,
    pub title: String,
    pub body: Option<String>,
    /// An image on this machine (an achievement's icon).
    pub image: Option<PathBuf>,
    pub level: Level,
    pub duration: Duration,
}

/// `AchievementUnlocked` → a toast.
pub fn from_event(e: &Event) -> Option<ToastData> {
    let Event::AchievementUnlocked { achievement_id, name, description, image, .. } = e else { return None };
    Some(ToastData {
        kind: ToastKind::Achievement,
        caption: "Achievement unlocked".into(),
        title: name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("Achievement {achievement_id}")),
        body: description.clone().filter(|d| !d.trim().is_empty()),
        image: image.as_ref().map(PathBuf::from).filter(|p| p.is_file()),
        level: Level::Accent,
        duration: Duration::from_secs(6),
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
    let OptionsEvent::Outcome { outcomes, changes, .. } = e else { return None };
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
        image: None,
        level,
        duration: Duration::from_secs(if level == Level::Bad { 6 } else { 4 }),
    })
}

struct Shown {
    id: u64,
    data: ToastData,
    until: Instant,
}

pub struct ToastView {
    toasts: Vec<(u64, ToastData)>,
}

fn level_colors(th: &Theme, l: Level) -> (Hsla, Hsla, Icon) {
    match l {
        Level::Good => (th.ok, th.ok_soft, Icon::CircleCheck),
        Level::Warn => (th.warn, th.warn_soft, Icon::TriangleAlert),
        Level::Bad => (th.bad, th.bad_soft, Icon::CircleX),
        Level::Accent => (th.acc_hi, th.acc_soft, Icon::Clock),
        _ => (th.tx2, th.ink4, Icon::Info),
    }
}

fn toast(th: &Theme, t: &ToastData) -> Div {
    let (fg, soft, level_icon) = level_colors(th, t.level);
    let glyph = match t.kind {
        ToastKind::Achievement => Icon::Trophy,
        ToastKind::Options => level_icon,
    };
    let badge = div().flex_none().size(px(36.)).rounded(px(9.)).bg(soft).flex().items_center().justify_center().overflow_hidden();
    let badge = match &t.image {
        Some(p) => badge.child(img(p.clone()).size(px(36.))),
        None => badge.child(icon(glyph, 18., fg)),
    };
    div()
        .w_full()
        .flex()
        .gap(px(12.))
        .items_center()
        .p(px(12.))
        .rounded(px(12.))
        .bg(th.ink2.opacity(0.94))
        .border_1()
        .border_color(th.line2)
        .shadow(vec![soft_shadow(th.scrim, 6., 18.)])
        .font_family(UI_FONT)
        .child(badge)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(caption(&t.caption, th.tx3))
                .child(div().text_size(px(14.)).font_weight(SEMIBOLD).text_color(th.tx1).child(t.title.clone()))
                .when_some(t.body.clone(), |d, b| d.child(div().text_size(px(12.)).line_height(px(16.)).text_color(th.tx2).line_clamp(3).child(b))),
        )
}

impl Render for ToastView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        let measure = canvas(
            |bounds, window, cx| {
                // The surface follows the stack's height.
                let want = gpui::size(px(WIDTH), px(f32::from(bounds.size.height).ceil().max(1.0)));
                let have = window.viewport_size();
                if (f32::from(have.height) - f32::from(want.height)).abs() > 0.5 {
                    window.defer(cx, move |w, _| w.resize(want));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        div().w(px(WIDTH)).flex().flex_col().items_start().child(
            div()
                .relative()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(self.toasts.iter().map(|(_, t)| toast(&th, t)))
                .child(measure),
        )
    }
}

/// The lane: what's shown and the surface showing it.
#[derive(Default)]
pub struct ToastLane {
    shown: Vec<Shown>,
    next_id: u64,
    window: Option<(WindowHandle<ToastView>, ToastCorner, Option<String>)>,
    pub corner: ToastCorner,
    /// The game's monitor, when a HUD model said which.
    pub monitor: Option<String>,
    pub enabled: bool,
}

impl ToastLane {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, ..Default::default() }
    }

    pub fn push(&mut self, t: ToastData, cx: &mut App) {
        if !self.enabled {
            tracing::info!(title = %t.title, "toast (no layer-shell: not shown)");
            return;
        }
        tracing::info!(title = %t.title, caption = %t.caption, "toast");
        self.next_id += 1;
        let until = Instant::now() + t.duration;
        self.shown.insert(0, Shown { id: self.next_id, data: t, until });
        self.shown.truncate(MAX_SHOWN);
        self.sync(cx);
    }

    /// Drops expired toasts (called a few times a second).
    pub fn tick(&mut self, cx: &mut App) {
        let now = Instant::now();
        let n = self.shown.len();
        self.shown.retain(|s| s.until > now);
        if self.shown.len() != n {
            self.sync(cx);
        }
    }

    fn sync(&mut self, cx: &mut App) {
        if self.shown.is_empty() {
            self.close(cx);
            return;
        }
        let toasts: Vec<(u64, ToastData)> = self.shown.iter().map(|s| (s.id, s.data.clone())).collect();
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
        let h = open_layer(cx, NAMESPACE, edges, margin, (WIDTH, 72.0 * toasts.len() as f32), self.monitor.as_deref(), ToastView { toasts })?;
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
        let t = from_options(&ev(vec![op("fg.mode", Outcome::SavedAt { at: "swapchain creation".into() })], vec![])).unwrap();
        assert_eq!(t.title, "Saved · applies at swapchain creation");
        assert_eq!(t.body.as_deref(), Some("fg.mode → on"));
        // The worst outcome wins, with a count.
        let t = from_options(&ev(
            vec![op("a", Outcome::AppliedLive { channel: LiveChannel::File, latency: None }), op("b", Outcome::Rejected { reason: "out of range".into() })],
            vec!["DLSS a 1 → 2"],
        ))
        .unwrap();
        assert_eq!((t.title.as_str(), t.level), ("Rejected (and 1 more)", Level::Bad));
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
        assert_eq!((t.title.as_str(), t.body.as_deref(), t.kind.clone()), ("Mysteries of Egypt", Some("Find all the tombs."), ToastKind::Achievement));
        assert_eq!(t.image, None, "a missing icon file falls back to the trophy");
        assert_eq!(t.duration, Duration::from_secs(6));
        assert!(from_event(&Event::LibraryChanged).is_none());
    }
}
