//! Performance and the pinned HUD (design/SIGHT-HUD.md): the state the game page's
//! Performance tab and Settings › Performance HUD share, their daemon calls, perf events
//! (`Reply::Perf`), the HUD settings form, the HUD's status and keys, and the live strip.
//!
//! HUD settings resolve profile > game > global (decision 12). The form edits one scope at
//! a time and says, per setting, which scope the value comes from; a setting stored at the
//! scope being edited can be reset to fall back to the next one out.

use crate::model::Gid;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures::StreamExt;
use gpui::{AnyElement, Context, Hsla, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::perf::{
    Anchor, Captures, HudConfig, HudKeybinds, HudLayoutInfo, HudScope, HudScopeRef, HudSettingsView, HudStatus, Level, PerfEvent, PerfRequest,
    PerfSessionInfo, PerfUpdate, PerfValue, Presenter, PresenterSetting, SessionSummary, Stat,
};

use crate::app::Quiver;
use crate::err_text;
use crate::nav::{Page, Tab};
use crate::theme::Theme;
use crate::ui::game::Load;
use crate::ui::{FocusExt, Icon, MEDIUM, SEMIBOLD, card, icon, mono};

// ---------------------------------------------------------------------------------------
// State

/// Which scope the game page's HUD form edits.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ScopePick {
    #[default]
    Game,
    /// A profile id.
    Profile(String),
    Global,
}

impl ScopePick {
    pub fn scope(&self, product: Gid) -> HudScopeRef {
        match self {
            ScopePick::Game => HudScopeRef::of(product.0),
            ScopePick::Profile(p) => HudScopeRef { game: Some(product.0), profile: Some(p.clone()) },
            ScopePick::Global => HudScopeRef::global(),
        }
    }
}

/// A live subscription for the open game's Performance tab.
pub struct Live {
    pub product: Gid,
    /// The subscription id, once quiverd answered.
    pub sub: Option<u64>,
    pub last: Option<PerfUpdate>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct PerfUi {
    /// The game whose summaries are loaded (0: none).
    pub summaries_for: Gid,
    pub summaries: Load<Vec<SessionSummary>>,
    /// The session shown (an index into `summaries`).
    pub sel: usize,
    /// Sessions compared with it; `None`: picked automatically.
    pub compare: Option<Vec<usize>>,
    /// Running games' perf sessions.
    pub sessions: Load<Vec<PerfSessionInfo>>,
    pub hud: BTreeMap<HudScopeRef, Load<HudSettingsView>>,
    /// The game page's HUD scope, per game.
    pub scope: HashMap<Gid, ScopePick>,
    pub layouts: Load<Vec<HudLayoutInfo>>,
    pub keybinds: Load<HudKeybinds>,
    pub status: HashMap<Gid, Load<HudStatus>>,
    pub live: Option<Live>,
    /// The perf event stream is set up.
    pub events: bool,
    /// A HUD change is on its way.
    pub saving: bool,
}

/// The HUD settings the form edits, in focus order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudField {
    Enabled,
    On,
    Layout,
    Anchor,
    Background,
    Scale,
    Rate,
    Captures,
    Presenter,
}

pub const HUD_FIELDS: [HudField; 9] = [
    HudField::Enabled,
    HudField::On,
    HudField::Layout,
    HudField::Anchor,
    HudField::Background,
    HudField::Scale,
    HudField::Rate,
    HudField::Captures,
    HudField::Presenter,
];

/// Grid order of the position control (the middle cell has no anchor).
const ANCHOR_GRID: [Option<Anchor>; 9] = [
    Some(Anchor::TopLeft),
    Some(Anchor::Top),
    Some(Anchor::TopRight),
    Some(Anchor::Left),
    None,
    Some(Anchor::Right),
    Some(Anchor::BottomLeft),
    Some(Anchor::Bottom),
    Some(Anchor::BottomRight),
];

const RATES: [u32; 4] = [1, 2, 4, 10];

impl HudField {
    /// The `HudSet` key (and `quiver hud set` key).
    pub fn key(self) -> &'static str {
        match self {
            HudField::Enabled => "enabled",
            HudField::On => "on",
            HudField::Layout => "layout",
            HudField::Anchor => "anchor",
            HudField::Background => "background",
            HudField::Scale => "scale",
            HudField::Rate => "rate_hz",
            HudField::Captures => "captures",
            HudField::Presenter => "presenter",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HudField::Enabled => "Quiver HUD",
            HudField::On => "Shown at launch",
            HudField::Layout => "Preset",
            HudField::Anchor => "Position",
            HudField::Background => "Opacity",
            HudField::Scale => "Scale",
            HudField::Rate => "Numbers update",
            HudField::Captures => "In captures",
            HudField::Presenter => "Drawn",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            HudField::Enabled => "Sets launches up for the HUD: the Sight layer and a session dir. Off: launches don't change.",
            HudField::On => "Shift+F3 shows or hides it while playing; the game remembers the last state.",
            HudField::Layout => "Ctrl+Shift+F3 steps through them in game. Rows a mod doesn't provide are left out.",
            HudField::Anchor => "Toasts own the top right.",
            HudField::Background => "The panel behind the numbers.",
            HudField::Scale => "Relative to the game's height (text is 1.6% of it at 100%).",
            HudField::Rate => "The frame-time graph follows every frame either way.",
            HudField::Captures => "Hidden keeps the HUD out of OBS game capture and screen shares.",
            HudField::Presenter => "",
        }
    }

    /// Stored at this scope.
    pub fn is_set(self, s: &HudScope) -> bool {
        match self {
            HudField::Enabled => s.enabled.is_some(),
            HudField::On => s.on.is_some(),
            HudField::Layout => s.layout.is_some(),
            HudField::Anchor => s.anchor.is_some(),
            HudField::Background => s.background.is_some(),
            HudField::Scale => s.scale.is_some(),
            HudField::Rate => s.rate_hz.is_some(),
            HudField::Captures => s.captures.is_some(),
            HudField::Presenter => s.presenter.is_some(),
        }
    }

    /// The setting's next value from `c` (a controller's A steps through them).
    pub fn next(self, c: &HudConfig, layouts: &[String]) -> HudScope {
        let mut s = HudScope::default();
        match self {
            HudField::Enabled => s.enabled = Some(!c.enabled),
            HudField::On => s.on = Some(!c.on),
            HudField::Layout => {
                let i = layouts.iter().position(|l| *l == c.layout).map(|i| i + 1).unwrap_or(0);
                s.layout = layouts.get(i % layouts.len().max(1)).cloned();
            }
            HudField::Anchor => {
                let order: Vec<Anchor> = ANCHOR_GRID.iter().flatten().copied().collect();
                let i = order.iter().position(|a| *a == c.anchor).unwrap_or(0);
                s.anchor = Some(order[(i + 1) % order.len()]);
            }
            HudField::Background => s.background = Some(step_wrap(c.background, 0.1, 0.0, 1.0)),
            HudField::Scale => s.scale = Some(step_wrap(c.scale, 0.25, 0.75, 2.5)),
            HudField::Rate => {
                let i = RATES.iter().position(|r| *r == c.rate_hz).unwrap_or(1);
                s.rate_hz = Some(RATES[(i + 1) % RATES.len()]);
            }
            HudField::Captures => s.captures = Some(if c.captures == Captures::Shown { Captures::Hidden } else { Captures::Shown }),
            HudField::Presenter => {
                s.presenter = Some(match c.presenter {
                    PresenterSetting::Auto => PresenterSetting::Game,
                    PresenterSetting::Game => PresenterSetting::Desktop,
                    PresenterSetting::Desktop => PresenterSetting::Auto,
                })
            }
        }
        s
    }
}

/// `v + step`, back to `lo` past `hi` (rounded to the step, so 0.62 + 0.1 is 0.7).
pub fn step_wrap(v: f32, step: f32, lo: f32, hi: f32) -> f32 {
    let n = ((v - lo) / step + 1e-3).floor() + 1.;
    let x = lo + n * step;
    if x > hi + 1e-3 { lo } else { x.min(hi) }
}

pub fn anchor_label(a: Anchor) -> &'static str {
    match a {
        Anchor::TopLeft => "Top left",
        Anchor::Top => "Top",
        Anchor::TopRight => "Top right",
        Anchor::Left => "Left",
        Anchor::Right => "Right",
        Anchor::BottomLeft => "Bottom left",
        Anchor::Bottom => "Bottom",
        Anchor::BottomRight => "Bottom right",
    }
}

pub fn presenter_label(p: PresenterSetting) -> &'static str {
    match p {
        PresenterSetting::Auto => "Automatic",
        PresenterSetting::Game => "In the game",
        PresenterSetting::Desktop => "Over the game",
    }
}

/// "SHIFT" + "F3" → "Shift+F3"; "CTRL SHIFT" → "Ctrl+Shift+F3".
pub fn keys_label(mods: &str, key: &str) -> String {
    let mut parts: Vec<String> = mods
        .split_whitespace()
        .map(|m| match m.to_ascii_uppercase().as_str() {
            "CTRL" | "CONTROL" => "Ctrl".to_owned(),
            "SHIFT" => "Shift".to_owned(),
            "ALT" => "Alt".to_owned(),
            "SUPER" | "WIN" | "MOD4" => "Super".to_owned(),
            other => {
                let mut c = other.chars();
                c.next().map(|f| f.to_string() + &c.as_str().to_ascii_lowercase()).unwrap_or_default()
            }
        })
        .collect();
    parts.push(key.to_owned());
    parts.join("+")
}

/// The scopes a setting at `scope` falls back through, innermost first (itself included).
pub fn scope_chain(scope: &HudScopeRef) -> Vec<HudScopeRef> {
    let mut v = vec![scope.clone()];
    if scope.profile.is_some() {
        v.push(HudScopeRef { game: scope.game, profile: None });
    }
    if scope.game.is_some() {
        v.push(HudScopeRef::global());
    }
    v
}

/// Where a scope's value of `f` comes from, as shown next to it.
pub fn origin_label(f: HudField, chain: &[(HudScopeRef, Option<&HudScope>)]) -> (bool, &'static str) {
    for (i, (s, set)) in chain.iter().enumerate() {
        if set.is_some_and(|x| f.is_set(x)) {
            let label = match (i, s.game, &s.profile) {
                (0, None, _) => "Set for all games",
                (0, Some(_), None) => "Set for this game",
                (0, Some(_), Some(_)) => "Set for this profile",
                (_, None, _) => "From all games",
                (_, Some(_), None) => "From the game",
                (_, Some(_), Some(_)) => "From the profile",
            };
            return (i == 0, label);
        }
    }
    (false, "Default")
}

/// A level's color (§1.2: the HUD's levels are theme tokens, never raw colors).
pub fn level_color(th: &Theme, l: Level) -> Hsla {
    match l {
        Level::Normal => th.tx1,
        Level::Dim => th.tx3,
        Level::Good => th.ok,
        Level::Warn => th.warn,
        Level::Bad => th.bad,
        Level::Accent => th.acc_hi,
    }
}

/// The first value of `id` in an update, whatever the stat.
pub fn live_value<'a>(u: &'a PerfUpdate, id: &str) -> Option<&'a PerfValue> {
    let m = u.values.get(id)?;
    m.get(&Stat::Mean).or_else(|| m.get(&Stat::Last)).or_else(|| m.values().next())
}

/// A value with something in it (quiverd sends `—` for a metric nobody provides).
pub fn has_value(v: &PerfValue) -> bool {
    v.value.is_some() || (!v.text.is_empty() && v.text != "—")
}

/// A frame-generation mod reports real frames: `fps.base` is its rate, not presented FPS
/// (which quiverd falls back to), and FG ratio has a value.
pub fn live_has_base(u: &PerfUpdate) -> bool {
    live_value(u, "fg.ratio").is_some_and(|v| v.value.is_some())
}

/// A mod-provided metric with a value.
pub fn live_has(u: &PerfUpdate, id: &str) -> bool {
    match id {
        "fps.base" => live_has_base(u),
        _ => live_value(u, id).is_some_and(has_value),
    }
}

// ---------------------------------------------------------------------------------------
// Loading, events

impl Quiver {
    /// Start receiving perf events (once, after connecting).
    pub fn start_perf_events(&mut self, cx: &mut Context<Self>) {
        if self.perf.events {
            return;
        }
        let Some(source) = self.source.clone() else { return };
        self.perf.events = true;
        let rx = crate::on_tokio(async move {
            let mut events = source.perf_events().await?;
            let (tx, rx) = futures::channel::mpsc::unbounded();
            tokio::spawn(async move {
                while let Some(e) = events.recv().await {
                    if tx.unbounded_send(e).is_err() {
                        break;
                    }
                }
            });
            anyhow::Ok(rx)
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(mut events)) = rx.await else {
                let _ = this.update(cx, |this, _| this.perf.events = false);
                return;
            };
            while let Some(e) = events.next().await {
                if this.update(cx, |this, cx| this.perf_event(e, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn perf_event(&mut self, e: PerfEvent, cx: &mut Context<Self>) {
        match e {
            PerfEvent::Update(u) => {
                let shown = self.perf_tab_open() == Some(Gid::from(u.product_id));
                match &mut self.perf.live {
                    Some(l) if shown && l.product == u.product_id && l.sub.is_none_or(|s| s == u.sub) => {
                        l.sub = Some(u.sub);
                        l.last = Some(u);
                    }
                    // Nobody looks at it any more: stop it.
                    _ => {
                        let sub = u.sub;
                        self.call::<serde_json::Value, _>(Request::Perf(PerfRequest::Unsubscribe { sub }), cx, |_, _, _| {});
                        if self.perf.live.as_ref().is_some_and(|l| l.sub == Some(sub)) {
                            self.perf.live = None;
                        }
                    }
                }
            }
            PerfEvent::Session { product_id, ended, .. } => {
                self.load_perf_sessions(cx);
                self.perf.status.remove(&Gid::from(product_id));
                if ended {
                    if self.perf.live.as_ref().is_some_and(|l| l.product == product_id) {
                        self.perf.live = None;
                    }
                    // The summary lands in the play history a moment later.
                    if self.perf.summaries_for == product_id {
                        let id = product_id;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(Duration::from_millis(800)).await;
                            let _ = this.update(cx, |this, cx| this.load_summaries(Gid::from(id), cx));
                        })
                        .detach();
                    }
                }
            }
            PerfEvent::HudChanged { product_id, .. } => {
                // Reload what's on screen; the rest reloads when shown.
                let keep: Vec<HudScopeRef> = self.perf.hud.keys().filter(|s| product_id.is_none() || s.game.is_none() || s.product_id() == product_id).cloned().collect();
                for s in keep {
                    self.load_hud(s, cx);
                }
                if let Some(p) = product_id {
                    self.perf.status.remove(&Gid::from(p));
                }
            }
            PerfEvent::CatalogChanged { .. } | PerfEvent::HudModel(_) => {}
        }
        cx.notify();
    }

    /// Daemon events the perf screens care about (`app.rs` calls this first).
    pub fn perf_on_event(&mut self, e: &quiver_api::Event, cx: &mut Context<Self>) {
        use quiver_api::Event;
        match e {
            // Subscriptions don't survive a restart.
            Event::Reconnected => {
                self.perf.live = None;
                self.perf.sessions = Load::Idle;
                self.perf.status.clear();
                self.perf.hud.clear();
                if !self.perf.events {
                    self.start_perf_events(cx);
                }
            }
            Event::GameStarted { product_id } | Event::GameStopped { product_id } => {
                self.perf.status.remove(&Gid::from(*product_id));
                self.load_perf_sessions(cx);
            }
            _ => {}
        }
    }

    /// The game whose Performance tab is on screen.
    pub fn perf_tab_open(&self) -> Option<Gid> {
        match self.nav.page {
            Page::Game(id) if self.nav.tab == Tab::Performance => Some(id),
            _ => None,
        }
    }

    pub fn load_summaries(&mut self, id: Gid, cx: &mut Context<Self>) {
        if self.perf.summaries_for != id {
            self.perf.summaries_for = id;
            self.perf.summaries = Load::Loading;
            self.perf.sel = 0;
            self.perf.compare = None;
        }
        self.call::<Vec<SessionSummary>, _>(Request::Perf(PerfRequest::Summaries { game: id.0.into(), limit: 30 }), cx, move |this, r, _| {
            if this.perf.summaries_for != id {
                return;
            }
            this.perf.summaries = match r {
                Ok(v) => {
                    this.perf.sel = this.perf.sel.min(v.len().saturating_sub(1));
                    Load::Ready(v)
                }
                Err(e) => Load::Failed(err_text(&e)),
            };
        });
    }

    pub fn load_perf_sessions(&mut self, cx: &mut Context<Self>) {
        if self.perf.sessions.ready().is_none() {
            self.perf.sessions = Load::Loading;
        }
        self.call::<Vec<PerfSessionInfo>, _>(Request::Perf(PerfRequest::Sessions), cx, |this, r, _| {
            this.perf.sessions = match r {
                Ok(v) => Load::Ready(v),
                // An older quiverd without perf: no sessions.
                Err(e) => Load::Failed(err_text(&e)),
            };
        });
    }

    /// The running perf session of a game.
    pub fn perf_session(&self, id: Gid) -> Option<&PerfSessionInfo> {
        self.perf.sessions.ready()?.iter().find(|s| s.game_ref() == id.0)
    }

    pub fn load_hud(&mut self, scope: HudScopeRef, cx: &mut Context<Self>) {
        self.perf.hud.entry(scope.clone()).or_insert(Load::Loading);
        let key = scope.clone();
        self.call::<HudSettingsView, _>(Request::Perf(PerfRequest::HudGet { scope }), cx, move |this, r, _| {
            let v = match r {
                Ok(v) => Load::Ready(v),
                Err(e) => Load::Failed(err_text(&e)),
            };
            this.perf.hud.insert(key, v);
        });
    }

    /// Load what the HUD form at `scope` shows, if not loaded yet.
    pub fn ensure_hud(&mut self, scope: &HudScopeRef, cx: &mut Context<Self>) {
        for s in scope_chain(scope) {
            if !self.perf.hud.contains_key(&s) {
                self.load_hud(s, cx);
            }
        }
        if self.perf.layouts.is_idle() {
            self.perf.layouts = Load::Loading;
            self.call::<Vec<HudLayoutInfo>, _>(Request::Perf(PerfRequest::HudLayouts), cx, |this, r, _| {
                this.perf.layouts = match r {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            });
        }
        if self.perf.keybinds.is_idle() {
            self.perf.keybinds = Load::Loading;
            self.call::<HudKeybinds, _>(Request::Perf(PerfRequest::HudKeybinds), cx, |this, r, _| {
                this.perf.keybinds = match r {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            });
        }
    }

    pub fn load_hud_status(&mut self, id: Gid, cx: &mut Context<Self>) {
        self.perf.status.insert(id, Load::Loading);
        self.call::<HudStatus, _>(Request::Perf(PerfRequest::HudStatus { session: None, game: Some(id.0) }), cx, move |this, r, _| {
            this.perf.status.insert(
                id,
                match r {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(err_text(&e)),
                },
            );
        });
    }

    /// Keep a live subscription for the game's tab while its perf session runs.
    pub fn ensure_live(&mut self, id: Gid, cx: &mut Context<Self>) {
        if self.perf.live.as_ref().is_some_and(|l| l.product == id) {
            return;
        }
        if let Some(sub) = self.perf.live.take().and_then(|l| l.sub) {
            self.call::<serde_json::Value, _>(Request::Perf(PerfRequest::Unsubscribe { sub }), cx, |_, _, _| {});
        }
        self.perf.live = Some(Live { product: id, sub: None, last: None, error: None });
        let req = PerfRequest::Subscribe { session: None, game: Some(id.0), select: vec![], every_ms: 1000, window_ms: 1000, stats: vec![] };
        self.call::<u64, _>(Request::Perf(req), cx, move |this, r, cx| match (r, &mut this.perf.live) {
            (Ok(sub), Some(l)) if l.product == id => l.sub = Some(sub),
            (Ok(sub), _) => this.call::<serde_json::Value, _>(Request::Perf(PerfRequest::Unsubscribe { sub }), cx, |_, _, _| {}),
            (Err(e), Some(l)) if l.product == id => l.error = Some(err_text(&e)),
            (Err(_), _) => {}
        });
    }

    /// The resolved settings the form shows at `scope`.
    pub fn hud_effective(&self, scope: &HudScopeRef) -> Option<&HudConfig> {
        self.perf.hud.get(scope).and_then(|l| l.ready()).map(|v| &v.effective)
    }

    /// Store `set` at `scope` (and drop `unset`).
    pub fn hud_apply(&mut self, scope: HudScopeRef, set: HudScope, unset: Vec<String>, cx: &mut Context<Self>) {
        self.perf.saving = true;
        let key = scope.clone();
        let req = PerfRequest::HudSet { scope, set, unset, message: None };
        self.call::<HudSettingsView, _>(Request::Perf(req), cx, move |this, r, cx| {
            this.perf.saving = false;
            match r {
                Ok(v) => {
                    this.perf.hud.insert(key.clone(), Load::Ready(v));
                    // Inner scopes resolve through this one.
                    let inner: Vec<HudScopeRef> = this.perf.hud.keys().filter(|s| **s != key && scope_chain(s).contains(&key)).cloned().collect();
                    for s in inner {
                        this.load_hud(s, cx);
                    }
                    if let Some(g) = key.game {
                        this.perf.status.remove(&Gid(g));
                    } else {
                        this.perf.status.clear();
                    }
                }
                Err(e) => this.show_toast(format!("Couldn't change the HUD: {}", err_text(&e)), cx),
            }
        });
    }

    /// A controller's A on a HUD setting: its next value.
    pub fn hud_cycle(&mut self, scope: HudScopeRef, f: HudField, cx: &mut Context<Self>) {
        let Some(c) = self.hud_effective(&scope).cloned() else { return };
        let layouts = self.hud_layout_ids();
        let set = f.next(&c, &layouts);
        self.hud_apply(scope, set, vec![], cx);
    }

    fn hud_layout_ids(&self) -> Vec<String> {
        match self.perf.layouts.ready() {
            Some(l) => l.iter().filter(|l| l.error.is_none()).map(|l| l.id.clone()).collect(),
            None => ["fps", "compact", "standard", "full"].map(str::to_owned).to_vec(),
        }
    }

    // -----------------------------------------------------------------------------------
    // Rendering

    /// The HUD form at `scope`: one row per setting. `focused` says which row has
    /// keyboard/controller focus; `head` goes above the rows (a scope picker).
    pub fn render_hud_form(
        &mut self,
        th: &Theme,
        scope: &HudScopeRef,
        head: Option<AnyElement>,
        focused: &dyn Fn(HudField) -> bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chain_scopes = scope_chain(scope);
        let views: Vec<(HudScopeRef, Option<HudScope>)> =
            chain_scopes.iter().map(|s| (s.clone(), self.perf.hud.get(s).and_then(|l| l.ready()).map(|v| v.set.clone()))).collect();
        let chain: Vec<(HudScopeRef, Option<&HudScope>)> = views.iter().map(|(s, v)| (s.clone(), v.as_ref())).collect();
        let body: Vec<AnyElement> = match self.perf.hud.get(scope) {
            Some(Load::Ready(v)) => {
                let c = v.effective.clone();
                HUD_FIELDS.iter().map(|&f| self.hud_row(th, scope, f, &c, &chain, focused(f), cx)).collect()
            }
            Some(Load::Failed(e)) => vec![hud_note(Icon::TriangleAlert, th.warn, format!("Couldn't load the HUD settings: {e}")).into_any_element()],
            _ => vec![div().py(px(12.)).text_size(px(12.5)).text_color(th.tx3).child("Loading the HUD settings…").into_any_element()],
        };
        div().flex().flex_col().children(head).children(body).into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn hud_row(&mut self, th: &Theme, scope: &HudScopeRef, f: HudField, c: &HudConfig, chain: &[(HudScopeRef, Option<&HudScope>)], focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let (here, origin) = origin_label(f, chain);
        let control = self.hud_control(th, scope, f, c, cx);
        let reset = here.then(|| {
            let scope = scope.clone();
            div()
                .id(SharedString::from(format!("hud-reset-{}", f.key())))
                .text_size(px(11.5))
                .text_color(th.acc_hi)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child("Reset")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.hud_apply(scope.clone(), HudScope::default(), vec![f.key().to_owned()], cx);
                    cx.notify();
                }))
        });
        let help = f.help();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(16.))
            .gap_y(px(8.))
            .py(px(10.))
            .px(px(6.))
            .mx(px(-6.))
            .rounded(px(8.))
            .border_t_1()
            .border_color(th.line1)
            .focus_ring(focused, th, 8.)
            .child(
                div()
                    .w(px(190.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .font_weight(MEDIUM)
                            .text_size(px(12.5))
                            .when(here, |d| d.child(div().size(px(6.)).rounded_full().bg(th.acc)))
                            .child(f.label()),
                    )
                    .when(!help.is_empty(), |d| d.child(div().text_size(px(11.5)).line_height(px(15.)).text_color(th.tx3).child(help))),
            )
            .child(div().flex_1().min_w(px(240.)).flex().child(control))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .w(px(170.))
                    .justify_end()
                    .child(div().text_size(px(11.5)).text_color(if here { th.tx2 } else { th.tx3 }).child(origin))
                    .children(reset),
            )
            .into_any_element()
    }

    fn hud_control(&mut self, th: &Theme, scope: &HudScopeRef, f: HudField, c: &HudConfig, cx: &mut Context<Self>) -> AnyElement {
        let set = |s: HudScope| (scope.clone(), s);
        match f {
            HudField::Enabled | HudField::On => {
                let on = if f == HudField::Enabled { c.enabled } else { c.on };
                let (sc, s) = set(f.next(c, &[]));
                let label = match (f, on) {
                    (HudField::Enabled, true) => "Used for launches",
                    (HudField::Enabled, false) => "Off: launches unchanged",
                    (_, true) => "Shown",
                    (_, false) => "Hidden until Shift+F3",
                };
                div()
                    .id(SharedString::from(format!("hud-{}", f.key())))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .child(crate::ui::game::toggle(th, on))
                    .child(div().text_size(px(12.5)).text_color(if on { th.tx1 } else { th.tx3 }).child(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.hud_apply(sc.clone(), s.clone(), vec![], cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            HudField::Layout => {
                let layouts: Vec<HudLayoutInfo> = self.perf.layouts.ready().cloned().unwrap_or_default();
                let known = layouts.iter().any(|l| l.id == c.layout);
                let mut chips: Vec<AnyElement> = layouts
                    .iter()
                    .map(|l| {
                        let on = l.id == c.layout;
                        let (sc, s) = set(HudScope { layout: Some(l.id.clone()), ..Default::default() });
                        let ok = l.error.is_none();
                        div()
                            .id(SharedString::from(format!("hud-layout-{}", l.id)))
                            .child(
                                crate::ui::chip(th, None, l.name.clone(), on)
                                    .when(!l.builtin, |d| d.child(div().text_size(px(10.5)).text_color(th.tx3).child("yours")))
                                    .when(!ok, |d| d.opacity(0.5)),
                            )
                            .when(ok, |d| d.cursor_pointer())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if ok {
                                    this.hud_apply(sc.clone(), s.clone(), vec![], cx);
                                    cx.notify();
                                }
                            }))
                            .into_any_element()
                    })
                    .collect();
                if !known {
                    chips.push(crate::ui::chip(th, Some(Icon::TriangleAlert), format!("{} (missing)", c.layout), true).into_any_element());
                }
                div().flex().flex_wrap().gap(px(6.)).children(chips).into_any_element()
            }
            HudField::Anchor => {
                let cells: Vec<AnyElement> = ANCHOR_GRID
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let on = *a == Some(c.anchor);
                        let cell = div().w(px(20.)).h(px(13.)).rounded(px(3.)).bg(if on { th.acc } else if a.is_some() { th.ink5 } else { th.ink3 });
                        match a {
                            Some(a) => {
                                let (sc, s) = set(HudScope { anchor: Some(*a), ..Default::default() });
                                let hover = th.acc_line;
                                div()
                                    .id(("hud-anchor", i))
                                    .cursor_pointer()
                                    .child(cell.hover(move |s| s.bg(hover)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.hud_apply(sc.clone(), s.clone(), vec![], cx);
                                        cx.notify();
                                    }))
                                    .into_any_element()
                            }
                            None => cell.into_any_element(),
                        }
                    })
                    .collect();
                let mut rows = Vec::new();
                let mut it = cells.into_iter();
                for _ in 0..3 {
                    rows.push(div().flex().gap(px(3.)).children(it.by_ref().take(3)).into_any_element());
                }
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(div().flex().flex_col().gap(px(3.)).p(px(4.)).rounded(px(6.)).bg(th.ink1).border_1().border_color(th.line2).children(rows))
                    .child(div().text_size(px(12.5)).child(anchor_label(c.anchor)))
                    .child(mono(11., th.tx3).child(format!("{:.0}, {:.0} px", c.offset[0], c.offset[1])))
                    .into_any_element()
            }
            HudField::Background => {
                let v = c.background;
                let minus = set(HudScope { background: Some(((v - 0.05) * 20.).round() / 20.), ..Default::default() });
                let plus = set(HudScope { background: Some(((v + 0.05) * 20.).round() / 20.), ..Default::default() });
                let swatch = div().w(px(34.)).h(px(18.)).rounded(px(4.)).border_1().border_color(th.line2).bg(th.ink0.opacity(v.clamp(0., 1.)));
                self.stepper(th, "hud-bg", format!("{:.0}%", v * 100.), v > 0.001, v < 0.999, minus, plus, Some(swatch.into_any_element()), cx)
            }
            HudField::Scale => {
                let v = c.scale;
                let minus = set(HudScope { scale: Some(((v - 0.25) * 4.).round() / 4.), ..Default::default() });
                let plus = set(HudScope { scale: Some(((v + 0.25) * 4.).round() / 4.), ..Default::default() });
                self.stepper(th, "hud-scale", format!("{:.0}%", v * 100.), v > 0.751, v < 2.499, minus, plus, None, cx)
            }
            HudField::Rate => {
                let items: Vec<(String, bool, (HudScopeRef, HudScope))> =
                    RATES.iter().map(|r| (format!("{r}/s"), c.rate_hz == *r, set(HudScope { rate_hz: Some(*r), ..Default::default() }))).collect();
                self.seg(th, "hud-rate", items, cx)
            }
            HudField::Captures => {
                let items = vec![
                    ("Shown".to_owned(), c.captures == Captures::Shown, set(HudScope { captures: Some(Captures::Shown), ..Default::default() })),
                    ("Hidden".to_owned(), c.captures == Captures::Hidden, set(HudScope { captures: Some(Captures::Hidden), ..Default::default() })),
                ];
                self.seg(th, "hud-captures", items, cx)
            }
            HudField::Presenter => {
                let opts = [
                    (PresenterSetting::Auto, "Automatic", "In the game when the Sight layer draws; over the game otherwise (OpenGL, anti-cheat)"),
                    (PresenterSetting::Game, "In the game (Sight layer)", "Keeps VRR pacing, direct scan-out and tearing"),
                    (PresenterSetting::Desktop, "Over the game (compositor)", "For OpenGL and anti-cheat games; turns off direct scan-out"),
                ];
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(9.))
                    .bg(th.ink1)
                    .border_1()
                    .border_color(th.line2)
                    .children(opts.into_iter().map(|(p, title, sub)| {
                        let on = c.presenter == p;
                        let (sc, s) = set(HudScope { presenter: Some(p), ..Default::default() });
                        let hover = th.ink3;
                        div()
                            .id(SharedString::from(format!("hud-pres-{title}")))
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .px(px(10.))
                            .py(px(7.))
                            .rounded(px(7.))
                            .cursor_pointer()
                            .when(on, |d| d.bg(th.acc_soft).border_1().border_color(th.acc_line))
                            .when(!on, |d| d.hover(move |s| s.bg(hover)))
                            .child(div().text_size(px(12.5)).font_weight(SEMIBOLD).text_color(if on { th.acc_hi } else { th.tx1 }).child(title))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(sub))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.hud_apply(sc.clone(), s.clone(), vec![], cx);
                                cx.notify();
                            }))
                    }))
                    .into_any_element()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn stepper(
        &mut self,
        th: &Theme,
        id: &'static str,
        value: String,
        can_minus: bool,
        can_plus: bool,
        minus: (HudScopeRef, HudScope),
        plus: (HudScopeRef, HudScope),
        extra: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let btn = |key: &'static str, i: Icon, enabled: bool, (sc, s): (HudScopeRef, HudScope), cx: &mut Context<Self>| {
            let hover = th.ink4;
            div()
                .id(SharedString::from(format!("{id}-{key}")))
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded(px(6.))
                .border_1()
                .border_color(th.line2)
                .bg(th.ink2)
                .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
                .when(!enabled, |d| d.opacity(0.4))
                .child(icon(i, 13., th.tx2))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if enabled {
                        this.hud_apply(sc.clone(), s.clone(), vec![], cx);
                        cx.notify();
                    }
                }))
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(btn("minus", Icon::Minus, can_minus, minus, cx))
            .child(mono(12.5, th.tx1).w(px(46.)).text_center().child(value))
            .child(btn("plus", Icon::Plus, can_plus, plus, cx))
            .children(extra)
            .into_any_element()
    }

    fn seg(&mut self, th: &Theme, id: &'static str, items: Vec<(String, bool, (HudScopeRef, HudScope))>, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_none()
            .p(px(3.))
            .gap(px(3.))
            .rounded(px(9.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line2)
            .children(items.into_iter().enumerate().map(|(i, (label, on, (sc, s)))| {
                let hover = th.ink3;
                div()
                    .id((id, i))
                    .px(px(11.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .text_size(px(12.))
                    .font_weight(SEMIBOLD)
                    .cursor_pointer()
                    .text_color(if on { th.acc_ink } else { th.tx2 })
                    .when(on, |d| d.bg(th.acc))
                    .when(!on, |d| d.hover(move |s| s.bg(hover)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.hud_apply(sc.clone(), s.clone(), vec![], cx);
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }

    /// Settings › Performance HUD: the defaults every game starts from.
    pub fn settings_hud(&mut self, th: &Theme, focused: &[HudField], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let scope = HudScopeRef::global();
        self.ensure_hud(&scope, cx);
        let is_focused = |f: HudField| focused.contains(&f);
        let form = self.render_hud_form(th, &scope, None, &is_focused, cx);
        let c = self.hud_effective(&scope).cloned();
        let summary = c.as_ref().map(|c| {
            format!(
                "{} · {} · {} · {}",
                if c.enabled { "On for every game" } else { "Off unless a game turns it on" },
                self.perf.layouts.ready().and_then(|l| l.iter().find(|x| x.id == c.layout)).map(|l| l.name.clone()).unwrap_or_else(|| c.layout.clone()),
                anchor_label(c.anchor),
                presenter_label(c.presenter),
            )
        });
        let keys = self.render_hud_keys(th);
        let title = |t: &str, sub: &str| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x(px(8.))
                .gap_y(px(2.))
                .pt(px(10.))
                .pb(px(8.))
                .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child(t.to_owned()))
                .when(!sub.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(th.tx3).child(sub.to_owned())))
        };
        vec![
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_size(px(20.)).font_weight(gpui::FontWeight::BOLD).child("Performance HUD"))
                .child(div().text_color(th.tx3).child(
                    "Quiver's own HUD, pinned on the game (instead of MangoHud). These are the defaults; a game's page, and its profiles, can change any of them.",
                ))
                .into_any_element(),
            card(th)
                .px(px(16.))
                .pt(px(4.))
                .pb(px(6.))
                .child(title("All games", summary.as_deref().unwrap_or("")))
                .child(form)
                .into_any_element(),
            card(th).px(px(16.)).pt(px(4.)).pb(px(12.)).child(title("Keys", "While a Quiver game has focus")).child(keys).into_any_element(),
            div()
                .flex()
                .child(crate::ui::command_line(th, "quiver hud set enabled=true layout=compact captures=shown"))
                .into_any_element(),
        ]
    }

    /// The HUD's keys (`HudKeybinds`), as information: quiverd doesn't bind them yet.
    pub fn render_hud_keys(&self, th: &Theme) -> AnyElement {
        let Some(k) = self.perf.keybinds.ready() else {
            return div().text_size(px(12.)).text_color(th.tx3).child("Loading the HUD's keys…").into_any_element();
        };
        let rows = k.binds.iter().map(|b| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .py(px(3.))
                .child(crate::ui::kbd(th, keys_label(&b.mods, &b.key)))
                .child(div().text_size(px(12.5)).child(b.description.clone()))
        });
        let state = match (k.applied, k.game_focused) {
            (true, true) => "Bound now: a Quiver game has focus.",
            (true, false) => "Bound while a Quiver game has focus.",
            (false, _) => "quiverd doesn't bind these yet: add the Hyprland lines below to use them (they then work over every window).",
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(rows)
            .child(div().pt(px(4.)).text_size(px(11.5)).text_color(th.tx3).child(state))
            .when(!k.applied && !k.conf.is_empty(), |d| {
                d.child(
                    div()
                        .mt(px(4.))
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(th.ink0)
                        .border_1()
                        .border_color(th.line1)
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .children(k.conf.lines().map(|l| mono(11., th.tx2).child(l.to_owned()))),
                )
            })
            .into_any_element()
    }

    /// Who draws the HUD of a running game and why (`HudStatus`), or what the setting means
    /// before launch.
    pub fn render_hud_status(&self, th: &Theme, id: Gid, c: Option<&HudConfig>) -> AnyElement {
        let running = self.perf_session(id).is_some();
        let line = |ic: Icon, color: Hsla, title: String, sub: String| {
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .child(div().pt(px(2.)).child(icon(ic, 14., color)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_size(px(12.5)).font_weight(SEMIBOLD).child(title))
                        .when(!sub.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(sub))),
                )
                .into_any_element()
        };
        if !running {
            let Some(c) = c else { return div().into_any_element() };
            if !c.enabled {
                return line(Icon::CircleDot, th.tx3, "Not set up for launches".into(), "Turn the Quiver HUD on to set up the game's next launch.".into());
            }
            let what = match c.presenter {
                PresenterSetting::Auto => "In the game when the Sight layer draws; else over the game",
                PresenterSetting::Game => "In the game (Sight layer)",
                PresenterSetting::Desktop => "Over the game (compositor)",
            };
            return line(Icon::Monitor, th.tx2, format!("Next launch: {what}"), "quiverd picks the presenter when the game starts; its reason shows here while it runs.".into());
        }
        match self.perf.status.get(&id) {
            Some(Load::Ready(s)) => {
                let (ic, color, who) = match s.presenter {
                    Some(Presenter::Game) => (Icon::CircleCheck, th.ok, "Drawn in the game"),
                    Some(Presenter::Desktop) => (Icon::Monitor, th.warn, "Drawn over the game"),
                    None => (Icon::CircleDot, th.tx3, "Not drawn"),
                };
                let mut sub = vec![s.presenter_reason.clone()];
                sub.push(if s.on { "shown".into() } else { "hidden (Shift+F3)".into() });
                if s.renderer_attached {
                    sub.push("renderer attached".into());
                }
                if let Some(l) = &s.layer {
                    sub.push(format!("layer: {} of {} presents drawn", l.drawn, l.presents));
                }
                let sub = sub.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(line(ic, color, format!("{who} · {}", s.layout), sub))
                    .when_some(s.last_error.clone(), |d, e| d.child(line(Icon::TriangleAlert, th.warn, "Last error".into(), e)))
                    .into_any_element()
            }
            Some(Load::Failed(e)) => line(Icon::TriangleAlert, th.warn, "No HUD status".into(), e.clone()),
            _ => line(Icon::LoaderCircle, th.tx3, "Asking quiverd…".into(), String::new()),
        }
    }

    /// The running game's numbers, once a second.
    pub fn render_live_strip(&self, th: &Theme, id: Gid) -> Option<AnyElement> {
        let info = self.perf_session(id)?;
        let live = self.perf.live.as_ref().filter(|l| l.product == id);
        // FPS cells are labelled FPS already; other units stay with their numbers.
        let shown = |v: &PerfValue| if v.unit == "fps" { v.text.clone() } else { v.display() };
        let cell = |label: &str, v: Option<&PerfValue>, big: bool| {
            let (text, color) = match v {
                Some(v) if !v.stale => (shown(v), level_color(th, v.level)),
                Some(v) => (shown(v), th.tx3),
                None => ("—".to_owned(), th.tx4),
            };
            div()
                .flex()
                .items_baseline()
                .gap(px(6.))
                .child(mono(if big { 20. } else { 14. }, color).font_weight(SEMIBOLD).child(text))
                .child(div().text_size(px(10.)).font_weight(SEMIBOLD).text_color(th.tx3).child(label.to_owned()))
                .into_any_element()
        };
        let u = live.and_then(|l| l.last.as_ref());
        let get = |id: &str| u.and_then(|u| live_value(u, id));
        let has = |id: &str| u.is_some_and(|u| live_has(u, id));
        let mut cells = vec![cell("FPS", get("fps"), true)];
        if has("fps.base") {
            cells.push(cell("BASE", get("fps.base"), true));
        }
        cells.push(cell("1% LOW", get("fps.p1low"), false));
        cells.push(cell("FRAME TIME", get("frametime"), false));
        if has("latency.render") {
            cells.push(cell("LATENCY", get("latency.render"), false));
        }
        cells.push(cell("GPU", get("gpu.load"), false));
        cells.push(cell("", get("gpu.temp"), false));
        cells.push(cell("", get("gpu.power"), false));
        cells.push(cell("CPU", get("cpu.load"), false));
        cells.push(cell("", get("cpu.temp"), false));
        let upscaler = get("upscaler").filter(|v| has_value(v)).map(|v| v.text.clone());
        let status = match live {
            Some(Live { error: Some(e), .. }) => e.clone(),
            Some(Live { last: Some(u), .. }) => format!("updated {}", u.at.with_timezone(&chrono::Local).format("%H:%M:%S")),
            _ => "waiting for numbers…".into(),
        };
        let producers = info.producers.iter().filter(|p| p.connected).map(|p| p.label.clone()).collect::<Vec<_>>().join(" · ");
        Some(
            card(th)
                .px(px(16.))
                .py(px(12.))
                .gap(px(8.))
                .border_color(th.acc_line)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(div().size(px(8.)).rounded_full().bg(th.ok))
                        .child(div().text_size(px(12.)).font_weight(SEMIBOLD).child("Live"))
                        .when_some(upscaler, |d, u| d.child(crate::ui::chip(th, None, u, false)))
                        .child(div().text_size(px(11.5)).text_color(th.tx3).child(producers))
                        .child(div().flex_1())
                        .child(div().text_size(px(11.5)).text_color(th.tx3).child(status)),
                )
                .child(div().flex().flex_wrap().items_baseline().gap_x(px(22.)).gap_y(px(6.)).children(cells))
                .into_any_element(),
        )
    }
}

fn hud_note(ic: Icon, color: Hsla, text: impl Into<SharedString>) -> gpui::Div {
    div().flex().items_center().gap(px(8.)).py(px(12.)).text_size(px(12.5)).text_color(color).child(icon(ic, 14., color)).child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_wraps_and_rounds() {
        assert!((step_wrap(0.62, 0.1, 0.0, 1.0) - 0.7).abs() < 1e-4);
        assert_eq!(step_wrap(1.0, 0.1, 0.0, 1.0), 0.0);
        assert_eq!(step_wrap(2.5, 0.25, 0.75, 2.5), 0.75);
        assert!((step_wrap(1.0, 0.25, 0.75, 2.5) - 1.25).abs() < 1e-4);
    }

    #[test]
    fn cycling_each_setting() {
        let c = HudConfig::default();
        let l = vec!["fps".to_owned(), "compact".to_owned(), "standard".to_owned()];
        assert_eq!(HudField::Layout.next(&c, &l).layout.as_deref(), Some("standard"));
        assert_eq!(HudField::Anchor.next(&c, &l).anchor, Some(Anchor::Top));
        assert_eq!(HudField::Rate.next(&c, &l).rate_hz, Some(4));
        assert_eq!(HudField::Presenter.next(&c, &l).presenter, Some(PresenterSetting::Game));
        assert_eq!(HudField::Enabled.next(&c, &l).enabled, Some(true));
        let last = HudConfig { anchor: Anchor::BottomRight, layout: "standard".into(), ..Default::default() };
        assert_eq!(HudField::Anchor.next(&last, &l).anchor, Some(Anchor::TopLeft));
        assert_eq!(HudField::Layout.next(&last, &l).layout.as_deref(), Some("fps"));
        // Every field's next value sets exactly that field.
        for f in HUD_FIELDS {
            assert!(f.is_set(&f.next(&c, &l)), "{f:?}");
        }
    }

    #[test]
    fn live_values_without_a_mod() {
        // From a scratch quiverd with the fake layer and no mod: quiverd fills fps.base
        // with presented FPS and sends `—` for what nobody provides.
        let line = r#"{"sub":2,"session":"s-732n13vjd7","product_id":3539,"at":"2026-11-17T08:14:45.767736691Z","values":{"cpu.load":{"last":{"value":66.18,"text":"66","unit":"%"}},"fg.ratio":{"last":{"text":"—","level":"dim"}},"fps":{"mean":{"value":143.99,"text":"144"}},"fps.base":{"last":{"value":143.99,"text":"144"}},"latency.render":{"last":{"text":"—","level":"dim"}},"upscaler":{"last":{"text":"—","level":"dim"}}}}"#;
        let u: PerfUpdate = serde_json::from_str(line).unwrap();
        assert!(!live_has(&u, "fps.base"), "base is presented FPS without an FG mod");
        assert!(!live_has(&u, "latency.render") && !live_has(&u, "upscaler"));
        assert!(live_has(&u, "fps") && live_has(&u, "cpu.load"));
        assert_eq!(live_value(&u, "cpu.load").unwrap().display(), "66%");
    }

    #[test]
    fn origins_and_labels() {
        assert_eq!(keys_label("CTRL SHIFT", "F3"), "Ctrl+Shift+F3");
        assert_eq!(keys_label("SHIFT", "F3"), "Shift+F3");
        let prof = HudScopeRef { game: Some(quiver_api::GameRef::ubisoft(1)), profile: Some("p".into()) };
        let chain = scope_chain(&prof);
        assert_eq!(chain, [prof.clone(), HudScopeRef::game(1), HudScopeRef::global()]);
        let game = HudScope { layout: Some("full".into()), ..Default::default() };
        let global = HudScope { captures: Some(Captures::Hidden), ..Default::default() };
        let c: Vec<(HudScopeRef, Option<&HudScope>)> = vec![(prof, None), (HudScopeRef::game(1), Some(&game)), (HudScopeRef::global(), Some(&global))];
        assert_eq!(origin_label(HudField::Layout, &c), (false, "From the game"));
        assert_eq!(origin_label(HudField::Captures, &c), (false, "From all games"));
        assert_eq!(origin_label(HudField::Scale, &c), (false, "Default"));
        assert_eq!(origin_label(HudField::Layout, &c[1..]), (true, "Set for this game"));
    }
}
