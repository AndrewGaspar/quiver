//! Writers and readers in different processes over real files in a temporary session dir
//! under /dev/shm (`QUIVER_PERF_DIR` points the child at it, as it does the layer). The test
//! binary re-executes itself as the child (`child_entry`, which does nothing unless the
//! parent set `UBI_PERF_CHILD`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use ubi_perf::clock::mono_raw_ns;
use ubi_perf::hud::{HudPoll, buffer_bytes_for, rgba_premul};
use ubi_perf::layout::{HudBufferHeader, HudGraph, LAYER_DRAWING};
use ubi_perf::record::{LayerPresent, LayerStatus, Record};
use ubi_perf::ring::RingParams;
use ubi_perf::session::{ENV_PERF_DIR, FileKind, SessionDir, SessionId, open_ring_writer};

const ROLE: &str = "UBI_PERF_CHILD";
const COUNT: &str = "UBI_PERF_COUNT";

fn shm_root() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("ubi-perf-test-").tempdir_in("/dev/shm").unwrap()
}

fn spawn(role: &str, dir: &Path, count: u64) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_entry", "--ignored", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .env(ENV_PERF_DIR, dir)
        .env(COUNT, count.to_string())
        .spawn()
        .unwrap()
}

fn present(seq: u64) -> LayerPresent {
    LayerPresent {
        swapchain: (seq / 1000) as u32,
        present_id: (seq % 2 == 0).then_some(seq + 7),
        seq,
        entry_ns: seq as i64 * 3,
        return_ns: seq as i64 * 3 + 1,
        image_index: (seq % 3) as u32,
        present_mode: 2,
        result: 0,
        hud_drawn: seq % 5 == 0,
        hud_ns: seq as i64 % 1000,
    }
}

fn hud_desc(rev: u64) -> HudBufferHeader {
    // Sizes change with the revision, so a mix of two publishes shows in the descriptor too.
    let (w, h) = (200 + (rev % 7) as u32 * 8, 100 + (rev % 5) as u32 * 4);
    let mut d = HudBufferHeader::ZERO;
    d.width = w;
    d.height = h;
    d.stride = w * 4 + (rev % 3) as u32 * 4;
    d.extent_w = 1920;
    d.extent_h = 1080;
    d.x = 12;
    d.y = 12;
    d.scale = 1.0;
    d.opacity = 0.9;
    d.model_rev = rev;
    d.ngraphs = 1;
    d.graph[0] = HudGraph { x: 4, y: 40, w: w as i32 - 8, h: 40, span_ms: 3000, series: 1, line: rgba_premul(255, 255, 255, 220), ..HudGraph::ZERO };
    d
}

fn hud_pixel(rev: u64, i: usize) -> u8 {
    (i as u64).wrapping_mul(31).wrapping_add(rev) as u8
}

#[test]
#[ignore = "child process entry point for the cross-process tests"]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE) else { return };
    let count: u64 = std::env::var(COUNT).unwrap().parse().unwrap();
    let dir = SessionDir::from_env().unwrap().expect("QUIVER_PERF_DIR");
    match role.as_str() {
        "layer" => {
            let w = dir.create_layer_ring(std::process::id(), ubi_perf::random_nonce()).unwrap();
            let mut s = LayerStatus::ZERO;
            s.flags = LAYER_DRAWING;
            s.extent_w = 1920;
            s.extent_h = 1080;
            LayerStatus::set_str(&mut s.engine_name, "DXVK");
            for seq in 1..=count {
                w.push(&present(seq).to_record()).unwrap();
                if seq % 1000 == 0 {
                    s.presents = seq;
                    s.drawn = seq / 5;
                    w.set_layer_status(&s);
                    w.heartbeat(mono_raw_ns());
                }
            }
            s.presents = count;
            w.set_layer_status(&s);
        }
        "crash" => {
            // A mod ring writer that dies mid-write: records, a claimed slot, then exit
            // without unmapping or any cleanup (no abort, so no core dump).
            let w = open_ring_writer(&dir.file(&FileKind::Mod { surface: "crashy".into(), conn: 0 })).unwrap();
            for i in 0..count {
                w.push(&Record::series(1, i as i64, i as f64)).unwrap();
            }
            w.claim_without_commit_for_test();
            std::process::exit(3);
        }
        "restart" => {
            let w = open_ring_writer(&dir.file(&FileKind::Mod { surface: "crashy".into(), conn: 0 })).unwrap();
            for i in 0..count {
                w.push(&Record::series(1, 1_000_000 + i as i64, i as f64)).unwrap();
            }
            w.heartbeat(42);
        }
        "sight" => {
            let mut w = dir.open_hud_writer().unwrap();
            w.set_writer_pid(std::process::id());
            let start = Instant::now();
            let mut rev = w.published();
            let mut px = vec![0u8; 1 << 20];
            while start.elapsed() < Duration::from_millis(count) {
                rev += 1;
                let d = hud_desc(rev);
                for (i, b) in px[..d.pixel_bytes()].iter_mut().enumerate() {
                    *b = hud_pixel(rev, i);
                }
                assert_eq!(w.publish(&d, &px[..d.pixel_bytes()]).unwrap(), rev);
                w.heartbeat(mono_raw_ns());
            }
        }
        other => panic!("unknown role {other}"),
    }
}

fn session(root: &Path, id: &str) -> SessionDir {
    SessionDir::create(root, &SessionId::parse(id).unwrap()).unwrap()
}

fn wait_for(path: &Path, child: &mut Child) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(child.try_wait().unwrap().is_none_or(|s| s.success()), "child failed before creating {}", path.display());
        assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(1));
    }
    path.to_owned()
}

/// The layer writes a million present records as fast as it can while quiverd's side drains
/// a 4096-slot ring: every record read is intact and in order, and read + lost + torn
/// accounts for every record written.
#[test]
fn layer_ring_across_processes() {
    let root = shm_root();
    let dir = session(root.path(), "s-xproc");
    const N: u64 = 1_000_000;
    let mut child = spawn("layer", dir.path(), N);
    let path = wait_for(&dir.file(&FileKind::Layer { pid: child.id() }), &mut child);
    let mut r = dir.open_ring(&FileKind::Layer { pid: child.id() }).unwrap();
    assert_eq!(r.info().writer_pid, child.id());
    let start = r.cursor();
    let (mut read, mut lost, mut torn, mut last_seq) = (0u64, 0u64, 0u64, 0u64);
    let mut status_seen = false;
    let mut done = false;
    let mut polls = 0u64;
    loop {
        // Mostly a tight drain, sometimes a quiverd-like pause, so the writer laps the ring
        // and the overrun path runs too.
        polls += 1;
        if polls % 16 == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let p = r.poll(usize::MAX, |i, rec| {
            rec.check(1).unwrap();
            let lp = LayerPresent::from_record(rec).unwrap();
            assert_eq!(lp.seq, i + 1, "record {i} is present seq {}", lp.seq);
            assert_eq!(lp, present(lp.seq));
            assert!(lp.seq > last_seq);
            last_seq = lp.seq;
        });
        read += p.read;
        lost += p.lost;
        torn += p.torn;
        if let Some(s) = r.layer_status() {
            assert_eq!(LayerStatus::get_str(&s.engine_name), Some("DXVK"));
            assert!(s.drawn * 5 <= s.presents);
            status_seen = true;
        }
        if done && !p.pending && r.cursor() == r.reserve() {
            break;
        }
        if !done && child.try_wait().unwrap().is_some() {
            done = true;
        }
    }
    assert!(child.wait().unwrap().success());
    assert_eq!(r.reserve(), N);
    assert_eq!(read + lost + torn, N - start, "read {read} lost {lost} torn {torn}");
    assert!(read > 0 && status_seen);
    assert_eq!(r.layer_status().unwrap().presents, N);
    assert!(r.region().is_file_at(&path));
    eprintln!("layer ring: read {read}, lost {lost} (overruns), torn {torn}, from {start}");
}

/// A mod ring writer crashes mid-write; a restarted writer attaches and carries on. The
/// reader gets everything committed, skips the dead slot after its stuck polls, and loses
/// nothing else.
#[test]
fn writer_crash_and_restart() {
    let root = shm_root();
    let dir = session(root.path(), "s-crash");
    let kind = FileKind::Mod { surface: "crashy".into(), conn: 0 };
    let p = RingParams::wine_mod(1024, 10_000_000, 1, ubi_perf::random_nonce());
    drop(dir.create_ring(&kind, &p).unwrap());
    let mut r = dir.open_ring(&kind).unwrap();
    let mut got = vec![];
    let mut lost = 0;
    let mut drain = |r: &mut ubi_perf::ring::RingReader<_>, got: &mut Vec<i64>| {
        let p = r.poll(usize::MAX, |_, rec| got.push(rec.t));
        lost += p.lost;
        p
    };
    let st = spawn("crash", dir.path(), 300).wait().unwrap();
    assert_eq!(st.code(), Some(3));
    // The dead slot is the newest: the reader waits on it.
    for _ in 0..10 {
        assert!(drain(&mut r, &mut got).pending);
    }
    assert_eq!(got.len(), 300);
    assert!(spawn("restart", dir.path(), 300).wait().unwrap().success());
    for _ in 0..10 {
        drain(&mut r, &mut got);
    }
    assert_eq!(lost, 1);
    assert_eq!(got.len(), 600);
    assert_eq!(&got[..300], &(0..300).collect::<Vec<_>>()[..]);
    assert_eq!(&got[300..], &(1_000_000..1_000_300).collect::<Vec<_>>()[..]);
    assert_eq!(r.heartbeat(), 42);
    assert_eq!(r.reserve(), 601);
}

/// quiver-sight publishes as fast as it can from one process; the layer's side polls from
/// another: every image taken is exactly one publish (descriptor and pixels).
#[test]
fn hud_image_across_processes() {
    let root = shm_root();
    let dir = session(root.path(), "s-hud");
    let bb = buffer_bytes_for(260 * 4, 120).unwrap();
    dir.create_hud_image(bb).unwrap();
    let mut r = dir.open_hud_reader().unwrap();
    assert!(r.is_stale(mono_raw_ns()));
    let mut child = spawn("sight", dir.path(), 1500);
    let mut dst = vec![0u8; bb as usize];
    let (mut images, mut torn, mut busy, mut last) = (0u64, 0u64, 0u64, 0u64);
    while child.try_wait().unwrap().is_none() {
        match r.poll((1920, 1080), &mut dst) {
            HudPoll::Image { published, desc } => {
                assert!(desc.model_rev >= published && desc.model_rev > last);
                assert_eq!(desc, { let mut d = hud_desc(desc.model_rev); d.seq = desc.seq; d });
                for (i, b) in dst[..desc.pixel_bytes()].iter().enumerate() {
                    assert_eq!(*b, hud_pixel(desc.model_rev, i), "pixel {i} of rev {}", desc.model_rev);
                }
                last = desc.model_rev;
                images += 1;
            }
            HudPoll::Torn => torn += 1,
            HudPoll::Busy => busy += 1,
            HudPoll::Unchanged => {}
            HudPoll::Invalid(e) => panic!("{e}"),
        }
    }
    assert!(child.wait().unwrap().success());
    assert!(images > 10, "images {images} torn {torn} busy {busy}");
    assert!(!r.is_stale(mono_raw_ns()));
    assert_eq!(r.writer_pid(), child.id());
    eprintln!("hud.image: {images} images, {torn} torn, {busy} busy, {} published", r.published());
}
