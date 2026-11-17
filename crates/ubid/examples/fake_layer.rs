//! A stand-in for the Sight layer, for trying the perf hub without a game: writes a
//! layer ring with present records and a status block into `$QUIVER_PERF_DIR`.
//!
//! ```sh
//! eval "$(quiver perf dev-session 3539)"            # QUIVER_SESSION, QUIVER_PERF_DIR
//! cargo run -p ubid --example fake_layer -- 144 30   # 144 fps for 30 s
//! quiver perf watch 3539
//! ```

use std::time::{Duration, Instant};

use ubi_perf::clock::mono_raw_ns;
use ubi_perf::layout::{DRAW_ERR_NO_IMAGE, LayerStatus};
use ubi_perf::record::LayerPresent;
use ubi_perf::session::SessionDir;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let fps: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(144.0);
    let secs: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(30.0);
    let dir = SessionDir::from_env()?.ok_or_else(|| std::io::Error::other("QUIVER_PERF_DIR is not set"))?;
    let w = dir.create_layer_ring(std::process::id(), ubi_perf::random_nonce())?;
    let mut s = LayerStatus::ZERO;
    (s.extent_w, s.extent_h, s.swapchain, s.swapchain_count, s.image_count) = (2560, 1600, 1, 1, 3);
    s.vendor_id = 0x10de;
    s.draw_error = DRAW_ERR_NO_IMAGE;
    LayerStatus::set_str(&mut s.engine_name, "DXVK");
    LayerStatus::set_str(&mut s.device_name, "fake layer");
    let period = Duration::from_secs_f64(1.0 / fps);
    let start = Instant::now();
    let mut seq = 0u64;
    let mut next = Instant::now();
    eprintln!("fake layer: {fps} fps for {secs} s into {}", dir.path().display());
    while start.elapsed().as_secs_f64() < secs {
        seq += 1;
        let t = mono_raw_ns();
        let p = LayerPresent { swapchain: 1, present_id: Some(seq), seq, entry_ns: t, return_ns: t + 80_000, image_index: (seq % 3) as u32, present_mode: 2, ..Default::default() };
        w.push(&p.to_record());
        s.presents = seq;
        if seq.is_multiple_of(16) {
            w.set_layer_status(&s);
            w.heartbeat(t);
        }
        // A hitch now and then: one frame at 4× the period.
        next += if seq.is_multiple_of(500) { period * 4 } else { period };
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    Ok(())
}
