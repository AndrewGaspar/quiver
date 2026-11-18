//! The Sight panel (design/SIGHT.md §6, §9; as built in §14): a full-output wlr-layer-shell
//! surface on the game's monitor (overlay layer, namespace [`NAMESPACE`], exclusive keyboard
//! focus while open), summoned with Shift+F2 in the game (read by the Sight layer), a
//! hyprpad chord or `quiver sight toggle`, and driven with the controller.
//!
//! - **Opening**: remembers the focused window (to give focus back on close) and finds the
//!   game's window (for Quit), asks Hyprland for the blur rule (the user's own
//!   `decoration:blur`; else the plain dim), opens the surface, and loads the session's data
//!   through the link. Live metrics are subscribed while it's open.
//! - **Input**: the keyboard (the surface has it), the mouse, and the controller two ways:
//!   hyprpad's `quiver-sight` mode types keys into the surface while the game's pad is held
//!   neutral (`tools/hyprpad/quiver-sight.lua`), and gilrs reads pads directly where it can
//!   (no hyprpad, or a pad hyprpad doesn't own). Both end up as the same [`Input`]s.
//! - **Closing**: B at the top, Escape, Shift+F2, the chord again: the surface goes, the blur
//!   rule is turned off, focus goes back to the game's window (Hyprland `focuswindow`).

pub mod data;
pub mod state;
pub mod view;

use anyhow::Result;
use futures::channel::mpsc::UnboundedSender;
use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
use gpui::{App, AppContext, Bounds, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px, size};
use quiver_api::sight::{Backdrop, PanelAction, PanelCommand, PanelReport, PanelSection};
use quiver_ui::gamepad::PadEvent;
use quiver_ui::input::Input;

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

struct Open {
    handle: WindowHandle<PanelView>,
    session: String,
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

    pub fn command(&mut self, c: PanelCommand, cx: &mut App) {
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
                    let _ = o.handle.update(cx, |v, _, cx| {
                        v.nav.select_section(s);
                        cx.notify();
                    });
                }
                return;
            }
            self.close(cx);
        }
        if let Err(e) = self.open(&session, c.section, c.monitor.as_deref(), cx) {
            tracing::warn!("panel: {e:#}");
            self.report.last_error = Some(format!("{e:#}"));
            self.report_now();
        }
    }

    fn open(&mut self, session: &str, section: Option<PanelSection>, monitor: Option<&str>, cx: &mut App) -> Result<()> {
        if !self.layer_shell {
            anyhow::bail!("no layer-shell here (not a Wayland session, or headless)");
        }
        self.start_pad();
        let namespace = self.namespace.clone();
        // Where the game is, before the panel takes focus.
        let (return_to, game_addr, mon, backdrop, detail) = {
            let h = self.hypr();
            match h {
                None => (None, None, None, Backdrop::Dim, "not Hyprland: plain dim".to_owned()),
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
                    let blur = h.blur();
                    let (backdrop, detail) = if !blur.enabled {
                        (Backdrop::Dim, "blur is off in Hyprland (decoration:blur:enabled): plain dim".to_owned())
                    } else {
                        match h.blur_on(&namespace) {
                            Ok(()) => (Backdrop::Blur, format!("Hyprland layer rule ({:?} syntax), your blur: size {} × {} passes", h.flavor, blur.size, blur.passes)),
                            Err(e) => (Backdrop::Dim, format!("the blur rule failed ({e}): plain dim")),
                        }
                    };
                    (active.map(|a| a.address), game, mon, backdrop, detail)
                }
            }
        };
        let logical = mon.as_ref().map(|m| m.logical()).unwrap_or((1920.0, 1080.0));
        let display_id = mon.as_ref().and_then(|m| crate::desktop::display_named(cx, &m.name));
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
        let opts = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(logical.0), px(logical.1)) })),
            app_id: Some("quiver-sight".into()),
            focus: true,
            window_background: WindowBackgroundAppearance::Transparent,
            display_id,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace,
                layer: Layer::Overlay,
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                // Over bars' exclusive zones too: the whole output is the panel's.
                exclusive_zone: Some(px(-1.)),
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                ..Default::default()
            }),
            ..Default::default()
        };
        let handle = cx.open_window(opts, move |window, cx| {
            cx.new(|cx| {
                let mut v = PanelView::new(sid, PanelData::default(), nav, backdrop, wiring, cx);
                v.footer = footer;
                window.focus(&v.focus, cx);
                v
            })
        })?;
        tracing::info!(session, monitor = ?mon.as_ref().map(|m| &m.name), backdrop = backdrop.as_str(), %detail, return_to = ?return_to, game_window = ?game_addr, "panel opened");
        self.open = Some(Open { handle, session: session.to_owned() });
        self.report = PanelReport {
            open: true,
            section: self.nav.section,
            session: Some(session.to_owned()),
            monitor: mon.map(|m| m.name),
            backdrop,
            backdrop_detail: detail,
            return_to,
            handled_seq: self.report.handled_seq,
            ..Default::default()
        };
        if let Some(s) = section {
            self.report.section = s;
        }
        self.report_now();
        self.send(LinkReq::Load { session: session.to_owned(), part: Part::All, view: None });
        self.send(LinkReq::Subscribe { session: session.to_owned() });
        Ok(())
    }

    pub fn close(&mut self, cx: &mut App) {
        let Some(o) = self.open.take() else { return };
        if let Ok(nav) = o.handle.update(cx, |v, w, _| {
            let nav = v.nav.clone();
            w.remove_window();
            nav
        }) {
            self.nav = nav;
            self.nav.confirm = None;
        }
        self.send(LinkReq::Unsubscribe);
        let back = self.report.return_to.clone();
        if let Some(h) = self.hypr() {
            h.blur_off();
            if let Some(a) = &back
                && let Err(e) = h.focus(a)
            {
                tracing::info!("giving focus back to {a}: {e}");
            }
        }
        tracing::info!(session = %o.session, "panel closed");
        self.report.open = false;
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
            let _ = o.handle.update(cx, |v, _, cx| {
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
            PadEvent::Input(i) if self.is_open() => self.with_view(cx, |v, cx| v.input(i, cx)),
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

    /// Redraws the clock and play time (called once a second while open).
    pub fn tick(&mut self, cx: &mut App) {
        self.with_view(cx, |_, _| {});
    }

    /// Feeds an input as if from the keyboard (tests, demo).
    pub fn input(&mut self, i: Input, cx: &mut App) {
        self.with_view(cx, |v, cx| v.input(i, cx));
    }

    /// Shows `data` without quiverd (the demo).
    pub fn open_with(&mut self, session: &str, section: Option<PanelSection>, data: PanelData, cx: &mut App) -> Result<()> {
        self.open(session, section, None, cx)?;
        self.with_view(cx, |v, _| v.data = data);
        Ok(())
    }
}
