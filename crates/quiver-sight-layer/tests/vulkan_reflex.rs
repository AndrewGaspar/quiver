//! Reflex through the layer on a real NVIDIA driver: a Vulkan app (tests/harness) enables
//! `VK_NV_low_latency2`, sets a sleep mode, sleeps and sets latency markers each frame as a
//! Reflex game does (or as dxvk-nvapi does on a game's behalf), and presents one or more
//! times per application frame like a frame generator. The layer's status block must report
//! the mode and cap, count application frames and presents, and carry PC latency; the app's
//! own `vkGetLatencyTimingsNV` reads must still get their reports with the layer's opt-in
//! reader on (and two reads in a row show whether a read consumes them).
//!
//! Needs an NVIDIA GPU (the only driver with the extension) and Xvfb (NVIDIA has no
//! `VK_EXT_headless_surface`; the window goes to a private Xvfb, never the desktop; set
//! `QUIVER_TEST_REFLEX_DESKTOP=1` to use `DISPLAY` instead). Opt-in:
//! `cargo test -p quiver-sight-layer --test vulkan_reflex -- --ignored --nocapture`
//! (`QUIVER_TEST_VK_DEVICE=<name substring>` picks the device).

mod harness;
mod xvfb;

use std::time::Duration;

use harness::*;
use quiver_perf::LayerStatus;
use quiver_perf::layout::{
    LAYER_METRIC_PRESENT, REFLEX_BOOST, REFLEX_COUNTING_MARKERS, REFLEX_COUNTING_SLEEP, REFLEX_LOW_LATENCY, REFLEX_MARKERS, REFLEX_MODE_SET, REFLEX_OUT_OF_BAND,
    REFLEX_SLEEPING, REFLEX_TIMINGS,
};
use quiver_perf::record::LayerPresent;
use quiver_perf::session::open_ring_reader;

fn status_now(session: &TestSession) -> Option<LayerStatus> {
    open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status())
}

#[test]
#[ignore = "needs an NVIDIA GPU with VK_NV_low_latency2 and an X display; see the module docs"]
fn reflex_reaches_the_status_block() {
    let x = if std::env::var("QUIVER_TEST_REFLEX_DESKTOP").as_deref() == Ok("1") { None } else { Some(xvfb::Xvfb::start().expect("Xvfb")) };
    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    // SAFETY: the only test in this binary; nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
        std::env::set_var("QUIVER_SIGHT_REFLEX_TIMINGS", "1");
        if let Some(x) = &x {
            std::env::set_var("DISPLAY", &x.display);
        }
        for (k, v) in layer_env(scratch.path(), None) {
            std::env::set_var(k, v);
        }
    }
    let device = Some(std::env::var("QUIVER_TEST_VK_DEVICE").unwrap_or_else(|_| "NVIDIA".into()));
    let window = || Surface::Window { title: "quiver-sight reflex test".into() };
    let mid = std::cell::RefCell::new(None);
    let frames = 240;

    // 1. Reflex on with boost and a 120 fps cap, sleeps and markers, 2× "frame generation".
    let r = run(Opts {
        frames,
        fps: 90.0,
        surface: window(),
        readback: false,
        device: device.clone(),
        reflex: Some(ReflexOpts { mode: Some((true, true, 8333)), sleep: true, markers: true, extra_presents: 1, out_of_band: false }),
        on_frame: Some(Box::new(|f, _, _| {
            if f == frames - 1 {
                *mid.borrow_mut() = status_now(&session);
            }
        })),
        ..Opts::default()
    });
    eprintln!("device {} present_id {}", r.device_name, r.present_id);
    let s = mid.borrow().expect("a status block while the swapchain lived");
    eprintln!(
        "1: flags {:#x} interval {} us, markers {}, sleeps {}, frames {}, presents {}, PC latency {} samples mean {} us, render latency {} samples mean {} us",
        s.reflex_flags,
        s.reflex_min_interval_us,
        s.reflex_markers,
        s.reflex_sleeps,
        s.app_frames,
        s.app_presents,
        s.pc_latency_frames,
        s.pc_latency_sum_us / s.pc_latency_frames.max(1),
        s.render_latency_frames,
        s.render_latency_sum_us / s.render_latency_frames.max(1)
    );
    let want = REFLEX_MODE_SET | REFLEX_LOW_LATENCY | REFLEX_BOOST | REFLEX_MARKERS | REFLEX_SLEEPING | REFLEX_COUNTING_SLEEP | REFLEX_TIMINGS;
    assert_eq!(s.reflex_flags & want, want, "flags {:#x}", s.reflex_flags);
    assert_eq!(s.reflex_min_interval_us, 8333);
    assert!(s.reflex_sleeps >= frames as u64 - 40, "sleeps {}", s.reflex_sleeps);
    let ratio = s.app_presents as f64 / s.app_frames.max(1) as f64;
    assert!((ratio - 2.0).abs() < 0.05, "presents per frame {ratio}");
    assert!(s.pc_latency_frames >= frames as u64 - 40, "PC latency samples {}", s.pc_latency_frames);
    let pc = s.pc_latency_sum_us / s.pc_latency_frames;
    assert!(pc > 0 && pc < 100_000, "PC latency {pc} us");
    // The app's own reads with the layer's reader on: both get reports.
    let rep = r.reflex.as_ref().unwrap();
    eprintln!("app timing reads: {} then {} reports; ids {:?}… / {:?}…; newest game-to-render {:?} us", rep.timings[0].0, rep.timings[1].0, rep.timings[0].1.iter().rev().take(3).collect::<Vec<_>>(), rep.timings[1].1.iter().rev().take(3).collect::<Vec<_>>(), rep.last_render_us);
    assert!(rep.timings[0].0 > 0, "the app got no timing reports");
    if rep.timings[0] == rep.timings[1] {
        eprintln!("a read doesn't consume the reports on this driver (the same {} reports twice)", rep.timings[0].0);
    } else {
        eprintln!("NOTE: the second read differs from the first: reads may consume reports");
    }
    // Present records: REAL then GENERATED for each frame while counting.
    let mut reader = open_ring_reader(&session.ring_path()).unwrap();
    let (mut real, mut further) = (0, 0);
    reader.poll(10_000, |_, rec| {
        if rec.metric == LAYER_METRIC_PRESENT
            && let Some(p) = LayerPresent::from_record(rec)
        {
            match p.app_frame {
                Some(true) => real += 1,
                Some(false) => further += 1,
                None => {}
            }
        }
    });
    eprintln!("present records: {real} first-of-frame, {further} further");
    assert!(real > 0 && (further as f64 / real as f64 - 1.0).abs() < 0.05);

    // 2. Markers only (no sleep), out-of-band present markers around 3 presents per frame (4×).
    *mid.borrow_mut() = None;
    run(Opts {
        frames: 120,
        fps: 60.0,
        surface: window(),
        readback: false,
        device: device.clone(),
        reflex: Some(ReflexOpts { mode: Some((false, false, 0)), sleep: false, markers: true, extra_presents: 3, out_of_band: true }),
        on_frame: Some(Box::new(|f, _, _| {
            if f == 119 {
                *mid.borrow_mut() = status_now(&session);
            }
        })),
        ..Opts::default()
    });
    let s = mid.borrow().unwrap();
    eprintln!("2: flags {:#x}, frames {}, presents {}, PC latency samples {}", s.reflex_flags, s.app_frames, s.app_presents, s.pc_latency_frames);
    assert_eq!(s.reflex_flags & (REFLEX_LOW_LATENCY | REFLEX_COUNTING_SLEEP), 0);
    assert_ne!(s.reflex_flags & REFLEX_COUNTING_MARKERS, 0);
    assert_ne!(s.reflex_flags & REFLEX_OUT_OF_BAND, 0);
    let ratio = s.app_presents as f64 / s.app_frames.max(1) as f64;
    assert!((ratio - 4.0).abs() < 0.1, "presents per frame {ratio}");

    // 3. The extension on but unused: nothing counted, no numbers.
    *mid.borrow_mut() = None;
    run(Opts {
        frames: 60,
        surface: window(),
        readback: false,
        device,
        reflex: Some(ReflexOpts::default()),
        on_frame: Some(Box::new(|f, _, _| {
            if f == 59 {
                *mid.borrow_mut() = status_now(&session);
            }
        })),
        ..Opts::default()
    });
    let s = mid.borrow().unwrap();
    eprintln!("3: flags {:#x}, frames {}, presents {}", s.reflex_flags, s.app_frames, s.app_presents);
    assert_eq!(s.reflex_flags & (REFLEX_MODE_SET | REFLEX_COUNTING_SLEEP | REFLEX_COUNTING_MARKERS), 0);
    assert_eq!((s.app_frames, s.app_presents, s.pc_latency_frames), (0, 0, 0));
    std::thread::sleep(Duration::from_millis(300));
}
