//! The stereo UI (design/STEREO-UI.md): on a side-by-side stereo output (an XReal under
//! HypXRland's `monitor = …, stereo:sbs`, or a HypXRland XR monitor under its depth desktop)
//! the app draws itself once per eye into a buffer two windows wide, tags its toplevel
//! `stereo:sbs`, and offsets each element by the disparity of its depth: art at the back,
//! content surfaces a step up, controls on them a step more, focus and hover more again,
//! overlays above everything (`quiver_ui::depth`).
//!
//! Settings → Display → "3D on stereo displays": **Auto** (the default) packs whenever the
//! window's monitor is a stereo output and stays mono elsewhere; **Off** never packs. Its
//! depth strength is 50, 100 (the default), 150 or 200%, `sight.depth`'s vocabulary. Both are
//! kept in `~/.config/quiver/app.json` (`QUIVER_APP_CONFIG` overrides the path).
//!
//! Environment overrides, for testing:
//! - `QUIVER_STEREO_UI`: `sbs`/`auto` as Auto, `sbs-always`/`always` packs regardless of the
//!   monitor (any compositor), `off`/`mono` never.
//! - `QUIVER_STEREO_UI_STRENGTH`: a percent, 0..200 (`off` is 0).
//! - `QUIVER_STEREO_UI_TAG`: `both` (default) also sets the Hyprland window tag `stereo-sbs`,
//!   which the existing `stereo-<layout>` tag rules map to `stereo sbs always`; `xdg` sends
//!   only the `xdg-toplevel-tag-v1` tag `stereo:sbs` (which needs
//!   `hl.window_rule({ name = "stereo-xdg-tag", stereo = "auto", match = { xdg_tag = "^stereo:" } })`).
//!
//! Depth is HypXRland's unitless rung (0..1 above the window's plane), the vocabulary
//! `quiver_perf::stereo` uses for Sight. Per-eye pixels come from HypXRland's own formula with
//! its defaults, rounded to whole device pixels so text and art stay pixel-exact in each eye.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use gpui::{Context, Window};
use serde::{Deserialize, Serialize};

use crate::app::Quiver;

pub use quiver_ui::depth;

pub const ENV: &str = "QUIVER_STEREO_UI";
pub const ENV_STRENGTH: &str = "QUIVER_STEREO_UI_STRENGTH";
pub const ENV_TAG: &str = "QUIVER_STEREO_UI_TAG";
pub const ENV_CONFIG: &str = "QUIVER_APP_CONFIG";

/// The strengths Settings offers, percent (`sight.depth`'s).
pub const STRENGTHS: [u32; 4] = [50, 100, 150, 200];

/// "3D on stereo displays".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Setting {
    /// Pack while the window's monitor is a stereo output.
    #[default]
    Auto,
    /// Never pack.
    Off,
}

impl Setting {
    pub fn label(self) -> &'static str {
        match self {
            Setting::Auto => "Auto",
            Setting::Off => "Off",
        }
    }
}

/// What actually decides packing: the setting, or the environment's override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Pack while the window's monitor is a stereo output.
    Auto,
    /// Pack regardless of the monitor.
    Always,
}

impl Mode {
    /// `QUIVER_STEREO_UI`'s spellings; `None` for anything else (no override).
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "sbs" | "auto" => Some(Mode::Auto),
            "sbs-always" | "always" => Some(Mode::Always),
            "off" | "mono" | "0" => Some(Mode::Off),
            _ => None,
        }
    }

    pub fn from_env() -> Option<Mode> {
        std::env::var(ENV).ok().and_then(|v| Mode::parse(&v))
    }
}

/// The app's own preferences file (`~/.config/quiver/app.json`): settings that are about this
/// app and machine, not about games (those live in quiverd).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// "3D on stereo displays".
    pub stereo_ui: Setting,
    /// Its depth strength, percent (0..=200).
    pub stereo_depth: u32,
    /// Where a game's Play goes, by game ref (`ubisoft:3539`): `here`, or a paired machine's
    /// id to stream from (`ui/machines.rs`). Games not listed play here when they can.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub play_on: std::collections::BTreeMap<String, String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { stereo_ui: Setting::Auto, stereo_depth: 100, play_on: Default::default() }
    }
}

pub fn prefs_path() -> PathBuf {
    if let Some(p) = std::env::var_os(ENV_CONFIG).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return p;
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("quiver/app.json")
}

impl Prefs {
    pub fn load() -> Prefs {
        let path = prefs_path();
        let Ok(text) = std::fs::read_to_string(&path) else { return Prefs::default() };
        match serde_json::from_str::<Prefs>(&text) {
            Ok(mut p) => {
                p.stereo_depth = p.stereo_depth.min(quiver_perf::stereo::DEPTH_MAX_PERCENT);
                p
            }
            Err(e) => {
                tracing::warn!("ignoring {}: {e}", path.display());
                Prefs::default()
            }
        }
    }

    /// Writes the file, keeping keys this version doesn't know.
    pub fn save(&self) -> Result<(), String> {
        let path = prefs_path();
        let mut doc: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if let serde_json::Value::Object(mine) = serde_json::to_value(self).map_err(|e| e.to_string())? {
            doc.extend(mine);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text + "\n").map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// `QUIVER_STEREO_UI_STRENGTH`: a percent, 0..=200, or `off`.
pub fn strength_from_env() -> Option<u32> {
    let v = std::env::var(ENV_STRENGTH).ok()?;
    match quiver_perf::stereo::DepthSetting::parse(Some(&v)) {
        quiver_perf::stereo::DepthSetting::Percent(p) => Some(p as u32),
        _ => None,
    }
}

/// The monitor the window is on, as far as stereo goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub name: String,
    /// It presents two side-by-side panes (`"stereo": "sbs"`).
    pub stereo: bool,
    /// One pane's logical width (Hyprland's `width` is the pane on a stereo output).
    pub pane_width: f32,
    pub scale: f32,
    /// Hyprland's `transform`: HypXRland turns depth off on a rotated or flipped output
    /// (disparity would run vertically or backwards), and so does the app.
    pub transform: i64,
}

impl Output {
    /// It's a stereo output the app can draw depth on.
    pub fn packs(&self) -> bool {
        self.stereo && self.transform == 0
    }

    /// The pane's width in device pixels.
    pub fn pane_px(&self) -> f32 {
        (self.pane_width * self.scale).round()
    }
}

#[derive(Debug, Deserialize)]
struct HyprClient {
    #[serde(default)]
    address: String,
    #[serde(default)]
    pid: i64,
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default)]
    monitor: i64,
    #[serde(default, rename = "focusHistoryID")]
    focus_history: Option<i64>,
    /// Logical size, `[w, h]`.
    #[serde(default)]
    size: Option<[f64; 2]>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct HyprMonitor {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    width: f64,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    transform: i64,
    /// Present (`"sbs"`) only on a stereo output (HypXRland's `HyprCtl.cpp`).
    #[serde(default)]
    stereo: Option<String>,
}

fn one() -> f64 {
    1.0
}

/// Whether any monitor in `hyprctl -j monitors` is a stereo output.
pub fn any_stereo(monitors: &str) -> bool {
    serde_json::from_str::<Vec<HyprMonitor>>(monitors)
        .map(|ms| ms.iter().any(|m| m.stereo.as_deref() == Some("sbs")))
        .unwrap_or(false)
}

/// Our window and its monitor, from `hyprctl -j clients` and `-j monitors`: the client with
/// our pid; with several (more than one window), the one whose logical size is nearest
/// `size` (the window's own), then the most recently focused.
pub fn our_output(clients: &str, monitors: &str, pid: u32, size: Option<(f32, f32)>) -> Option<(String, Output)> {
    let clients: Vec<HyprClient> = serde_json::from_str(clients).ok()?;
    let off = |c: &HyprClient| match (size, c.size) {
        (Some((w, h)), Some([cw, ch])) => ((cw as f32 - w).abs() + (ch as f32 - h).abs()).round() as i64,
        _ => 0,
    };
    let me = clients
        .into_iter()
        .filter(|c| c.pid == pid as i64 && c.mapped && !c.address.is_empty())
        .min_by_key(|c| (off(c), c.focus_history.unwrap_or(i64::MAX)))?;
    let monitors: Vec<HyprMonitor> = serde_json::from_str(monitors).ok()?;
    let m = monitors.into_iter().find(|m| m.id == me.monitor)?;
    let stereo = m.stereo.as_deref().is_some_and(|s| s == "sbs");
    Some((
        me.address,
        Output { name: m.name, stereo, pane_width: m.width as f32, scale: m.scale as f32, transform: m.transform },
    ))
}

/// Hyprland events after which the window's monitor (or whether it is stereo) may differ.
pub fn relevant_event(line: &str) -> bool {
    let name = line.split_once(">>").map_or(line, |(n, _)| n);
    matches!(
        name,
        "monitoradded"
            | "monitoraddedv2"
            | "monitorremoved"
            | "monitorremovedv2"
            | "movewindow"
            | "movewindowv2"
            | "moveworkspace"
            | "moveworkspacev2"
            | "workspace"
            | "workspacev2"
            | "focusedmon"
            | "focusedmonv2"
            | "openwindow"
            | "configreloaded"
            | "fullscreen"
            | "changefloatingmode"
    )
}

fn hypr_socket(name: &str) -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty())?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty())?;
    Some(PathBuf::from(runtime).join("hypr").join(sig).join(name))
}

/// A read-only Hyprland query (`j/monitors`, `j/clients`) straight over its request socket,
/// as `hyprctl -j` sends it, without starting a process every few seconds; `hyprctl` if the
/// socket doesn't answer.
fn hypr_query(what: &str) -> Result<String, String> {
    let direct = (|| -> std::io::Result<String> {
        let path = hypr_socket(".socket.sock").ok_or_else(|| std::io::Error::other("not Hyprland"))?;
        let mut s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.set_write_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(format!("j/{what}").as_bytes())?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    })();
    match direct {
        Ok(out) if out.trim_start().starts_with('[') => Ok(out),
        _ => crate::selfupdate::hypr::hyprctl(&["-j", what]),
    }
}

/// What a look at Hyprland found.
#[derive(Clone, Debug, PartialEq)]
pub enum Probe {
    /// Hyprland didn't answer (or our window isn't mapped yet).
    Unknown,
    /// No monitor is a stereo output (so nobody needs to know which one we're on).
    NoStereo,
    /// Our window's address and monitor.
    Ours(String, Output),
}

fn probe(pid: u32, size: Option<(f32, f32)>) -> Probe {
    let Ok(monitors) = hypr_query("monitors") else { return Probe::Unknown };
    if !any_stereo(&monitors) {
        return Probe::NoStereo;
    }
    let Ok(clients) = hypr_query("clients") else { return Probe::Unknown };
    match our_output(&clients, &monitors, pid, size) {
        Some((addr, out)) => Probe::Ours(addr, out),
        None => Probe::Unknown,
    }
}

/// The Hyprland dispatches that add (`+`) or remove (`-`) a window tag: the Lua front end's
/// form, then the classic one's.
fn tag_dispatches(sign: char, tag: &str, address: &str) -> [String; 2] {
    [
        format!("hl.dsp.window.tag({{ tag = \"{sign}{tag}\", window = \"address:{address}\" }})"),
        format!("tagwindow {sign}{tag} address:{address}"),
    ]
}

fn set_hypr_tag(sign: char, address: &str) -> Result<(), String> {
    let mut replies = Vec::new();
    for d in tag_dispatches(sign, "stereo-sbs", address) {
        let reply = crate::selfupdate::hypr::hyprctl(&["dispatch", &d])?;
        if reply.trim() == "ok" {
            return Ok(());
        }
        replies.push(reply.trim().lines().next().unwrap_or_default().to_owned());
    }
    Err(replies.join(" / "))
}

/// What packing should be, given the mode, what Hyprland said, and the window's scale (for
/// `Always` off a known monitor): the pane width in device pixels, or `None` for mono.
pub fn wanted_pane(mode: Mode, probe: &Probe, window_scale: f32) -> Option<f32> {
    match (mode, probe) {
        (Mode::Off, _) => None,
        (Mode::Always, Probe::Ours(_, out)) if out.stereo => Some(out.pane_px()),
        (Mode::Always, _) => Some((1920.0 * window_scale).round()),
        (Mode::Auto, Probe::Ours(_, out)) if out.packs() => Some(out.pane_px()),
        (Mode::Auto, _) => None,
    }
}

/// The app's stereo state.
#[derive(Default)]
pub struct StereoUi {
    pub prefs: Prefs,
    /// `QUIVER_STEREO_UI`, if set to something it knows.
    pub env_mode: Option<Mode>,
    /// `QUIVER_STEREO_UI_STRENGTH`, if set.
    pub env_strength: Option<u32>,
    pub packed: bool,
    /// The pane width (device pixels) and strength the current packing was made for.
    pane_px: f32,
    percent: u32,
    /// The window the Hyprland tag is on.
    hypr_tagged: Option<String>,
    /// Whether to set the Hyprland window tag too.
    hypr_tag: bool,
    /// What the last look at Hyprland found (Settings shows it).
    pub last: Option<Probe>,
    /// Wakes the detection task (settings changed, or something to look at again).
    wake: Option<futures::channel::mpsc::UnboundedSender<()>>,
}

impl StereoUi {
    pub fn mode(&self) -> Mode {
        self.env_mode.unwrap_or(match self.prefs.stereo_ui {
            Setting::Auto => Mode::Auto,
            Setting::Off => Mode::Off,
        })
    }

    pub fn percent(&self) -> u32 {
        self.env_strength.unwrap_or(self.prefs.stereo_depth)
    }

    fn wake(&self) {
        if let Some(tx) = &self.wake {
            let _ = tx.unbounded_send(());
        }
    }
}

impl Quiver {
    /// Sets the window's depth rules, reads the setting and starts following the window's
    /// monitor (see the module docs).
    pub fn start_stereo_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.set_stereo_policy(depth::policy());
        self.stereo.prefs = Prefs::load();
        self.stereo.env_mode = Mode::from_env();
        self.stereo.env_strength = strength_from_env();
        self.stereo.hypr_tag = std::env::var(ENV_TAG).map(|v| v.trim() != "xdg").unwrap_or(true);
        let hyprland = hypr_socket(".socket.sock").is_some();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        // Hyprland's event socket, read on a thread: any event that can move the window to
        // another monitor (or change a monitor) wakes the check.
        if let Some(path) = hypr_socket(".socket2.sock") {
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("stereo-ui-events".into())
                .spawn(move || {
                    let Ok(s) = UnixStream::connect(&path) else { return };
                    for line in BufReader::new(s).lines() {
                        let Ok(line) = line else { break };
                        if relevant_event(&line) && tx.unbounded_send(()).is_err() {
                            break;
                        }
                    }
                })
                .ok();
        }
        let _ = tx.unbounded_send(());
        self.stereo.wake = Some(tx);
        // The window entering another output (`wl_surface.enter`), or changing size (fullscreen,
        // maximise; the size also tells this window from another of ours): look again now
        // rather than at the next Hyprland event or tick.
        cx.observe_window_display(window, |q, _, _| q.stereo.wake()).detach();
        cx.observe_window_bounds(window, |q, _, _| q.stereo.wake()).detach();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                // Coalesce a burst of events, and look again now and then anyway (a `stereo`
                // change doesn't always come with an event).
                let tick = cx.background_executor().timer(Duration::from_secs(5));
                futures::select_biased! {
                    e = rx.next() => if e.is_none() { break },
                    _ = futures::FutureExt::fuse(tick) => {},
                }
                cx.background_executor().timer(Duration::from_millis(120)).await;
                while rx.try_recv().is_ok() {}
                let Ok((mode, size)) = this.update_in(cx, |q, window, _| {
                    let s = window.viewport_size();
                    (q.stereo.mode(), Some((f32::from(s.width), f32::from(s.height))))
                }) else {
                    break;
                };
                // Off: nothing to ask Hyprland. Not Hyprland: nothing to ask either.
                let found = if mode == Mode::Off || !hyprland {
                    Probe::Unknown
                } else {
                    let pid = std::process::id();
                    cx.background_executor().spawn(async move { probe(pid, size) }).await
                };
                if this.update_in(cx, |q, window, cx| q.apply_stereo(found, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_stereo(&mut self, found: Probe, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.stereo.mode();
        // Hyprland not answering keeps the current state rather than flapping.
        if found == Probe::Unknown && mode == Mode::Auto && hypr_socket(".socket.sock").is_some() {
            return;
        }
        let percent = self.stereo.percent();
        let want = wanted_pane(mode, &found, window.scale_factor());
        let name = match &found {
            Probe::Ours(_, out) => out.name.clone(),
            _ => "this monitor".into(),
        };
        match want {
            Some(pane) if !self.stereo.packed || self.stereo.pane_px != pane || self.stereo.percent != percent => {
                let ok = window.set_stereo_packing(Some(depth::packing(pane, percent)));
                self.stereo.packed = ok;
                self.stereo.pane_px = pane;
                self.stereo.percent = percent;
                tracing::info!("stereo ui: {name} is stereo (pane {pane} px); packing side by side at {percent}%: {ok}");
            }
            None if self.stereo.packed => {
                window.set_stereo_packing(None);
                self.stereo.packed = false;
                tracing::info!("stereo ui: {name} is mono (or 3D is off); packing off");
            }
            _ => {}
        }
        // The Hyprland window tag follows the packing (the xdg tag is sent by the window,
        // with the first buffer of the new width).
        let address = match &found {
            Probe::Ours(a, _) => Some(a.clone()),
            _ => None,
        };
        let tagged_here = address.is_some() && self.stereo.hypr_tagged == address;
        if self.stereo.hypr_tag && self.stereo.packed && !tagged_here && let Some(addr) = address {
            if let Some(old) = self.stereo.hypr_tagged.replace(addr.clone()) {
                Self::hypr_tag_later('-', old, cx);
            }
            Self::hypr_tag_later('+', addr, cx);
        } else if !self.stereo.packed && let Some(addr) = self.stereo.hypr_tagged.take() {
            Self::hypr_tag_later('-', addr, cx);
        }
        self.stereo.last = Some(found);
        cx.notify();
    }

    fn hypr_tag_later(sign: char, address: String, cx: &mut Context<Self>) {
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = set_hypr_tag(sign, &address) {
                    tracing::warn!("stereo ui: Hyprland window tag: {e}");
                }
            })
            .detach();
    }

    /// Settings → Display: "3D on stereo displays".
    pub fn set_stereo_setting(&mut self, s: Setting, cx: &mut Context<Self>) {
        self.stereo.prefs.stereo_ui = s;
        self.save_stereo_prefs(cx);
    }

    /// Settings → Display: the depth strength, percent.
    pub fn set_stereo_strength(&mut self, percent: u32, cx: &mut Context<Self>) {
        self.stereo.prefs.stereo_depth = percent.min(quiver_perf::stereo::DEPTH_MAX_PERCENT);
        self.save_stereo_prefs(cx);
    }

    fn save_stereo_prefs(&mut self, cx: &mut Context<Self>) {
        // Demo mode doesn't write the person's settings.
        if !self.demo
            && let Err(e) = self.stereo.prefs.save()
        {
            self.show_toast(format!("Couldn't save the display settings: {e}"), cx);
        }
        self.stereo.wake();
        cx.notify();
    }

    /// One line for Settings: what the window is on and whether it's drawing in 3D.
    pub fn stereo_status(&self) -> String {
        let s = &self.stereo;
        let env = match s.env_mode {
            Some(Mode::Always) => " (QUIVER_STEREO_UI forces it on)",
            Some(Mode::Off) => " (QUIVER_STEREO_UI turns it off)",
            Some(Mode::Auto) | None => "",
        };
        if s.packed {
            let on = match &s.last {
                Some(Probe::Ours(_, out)) => format!("on {}, a stereo display", out.name),
                _ => "packed side by side".into(),
            };
            return format!("In 3D now: {on}, at {}%{env}.", s.percent);
        }
        match (s.mode(), &s.last) {
            (Mode::Off, _) => format!("Off: the app stays flat on every display{env}."),
            (_, Some(Probe::Ours(_, out))) if out.stereo && out.transform != 0 => {
                format!("{} is a stereo display, but rotated: depth stays off there.", out.name)
            }
            (_, Some(Probe::Ours(_, out))) => format!("{} is a flat display: the app is flat there.", out.name),
            (_, Some(Probe::NoStereo)) => "No stereo display is connected: the app is flat.".into(),
            _ if hypr_socket(".socket.sock").is_none() => "Needs Hyprland (HypXRland) to tell a stereo display apart: flat for now.".into(),
            _ => "Looking for a stereo display…".into(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Evidence: packed frames rendered off-screen

/// `QUIVER_STEREO_SHOT=DIR` (with `QUIVER_DEMO=1`): renders the app off-screen (no compositor,
/// no daemon) and writes PNGs of the mono and packed frames to DIR, plus each depth scope's
/// bounds and per-eye shifts as JSON for `tools/stereo-evidence.py` and timings
/// (design/STEREO-UI.md, "Evidence"). The demo flags (`QUIVER_DEMO_GLOBAL`, `QUIVER_DEMO_INPUT`)
/// pick the screen.
///
/// - `QUIVER_STEREO_SHOT_NAME=library`: the files' prefix.
/// - `QUIVER_STEREO_SHOT_SIZE=1920x1080`: the window's logical size.
/// - `QUIVER_STEREO_SHOT_SCALE=1`: its scale factor (1.25, 1.5 for fractional scales).
/// - `QUIVER_STEREO_SHOT_WAIT=5000`: ms to wait after the library is up (art, demo flags).
/// - `QUIVER_STEREO_SHOT_HOVER=X,Y`: move the pointer there (logical px) before the shots.
/// - `QUIVER_STEREO_SHOT_STRENGTHS=100`: comma-separated percents to shoot (`100,200`).
/// - `QUIVER_STEREO_SHOT_TIME=1`: also time mono and packed frames (30 each) and packing.
/// - `QUIVER_STEREO_SHOT_PANE=1920`: the stereo pane's width in device px (the monitor's eye, not
///   the window).
pub mod shot {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use gpui::{App, AppContext, OffscreenWindowOptions, StereoPacking, px, size};

    use super::depth;
    use crate::app::Quiver;

    #[derive(Default, Clone)]
    struct Frame {
        w: u32,
        h: u32,
        stride: u32,
        px: Vec<u8>,
        render: Duration,
    }

    fn save(f: &Frame, path: &Path) {
        let img = image::RgbaImage::from_fn(f.w, f.h, |x, y| {
            let p = &f.px[(y * f.stride + x * 4) as usize..][..4];
            image::Rgba([p[0], p[1], p[2], 255])
        });
        img.save(path).unwrap();
        println!("{} ({}x{}, rendered in {:?})", path.display(), f.w, f.h, f.render);
    }

    fn env<T: std::str::FromStr>(k: &str) -> Option<T> {
        std::env::var(k).ok().and_then(|v| v.trim().parse().ok())
    }

    pub fn run(dir: PathBuf) {
        let (w, h) = std::env::var("QUIVER_STEREO_SHOT_SIZE")
            .ok()
            .and_then(|v| {
                let (a, b) = v.split_once('x')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((1920.0f32, 1080.0f32));
        let scale: f32 = env("QUIVER_STEREO_SHOT_SCALE").unwrap_or(1.0);
        let name = std::env::var("QUIVER_STEREO_SHOT_NAME").unwrap_or_else(|_| "library".into());
        let wait: u64 = env("QUIVER_STEREO_SHOT_WAIT").unwrap_or(5000);
        let hover: Option<(f32, f32)> = std::env::var("QUIVER_STEREO_SHOT_HOVER").ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        });
        let strengths: Vec<u32> = std::env::var("QUIVER_STEREO_SHOT_STRENGTHS")
            .ok()
            .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
            .unwrap_or_else(|| vec![100]);
        let time = std::env::var("QUIVER_STEREO_SHOT_TIME").is_ok_and(|v| v == "1");
        std::fs::create_dir_all(&dir).unwrap();
        gpui_platform::headless().with_assets(crate::assets::Assets).run(move |cx: &mut App| {
            crate::assets::register_fonts(cx);
            cx.set_global(crate::theme::load(&crate::theme::omarchy_state_dir()));
            crate::text_input::bind_keys(cx);
            crate::app::bind_keys(cx);
            let frame: Rc<RefCell<Frame>> = Rc::default();
            let sink = {
                let f = frame.clone();
                move |o: &gpui::OffscreenFrame<'_>| {
                    *f.borrow_mut() = Frame {
                        w: o.width,
                        h: o.height,
                        stride: o.stride,
                        px: o.data.to_vec(),
                        render: o.render_duration,
                    }
                }
            };
            let opts = OffscreenWindowOptions {
                size: size(px(w), px(h)),
                scale_factor: scale,
                prefer_low_power_gpu: false,
                ..Default::default()
            };
            let handle = cx
                .open_offscreen_window(opts, sink, |window, cx| cx.new(|cx| Quiver::new(true, window, cx)))
                .expect("off-screen window");
            let any: gpui::AnyWindowHandle = handle.into();
            cx.spawn(async move |cx| {
                // The library (or the sign-in/welcome screen), then its art and the demo's
                // flags and inputs, then a quiet moment.
                for _ in 0..100 {
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                    let ready = handle
                        .update(cx, |q, _, _| !q.layout.grid.is_empty() || !matches!(q.conn, crate::app::Conn::Ready | crate::app::Conn::Connecting))
                        .unwrap_or(false);
                    if ready {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_millis(wait)).await;
                if let Some((x, y)) = hover {
                    let _ = cx.update_window(any, |_, win, cx| {
                        let ev = gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position: gpui::point(px(x), px(y)),
                            pressed_button: None,
                            modifiers: Default::default(),
                        });
                        win.dispatch_event(ev, cx);
                    });
                    cx.background_executor().timer(Duration::from_millis(1500)).await;
                }
                let render = |cx: &mut gpui::AsyncApp| {
                    let _ = cx.update_window(any, |_, win, cx| {
                        win.refresh();
                        win.render_now(cx)
                    });
                    frame.borrow().clone()
                };
                let set = |cx: &mut gpui::AsyncApp, p: Option<StereoPacking>| {
                    let _ = cx.update_window(any, |_, win, _| win.set_stereo_packing(p));
                };
                // The monitor's pane, not the window: a window narrower than the pane gets the same
                // disparities per depth as a full-screen one.
                let pane: f32 = env("QUIVER_STEREO_SHOT_PANE").unwrap_or(1920.0);
                let timed = |cx: &mut gpui::AsyncApp, label: &str| {
                    render(cx);
                    let mut gpu = Vec::new();
                    let mut wall = Vec::new();
                    let before = cx.update_window(any, |_, win, _| win.stereo_stats()).unwrap();
                    for _ in 0..30 {
                        let t = Instant::now();
                        let f = render(cx);
                        wall.push(t.elapsed());
                        gpu.push(f.render);
                    }
                    let after = cx.update_window(any, |_, win, _| win.stereo_stats()).unwrap();
                    gpu.sort();
                    wall.sort();
                    let packs = after.packed - before.packed;
                    let pack = if packs > 0 { (after.total_pack - before.total_pack) / packs as u32 } else { Duration::ZERO };
                    println!(
                        "{label}: render+readback p50 {:?} p90 {:?}; render_now (layout+paint+pack+render+readback) p50 {:?} p90 {:?}; pack mean {:?} over {packs} ({} primitives, {} scopes)",
                        gpu[15], gpu[27], wall[15], wall[27], pack, after.primitives, after.scopes
                    );
                };

                set(cx, None);
                save(&render(cx), &dir.join(format!("{name}-mono.png")));
                if time {
                    timed(cx, "mono");
                }
                for (i, &percent) in strengths.iter().enumerate() {
                    set(cx, Some(depth::packing(pane, percent)));
                    let f = render(cx);
                    let suffix = if i == 0 { String::new() } else { format!("-{percent}") };
                    save(&f, &dir.join(format!("{name}-sbs{suffix}.png")));
                    let (scopes, shifts) =
                        cx.update_window(any, |_, win, _| (win.stereo_scopes(), win.stereo_scope_shifts())).unwrap_or_default();
                    write_scopes(&dir.join(format!("{name}-scopes{suffix}.json")), &scopes, &shifts, pane, scale, percent, h);
                    if time && i == 0 {
                        timed(cx, "sbs");
                    }
                }
                set(cx, None);
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }

    /// Each scope: its depth, its element's bounds (device px) and the two eyes' shifts as the
    /// window packed them (after the edge clamp).
    fn write_scopes(path: &Path, scopes: &[gpui::StereoScope], shifts: &[[f32; 2]], pane: f32, scale: f32, percent: u32, h: f32) {
        let mut out = Vec::new();
        for (i, s) in scopes.iter().enumerate() {
            let bounds = s.bounds.map(|b| [b.origin.x.0, b.origin.y.0, b.size.width.0, b.size.height.0]);
            let shift = shifts.get(i).copied().unwrap_or([0.0, 0.0]);
            let [l, r] = depth::eye_shifts(s.depth, pane, percent);
            out.push(serde_json::json!({ "index": i, "depth": s.depth, "bounds": bounds, "shift": shift, "unclamped": [l, r] }));
        }
        let doc = serde_json::json!({ "eye_width": pane, "height": (h * scale).round(), "scale": scale, "percent": percent, "scopes": out });
        std::fs::write(path, serde_json::to_string(&doc).unwrap()).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(Mode::parse("sbs"), Some(Mode::Auto));
        assert_eq!(Mode::parse("sbs-always"), Some(Mode::Always));
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse(""), None);
        assert_eq!(Mode::parse("hsbs"), None);
    }

    #[test]
    fn prefs_round_trip_and_keep_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quiver/app.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"someday": true, "stereo_ui": "off"}"#).unwrap();
        // SAFETY: this test is the only one touching the variable.
        unsafe { std::env::set_var(ENV_CONFIG, &path) };
        let p = Prefs::load();
        assert_eq!(p, Prefs { stereo_ui: Setting::Off, stereo_depth: 100, ..Default::default() });
        Prefs { stereo_ui: Setting::Auto, stereo_depth: 150, ..Default::default() }.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"someday\": true"), "{text}");
        assert_eq!(Prefs::load(), Prefs { stereo_ui: Setting::Auto, stereo_depth: 150, ..Default::default() });
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Prefs::load(), Prefs::default());
        unsafe { std::env::remove_var(ENV_CONFIG) };
    }

    const CLIENTS: &str = r#"[
        {"address": "0xa", "pid": 10, "mapped": true, "monitor": 0, "focusHistoryID": 2},
        {"address": "0xb", "pid": 42, "mapped": true, "monitor": 1, "focusHistoryID": 1}
    ]"#;

    const MONITORS: &str = r#"[
        {"id": 0, "name": "eDP-2", "width": 2560, "scale": 1.25, "transform": 0},
        {"id": 1, "name": "DP-5", "width": 1920, "scale": 1.0, "transform": 0, "stereo": "sbs",
         "scanoutWidth": 3840, "scanoutHeight": 1080, "stereoComposites": 2, "stereoContent": false}
    ]"#;

    #[test]
    fn finds_the_stereo_monitor() {
        let (addr, out) = our_output(CLIENTS, MONITORS, 42, None).unwrap();
        assert_eq!(addr, "0xb");
        assert!(out.packs());
        assert_eq!(out.pane_px(), 1920.0);
        let (_, out) = our_output(CLIENTS, MONITORS, 10, None).unwrap();
        assert!(!out.stereo);
        assert!(our_output(CLIENTS, MONITORS, 7, None).is_none());
        assert!(any_stereo(MONITORS));
        assert!(!any_stereo(r#"[{"id": 0, "name": "eDP-2", "width": 2560}]"#));
        // Rotated: HypXRland turns depth off there, so does the app.
        let rotated = MONITORS.replace(r#""transform": 0, "stereo""#, r#""transform": 1, "stereo""#);
        let (_, out) = our_output(CLIENTS, &rotated, 42, None).unwrap();
        assert!(out.stereo && !out.packs());
    }

    /// Two windows of one process on different monitors: each finds its own by its size.
    #[test]
    fn several_windows() {
        let clients = r#"[
            {"address": "0xc", "pid": 42, "mapped": true, "monitor": 0, "focusHistoryID": 0, "size": [800, 600]},
            {"address": "0xb", "pid": 42, "mapped": true, "monitor": 1, "focusHistoryID": 3, "size": [1920, 1080]}
        ]"#;
        let (addr, out) = our_output(clients, MONITORS, 42, Some((1920.0, 1080.0))).unwrap();
        assert_eq!((addr.as_str(), out.stereo), ("0xb", true));
        let (addr, out) = our_output(clients, MONITORS, 42, Some((800.0, 600.0))).unwrap();
        assert_eq!((addr.as_str(), out.stereo), ("0xc", false));
        // Without a size: the most recently focused.
        assert_eq!(our_output(clients, MONITORS, 42, None).unwrap().0, "0xc");
    }

    #[test]
    fn what_to_pack() {
        let (addr, stereo) = our_output(CLIENTS, MONITORS, 42, None).unwrap();
        let (_, flat) = our_output(CLIENTS, MONITORS, 10, None).unwrap();
        let on = Probe::Ours(addr.clone(), stereo.clone());
        let off = Probe::Ours(addr.clone(), flat);
        assert_eq!(wanted_pane(Mode::Auto, &on, 1.0), Some(1920.0));
        assert_eq!(wanted_pane(Mode::Auto, &off, 1.0), None, "moved to a flat monitor: mono");
        assert_eq!(wanted_pane(Mode::Auto, &Probe::NoStereo, 1.0), None);
        assert_eq!(wanted_pane(Mode::Off, &on, 1.0), None, "the setting wins");
        assert_eq!(wanted_pane(Mode::Always, &Probe::Unknown, 1.5), Some(2880.0));
        assert_eq!(wanted_pane(Mode::Always, &on, 1.5), Some(1920.0), "the monitor's pane when known");
        // A fractional scale on the stereo output: the pane in device pixels.
        let scaled = Probe::Ours(addr, Output { scale: 1.25, ..stereo });
        assert_eq!(wanted_pane(Mode::Auto, &scaled, 1.25), Some(2400.0));
    }

    #[test]
    fn events_and_dispatches() {
        assert!(relevant_event("movewindowv2>>0xb,3,3"));
        assert!(relevant_event("monitoradded>>DP-5"));
        assert!(!relevant_event("activewindow>>kitty,~"));
        let [lua, classic] = tag_dispatches('+', "stereo-sbs", "0xb");
        assert_eq!(lua, r#"hl.dsp.window.tag({ tag = "+stereo-sbs", window = "address:0xb" })"#);
        assert_eq!(classic, "tagwindow +stereo-sbs address:0xb");
    }

    /// The packed frame, built from a real scene: each depth moved per eye, masks clipped to
    /// the eye, shifts kept off the eye's edges, both eyes' copies in draw order.
    #[test]
    fn packing_moves_each_scope_per_eye() {
        use gpui::{Bounds, ContentMask, Quad, Scene, ScaledPixels, point, size};
        let b = |x: f32, w: f32| Bounds { origin: point(ScaledPixels(x), ScaledPixels(10.0)), size: size(ScaledPixels(w), ScaledPixels(20.0)) };
        let full = ContentMask { bounds: b(0.0, 100.0) };
        let mut scene = Scene::default();
        scene.insert_primitive(Quad { bounds: b(0.0, 100.0), content_mask: full.clone(), ..Default::default() });
        // A focused control in the middle (0.6: 4 px) and one flush with the right edge.
        let prev = scene.push_stereo_scope(0.6, Some(b(40.0, 10.0)));
        scene.insert_primitive(Quad { bounds: b(40.0, 10.0), content_mask: full.clone(), ..Default::default() });
        scene.pop_stereo_scope(prev);
        let prev = scene.push_stereo_scope(1.0, Some(b(90.0, 10.0)));
        scene.insert_primitive(Quad { bounds: b(90.0, 10.0), content_mask: full.clone(), ..Default::default() });
        scene.pop_stereo_scope(prev);
        scene.finish();
        let p = depth::packing(1920.0, 100);
        let packed = scene.pack_side_by_side(size(ScaledPixels(100.0), ScaledPixels(40.0)), &*p.shift, p.edge_slack);
        let xs: Vec<(f32, f32, f32)> = packed
            .quads
            .iter()
            .map(|q| (q.bounds.origin.x.0, q.content_mask.bounds.origin.x.0, q.content_mask.bounds.size.width.0))
            .collect();
        assert_eq!(
            xs,
            vec![
                // The background, at 0, fills each eye exactly.
                (0.0, 0.0, 100.0),
                (100.0, 100.0, 100.0),
                // 0.6: +2 / -2, its mask moved with it and clipped to its eye.
                (42.0, 2.0, 98.0),
                (138.0, 100.0, 98.0),
                // 1.0 would be +4 / -3, but it's flush with the right edge: the left eye's
                // move toward that edge is held to the 2 px slack.
                (92.0, 2.0, 98.0),
                (187.0, 100.0, 97.0),
            ]
        );
        // Draw orders: each primitive's two copies side by side, in order.
        assert!(packed.quads.windows(2).all(|w| w[0].order <= w[1].order));
        // The window gets the shifts as packed, by scope.
        assert_eq!(packed.packed_scope_shifts(), &[[0.0, 0.0], [2.0, -2.0], [2.0, -3.0]]);
    }

    /// An overlay's scrim spans the eye: it stays put (moving it would uncover a strip at one
    /// edge) and doesn't hold its panel back at the edge clamp.
    #[test]
    fn full_width_fills_stay_put() {
        use gpui::{Bounds, ContentMask, Quad, Scene, ScaledPixels, point, size};
        let b = |x: f32, w: f32| Bounds { origin: point(ScaledPixels(x), ScaledPixels(0.0)), size: size(ScaledPixels(w), ScaledPixels(40.0)) };
        let full = ContentMask { bounds: b(0.0, 100.0) };
        let mut scene = Scene::default();
        let prev = scene.push_stereo_scope(depth::OVERLAY, Some(b(0.0, 100.0)));
        scene.insert_primitive(Quad { bounds: b(0.0, 100.0), content_mask: full.clone(), ..Default::default() });
        scene.insert_primitive(Quad { bounds: b(30.0, 40.0), content_mask: full.clone(), ..Default::default() });
        scene.pop_stereo_scope(prev);
        scene.finish();
        let p = depth::packing(1920.0, 200);
        let packed = scene.pack_side_by_side(size(ScaledPixels(100.0), ScaledPixels(40.0)), &*p.shift, p.edge_slack);
        let xs: Vec<f32> = packed.quads.iter().map(|q| q.bounds.origin.x.0).collect();
        // The scrim at 0 / 100; the panel the overlay's full 200% shift, 8 px: +4 / -4.
        assert_eq!(xs, vec![0.0, 100.0, 34.0, 126.0]);
    }

    #[test]
    fn the_policy_is_the_shared_ladder() {
        let p = depth::policy();
        assert_eq!((p.control, p.hover, p.focus, p.max, p.tooltip), (0.15, 0.15, 0.3, 1.0, 0.75));
        assert!(p.hover <= p.focus);
    }
}
