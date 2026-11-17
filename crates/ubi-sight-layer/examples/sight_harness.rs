//! Runs the test app (tests/harness) through the layer, for screenshots and overhead numbers.
//!
//! ```text
//! cargo run --release -p ubi-sight-layer --example sight_harness -- [options]
//!   --layer off|passthrough|telemetry|hud   (default hud)
//!   --frames N --fps F --srgb --device <name substring> --validation
//!   --window                                 an X11 window instead of headless
//!   --screenshot <path.png>                  with --window: grim the window after 2.5 s
//! ```
//!
//! `off`: no layer. `passthrough`: the layer without `QUIVER_PERF_DIR`. `telemetry`: ring and
//! status, no drawing (`QUIVER_SIGHT_DRAW=0`). `hud`: everything, with a test HUD image that
//! changes every 0.5 s.

#[path = "../tests/harness/mod.rs"]
mod harness;

use std::process::Command;
use std::time::Duration;

use harness::*;
use ubi_perf::record::LayerPresent;
use ubi_perf::session::open_ring_reader;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut mode, mut frames, mut fps, mut srgb, mut device, mut validation, mut window, mut shot) =
        ("hud".to_owned(), 1000u32, 240.0, false, None, false, false, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--layer" => mode = args.next().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--fps" => fps = args.next().unwrap().parse().unwrap(),
            "--srgb" => srgb = true,
            "--device" => device = args.next(),
            "--validation" => validation = true,
            "--window" => window = true,
            "--screenshot" => shot = args.next(),
            _ => panic!("unknown argument {a}"),
        }
    }
    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    let vdir = validation.then(|| validation_dir().expect("no validation layer found"));
    // SAFETY: single-threaded so far (the session's heartbeat thread doesn't read the env).
    unsafe {
        if mode != "off" {
            for (k, v) in layer_env(scratch.path(), vdir.as_deref()) {
                std::env::set_var(k, v);
            }
        }
        if mode == "telemetry" || mode == "hud" {
            std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        }
        if mode == "telemetry" {
            std::env::set_var("QUIVER_SIGHT_DRAW", "0");
        }
    }
    let title = format!("quiver-sight-harness-{}", std::process::id());
    let mut shot_taken = false;
    let mut last = (u32::MAX, (0, 0));
    let r = run(Opts {
        frames,
        fps,
        srgb,
        device: device.or(Opts::default().device),
        surface: if window { Surface::Window { title: title.clone() } } else { Surface::Headless },
        readback: false,
        hitches: (1..frames / 100).map(|k| k * 100 + 37).collect(),
        debug_utils: validation,
        on_frame: Some(Box::new(|_, t, extent| {
            let variant = (t.as_millis() / 500) as u32;
            // quiver-sight publishes a new image on a model change or a new extent.
            if (variant, extent) != last {
                last = (variant, extent);
                session.publish(&test_image(extent, (24, 24), (380, 190), variant, 0.92));
            }
            if let Some(path) = shot.as_deref()
                && !shot_taken
                && t > Duration::from_millis(2500)
            {
                shot_taken = true;
                screenshot(&title, path);
            }
        })),
        ..Opts::default()
    });
    for m in &r.messages {
        println!("{m}");
    }
    let present = &r.present_ns;
    println!(
        "{} {:?} {}x{} ({} recreations) layer={mode}: {} presents, app-side vkQueuePresentKHR median {:.1} us, p99 {:.1} us; validation messages {}",
        r.device_name,
        r.format,
        r.extent.0,
        r.extent.1,
        r.recreations,
        present.len(),
        median(present) as f64 / 1e3,
        percentile(present, 0.99) as f64 / 1e3,
        r.messages.len()
    );
    if let Ok(mut ring) = open_ring_reader(&session.ring_path()) {
        let mut hud = Vec::new();
        let mut inner = Vec::new();
        let mut n = 0;
        ring.poll(usize::MAX, |_, rec| {
            if let Some(p) = LayerPresent::from_record(rec) {
                n += 1;
                inner.push((p.return_ns - p.entry_ns) as u64);
                if p.hud_drawn {
                    hud.push(p.hud_ns as u64);
                }
            }
        });
        println!(
            "ring: {n} records; layer entry→return median {:.1} us; HUD CPU per drawn present ({}): median {:.1} us, p99 {:.1} us, max {:.1} us",
            median(&inner) as f64 / 1e3,
            hud.len(),
            median(&hud) as f64 / 1e3,
            percentile(&hud, 0.99) as f64 / 1e3,
            percentile(&hud, 1.0) as f64 / 1e3
        );
    }
}

/// `grim` the test window, found by title in `hyprctl clients -j`.
fn screenshot(title: &str, path: &str) {
    let out = Command::new("hyprctl").args(["clients", "-j"]).output().expect("hyprctl");
    let clients: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let c = clients.as_array().unwrap().iter().find(|c| c["title"] == title).expect("test window not found");
    let (x, y, w, h) = (c["at"][0].as_i64().unwrap(), c["at"][1].as_i64().unwrap(), c["size"][0].as_i64().unwrap(), c["size"][1].as_i64().unwrap());
    let st = Command::new("grim").args(["-g", &format!("{x},{y} {w}x{h}"), path]).status().expect("grim");
    assert!(st.success());
    println!("screenshot {path} ({w}x{h} at {x},{y})");
}
