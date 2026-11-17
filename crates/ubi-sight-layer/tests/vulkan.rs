//! The layer in a real Vulkan app (tests/harness): present records reach the ring, a
//! published HUD image is composited (UNORM and sRGB, padded rows), re-uploaded on publish,
//! the graph draws, and nothing is drawn for another extent or a stale writer. Validation
//! runs below the layer when a validation layer is on this machine.
//!
//! Needs a GPU whose driver supports `VK_EXT_headless_surface` (Mesa: RADV, ANV), so it's
//! opt-in: `cargo test -p ubi-sight-layer --test vulkan -- --ignored --nocapture`.
//! `QUIVER_TEST_VK_DEVICE=<name substring>` picks the device; `QUIVER_TEST_VALIDATION=0`
//! skips validation; `QUIVER_TEST_SIGHT_PNG=<dir>` writes the read-back frames there.

mod harness;

use std::time::Duration;

use harness::*;
use ubi_perf::layout::{DRAW_ERR_NO_IMAGE, LAYER_PRESENT_ID, LAYER_METRIC_PRESENT};
use ubi_perf::record::LayerPresent;
use ubi_perf::session::open_ring_reader;

fn lin(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}
fn enc(c: f32) -> f32 {
    if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// The expected composite of straight-alpha colour `c` (premultiplied by the writer) at
/// `opacity` over `dst`.
fn expect(c: [u8; 4], opacity: f32, dst: [u8; 4], srgb: bool) -> [f32; 3] {
    let a = c[3] as f32 / 255.0;
    let mut out = [0.0; 3];
    for i in 0..3 {
        let s = (c[i] as f32 * a).round() / 255.0; // premultiplied byte
        let d = dst[i] as f32 / 255.0;
        out[i] = if srgb { enc(lin(s) * opacity + lin(d) * (1.0 - a * opacity)) } else { s * opacity + d * (1.0 - a * opacity) } * 255.0;
    }
    out
}

struct Frame<'a> {
    px: &'a [u8],
    w: u32,
}

impl Frame<'_> {
    fn at(&self, x: i32, y: i32) -> [u8; 4] {
        let o = ((y as u32 * self.w + x as u32) * 4) as usize;
        self.px[o..o + 4].try_into().unwrap()
    }
}

fn close(got: [u8; 4], want: [f32; 3], tol: f32, what: &str) {
    for i in 0..3 {
        assert!((got[i] as f32 - want[i]).abs() <= tol, "{what}: got {got:?}, want {want:?}");
    }
}

fn save(name: &str, w: u32, h: u32, px: &[u8]) {
    if let Some(dir) = std::env::var_os("QUIVER_TEST_SIGHT_PNG") {
        let p = std::path::Path::new(&dir).join(name);
        image::save_buffer(&p, px, w, h, image::ExtendedColorType::Rgba8).unwrap();
        eprintln!("wrote {}", p.display());
    }
}

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface; see the module docs"]
fn composites_and_records() {
    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    let validation = if std::env::var("QUIVER_TEST_VALIDATION").as_deref() == Ok("0") { None } else { validation_dir() };
    // SAFETY (all set_var/remove_var here): the only test in this binary; the session's
    // heartbeat thread and the Vulkan loader don't read these concurrently with the writes.
    // 0. Inert unless enabled: the manifest is discoverable and QUIVER_PERF_DIR is set, but
    // without QUIVER_SIGHT=1 (or with QUIVER_SIGHT_DISABLE=1) the loader doesn't load it.
    unsafe {
        std::env::set_var("VK_ADD_IMPLICIT_LAYER_PATH", scratch.path());
        std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
    }
    let inert = |what: &str| {
        let r = run(Opts { frames: 3, readback: false, ..Opts::default() });
        assert_eq!(r.present_ns.len(), 3);
        assert!(!session.ring_path().exists(), "the layer loaded {what}");
    };
    inert("without QUIVER_SIGHT");
    unsafe {
        std::env::set_var("QUIVER_SIGHT", "1");
        std::env::set_var("QUIVER_SIGHT_DISABLE", "1");
    }
    inert("with QUIVER_SIGHT_DISABLE=1");
    unsafe {
        std::env::remove_var("QUIVER_SIGHT_DISABLE");
        for (k, v) in layer_env(scratch.path(), validation.as_deref()) {
            std::env::set_var(k, v);
        }
    }
    eprintln!("validation: {validation:?}");

    let extent = (1280, 720);
    let (pos, size, opacity) = ((40, 30), (360u32, 180u32), 0.95);
    session.publish(&test_image(extent, pos, size, 0, opacity));
    let v1 = test_image(extent, pos, size, 1, opacity);
    let frames = 150;
    let mid = std::cell::RefCell::new(None);

    // 1. UNORM, a re-publish halfway, hitches for the graph.
    let r = run(Opts {
        frames,
        hitches: vec![60, 100],
        debug_utils: validation.is_some(),
        on_frame: Some(Box::new(|f, _, _| {
            if f == 75 {
                session.publish(&v1);
            }
            if f == frames - 1 {
                // While the swapchain exists (a heartbeat tick wrote it within 250 ms).
                *mid.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status());
            }
        })),
        ..Opts::default()
    });
    eprintln!("device {} format {:?} present_id {}", r.device_name, r.format, r.present_id);
    for m in &r.messages {
        eprintln!("{m}");
    }
    assert!(r.messages.is_empty(), "validation reported {} messages", r.messages.len());
    let px = r.pixels.as_ref().unwrap();
    save("sight-unorm.png", extent.0, extent.1, px);
    let f = Frame { px, w: extent.0 };
    let dst = f.at(1200, 700);
    let (w, h) = (size.0 as i32, size.1 as i32);
    close(f.at(pos.0 + w - 10, pos.1 + 20), expect(BG, opacity, dst, false), 3.0, "background");
    close(f.at(pos.0, pos.1), expect(BORDER, opacity, dst, false), 3.0, "border");
    // Bar 0 is 30 px in image 0 and 67 px in image 1: the re-published image is the one shown.
    close(f.at(pos.0 + 12 + 50, pos.1 + 15), expect(BAR, opacity, dst, false), 3.0, "re-uploaded bar");
    close(f.at(pos.0 - 5, pos.1 - 5), [dst[0] as f32, dst[1] as f32, dst[2] as f32], 1.0, "outside the HUD");
    // The graph: line-coloured pixels inside its rectangle.
    let g = &v1.desc.graph[0];
    let mut line_px = 0;
    for y in g.y..g.y + g.h {
        for x in g.x..g.x + g.w {
            let p = f.at(pos.0 + x, pos.1 + y);
            if p[1] > 150 && p[0] < 140 && p[2] < 160 {
                line_px += 1;
            }
        }
    }
    // 150 frames = 1.25 s of the 3 s span: the right ~40% of the rectangle.
    assert!(line_px > 100, "graph line pixels: {line_px}");

    // The ring: one record per present, the app's present ids, the HUD flag.
    let mut reader = open_ring_reader(&session.ring_path()).unwrap();
    let mut recs = Vec::new();
    reader.poll(10_000, |_, rec| {
        assert_eq!(rec.metric, LAYER_METRIC_PRESENT);
        recs.push(LayerPresent::from_record(rec).unwrap());
    });
    assert_eq!(recs.len(), frames as usize);
    for (i, p) in recs.iter().enumerate() {
        assert_eq!(p.seq, i as u64 + 1);
        assert!(p.return_ns >= p.entry_ns);
        assert_eq!(p.result, 0);
        if r.present_id {
            assert_eq!(p.present_id, Some(i as u64 + 1));
        }
    }
    let drawn = recs.iter().filter(|p| p.hud_drawn).count();
    assert!(drawn >= frames as usize - 2, "drawn {drawn}");
    let hud_ns: Vec<u64> = recs.iter().filter(|p| p.hud_drawn).map(|p| p.hud_ns as u64).collect();
    let present: Vec<u64> = recs.iter().map(|p| (p.return_ns - p.entry_ns) as u64).collect();
    eprintln!(
        "layer HUD CPU per present: median {} us, p99 {} us; present call (layer entry→return) median {} us; app-side present median {} us",
        median(&hud_ns) / 1000,
        percentile(&hud_ns, 0.99) / 1000,
        median(&present) / 1000,
        median(&r.present_ns) / 1000
    );
    std::thread::sleep(Duration::from_millis(400)); // a heartbeat tick writes the status
    let m = mid.borrow().unwrap();
    assert_eq!((m.extent_w, m.extent_h), extent);
    assert_eq!(m.vk_format, r.format.as_raw() as u32);
    assert_eq!(m.swapchain_count, 1);
    assert_eq!(m.draw_error, 0);
    assert_ne!(m.flags & ubi_perf::layout::LAYER_DRAWING, 0);
    assert!(m.drawn > 0 && m.presents > 0);
    // After the run: counters and identity stay, the swapchain is gone.
    let st = reader.layer_status().unwrap();
    assert_eq!((st.extent_w, st.extent_h, st.swapchain_count), (0, 0, 0));
    assert_eq!(st.presents, frames as u64);
    assert!(st.drawn >= drawn as u64);
    assert_eq!(st.hud_published, 2);
    assert_eq!(ubi_perf::LayerStatus::get_str(&st.engine_name), Some("QuiverSightTest"));
    assert_eq!(st.engine_version, 7);
    if r.present_id {
        assert_ne!(st.flags & LAYER_PRESENT_ID, 0);
    }
    assert!(reader.heartbeat() > 0);

    // 2. sRGB: blended in linear.
    let r = run(Opts { frames: 30, srgb: true, debug_utils: validation.is_some(), ..Opts::default() });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    if r.format == ash::vk::Format::B8G8R8A8_SRGB {
        let px = r.pixels.as_ref().unwrap();
        save("sight-srgb.png", extent.0, extent.1, px);
        let f = Frame { px, w: extent.0 };
        let dst = f.at(1200, 700);
        close(f.at(pos.0 + w - 10, pos.1 + 20), expect(BG, opacity, dst, true), 4.0, "sRGB background");
        close(f.at(pos.0 + 1, pos.1 + 1), expect(BORDER, opacity, dst, true), 4.0, "sRGB border");
    } else {
        eprintln!("no B8G8R8A8_SRGB surface format; sRGB check skipped");
    }
    reader.poll(10_000, |_, _| {});

    // 2b. Two swapchains in one present (two wait semaphores): the larger one gets the HUD,
    // both get records.
    let mid = std::cell::RefCell::new(None);
    let r = run(Opts {
        frames: 30,
        second: Some((320, 240)),
        debug_utils: validation.is_some(),
        on_frame: Some(Box::new(|f, _, _| {
            if f == 29 {
                std::thread::sleep(Duration::from_millis(300));
                *mid.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status());
            }
        })),
        ..Opts::default()
    });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    let f = Frame { px: r.pixels.as_ref().unwrap(), w: extent.0 };
    let dst = f.at(1200, 700);
    close(f.at(pos.0 + w - 10, pos.1 + 20), expect(BG, opacity, dst, false), 3.0, "two swapchains: background");
    let m = mid.borrow().unwrap();
    assert_eq!((m.extent_w, m.extent_h, m.swapchain_count), (1280, 720, 2));
    let mut by_id = std::collections::BTreeMap::<u32, (usize, usize, Option<u64>)>::new();
    reader.poll(10_000, |_, rec| {
        let p = LayerPresent::from_record(rec).unwrap();
        let e = by_id.entry(p.swapchain).or_default();
        e.0 += 1;
        e.1 += p.hud_drawn as usize;
        e.2 = p.present_id;
    });
    let v: Vec<_> = by_id.values().collect();
    assert_eq!(v.len(), 2, "{by_id:?}");
    assert_eq!((v[0].0, v[1].0), (30, 30));
    assert!(v[0].1 >= 28 && v[1].1 == 0, "{by_id:?}");
    if r.present_id {
        assert_eq!((v[0].2, v[1].2), (Some(30), Some(10_030)));
    }

    // 2c. A bigger image while frames are in flight: the texture is replaced (the old one
    // retired, not waited for) and the new size is drawn.
    let big = test_image(extent, pos, (700, 300), 5, opacity);
    let r = run(Opts {
        frames: 60,
        fps: 1000.0,
        debug_utils: validation.is_some(),
        on_frame: Some(Box::new(|f, _, _| {
            if f == 20 {
                session.publish(&big);
            }
        })),
        ..Opts::default()
    });
    assert!(r.messages.is_empty(), "{:#?}", r.messages);
    let f = Frame { px: r.pixels.as_ref().unwrap(), w: extent.0 };
    let dst = f.at(1200, 700);
    close(f.at(pos.0 + 690, pos.1 + 20), expect(BG, opacity, dst, false), 3.0, "grown texture: right edge");
    close(f.at(pos.0 + 699, pos.1 + 150), expect(BORDER, opacity, dst, false), 3.0, "grown texture: border");
    let mut drawn = 0;
    reader.poll(10_000, |_, rec| drawn += LayerPresent::from_record(rec).unwrap().hud_drawn as usize);
    assert!(drawn >= 58, "drawn {drawn} of 60 across the growth");

    // 3. Another extent: the image is laid out for 1280×720, so nothing is drawn.
    let mid = std::cell::RefCell::new(None);
    let r = run(Opts {
        frames: 20,
        extent: (1024, 600),
        on_frame: Some(Box::new(|f, _, _| {
            if f == 19 {
                std::thread::sleep(Duration::from_millis(300));
                *mid.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status());
            }
        })),
        ..Opts::default()
    });
    let px = r.pixels.as_ref().unwrap();
    let f = Frame { px, w: 1024 };
    let dst = f.at(1000, 590);
    close(f.at(pos.0 + w - 10, pos.1 + 20), [dst[0] as f32, dst[1] as f32, dst[2] as f32], 1.0, "other extent: not drawn");
    let mut n = 0;
    reader.poll(10_000, |_, rec| {
        n += 1;
        assert!(!LayerPresent::from_record(rec).unwrap().hud_drawn);
    });
    assert_eq!(n, 20);
    let m = mid.borrow().unwrap();
    assert_eq!((m.extent_w, m.extent_h, m.draw_error), (1024, 600, DRAW_ERR_NO_IMAGE));
    assert_eq!(m.flags & ubi_perf::layout::LAYER_DRAWING, 0);

    // 3b. Hostile hud.image: corrupted and random descriptors written straight into the file
    // (no HudWriter validation), a new publish every frame. Nothing may be drawn or crash;
    // a valid image afterwards is drawn again.
    {
        use std::os::unix::fs::FileExt;
        let hud = std::fs::OpenOptions::new().read(true).write(true).open(session.dir.file(&ubi_perf::session::FileKind::HudImage)).unwrap();
        let mut word = [0u8; 4];
        hud.read_exact_at(&mut word, 12).unwrap();
        let bb = u32::from_le_bytes(word) as u64;
        let valid = test_image(extent, pos, size, 3, opacity);
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut published = 1_000u64;
        let r = run(Opts {
            frames: 80,
            debug_utils: validation.is_some(),
            on_frame: Some(Box::new(|f, _, _| {
                if f >= 60 {
                    if f == 60 {
                        session.publish(&valid);
                    }
                    return;
                }
                let mut d = valid.desc;
                match f % 16 {
                    0 => d.width = 100_000,
                    1 => d.height = 0,
                    2 => d.stride = d.width * 4 - 4,
                    3 => d.stride = u32::MAX - 3,
                    4 => d.x = -5,
                    5 => d.y = extent.1 as i32 - 10,
                    6 => d.opacity = f32::NAN,
                    7 => d.scale = f32::INFINITY,
                    8 => d.ngraphs = 3,
                    9 => d.graph[0].w = 100_000,
                    10 => d.graph[0].span_ms = 0,
                    11 => d.graph[0].max_ms = f32::NAN,
                    12 => d.format = 2,
                    13 => d.extent_w = 1281,
                    14 => d.graph[0].series = 0,
                    _ => {}
                }
                // SAFETY: HudBufferHeader is 256 bytes of plain integers and floats.
                let mut bytes: [u8; 256] = unsafe { std::mem::transmute(d) };
                if f % 16 == 15 {
                    for b in bytes.iter_mut() {
                        *b = next() as u8;
                    }
                }
                bytes[..8].fill(0); // seq even: "not being written"
                published += 1;
                let base = 4096 + (published % 2) * bb;
                hud.write_all_at(&bytes, base).unwrap();
                let junk: Vec<u8> = (0..4096).map(|_| next() as u8).collect();
                hud.write_all_at(&junk, base + 256).unwrap();
                hud.write_all_at(&published.to_le_bytes(), 64).unwrap();
            })),
            ..Opts::default()
        });
        assert!(r.messages.is_empty(), "{:#?}", r.messages);
        let mut drawn = Vec::new();
        reader.poll(10_000, |_, rec| drawn.push(LayerPresent::from_record(rec).unwrap().hud_drawn));
        assert_eq!(drawn.len(), 80);
        assert!(drawn[..60].iter().all(|d| !d), "a corrupt descriptor was drawn: {drawn:?}");
        assert!(drawn[62..].iter().all(|d| *d), "no recovery after a valid image: {drawn:?}");
    }

    // 4. A stale writer (no heartbeat for > 3 s): nothing is drawn.
    let mut session = session;
    session.stop_heartbeat();
    std::thread::sleep(Duration::from_millis(3300));
    let r = run(Opts { frames: 20, ..Opts::default() });
    let px = r.pixels.as_ref().unwrap();
    let f = Frame { px, w: extent.0 };
    let dst = f.at(1200, 700);
    close(f.at(pos.0 + w - 10, pos.1 + 20), [dst[0] as f32, dst[1] as f32, dst[2] as f32], 1.0, "stale: not drawn");
    let _ = h;
}
