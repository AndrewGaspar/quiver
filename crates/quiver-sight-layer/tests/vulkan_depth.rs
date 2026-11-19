//! Depth on stereo frames (design/STEREO-UI.md) through the real layer in a headless Vulkan app
//! (tests/harness): a `panel.image` on a full-SBS frame with 1920-wide eyes is drawn in each eye
//! with that eye's share of each part's disparity: the image as a whole, its card and its
//! focused row (lifts), the strip a lift moves off filled from beside it, and the cursor at the
//! depth of what it's over. The status block reports the setting and the disparity drawn.
//!
//! Needs a GPU whose driver supports `VK_EXT_headless_surface` (Mesa: RADV, ANV), so it's
//! opt-in: `cargo test -p quiver-sight-layer --test vulkan_depth -- --ignored --nocapture`.
//! `QUIVER_TEST_SIGHT_PNG=<dir>` writes the read-back frames there.

mod harness;

use std::time::Duration;

use harness::*;
use quiver_perf::HudBufferHeader;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::{HudWriter, PANEL_LIMITS, buffer_bytes_within, rgba_premul};
use quiver_perf::layout::{DEPTH_DRAWN_VALID, HUD_FORMAT_RGBA8_PREMUL_SRGB, HudLift};
use quiver_perf::panel::{PANEL_HOLD_NS, PANEL_IMAGE_FILE, PANEL_INPUT_FILE, create_panel_image, create_panel_input};
use quiver_perf::shm::Mapping;
use quiver_perf::stereo::{DepthSetting, Stereo, rung};

/// Two 1920-wide eyes, so the rungs give the disparities they do on the glasses.
const EXTENT: (u32, u32) = (3840, 400);
const EYE: u32 = 1920;
const CARD: [u8; 4] = [60, 60, 60, 255];
const ROW: [u8; 4] = [90, 90, 90, 255];
const MAGENTA: [u8; 4] = [255, 0, 255, 255];
/// The card, the focused row inside it and the magenta "label" inside the row, logical px.
const CARD_RECT: (u32, u32, u32, u32) = (100, 20, 1820, 380);
const ROW_RECT: (u32, u32, u32, u32) = (400, 150, 1400, 250);
const LABEL: (u32, u32, u32, u32) = (420, 160, 1380, 240);

/// One eye's image: transparent (the game shows through), the card, the row and its label;
/// `lifts`: the card and row as lifts at their rungs, else the whole image at `depth`.
fn image(lifts: bool, depth: u16) -> (HudBufferHeader, Vec<u8>) {
    let (w, h) = (EYE, EXTENT.1);
    let mut px = vec![0u8; (w * 4 * h) as usize];
    let mut fill = |r: (u32, u32, u32, u32), c: [u8; 4]| {
        for y in r.1..r.3 {
            for x in r.0..r.2 {
                let o = ((y * w + x) * 4) as usize;
                px[o..o + 4].copy_from_slice(&rgba_premul(c[0], c[1], c[2], c[3]).to_le_bytes());
            }
        }
    };
    fill(CARD_RECT, CARD);
    fill(ROW_RECT, ROW);
    fill(LABEL, MAGENTA);
    let mut d = HudBufferHeader::ZERO;
    (d.width, d.height, d.stride, d.format, d.scale) = (w, h, w * 4, HUD_FORMAT_RGBA8_PREMUL_SRGB, 1.0);
    (d.extent_w, d.extent_h, d.opacity, d.stereo) = (EXTENT.0, EXTENT.1, 1.0, Stereo::Sbs.code());
    d.depth = depth;
    if lifts {
        let lift = |r: (u32, u32, u32, u32), depth| HudLift { x: r.0 as u16, y: r.1 as u16, w: (r.2 - r.0) as u16, h: (r.3 - r.1) as u16, depth, _pad: 0 };
        d.nlifts = 2;
        d.lifts[0] = lift(CARD_RECT, rung::PANEL);
        d.lifts[1] = lift(ROW_RECT, rung::FOCUS);
    }
    (d, px)
}

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface; see the module docs"]
fn parts_at_their_depth_in_each_eye() {
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
        std::env::set_var("QUIVER_SIGHT_DEPTH", "100");
    }
    let panel = create_panel_input(&session.dir.path().join(PANEL_INPUT_FILE)).unwrap();
    let bb = buffer_bytes_within(EYE, EXTENT.1, &PANEL_LIMITS).unwrap();
    let writer: std::cell::RefCell<HudWriter<Mapping>> = std::cell::RefCell::new(create_panel_image(&session.dir.path().join(PANEL_IMAGE_FILE), bb).unwrap());
    let mut epoch = 0;
    let mut frame = |lifts: bool, depth: u16, cursor: (i32, i32), name: &str| -> Vec<u8> {
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
                    panel.set_cursor(cursor.0, cursor.1, (EYE, EXTENT.1));
                    let (d, px) = image(lifts, depth);
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
            image::save_buffer(std::path::Path::new(&dir).join(format!("sight-depth-{name}.png")), &px, EXTENT.0, EXTENT.1, image::ColorType::Rgba8).unwrap();
        }
        eprintln!("{name}: {} {:?}", r.device_name, r.format);
        px
    };
    let at = |px: &[u8], x: u32, y: u32| -> [u8; 3] {
        let o = ((y * EXTENT.0 + x) * 4) as usize;
        [px[o], px[o + 1], px[o + 2]]
    };
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(a, b)| (*a as i32 - b as i32).abs() <= 3);
    let rgb = |c: [u8; 4]| [c[0], c[1], c[2]];
    // The first x in `from..to` on row `y` whose colour is `want`, as an eye's logical x.
    let edge = |px: &[u8], eye: u32, y: u32, from: u32, to: u32, want: [u8; 4]| -> i32 {
        let x = (from..to).find(|&x| near(at(px, eye * EYE + x, y), rgb(want))).unwrap_or_else(|| panic!("no {want:?} on row {y} in eye {eye} {from}..{to}"));
        x as i32
    };

    // The whole image at the HUD's rung: 2 px total, +1 in the left eye, -1 in the right.
    let px = frame(false, rung::HUD, (-100, -100), "image");
    let (l, r) = (edge(&px, 0, 100, 50, 200, CARD), edge(&px, 1, 100, 50, 200, CARD));
    eprintln!("image: the card's left edge at {l} (left eye) and {r} (right eye): {} px", l - r);
    assert_eq!((l, r), (101, 99));

    // The card and the focused row as lifts over an image at the game's plane.
    let tip = (700, 190);
    let px = frame(true, 0, tip, "lifts");
    let game = at(&px, 10, 10);
    assert!(near(at(&px, 10, 10), at(&px, EYE + 10, 10)), "the scrim region is the game, the same in both eyes");
    let card = (edge(&px, 0, 100, 50, 200, CARD), edge(&px, 1, 100, 50, 200, CARD));
    let row = (edge(&px, 0, 200, 380, 420, ROW), edge(&px, 1, 200, 380, 420, ROW));
    let label = (edge(&px, 0, 200, 410, 440, MAGENTA), edge(&px, 1, 200, 410, 440, MAGENTA));
    eprintln!("lifts: card {card:?} ({} px), row {row:?} ({} px), label {label:?} ({} px)", card.0 - card.1, row.0 - row.1, label.0 - label.1);
    assert_eq!(card, (102, 99), "the card: 3 px");
    assert_eq!(row, (403, 398), "the row: 5 px");
    assert_eq!(label, (423, 418), "its contents with it");
    // Beside the row, the strip it moved off shows the card, not a second copy of the row's edge.
    assert!(near(at(&px, 402, 200), rgb(CARD)), "left eye, left of the row: {:?}", at(&px, 402, 200));
    assert!(near(at(&px, EYE + 1398, 200), rgb(CARD)), "right eye, right of the row: {:?}", at(&px, EYE + 1398, 200));
    assert!(near(at(&px, EYE + 1397, 200), rgb(ROW)), "right eye: the row ends at 1397");
    // Left of the card in the left eye is the game.
    assert!(near(at(&px, 101, 100), game), "left eye, left of the card: {:?}", at(&px, 101, 100));
    // The cursor over the row is drawn at the row's depth: its fill starts 5 px apart.
    let white = [250, 250, 250, 255];
    let cl = edge(&px, 0, tip.1 as u32 + 8, tip.0 as u32 - 6, tip.0 as u32 + 12, white);
    let cr = edge(&px, 1, tip.1 as u32 + 8, tip.0 as u32 - 6, tip.0 as u32 + 12, white);
    eprintln!("cursor: fill from {cl} (left eye) and {cr} (right eye): {} px", cl - cr);
    assert_eq!(cl - cr, 5);

    let st = quiver_perf::session::open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()).expect("the layer's status");
    assert_eq!(DepthSetting::from_code(st.depth_setting), DepthSetting::Percent(100));
    assert_eq!(st.depth_drawn, DEPTH_DRAWN_VALID | 5);
}
