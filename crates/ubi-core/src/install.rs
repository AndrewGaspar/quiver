//! Installing a game from its manifest: download slices, verify, decompress, assemble files.
//!
//! Each file in the manifest is a list of slices; a slice is stored on the CDN under the
//! SHA-1 of its compressed bytes and decompresses to `size` bytes at `fileOffset`.
//! Progress is resumable: finished files are recorded in `<dest>/.ubi-install/done`.

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use sha1::{Digest, Sha1};
use ubi_proto::download;

use crate::download::{Downloader, slice_path};

/// How many slices are fetched concurrently.
const PARALLEL: usize = 24;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
/// Slice paths per signed-URL request.
const URL_BATCH: usize = 256;

#[derive(Debug, Clone)]
pub struct SliceRef {
    pub sha1: Vec<u8>,
    pub size: u64,
    pub download_size: u64,
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Path relative to the install dir, with `/` separators.
    pub path: String,
    pub size: u64,
    pub slices: Vec<SliceRef>,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    pub dirs: Vec<String>,
    pub install_bytes: u64,
    pub download_bytes: u64,
    pub chunks: Vec<u32>,
}

/// Choose chunks: all without a language, plus those for `language`; required first.
pub fn plan(manifest: &download::Manifest, language: &str) -> Result<Plan> {
    let mut chunks: Vec<&download::Chunk> = manifest
        .chunks
        .iter()
        .filter(|c| c.language.as_deref().is_none_or(|l| l.is_empty() || l.eq_ignore_ascii_case(language)))
        .collect();
    chunks.sort_by_key(|c| (c.r#type, c.id));
    let mut plan = Plan::default();
    let mut seen = HashSet::new();
    for chunk in chunks {
        plan.chunks.push(chunk.id);
        for f in &chunk.files {
            let path = f.name.replace('\\', "/");
            if f.is_dir {
                plan.dirs.push(path);
                continue;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            if f.slice_list.is_empty() && !f.slices.is_empty() {
                bail!("{path}: manifest uses the legacy slice format, not supported yet");
            }
            let mut offset = 0;
            let slices = f
                .slice_list
                .iter()
                .map(|s| {
                    let r = SliceRef {
                        sha1: s.download_sha1.clone().unwrap_or_default(),
                        size: u64::from(s.size),
                        download_size: u64::from(s.download_size.unwrap_or(s.size)),
                        offset: s.file_offset.unwrap_or(offset),
                    };
                    offset = r.offset + r.size;
                    r
                })
                .collect::<Vec<_>>();
            plan.install_bytes += f.size;
            plan.download_bytes += slices.iter().map(|s| s.download_size).sum::<u64>();
            plan.files.push(PlannedFile { path, size: f.size, slices });
        }
    }
    Ok(plan)
}

#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub written: AtomicU64,
    pub files_done: AtomicU64,
}

fn state_dir(dest: &Path) -> PathBuf {
    dest.join(".ubi-install")
}

fn load_done(dest: &Path) -> HashSet<String> {
    std::fs::read_to_string(state_dir(dest).join("done"))
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

struct Job {
    file: Arc<std::fs::File>,
    file_idx: usize,
    slice: SliceRef,
}

/// Install `plan` into `dest`. `compressed` says whether slices are zstd-compressed.
pub async fn install(dl: &mut Downloader, plan: &Plan, dest: &Path, compressed: bool, progress: &Progress) -> Result<()> {
    std::fs::create_dir_all(state_dir(dest))?;
    for d in &plan.dirs {
        std::fs::create_dir_all(dest.join(d))?;
    }
    let done = load_done(dest);
    let mut done_log = std::fs::OpenOptions::new().create(true).append(true).open(state_dir(dest).join("done"))?;

    // Open every pending file at its final size; slices are written in place.
    let mut jobs = Vec::new();
    let mut remaining: Vec<usize> = Vec::with_capacity(plan.files.len());
    for (i, f) in plan.files.iter().enumerate() {
        remaining.push(f.slices.len());
        if done.contains(&f.path) {
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            progress.written.fetch_add(f.size, Ordering::Relaxed);
            continue;
        }
        let path = dest.join(&f.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path)?;
        file.set_len(f.size)?;
        if f.slices.is_empty() {
            writeln!(done_log, "{}", f.path)?;
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let file = Arc::new(file);
        for s in &f.slices {
            jobs.push(Job { file: file.clone(), file_idx: i, slice: s.clone() });
        }
    }

    // Fetch signed URLs per batch, then download that batch in parallel.
    for batch in jobs.chunks(URL_BATCH) {
        let paths: Vec<String> = batch.iter().map(|j| slice_path(&j.slice.sha1)).collect();
        let urls = dl.urls(&paths).await?;
        let dl_ref = &*dl;
        let finished: Vec<usize> = futures::stream::iter(batch.iter().zip(urls))
            .map(|(job, mirrors)| async move {
                let body = dl_ref.get(&mirrors).await.with_context(|| slice_path(&job.slice.sha1))?;
                if Sha1::digest(&body).as_slice() != job.slice.sha1.as_slice() {
                    bail!("slice {} failed SHA-1 check", hex::encode_upper(&job.slice.sha1));
                }
                progress.downloaded.fetch_add(body.len() as u64, Ordering::Relaxed);
                let job_file = job.file.clone();
                let slice = job.slice.clone();
                let written = tokio::task::spawn_blocking(move || -> Result<u64> {
                    // Slices that don't compress are stored raw; detect by the zstd frame magic.
                    let data = if compressed && body.starts_with(&ZSTD_MAGIC) {
                        zstd::bulk::decompress(&body, slice.size as usize).context("zstd")?
                    } else {
                        body
                    };
                    if data.len() as u64 != slice.size {
                        bail!("slice decompressed to {} bytes, expected {}", data.len(), slice.size);
                    }
                    job_file.write_all_at(&data, slice.offset)?;
                    Ok(slice.size)
                })
                .await??;
                progress.written.fetch_add(written, Ordering::Relaxed);
                Ok::<usize, anyhow::Error>(job.file_idx)
            })
            .buffer_unordered(PARALLEL)
            .try_collect()
            .await?;
        for idx in finished {
            remaining[idx] -= 1;
            if remaining[idx] == 0 {
                writeln!(done_log, "{}", plan.files[idx].path)?;
                progress.files_done.fetch_add(1, Ordering::Relaxed);
            }
        }
        done_log.flush()?;
    }
    Ok(())
}
