//! Renders the quick menu, the overlay drawn in the game (toasts, the status chip, the
//! first-launch hint) and every toast kind off-screen into PNGs (design/SIGHT.md §16), over a
//! stand-in "game" backdrop, at the mock-ups' 1440x900.
//!
//! ```text
//! cargo run -p quiver-sight --example sight_png -- OUT_DIR [--scale 1.0]
//! ```

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext, OffscreenWindowOptions, px, size};
use quiver_api::sight::{PinKind, QuickPin};
use quiver_sight::demo::{more_toasts, sample_panel, sample_toasts};
use quiver_sight::overlay::{Chip, OverlayView};
use quiver_sight::place::ToastCorner;
use quiver_sight::quick::{Key, Quick, QuickView, tiles};
use quiver_sight::toasts::ToastView;
use quiver_ui::theme;

#[derive(Default, Clone)]
struct Shot {
    w: u32,
    h: u32,
    stride: u32,
    px: Vec<u8>,
}

fn save(s: &Shot, path: &PathBuf) {
    // Premultiplied over a stand-in game: a dusky gradient with a horizon.
    let img = image::RgbImage::from_fn(s.w, s.h, |x, y| {
        let p = &s.px[(y * s.stride + x * 4) as usize..][..4];
        let (fx, fy) = (x as f32 / s.w as f32, y as f32 / s.h as f32);
        let sky = fy < 0.55;
        let bg = if sky { [70.0 + 90.0 * fy, 90.0 + 60.0 * fy, 120.0 - 40.0 * fx] } else { [120.0 - 40.0 * fy, 95.0 - 30.0 * fy, 60.0 - 20.0 * fy] };
        let ia = 1.0 - p[3] as f32 / 255.0;
        image::Rgb([0, 1, 2].map(|c| (p[c] as f32 + bg[c] * ia).min(255.0) as u8))
    });
    img.save(path).unwrap();
    println!("{} ({}x{})", path.display(), s.w, s.h);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    let mut scale = 1.0f32;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--scale" => scale = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&out).unwrap();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        let opts = |w: f32, h: f32| OffscreenWindowOptions { size: size(px(w), px(h)), scale_factor: scale, ..Default::default() };
        let mut shots: Vec<(String, Rc<RefCell<Shot>>, gpui::AnyWindowHandle)> = Vec::new();
        let sink = |s: &Rc<RefCell<Shot>>| {
            let s = s.clone();
            move |f: &gpui::OffscreenFrame<'_>| *s.borrow_mut() = Shot { w: f.width, h: f.height, stride: f.stride, px: f.data.to_vec() }
        };
        // The toast lane on the desktop surface: the mock's four, then the rest.
        for (name, list) in [("toasts", sample_toasts()), ("toasts-more", more_toasts())] {
            let s: Rc<RefCell<Shot>> = Rc::default();
            let w = cx.open_offscreen_window(opts(400.0, 560.0), sink(&s), move |_, cx| cx.new(|_| ToastView::new(list))).unwrap();
            shots.push((name.into(), s, w.into()));
        }
        // The quick menu, held, on the HUD tile, with a pin; then latched on Quit, A half held.
        let data = sample_panel();
        let pins = vec![QuickPin { kind: PinKind::Option, surface: "m1/aco-dlss".into(), id: "world.npc_density".into(), label: "Crowd density".into() }];
        let notices = vec![sample_toasts().remove(3)];
        for (name, latched_quit, with_notice) in [("quick", false, false), ("quick-notice", false, true), ("quick-quit", true, false)] {
            let mut q = Quick::default();
            q.set_tiles(tiles(&data, &pins, if with_notice { &notices } else { &[] }, if with_notice { quiver_sight::replay::State::External { pid: 4242, seconds: 60 } } else { quiver_sight::replay::State::Off }));
            let t = Instant::now();
            if latched_quit {
                q.latch("s-1", t);
                q.focus = q.tiles.len() - 1;
                q.key(Key::Enter, true, t);
            } else {
                q.press("s-1", t);
                q.chord(Some(true), t + quiver_sight::quick::HOLD);
                q.focus = 1.min(q.tiles.len() - 1);
            }
            let at = if latched_quit { t + Duration::from_millis(500) } else { t };
            let s: Rc<RefCell<Shot>> = Rc::default();
            let w = cx.open_offscreen_window(opts(1440.0, 900.0), sink(&s), move |_, cx| cx.new(|cx| QuickView::from(&q, at, cx))).unwrap();
            shots.push((name.into(), s, w.into()));
        }
        // The overlay in the game: toasts, the chip, the hint.
        let s: Rc<RefCell<Shot>> = Rc::default();
        let w = cx
            .open_offscreen_window(opts(1440.0, 900.0), sink(&s), move |_, cx| {
                cx.new(|cx| {
                    let mut v = OverlayView::new(cx);
                    v.toasts = sample_toasts().into_iter().take(3).enumerate().map(|(i, t)| (i as u64, t)).collect();
                    v.corner = ToastCorner::TopRight;
                    v.chip = Some(Chip { fps: Some(118.0), started: Some(chrono::Utc::now() - chrono::Duration::minutes(72)) });
                    v.hint = true;
                    v
                })
            })
            .unwrap();
        shots.push(("overlay".into(), s, w.into()));
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            for (_, _, w) in &shots {
                let _: Result<(), _> = cx.update_window(*w, |_, win, cx| {
                    win.refresh();
                    win.render_now(cx)
                });
            }
            for (name, s, _) in &shots {
                save(&s.borrow(), &out.join(format!("{name}.png")));
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
