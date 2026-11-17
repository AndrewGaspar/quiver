//! `include/quiver_perf.h` must not drift from the Rust layouts:
//! - every `QV_ASSERT_SIZE` / `QV_ASSERT_OFFSET` in the header equals the Rust value, and
//!   every Rust size/offset is asserted there (both directions);
//! - every numeric `#define QV_*` equals the Rust constant, both directions;
//! - the header compiles as C99 and C++ (native, and MinGW when installed), so the compiler
//!   checks those asserts against the header's own structs;
//! - a C program using the header's writer produces records the Rust reader accepts.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use quiver_perf::layout::{C_LAYOUT, FLAG_HAS_DURATION, FLAG_HAS_LABEL, FLAG_REAL, RING_MIN_SLOTS};
use quiver_perf::record::Record;
use quiver_perf::ring::RingParams;
use quiver_perf::session::{create_ring_file, open_ring_reader};

fn header() -> &'static str {
    quiver_perf::C_HEADER
}

fn include_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("include")
}

fn parse_int(s: &str) -> Option<u64> {
    let s = s.trim_end_matches(['u', 'U', 'l', 'L']);
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn defines() -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for line in header().lines() {
        let Some(rest) = line.trim_start().strip_prefix("#define ") else { continue };
        let mut it = rest.split_whitespace();
        let (Some(name), Some(value)) = (it.next(), it.next()) else { continue };
        if !name.starts_with("QV_") || name.contains('(') {
            continue;
        }
        if let Some(v) = parse_int(value) {
            assert!(out.insert(name.to_owned(), v).is_none(), "{name} defined twice");
        }
    }
    out
}

fn asserts() -> (BTreeMap<String, usize>, BTreeMap<(String, String), usize>) {
    let (mut sizes, mut offsets) = (BTreeMap::new(), BTreeMap::new());
    for line in header().lines() {
        let args = |rest: &str| -> Vec<String> {
            rest.trim_end_matches(';').trim_end_matches(')').split(',').map(|s| s.trim().to_owned()).collect()
        };
        if let Some(rest) = line.strip_prefix("QV_ASSERT_SIZE(") {
            let a = args(rest);
            sizes.insert(a[0].clone(), a[1].parse().unwrap());
        } else if let Some(rest) = line.strip_prefix("QV_ASSERT_OFFSET(") {
            let a = args(rest);
            offsets.insert((a[0].clone(), a[1].clone()), a[2].parse().unwrap());
        }
    }
    (sizes, offsets)
}

#[test]
fn constants_match() {
    let defs = defines();
    let rust: BTreeMap<String, u64> = C_LAYOUT.constants.iter().map(|(n, v)| (n.to_string(), *v)).collect();
    for (name, v) in &rust {
        assert_eq!(defs.get(name), Some(v), "#define {name} in quiver_perf.h");
    }
    for (name, v) in &defs {
        assert_eq!(rust.get(name), Some(v), "{name} = {v} in the header has no Rust constant in C_LAYOUT");
    }
}

#[test]
fn layout_asserts_match() {
    let (sizes, offsets) = asserts();
    let rs: BTreeMap<String, usize> = C_LAYOUT.sizes.iter().map(|(n, v)| (n.to_string(), *v)).collect();
    assert_eq!(sizes, rs, "QV_ASSERT_SIZE lines vs Rust sizes");
    let ro: BTreeMap<(String, String), usize> = C_LAYOUT.offsets.iter().map(|(s, f, o)| ((s.to_string(), f.to_string()), *o)).collect();
    for (k, v) in &ro {
        assert_eq!(offsets.get(k), Some(v), "QV_ASSERT_OFFSET({}, {}) in quiver_perf.h", k.0, k.1);
    }
    for (k, v) in &offsets {
        assert_eq!(ro.get(k), Some(v), "QV_ASSERT_OFFSET({}, {}, {v}) has no Rust counterpart", k.0, k.1);
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success())
}

fn compile(tool: &str, args: &[&str], src: &Path, out: &Path) {
    let o = Command::new(tool)
        .args(args)
        .args(["-Wall", "-Wextra", "-Werror", "-pedantic", "-I"])
        .arg(include_dir())
        .arg(src)
        .arg("-o")
        .arg(out)
        .output()
        .unwrap();
    assert!(o.status.success(), "{tool} {args:?} {}:\n{}", src.display(), String::from_utf8_lossy(&o.stderr));
}

#[test]
fn header_compiles() {
    let dir = tempfile::tempdir().unwrap();
    let check = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/c/check.c");
    let mut ran = vec![];
    if have("cc") {
        compile("cc", &["-std=c99", "-O2"], &check, &dir.path().join("check"));
        compile("cc", &["-std=c11", "-O0"], &check, &dir.path().join("check11"));
        ran.push("cc");
    }
    if have("c++") {
        compile("c++", &["-x", "c++", "-std=c++11"], &check, &dir.path().join("checkxx"));
        ran.push("c++");
    }
    if have("x86_64-w64-mingw32-gcc") {
        compile("x86_64-w64-mingw32-gcc", &["-std=c99", "-O2"], &check, &dir.path().join("check.exe"));
        ran.push("mingw");
    }
    if have("i686-w64-mingw32-gcc") {
        compile("i686-w64-mingw32-gcc", &["-std=c99", "-O2"], &check, &dir.path().join("check32.exe"));
        ran.push("mingw32");
    }
    // 32-bit SysV aligns uint64_t to 4 in structs; the asserts show the layout still holds.
    // (Needs multilib; skipped if -m32 can't build.)
    let probe = dir.path().join("probe.c");
    std::fs::write(&probe, "#include <stdint.h>\n#include <string.h>\nint main(void) { return 0; }\n").unwrap();
    if have("cc") && Command::new("cc").args(["-m32", "-c", "-o"]).arg(dir.path().join("probe.o")).arg(&probe).status().is_ok_and(|s| s.success()) {
        compile("cc", &["-m32", "-std=c99", "-c"], &check, &dir.path().join("check32.o"));
        ran.push("cc -m32");
    }
    if have("clang") {
        compile("clang", &["-std=c99", "-O2"], &check, &dir.path().join("checkclang"));
        ran.push("clang");
    }
    eprintln!("compiled quiver_perf.h with {ran:?}");
}

/// The header's writer (GCC atomics path) against the Rust reader, through a real file.
#[test]
fn c_writer_rust_reader() {
    if !have("cc") {
        eprintln!("no C compiler: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("ring_writer");
    compile("cc", &["-std=c99", "-O2"], &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/c/ring_writer.c"), &exe);
    let shm = tempfile::Builder::new().prefix("quiver-perf-test-").tempdir_in("/dev/shm").unwrap();
    let ring = shm.path().join("mod-c.ring");
    let w = create_ring_file(&ring, &RingParams::wine_mod(RING_MIN_SLOTS * 4, 10_000_000, 1, [3; 16])).unwrap();
    drop(w);
    let mut r = open_ring_reader(&ring).unwrap();
    let out = Command::new(&exe).arg(&ring).arg("50").output().unwrap();
    assert!(out.status.success(), "{:?} {}", out.status, String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "4096 128");
    let mut recs: Vec<Record> = vec![];
    let p = r.poll(usize::MAX, |_, rec| recs.push(*rec));
    // 64 slots: all 50 fit.
    assert_eq!((p.read, p.lost, p.torn), (50, 0, 0));
    assert_eq!(r.heartbeat(), 12345);
    assert_eq!(r.dropped(), 2);
    for (i, rec) in recs.iter().enumerate() {
        let i = i as i64;
        assert_eq!(rec.commit, i as u64 + 1);
        assert_eq!(rec.t, i * 1000);
        rec.check(3).unwrap();
        match i % 3 {
            0 => {
                assert_eq!((rec.metric, rec.flags, rec.swapchain, rec.key), (1, FLAG_REAL, 7, i as u64));
                assert_eq!(rec.fields(), &[i * 10, i * 10 + 1, i * 10 + 2]);
            }
            1 => assert_eq!((rec.metric, rec.series_value()), (2, Some(i as f64 * 0.5))),
            _ => {
                assert_eq!(rec.metric, 3);
                assert_eq!(rec.flags, FLAG_HAS_LABEL | FLAG_HAS_DURATION);
                assert_eq!(rec.event_duration(), Some(i));
                let label = rec.event_label().unwrap();
                assert!(label.len() <= 48 && "marker é with a label longer than forty-eight bytes".starts_with(label), "{label}");
                assert_eq!(label, Record::event(3, 0, None, Some("marker é with a label longer than forty-eight bytes")).event_label().unwrap());
            }
        }
    }
}
