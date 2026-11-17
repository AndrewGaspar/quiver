//! Renders Sight's desktop surfaces (the toast stack and the desktop-presenter HUD, which
//! draws its own frame-time graph) off-screen and saves PNGs, for looking at them without a
//! compositor (or with the display off).
//!
//! ```text
//! cargo run -p quiver-sight --example views_png -- OUT_DIR [--scale 1.25] [--layout standard]
//! ```

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, AppContext, OffscreenWindowOptions, px, size};
use quiver_api::perf::Presenter;
use quiver_sight::demo::{sample_model, sample_toasts};
use quiver_sight::hud_view::{HudView, Mode};
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
    // Premultiplied over a dark "game" backdrop.
    let img = image::RgbImage::from_fn(s.w, s.h, |x, y| {
        let p = &s.px[(y * s.stride + x * 4) as usize..][..4];
        let t = (x + y) as f32 / (s.w + s.h) as f32;
        let bg = [40.0 + 50.0 * t, 60.0 + 30.0 * t, 70.0 - 20.0 * t];
        let ia = 1.0 - p[3] as f32 / 255.0;
        image::Rgb([0, 1, 2].map(|c| (p[c] as f32 + bg[c] * ia).min(255.0) as u8))
    });
    img.save(path).unwrap();
    println!("{} ({}x{})", path.display(), s.w, s.h);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    let (mut scale, mut layout) = (1.25f32, "standard".to_owned());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--scale" => scale = args.next().unwrap().parse().unwrap(),
            "--layout" => layout = args.next().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&out).unwrap();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        let toast_shot: Rc<RefCell<Shot>> = Rc::default();
        let hud_shot: Rc<RefCell<Shot>> = Rc::default();
        let opts = |w: f32, h: f32| OffscreenWindowOptions { size: size(px(w), px(h)), scale_factor: scale, ..Default::default() };
        let sink = |s: &Rc<RefCell<Shot>>| {
            let s = s.clone();
            move |f: &gpui::OffscreenFrame<'_>| *s.borrow_mut() = Shot { w: f.width, h: f.height, stride: f.stride, px: f.data.to_vec() }
        };
        let toasts = cx.open_offscreen_window(opts(380.0, 320.0), sink(&toast_shot), |_, cx| cx.new(|_| ToastView::new(sample_toasts()))).unwrap();
        let model = sample_model(&layout, 3, Presenter::Desktop, None);
        let hud = cx.open_offscreen_window(opts(560.0, 260.0), sink(&hud_shot), move |_, cx| cx.new(|_| HudView::new(model, 1.0, Mode::Desktop))).unwrap();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            for w in [toasts.into(), hud.into()] {
                let _: Result<(), _> = cx.update_window(w, |_, win, cx| {
                    win.refresh();
                    win.render_now(cx)
                });
            }
            save(&toast_shot.borrow(), &out.join("toasts.png"));
            save(&hud_shot.borrow(), &out.join(format!("desktop-hud-{layout}.png")));
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
