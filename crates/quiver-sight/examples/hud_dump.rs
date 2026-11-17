//! Reads `hud.image` the way the Sight layer does (`HudReader`, validated against the
//! swapchain extent the image was laid out for) and saves what quiver-sight published.
//!
//! ```text
//! cargo run -p quiver-sight --example hud_dump -- HUD_IMAGE OUT_DIR [--count N] [--timeout SECS]
//! ```
//!
//! For each new publish (up to `--count`, default 1) writes `OUT_DIR/hud-r<rev>.png` (the
//! image, straight alpha) and `OUT_DIR/hud-r<rev>-frame.png` (composited at its position onto
//! a stand-in game frame of the swapchain's size, graph rectangles outlined, cropped to the
//! HUD's quadrant), and prints the descriptor, the heartbeat age and the writer pid.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use quiver_perf::hud::HudPoll;
use quiver_perf::layout::HUD_HEADER_BYTES;
use quiver_perf::session::open_hud_reader;

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("HUD_IMAGE"));
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    let (mut count, mut timeout) = (1u32, 10.0f32);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--count" => count = args.next().unwrap().parse().unwrap(),
            "--timeout" => timeout = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&out).unwrap();
    let mut reader = open_hud_reader(&path).expect("open hud.image");
    let bb = reader.info().buffer_bytes as usize;
    let mut dst = vec![0u8; bb];
    let start = Instant::now();
    let mut got = 0;
    while got < count && start.elapsed().as_secs_f32() < timeout {
        let p = reader.published();
        if p == 0 {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        // The extent the current image was laid out for (the layer knows its own; we read it).
        let raw = std::fs::read(&path).unwrap();
        let base = HUD_HEADER_BYTES + (p % 2) as usize * bb;
        let extent = (u32_at(&raw, base + 28), u32_at(&raw, base + 32));
        match reader.poll(extent, &mut dst) {
            HudPoll::Image { published, desc } => {
                got += 1;
                let age = (quiver_perf::clock::mono_raw_ns() - reader.heartbeat()) / 1_000_000;
                println!(
                    "publish {published}: rev {} {}x{} stride {} at ({}, {}) on {}x{}, scale {:.3}, opacity {}, graphs {}; heartbeat {age} ms ago, writer pid {}",
                    desc.model_rev, desc.width, desc.height, desc.stride, desc.x, desc.y, desc.extent_w, desc.extent_h, desc.scale, desc.opacity, desc.ngraphs, reader.writer_pid()
                );
                for g in desc.graphs() {
                    println!("  graph {}x{} at ({}, {}) span {} ms, range {}..{} ms, warn > {} ms, bad > {} ms", g.w, g.h, g.x, g.y, g.span_ms, g.min_ms, g.max_ms, g.warn_above_ms, g.bad_above_ms);
                }
                save(&out, &desc, &dst);
            }
            HudPoll::Unchanged | HudPoll::Busy | HudPoll::Torn => std::thread::sleep(Duration::from_millis(20)),
            HudPoll::Invalid(e) => {
                println!("publish {p}: invalid: {e}");
                got += 1;
            }
        }
    }
    if got < count {
        eprintln!("only {got} of {count} images within {timeout} s");
        std::process::exit(1);
    }
}

fn save(out: &std::path::Path, d: &quiver_perf::HudBufferHeader, px: &[u8]) {
    let (w, h, stride) = (d.width, d.height, d.stride as usize);
    let mut img = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let p = &px[y as usize * stride + x as usize * 4..][..4];
            let a = p[3];
            let un = |v: u8| if a == 0 { 0 } else { ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
            img.put_pixel(x, y, image::Rgba([un(p[0]), un(p[1]), un(p[2]), a]));
        }
    }
    let name = format!("hud-r{}", d.model_rev);
    img.save(out.join(format!("{name}.png"))).unwrap();
    // A stand-in frame: the HUD's quadrant of the swapchain, blended like the layer does
    // (premultiplied over, times the descriptor's opacity).
    let (ew, eh) = (d.extent_w, d.extent_h);
    let (qx, qy) = (if d.x as u32 + w / 2 > ew / 2 { ew / 2 } else { 0 }, if d.y as u32 + h / 2 > eh / 2 { eh / 2 } else { 0 });
    let (qw, qh) = (ew / 2, eh / 2);
    let mut frame = image::RgbImage::from_fn(qw, qh, |x, y| {
        let (fx, fy) = ((qx + x) as f32 / ew as f32, (qy + y) as f32 / eh as f32);
        let hill = ((fx * 9.0).sin() * 0.5 + 0.5) * 0.25;
        image::Rgb([(70.0 + 120.0 * fy + 60.0 * hill) as u8, (110.0 + 60.0 * fx) as u8, (150.0 - 50.0 * fy + 40.0 * hill) as u8])
    });
    let o = d.opacity;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (d.x as u32 + x, d.y as u32 + y);
            if fx < qx || fy < qy || fx >= qx + qw || fy >= qy + qh {
                continue;
            }
            let s = &px[y as usize * stride + x as usize * 4..][..4];
            let dp = frame.get_pixel_mut(fx - qx, fy - qy);
            let ia = 1.0 - s[3] as f32 / 255.0 * o;
            for (d, s) in dp.0.iter_mut().zip(s) {
                *d = (*s as f32 * o + *d as f32 * ia).round().min(255.0) as u8;
            }
        }
    }
    for g in d.graphs() {
        for k in 0..g.w.max(0) as u32 {
            for (gx, gy) in [(g.x as u32 + k, g.y as u32), (g.x as u32 + k, (g.y + g.h - 1) as u32)] {
                let (fx, fy) = (d.x as u32 + gx, d.y as u32 + gy);
                if fx >= qx && fy >= qy && fx < qx + qw && fy < qy + qh {
                    frame.put_pixel(fx - qx, fy - qy, image::Rgb([255, 0, 255]));
                }
            }
        }
    }
    frame.save(out.join(format!("{name}-frame.png"))).unwrap();
}
