//! Wine interop (SIGHT-HUD §3.7, spike H3 in part): a Windows program maps a file under
//! `Z:\dev\shm\…` with `CreateFileMappingW` and writes ring records; this Linux reader sees
//! them, and their QPC timestamps are CLOCK_MONOTONIC_RAW / 100 (§3.9). Needs a Proton build,
//! so it's opt-in:
//!
//! `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton10-34 cargo test
//! -p ubi-perf --test wine -- --ignored`
//!
//! Runs headless in a throwaway prefix and HOME; builds the Windows example with cargo into
//! its own target dir (so it doesn't wait on the outer cargo's lock).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ubi_perf::clock::{WINE_QPC_FREQ, mono_raw_ns, wine_qpc_to_raw_ns};
use ubi_perf::layout::{CLOCK_QPC, FLAG_GENERATED, FLAG_REAL, PRODUCER_MOD, RING_STATUS_DATA_BYTES};
use ubi_perf::session::{open_ring_reader, to_windows_path, to_wine_unix_path};

fn build_writer() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.join("../../target/ubi-perf-wine");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "-q", "-p", "ubi-perf", "--example", "wine_ring_writer", "--target", "x86_64-pc-windows-gnu", "--target-dir"])
        .arg(&target)
        .current_dir(manifest)
        .status()
        .unwrap();
    assert!(st.success(), "building the Windows example failed");
    target.join("x86_64-pc-windows-gnu/debug/examples/wine_ring_writer.exe")
}

struct Wine {
    proton: PathBuf,
    tmp: tempfile::TempDir,
}

impl Wine {
    fn command(&self, args: &[&str]) -> Command {
        let t = self.tmp.path();
        let mut cmd = Command::new(self.proton.join("files/bin/wine"));
        cmd.args(args)
            .env("WINEPREFIX", t.join("pfx"))
            .env("HOME", t.join("home"))
            .env("XDG_DATA_HOME", t.join("home/.local/share"))
            .env("XDG_CONFIG_HOME", t.join("home/.config"))
            .env("XDG_CACHE_HOME", t.join("home/.cache"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        cmd
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let out = self.command(args).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

impl Drop for Wine {
    fn drop(&mut self) {
        let _ = Command::new(self.proton.join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", self.tmp.path().join("pfx")).status();
    }
}

/// Runs the writer under Wine while draining its ring from here, as quiverd would.
fn write_and_drain(wine: &Wine, exe_win: &str, ring: &Path, ring_win: &str, count: u64) {
    let before = mono_raw_ns();
    let child = wine.command(&[exe_win, ring_win, &count.to_string()]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let waiter = std::thread::spawn(move || child.wait_with_output().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut r = loop {
        match open_ring_reader(ring) {
            Ok(r) => break r,
            Err(_) if !waiter.is_finished() => {
                assert!(Instant::now() < deadline, "no ring from wine (pid {pid})");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("{}: {e}; wine output: {:?}", ring.display(), waiter.join().unwrap()),
        }
    };
    r.seek_oldest();
    let mut recs = vec![];
    let (mut lost, mut torn) = (0, 0);
    loop {
        let finished = waiter.is_finished();
        let p = r.poll(usize::MAX, |i, rec| recs.push((i, *rec)));
        lost += p.lost;
        torn += p.torn;
        if finished && r.cursor() == r.reserve() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let out = waiter.join().unwrap();
    let after = mono_raw_ns();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));

    let info = *r.info();
    let (nonce, freq) = stdout.trim().split_once(' ').unwrap();
    assert_eq!(ubi_perf::control::nonce_to_hex(&info.nonce), nonce);
    assert_eq!(freq.parse::<u64>().unwrap(), info.clock_freq);
    assert_eq!((info.clock, info.producer_kind, info.slot_count), (CLOCK_QPC, PRODUCER_MOD, 1024));
    // Wine 10: QPC is CLOCK_MONOTONIC_RAW in 100 ns ticks.
    assert_eq!(info.clock_freq, WINE_QPC_FREQ, "QPC frequency under this Proton");
    assert_eq!(r.reserve(), count);
    assert_eq!((recs.len() as u64, lost, torn), (count, 0, 0), "a 1024-slot ring drained every ms at 10k records/s");
    let mut last_t = 0;
    for (i, rec) in &recs {
        let ns = wine_qpc_to_raw_ns(rec.t);
        assert!(ns >= before && ns <= after, "record {i}: QPC×100 = {ns} outside [{before}, {after}]");
        assert!(rec.t >= last_t);
        last_t = rec.t;
        if i % 2 == 0 {
            assert_eq!(rec.key, *i);
            assert_eq!(rec.flags, if i % 4 == 0 { FLAG_REAL } else { FLAG_GENERATED });
            assert_eq!(rec.fields(), &[rec.t, rec.t + 1, *i as i64]);
        } else {
            assert_eq!(rec.series_value(), Some(*i as f64));
        }
    }
    let mut buf = [0u8; RING_STATUS_DATA_BYTES];
    assert_eq!(r.status(&mut buf), Some((7, 9)));
    assert_eq!(&buf[..9], b"from wine");
    assert_eq!(r.heartbeat(), last_t);
    let span_ms = (wine_qpc_to_raw_ns(last_t) - wine_qpc_to_raw_ns(recs[0].1.t)) / 1_000_000;
    eprintln!("{}: {count} records from Wine over {span_ms} ms, read live; QPC×100 within the RAW window", ring.display());
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON (a Proton build directory)"]
fn wine_maps_session_dir_files() {
    let Some(proton) = std::env::var_os("QUIVER_TEST_PROTON").map(PathBuf::from) else {
        panic!("set QUIVER_TEST_PROTON to a Proton build directory");
    };
    let exe = build_writer();
    let wine = Wine { proton, tmp: tempfile::tempdir().unwrap() };
    std::fs::create_dir_all(wine.tmp.path().join("home")).unwrap();
    // Make the prefix first, so its setup isn't inside the timing window.
    let (ok, out, err) = wine.run(&["cmd", "/c", "echo ready"]);
    assert!(ok, "{out}\n{err}");
    let shm = tempfile::Builder::new().prefix("ubi-perf-wine-").tempdir_in("/dev/shm").unwrap();
    let exe_win = to_windows_path(&exe).unwrap();
    let z = shm.path().join("mod-z.ring");
    write_and_drain(&wine, &exe_win, &z, &to_windows_path(&z).unwrap(), 5000);
    let unix = shm.path().join("mod-unix.ring");
    write_and_drain(&wine, &exe_win, &unix, &to_wine_unix_path(&unix).unwrap(), 5000);
}
