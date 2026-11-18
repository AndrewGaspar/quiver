//! The backdrop blur behind the Sight panel (design/SIGHT.md §14) in a real Vulkan app
//! (tests/harness): with quiver-sight's request in `hud.image` the layer blurs the game's
//! frame (striped here, so the blur is measurable), under a sharp HUD; the dim applies; a
//! cleared or expired request leaves the frame alone; sRGB swapchains blur in linear; HDR10
//! and scRGB swapchains blur when the surface offers them; the status block says ready,
//! blurring and the GPU time. Ends with the cost at 1080p, 1440p and 4K.
//!
//! Needs a GPU whose driver supports `VK_EXT_headless_surface` (Mesa: RADV, ANV), so it's
//! opt-in: `cargo test -p quiver-sight-layer --test vulkan_blur -- --ignored --nocapture`.
//! `QUIVER_TEST_VK_DEVICE=<name substring>` picks the device; `QUIVER_TEST_VALIDATION=0`
//! skips validation; `QUIVER_TEST_SIGHT_PNG=<dir>` writes the read-back frames there.

mod harness;

use std::cell::RefCell;
use std::time::Duration;

use ash::vk;
use harness::*;
use quiver_perf::LayerStatus;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::BackdropRequest;
use quiver_perf::layout::{LAYER_BLUR_READY, LAYER_BLURRING};
use quiver_perf::record::LayerPresent;
use quiver_perf::session::open_ring_reader;

fn save(name: &str, w: u32, h: u32, px: &[u8]) {
    if let Some(dir) = std::env::var_os("QUIVER_TEST_SIGHT_PNG") {
        let p = std::path::Path::new(&dir).join(name);
        image::save_buffer(&p, px, w, h, image::ExtendedColorType::Rgba8).unwrap();
        eprintln!("wrote {}", p.display());
    }
}

/// Min, max and mean of the red channel (8-bit formats) along row `y`, `x0..x1`.
fn row(px: &[u8], w: u32, y: u32, x0: u32, x1: u32) -> (u8, u8, f32) {
    let v: Vec<u8> = (x0..x1).map(|x| px[((y * w + x) * 4) as usize]).collect();
    (*v.iter().min().unwrap(), *v.iter().max().unwrap(), v.iter().map(|&c| c as f32).sum::<f32>() / v.len() as f32)
}

/// The same for the red channel of a 10-bit packed format, scaled to 0..255.
fn row10(px: &[u8], w: u32, y: u32, x0: u32, x1: u32, red_low: bool) -> (u8, u8, f32) {
    let v: Vec<f32> = (x0..x1)
        .map(|x| {
            let o = ((y * w + x) * 4) as usize;
            let word = u32::from_le_bytes(px[o..o + 4].try_into().unwrap());
            let r = if red_low { word & 0x3ff } else { (word >> 20) & 0x3ff };
            r as f32 * 255.0 / 1023.0
        })
        .collect();
    let min = v.iter().cloned().fold(f32::MAX, f32::min);
    let max = v.iter().cloned().fold(f32::MIN, f32::max);
    (min as u8, max as u8, v.iter().sum::<f32>() / v.len() as f32)
}

/// Runs `frames` frames of stripes, renewing `req` (when given) every frame, and returns the
/// report and the status block read mid-run (while the swapchain exists).
fn blur_run(session: &TestSession, frames: u32, req: Option<BackdropRequest>, clear_at: Option<u32>, opts: Opts<'_>) -> (Report, LayerStatus) {
    let mid = RefCell::new(None);
    let ring = session.ring_path();
    let r = run(Opts {
        frames,
        stripes: true,
        on_frame: Some(Box::new(|f, _, _| {
            if clear_at.is_some_and(|c| f >= c) {
                session.backdrop(&BackdropRequest::NONE);
            } else if let Some(mut q) = req {
                if q.until > 1 {
                    q.until = (mono_raw_ns() + 2_000_000_000) as u64;
                }
                session.backdrop(&q);
            }
            if f == frames - 1 {
                std::thread::sleep(Duration::from_millis(300));
                *mid.borrow_mut() = open_ring_reader(&ring).ok().and_then(|r| r.layer_status());
            }
        })),
        ..opts
    });
    session.backdrop(&BackdropRequest::NONE);
    let st = mid.borrow().expect("a status block");
    (r, st)
}

fn blur_req(dim: u16) -> BackdropRequest {
    BackdropRequest::blur(dim, 0, mono_raw_ns() + 2_000_000_000)
}

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface; see the module docs"]
fn blurs_behind_the_panel() {
    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    let validation = if std::env::var("QUIVER_TEST_VALIDATION").as_deref() == Ok("0") { None } else { validation_dir() };
    // SAFETY: the only test in this binary; nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
        for (k, v) in layer_env(scratch.path(), validation.as_deref()) {
            std::env::set_var(k, v);
        }
    }
    eprintln!("validation: {validation:?}");
    let du = validation.is_some();
    let (w, h) = (1280u32, 720u32);
    let mid_y = h / 2 + 100;
    let (x0, x1) = (500, 900);

    // 1. No request: the stripes as drawn; the layer says it could blur.
    let (r, st) = blur_run(&session, 20, None, None, Opts { debug_utils: du, ..Opts::default() });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    eprintln!("device {} format {:?}", r.device_name, r.format);
    let px = r.pixels.as_ref().unwrap();
    save("blur-0-none.png", w, h, px);
    let (lo, hi, _) = row(px, w, mid_y, x0, x1);
    assert!(lo < 3 && hi > 252, "untouched stripes: {lo}..{hi}");
    assert_ne!(st.flags & LAYER_BLUR_READY, 0, "blur ready (error {})", st.backdrop_error);
    assert_eq!((st.flags & LAYER_BLURRING, st.backdrop_error), (0, 0));
    let mut reader = open_ring_reader(&session.ring_path()).unwrap();
    reader.poll(10_000, |_, _| {});

    // 2. A live request, with the HUD: the game is blurred, the HUD drawn sharp on top.
    let (pos, size, opacity) = ((40, 30), (360u32, 180u32), 0.95);
    session.publish(&test_image((w, h), pos, size, 0, opacity));
    let (r, st) = blur_run(&session, 40, Some(blur_req(0)), None, Opts { debug_utils: du, ..Opts::default() });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    let px = r.pixels.as_ref().unwrap();
    save("blur-1-unorm.png", w, h, px);
    let (lo, hi, mean) = row(px, w, mid_y, x0, x1);
    assert!(hi - lo < 24, "blurred stripes still range {lo}..{hi}");
    assert!((mean - 127.5).abs() < 8.0, "blurred mean {mean}");
    // The HUD's 2 px border stays crisp (≈ 0.95 × 255 + 0.05 × the blur behind it), and the
    // HUD's background right inside it isn't smeared with it.
    let at = |x: i32, y: i32| px[((y as u32 * w + x as u32) * 4) as usize];
    let border = at(pos.0 + 1, pos.1 + 60);
    assert!(border >= 240, "HUD border {border}");
    let inside = at(pos.0 + 4, pos.1 + 60);
    assert!(inside < 150, "HUD background next to the border {inside}");
    assert_ne!(st.flags & LAYER_BLURRING, 0, "blurring flag");
    assert!(st.backdrop_gpu_ns > 0, "GPU time measured");
    eprintln!("1280×720 blur GPU time: {:.3} ms", st.backdrop_gpu_ns as f64 / 1e6);
    let mut drawn = 0;
    reader.poll(10_000, |_, rec| drawn += LayerPresent::from_record(rec).unwrap().hud_drawn as usize);
    assert!(drawn >= 30, "HUD drawn on {drawn} presents while blurring");

    // 3. Dimmed by half.
    let (r, _) = blur_run(&session, 40, Some(blur_req(500)), None, Opts { debug_utils: du, ..Opts::default() });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    let px = r.pixels.as_ref().unwrap();
    save("blur-2-dim.png", w, h, px);
    let (_, _, mean) = row(px, w, mid_y, x0, x1);
    assert!((mean - 64.0).abs() < 8.0, "dimmed mean {mean}");

    // 4. Cleared early: faded out by the end (0.2 s at 120 fps is 24 frames), and no longer
    // "blurring" half a second later.
    let (r, st) = blur_run(&session, 120, Some(blur_req(0)), Some(30), Opts { debug_utils: du, ..Opts::default() });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    let px = r.pixels.as_ref().unwrap();
    let (lo, hi, _) = row(px, w, mid_y, x0, x1);
    assert!(lo < 3 && hi > 252, "stripes back after the fade-out: {lo}..{hi}");
    assert_eq!(st.flags & LAYER_BLURRING, 0, "not blurring after the fade-out");

    // 5. An expired request (quiver-sight died with the panel open) is ignored.
    let expired = BackdropRequest::blur(0, 0, mono_raw_ns() - 1);
    let (r, _) = blur_run(&session, 20, Some(BackdropRequest { until: 1, ..expired }), None, Opts::default());
    let px = r.pixels.as_ref().unwrap();
    let (lo, hi, _) = row(px, w, mid_y, x0, x1);
    assert!(lo < 3 && hi > 252, "expired request: {lo}..{hi}");

    // 6. sRGB: blurred in linear light (half white is 188 encoded, not 128).
    let (r, _) = blur_run(&session, 40, Some(blur_req(0)), None, Opts { srgb: true, debug_utils: du, ..Opts::default() });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    if r.format == vk::Format::B8G8R8A8_SRGB {
        let px = r.pixels.as_ref().unwrap();
        save("blur-3-srgb.png", w, h, px);
        let (lo, hi, mean) = row(px, w, mid_y, x0, x1);
        assert!(hi - lo < 24 && (mean - 188.0).abs() < 8.0, "sRGB blur {lo}..{hi} mean {mean}");
    } else {
        eprintln!("no B8G8R8A8_SRGB surface format; sRGB check skipped");
    }

    // 7. HDR10 and scRGB swapchains, when the surface offers them (the HUD can't draw there;
    // the blur can).
    for (f, cs) in [
        (vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT),
        (vk::Format::A2R10G10B10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT),
        (vk::Format::R16G16B16A16_SFLOAT, vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT),
        (vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::SRGB_NONLINEAR),
    ] {
        let (r, st) = blur_run(&session, 40, Some(blur_req(0)), None, Opts { surface_format: Some((f, cs)), readback: f != vk::Format::R16G16B16A16_SFLOAT, debug_utils: du, ..Opts::default() });
        if (r.format, r.color_space) != (f, cs) {
            eprintln!("{f:?} {cs:?} not offered by the surface: skipped");
            continue;
        }
        assert!(r.messages.is_empty(), "{f:?} {cs:?}: {:#?}", r.messages);
        assert_ne!(st.flags & LAYER_BLUR_READY, 0, "{f:?} {cs:?}: blur ready (error {})", st.backdrop_error);
        assert_ne!(st.flags & LAYER_BLURRING, 0, "{f:?} {cs:?}: blurring");
        if let Some(px) = &r.pixels {
            let (lo, hi, mean) = row10(px, w, mid_y, x0, x1, f == vk::Format::A2B10G10R10_UNORM_PACK32);
            assert!(hi - lo < 24 && (mean - 127.5).abs() < 10.0, "{f:?} {cs:?}: {lo}..{hi} mean {mean}");
        }
        eprintln!("{f:?} {cs:?}: blurred");
    }

    // 8. What it costs: GPU time of the blur per present at common sizes, uncapped.
    for extent in [(1920, 1080), (2560, 1440), (3840, 2160)] {
        let (_, st) = blur_run(&session, 240, Some(blur_req(0)), None, Opts { extent, fps: 1000.0, readback: false, ..Opts::default() });
        let ms = st.backdrop_gpu_ns as f64 / 1e6;
        eprintln!("blur GPU time at {}×{}: {ms:.3} ms per present", extent.0, extent.1);
        assert!(st.backdrop_gpu_ns > 0 && ms < 3.0, "{extent:?}: {ms} ms");
    }
}
