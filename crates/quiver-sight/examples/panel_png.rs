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
use std::time::Duration;

use gpui::{App, AppContext, OffscreenWindowOptions, px, size};
use quiver_api::sight::{Backdrop, PanelSection};
use quiver_sight::demo::sample_panel;
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
    // (section, presses after opening on it, file name)
    let shots: Vec<(PanelSection, Vec<Input>, &str)> = vec![
        // The first quick toggle focused, as in the mock-up.
        (PanelSection::Session, vec![], "session"),
        (PanelSection::Session, vec![Input::Down; 7].into_iter().chain([Input::Activate, Input::Right]).collect(), "session-force-quit"),
        (PanelSection::Mods, vec![Input::Down, Input::Down, Input::Down], "mods"),
        (PanelSection::Tweaks, vec![Input::Down], "tweaks"),
        (PanelSection::Performance, vec![Input::Down], "performance"),
        (PanelSection::Achievements, vec![Input::Down, Input::Down, Input::Down], "achievements"),
        // RT twice: Locked (progress bars), the hidden one focused.
        (PanelSection::Achievements, vec![Input::NextTab, Input::NextTab, Input::Down, Input::Down, Input::Down, Input::Down, Input::Down], "achievements-locked"),
    ];
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        let mut windows = Vec::new();
        for (section, presses, name) in shots {
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
                        let mut v = PanelView::new("s-demo".into(), sample_panel(), nav, backdrop, Wiring::default(), cx);
                        v.footer = Footer { pad: "Steam Controller".into(), pad_shared: false };
                        for i in presses {
                            v.input(i, cx);
                        }
                        v
                    })
                })
                .unwrap();
            windows.push((handle, shot, name));
        }
        let blurred = backdrop.blurred();
        let tag = if blurred { "" } else { "-dim" };
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            for (handle, shot, name) in &windows {
                let w: gpui::AnyWindowHandle = (*handle).into();
                let _: Result<(), _> = cx.update_window(w, |_, win, cx| {
                    win.refresh();
                    win.render_now(cx)
                });
                save(&shot.borrow(), &out.join(format!("panel-{name}{tag}.png")), blurred);
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
