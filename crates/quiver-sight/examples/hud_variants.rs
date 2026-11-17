//! Renders the demo models of every preset in each content variant (`demo::VARIANTS`)
//! through the game presenter's real path (`GameHud` → `hud.image`), reads each image back
//! the way the layer does, and saves it over a stand-in backdrop with the graph rectangles
//! outlined. Prints each image's size, to check that a layout's card keeps its size whatever
//! it holds.
//!
//! ```text
//! cargo run -p quiver-sight --example hud_variants -- OUT_DIR [--extent 2560x1600] [--layouts fps,compact,standard,full]
//! ```

use std::path::PathBuf;
use std::time::Duration;

use gpui::App;
use quiver_api::perf::Presenter;
use quiver_perf::hud::HudPoll;
use quiver_perf::session::{create_hud_file, open_hud_reader};
use quiver_sight::demo::{VARIANTS, sample_model, vary};
use quiver_sight::game::GameHud;
use quiver_ui::theme;

fn save(path: &std::path::Path, d: &quiver_perf::HudBufferHeader, px: &[u8]) {
    let (w, h, stride) = (d.width, d.height, d.stride as usize);
    let pad = 8;
    let img = image::RgbImage::from_fn(w + 2 * pad, h + 2 * pad, |x, y| {
        let t = (x + y) as f32 / (w + h + 4 * pad) as f32;
        let bg = [70.0 + 90.0 * t, 110.0 + 40.0 * t, 150.0 - 40.0 * t];
        if x < pad || y < pad || x >= w + pad || y >= h + pad {
            return image::Rgb(bg.map(|c| c as u8));
        }
        let (ix, iy) = (x - pad, y - pad);
        for g in d.graphs() {
            let inside_x = ix as i32 >= g.x && (ix as i32) < g.x + g.w;
            let inside_y = iy as i32 >= g.y && (iy as i32) < g.y + g.h;
            if (inside_x && (iy as i32 == g.y || iy as i32 == g.y + g.h - 1)) || (inside_y && (ix as i32 == g.x || ix as i32 == g.x + g.w - 1)) {
                return image::Rgb([255, 0, 255]);
            }
        }
        let p = &px[iy as usize * stride + ix as usize * 4..][..4];
        let ia = 1.0 - p[3] as f32 / 255.0;
        image::Rgb([0, 1, 2].map(|c| (p[c] as f32 + bg[c] * ia).round().min(255.0) as u8))
    });
    img.save(path).unwrap();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    let (mut extent, mut layouts) = ([2560u32, 1600u32], vec!["fps".to_owned(), "compact".into(), "standard".into(), "full".into()]);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--extent" => {
                let v = args.next().unwrap();
                let (w, h) = v.split_once('x').unwrap();
                extent = [w.parse().unwrap(), h.parse().unwrap()];
            }
            "--layouts" => layouts = args.next().unwrap().split(',').map(String::from).collect(),
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
                        hud.update(&m, cx).unwrap();
                        let HudPoll::Image { desc, .. } = reader.poll((extent[0], extent[1]), &mut dst) else { panic!("no image for {layout}/{variant}") };
                        let file = out.join(format!("{layout}-{variant}.png"));
                        save(&file, &desc, &dst);
                        let graphs: Vec<String> = desc.graphs().iter().map(|g| format!("{}x{}@{},{}", g.w, g.h, g.x, g.y)).collect();
                        println!("{layout:>8} {variant:<6} {}x{} graphs [{}]  {}", desc.width, desc.height, graphs.join(" "), file.display());
                        sizes.push((desc.width, desc.height));
                    }
                    let same = sizes.windows(2).all(|w| w[0] == w[1]);
                    println!("{layout:>8} size {}", if same { "constant" } else { "VARIES" });
                    hud.close(cx);
                }
                let _ = std::fs::remove_file(&image_path);
                cx.quit();
            });
        })
        .detach();
    });
}
