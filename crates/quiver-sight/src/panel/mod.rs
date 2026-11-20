//! The Sight panel (design/SIGHT.md §6, §9; as built in §14), summoned with Shift+F2 in the
//! game (read by the Sight layer), a hyprpad chord or `quiver sight toggle`, and driven with
//! the controller. Two presenters:
//!
//! - **In the game** ([`ingame`], the first choice): the Sight layer composites the panel into
//!   the game's frames and the input preload hands it the game's keyboard and mouse; the game
//!   keeps the keyboard focus, so nothing about the game's window changes (exclusive-fullscreen
//!   games minimised when a surface took the keyboard: SIGHT §6 "Focus side effects").
//! - **On the desktop**, when the game can't (no layer drawing: OpenGL, the layer off or not
//!   loaded, an HDR swapchain; no preload; or the layer didn't show it within 2 s): a
//!   full-output wlr-layer-shell surface on the game's monitor (overlay layer, namespace
//!   [`NAMESPACE`], on-demand keyboard focus while shown).
//!
//! Either way the state file `$XDG_RUNTIME_DIR/quiver/sight-panel` says the panel is open
//! (hyprpad's `quiver-sight` mode reads it: `tools/hyprpad/quiver-sight.lua`).
//!
//! - **Opening**: remembers the focused window (to give focus back on close) and finds the
//!   game's window (for Quit), picks the backdrop, opens the surface, and loads the session's
//!   data through the link. Live metrics are subscribed while it's open.
//! - **Backdrop** (SIGHT §14): when the session's HUD model says its Sight layer can blur
//!   (`HudModel::backdrop`), the request goes into the session's `hud.image` and the layer
//!   blurs the game's own frames under any compositor ([`Backdrop::GameBlur`]); it is renewed
//!   every half second and dropped if the layer doesn't report blurring within 2.5 s.
//!   Otherwise (OpenGL, no layer, the layer can't) Hyprland's blur rule when its blur is on,
//!   else the plain dim.
//! - **Input**: the keyboard (the surface has it), the mouse, and the controller two ways:
//!   hyprpad's `quiver-sight` mode types keys into the surface while the game's pad is held
//!   neutral (`tools/hyprpad/quiver-sight.lua`), and gilrs reads pads directly where it can
//!   (no hyprpad, or a pad hyprpad doesn't own). Both end up as the same [`Input`]s.
//! - **Tied to the game** (SIGHT §14): while the game isn't in front ([`crate::front`]:
//!   another workspace, another window active, minimised) the panel is hidden: its surface
//!   goes (no keyboard grab, hyprpad's mode ends, the blur request is dropped) but the view
//!   and its state stay, and a new surface around the same view comes back when the game is.
//!   On-demand keyboard focus is what lets the compositor move focus away at all (an exclusive
//!   surface keeps it on every workspace); if it doesn't arrive, the panel is mapped again
//!   exclusive.
//! - **Closing**: B at the top, Escape, Shift+F2, the chord again: the surface goes, the
//!   layer's blur request is cleared (it fades out) or the blur rule turned off, focus goes back
//!   to the game's window (Hyprland `focuswindow`).

pub mod data;
pub mod ingame;
pub mod sections;
pub mod state;
pub mod view;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
use gpui::{App, AppContext, Bounds, Entity, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px, size};
use quiver_api::perf::LayerBackdrop;
use quiver_api::sight::{Backdrop, PanelAction, PanelCommand, PanelReport, PanelSection};
use quiver_perf::stereo::Stereo;
use quiver_perf::hud::{BackdropRequest, HudWriter};
use quiver_perf::panel::{PANEL_INPUT_FILE, PanelEvent, PanelInput, create_panel_input};
use quiver_perf::shm::Mapping;
use quiver_ui::gamepad::PadEvent;
use quiver_ui::input::Input;

use crate::front::Verdict;
use crate::hypr::{self, Hypr};
use crate::link::{LinkReq, Loaded, Msg};
use data::Part;
use state::{Nav, PanelData};
use view::{Footer, PanelView, Wiring};

/// The panel's layer-shell namespace: hyprpad's mode rule and the blur rule match exactly
/// this (toasts are `quiver-sight-toast`, the desktop HUD `quiver-sight-hud`).
pub const NAMESPACE: &str = "quiver-sight";

/// Sight's own settings (`$XDG_DATA_HOME/quiver/sight.toml`, next to Quiver's other files).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Settings {
    /// Select+Start held 0.6 s opens the panel (SIGHT §6's fallback without hyprpad; the
    /// game sees those buttons too). Off by default.
    #[serde(default)]
    pub pad_chord: bool,
}

pub fn settings_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("quiver").join("sight.toml")
}

pub fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

/// hyprpad is running (its status file names a live pid), so its `quiver-sight` mode can
/// hold the game's pad neutral and type keys into the panel.
pub fn hyprpad_running() -> bool {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else { return false };
    let p = std::path::Path::new(&dir).join("hyprpad/status.json");
    let pid = std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v.get("pid")?.as_u64());
    pid.is_some_and(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
}

/// The open panel. Its view outlives its windows: while the game isn't in front the panel is
/// hidden (no surface) but stays open, and comes back in the same state (section, selection,
/// drafts, scroll).
struct Open {
    view: Entity<PanelView>,
    /// The layer surface while shown.
    window: Option<WindowHandle<PanelView>>,
    session: String,
    /// The game's monitor (output name) and its logical size.
    monitor: Option<String>,
    logical: (f32, f32),
    /// When the current surface was mapped.
    mapped_at: Option<Instant>,
    /// Since when the surface has had the keyboard (`None`: it hasn't now); whether it has had
    /// it at all since it was mapped.
    keyboard: Option<Instant>,
    had_keyboard: bool,
    /// Numbers the surfaces, so a late activation event from an old one is ignored.
    map_gen: u64,
    /// The game has been away since (hiding waits [`HIDE_AFTER`]).
    away_since: Option<Instant>,
    interactivity: KeyboardInteractivity,
    /// Drawn in the game instead of a surface.
    game: Option<ingame::InGame>,
    /// While drawn in the game: an invisible marker surface in the panel's namespace (see
    /// [`Panel::map_marker`]).
    marker: Option<WindowHandle<Marker>>,
}

/// The marker surface's view: nothing to draw.
pub struct Marker;

impl gpui::Render for Marker {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

/// `$XDG_RUNTIME_DIR/quiver/sight-panel`: `open <pid> <game|desktop> <session>` while the panel
/// is open, absent otherwise. hyprpad's `quiver-sight` mode reads it (the panel drawn in the
/// game has no layer-shell surface for `ctx.layers` to see) and checks the pid, so a
/// quiver-sight that died doesn't hold the pad.
pub fn state_path() -> std::path::PathBuf {
    quiver_api::socket_path().parent().map(std::path::Path::to_path_buf).unwrap_or_else(std::env::temp_dir).join("sight-panel")
}

/// Writes (or with `None` removes) the state file. Best effort.
pub fn write_state(open: Option<(&str, &str)>) {
    let p = state_path();
    match open {
        Some((session, presenter)) => {
            let tmp = p.with_extension("tmp");
            let line = format!("open {} {presenter} {session}\n", std::process::id());
            if std::fs::write(&tmp, line).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
                tracing::debug!("couldn't write {}", p.display());
            }
        }
        None => {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// The game must be away this long before the panel hides: a focus change that comes in
/// two steps on two connections (the panel's keyboard, the toplevel list) doesn't flicker it.
pub const HIDE_AFTER: Duration = Duration::from_millis(120);
/// A freshly mapped panel counts as having the keyboard until its first `enter` or this long.
pub const KEYBOARD_GRACE: Duration = Duration::from_millis(700);

/// How long the request lasts without renewal: quiver-sight renews it every half second
/// while the panel is open, so a quiver-sight that dies stops the blur within this.
const GAME_BLUR_HOLD: Duration = Duration::from_secs(2);
/// The layer must report blurring within this, else the panel falls back (the game stopped
/// presenting when it lost focus, or the layer can't after all).
const GAME_BLUR_CONFIRM: Duration = Duration::from_millis(2500);

/// The Sight layer's blur of the game behind the open panel (design/SIGHT.md §14): the
/// request in the session's `hud.image`, renewed while the panel is open.
struct GameBlur {
    session: String,
    writer: HudWriter<Mapping>,
    since: Instant,
    confirmed: bool,
    /// The frames' stereo layout: the layer keeps each eye's blur inside it.
    stereo: Stereo,
}

impl GameBlur {
    fn start(session: &str, image: &std::path::Path, stereo: Stereo) -> Result<GameBlur> {
        let writer = quiver_perf::session::open_hud_writer(image).with_context(|| format!("opening {}", image.display()))?;
        let g = GameBlur { session: session.to_owned(), writer, since: Instant::now(), confirmed: false, stereo };
        g.renew();
        Ok(g)
    }

    fn renew(&self) {
        let until = quiver_perf::clock::mono_raw_ns() + GAME_BLUR_HOLD.as_nanos() as i64;
        // No dim from the layer: the panel draws its own (theme-aware) tint over the blur.
        self.writer.set_backdrop(&BackdropRequest::blur(0, 0, until).with_stereo(self.stereo));
    }
}

impl Drop for GameBlur {
    fn drop(&mut self) {
        self.writer.set_backdrop(&BackdropRequest::NONE);
    }
}

pub struct PanelCtl {
    open: Option<Open>,
    link: Option<tokio::sync::mpsc::UnboundedSender<LinkReq>>,
    ui: Option<UnboundedSender<Msg>>,
    hypr: Option<Hypr>,
    hypr_tried: bool,
    layer_shell: bool,
    /// Kept across opens (the section and rows the user was on).
    nav: Nav,
    report: PanelReport,
    pad_started: bool,
    pad_name: Option<String>,
    pad_unavailable: Option<String>,
    settings: Settings,
    /// The namespace to open with (`--demo panel` uses a test one).
    pub namespace: String,
    /// Each session's Sight layer blur state, from its HUD model.
    layers: HashMap<String, LayerBackdrop>,
    /// Each session's `panel.input` (made next to its `hud.image` when the session appears,
    /// so the game's layer and preload can say what they can do before the panel opens).
    inputs: HashMap<String, Arc<PanelInput<Mapping>>>,
    /// The layer blurring the game behind the open panel.
    game_blur: Option<GameBlur>,
    /// Each session's stereo layout, from its HUD model (design/SIGHT.md §15).
    stereo: HashMap<String, Stereo>,
}

impl PanelCtl {
    pub fn new(layer_shell: bool, link: Option<tokio::sync::mpsc::UnboundedSender<LinkReq>>, ui: Option<UnboundedSender<Msg>>) -> Self {
        let settings = load_settings();
        Self {
            open: None,
            link,
            ui,
            hypr: None,
            hypr_tried: false,
            layer_shell,
            nav: Nav::default(),
            report: PanelReport { pad_chord: settings.pad_chord, ..Default::default() },
            pad_started: false,
            pad_name: None,
            pad_unavailable: None,
            settings,
            namespace: NAMESPACE.into(),
            layers: HashMap::new(),
            inputs: HashMap::new(),
            game_blur: None,
            stereo: HashMap::new(),
        }
    }

    /// A session's HUD model said whether its Sight layer can blur the game (or `None`: it has
    /// no `hud.image`, or the session ended).
    pub fn layer_backdrop(&mut self, session: &str, b: Option<LayerBackdrop>, cx: &mut App) {
        match b {
            Some(b) => {
                if !self.inputs.contains_key(session)
                    && let Some(dir) = b.image.parent()
                {
                    match create_panel_input(&dir.join(PANEL_INPUT_FILE)) {
                        Ok(p) => {
                            p.set_sight_pid(std::process::id());
                            self.inputs.insert(session.to_owned(), Arc::new(p));
                        }
                        Err(e) => tracing::info!(session, "no panel.input ({e}): the panel can only open on the desktop"),
                    }
                }
                self.layers.insert(session.to_owned(), b);
            }
            None => {
                self.layers.remove(session);
                self.inputs.remove(session);
            }
        }
        self.check_game_blur(cx);
    }

    /// A session's HUD model said how its frames pack their eyes (`None`: mono, or the session
    /// ended). A change while the panel is drawn in that game opens it again, laid out anew.
    pub fn set_stereo(&mut self, session: &str, s: Option<Stereo>, cx: &mut App) {
        let s = s.unwrap_or_default();
        let old = self.stereo.insert(session.to_owned(), s).unwrap_or_default();
        if old == s {
            return;
        }
        tracing::info!(session, from = old.as_str(), to = s.as_str(), "stereo layout");
        if let Some(g) = self.game_blur.as_mut().filter(|g| g.session == session) {
            g.stereo = s;
            g.renew();
        }
        let Some(o) = self.open.as_mut() else { return };
        if o.session != session || o.game.is_none() {
            return;
        }
        let g = o.game.take().unwrap();
        let input = g.input_handle();
        let extent = g.extent;
        g.close(cx);
        if let Err(e) = self.open_in_game(session, input, extent, cx) {
            tracing::warn!(session, "panel in the game, laid out for {}: {e:#}", s.as_str());
            self.report.presenter_detail = format!("drawing it in the game failed: {e:#}");
            self.report_now();
        }
    }

    /// Every half second: renews the open panel's blur request, and gives up on it if the
    /// layer can't (or doesn't) blur.
    pub fn keepalive(&mut self, cx: &mut App) {
        if let Some(g) = &self.game_blur {
            g.renew();
        }
        self.check_game_blur(cx);
        self.check_in_game(cx);
    }

    /// Renews the panel drawn in the game (every tick), and moves it to the desktop if the
    /// layer hasn't shown it within [`ingame::CONFIRM_WITHIN`].
    pub fn renew_in_game(&mut self) {
        if let Some(g) = self.open.as_mut().and_then(|o| o.game.as_mut()) {
            g.renew();
        }
    }

    fn check_in_game(&mut self, cx: &mut App) {
        let Some(o) = self.open.as_mut() else { return };
        let Some(g) = o.game.as_mut() else { return };
        if g.renew() || g.opened.elapsed() < ingame::CONFIRM_WITHIN {
            return;
        }
        let g = o.game.take().unwrap();
        let why = format!("the game didn't show the panel within {:?} (is it presenting?)", ingame::CONFIRM_WITHIN);
        tracing::info!(session = %g.session, %why, "panel: moving to the desktop");
        g.close(cx);
        self.game_blur = None;
        self.report.presenter = "desktop".into();
        self.report.presenter_detail = why;
        if let Err(e) = self.map(cx) {
            tracing::warn!("panel: {e:#}");
            self.report.last_error = Some(format!("{e:#}"));
            self.report_now();
        }
        write_state(self.open.as_ref().map(|o| (o.session.as_str(), "desktop")));
    }

    /// Input the game's preload forwarded to the panel drawn in the game.
    pub fn game_input(&mut self, session: &str, events: &[PanelEvent], cx: &mut App) {
        if let Some(g) = self.open.as_mut().and_then(|o| o.game.as_mut()).filter(|g| g.session == session) {
            g.input(events, cx);
        }
    }

    /// The open panel's navigation state (tests, the demo).
    pub fn nav(&self, cx: &App) -> Option<Nav> {
        self.open.as_ref().map(|o| o.view.read(cx).nav.clone())
    }

    /// The last report (tests).
    pub fn report(&self) -> &PanelReport {
        &self.report
    }

    /// The panel is drawn in the game now.
    pub fn in_game(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.game.is_some())
    }

    fn check_game_blur(&mut self, cx: &mut App) {
        let Some(g) = &mut self.game_blur else { return };
        let layer = self.layers.get(&g.session);
        if layer.is_some_and(|l| l.blurring) {
            if !g.confirmed {
                tracing::info!(session = %g.session, after = ?g.since.elapsed(), "the Sight layer blurs the game behind the panel");
            }
            g.confirmed = true;
            return;
        }
        let why = match layer {
            None => "the session has no Sight layer state any more".to_owned(),
            Some(l) if !l.ready => format!("the Sight layer can't blur: {}", l.reason),
            Some(_) if !g.confirmed && g.since.elapsed() > GAME_BLUR_CONFIRM => "the Sight layer didn't blur (is the game still presenting?)".to_owned(),
            Some(_) => return,
        };
        self.game_blur = None;
        // In the game, the compositor's blur doesn't reach the panel: the dim.
        let (backdrop, detail) = if self.in_game() { (Backdrop::Dim, "plain dim".to_owned()) } else { self.compositor_backdrop() };
        let detail = format!("{why}; {detail}");
        tracing::info!(backdrop = backdrop.as_str(), %detail, "panel backdrop changed");
        self.with_view(cx, |v, _| v.backdrop = backdrop);
        self.report.backdrop = backdrop;
        self.report.backdrop_detail = detail;
        self.report_now();
    }

    /// The backdrop without the Sight layer: Hyprland's blur rule when its blur is on, else
    /// the plain dim.
    fn compositor_backdrop(&mut self) -> (Backdrop, String) {
        let namespace = self.namespace.clone();
        let Some(h) = self.hypr() else { return (Backdrop::Dim, "not Hyprland: plain dim".to_owned()) };
        let blur = h.blur();
        if !blur.enabled {
            return (Backdrop::Dim, "blur is off in Hyprland (decoration:blur:enabled): plain dim".to_owned());
        }
        match h.blur_on(&namespace) {
            Ok(()) => (Backdrop::Blur, format!("Hyprland layer rule ({:?} syntax), your blur: size {} × {} passes", h.flavor, blur.size, blur.passes)),
            Err(e) => (Backdrop::Dim, format!("the blur rule failed ({e}): plain dim")),
        }
    }

    /// Starts reading pads (once): at startup with the chord fallback on, else at the first open.
    pub fn start_pad(&mut self) {
        if self.pad_started {
            return;
        }
        let Some(ui) = self.ui.clone() else { return };
        self.pad_started = true;
        let opts = quiver_ui::gamepad::PadOptions { chord: self.settings.pad_chord.then(quiver_ui::gamepad::select_start) };
        quiver_ui::gamepad::spawn_with(opts, move |e| {
            let _ = ui.unbounded_send(Msg::Pad(e));
        });
    }

    pub fn pad_chord(&self) -> bool {
        self.settings.pad_chord
    }

    fn hypr(&mut self) -> Option<&mut Hypr> {
        if !self.hypr_tried {
            self.hypr_tried = true;
            self.hypr = Hypr::connect();
        }
        self.hypr.as_mut()
    }

    fn send(&self, r: LinkReq) {
        if let Some(l) = &self.link {
            let _ = l.send(r);
        }
    }

    fn report_now(&mut self) {
        self.report.pad = match (&self.pad_name, &self.pad_unavailable) {
            (Some(n), _) => n.clone(),
            (None, Some(why)) => format!("no gamepad support: {why}"),
            (None, None) => "none".into(),
        };
        self.report.pad_chord = self.settings.pad_chord;
        self.send(LinkReq::Report(self.report.clone()));
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Open and on screen (or in the game's frames).
    pub fn is_shown(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.window.is_some() || o.game.is_some())
    }

    /// The open panel's session (shown or hidden).
    pub fn session(&self) -> Option<&str> {
        self.open.as_ref().map(|o| o.session.as_str())
    }

    /// The panel is shown and has the keyboard, or was just mapped and hasn't had its first
    /// `enter` yet ([`KEYBOARD_GRACE`]): for the "is the game in front" verdict.
    pub fn has_keyboard(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.window.is_some() && (o.keyboard.is_some() || (!o.had_keyboard && o.mapped_at.is_some_and(|t| t.elapsed() < KEYBOARD_GRACE))))
    }

    /// Since when the shown panel has had the keyboard.
    pub fn keyboard_since(&self) -> Option<Instant> {
        self.open.as_ref().filter(|o| o.window.is_some()).and_then(|o| o.keyboard)
    }

    /// Hyprland's window for `session` (the one whose process has `QUIVER_SESSION`), when
    /// Hyprland is there: what ties the session to its toplevel most surely.
    pub fn hypr_game_window(&mut self, session: &str) -> Option<hypr::Window> {
        let h = self.hypr()?;
        let active = h.active_window();
        let clients = h.clients();
        hypr::game_window(&clients, active.as_ref(), session, &[], std::path::Path::new("/proc")).cloned()
    }

    /// `show`: whether the game is in front when the command comes (else the panel opens
    /// hidden, and shows when the game is in front).
    pub fn command(&mut self, c: PanelCommand, show: Option<String>, cx: &mut App) {
        if c.seq > 0 {
            self.report.handled_seq = self.report.handled_seq.max(c.seq);
        }
        let open = match c.action {
            PanelAction::Close => false,
            PanelAction::Open => true,
            PanelAction::Toggle => !self.is_open(),
        };
        if !open {
            self.close(cx);
            return;
        }
        let Some(session) = c.session.clone() else {
            self.report.last_error = Some("no session to open the panel for".into());
            self.report_now();
            return;
        };
        if let Some(o) = &self.open {
            // Already open: switch section (or session).
            if o.session == session {
                if let Some(s) = c.section {
                    o.view.update(cx, |v, cx| {
                        v.nav.select_section(s);
                        cx.notify();
                    });
                }
                return;
            }
            self.close(cx);
        }
        if let Err(e) = self.open(&session, c.section, c.monitor.as_deref(), show, cx) {
            tracing::warn!("panel: {e:#}");
            self.report.last_error = Some(format!("{e:#}"));
            self.report_now();
        }
    }

    /// The backdrop: the Sight layer blurs the game's own frames when it can (any compositor);
    /// else the compositor's blur, else the dim. Starts the layer's request.
    fn pick_backdrop(&mut self, session: &str) -> (Backdrop, String) {
        self.game_blur = None;
        match self.layers.get(session) {
            Some(l) if l.ready => match GameBlur::start(session, &l.image, self.stereo.get(session).copied().unwrap_or_default()) {
                Ok(g) => {
                    self.game_blur = Some(g);
                    (Backdrop::GameBlur, "the Sight layer blurs the game's frames".to_owned())
                }
                Err(e) => {
                    let (b, d) = self.compositor_backdrop();
                    (b, format!("asking the Sight layer to blur failed ({e:#}); {d}"))
                }
            },
            l => {
                let why = l.map_or_else(|| "no Sight layer state for this session".to_owned(), |l| format!("the Sight layer can't blur: {}", l.reason));
                let (b, d) = self.compositor_backdrop();
                (b, format!("{why}; {d}"))
            }
        }
    }

    fn open(&mut self, session: &str, section: Option<PanelSection>, monitor: Option<&str>, hidden: Option<String>, cx: &mut App) -> Result<()> {
        // In the game when the game can show it; else a layer-shell surface.
        let now = quiver_perf::clock::mono_raw_ns();
        let game = match self.inputs.get(session) {
            Some(p) => ingame::available(p, now).map(|extent| (p.clone(), extent)),
            None => Err("no panel.input for this session (no Sight layer state yet)".into()),
        };
        if game.is_err() && !self.layer_shell {
            anyhow::bail!("can't draw it in the game ({}) and no layer-shell here (not a Wayland session, or headless)", game.err().unwrap_or_default());
        }
        self.start_pad();
        // Where the game is, before the panel takes focus.
        let (return_to, game_addr, mon) = match self.hypr() {
            None => (None, None, None),
            Some(h) => {
                let active = h.active_window();
                let clients = h.clients();
                let game = hypr::game_window(&clients, active.as_ref(), session, &[], std::path::Path::new("/proc")).map(|w| w.address.clone());
                let monitors = h.monitors();
                let mon = monitor
                    .and_then(|m| monitors.iter().find(|x| x.name == m))
                    .or_else(|| active.as_ref().and_then(|a| monitors.iter().find(|m| m.id == a.monitor)))
                    .or_else(|| monitors.iter().find(|m| m.focused))
                    .cloned();
                (active.map(|a| a.address), game, mon)
            }
        };
        let flavor = self.hypr.as_ref().map(|h| h.flavor);
        let quit: Option<std::rc::Rc<dyn Fn() -> Result<(), String>>> = game_addr.clone().zip(flavor).map(|(addr, flavor)| {
            let f: std::rc::Rc<dyn Fn() -> Result<(), String>> = std::rc::Rc::new(move || match Hypr::with_flavor(flavor) {
                Some(h) => h.close(&addr),
                None => Err("Hyprland isn't reachable".into()),
            });
            f
        });
        let wiring = Wiring { link: self.link.clone(), ui: self.ui.clone(), quit };
        let mut nav = self.nav.clone();
        nav.open(section);
        let footer = Footer { pad: self.pad_name.clone().unwrap_or_default(), pad_shared: self.pad_name.is_some() && !hyprpad_running() };
        let sid = session.to_owned();
        // The view outlives its windows: hiding the panel drops the surface, not this.
        let view = cx.new(|cx| {
            let mut v = PanelView::new(sid, PanelData::default(), nav, Backdrop::Dim, wiring, cx);
            v.footer = footer;
            v
        });
        let logical = mon.as_ref().map(|m| m.logical()).unwrap_or((1920.0, 1080.0));
        self.open = Some(Open {
            view,
            window: None,
            session: session.to_owned(),
            monitor: mon.as_ref().map(|m| m.name.clone()),
            logical,
            mapped_at: None,
            keyboard: None,
            had_keyboard: false,
            map_gen: 0,
            away_since: None,
            interactivity: KeyboardInteractivity::OnDemand,
            game: None,
            marker: None,
        });
        self.report = PanelReport {
            open: true,
            section: section.unwrap_or(self.nav.section),
            session: Some(session.to_owned()),
            monitor: mon.map(|m| m.name),
            return_to,
            handled_seq: self.report.handled_seq,
            ..Default::default()
        };
        tracing::info!(session, monitor = ?self.report.monitor, return_to = ?self.report.return_to, game_window = ?game_addr, hidden = ?hidden, "panel opened");
        let in_game = match game {
            Ok((p, extent)) => match self.open_in_game(session, p, extent, cx) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(session, "panel in the game: {e:#}");
                    self.report.presenter_detail = format!("drawing it in the game failed: {e:#}");
                    false
                }
            },
            Err(why) => {
                tracing::info!(session, %why, "panel on the desktop");
                self.report.presenter_detail = why;
                false
            }
        };
        if !in_game {
            if !self.layer_shell {
                self.close(cx);
                anyhow::bail!("no layer-shell here (not a Wayland session, or headless)");
            }
            self.report.presenter = "desktop".into();
            match hidden {
                None => self.map(cx)?,
                Some(why) => {
                    self.report.hidden = Some(why);
                    self.report_now();
                }
            }
        }
        if in_game {
            self.map_marker(cx);
        }
        write_state(Some((session, if in_game { "game" } else { "desktop" })));
        self.send(LinkReq::Load { session: session.to_owned(), part: Part::All, view: None });
        self.send(LinkReq::Subscribe { session: session.to_owned() });
        Ok(())
    }

    /// The panel drawn in the game: its window at the swapchain's size, the layer's blur
    /// behind it (else a plain dim: no compositor blur reaches the game's frames).
    fn open_in_game(&mut self, session: &str, input: Arc<PanelInput<Mapping>>, extent: (u32, u32), cx: &mut App) -> Result<()> {
        let Some(dir) = self.layers.get(session).and_then(|l| l.image.parent().map(|d| d.to_path_buf())) else {
            anyhow::bail!("no session dir");
        };
        let Some(ui) = self.ui.clone() else { anyhow::bail!("no UI channel") };
        let Some(o) = &self.open else { anyhow::bail!("not open") };
        // A packed stereo frame: the panel is laid out for one eye and drawn in each
        // (design/SIGHT.md §15).
        let stereo = self.stereo.get(session).copied().filter(|s| s.fits(extent)).unwrap_or_default();
        let factor = ingame::factor(stereo.logical_extent(extent), o.monitor.is_some().then_some(o.logical));
        let view = o.view.clone();
        self.game_blur = None;
        let (backdrop, detail) = match self.layers.get(session) {
            Some(l) if l.ready => match GameBlur::start(session, &l.image, self.stereo.get(session).copied().unwrap_or_default()) {
                Ok(g) => {
                    self.game_blur = Some(g);
                    (Backdrop::GameBlur, "the Sight layer blurs the game's frames".to_owned())
                }
                Err(e) => (Backdrop::Dim, format!("asking the Sight layer to blur failed ({e:#}): plain dim")),
            },
            l => (Backdrop::Dim, format!("{}: plain dim", l.map_or("no Sight layer state".to_owned(), |l| format!("the Sight layer can't blur ({})", l.reason)))),
        };
        view.update(cx, |v, cx| {
            v.backdrop = backdrop;
            cx.notify();
        });
        let g = ingame::InGame::open(session, input, &dir, extent, stereo, factor, view, ui, cx)?;
        let Some(o) = &mut self.open else { anyhow::bail!("not open") };
        o.game = Some(g);
        self.report.presenter = "game".into();
        let eyes = if stereo.is_stereo() {
            let (lw, lh) = stereo.logical_extent(extent);
            format!(" in each eye ({}, laid out at {lw}x{lh})", stereo.as_str())
        } else {
            String::new()
        };
        self.report.presenter_detail = format!("the Sight layer draws it in the game's {}x{} frames{eyes} at {factor:.2}×; the game keeps the keyboard", extent.0, extent.1);
        self.report.backdrop = backdrop;
        self.report.backdrop_detail = detail;
        self.report.hidden = None;
        self.report_now();
        Ok(())
    }

    /// The panel drawn in the game has no surface, so nothing tells the compositor it's open:
    /// hyprpad's `quiver-sight` mode, which holds the game's pad neutral and types the pad
    /// into the panel, never engaged (its rule's state-file check isn't re-run when nothing
    /// in the compositor changes). A 1x1 surface in the panel's namespace on the background
    /// layer, under the game (invisible, no input, no keyboard, no effect on the game's
    /// direct scanout or VRR), makes Hyprland announce the namespace (`openlayer`), which
    /// hyprpad's `ctx.layers:has("quiver-sight")` follows at once. Removed on close.
    fn map_marker(&mut self, cx: &mut App) {
        if !self.layer_shell {
            return;
        }
        let Some(o) = &mut self.open else { return };
        if o.marker.is_some() {
            return;
        }
        let display_id = o.monitor.as_deref().and_then(|m| crate::desktop::display_named(cx, m));
        let opts = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(1.), px(1.)) })),
            app_id: Some("quiver-sight".into()),
            focus: false,
            window_background: WindowBackgroundAppearance::Transparent,
            display_id,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: self.namespace.clone(),
                layer: Layer::Background,
                anchor: Anchor::TOP | Anchor::LEFT,
                keyboard_interactivity: KeyboardInteractivity::None,
                ..Default::default()
            }),
            ..Default::default()
        };
        match cx.open_window(opts, |_, cx| cx.new(|_| Marker)) {
            Ok(h) => {
                tracing::info!(session = %o.session, namespace = %self.namespace, "panel marker surface mapped (hyprpad's quiver-sight mode)");
                o.marker = Some(h);
            }
            Err(e) => tracing::warn!("panel marker surface: {e:#}"),
        }
    }

    /// Puts the open panel on screen (at open, and each time the game is in front again):
    /// a new layer surface around the same view, the backdrop picked anew.
    fn map(&mut self, cx: &mut App) -> Result<()> {
        let Some(o) = &self.open else { return Ok(()) };
        if o.window.is_some() {
            return Ok(());
        }
        let session = o.session.clone();
        let (backdrop, detail) = self.pick_backdrop(&session);
        let Some(o) = &mut self.open else { return Ok(()) };
        let display_id = o.monitor.as_deref().and_then(|m| crate::desktop::display_named(cx, m));
        let view = o.view.clone();
        view.update(cx, |v, cx| {
            v.backdrop = backdrop;
            cx.notify();
        });
        o.map_gen += 1;
        let map_gen = o.map_gen;
        let ui = self.ui.clone();
        let opts = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(o.logical.0), px(o.logical.1)) })),
            app_id: Some("quiver-sight".into()),
            focus: true,
            window_background: WindowBackgroundAppearance::Transparent,
            display_id,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: self.namespace.clone(),
                layer: Layer::Overlay,
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                // Over bars' exclusive zones too: the whole output is the panel's.
                exclusive_zone: Some(px(-1.)),
                // On demand, not exclusive: the compositor focuses it when it maps (Hyprland
                // does), and can still take the keyboard away on a workspace switch or a
                // focus change, which is what hides the panel (an exclusive surface keeps the
                // keyboard on every workspace, and Hyprland then refuses every focus change,
                // so the game's toplevel never stops being active). If the keyboard doesn't
                // come, the panel is mapped again exclusive (`front`).
                keyboard_interactivity: o.interactivity,
                ..Default::default()
            }),
            ..Default::default()
        };
        let root = view.clone();
        let handle = cx.open_window(opts, move |window, cx| {
            let focus = root.read(cx).focus.clone();
            window.focus(&focus, cx);
            root.update(cx, |_, cx| {
                // Keyboard focus comes and goes with the compositor's: tell the controller.
                cx.observe_window_activation(window, move |_, window, _| {
                    if let Some(ui) = &ui {
                        let _ = ui.unbounded_send(Msg::PanelKeyboard { map_gen, active: window.is_window_active() });
                    }
                })
                .detach();
            });
            root
        })?;
        o.window = Some(handle);
        o.mapped_at = Some(Instant::now());
        o.keyboard = None;
        o.had_keyboard = false;
        o.away_since = None;
        tracing::info!(session = %o.session, monitor = ?o.monitor, backdrop = backdrop.as_str(), %detail, interactivity = ?o.interactivity, "panel shown");
        self.report.backdrop = backdrop;
        self.report.backdrop_detail = detail;
        self.report.hidden = None;
        self.report_now();
        Ok(())
    }

    /// Takes the open panel off screen without closing it: the surface goes (and with it the
    /// keyboard and hyprpad's `quiver-sight` mode), the layer's blur request is dropped (the
    /// game isn't visible), the view and its state stay.
    fn hide(&mut self, why: String, cx: &mut App) {
        let Some(o) = &mut self.open else { return };
        let Some(h) = o.window.take() else { return };
        let _ = h.update(cx, |_, w, _| w.remove_window());
        o.mapped_at = None;
        o.keyboard = None;
        o.away_since = None;
        self.game_blur = None;
        tracing::info!(session = %o.session, %why, "panel hidden: the game isn't in front");
        self.report.hidden = Some(why);
        self.report_now();
    }

    /// The window's keyboard focus changed (from the map numbered `map_gen`).
    pub fn keyboard(&mut self, map_gen: u64, active: bool) {
        let Some(o) = &mut self.open else { return };
        if o.map_gen != map_gen || o.window.is_none() {
            return;
        }
        o.keyboard = if active { Some(o.keyboard.unwrap_or_else(Instant::now)) } else { None };
        o.had_keyboard |= active;
        tracing::debug!(active, "panel keyboard");
    }

    /// Is the game in front ([`crate::front`])? Hides the panel when it isn't for
    /// [`HIDE_AFTER`], shows it again when it is (or when nothing can tell any more). Returns
    /// when to ask again, if a timer is pending.
    pub fn front(&mut self, v: &Verdict, game_window: Option<String>, cx: &mut App) -> Option<Duration> {
        let o = self.open.as_mut()?;
        let now = Instant::now();
        let changed = self.report.front != v.why() || self.report.game_window != game_window;
        self.report.front = v.why().to_owned();
        self.report.game_window = game_window;
        // In the game's frames it's only seen with the game: nothing to hide or show.
        if o.game.is_some() {
            if changed {
                self.report_now();
            }
            return None;
        }
        let shown = o.window.is_some();
        let recheck = match (shown, v.away()) {
            (true, true) => {
                let since = *o.away_since.get_or_insert(now);
                let left = HIDE_AFTER.saturating_sub(now.saturating_duration_since(since));
                if left.is_zero() {
                    self.hide(v.why().to_owned(), cx);
                    return None;
                }
                Some(left)
            }
            (true, false) => {
                o.away_since = None;
                // Mapped on demand, and the keyboard never came: map it again exclusive (a
                // compositor that doesn't focus on-demand surfaces when they map).
                let waited = o.mapped_at.map(|t| now.saturating_duration_since(t));
                if !o.had_keyboard && o.interactivity == KeyboardInteractivity::OnDemand && waited.is_some_and(|w| w >= KEYBOARD_GRACE) && matches!(v, Verdict::Front(_)) {
                    tracing::info!("the panel didn't get the keyboard on demand: mapping it again with exclusive keyboard focus");
                    o.interactivity = KeyboardInteractivity::Exclusive;
                    if let Some(h) = o.window.take() {
                        let _ = h.update(cx, |_, w, _| w.remove_window());
                    }
                    self.game_blur = None;
                    if let Err(e) = self.map(cx) {
                        tracing::warn!("panel: {e:#}");
                    }
                    return None;
                }
                // Until the first `enter` the grace counts as having the keyboard: ask again
                // when it ends.
                waited.filter(|w| !o.had_keyboard && *w < KEYBOARD_GRACE).map(|w| KEYBOARD_GRACE - w + Duration::from_millis(10))
            }
            (false, false) => {
                if let Err(e) = self.map(cx) {
                    tracing::warn!("panel: {e:#}");
                    self.report.last_error = Some(format!("{e:#}"));
                }
                return None;
            }
            (false, true) => None,
        };
        if changed {
            self.report_now();
        }
        recheck
    }

    pub fn close(&mut self, cx: &mut App) {
        let Some(mut o) = self.open.take() else { return };
        write_state(None);
        if let Some(m) = o.marker.take() {
            let _ = m.update(cx, |_, w, _| w.remove_window());
        }
        let shown = o.window.is_some();
        if let Some(h) = &o.window {
            let _ = h.update(cx, |_, w, _| w.remove_window());
        }
        if let Some(g) = o.game.take() {
            g.close(cx);
        }
        self.nav = o.view.read(cx).nav.clone();
        self.nav.confirm = None;
        self.send(LinkReq::Unsubscribe);
        // The layer fades the blur out over the game's next frames.
        self.game_blur = None;
        let back = self.report.return_to.clone();
        let rule = self.report.backdrop == Backdrop::Blur;
        if let Some(h) = self.hypr() {
            if rule {
                h.blur_off();
            }
            // Only from on screen: closing a hidden panel (the user is elsewhere) mustn't
            // pull focus back to the game.
            if shown
                && let Some(a) = &back
                && let Err(e) = h.focus(a)
            {
                tracing::info!("giving focus back to {a}: {e}");
            }
        }
        tracing::info!(session = %o.session, hidden = !shown, "panel closed");
        self.report.open = false;
        self.report.hidden = None;
        self.report.section = self.nav.section;
        self.report_now();
    }

    /// A session ended: its panel goes.
    pub fn ended(&mut self, session: &str, cx: &mut App) {
        if self.open.as_ref().is_some_and(|o| o.session == session) {
            self.close(cx);
        }
    }

    fn with_view(&self, cx: &mut App, f: impl FnOnce(&mut PanelView, &mut gpui::Context<PanelView>)) {
        if let Some(o) = &self.open {
            o.view.update(cx, |v, cx| {
                f(v, cx);
                cx.notify();
            });
        }
    }

    pub fn loaded(&mut self, session: &str, data: Loaded, cx: &mut App) {
        if self.open.as_ref().is_none_or(|o| o.session != session) {
            return;
        }
        self.with_view(cx, |v, _| v.loaded(data));
    }

    pub fn done(&mut self, change: crate::link::Change, result: Result<Option<quiver_api::options::OptionsChange>, String>, cx: &mut App) {
        self.with_view(cx, |v, _| v.done(change, result));
    }

    pub fn metrics(&mut self, u: quiver_api::perf::PerfUpdate, cx: &mut App) {
        if self.open.as_ref().is_none_or(|o| o.session != u.session) {
            return;
        }
        self.with_view(cx, |v, _| v.data.metrics = u.values);
    }

    pub fn options_changed(&mut self, session: &str) {
        if self.open.as_ref().is_some_and(|o| o.session == session) {
            self.send(LinkReq::Load { session: session.to_owned(), part: Part::Options, view: None });
        }
    }

    pub fn pad(&mut self, e: PadEvent, cx: &mut App) {
        match e {
            // Only on screen: a hidden panel's input isn't grabbed (the pad is the game's or
            // the desktop's again).
            PadEvent::Input(i) if self.is_shown() => self.with_view(cx, |v, cx| v.input(i, cx)),
            PadEvent::Scroll(_) | PadEvent::Input(_) => {}
            PadEvent::Chord => {
                tracing::info!("controller chord: toggling the panel");
                if self.is_open() {
                    self.close(cx);
                } else {
                    self.send(LinkReq::Toggle);
                }
            }
            PadEvent::Connected(n) => {
                tracing::info!("gamepad: {n}");
                self.pad_name = Some(n);
                self.report_now();
            }
            PadEvent::Disconnected => {
                self.pad_name = None;
                self.report_now();
            }
            PadEvent::Unavailable(why) => {
                self.pad_unavailable = Some(why);
                self.report_now();
            }
        }
    }

    /// Redraws the clock and play time, and refreshes what the section shows (called once a
    /// second while open).
    pub fn tick(&mut self, cx: &mut App) {
        if self.is_shown() {
            self.with_view(cx, |v, cx| v.tick(cx));
        }
    }

    /// quiverd's answer to the Performance section.
    pub fn perf_reply(&mut self, session: &str, reply: sections::performance::Reply, cx: &mut App) {
        if self.open.as_ref().is_none_or(|o| o.session != session) {
            return;
        }
        self.with_view(cx, |v, cx| v.perf_reply(reply, cx));
    }

    /// Feeds an input as if from the keyboard (tests, demo).
    pub fn input(&mut self, i: Input, cx: &mut App) {
        self.with_view(cx, |v, cx| v.input(i, cx));
    }

    /// Shows `data` without quiverd (the demo).
    pub fn open_with(&mut self, session: &str, section: Option<PanelSection>, data: PanelData, cx: &mut App) -> Result<()> {
        self.open(session, section, None, None, cx)?;
        self.with_view(cx, |v, _| v.data = data);
        Ok(())
    }
}
