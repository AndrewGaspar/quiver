//! Renders the Sight panel off-screen with the demo fixture (`demo::sample_panel`), one PNG
//! per section, composited over a stand-in "game" image, for looking at it without a
//! compositor or a running game. A row is focused in each, as after a few presses.
//!
//! ```text
//! cargo run -p quiver-sight --example panel_png -- OUT_DIR [--scale 1.6] [--size 1600x1000] [--backdrop blur|dim]
//! ```

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext, OffscreenWindowOptions, px, size};
use quiver_api::sight::{Backdrop, PanelSection};
use quiver_api::perf::{HudPreview, Presenter};
use quiver_sight::demo::{sample_model, sample_panel, sample_perf, standard_layout};
use quiver_sight::panel::sections::performance as perf;
use quiver_sight::panel::state::Nav;
use quiver_sight::panel::view::{Footer, PanelView, Wiring};
use quiver_ui::input::Input;
use quiver_ui::theme;

#[derive(Default, Clone)]
struct Shot {
    w: u32,
    h: u32,
    stride: u32,
    px: Vec<u8>,
}

/// A soft, game-like backdrop (dusk over sand), blurred-looking for the blur shots.
fn game_pixel(x: u32, y: u32, w: u32, h: u32, blurred: bool) -> [f32; 3] {
    let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
    let sky = [210.0 - 90.0 * fy, 140.0 - 40.0 * fy, 90.0 + 30.0 * fy];
    let sand = [190.0, 150.0, 95.0];
    let horizon = 0.62 + 0.04 * (fx * 9.0).sin();
    let t = if blurred { ((fy - horizon) * 12.0).clamp(-1.0, 1.0) * 0.5 + 0.5 } else if fy > horizon { 1.0 } else { 0.0 };
    let mut c = [0.0; 3];
    for i in 0..3 {
        c[i] = sky[i] * (1.0 - t) + sand[i] * t;
    }
    // A dark pyramid silhouette.
    let px_ = (fx - 0.7).abs();
    if fy > 0.35 + px_ * 1.1 && fy < horizon + 0.02 {
        let k = if blurred { 0.75 } else { 0.55 };
        c = c.map(|v| v * k);
    }
    c
}

fn save(s: &Shot, path: &PathBuf, blurred: bool) {
    let img = image::RgbImage::from_fn(s.w, s.h, |x, y| {
        let p = &s.px[(y * s.stride + x * 4) as usize..][..4];
        let bg = game_pixel(x, y, s.w, s.h, blurred);
        let ia = 1.0 - p[3] as f32 / 255.0;
        image::Rgb([0, 1, 2].map(|c| (p[c] as f32 + bg[c] * ia).min(255.0) as u8))
    });
    img.save(path).unwrap();
    println!("{} ({}x{})", path.display(), s.w, s.h);
}

/// What a shot sets up besides the presses: a page, this session's changes, a chip.
#[derive(Clone, Copy)]
enum Setup {
    None,
    /// Mods on DLSS's World page, Crowd density focused, after three changes this session
    /// (one waiting for the next launch), with the chip of the last.
    ModsWorld,
    /// The same, Y held on Crowd density: comparing with its value before.
    Compare,
    /// Tweaks after the multiplier went 2× live and the cap waits for the next launch.
    Tweaks,
}

impl Setup {
    fn apply(self, v: &mut PanelView, cx: &mut gpui::Context<PanelView>) {
        use quiver_api::options::{LiveChannel, OpOutcome, OptionDef, OptionType, Outcome};
        use quiver_sight::panel::sections::mods::{Chip, ChipWhen};
        use serde_json::json;
        let t = Instant::now();
        let live = Outcome::AppliedLive { channel: LiveChannel::Sdk, latency: None };
        let o = |surface: &str, id: &str, b: serde_json::Value, a: serde_json::Value, outcome: Outcome| OpOutcome { surface: surface.into(), id: id.into(), outcome, before: Some(b), after: Some(a) };
        let num = |p: u32, unit: Option<&str>| OptionDef { ty: OptionType::Float, precision: Some(p), unit: unit.map(String::from), ..Default::default() };
        match self {
            Setup::None => {}
            Setup::ModsWorld | Setup::Compare => {
                let dlss = "m1/aco-dlss";
                v.nav.pages.set_page(dlss, 3);
                v.nav.set_focus_index(1);
                v.log.record(false, "Render scale", &num(2, None), &o(dlss, "upscale.scale", json!(0.5), json!(0.58), Outcome::SavedRestart { running: true }), false, t - Duration::from_secs(90));
                v.log.record(false, "Draw distance", &num(1, Some("×")), &o(dlss, "world.lod", json!(1.0), json!(1.5), Outcome::SavedAt { at: "next area load".into() }), false, t - Duration::from_secs(20));
                let c = v.log.record(false, "Crowd density", &num(1, Some("×")), &o(dlss, "world.npc_density", json!(1.0), json!(2.0), live), false, t - Duration::from_millis(600));
                if let Some(mut c) = c {
                    if matches!(self, Setup::Compare) {
                        c = Chip { label: "Crowd density".into(), before: "2.0×".into(), after: "1.0×".into(), when: ChipWhen::Live { session_only: true }, at: t, comparing: true };
                    }
                    v.show_chip(c, cx);
                }
            }
            Setup::Tweaks => {
                let tw = "quiver/tweaks";
                let e = OptionDef { ty: OptionType::Enum, values: ["off", "2", "3"].iter().map(|x| quiver_api::options::EnumValue { value: json!(x), label: Some(if *x == "off" { "Off".into() } else { format!("{x}×") }), ..Default::default() }).collect(), ..Default::default() };
                let int = OptionDef { ty: OptionType::Int, ..Default::default() };
                v.log.record(true, "Frame-rate cap", &int, &o(tw, "fps.cap", json!(162), json!(120), Outcome::SavedRestart { running: true }), false, t - Duration::from_secs(40));
                if let Some(c) = v.log.record(true, "Lossless Scaling FG", &e, &o(tw, "lsfg.multiplier", json!("off"), json!("2"), Outcome::AppliedLive { channel: LiveChannel::File, latency: None }), false, t - Duration::from_millis(400)) {
                    v.show_chip(c, cx);
                }
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    let (mut scale, mut w, mut h, mut backdrop) = (1.6f32, 1600.0f32, 1000.0f32, Backdrop::Blur);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--scale" => scale = args.next().unwrap().parse().unwrap(),
            "--size" => {
                let v = args.next().unwrap();
                let (a, b) = v.split_once('x').unwrap();
                (w, h) = (a.parse().unwrap(), b.parse().unwrap());
            }
            "--backdrop" => backdrop = if args.next().as_deref() == Some("dim") { Backdrop::Dim } else { Backdrop::Blur },
            other => panic!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&out).unwrap();
    // (section, docked, presses after opening on it, what else to set up, file name)
    let shots: Vec<(PanelSection, bool, Vec<Input>, Setup, &str)> = vec![
        (PanelSection::Session, false, vec![], Setup::None, "session"),
        (PanelSection::Session, false, vec![Input::Down, Input::Activate, Input::Right], Setup::None, "session-force-quit"),
        (PanelSection::Mods, false, vec![], Setup::ModsWorld, "mods"),
        (PanelSection::Mods, true, vec![], Setup::ModsWorld, "mods-docked"),
        (PanelSection::Mods, true, vec![Input::Back, Input::Down], Setup::ModsWorld, "mods-docked-surfaces"),
        (PanelSection::Mods, true, vec![], Setup::Compare, "chip-compare"),
        (PanelSection::Tweaks, false, vec![Input::Down], Setup::Tweaks, "tweaks"),
        (PanelSection::Tweaks, true, vec![], Setup::Tweaks, "tweaks-docked"),
        (PanelSection::Performance, false, vec![Input::Down], Setup::None, "performance"),
        // The layout editor (Edit, A), a metric focused; a row picked up and moved; the
        // settings.
        (PanelSection::Performance, false, vec![Input::Down, Input::Down, Input::Activate, Input::Down, Input::Down, Input::Down], Setup::None, "performance-editor"),
        (PanelSection::Performance, false, vec![Input::Down, Input::Down, Input::Activate, Input::Right, Input::Activate, Input::Down, Input::Down], Setup::None, "performance-editor-moving"),
        (PanelSection::Performance, false, vec![Input::Down, Input::Down, Input::Activate, Input::Right, Input::Right, Input::Down, Input::Down], Setup::None, "performance-editor-settings"),
        (PanelSection::Achievements, false, vec![Input::Down, Input::Down, Input::Down], Setup::None, "achievements"),
    ];
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        let mut windows = Vec::new();
        for (section, docked, presses, setup, name) in shots {
            let shot: Rc<RefCell<Shot>> = Rc::default();
            let sink = {
                let s = shot.clone();
                move |f: &gpui::OffscreenFrame<'_>| *s.borrow_mut() = Shot { w: f.width, h: f.height, stride: f.stride, px: f.data.to_vec() }
            };
            let opts = OffscreenWindowOptions { size: size(px(w), px(h)), scale_factor: scale, ..Default::default() };
            let handle = cx
                .open_offscreen_window(opts, sink, move |_, cx| {
                    cx.new(|cx| {
                        let mut nav = Nav::default();
                        nav.open(Some(section));
                        if nav.docked() != docked {
                            nav.toggle_dock();
                        }
                        let mut v = PanelView::new("s-demo".into(), sample_panel(), nav, backdrop, Wiring::default(), cx);
                        v.footer = Footer { pad: "Steam Controller".into(), pad_shared: false };
                        v.perf.data = sample_perf();
                        let editor = name.starts_with("performance-editor");
                        // The editor opens when quiverd sends the layout (A on Edit asks).
                        let open_at = if editor { 3 } else { usize::MAX };
                        for (k, i) in presses.into_iter().enumerate() {
                            if k == open_at {
                                v.perf_reply(perf::Reply::Layout(Ok(standard_layout())), cx);
                                let l = v.perf.editor.as_ref().unwrap().layout.clone();
                                let model = sample_model("standard", 3, Presenter::Desktop, None);
                                let rows = (0..model.rows.len()).collect();
                                v.perf_reply(perf::Reply::Preview { sent: l, result: Ok(HudPreview { model, rows }) }, cx);
                            }
                            v.input(i, cx);
                        }
                        v
                    })
                })
                .unwrap();
            windows.push((handle, shot, name, docked, setup));
        }
        let blurred = backdrop.blurred();
        let tag = if blurred { "" } else { "-dim" };
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            for (handle, shot, name, docked, setup) in &windows {
                // Set up now, so the ages ("0.6 s ago") and the chip are as at a real frame.
                let _ = handle.update(cx, |v, _, cx| setup.apply(v, cx));
                let w: gpui::AnyWindowHandle = (*handle).into();
                let _: Result<(), _> = cx.update_window(w, |_, win, cx| {
                    win.refresh();
                    win.render_now(cx)
                });
                // Docked: the game in full view, unblurred.
                save(&shot.borrow(), &out.join(format!("panel-{name}{tag}.png")), blurred && !docked);
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
