//! Stereo frames (design/SIGHT.md §15) through the real layer in a headless Vulkan app
//! (tests/harness): a `panel.image` laid out for one eye is drawn in both halves of the
//! swapchain, at the same place in each (sbs), squeezed 2:1 with texel pairs averaged (hsbs),
//! with the cursor in both eyes and nothing across the seam; the status block says what the
//! layer drew.
//!
//! Needs a GPU whose driver supports `VK_EXT_headless_surface` (Mesa: RADV, ANV), so it's
//! opt-in: `cargo test -p quiver-sight-layer --test vulkan_stereo -- --ignored --nocapture`.
//! `QUIVER_TEST_SIGHT_PNG=<dir>` writes the read-back frames there.

mod harness;

use std::time::Duration;

use harness::*;
use quiver_perf::HudBufferHeader;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::{HudWriter, PANEL_LIMITS, buffer_bytes_within, rgba_premul};
use quiver_perf::layout::{HUD_FORMAT_RGBA8_PREMUL_SRGB, STEREO_DRAWN_VALID};
use quiver_perf::panel::{PANEL_HOLD_NS, PANEL_IMAGE_FILE, PANEL_INPUT_FILE, create_panel_image, create_panel_input};
use quiver_perf::shm::Mapping;
use quiver_perf::stereo::Stereo;

const EXTENT: (u32, u32) = (640, 400);
const MAGENTA: [u8; 4] = [255, 0, 255, 255];

/// A panel image laid out for one eye of `stereo` on EXTENT: magenta in `block` (logical
/// px), and with `stripes` one-pixel black/white columns in logical x 400..500, y 300..340.
fn image(stereo: Stereo, block: (u32, u32, u32, u32), stripes: bool) -> (HudBufferHeader, Vec<u8>) {
    let (w, h) = stereo.logical_extent(EXTENT);
    let mut px = vec![0u8; (w * 4 * h) as usize];
    let mut put = |x: u32, y: u32, c: [u8; 4]| {
        let o = ((y * w + x) * 4) as usize;
        px[o..o + 4].copy_from_slice(&rgba_premul(c[0], c[1], c[2], c[3]).to_le_bytes());
    };
    for y in block.1..block.3 {
        for x in block.0..block.2 {
            put(x, y, MAGENTA);
        }
    }
    if stripes {
        for y in 300..340 {
            for x in 400..500 {
                put(x, y, if x % 2 == 0 { [255, 255, 255, 255] } else { [0, 0, 0, 255] });
            }
        }
    }
    let mut d = HudBufferHeader::ZERO;
    (d.width, d.height, d.stride, d.format, d.scale) = (w, h, w * 4, HUD_FORMAT_RGBA8_PREMUL_SRGB, 1.0);
    (d.extent_w, d.extent_h, d.opacity, d.stereo) = (EXTENT.0, EXTENT.1, 1.0, stereo.code());
    (d, px)
}

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface; see the module docs"]
fn panel_in_both_eyes() {
    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    // SAFETY: the only test in this binary; set before any thread reads them.
    unsafe {
        for (k, v) in layer_env(scratch.path(), None) {
            std::env::set_var(k, v);
        }
        std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
        std::env::set_var("QUIVER_SIGHT_STEREO", "hsbs");
    }
    let panel = create_panel_input(&session.dir.path().join(PANEL_INPUT_FILE)).unwrap();
    let bb = buffer_bytes_within(EXTENT.0, EXTENT.1, &PANEL_LIMITS).unwrap();
    let writer: std::cell::RefCell<HudWriter<Mapping>> = std::cell::RefCell::new(create_panel_image(&session.dir.path().join(PANEL_IMAGE_FILE), bb).unwrap());
    let mut epoch = 0;
    let mut frame = |stereo: Stereo, block: (u32, u32, u32, u32), cursor: (i32, i32), name: &str| -> (Vec<u8>, String) {
        epoch += 1;
        let ep = epoch;
        let r = run(Opts {
            frames: 60,
            extent: EXTENT,
            fps: 60.0,
            on_frame: Some(Box::new(|f, _, _| {
                let now = mono_raw_ns();
                writer.borrow().heartbeat(now);
                if f == 1 {
                    panel.set_sight_pid(std::process::id());
                    panel.open(ep, now, now + PANEL_HOLD_NS as i64, true);
                    // The cursor is in the image's (one eye's logical) pixels.
                    panel.set_cursor(cursor.0, cursor.1, stereo.logical_extent(EXTENT));
                    let (d, px) = image(stereo, block, stereo == Stereo::Hsbs);
                    writer.borrow_mut().publish(&d, &px).unwrap();
                }
                if f > 1 {
                    panel.renew(now + PANEL_HOLD_NS as i64);
                }
                if f == 2 {
                    std::thread::sleep(Duration::from_millis(600));
                }
            })),
            ..Opts::default()
        });
        panel.close();
        let px = r.pixels.clone().unwrap();
        if let Some(dir) = std::env::var_os("QUIVER_TEST_SIGHT_PNG") {
            image::save_buffer(std::path::Path::new(&dir).join(format!("sight-stereo-{name}.png")), &px, EXTENT.0, EXTENT.1, image::ColorType::Rgba8).unwrap();
        }
        (px, format!("{} {:?}", r.device_name, r.format))
    };
    let at = |px: &[u8], x: u32, y: u32| -> [u8; 3] {
        let o = ((y * EXTENT.0 + x) * 4) as usize;
        [px[o], px[o + 1], px[o + 2]]
    };
    let near = |got: [u8; 3], want: [u8; 3], what: &str| assert!(got.iter().zip(want).all(|(a, b)| (*a as i32 - b as i32).abs() <= 3), "{what}: got {got:?}, want {want:?}");

    // sbs: each eye 320x400 at 1:1; the block at logical 40..200 in both.
    let (px, dev) = frame(Stereo::Sbs, (40, 50, 200, 250), (100, 300), "sbs");
    eprintln!("device {dev}");
    let game = at(&px, 630, 390);
    near(at(&px, 120, 150), [255, 0, 255], "sbs: the block in the left eye");
    near(at(&px, 320 + 120, 150), [255, 0, 255], "sbs: the block in the right eye, same place");
    near(at(&px, 250, 150), game, "sbs: right of the left eye's block is the game");
    near(at(&px, 320 + 30, 150), game, "sbs: left of the right eye's block is the game");
    near(at(&px, 100 + 2, 300 + 8), [250, 250, 250], "sbs: the cursor in the left eye");
    near(at(&px, 320 + 100 + 2, 300 + 8), [250, 250, 250], "sbs: the cursor in the right eye");

    // hsbs: laid out at 640x400, each eye 320 wide: logical 100..300 → 50..150 and 370..470.
    let (px, _) = frame(Stereo::Hsbs, (100, 50, 300, 250), (200, 100), "hsbs");
    near(at(&px, 55, 150), [255, 0, 255], "hsbs: the squeezed block in the left eye");
    near(at(&px, 145, 150), [255, 0, 255], "hsbs: … to its squeezed end");
    near(at(&px, 160, 150), game, "hsbs: past the squeezed block");
    near(at(&px, 320 + 55, 150), [255, 0, 255], "hsbs: the block in the right eye");
    near(at(&px, 320 + 160, 150), game, "hsbs: past it in the right eye");
    near(at(&px, 100 + 1, 100 + 8), [250, 250, 250], "hsbs: the cursor (logical 200) at 100 in the left eye");
    near(at(&px, 320 + 100 + 1, 100 + 8), [250, 250, 250], "hsbs: and at 420 in the right");
    // One-pixel stripes at logical 400..500 → 200..250: each pixel averages a white and a
    // black texel (grey in either encoding), not one of them.
    for x in [205, 220, 240, 320 + 220] {
        let g = at(&px, x, 320);
        assert!(g.iter().all(|&c| (60..=220).contains(&c)), "hsbs stripes at {x}: {g:?} (a 2:1 box filter gives grey)");
    }
    // The layer reports the setting from its environment and what it drew.
    let st = quiver_perf::session::open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()).expect("the layer's status");
    assert_eq!(quiver_perf::stereo::Setting::from_code(st.stereo_setting), quiver_perf::stereo::Setting::Fixed(Stereo::Hsbs));
    assert_eq!(st.stereo_drawn, STEREO_DRAWN_VALID | Stereo::Hsbs.code());
}
