//! Stereo UI, a prototype (design/STEREO-UI.md): on a side-by-side stereo output (an XReal
//! under HypXRland's `monitor = …, stereo:sbs`, or a HypXRland XR monitor under its depth
//! desktop) the app draws itself once per eye into a buffer two windows wide, tags its toplevel
//! `stereo:sbs`, and offsets each element by the disparity of its depth, so the cover grid
//! floats above the hero art and the focused tile pops forward. Off unless `QUIVER_STEREO_UI`
//! asks for it; nothing changes by default.
//!
//! Environment:
//! - `QUIVER_STEREO_UI=sbs`: pack while the window is on a stereo output (Hyprland only:
//!   `hyprctl -j monitors` says `"stereo": "sbs"`), mono elsewhere. `sbs-always`: pack
//!   regardless (any compositor; for testing). Unset, `off` or `mono`: never.
//! - `QUIVER_STEREO_UI_STRENGTH=1.0`: multiplies every depth's rise (0..2).
//! - `QUIVER_STEREO_UI_TAG=both`: how the compositor is told. `xdg`: only the
//!   `xdg-toplevel-tag-v1` tag `stereo:sbs` (needs the rule
//!   `hl.window_rule({ name = "stereo-xdg-tag", stereo = "auto", match = { xdg_tag = "^stereo:" } })`);
//!   `both` (default) also sets the Hyprland window tag `stereo-sbs` over IPC, which the
//!   existing `stereo-tag-<layout>` rules map to `stereo sbs always`.
//!
//! Depth is HypXRland's unitless rung (0..1 above the window's plane), the vocabulary
//! `quiver_perf::stereo` uses for Sight. Per-eye pixels come from HypXRland's own formula
//! (docs/openxr/05-configuration.md §8.7, research/24 §8.1) with its defaults, rounded to
//! whole device pixels so text and art stay pixel-exact in each eye.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{Context, StereoPacking, Window};
use serde::Deserialize;

use crate::app::Quiver;

pub const ENV: &str = "QUIVER_STEREO_UI";
pub const ENV_STRENGTH: &str = "QUIVER_STEREO_UI_STRENGTH";
pub const ENV_TAG: &str = "QUIVER_STEREO_UI_TAG";

/// The app's depth rungs. Steps between rungs that are on screen together stay at or under
/// ~3 px of total disparity at a 1920 px pane (HypXRland's foveal-fusion limit, research/24
/// §8.2), and nothing goes behind the window's plane.
pub mod depth {
    /// Hero art and page backgrounds: the window's plane.
    pub const HERO: f32 = 0.0;
    /// The top bar and other chrome.
    pub const CHROME: f32 = 0.15;
    /// Cover tiles in the grid.
    pub const TILE: f32 = 0.3;
    /// The focused (or hovered) tile.
    pub const FOCUSED: f32 = 0.6;
    /// Dialogs, overlays, toasts.
    pub const DIALOG: f32 = 0.8;
}

/// HypXRland's geometry defaults (`decoration:depth_scale`, `depth_distance`,
/// `depth_screen_width`, the fixed interocular distance).
pub const DEPTH_SCALE_M: f64 = 0.12;
pub const DISTANCE_M: f64 = 1.5;
pub const SCREEN_WIDTH_M: f64 = 1.6;
pub const IPD_M: f64 = 0.063;
/// HypXRland's hard ceiling on angular disparity.
pub const MAX_DISPARITY_DEG: f64 = 1.0;

/// Total disparity in pixels (both eyes together) for `depth` on a pane `pane_px` pixels
/// wide: the rise `h = depth · 0.12 m · strength` toward a screen 1.5 m away and 1.6 m wide,
/// parallax `b·h / (D − h)`, capped at 1°. Unrounded.
pub fn disparity_px(depth: f32, pane_px: f32, strength: f32) -> f32 {
    let h = depth.clamp(0.0, 1.0) as f64 * DEPTH_SCALE_M * strength.clamp(0.0, 2.0) as f64;
    let parallax = IPD_M * h / (DISTANCE_M - h);
    let parallax = parallax.min(DISTANCE_M * MAX_DISPARITY_DEG.to_radians().tan());
    (parallax * pane_px as f64 / SCREEN_WIDTH_M) as f32
}

/// Each eye's offset in whole pixels, `[left, right]`: the total rounded, the left eye's
/// content moving right by the larger half and the right eye's moving left by the rest
/// (crossed disparity: in front of the screen).
pub fn eye_shifts(depth: f32, pane_px: f32, strength: f32) -> [i32; 2] {
    let total = disparity_px(depth, pane_px, strength).round().max(0.0) as i32;
    [(total + 1) / 2, -(total / 2)]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Pack while the window's monitor is a stereo output.
    Auto,
    /// Pack regardless of the monitor.
    Always,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.trim() {
            "sbs" | "auto" => Mode::Auto,
            "sbs-always" | "always" => Mode::Always,
            _ => Mode::Off,
        }
    }

    pub fn from_env() -> Mode {
        std::env::var(ENV).map(|v| Mode::parse(&v)).unwrap_or(Mode::Off)
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
    /// Present (`"sbs"`) only on a stereo output (HypXRland's `HyprCtl.cpp`).
    #[serde(default)]
    stereo: Option<String>,
}

fn one() -> f64 {
    1.0
}

/// Our window (by pid) and its monitor, from `hyprctl -j clients` and `-j monitors`.
pub fn our_output(clients: &str, monitors: &str, pid: u32) -> Option<(String, Output)> {
    let clients: Vec<HyprClient> = serde_json::from_str(clients).ok()?;
    let me = clients
        .into_iter()
        .filter(|c| c.pid == pid as i64 && c.mapped && !c.address.is_empty())
        .min_by_key(|c| c.focus_history.unwrap_or(i64::MAX))?;
    let monitors: Vec<HyprMonitor> = serde_json::from_str(monitors).ok()?;
    let m = monitors.into_iter().find(|m| m.id == me.monitor)?;
    let stereo = m.stereo.as_deref().is_some_and(|s| s == "sbs");
    Some((
        me.address,
        Output { name: m.name, stereo, pane_width: m.width as f32, scale: m.scale as f32 },
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

/// The packing for a pane `pane_px` device pixels wide.
pub fn packing(pane_px: f32, strength: f32) -> StereoPacking {
    StereoPacking {
        shift: Rc::new(move |depth| {
            let [l, r] = eye_shifts(depth, pane_px, strength);
            [l as f32, r as f32]
        }),
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

/// The app's stereo state.
#[derive(Default)]
pub struct StereoUi {
    pub packed: bool,
    /// The pane width (device pixels) the current packing was made for.
    pane_px: f32,
    /// The window the Hyprland tag is on.
    hypr_tagged: Option<String>,
}

impl Quiver {
    /// Starts following the window's monitor (see the module docs). Does nothing unless
    /// `QUIVER_STEREO_UI` is set.
    pub fn start_stereo_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = Mode::from_env();
        let strength: f32 = std::env::var(ENV_STRENGTH).ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let hypr_tag = std::env::var(ENV_TAG).map(|v| v.trim() != "xdg").unwrap_or(true);
        match mode {
            Mode::Off => {}
            Mode::Always => {
                let pane = 1920.0 * window.scale_factor();
                let ok = window.set_stereo_packing(Some(packing(pane, strength)));
                self.stereo.packed = ok;
                self.stereo.pane_px = pane;
                tracing::info!("stereo ui: packing side by side regardless of the monitor (supported: {ok})");
            }
            Mode::Auto => {
                if hypr_socket(".socket.sock").is_none() {
                    tracing::info!("stereo ui: not Hyprland; staying mono");
                    return;
                }
                // Hyprland's event socket, read on a thread: any event that can move the window
                // to another monitor (or change a monitor) wakes the check.
                let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
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
                cx.spawn_in(window, async move |this, cx| {
                    loop {
                        // Coalesce a burst of events, and look again now and then anyway (a
                        // `stereo` change doesn't always come with an event).
                        let tick = cx.background_executor().timer(Duration::from_secs(5));
                        futures::select_biased! {
                            e = rx.next() => if e.is_none() { break },
                            _ = futures::FutureExt::fuse(tick) => {},
                        }
                        cx.background_executor().timer(Duration::from_millis(120)).await;
                        while rx.try_recv().is_ok() {}
                        let pid = std::process::id();
                        let found = cx
                            .background_executor()
                            .spawn(async move {
                                let hyprctl = crate::selfupdate::hypr::hyprctl;
                                let clients = hyprctl(&["-j", "clients"]).ok()?;
                                let monitors = hyprctl(&["-j", "monitors"]).ok()?;
                                our_output(&clients, &monitors, pid)
                            })
                            .await;
                        let alive = this.update_in(cx, |q, window, cx| q.apply_stereo(found, strength, hypr_tag, window, cx));
                        if alive.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        }
    }

    fn apply_stereo(
        &mut self,
        found: Option<(String, Output)>,
        strength: f32,
        hypr_tag: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((address, out)) = found else { return };
        let pane = out.pane_width * out.scale;
        let want = out.stereo;
        if want && (!self.stereo.packed || self.stereo.pane_px != pane) {
            let ok = window.set_stereo_packing(Some(packing(pane, strength)));
            self.stereo.packed = ok;
            self.stereo.pane_px = pane;
            tracing::info!("stereo ui: {} is stereo (pane {pane} px); packing side by side: {ok}", out.name);
        } else if !want && self.stereo.packed {
            window.set_stereo_packing(None);
            self.stereo.packed = false;
            tracing::info!("stereo ui: {} is mono; packing off", out.name);
        }
        // The Hyprland window tag follows the packing (the xdg tag is set by the window).
        let tagged = self.stereo.hypr_tagged.as_deref() == Some(address.as_str());
        if hypr_tag && self.stereo.packed && !tagged {
            let addr = address.clone();
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = set_hypr_tag('+', &addr) {
                        tracing::warn!("stereo ui: Hyprland window tag: {e}");
                    }
                })
                .detach();
            self.stereo.hypr_tagged = Some(address);
        } else if !self.stereo.packed && let Some(addr) = self.stereo.hypr_tagged.take() {
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = set_hypr_tag('-', &addr) {
                        tracing::warn!("stereo ui: Hyprland window tag: {e}");
                    }
                })
                .detach();
        }
        cx.notify();
    }
}

// ---------------------------------------------------------------------------------------------
// Evidence: the packed frame, rendered off-screen

/// `QUIVER_STEREO_SHOT=DIR` (with `QUIVER_DEMO=1`): renders the app's demo library off-screen
/// (no compositor, no daemon) and writes PNGs of the per-eye packed frame to DIR, plus timings
/// (design/STEREO-UI.md, "Evidence"). `QUIVER_STEREO_SHOT_SIZE=1920x1080` sets the window's
/// logical size.
pub mod shot {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use gpui::{App, AppContext, OffscreenWindowOptions, StereoPacking, px, size};

    use super::{depth, disparity_px, packing};
    use crate::app::{InputMode, Quiver};
    use crate::nav::Input;

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

    /// Each eye moved by half the *unrounded* disparity: what a sub-pixel path would draw.
    fn fractional(pane: f32) -> StereoPacking {
        StereoPacking {
            shift: Rc::new(move |d| {
                let t = disparity_px(d, pane, 1.0);
                [t / 2.0, -t / 2.0]
            }),
        }
    }

    pub fn run(dir: PathBuf) {
        let (w, h) = std::env::var("QUIVER_STEREO_SHOT_SIZE")
            .ok()
            .and_then(|v| {
                let (a, b) = v.split_once('x')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((1920.0f32, 1080.0f32));
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
                scale_factor: 1.0,
                prefer_low_power_gpu: false,
                ..Default::default()
            };
            let handle = cx
                .open_offscreen_window(opts, sink, |window, cx| cx.new(|cx| Quiver::new(true, window, cx)))
                .expect("off-screen window");
            let any: gpui::AnyWindowHandle = handle.into();
            cx.spawn(async move |cx| {
                // The library, then its art (from the cache), then a quiet moment.
                for _ in 0..100 {
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                    let ready = handle.update(cx, |q, _, _| !q.layout.grid.is_empty()).unwrap_or(false);
                    if ready {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_secs(4)).await;
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
                let pane = w;
                let timed = |cx: &mut gpui::AsyncApp, label: &str| {
                    render(cx);
                    let mut gpu = Vec::new();
                    let mut wall = Vec::new();
                    for _ in 0..30 {
                        let t = Instant::now();
                        let f = render(cx);
                        wall.push(t.elapsed());
                        gpu.push(f.render);
                    }
                    gpu.sort();
                    wall.sort();
                    println!(
                        "{label}: render+readback p50 {:?} p90 {:?}; render_now (layout+paint+pack+render+readback) p50 {:?} p90 {:?}",
                        gpu[15], gpu[27], wall[15], wall[27]
                    );
                };

                set(cx, None);
                save(&render(cx), &dir.join("mono.png"));
                timed(cx, "mono");
                set(cx, Some(packing(pane, 1.0)));
                save(&render(cx), &dir.join("sbs.png"));
                timed(cx, "sbs");
                // A focused tile: the controller moves the selection down from the hero.
                for n in 1..=2 {
                    let _ = handle.update(cx, |q, _, cx| {
                        q.mode = InputMode::Pad;
                        q.key(Input::Down, cx);
                    });
                    save(&render(cx), &dir.join(format!("sbs-focus-{n}.png")));
                }
                set(cx, None);
                save(&render(cx), &dir.join("mono-focus.png"));
                set(cx, Some(fractional(pane)));
                save(&render(cx), &dir.join("sbs-fractional.png"));
                println!(
                    "rungs (total px at {pane} px): hero {:.2}, chrome {:.2}, tile {:.2}, focused {:.2}, dialog {:.2}",
                    disparity_px(depth::HERO, pane, 1.0),
                    disparity_px(depth::CHROME, pane, 1.0),
                    disparity_px(depth::TILE, pane, 1.0),
                    disparity_px(depth::FOCUSED, pane, 1.0),
                    disparity_px(depth::DIALOG, pane, 1.0),
                );
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disparity_matches_hypxrland_table() {
        // 05-configuration.md §8.7's table at the shipped defaults, 1920 px pane.
        for (d, total) in [(0.2, 1.2), (0.6, 3.8), (0.8, 5.2), (1.0, 6.6)] {
            let t = disparity_px(d, 1920.0, 1.0);
            assert!((t - total).abs() < 0.06, "depth {d}: {t} px, table {total}");
        }
        assert_eq!(disparity_px(0.0, 1920.0, 1.0), 0.0);
        // The 1° ceiling: about 31.4 px total at these defaults.
        assert!((disparity_px(1.0, 1920.0, 100.0) - disparity_px(1.0, 1920.0, 2.0)).abs() < 1e-6);
    }

    #[test]
    fn rungs_in_whole_pixels() {
        let px = |d| eye_shifts(d, 1920.0, 1.0);
        assert_eq!(px(depth::HERO), [0, 0]);
        assert_eq!(px(depth::CHROME), [1, 0]);
        assert_eq!(px(depth::TILE), [1, -1]);
        assert_eq!(px(depth::FOCUSED), [2, -2]);
        assert_eq!(px(depth::DIALOG), [3, -2]);
        // Adjacent rungs differ by at most 3 px of total disparity.
        let rungs = [depth::HERO, depth::CHROME, depth::TILE, depth::FOCUSED, depth::DIALOG];
        for w in rungs.windows(2) {
            let t = |d| {
                let [l, r] = px(d);
                l - r
            };
            assert!(t(w[1]) - t(w[0]) <= 3);
        }
    }

    #[test]
    fn packing_moves_each_depth_per_eye() {
        use gpui::{Bounds, ContentMask, Quad, Scene, ScaledPixels, point, size};
        let b = |x: f32, w: f32| Bounds {
            origin: point(ScaledPixels(x), ScaledPixels(10.0)),
            size: size(ScaledPixels(w), ScaledPixels(20.0)),
        };
        let full = ContentMask { bounds: b(0.0, 100.0) };
        let mut scene = Scene::default();
        scene.insert_primitive(Quad { bounds: b(0.0, 100.0), content_mask: full.clone(), ..Default::default() });
        scene.set_stereo_depth(depth::FOCUSED);
        scene.insert_primitive(Quad { bounds: b(40.0, 10.0), content_mask: full.clone(), ..Default::default() });
        scene.set_stereo_depth(0.0);
        scene.finish();
        let p = packing(1920.0, 1.0);
        let packed = scene.pack_side_by_side(size(ScaledPixels(100.0), ScaledPixels(40.0)), &*p.shift);
        let mut xs: Vec<(f32, f32, f32)> = packed
            .quads
            .iter()
            .map(|q| (q.bounds.origin.x.0, q.content_mask.bounds.origin.x.0, q.content_mask.bounds.size.width.0))
            .collect();
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // Backgrounds at depth 0 fill each eye exactly; the raised quad moves +2 / -2 and its
        // mask moves with it, clipped to its eye.
        assert_eq!(xs, vec![(0.0, 0.0, 100.0), (42.0, 2.0, 98.0), (100.0, 100.0, 100.0), (138.0, 100.0, 98.0)]);
    }

    #[test]
    fn modes() {
        assert_eq!(Mode::parse("sbs"), Mode::Auto);
        assert_eq!(Mode::parse("sbs-always"), Mode::Always);
        assert_eq!(Mode::parse(""), Mode::Off);
        assert_eq!(Mode::parse("hsbs"), Mode::Off);
    }

    const CLIENTS: &str = r#"[
        {"address": "0xa", "pid": 10, "mapped": true, "monitor": 0, "focusHistoryID": 2},
        {"address": "0xb", "pid": 42, "mapped": true, "monitor": 1, "focusHistoryID": 1}
    ]"#;

    #[test]
    fn finds_the_stereo_monitor() {
        let monitors = r#"[
            {"id": 0, "name": "eDP-2", "width": 2560, "scale": 1.25},
            {"id": 1, "name": "DP-5", "width": 1920, "scale": 1.0, "stereo": "sbs",
             "scanoutWidth": 3840, "scanoutHeight": 1080, "stereoComposites": 2, "stereoContent": false}
        ]"#;
        let (addr, out) = our_output(CLIENTS, monitors, 42).unwrap();
        assert_eq!(addr, "0xb");
        assert!(out.stereo);
        assert_eq!(out.pane_width, 1920.0);
        let (_, out) = our_output(CLIENTS, monitors, 10).unwrap();
        assert!(!out.stereo);
        assert!(our_output(CLIENTS, monitors, 7).is_none());
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
}
