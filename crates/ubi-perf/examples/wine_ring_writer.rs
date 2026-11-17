//! A Windows program for the Wine interop check (`tests/wine.rs`, SIGHT-HUD spike H3): it
//! creates a ring the way the bridge would (`CreateFileW` + `CreateFileMappingW` +
//! `MapViewOfFile` on a `Z:\dev\shm\…` path) and writes records with QPC timestamps that a
//! Linux reader checks.
//!
//! `wine wine_ring_writer.exe <ring path> <count>`; prints `<nonce hex> <qpc freq>`.

#[cfg(not(windows))]
fn main() {
    eprintln!("build with --target x86_64-pc-windows-gnu and run under Wine");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    use ubi_perf::clock::qpc;
    use ubi_perf::layout::{FLAG_GENERATED, FLAG_REAL};
    use ubi_perf::record::Record;
    use ubi_perf::ring::RingParams;

    let args: Vec<String> = std::env::args().collect();
    let path = std::path::Path::new(&args[1]);
    let count: u64 = args[2].parse().unwrap();
    let (_, freq) = qpc();
    let nonce = ubi_perf::random_nonce();
    let p = RingParams::wine_mod(1024, freq, std::process::id(), nonce);
    let w = match ubi_perf::session::create_ring_file(path, &p) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("create {}: {e}", path.display());
            std::process::exit(3);
        }
    };
    let start = std::time::Instant::now();
    for i in 0..count {
        let (t, _) = qpc();
        let rec = if i % 2 == 0 {
            Record::frame(1, i, if i % 4 == 0 { FLAG_REAL } else { FLAG_GENERATED }, 9, t, &[t, t + 1, i as i64]).unwrap()
        } else {
            Record::series(2, t, i as f64)
        };
        if w.push(&rec).is_none() {
            std::process::exit(4);
        }
        w.heartbeat(t);
        // Pace to ~10k records/s, the rate H3 asks about.
        while start.elapsed().as_micros() < (i as u128 + 1) * 100 {
            std::hint::spin_loop();
        }
    }
    w.set_status(7, b"from wine");
    println!("{} {freq}", ubi_perf::control::nonce_to_hex(&nonce));
}
