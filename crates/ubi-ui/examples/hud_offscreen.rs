//! Renders a HUD-like view built from `ubi-ui` components into an off-screen GPUI window
//! (`App::open_offscreen_window`, the patched GPUI in `third_party/zed`) and writes PNGs.
//! This is the path `quiver-sight` uses for the in-game HUD (SIGHT-HUD.md §2.9).
//!
//! ```text
//! cargo run -p ubi-ui --example hud_offscreen --release -- [--scale 2] [--headless] [--out DIR] [--frames N]
//! env -u WAYLAND_DISPLAY -u DISPLAY cargo run -p ubi-ui --example hud_offscreen --release
//! ```
//!
//! Writes `DIR/hud-<scale>x-<n>.png` (straight alpha, as PNG stores it) and
//! `DIR/hud-<scale>x-preview.png` (composited over a gradient, the way the Sight layer blends
//! it onto a game frame), then prints timings and RSS. `--headless` forces GPUI's headless
//! platform even when a display server is available; otherwise the platform follows the
//! environment (Wayland or X11 if `WAYLAND_DISPLAY`/`DISPLAY` is set, else headless). The
//! off-screen window never uses the display connection either way. A sample 2x preview is
//! next to this file (`hud_offscreen-2x.png`).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Context, Div, Hsla, IntoElement, OffscreenWindowOptions, ParentElement, Render, SharedString,
    Styled, Window, div, px, size,
};
use ubi_ui::assets::{self, UI_FONT};
use ubi_ui::components::{Icon, PillKind, SEMIBOLD, badge, caption, icon, mono, pill, progress};
use ubi_ui::theme::{self, Theme, rgba};

/// Values one HUD frame shows (quiverd formats these from its metrics in the real thing).
#[derive(Clone)]
struct Stats {
    fps: u32,
    base: u32,
    frame_ms: f32,
    low_1: u32,
    latency_ms: u32,
    /// Latency split: simulation, render submit, queue, GPU, present (fractions of the bar).
    split: [f32; 5],
    gpu_pct: u32,
    gpu_temp: u32,
    gpu_w: u32,
    vram: (f32, f32),
    cpu_pct: u32,
    cpu_temp: u32,
    /// Presented frame times (ms), oldest first.
    frame_times: Vec<f32>,
}

impl Stats {
    fn sample(i: u32) -> Self {
        let wobble = [0i32, 3, -2, 5, -4, 1, 2, -1][i as usize % 8];
        let fps = (158 + wobble) as u32;
        let frame_times = (0..48)
            .map(|k| {
                let k = k + i as usize;
                let spike = if k % 29 == 17 { 9.0 } else if k % 11 == 3 { 2.5 } else { 0.0 };
                6.3 + ((k * 7) % 5) as f32 * 0.35 + spike
            })
            .collect();
        Self {
            fps,
            base: fps / 2,
            frame_ms: 1000.0 / fps as f32,
            low_1: (128 + wobble / 2) as u32,
            latency_ms: (21 + wobble / 2) as u32,
            split: [0.2, 0.1, 0.27, 0.34, 0.09],
            gpu_pct: (97 - wobble.abs()) as u32,
            gpu_temp: 71,
            gpu_w: (92 + wobble) as u32,
            vram: (6.1, 8.0),
            cpu_pct: (38 - wobble) as u32,
            cpu_temp: 64,
            frame_times,
        }
    }
}

struct Hud {
    stats: Stats,
}

fn row() -> Div {
    div().flex().items_center().gap(px(8.))
}

fn key(th: &Theme, text: &str) -> Div {
    caption(text, th.tx3).w(px(30.)).flex_none()
}

fn value(th: &Theme, text: impl Into<SharedString>, color: Option<Hsla>) -> Div {
    mono(12., color.unwrap_or(th.tx1)).font_weight(SEMIBOLD).child(text.into())
}

/// Frame-time bars, tallest = slowest; spikes over 1.5x the median are coloured as warnings.
fn sparkline(th: &Theme, times: &[f32], height: f32) -> Div {
    let mut sorted = times.to_vec();
    sorted.sort_by(f32::total_cmp);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(1.0);
    let max = sorted.last().copied().unwrap_or(1.0).max(median * 2.0);
    div().flex().items_end().gap(px(1.)).h(px(height)).children(times.iter().map(|t| {
        let color = if *t > median * 1.5 { th.warn } else { th.acc };
        div().w(px(3.)).h(px((t / max * height).max(1.))).rounded(px(1.)).bg(color.opacity(0.85))
    }))
}

impl Render for Hud {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        let s = &self.stats;
        let seg_colors = [th.acc, th.acc_hi, th.warn, th.ok, th.tx4];

        let fps = row()
            .items_baseline()
            .child(key(&th, "FPS"))
            .child(mono(22., th.tx1).font_weight(SEMIBOLD).child(format!("{} → {}", s.base, s.fps)))
            .child(div().flex_1())
            .child(caption("1% low", th.tx3))
            .child(value(&th, s.low_1.to_string(), Some(th.tx2)));
        let ft = row()
            .child(key(&th, "FT"))
            .child(value(&th, format!("{:.1} ms", s.frame_ms * 2.0), None))
            .child(caption("base", th.tx3))
            .child(value(&th, format!("{:.1} ms", s.frame_ms), Some(th.tx2)))
            .child(caption("presented", th.tx3));
        let gpu = row()
            .child(key(&th, "GPU"))
            .child(value(&th, format!("{}%", s.gpu_pct), None))
            .child(value(&th, format!("{}°C", s.gpu_temp), Some(th.tx2)))
            .child(value(&th, format!("{} W", s.gpu_w), Some(th.tx2)))
            .child(div().flex_1())
            .child(progress(&th, s.vram.0 / s.vram.1, Some(46.), 4., None))
            .child(mono(11., th.tx2).child(format!("{:.1}/{:.0} GB", s.vram.0, s.vram.1)));
        let cpu = row()
            .child(key(&th, "CPU"))
            .child(value(&th, format!("{}%", s.cpu_pct), None))
            .child(value(&th, format!("{}°C", s.cpu_temp), Some(th.tx2)))
            .child(div().flex_1())
            .child(caption("RAM", th.tx3))
            .child(value(&th, "11.2 GB", Some(th.tx2)));
        let dlss = row()
            .child(key(&th, "DLSS"))
            .child(pill(&th, PillKind::Accent, Some(Icon::Sparkles), "K · FG 2×"))
            .child(mono(11., th.tx2).child("800p → 1600p"))
            .child(div().flex_1())
            .child(badge(&th, "1.6 ms", false));
        let bar = div().flex().flex_1().h(px(6.)).gap(px(2.)).children(
            s.split
                .iter()
                .zip(seg_colors)
                .map(|(f, c)| div().h_full().flex_grow(*f).flex_basis(px(0.)).rounded(px(2.)).bg(c)),
        );
        let lat = row()
            .child(key(&th, "LAT"))
            .child(value(&th, format!("{} ms", s.latency_ms), None))
            .child(bar)
            .child(icon(Icon::Cpu, 12., th.tx3));

        // The window is transparent; the card is the only thing drawn.
        div().size_full().child(
            div()
                .w(px(320.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .pt(px(9.))
                .pb(px(11.))
                .rounded(px(10.))
                .bg(rgba(0x080a0e, 0.66))
                .border_1()
                .border_color(rgba(0xffffff, 0.08))
                .font_family(UI_FONT)
                .text_color(th.tx1)
                .child(fps)
                .child(ft)
                .child(sparkline(&th, &s.frame_times, 22.))
                .child(gpu)
                .child(cpu)
                .child(dlss)
                .child(lat),
        )
    }
}

/// An owned copy of a delivered frame.
#[derive(Default, Clone)]
struct Captured {
    width: u32,
    height: u32,
    stride: u32,
    data: Vec<u8>,
    render_duration: Duration,
    /// Copying the pixels out of the mapped readback buffer (the spike saw 4.6 ms for this
    /// from uncached memory, about 10 µs from host-cached memory).
    copy_duration: Duration,
    delivered_at: Option<Instant>,
}

struct Args {
    scale: f32,
    headless: bool,
    out: PathBuf,
    warm_frames: u32,
}

fn args() -> Args {
    let mut a = Args { scale: 1.0, headless: false, out: PathBuf::from("target/hud-offscreen"), warm_frames: 300 };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--scale" => a.scale = it.next().and_then(|v| v.parse().ok()).expect("--scale N"),
            "--headless" => a.headless = true,
            "--out" => a.out = it.next().map(PathBuf::from).expect("--out DIR"),
            "--frames" => a.warm_frames = it.next().and_then(|v| v.parse().ok()).expect("--frames N"),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// Premultiplied RGBA -> straight RGBA (what PNG stores).
fn unpremultiply(c: &Captured) -> image::RgbaImage {
    let mut img = image::RgbaImage::new(c.width, c.height);
    for y in 0..c.height {
        let row = &c.data[(y * c.stride) as usize..][..(c.width * 4) as usize];
        for x in 0..c.width {
            let p = &row[(x * 4) as usize..][..4];
            let a = p[3];
            let un = |v: u8| if a == 0 { 0 } else { ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
            img.put_pixel(x, y, image::Rgba([un(p[0]), un(p[1]), un(p[2]), a]));
        }
    }
    img
}

/// Premultiplied "over" onto a gradient, like the layer blends it onto a game frame.
fn preview(c: &Captured) -> image::RgbImage {
    let mut img = image::RgbImage::new(c.width + 32, c.height + 32);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let t = (x + y) as f32 / (c.width + c.height + 64) as f32;
        *px = image::Rgb([(70.0 + 90.0 * t) as u8, (90.0 + 40.0 * t) as u8, (60.0 + 20.0 * t) as u8]);
    }
    for y in 0..c.height {
        for x in 0..c.width {
            let i = (y * c.stride + x * 4) as usize;
            let s = &c.data[i..i + 4];
            let d = img.get_pixel_mut(x + 16, y + 16);
            let ia = 255 - s[3] as u32;
            for (dst, src) in d.0.iter_mut().zip(s) {
                *dst = (*src as u32 + (*dst as u32 * ia + 127) / 255).min(255) as u8;
            }
        }
    }
    img
}

/// Bounding box of non-transparent pixels, and whether any pixel breaks premultiplication
/// (a colour channel above alpha).
fn inspect(c: &Captured) -> ((u32, u32), usize, usize) {
    let (mut w, mut h, mut bad, mut clear) = (0, 0, 0, 0);
    for y in 0..c.height {
        for x in 0..c.width {
            let i = (y * c.stride + x * 4) as usize;
            let p = &c.data[i..i + 4];
            if p[3] != 0 {
                w = w.max(x + 1);
                h = h.max(y + 1);
            } else {
                clear += 1;
            }
            if p[0] > p[3] || p[1] > p[3] || p[2] > p[3] {
                bad += 1;
            }
        }
    }
    ((w, h), clear, bad)
}

fn rss_kib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let process_start = Instant::now();
    let args = args();
    std::fs::create_dir_all(&args.out).expect("create output dir");
    let app = if args.headless { gpui_platform::headless() } else { gpui_platform::application() };
    let client = if args.headless {
        "headless (forced)".to_string()
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "Wayland".to_string()
    } else if std::env::var_os("DISPLAY").is_some() {
        "X11".to_string()
    } else {
        "headless (no display in the environment)".to_string()
    };

    app.with_assets(assets::Assets).run(move |cx: &mut App| {
        let app_ready = process_start.elapsed();
        let rss_before_window = rss_kib().0;
        let th = if std::env::var("HUD_THEME").is_ok_and(|v| v == "omarchy") {
            theme::load(&theme::omarchy_state_dir())
        } else {
            Theme::builtin()
        };
        ubi_ui::init(cx, th);

        let last: Rc<RefCell<Captured>> = Rc::default();
        let frames_delivered = Rc::new(RefCell::new(0u32));
        let scale = args.scale;
        let open_started = Instant::now();
        let window = cx
            .open_offscreen_window(
                OffscreenWindowOptions {
                    size: size(px(320.), px(216.)),
                    scale_factor: scale,
                    ..Default::default()
                },
                {
                    let last = last.clone();
                    let frames_delivered = frames_delivered.clone();
                    move |f| {
                        let mut c = last.borrow_mut();
                        c.width = f.width;
                        c.height = f.height;
                        c.stride = f.stride;
                        let copy_started = Instant::now();
                        c.data.clear();
                        c.data.extend_from_slice(f.data);
                        c.copy_duration = copy_started.elapsed();
                        c.render_duration = f.render_duration;
                        c.delivered_at = Some(Instant::now());
                        *frames_delivered.borrow_mut() += 1;
                    }
                },
                |_, cx| cx.new(|_| Hud { stats: Stats::sample(0) }),
            )
            .expect("open off-screen window");
        let open_took = open_started.elapsed();
        if let Ok(Some(gpu)) = window.update(cx, |_, window, _| window.gpu_specs()) {
            println!("gpu: {} ({}, {}), software: {}", gpu.device_name, gpu.driver_name, gpu.driver_info, gpu.is_software_emulated);
        }
        let out = args.out.clone();
        let warm_frames = args.warm_frames;

        cx.spawn(async move |cx| {
            let tag = format!("{scale}x");
            // Phase 1: demand-driven frames. Update the view and `notify`; the window wakes, asks
            // for a frame on the foreground executor and delivers it (no compositor involved).
            let mut first_frame = None;
            for i in 0..5u32 {
                let before = *frames_delivered.borrow();
                let notified_at = Instant::now();
                window
                    .update(cx, |hud, _, cx| {
                        hud.stats = Stats::sample(i);
                        cx.notify();
                    })
                    .unwrap();
                while *frames_delivered.borrow() == before {
                    cx.background_executor().timer(Duration::from_millis(1)).await;
                }
                let c = last.borrow().clone();
                if i == 0 {
                    first_frame = Some(process_start.elapsed());
                }
                let latency = c.delivered_at.unwrap() - notified_at;
                let png = out.join(format!("hud-{tag}-{i}.png"));
                unpremultiply(&c).save(&png).expect("write png");
                println!(
                    "notify frame {i}: {}x{} stride {} render {:?} notify->frame {:?} -> {}",
                    c.width,
                    c.height,
                    c.stride,
                    c.render_duration,
                    latency,
                    png.display()
                );
                if i == 0 {
                    preview(&c).save(out.join(format!("hud-{tag}-preview.png"))).expect("write preview");
                    let ((w, h), clear, bad) = inspect(&c);
                    println!(
                        "content bounds {w}x{h} device px; {clear} fully transparent px; {bad} px with colour > alpha"
                    );
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            let idle_before = *frames_delivered.borrow();
            cx.background_executor().timer(Duration::from_millis(300)).await;
            println!("frames while idle for 300 ms: {}", *frames_delivered.borrow() - idle_before);

            // Phase 2: warm timing with synchronous frames (`Window::render_now`): update,
            // layout, paint, GPU render, readback, callback.
            let mut total = Vec::new();
            let mut render = Vec::new();
            let mut draw = Vec::new();
            let mut copy = Vec::new();
            for i in 0..warm_frames {
                let t = Instant::now();
                window
                    .update(cx, |hud, _, cx| {
                        hud.stats = Stats::sample(i);
                        cx.notify();
                    })
                    .unwrap();
                let t_draw = Instant::now();
                // Through the untyped handle, so the root view isn't leased while it renders.
                cx.update_window(window.into(), |_, window, cx| window.render_now(cx)).unwrap();
                draw.push(t_draw.elapsed());
                total.push(t.elapsed());
                render.push(last.borrow().render_duration);
                copy.push(last.borrow().copy_duration);
            }
            copy.sort();
            total.sort();
            render.sort();
            draw.sort();
            let (rss, hwm) = rss_kib();
            println!("platform: {client}, scale {scale}");
            println!("app ready (process start -> run callback): {app_ready:?}");
            println!("open_offscreen_window (device + pipelines): {open_took:?}");
            println!("first frame delivered (process start): {:?}", first_frame.unwrap());
            println!(
                "warm frame total (update+layout+paint+render+readback), {} frames: p50 {:?} p90 {:?} p99 {:?} max {:?}",
                total.len(),
                percentile(&total, 0.5),
                percentile(&total, 0.9),
                percentile(&total, 0.99),
                total.last().unwrap()
            );
            println!(
                "  of which render_now (layout+paint+render+readback): p50 {:?} p90 {:?} p99 {:?}",
                percentile(&draw, 0.5),
                percentile(&draw, 0.9),
                percentile(&draw, 0.99)
            );
            println!(
                "  of which renderer (encode+GPU+wait+readback): p50 {:?} p90 {:?} p99 {:?}",
                percentile(&render, 0.5),
                percentile(&render, 0.9),
                percentile(&render, 0.99)
            );
            println!(
                "  copy out of the mapped readback buffer ({} KiB): p50 {:?} p90 {:?} p99 {:?}",
                last.borrow().data.len() / 1024,
                percentile(&copy, 0.5),
                percentile(&copy, 0.9),
                percentile(&copy, 0.99)
            );
            println!("frames delivered: {}", frames_delivered.borrow());
            println!(
                "RSS: {} MiB (peak {} MiB); before the window/GPU init: {} MiB",
                rss / 1024,
                hwm / 1024,
                rss_before_window / 1024
            );
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
