//! The quick menu (design/SIGHT.md §16; the mock-up's `SightQuick.dc.html`): a strip of tiles
//! along the bottom of the game, over a gradient (no blur), held open while the chord is.
//!
//! **Opening.** Hold Shift+F2 (or Guide+Start, which hyprpad turns into Shift+F2 held:
//! `tools/hyprpad/quiver-sight.lua`). The Sight layer counts the press as before (quiverd
//! sends the panel toggle) and also says, in `panel.input`, whether the chord is still held
//! (`quiver_perf::panel::LayerChord`, read here every [`POLL`]). Released within [`HOLD`]: a
//! tap, the panel opens as it always did. Held past it: the quick menu shows. Left / Right (the
//! D-pad, the arrows) choose; **releasing applies the focused tile** and closes. Escape or B
//! closes without applying. `quiver sight quick` opens it latched (no hold): A or Enter applies.
//! A layer from before (no chord word) or a game that can't draw it: the tap's behaviour, the
//! panel.
//!
//! **Tiles** ([`tiles`]), left to right: the buttons of a notice that's up (an agent's "Keep v3
//! for now"), frame generation (a mod's live `fg.enabled`), the HUD preset, the frame-rate cap
//! (the `fps.cap` Tweak), pinned items (any option or Tweak pinned with "pin to quick menu",
//! per game and profile: `SightSettings::pins_for`), screenshot, save replay (only while a
//! gpu-screen-recorder replay buffer runs), quit game. Quit needs A (Enter) held for
//! [`QUIT_HOLD`]: releasing the chord on it latches the menu instead of quitting.
//!
//! **Drawing.** In the game, like the panel (`panel::ingame`): an off-screen GPUI window at the
//! swapchain's logical size, published into `panel.image` with input captured (the arrows
//! mustn't reach the game), in each eye on a stereo frame; the strip at the panel's rung and
//! the focused tile at the focus rung (design/STEREO-UI.md §12). No desktop fallback: where
//! the game can't draw it, the hold opens the panel.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{Bounds, Context, FocusHandle, IntoElement, ParentElement, Pixels, Render, Styled, Window, canvas, div, linear_color_stop, linear_gradient, px};
use quiver_api::options::{OptionState, OptionType, same_value};
use quiver_api::sight::{PinKind, QuickPin};
use quiver_perf::stereo::rung;
use quiver_ui::assets::UI_FONT;
use quiver_ui::components::{Icon, SEMIBOLD, glyph, icon, kbd};
use quiver_ui::theme::Theme;
use serde_json::Value;

use crate::panel::ingame::{DepthParts, InGameView};
use crate::panel::state::{PRESETS, PanelData, adjustable, shown_in_game, step_value};
use crate::toasts::ToastData;

/// A press held this long is a hold (the quick menu); released sooner, a tap (the panel).
pub const HOLD: Duration = Duration::from_millis(280);
/// A (Enter) held this long on "Quit game" quits.
pub const QUIT_HOLD: Duration = Duration::from_millis(1000);
/// No word from the layer on the chord for this long while waiting or held: give up.
pub const STALE: Duration = Duration::from_millis(2500);
/// How often the chord word is read while it matters.
pub const POLL: Duration = Duration::from_millis(25);

/// Frame-rate caps the tile steps through (`fps.cap`: 0 is "No cap", `vrr` the refresh rate
/// less three).
pub const CAPS: [&str; 7] = ["0", "30", "60", "90", "120", "144", "vrr"];

#[derive(Debug, Clone, PartialEq)]
pub enum TileKind {
    /// A notice's button (its id, the button's).
    Notice { id: String, button: String },
    /// A mod's frame generation switch: (surface key, option id).
    FrameGen { surface: String, id: String },
    HudPreset,
    FpsCap,
    Pin(QuickPin),
    Screenshot,
    /// gpu-screen-recorder's replay buffer: its pid.
    Replay { pid: u32 },
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub kind: TileKind,
    pub label: String,
    pub value: String,
    /// A switch's state (the value's colour).
    pub on: Option<bool>,
    pub icon: Icon,
}

fn tile(kind: TileKind, label: impl Into<String>, value: impl Into<String>, on: Option<bool>, icon: Icon) -> Tile {
    Tile { kind, label: label.into(), value: value.into(), on, icon }
}

/// An option's value in a few words ("On", "Balanced", "2.0×").
pub fn value_words(st: &OptionState) -> String {
    match st.def.ty {
        OptionType::Bool => if st.value.as_bool().unwrap_or(false) { "On" } else { "Off" }.into(),
        _ => match st.def.special_of(&st.value) {
            Some(m) => m.label.clone(),
            None => quiver_ui::options::short_value(&st.def, &st.value),
        },
    }
}

/// `fps.cap`'s value in words.
pub fn cap_words(v: &Value) -> String {
    match v {
        Value::String(s) if s == "vrr" => "Refresh − 3".into(),
        v => match v.as_f64() {
            Some(n) if n <= 0.0 => "No cap".into(),
            Some(n) => format!("{} fps", n.round() as i64),
            None => v.to_string(),
        },
    }
}

/// The cap after `cur` in [`CAPS`] (a value between two goes to the next one up).
pub fn next_cap(cur: &Value) -> Value {
    let as_value = |s: &str| if s == "vrr" { Value::String("vrr".into()) } else { Value::from(s.parse::<i64>().unwrap_or(0)) };
    let i = CAPS.iter().position(|c| same_value(&as_value(c), cur)).map(|i| i + 1).unwrap_or_else(|| {
        let n = cur.as_f64().unwrap_or(0.0);
        CAPS.iter().position(|c| c.parse::<f64>().is_ok_and(|x| x > n)).unwrap_or(0)
    });
    as_value(CAPS[i % CAPS.len()])
}

fn find_option<'a>(d: &'a PanelData, surface: &str, id: &str) -> Option<&'a OptionState> {
    d.options.ready()?.iter().find(|s| s.key == surface)?.options.get(id)
}

/// The mod's live frame generation switch, if one exposes it: a `fg.enabled` toggle shown in
/// game, with the key the mod flips it with.
fn frame_gen(d: &PanelData) -> Option<Tile> {
    for s in d.options.ready()? {
        let Some(st) = s.options.get("fg.enabled").filter(|st| st.def.ty == OptionType::Bool && shown_in_game(st) && adjustable(st)) else { continue };
        let on = st.value.as_bool().unwrap_or(false);
        let key = s.actions.iter().filter(|(id, _)| id.starts_with("fg.")).find_map(|(_, a)| a.hotkey.clone());
        let value = match key {
            Some(k) => format!("{} · {k}", if on { "On" } else { "Off" }),
            None => (if on { "On" } else { "Off" }).to_owned(),
        };
        let label = st.def.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| "Frame Generation".into());
        return Some(tile(TileKind::FrameGen { surface: s.key.clone(), id: "fg.enabled".into() }, label, value, Some(on), Icon::Layers));
    }
    None
}

/// A pin's tile, when its option is there and adjustable here.
fn pin_tile(d: &PanelData, p: &QuickPin) -> Option<Tile> {
    let st = match p.kind {
        PinKind::Option => find_option(d, &p.surface, &p.id)?,
        PinKind::Tweak => d.tweaks.ready()?.options.get(&p.id)?,
    };
    if !adjustable(st) {
        return None;
    }
    let label = st.def.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| if p.label.is_empty() { p.id.clone() } else { p.label.clone() });
    let on = (st.def.ty == OptionType::Bool).then(|| st.value.as_bool().unwrap_or(false));
    Some(tile(TileKind::Pin(p.clone()), label, value_words(st), on, Icon::Pin))
}

/// The tiles for a session, left to right (module docs).
pub fn tiles(d: &PanelData, pins: &[QuickPin], notices: &[ToastData], replay: Option<u32>) -> Vec<Tile> {
    let mut out = Vec::new();
    for n in notices {
        let Some(id) = &n.notice else { continue };
        for b in &n.buttons {
            out.push(tile(TileKind::Notice { id: id.clone(), button: b.id.clone() }, b.label.clone(), n.title.clone(), None, Icon::Bot));
        }
    }
    if let Some(t) = frame_gen(d) {
        out.push(t);
    }
    if let Some(h) = d.hud.ready() {
        let live = h.live();
        let shown = h.status.as_ref().is_some_and(|s| s.on);
        let layout = h.status.as_ref().filter(|_| live).map(|s| s.layout.clone()).unwrap_or_else(|| h.settings.effective.layout.clone());
        let name = PRESETS.iter().find(|(id, _)| *id == layout).map(|(_, n)| (*n).to_owned()).unwrap_or(layout);
        let value = if live && !shown { format!("Hidden · {name}") } else { name };
        out.push(tile(TileKind::HudPreset, "HUD", value, None, Icon::Activity));
    }
    if let Some(st) = d.tweaks.ready().and_then(|t| t.options.get("fps.cap")) {
        out.push(tile(TileKind::FpsCap, "Frame rate cap", cap_words(&st.value), None, Icon::Clock));
    }
    for p in pins {
        if let Some(t) = pin_tile(d, p) {
            out.push(t);
        }
    }
    out.push(tile(TileKind::Screenshot, "Screenshot", "This screen", None, Icon::Image));
    if let Some(pid) = replay {
        out.push(tile(TileKind::Replay { pid }, "Save replay", "The replay buffer", None, Icon::History));
    }
    out.push(tile(TileKind::Quit, "Quit game", "Hold A", None, Icon::LogOut));
    out
}

/// What a tile does: the value to set (options and Tweaks), or the action.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// (surface key, option id, value)
    SetOption(String, String, Value),
    /// (tweak id, value)
    SetTweak(String, Value),
    /// The next HUD preset (shown).
    HudPreset(String),
    /// The HUD is hidden: show it.
    HudShow,
    Notice { id: String, button: String },
    Screenshot,
    Replay { pid: u32 },
    Quit,
}

/// What applying `t` does with the session's data.
pub fn act(t: &Tile, d: &PanelData) -> Option<Act> {
    Some(match &t.kind {
        TileKind::Notice { id, button } => Act::Notice { id: id.clone(), button: button.clone() },
        TileKind::FrameGen { surface, id } => {
            let st = find_option(d, surface, id)?;
            Act::SetOption(surface.clone(), id.clone(), Value::Bool(!st.value.as_bool().unwrap_or(false)))
        }
        TileKind::HudPreset => {
            let h = d.hud.ready()?;
            if h.live() && !h.status.as_ref().is_some_and(|s| s.on) {
                return Some(Act::HudShow);
            }
            let cur = h.status.as_ref().filter(|_| h.live()).map(|s| s.layout.clone()).unwrap_or_else(|| h.settings.effective.layout.clone());
            let ids: Vec<String> = PRESETS.iter().map(|p| p.0.to_owned()).collect();
            Act::HudPreset(crate::panel::state::cycle(&ids, &cur, 1))
        }
        TileKind::FpsCap => {
            let st = d.tweaks.ready()?.options.get("fps.cap")?;
            Act::SetTweak("fps.cap".into(), next_cap(&st.value))
        }
        TileKind::Pin(p) => match p.kind {
            PinKind::Option => {
                let st = find_option(d, &p.surface, &p.id)?;
                Act::SetOption(p.surface.clone(), p.id.clone(), step_value(st, &st.value, 0)?)
            }
            PinKind::Tweak => {
                let st = d.tweaks.ready()?.options.get(&p.id)?;
                Act::SetTweak(p.id.clone(), step_value(st, &st.value, 0)?)
            }
        },
        TileKind::Screenshot => Act::Screenshot,
        TileKind::Replay { pid } => Act::Replay { pid: *pid },
        TileKind::Quit => Act::Quit,
    })
}

// ---- The state machine ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Closed,
    /// The chord went down; a tap or a hold, not known yet (nothing shown).
    Pending,
    /// Shown while the chord is held: releasing applies.
    Held,
    /// Shown until a tile is applied or it's dismissed.
    Latched,
}

/// What the controller does after an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Redraw (the focus moved, it latched).
    Redraw,
    /// It was a tap: open the panel instead.
    OpenPanel,
    Show,
    /// Apply the tile at this index, and close.
    Apply(usize),
    Close,
    Quit,
}

/// The keys the menu reads (the game's arrows, Enter, Escape; hyprpad types the pad's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Enter,
    Back,
}

impl Key {
    /// From a Linux evdev key code (the input preload's events).
    pub fn from_evdev(code: u32) -> Option<Key> {
        Some(match code {
            105 | 104 => Key::Left, // Left, Page Up (hyprpad types LB as Page Up)
            106 | 109 => Key::Right,
            28 | 96 | 57 => Key::Enter,
            1 | 14 => Key::Back,
            _ => return None,
        })
    }
}

#[derive(Debug, Default)]
pub struct Quick {
    pub phase: Phase,
    pub session: Option<String>,
    /// When the chord went down.
    pub since: Option<Instant>,
    pub focus: usize,
    pub tiles: Vec<Tile>,
    /// Enter held since (for Quit).
    pub enter_since: Option<Instant>,
    /// When the layer last said anything about the chord.
    pub last_word: Option<Instant>,
    /// The kind focused when it last closed (focus comes back to it).
    remember: Option<TileKind>,
}

impl Quick {
    pub fn active(&self) -> bool {
        self.phase != Phase::Closed
    }

    pub fn shown(&self) -> bool {
        matches!(self.phase, Phase::Held | Phase::Latched)
    }

    /// The chord went down (Shift+F2 counted by the layer) at `at`.
    pub fn press(&mut self, session: &str, at: Instant) {
        self.phase = Phase::Pending;
        self.session = Some(session.to_owned());
        self.since = Some(at);
        self.last_word = Some(at);
        self.enter_since = None;
    }

    /// Opened with no hold (`quiver sight quick`).
    pub fn latch(&mut self, session: &str, now: Instant) -> Effect {
        self.press(session, now);
        self.phase = Phase::Latched;
        self.refocus();
        Effect::Show
    }

    fn close(&mut self) {
        self.remember = self.tiles.get(self.focus).map(|t| t.kind.clone());
        self.phase = Phase::Closed;
        self.since = None;
        self.enter_since = None;
    }

    fn refocus(&mut self) {
        let want = self.remember.as_ref().and_then(|k| self.tiles.iter().position(|t| same_kind(&t.kind, k)));
        self.focus = want.unwrap_or(0).min(self.tiles.len().saturating_sub(1));
    }

    /// New tiles (data came in): focus stays on the same kind of tile.
    pub fn set_tiles(&mut self, tiles: Vec<Tile>) {
        let cur = self.tiles.get(self.focus).map(|t| t.kind.clone());
        self.tiles = tiles;
        match cur.and_then(|k| self.tiles.iter().position(|t| same_kind(&t.kind, &k))) {
            Some(i) => self.focus = i,
            None => self.refocus(),
        }
    }

    pub fn focused(&self) -> Option<&Tile> {
        self.tiles.get(self.focus)
    }

    /// The layer's word on the chord (`None`: it said nothing, or nothing fresh).
    pub fn chord(&mut self, held: Option<bool>, now: Instant) -> Effect {
        if held.is_some() {
            self.last_word = Some(now);
        }
        let stale = self.last_word.is_none_or(|t| now.saturating_duration_since(t) > STALE);
        match self.phase {
            Phase::Pending => match held {
                Some(false) => {
                    self.close();
                    Effect::OpenPanel
                }
                Some(true) if self.since.is_some_and(|s| now.saturating_duration_since(s) >= HOLD) => {
                    self.phase = Phase::Held;
                    self.refocus();
                    Effect::Show
                }
                _ if stale => {
                    self.close();
                    Effect::OpenPanel
                }
                _ => Effect::None,
            },
            Phase::Held => match held {
                Some(false) => {
                    if matches!(self.focused().map(|t| &t.kind), Some(TileKind::Quit)) {
                        // Quitting needs A held: released on it, the menu stays.
                        self.phase = Phase::Latched;
                        return Effect::Redraw;
                    }
                    let i = self.focus;
                    self.close();
                    if self.tiles.is_empty() { Effect::Close } else { Effect::Apply(i) }
                }
                _ if stale => {
                    self.close();
                    Effect::Close
                }
                _ => Effect::None,
            },
            Phase::Latched | Phase::Closed => Effect::None,
        }
    }

    pub fn key(&mut self, k: Key, down: bool, now: Instant) -> Effect {
        if !self.shown() {
            return Effect::None;
        }
        match (k, down) {
            (Key::Left, true) if self.focus > 0 => {
                self.focus -= 1;
                self.enter_since = None;
                Effect::Redraw
            }
            (Key::Right, true) if self.focus + 1 < self.tiles.len() => {
                self.focus += 1;
                self.enter_since = None;
                Effect::Redraw
            }
            (Key::Enter, true) => {
                if matches!(self.focused().map(|t| &t.kind), Some(TileKind::Quit)) {
                    self.enter_since.get_or_insert(now);
                    return Effect::Redraw;
                }
                let i = self.focus;
                self.close();
                if self.tiles.is_empty() { Effect::Close } else { Effect::Apply(i) }
            }
            (Key::Enter, false) => {
                self.enter_since = None;
                Effect::Redraw
            }
            (Key::Back, true) => {
                self.close();
                Effect::Close
            }
            _ => Effect::None,
        }
    }

    /// Time passing: Enter held long enough on Quit quits.
    pub fn tick(&mut self, now: Instant) -> Effect {
        if self.shown()
            && matches!(self.focused().map(|t| &t.kind), Some(TileKind::Quit))
            && self.enter_since.is_some_and(|t| now.saturating_duration_since(t) >= QUIT_HOLD)
        {
            self.close();
            return Effect::Quit;
        }
        Effect::None
    }

    /// How far along holding A on Quit is (0..1), for the tile's bar.
    pub fn quit_progress(&self, now: Instant) -> Option<f32> {
        let t = self.enter_since?;
        Some((now.saturating_duration_since(t).as_secs_f32() / QUIT_HOLD.as_secs_f32()).clamp(0.0, 1.0))
    }
}

fn same_kind(a: &TileKind, b: &TileKind) -> bool {
    match (a, b) {
        (TileKind::Pin(x), TileKind::Pin(y)) => x.same_item(y),
        (TileKind::Notice { id: a, button: x }, TileKind::Notice { id: b, button: y }) => a == b && x == y,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

// ---- Save replay: gpu-screen-recorder's replay buffer --------------------------------------

/// A running `gpu-screen-recorder` with a replay buffer (`-r SECONDS`): its pid and seconds.
/// It saves the buffer on SIGUSR1.
pub fn replay_buffer() -> Option<(u32, u32)> {
    let dir = std::fs::read_dir("/proc").ok()?;
    let uid = unsafe { libc::getuid() };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(md) = std::fs::metadata(e.path()) else { continue };
        if std::os::unix::fs::MetadataExt::uid(&md) != uid {
            continue;
        }
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        if let Some(secs) = replay_args(&cmd) {
            return Some((pid, secs));
        }
    }
    None
}

/// `gpu-screen-recorder … -r 60 …` (NUL-separated) → 60.
pub fn replay_args(cmdline: &[u8]) -> Option<u32> {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let exe = args.first()?;
    if !exe.rsplit(|b| *b == b'/').next().is_some_and(|n| n == b"gpu-screen-recorder") {
        return None;
    }
    let i = args.iter().position(|a| *a == b"-r")?;
    std::str::from_utf8(args.get(i + 1)?).ok()?.parse().ok().filter(|s: &u32| *s > 0)
}

// ---- The view --------------------------------------------------------------------------------

pub struct QuickView {
    pub tiles: Vec<Tile>,
    pub focus: usize,
    pub latched: bool,
    /// Holding A on Quit (0..1).
    pub quit: Option<f32>,
    pub focus_handle: FocusHandle,
    parts: Rc<RefCell<Vec<(Bounds<Pixels>, u16)>>>,
}

impl QuickView {
    pub fn new(cx: &mut Context<Self>) -> QuickView {
        QuickView { tiles: Vec::new(), focus: 0, latched: false, quit: None, focus_handle: cx.focus_handle(), parts: Rc::default() }
    }

    pub fn from(q: &Quick, now: Instant, cx: &mut Context<Self>) -> QuickView {
        let mut v = QuickView::new(cx);
        v.set(q, now);
        v
    }

    pub fn set(&mut self, q: &Quick, now: Instant) {
        self.tiles = q.tiles.clone();
        self.focus = q.focus;
        self.latched = q.phase == Phase::Latched;
        self.quit = q.quit_progress(now);
    }
}

impl InGameView for QuickView {
    fn depth_parts(&self) -> DepthParts {
        let p = self.parts.clone();
        Rc::new(move || p.borrow().clone())
    }
    fn focus_handle(&self) -> Option<FocusHandle> {
        Some(self.focus_handle.clone())
    }
}

/// Records its parent's bounds (padded) as a part at `depth`.
fn part(parts: &Rc<RefCell<Vec<(Bounds<Pixels>, u16)>>>, depth: u16) -> impl IntoElement {
    let parts = parts.clone();
    canvas(
        move |bounds, _, _| {
            parts.borrow_mut().push((bounds.dilate(px(6.)), depth));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn hint(th: &Theme, parts: Vec<gpui::AnyElement>) -> gpui::Div {
    div().flex().gap(px(8.)).items_center().children(parts).text_color(th.tx2)
}

impl Render for QuickView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        self.parts.borrow_mut().clear();
        let shade = th.ink0;
        let tiles = self.tiles.iter().enumerate().map(|(i, t)| {
            let focused = i == self.focus;
            let value_color = match t.on {
                Some(true) => th.ok,
                _ => th.tx3,
            };
            let mut d = div()
                .relative()
                .w(px(148.))
                .h(px(128.))
                .p(px(14.))
                .rounded(px(16.))
                .border_1()
                .border_color(if focused { th.acc_hi } else { th.line2 })
                .bg(th.ink1.opacity(0.9))
                .flex()
                .flex_col()
                .justify_between()
                .child(icon(t.icon, 24., th.acc_hi))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_size(px(14.)).font_weight(SEMIBOLD).text_color(th.tx1).line_clamp(2).child(t.label.clone()))
                        .child(div().text_size(px(13.)).text_color(value_color).truncate().child(t.value.clone())),
                );
            if focused {
                d = d.mb(px(8.)).shadow(vec![gpui::BoxShadow { color: th.acc_hi.opacity(0.4), offset: gpui::point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(3.), inset: false }]);
                if let (TileKind::Quit, Some(p)) = (&t.kind, self.quit) {
                    d = d.child(div().absolute().left(px(14.)).right(px(14.)).bottom(px(8.)).h(px(3.)).rounded(px(2.)).bg(th.ink4).child(div().h_full().rounded(px(2.)).bg(th.bad).w(gpui::relative(p))));
                }
                d = d.child(part(&self.parts, rung::FOCUS));
            }
            d
        });
        let apply = if self.latched {
            hint(&th, vec![glyph(&th, "A", false).into_any_element(), div().child("Apply").into_any_element(), glyph(&th, "B", false).into_any_element(), div().child("Close").into_any_element()])
        } else {
            hint(&th, vec![div().child("Release").into_any_element(), glyph(&th, "Guide", false).into_any_element(), div().child("to apply").into_any_element()])
        };
        let bar = div()
            .flex()
            .gap(px(20.))
            .items_center()
            .px(px(16.))
            .py(px(10.))
            .rounded(px(12.))
            .bg(th.ink0.opacity(0.72))
            .text_size(px(13.))
            .child(hint(&th, vec![glyph(&th, "◀ ▶", false).into_any_element(), div().child("Choose").into_any_element()]))
            .child(apply)
            .child(hint(&th, vec![glyph(&th, "Y", false).into_any_element(), div().child("in any Sight page pins an item here").into_any_element()]))
            .child(div().flex().gap(px(6.)).items_center().text_color(th.tx3).child("Keyboard: hold").child(kbd(&th, "Shift")).child(kbd(&th, "F2")));
        div().size_full().relative().font_family(UI_FONT).child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(gpui::relative(0.47))
                .bg(linear_gradient(0., linear_color_stop(shade.opacity(0.85), 0.), linear_color_stop(shade.opacity(0.), 1.)))
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(64.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(18.))
                        .child(div().relative().flex().gap(px(14.)).items_end().children(tiles).child(part(&self.parts, rung::PANEL)))
                        .child(bar),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::state::{HudState, Load};
    use quiver_api::options::{ActionInfo, OptionDef, SurfaceState};
    use quiver_api::perf::{HudSettingsView, HudStatus};
    use quiver_api::sight::NoticeButton;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn opt(ty: OptionType, label: &str, value: Value) -> OptionState {
        OptionState { def: OptionDef { ty, label: Some(label.into()), ..Default::default() }, value, visible: true, ..Default::default() }
    }

    fn data() -> PanelData {
        let mut o = BTreeMap::new();
        o.insert("fg.enabled".to_owned(), opt(OptionType::Bool, "Frame Generation", json!(true)));
        o.insert("world.npc_density".to_owned(), OptionState { def: OptionDef { ty: OptionType::Float, label: Some("Crowd density".into()), min: Some(0.5), max: Some(3.0), step: Some(0.5), ..Default::default() }, value: json!(2.0), visible: true, ..Default::default() });
        let mut actions = BTreeMap::new();
        actions.insert("fg.toggle".to_owned(), ActionInfo { label: "Toggle FG".into(), hotkey: Some("F9".into()), ..Default::default() });
        let dlss = SurfaceState { key: "m1/aco-dlss".into(), title: "DLSS".into(), options: o, actions, ..Default::default() };
        let mut t = BTreeMap::new();
        t.insert("fps.cap".to_owned(), OptionState { def: OptionDef { ty: OptionType::Float, label: Some("Max frame rate".into()), ..Default::default() }, value: json!(120), visible: true, ..Default::default() });
        let tweaks = SurfaceState { key: "quiver".into(), options: t, ..Default::default() };
        let mut effective = quiver_api::perf::HudConfig::default();
        effective.layout = "compact".into();
        let settings = HudSettingsView { scope: Default::default(), set: Default::default(), effective, revision: 1, anti_cheat: None };
        let hud = HudState { status: Some(HudStatus { enabled: true, on: true, layout: "compact".into(), ..Default::default() }), settings };
        PanelData { options: Load::Ready(vec![dlss]), tweaks: Load::Ready(tweaks), hud: Load::Ready(hud), ..Default::default() }
    }

    fn pin() -> QuickPin {
        QuickPin { kind: PinKind::Option, surface: "m1/aco-dlss".into(), id: "world.npc_density".into(), label: "Crowd density".into() }
    }

    #[test]
    fn tiles_like_the_mock() {
        let t = tiles(&data(), &[pin()], &[], None);
        let names: Vec<(&str, &str)> = t.iter().map(|t| (t.label.as_str(), t.value.as_str())).collect();
        assert_eq!(names, [("Frame Generation", "On · F9"), ("HUD", "Compact"), ("Frame rate cap", "120 fps"), ("Crowd density", "2"), ("Screenshot", "This screen"), ("Quit game", "Hold A")]);
        // A replay buffer and a notice's buttons; no frame generation without the option.
        let mut d = data();
        if let Load::Ready(v) = &mut d.options {
            v[0].options.remove("fg.enabled");
        }
        let n = ToastData { notice: Some("n-3".into()), title: "DLSS v4 is ready".into(), buttons: vec![NoticeButton { id: "keep_v3".into(), label: "Keep v3 for now".into(), primary: true }], ..Default::default() };
        let t = tiles(&d, &[], &[n], Some(77));
        assert_eq!(t[0].kind, TileKind::Notice { id: "n-3".into(), button: "keep_v3".into() });
        assert!(!t.iter().any(|t| matches!(t.kind, TileKind::FrameGen { .. })));
        assert!(t.iter().any(|t| t.kind == TileKind::Replay { pid: 77 }));
        // A pin that's gone from the profile has no tile.
        let gone = QuickPin { id: "x.y".into(), ..pin() };
        assert_eq!(tiles(&data(), &[gone], &[], None).len(), 5);
    }

    #[test]
    fn what_tiles_do() {
        let d = data();
        let t = tiles(&d, &[pin()], &[], None);
        assert_eq!(act(&t[0], &d), Some(Act::SetOption("m1/aco-dlss".into(), "fg.enabled".into(), json!(false))));
        assert_eq!(act(&t[1], &d), Some(Act::HudPreset("standard".into())));
        assert_eq!(act(&t[2], &d), Some(Act::SetTweak("fps.cap".into(), json!(144))));
        assert_eq!(act(&t[3], &d), Some(Act::SetOption("m1/aco-dlss".into(), "world.npc_density".into(), json!(2.5))));
        assert_eq!(act(&t[4], &d), Some(Act::Screenshot));
        assert_eq!(act(&t[5], &d), Some(Act::Quit));
    }

    #[test]
    fn caps_step() {
        assert_eq!(next_cap(&json!(0)), json!(30));
        assert_eq!(next_cap(&json!(144)), json!("vrr"));
        assert_eq!(next_cap(&json!("vrr")), json!(0));
        assert_eq!(next_cap(&json!(100)), json!(120), "between two: the next one up");
        assert_eq!((cap_words(&json!(0)), cap_words(&json!(118.0)), cap_words(&json!("vrr"))), ("No cap".into(), "118 fps".into(), "Refresh − 3".into()));
    }

    fn quick() -> Quick {
        let mut q = Quick::default();
        q.set_tiles(tiles(&data(), &[pin()], &[], None));
        q
    }

    #[test]
    fn a_tap_opens_the_panel() {
        let mut q = quick();
        let t = Instant::now();
        q.press("s-1", t);
        assert_eq!(q.chord(Some(true), t + Duration::from_millis(100)), Effect::None);
        assert_eq!(q.chord(Some(false), t + Duration::from_millis(150)), Effect::OpenPanel);
        assert!(!q.active());
        // No word from the layer at all (an older one): the panel, after a while.
        q.press("s-1", t);
        assert_eq!(q.chord(None, t + Duration::from_millis(500)), Effect::None);
        assert_eq!(q.chord(None, t + STALE + Duration::from_millis(1)), Effect::OpenPanel);
    }

    #[test]
    fn hold_choose_release_applies() {
        let mut q = quick();
        let t = Instant::now();
        q.press("s-1", t);
        assert_eq!(q.chord(Some(true), t + HOLD), Effect::Show);
        assert!(q.shown() && q.focus == 0);
        assert_eq!(q.key(Key::Right, true, t + HOLD), Effect::Redraw);
        assert_eq!(q.key(Key::Right, true, t + HOLD), Effect::Redraw);
        assert_eq!(q.key(Key::Left, true, t + HOLD), Effect::Redraw);
        assert_eq!(q.focused().unwrap().kind, TileKind::HudPreset);
        assert_eq!(q.chord(Some(false), t + 2 * HOLD), Effect::Apply(1));
        assert!(!q.active());
        // Next time, focus comes back to the same tile.
        q.press("s-1", t);
        q.chord(Some(true), t + HOLD);
        assert_eq!(q.focused().unwrap().kind, TileKind::HudPreset);
        // Left at the first tile goes nowhere; Back closes without applying.
        q.focus = 0;
        assert_eq!(q.key(Key::Left, true, t), Effect::None);
        assert_eq!(q.key(Key::Back, true, t), Effect::Close);
    }

    #[test]
    fn quit_needs_a_held() {
        let mut q = quick();
        let t = Instant::now();
        q.press("s-1", t);
        q.chord(Some(true), t + HOLD);
        q.focus = q.tiles.len() - 1;
        // Released on Quit: latched, nothing quits.
        assert_eq!(q.chord(Some(false), t + 2 * HOLD), Effect::Redraw);
        assert_eq!(q.phase, Phase::Latched);
        let a = t + Duration::from_secs(1);
        assert_eq!(q.key(Key::Enter, true, a), Effect::Redraw);
        assert_eq!(q.tick(a + QUIT_HOLD / 2), Effect::None);
        assert!(q.quit_progress(a + QUIT_HOLD / 2).is_some_and(|p| (p - 0.5).abs() < 0.01));
        // Let go early: nothing.
        q.key(Key::Enter, false, a + QUIT_HOLD / 2);
        assert_eq!(q.tick(a + QUIT_HOLD * 2), Effect::None);
        q.key(Key::Enter, true, a + QUIT_HOLD * 2);
        assert_eq!(q.tick(a + QUIT_HOLD * 3), Effect::Quit);
        assert!(!q.active());
    }

    #[test]
    fn latched_from_the_cli() {
        let mut q = quick();
        let t = Instant::now();
        assert_eq!(q.latch("s-1", t), Effect::Show);
        // The chord's word doesn't close a latched menu.
        assert_eq!(q.chord(Some(false), t + HOLD), Effect::None);
        q.key(Key::Right, true, t);
        assert_eq!(q.key(Key::Enter, true, t), Effect::Apply(1));
    }

    #[test]
    fn a_held_menu_without_word_closes() {
        let mut q = quick();
        let t = Instant::now();
        q.press("s-1", t);
        q.chord(Some(true), t + HOLD);
        assert_eq!(q.chord(None, t + HOLD + Duration::from_millis(100)), Effect::None);
        assert_eq!(q.chord(None, t + HOLD + STALE + Duration::from_millis(1)), Effect::Close);
    }

    #[test]
    fn replay_buffers() {
        assert_eq!(replay_args(b"/usr/bin/gpu-screen-recorder\0-w\0screen\0-r\x0060\0-o\0/tmp\0"), Some(60));
        assert_eq!(replay_args(b"gpu-screen-recorder\0-w\0screen\0-o\0/tmp/a.mp4\0"), None, "recording, no replay");
        assert_eq!(replay_args(b"/usr/bin/obs\0-r\x0060\0"), None);
    }
}
