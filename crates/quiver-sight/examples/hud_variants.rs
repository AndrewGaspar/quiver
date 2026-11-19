//! Renders the demo models of every preset in each content variant (`demo::VARIANTS`)
//! through the game presenter's real path (`GameHud` → `hud.image`), reads each image back
//! the way the layer does, and saves it over a stand-in backdrop with the live frame-time
//! graph drawn in by the layer's own graph code (`quiver_sight_layer::graph`, from synthetic
//! present times: 2× frame generation in the `base` and `long` variants, none in `short`).
//! Prints each image's size, to check that a layout's card keeps its size whatever it holds.
//!
//! ```text
//! cargo run -p quiver-sight --example hud_variants -- OUT_DIR [--extent 2560x1600]
//!     [--layouts fps,compact,standard,full,diagnostics,clock] [--anchor right] [--scale 0.75]
//!     [--background 0.45] [--outline] [--suffix -right] [--desktop]
//! ```
//!
//! `--desktop` renders the desktop presenter's view instead (its graph drawn by GPUI from
//! the model's points), off-screen, to `<layout>-<variant>-desktop.png`.
//!
//! ```text
//! ```

use std::path::PathBuf;
use std::time::Duration;

use gpui::App;
use quiver_api::perf::{Anchor, Presenter};
use quiver_perf::hud::HudPoll;
use quiver_perf::session::{create_hud_file, open_hud_reader};
use quiver_sight::demo::{LAYOUTS, VARIANTS, frametimes, sample_model, vary};
use quiver_sight::game::GameHud;
use quiver_sight_layer::graph::{History, VERTS_PER_GRAPH, Vertex, build};
use quiver_ui::theme;

/// Synthetic present times (ns) for the graph: the demo's frame times, every other present
/// starting an application frame when `fg`.
fn history(fg: bool) -> History {
    let mut h = History::new();
    let mut t = 1_000_000_000i64;
    for (k, ms) in frametimes(0, 700).into_iter().enumerate() {
        // Without FG the game renders every frame, twice as long each.
        t += ((if fg { ms } else { 2.0 * ms }) as f64 * 1e6) as i64;
        h.push(t);
        if fg && k % 2 == 0 {
            h.push_base(t);
        }
    }
    h
}

/// Fills a triangle of premultiplied colour over `img` (straight RGB), as the layer's graph
/// pipeline blends (no anti-aliasing).
fn fill_tri(img: &mut image::RgbImage, v: [&Vertex; 3], off: (f32, f32)) {
    let p: Vec<(f32, f32)> = v.iter().map(|v| (v.pos[0] + off.0, v.pos[1] + off.1)).collect();
    let [r, g, b, a] = v[0].color.to_le_bytes().map(|c| c as f32);
    let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
    if area.abs() < 1e-6 {
        return;
    }
    let (x0, x1) = (p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor().max(0.0) as u32, p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as u32);
    let (y0, y1) = (p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor().max(0.0) as u32, p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as u32);
    for y in y0..y1.min(img.height()) {
        for x in x0..x1.min(img.width()) {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let e = |i: usize, j: usize| ((p[j].0 - p[i].0) * (py - p[i].1) - (p[j].1 - p[i].1) * (px - p[i].0)) * area.signum();
            if e(0, 1) >= 0.0 && e(1, 2) >= 0.0 && e(2, 0) >= 0.0 {
                let d = img.get_pixel_mut(x, y);
                let ia = 1.0 - a / 255.0;
                *d = image::Rgb([(r + d[0] as f32 * ia).min(255.0) as u8, (g + d[1] as f32 * ia).min(255.0) as u8, (b + d[2] as f32 * ia).min(255.0) as u8]);
            }
        }
    }
}

fn save(path: &std::path::Path, d: &quiver_perf::HudBufferHeader, px: &[u8], fg: bool, outline: bool) {
    let (w, h, stride) = (d.width, d.height, d.stride as usize);
    let pad = 8;
    let mut img = image::RgbImage::from_fn(w + 2 * pad, h + 2 * pad, |x, y| {
        let t = (x + y) as f32 / (w + h + 4 * pad) as f32;
        let bg = [70.0 + 90.0 * t, 110.0 + 40.0 * t, 150.0 - 40.0 * t];
        if x < pad || y < pad || x >= w + pad || y >= h + pad {
            return image::Rgb(bg.map(|c| c as u8));
        }
        let (ix, iy) = (x - pad, y - pad);
        let p = &px[iy as usize * stride + ix as usize * 4..][..4];
        let ia = 1.0 - p[3] as f32 / 255.0;
        image::Rgb([0, 1, 2].map(|c| (p[c] as f32 + bg[c] * ia).round().min(255.0) as u8))
    });
    // The layer's graph, into each rectangle the descriptor names.
    let hist = history(fg);
    let mut out = vec![Vertex::default(); VERTS_PER_GRAPH];
    for g in d.graphs() {
        let n = build(&hist, g, (0, 0), d.scale.max(1.0), &mut out);
        for t in out[..n].chunks_exact(3) {
            fill_tri(&mut img, [&t[0], &t[1], &t[2]], (pad as f32, pad as f32));
        }
        if outline {
            for x in g.x..g.x + g.w {
                for y in [g.y, g.y + g.h - 1] {
                    img.put_pixel(x as u32 + pad, y as u32 + pad, image::Rgb([255, 0, 255]));
                }
            }
        }
    }
    img.save(path).unwrap();
}

/// The desktop presenter's view (graph lines drawn by GPUI from the model's points, not the
/// layer) in an off-screen window, saved like the game images: `--desktop`.
fn desktop_png(m: quiver_api::perf::HudModel, file: &std::path::Path, cx: &mut App) {
    use gpui::{AppContext, OffscreenWindowOptions, px, size};
    use quiver_sight::hud_view::{HudView, Mode, model_size};
    let (w, h) = model_size(&m);
    let f = 2.0;
    // (width, height, stride, pixels)
    type Frame = (u32, u32, u32, Vec<u8>);
    let frame: std::rc::Rc<std::cell::RefCell<Frame>> = Default::default();
    let sink = frame.clone();
    let view = HudView::new(m, 1.0, Mode::Desktop);
    let handle = cx
        .open_offscreen_window(
            OffscreenWindowOptions { size: size(px(w + 1.0), px(h + 1.0)), scale_factor: f, prefer_low_power_gpu: true, min_frame_interval: Some(Duration::from_millis(100)), ..Default::default() },
            move |fr| *sink.borrow_mut() = (fr.width, fr.height, fr.stride, fr.data.to_vec()),
            move |_, cx| cx.new(|_| view),
        )
        .unwrap();
    cx.update_window(handle.into(), |_, win, cx| win.render_now(cx)).unwrap();
    let (fw, fh, stride, data) = frame.borrow().clone();
    let pad = 8;
    let img = image::RgbImage::from_fn(fw + 2 * pad, fh + 2 * pad, |x, y| {
        let t = (x + y) as f32 / (fw + fh + 4 * pad) as f32;
        let bg = [70.0 + 90.0 * t, 110.0 + 40.0 * t, 150.0 - 40.0 * t];
        if x < pad || y < pad || x >= fw + pad || y >= fh + pad {
            return image::Rgb(bg.map(|c| c as u8));
        }
        let p = &data[(y - pad) as usize * stride as usize + (x - pad) as usize * 4..][..4];
        let ia = 1.0 - p[3] as f32 / 255.0;
        image::Rgb([0, 1, 2].map(|c| (p[c] as f32 + bg[c] * ia).round().min(255.0) as u8))
    });
    img.save(file).unwrap();
    let _ = handle.update(cx, |_, win, _| win.remove_window());
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    let (mut extent, mut layouts) = ([2560u32, 1600u32], LAYOUTS.map(String::from).to_vec());
    let (mut anchor, mut scale, mut background, mut outline, mut suffix, mut desktop) = (None::<Anchor>, None::<f32>, None::<f32>, false, String::new(), false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--extent" => {
                let v = args.next().unwrap();
                let (w, h) = v.split_once('x').unwrap();
                extent = [w.parse().unwrap(), h.parse().unwrap()];
            }
            "--layouts" => layouts = args.next().unwrap().split(',').map(String::from).collect(),
            "--anchor" => anchor = Some(serde_json::from_value(serde_json::Value::String(args.next().unwrap())).expect("an anchor: top-left, right, …")),
            "--scale" => scale = Some(args.next().unwrap().parse().unwrap()),
            "--background" => background = Some(args.next().unwrap().parse().unwrap()),
            "--outline" => outline = true,
            "--desktop" => desktop = true,
            "--suffix" => suffix = args.next().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&out).unwrap();
    let image_path = out.join("hud.image");
    create_hud_file(&image_path, quiver_perf::hud::buffer_bytes_for(1024, 1000).unwrap()).unwrap();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(50)).await;
            cx.update(|cx| {
                let mut reader = open_hud_reader(&image_path).unwrap();
                let mut dst = vec![0u8; reader.info().buffer_bytes as usize];
                let mut rev = 0;
                for layout in &layouts {
                    let mut hud = GameHud::default();
                    let mut sizes = Vec::new();
                    for variant in VARIANTS {
                        rev += 1;
                        let mut m = vary(sample_model(layout, 3, Presenter::Game, Some(extent)), variant);
                        m.image = Some(image_path.clone());
                        m.rev = rev;
                        if let Some(a) = anchor {
                            m.anchor = a;
                        }
                        if let Some(s) = scale {
                            m.scale = s;
                        }
                        if let Some(b) = background {
                            m.background = b;
                        }
                        if desktop {
                            let d = vary(sample_model(layout, 3, Presenter::Desktop, None), variant);
                            let d = quiver_api::perf::HudModel { anchor: m.anchor, scale: m.scale, background: m.background, ..d };
                            let file = out.join(format!("{layout}-{variant}{suffix}-desktop.png"));
                            desktop_png(d, &file, cx);
                            println!("{layout:>11} {variant:<6} desktop  {}", file.display());
                            continue;
                        }
                        hud.update(&m, cx).unwrap();
                        let HudPoll::Image { desc, .. } = reader.poll((extent[0], extent[1]), &mut dst) else { panic!("no image for {layout}/{variant}") };
                        let file = out.join(format!("{layout}-{variant}{suffix}.png"));
                        save(&file, &desc, &dst, variant != "short", outline);
                        let graphs: Vec<String> = desc.graphs().iter().map(|g| format!("{}x{}@{},{} s{}", g.w, g.h, g.x, g.y, g.series)).collect();
                        println!("{layout:>11} {variant:<6} {}x{} graphs [{}]  {}", desc.width, desc.height, graphs.join(" "), file.display());
                        sizes.push((desc.width, desc.height));
                    }
                    let same = sizes.windows(2).all(|w| w[0] == w[1]);
                    println!("{layout:>11} size {}", if same { "constant" } else { "VARIES" });
                    hud.close(cx);
                }
                let _ = std::fs::remove_file(&image_path);
                cx.quit();
            });
        })
        .detach();
    });
}
