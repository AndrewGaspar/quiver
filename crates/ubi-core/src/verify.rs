//! Verifying an install against its manifest and repairing it.
//!
//! Every slice of every file the plan lists is read back and hashed (SHA-1 of the
//! decompressed bytes, the manifest's legacy `slices` list). Only files in the plan are
//! read or written: anything else in the install folder (mods, extra DLLs, configs,
//! logs) is left alone. A repair re-downloads just the slices that failed.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use sha1::{Digest, Sha1};
use ubi_api::downloads::{CorruptFile, VerifyReport};

use crate::install::{self, Cancelled, PlannedFile, Plan, Progress};

/// Threads hashing files at once.
const THREADS: usize = 4;

/// A verify's findings, with the slices to fetch again.
#[derive(Debug, Default)]
pub struct Verification {
    pub report: VerifyReport,
    /// Path -> indexes of bad slices (every slice for a missing file).
    pub bad: HashMap<String, Vec<usize>>,
    /// Manifest paths that are symlinks or hard links in the install (repair replaces
    /// them with a file of their own, so the other end is never written).
    pub shared: HashSet<String>,
}

enum FileResult {
    Ok,
    Missing,
    Corrupt { size: u64, bad: Vec<usize>, shared: bool },
    Unverified,
}

fn check_file(dest: &Path, f: &PlannedFile, progress: &Progress, buf: &mut Vec<u8>) -> Result<FileResult> {
    let path = dest.join(&f.path);
    let Ok(meta) = path.symlink_metadata() else {
        progress.hashed.fetch_add(f.size, Ordering::Relaxed);
        return Ok(FileResult::Missing);
    };
    let all = || (0..f.slices.len()).collect::<Vec<_>>();
    if meta.file_type().is_symlink() || !meta.is_file() {
        progress.hashed.fetch_add(f.size, Ordering::Relaxed);
        return Ok(FileResult::Corrupt { size: meta.len(), bad: all(), shared: true });
    }
    let shared = meta.nlink() > 1;
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => {
            progress.hashed.fetch_add(f.size, Ordering::Relaxed);
            return Ok(FileResult::Corrupt { size: meta.len(), bad: all(), shared });
        }
    };
    let mut bad = Vec::new();
    let mut unverified = false;
    for (i, s) in f.slices.iter().enumerate() {
        if progress.cancelled() {
            return Err(Cancelled.into());
        }
        let ok = match s.disk_sha1() {
            None => {
                unverified = true;
                true
            }
            Some(_) if s.offset + s.size > meta.len() => false,
            Some(want) => {
                buf.resize(s.size as usize, 0);
                file.read_exact_at(buf, s.offset).is_ok() && Sha1::digest(&buf[..]).as_slice() == want
            }
        };
        progress.hashed.fetch_add(s.size, Ordering::Relaxed);
        if !ok {
            bad.push(i);
        }
    }
    if meta.len() != f.size && bad.is_empty() {
        // Right data, wrong length (extra bytes at the end): rewrite the last slice, which
        // also sets the length. A file with no slices just gets truncated.
        bad.extend(f.slices.len().checked_sub(1));
        if f.slices.is_empty() {
            return Ok(FileResult::Corrupt { size: meta.len(), bad, shared });
        }
    }
    Ok(if !bad.is_empty() {
        FileResult::Corrupt { size: meta.len(), bad, shared }
    } else if unverified {
        FileResult::Unverified
    } else {
        FileResult::Ok
    })
}

/// Hash every file of `plan` in `dest` (blocking; run it off the async runtime).
/// Progress goes to `progress.hashed` / `hash_total`; `progress.cancel` stops it.
pub fn verify(plan: &Plan, dest: &Path, progress: &Progress) -> Result<Verification> {
    progress.hash_total.store(plan.install_bytes, Ordering::Relaxed);
    progress.hashed.store(0, Ordering::Relaxed);
    // Biggest first, so one huge file doesn't finish alone at the end.
    let mut order: Vec<usize> = (0..plan.files.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(plan.files[i].size));
    let next = AtomicUsize::new(0);
    let results: Vec<Result<Vec<(usize, FileResult)>>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS.min(plan.files.len()).max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    let mut buf = Vec::new();
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = order.get(n) else { break };
                        out.push((i, check_file(dest, &plan.files[i], progress, &mut buf)?));
                    }
                    Ok(out)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap_or_else(|_| Err(anyhow::anyhow!("verify thread panicked")))).collect()
    });
    let mut all = Vec::with_capacity(plan.files.len());
    for r in results {
        all.extend(r?);
    }
    all.sort_by_key(|(i, _)| *i);
    let mut v = Verification::default();
    v.report.files = plan.files.len();
    v.report.bytes = plan.install_bytes;
    for (i, r) in all {
        let f = &plan.files[i];
        match r {
            FileResult::Ok => v.report.ok += 1,
            FileResult::Unverified => {
                v.report.ok += 1;
                v.report.unverified.push(f.path.clone());
            }
            FileResult::Missing => {
                v.report.missing.push(f.path.clone());
                v.bad.insert(f.path.clone(), (0..f.slices.len()).collect());
            }
            FileResult::Corrupt { size, bad, shared } => {
                v.report.corrupt.push(CorruptFile {
                    path: f.path.clone(),
                    size,
                    expected_size: f.size,
                    bad_slices: bad.len(),
                    bad_bytes: bad.iter().map(|&j| f.slices[j].size).sum(),
                });
                if shared {
                    v.shared.insert(f.path.clone());
                }
                v.bad.insert(f.path.clone(), bad);
            }
        }
    }
    Ok(v)
}

/// A plan holding only what failed: missing files whole, corrupt files with just their
/// bad slices.
pub fn repair_plan(plan: &Plan, v: &Verification) -> Plan {
    let mut out = Plan { compression: plan.compression, ..Plan::default() };
    for f in &plan.files {
        let Some(bad) = v.bad.get(&f.path) else { continue };
        let slices: Vec<_> = bad.iter().map(|&i| f.slices[i].clone()).collect();
        out.download_bytes += slices.iter().map(|s| s.download_size).sum::<u64>();
        out.install_bytes += slices.iter().map(|s| s.size).sum::<u64>();
        out.files.push(PlannedFile { path: f.path.clone(), size: f.size, slices });
    }
    out
}

/// Get `dest` ready for installing a [`repair_plan`]: the bad files are taken off the done
/// list, and links are replaced by files of their own (a symlink is removed; a hard
/// link is copied, which reflinks on btrfs/xfs) so a repair never writes through to
/// another file. Reflinked copies need nothing: writing to one never changes the other.
pub fn prepare_repair(dest: &Path, repair: &Plan, v: &Verification) -> Result<()> {
    let paths: HashSet<&str> = repair.files.iter().map(|f| f.path.as_str()).collect();
    install::forget_done(dest, &paths)?;
    for p in &v.shared {
        let path = dest.join(p);
        let Ok(meta) = path.symlink_metadata() else { continue };
        if meta.file_type().is_symlink() || !meta.is_file() {
            if meta.is_dir() {
                anyhow::bail!("{p} is a directory; the manifest lists it as a file");
            }
            std::fs::remove_file(&path)?;
        } else {
            let tmp = path.with_file_name(format!(".{}.quiver-repair", path.file_name().unwrap_or_default().to_string_lossy()));
            std::fs::copy(&path, &tmp)?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::tests::{FakeCdn, data, fixture};

    #[tokio::test]
    async fn verify_and_repair_only_what_is_bad() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("game");
        let mut cdn = FakeCdn::default();
        let m = fixture(
            &[("a.bin", data(1, 9_500)), ("b.bin", data(2, 4_000)), ("c.bin", data(3, 2_000)), ("d.bin", data(4, 3_000)), ("e.bin", data(5, 1_000))],
            &mut cdn.slices,
        );
        let plan = install::plan(&m, "en-US").unwrap();
        install::install(&mut cdn, &plan, &dest, true, &Progress::default()).await.unwrap();

        // An extra file next to the game's, which must not be touched.
        std::fs::write(dest.join("nvngx_dlss.dll"), b"mod").unwrap();
        let progress = Progress::default();
        let v = verify(&plan, &dest, &progress).unwrap();
        assert!(v.report.is_ok(), "{:?}", v.report);
        assert_eq!(v.report.ok, 5);
        assert_eq!(progress.hashed.load(Ordering::Relaxed), plan.install_bytes);

        // Break things: one flipped byte, a missing file, a truncated file, a hard link
        // shared with a file outside the install, and a file with junk on the end.
        let mut a = std::fs::read(dest.join("a.bin")).unwrap();
        a[5_123] ^= 1;
        std::fs::write(dest.join("a.bin"), &a).unwrap();
        std::fs::remove_file(dest.join("b.bin")).unwrap();
        std::fs::write(dest.join("c.bin"), &data(3, 2_000)[..1_500]).unwrap();
        let outside = dir.path().join("outside.bin");
        let mut wrong = data(4, 3_000);
        wrong[10] ^= 1;
        std::fs::write(&outside, &wrong).unwrap();
        std::fs::remove_file(dest.join("d.bin")).unwrap();
        std::fs::hard_link(&outside, dest.join("d.bin")).unwrap();
        let mut e = data(5, 1_000);
        e.extend_from_slice(b"junk");
        std::fs::write(dest.join("e.bin"), &e).unwrap();

        let v = verify(&plan, &dest, &Progress::default()).unwrap();
        assert_eq!(v.report.missing, ["b.bin"]);
        let corrupt: Vec<(&str, usize)> = v.report.corrupt.iter().map(|c| (c.path.as_str(), c.bad_slices)).collect();
        assert_eq!(corrupt, [("a.bin", 1), ("c.bin", 1), ("d.bin", 1), ("e.bin", 1)]);
        assert_eq!(v.report.ok, 0);
        assert!(v.shared.contains("d.bin"));

        let repair = repair_plan(&plan, &v);
        let b_slices = plan.files.iter().find(|f| f.path == "b.bin").unwrap().slices.len();
        assert_eq!(repair.files.iter().map(|f| f.slices.len()).sum::<usize>(), 4 + b_slices);
        prepare_repair(&dest, &repair, &v).unwrap();
        let before = cdn.gets.load(Ordering::SeqCst);
        install::install(&mut cdn, &repair, &dest, true, &Progress::default()).await.unwrap();
        assert_eq!(cdn.gets.load(Ordering::SeqCst) - before, 4 + b_slices);

        assert!(verify(&plan, &dest, &Progress::default()).unwrap().report.is_ok());
        assert_eq!(std::fs::read(dest.join("e.bin")).unwrap(), data(5, 1_000));
        assert_eq!(std::fs::read(&outside).unwrap(), wrong, "the other end of the hard link is untouched");
        assert_eq!(std::fs::read(dest.join("nvngx_dlss.dll")).unwrap(), b"mod");
        let done = std::fs::read_to_string(dest.join(".ubi-install/done")).unwrap();
        assert_eq!(done.lines().count(), 5);
    }

    #[test]
    fn symlinks_are_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("game");
        let mut cdn = FakeCdn::default();
        let m = fixture(&[("x.bin", data(7, 1_200))], &mut cdn.slices);
        let plan = install::plan(&m, "en-US").unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let target = dir.path().join("elsewhere.bin");
        std::fs::write(&target, data(7, 1_200)).unwrap();
        std::os::unix::fs::symlink(&target, dest.join("x.bin")).unwrap();
        let v = verify(&plan, &dest, &Progress::default()).unwrap();
        assert_eq!(v.report.corrupt.len(), 1);
        let repair = repair_plan(&plan, &v);
        prepare_repair(&dest, &repair, &v).unwrap();
        assert!(!dest.join("x.bin").exists());
        assert!(target.exists());
    }
}
