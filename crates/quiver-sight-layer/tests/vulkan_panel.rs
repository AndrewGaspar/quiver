//! The Sight panel drawn in the game (design/SIGHT.md §14) through the real layer in a
//! headless Vulkan app (tests/harness): with `panel.input` open and a `panel.image` published
//! at the swapchain's extent, the read-back frame shows the panel's pixels over the game's,
//! the panel's cursor at its position, and a later image that names only its changed rows
//! (`dirty_*`) updates those rows and keeps the rest; the layer says it can draw the panel and
//! marks the open panel drawn (what the input preload keys on). Closed, nothing is drawn.
//!
//! Needs a GPU whose driver supports `VK_EXT_headless_surface` (Mesa: RADV, ANV), so it's
//! opt-in: `cargo test -p quiver-sight-layer --test vulkan_panel -- --ignored --nocapture`.
//! `QUIVER_TEST_SIGHT_PNG=<dir>` writes the read-back frames there.

mod harness;

use std::time::Duration;

use harness::*;
use quiver_perf::HudBufferHeader;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::{HudWriter, PANEL_LIMITS, buffer_bytes_within, rgba_premul};
use quiver_perf::layout::HUD_FORMAT_RGBA8_PREMUL_SRGB;
use quiver_perf::panel::{PANEL_HOLD_NS, PANEL_IMAGE_FILE, PANEL_INPUT_FILE, create_panel_image, create_panel_input};
use quiver_perf::shm::Mapping;

const EXTENT: (u32, u32) = (640, 400);
const MAGENTA: [u8; 4] = [255, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
/// The panel's block: everything else in the image is transparent.
const BLOCK: (u32, u32, u32, u32) = (100, 80, 540, 320);
const BAND: (u32, u32) = (100, 120);

fn image(band: bool) -> (HudBufferHeader, Vec<u8>) {
    let (w, h) = EXTENT;
    let mut px = vec![0u8; (w * 4 * h) as usize];
    for y in BLOCK.1..BLOCK.3 {
        for x in BLOCK.0..BLOCK.2 {
            let c = if band && (BAND.0..BAND.1).contains(&y) { GREEN } else { MAGENTA };
            let o = ((y * w + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&rgba_premul(c[0], c[1], c[2], c[3]).to_le_bytes());
        }
    }
    let mut d = HudBufferHeader::ZERO;
    (d.width, d.height, d.stride, d.format, d.scale) = (w, h, w * 4, HUD_FORMAT_RGBA8_PREMUL_SRGB, 1.0);
    (d.extent_w, d.extent_h, d.opacity) = (w, h, 1.0);
    if band {
        // Only the band changed since the first publish.
        (d.dirty_y0, d.dirty_y1, d.dirty_base) = (BAND.0, BAND.1, 1);
        // A pixel outside the band that differs: a partial upload must not take it.
        let o = ((200 * w + 300) * 4) as usize;
        px[o..o + 4].copy_from_slice(&rgba_premul(GREEN[0], GREEN[1], GREEN[2], 255).to_le_bytes());
    }
    (d, px)
}

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface; see the module docs"]
fn panel_composited_with_cursor_and_partial_updates() {
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
    }
    let panel = create_panel_input(&session.dir.path().join(PANEL_INPUT_FILE)).unwrap();
    let bb = buffer_bytes_within(EXTENT.0, EXTENT.1, &PANEL_LIMITS).unwrap();
    let writer: std::cell::RefCell<HudWriter<Mapping>> = std::cell::RefCell::new(create_panel_image(&session.dir.path().join(PANEL_IMAGE_FILE), bb).unwrap());
    let cursor = (200, 150);
    let frames = 120;

    // 1. Open, an image, a partial update at frame 60, the cursor.
    let r = run(Opts {
        frames,
        extent: EXTENT,
        fps: 60.0,
        on_frame: Some(Box::new(|f, _, _| {
            let now = mono_raw_ns();
            let w = writer.borrow_mut();
            w.heartbeat(now);
            drop(w);
            if f == 1 {
                panel.set_sight_pid(std::process::id());
                panel.open(1, now, now + PANEL_HOLD_NS as i64, true);
                panel.set_cursor(cursor.0, cursor.1, EXTENT);
                let (d, px) = image(false);
                assert_eq!(writer.borrow_mut().publish(&d, &px).unwrap(), 1);
            }
            if f == 60 {
                let (d, px) = image(true);
                writer.borrow_mut().publish(&d, &px).unwrap();
            }
            if f > 1 {
                panel.renew(now + PANEL_HOLD_NS as i64);
            }
            // The heartbeat thread maps panel.image within a tick: give it time at the start.
            if f == 2 {
                std::thread::sleep(Duration::from_millis(600));
            }
        })),
        ..Opts::default()
    });
    let px = r.pixels.as_ref().unwrap();
    if let Some(dir) = std::env::var_os("QUIVER_TEST_SIGHT_PNG") {
        image::save_buffer(std::path::Path::new(&dir).join("sight-panel.png"), px, EXTENT.0, EXTENT.1, image::ColorType::Rgba8).unwrap();
    }
    let at = |x: u32, y: u32| -> [u8; 3] {
        let o = ((y * EXTENT.0 + x) * 4) as usize;
        [px[o], px[o + 1], px[o + 2]]
    };
    let near = |got: [u8; 3], want: [u8; 3], what: &str| assert!(got.iter().zip(want).all(|(a, b)| (*a as i32 - b as i32).abs() <= 3), "{what}: got {got:?}, want {want:?}");
    let game = at(20, 390);
    eprintln!("device {} format {:?}; game pixel {game:?}", r.device_name, r.format);
    near(at(450, 250), [255, 0, 255], "the panel's block");
    near(at(450, 110), [0, 255, 0], "the band of the partial update");
    near(at(450, 90), [255, 0, 255], "rows above the band, kept from the first image");
    near(at(300, 200), [255, 0, 255], "a pixel outside the dirty rows isn't taken");
    near(at(50, 50), game, "the transparent part of the panel shows the game");
    // The cursor: its light fill just below the tip, its dark outline under that.
    near(at(cursor.0 as u32 + 2, cursor.1 as u32 + 8), [250, 250, 250], "the cursor's fill");
    let o = at(cursor.0 as u32, cursor.1 as u32 + 18);
    assert!(o.iter().all(|&c| c < 40), "the cursor's outline: {o:?}");
    let layer = panel.layer();
    eprintln!("layer word: {layer:?}");
    assert_eq!(layer.extent, EXTENT);
    assert!(layer.flags & quiver_perf::panel::PANEL_LAYER_CAN_DRAW != 0);
    assert_eq!(layer.drawn_epoch, 1, "the layer never marked the panel drawn");
    assert!(mono_raw_ns() as u64 - layer.drawn_ns < 2_000_000_000);

    // 2. Closed: the frame is the game's.
    panel.close();
    let r = run(Opts {
        frames: 30,
        extent: EXTENT,
        fps: 60.0,
        on_frame: Some(Box::new(|_, _, _| writer.borrow_mut().heartbeat(mono_raw_ns()))),
        ..Opts::default()
    });
    let px2 = r.pixels.as_ref().unwrap();
    let o = ((250 * EXTENT.0 + 450) * 4) as usize;
    let g = ((390 * EXTENT.0 + 20) * 4) as usize;
    assert_eq!(&px2[o..o + 3], &px2[g..g + 3], "the panel was drawn after it closed");
}
