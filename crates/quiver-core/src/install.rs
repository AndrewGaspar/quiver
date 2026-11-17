//! Installing a game from its manifest: download slices, verify, decompress, assemble files.
//!
//! Each file in the manifest is a list of slices; a slice is stored on the CDN under the
//! SHA-1 of its compressed bytes and decompresses to `size` bytes at `fileOffset`.
//! Progress is resumable: finished files are recorded in `<dest>/.quiver-install/done`, and
//! finished slices of unfinished files in `<dest>/.quiver-install/slices`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sha1::{Digest, Sha1};
use quiver_proto::download;

use crate::download::slice_path_for;

/// How many slices are fetched concurrently.
const PARALLEL: usize = 24;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

fn is_zlib(b: &[u8]) -> bool {
    b.len() >= 2 && b[0] & 0x0f == 8 && (u16::from(b[0]) << 8 | u16::from(b[1])) % 31 == 0
}
/// Slice paths per signed-URL request.
const URL_BATCH: usize = 256;

#[derive(Debug, Clone)]
pub struct SliceRef {
    /// SHA-1 of the bytes as stored on the CDN (compressed, if the build is).
    pub sha1: Vec<u8>,
    /// SHA-1 of the decompressed bytes as they are on disk (the manifest's legacy
    /// `slices` list); empty if the manifest doesn't have it.
    pub data_sha1: Vec<u8>,
    pub size: u64,
    pub download_size: u64,
    pub offset: u64,
}

impl SliceRef {
    /// The hash the installed bytes of this slice must have, if the manifest says.
    pub fn disk_sha1(&self) -> Option<&[u8]> {
        if self.data_sha1.len() == 20 {
            Some(&self.data_sha1)
        } else if self.download_size == self.size && self.sha1.len() == 20 {
            Some(&self.sha1) // stored raw: the CDN hash is the data's hash
        } else {
            None
        }
    }

    /// Same bytes at the same place (so an old copy on disk is already right).
    fn same_data(&self, other: &SliceRef) -> bool {
        self.offset == other.offset
            && self.size == other.size
            && match (self.disk_sha1(), other.disk_sha1()) {
                (Some(a), Some(b)) => a == b,
                _ => self.sha1 == other.sha1,
            }
    }
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Path relative to the install dir, with `/` separators.
    pub path: String,
    pub size: u64,
    pub slices: Vec<SliceRef>,
}

#[derive(Debug, Default, Clone)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    pub dirs: Vec<String>,
    pub install_bytes: u64,
    pub download_bytes: u64,
    pub chunks: Vec<u32>,
    /// `download.CompressionMethod`; decides the CDN layout.
    pub compression: Option<i32>,
}

/// Choose chunks: all without a language, plus those for `language` (`*` = every
/// language); required first.
pub fn plan(manifest: &download::Manifest, language: &str) -> Result<Plan> {
    let mut chunks: Vec<&download::Chunk> = manifest
        .chunks
        .iter()
        .filter(|c| {
            language == "*" || c.language.as_deref().is_none_or(|l| l.is_empty() || l.eq_ignore_ascii_case(language))
        })
        .collect();
    chunks.sort_by_key(|c| (c.r#type, c.id));
    if manifest.compression_method == Some(download::CompressionMethod::Lzham as i32) {
        bail!("manifest uses LZHAM compression, which is not supported yet");
    }
    let mut plan = Plan { compression: manifest.compression_method, ..Plan::default() };
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
                .enumerate()
                .map(|(i, s)| {
                    // Oldest manifests keep slice hashes in the legacy `slices` list. That
                    // list is the SHA-1 of the decompressed data, which equals the CDN hash
                    // only for slices stored raw.
                    let data_sha1 = f.slices.get(i).cloned().unwrap_or_default();
                    let sha1 = s.download_sha1.clone().filter(|h| !h.is_empty()).or_else(|| f.slices.get(i).cloned());
                    let r = SliceRef {
                        sha1: sha1.unwrap_or_default(),
                        data_sha1,
                        size: u64::from(s.size),
                        download_size: u64::from(s.download_size.unwrap_or(s.size)),
                        offset: s.file_offset.unwrap_or(offset),
                    };
                    offset = r.offset + r.size;
                    r
                })
                .collect::<Vec<_>>();
            if let Some(bad) = slices.iter().find(|s| s.sha1.len() != 20) {
                bail!("{path}: slice at offset {} has no SHA-1 in the manifest", bad.offset);
            }
            plan.install_bytes += f.size;
            plan.download_bytes += slices.iter().map(|s| s.download_size).sum::<u64>();
            plan.files.push(PlannedFile { path, size: f.size, slices });
        }
    }
    Ok(plan)
}

/// Languages a manifest offers: its language list plus any chunk language, with the
/// download and install size of each language's own chunks.
pub fn languages(manifest: &download::Manifest) -> Vec<(String, u64, u64)> {
    let mut out: Vec<(String, u64, u64)> = Vec::new();
    let mut add = |code: &str, dl: u64, inst: u64| match out.iter_mut().find(|(c, ..)| c.eq_ignore_ascii_case(code)) {
        Some(e) => {
            e.1 += dl;
            e.2 += inst;
        }
        None => out.push((code.to_owned(), dl, inst)),
    };
    for code in manifest.languages.iter().filter_map(|l| l.code.as_deref()) {
        add(code, 0, 0);
    }
    for c in &manifest.chunks {
        let Some(code) = c.language.as_deref().filter(|l| !l.is_empty()) else { continue };
        let files = c.files.iter().filter(|f| !f.is_dir);
        let inst = files.clone().map(|f| f.size).sum();
        let dl = files.flat_map(|f| &f.slice_list).map(|s| u64::from(s.download_size.unwrap_or(s.size))).sum();
        add(code, dl, inst);
    }
    out
}

/// What moving an install from `old` to `new` involves (nothing is changed on disk).
#[derive(Debug, Default, Clone)]
pub struct UpdateDiff {
    /// Files already right on disk.
    pub unchanged: Vec<String>,
    /// Files in both builds that must be (partly) rewritten.
    pub changed: Vec<String>,
    pub added: Vec<String>,
    /// Files only in `old`; deleted by [`prepare_update`].
    pub removed: Vec<String>,
    /// Slices of changed files whose bytes are already in place: `(path, slice index in new)`.
    pub reusable: Vec<(String, usize)>,
    pub download_bytes: u64,
    /// Bytes that will be written (new and changed slices): the disk space the update
    /// may need, on a copy-on-write filesystem too.
    pub write_bytes: u64,
}

/// Compare an installed build with a new one. A file counts as unchanged when its slices
/// are the same, it has the right size on disk and (if the install keeps a done list) it
/// was finished.
pub fn diff(old: &Plan, new: &Plan, dest: &Path) -> UpdateDiff {
    let old_files: HashMap<&str, &PlannedFile> = old.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let new_paths: HashSet<&str> = new.files.iter().map(|f| f.path.as_str()).collect();
    let done = done_list(dest);
    let finished = |path: &str| done.as_ref().is_none_or(|d| d.contains(path));
    let mut d = UpdateDiff::default();
    for f in &new.files {
        let Some(o) = old_files.get(f.path.as_str()) else {
            d.added.push(f.path.clone());
            d.download_bytes += f.slices.iter().map(|s| s.download_size).sum::<u64>();
            d.write_bytes += f.size;
            continue;
        };
        let on_disk = dest.join(&f.path).symlink_metadata().ok().filter(|m| m.is_file());
        let same = o.size == f.size && o.slices.len() == f.slices.len() && o.slices.iter().zip(&f.slices).all(|(a, b)| a.same_data(b));
        if same && on_disk.as_ref().is_some_and(|m| m.len() == f.size) && finished(&f.path) {
            d.unchanged.push(f.path.clone());
            continue;
        }
        d.changed.push(f.path.clone());
        // A finished old copy keeps the slices that didn't move or change.
        let old_ok = on_disk.is_some() && finished(&f.path);
        for (i, s) in f.slices.iter().enumerate() {
            if old_ok && o.slices.iter().any(|os| os.offset == s.offset && os.same_data(s)) {
                d.reusable.push((f.path.clone(), i));
            } else {
                d.download_bytes += s.download_size;
                d.write_bytes += s.size;
            }
        }
    }
    d.removed = old.files.iter().filter(|f| !new_paths.contains(f.path.as_str())).map(|f| f.path.clone()).collect();
    d
}

/// Prepare `dest` for installing `new` over an existing install of `old`: files whose slices
/// are unchanged are recorded as done, slices of changed files that are already in place
/// are recorded too, files no longer in the game are deleted, and the rest will be
/// (re)downloaded. Returns (kept, removed) counts.
pub fn prepare_update(old: &Plan, new: &Plan, dest: &Path) -> Result<(usize, usize)> {
    let d = diff(old, new, dest);
    std::fs::create_dir_all(state_dir(dest))?;
    let mut done = std::fs::File::create(state_dir(dest).join("done"))?;
    for p in &d.unchanged {
        writeln!(done, "{p}")?;
    }
    let new_files: HashMap<&str, &PlannedFile> = new.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut slices = std::fs::File::create(state_dir(dest).join("slices"))?;
    for (path, i) in &d.reusable {
        writeln!(slices, "{}", slice_key(path, &new_files[path.as_str()].slices[*i]))?;
    }
    let mut removed = 0;
    for p in &d.removed {
        if std::fs::remove_file(dest.join(p)).is_ok() {
            removed += 1;
        }
    }
    Ok((d.unchanged.len(), removed))
}

/// Start over: forget which files and slices are done.
pub fn reset_progress(dest: &Path) -> Result<()> {
    for name in ["done", "slices"] {
        match std::fs::remove_file(state_dir(dest).join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Manifest hash of the build `dest` is being (or was last) installed from.
pub fn target(dest: &Path) -> Option<String> {
    std::fs::read_to_string(state_dir(dest).join("target")).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

pub fn set_target(dest: &Path, hash: &str) -> Result<()> {
    std::fs::create_dir_all(state_dir(dest))?;
    std::fs::write(state_dir(dest).join("target"), hash)?;
    Ok(())
}

/// Bytes still to download and to write for `plan` in `dest`, after files and slices
/// recorded as done.
pub fn remaining(plan: &Plan, dest: &Path) -> (u64, u64) {
    let done = load_done(dest);
    let slices = load_slices(dest);
    let (mut dl, mut write) = (0, 0);
    for f in plan.files.iter().filter(|f| !done.contains(&f.path)) {
        for s in f.slices.iter().filter(|s| !slices.contains(&slice_key(&f.path, s))) {
            dl += s.download_size;
            write += s.size;
        }
    }
    (dl, write)
}

/// Free bytes for unprivileged writes on the filesystem holding `path` (or its nearest
/// existing parent).
pub fn free_space(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let mut p = path;
    while !p.exists() {
        p = p.parent().context("no existing parent directory")?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {}", p.display()));
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Space kept free on top of what an install needs: 1 GiB or 5%, whichever is more.
pub fn space_margin(required: u64) -> u64 {
    (1 << 30).max(required / 20)
}

/// Refuse to start when `required` bytes (plus the margin) don't fit at `dest`.
pub fn check_space(dest: &Path, required: u64) -> Result<()> {
    let free = free_space(dest)?;
    let margin = space_margin(required);
    if free < required + margin {
        bail!(
            "not enough free space for {}: needs {} plus {} margin, {} free",
            dest.display(),
            human_bytes(required),
            human_bytes(margin),
            human_bytes(free)
        );
    }
    Ok(())
}

pub fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// Paces downloads to a byte rate shared by every install using it (0 = unlimited).
/// The rate can change while downloads run.
#[derive(Debug)]
pub struct Throttle {
    rate: AtomicU64,
    next: Mutex<Instant>,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Throttle {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self { rate: AtomicU64::new(bytes_per_sec), next: Mutex::new(Instant::now()) }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
        *self.next.lock().unwrap() = Instant::now();
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Wait until `bytes` more may be fetched.
    pub async fn take(&self, bytes: u64) {
        let rate = self.rate();
        if rate == 0 {
            return;
        }
        let wait = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            let start = (*next).max(now);
            *next = start + Duration::from_secs_f64(bytes as f64 / rate as f64);
            start - now
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[derive(Default)]
pub struct Progress {
    /// Bytes to download in total (set once the plan is known).
    pub total: AtomicU64,
    pub downloaded: AtomicU64,
    pub written: AtomicU64,
    pub files_done: AtomicU64,
    /// Verifying: bytes hashed so far, of `hash_total`.
    pub hashed: AtomicU64,
    pub hash_total: AtomicU64,
    /// Set to stop the install; finished files stay recorded, so it can be resumed.
    pub cancel: std::sync::atomic::AtomicBool,
    /// Bandwidth limit shared with other downloads.
    pub throttle: Option<Arc<Throttle>>,
    /// Slice retries so far, and slices waiting to retry right now.
    pub retries: AtomicU64,
    pub retrying: AtomicU64,
    pub retry: RetryPolicy,
}

/// How often and how patiently a failing slice is tried again.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Tries per slice, the first included.
    pub attempts: u32,
    /// Wait before the first retry; doubles each time (with jitter) up to `max_delay`.
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Signed URLs older than this are renewed before use.
    pub url_max_age: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { attempts: 6, base_delay: Duration::from_secs(1), max_delay: Duration::from_secs(30), url_max_age: Duration::from_secs(600) }
    }
}

impl RetryPolicy {
    /// Wait before retry `n` (1-based): base·2^(n-1), capped, ±25% jitter.
    pub fn delay(&self, n: u32) -> Duration {
        let d = self.base_delay.saturating_mul(1 << (n.saturating_sub(1)).min(16)).min(self.max_delay);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        let jitter = 0.75 + (nanos % 1000) as f64 / 2000.0;
        d.mul_f64(jitter)
    }
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The error an install returns when it was cancelled through [`Progress::cancel`].
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Whether an install error means it was cancelled.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Cancelled>())
}

/// The install's progress state in the game's folder (`.quiver-install`; installs from before
/// the rename to Quiver have `.ubi-install`, moved here on first use).
pub fn state_dir(dest: &Path) -> PathBuf {
    crate::launcher::migrate_renamed(dest.join(".quiver-install"), &dest.join(".ubi-install"))
}

fn load_done(dest: &Path) -> HashSet<String> {
    done_list(dest).unwrap_or_default()
}

/// The done list, or `None` if the install has none at all.
fn done_list(dest: &Path) -> Option<HashSet<String>> {
    std::fs::read_to_string(state_dir(dest).join("done")).ok().map(|s| s.lines().map(str::to_owned).collect())
}

fn slice_key(path: &str, s: &SliceRef) -> String {
    format!("{} {} {path}", s.offset, hex::encode_upper(&s.sha1))
}

fn load_slices(dest: &Path) -> HashSet<String> {
    std::fs::read_to_string(state_dir(dest).join("slices"))
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Remove files from the done list (and their slices from the slice list), so the next
/// install rewrites them.
pub(crate) fn forget_done(dest: &Path, paths: &HashSet<&str>) -> Result<()> {
    if let Some(done) = done_list(dest) {
        let mut keep: Vec<&String> = done.iter().filter(|p| !paths.contains(p.as_str())).collect();
        keep.sort();
        let mut text = String::new();
        for p in keep {
            text.push_str(p);
            text.push('\n');
        }
        std::fs::write(state_dir(dest).join("done"), text)?;
    }
    match std::fs::remove_file(state_dir(dest).join("slices")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Why fetching a slice failed, and what to do about it.
#[derive(Debug)]
pub enum SliceError {
    /// Transient (connection, timeout, cut-off body, 5xx, 429): try again later.
    Retry(anyhow::Error),
    /// The signed URL was refused or has expired (403/404/410…): get a fresh one.
    Refresh(anyhow::Error),
    /// Retrying won't help.
    Fatal(anyhow::Error),
}

impl SliceError {
    pub fn into_inner(self) -> anyhow::Error {
        match self {
            Self::Retry(e) | Self::Refresh(e) | Self::Fatal(e) => e,
        }
    }
}

/// Where slices come from: the CDN through `download_service`, or a stand-in in tests.
#[allow(async_fn_in_trait)]
pub trait SliceSource {
    /// Signed URLs (one or more mirrors each) for relative CDN paths.
    async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>>;
    /// GET the first working mirror.
    async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError>;
}

/// Fetch one slice and check its hash, retrying transient failures with backoff and
/// renewing its URL when the CDN refuses it or it is old. `Ok(None)`: cancelled.
async fn fetch_slice(
    dl: &impl SliceSource,
    path: &str,
    sha1: &[u8],
    mut mirrors: Vec<String>,
    mut issued: Instant,
    progress: &Progress,
) -> Result<Option<Vec<u8>>> {
    let policy = progress.retry;
    let mut attempt = 0;
    loop {
        attempt += 1;
        if issued.elapsed() > policy.url_max_age {
            if let Ok(mut fresh) = dl.urls(&[path.to_owned()]).await
                && let Some(m) = fresh.pop()
            {
                (mirrors, issued) = (m, Instant::now());
            }
        }
        let err = match dl.get(&mirrors).await {
            Ok(body) if Sha1::digest(&body).as_slice() == sha1 => return Ok(Some(body)),
            // A cut-off or mangled body: the next try usually works.
            Ok(_) => SliceError::Retry(anyhow::anyhow!("slice {} failed SHA-1 check", hex::encode_upper(sha1))),
            Err(e) => e,
        };
        let refresh = matches!(err, SliceError::Refresh(_));
        let e = match err {
            SliceError::Fatal(e) => return Err(e.context(path.to_owned())),
            SliceError::Retry(e) | SliceError::Refresh(e) => e,
        };
        if attempt >= policy.attempts {
            return Err(e.context(format!("{path}: gave up after {attempt} tries")));
        }
        tracing::debug!("{path}: try {attempt} failed ({e:#}); retrying");
        progress.retries.fetch_add(1, Ordering::Relaxed);
        progress.retrying.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(policy.delay(attempt)).await;
        progress.retrying.fetch_sub(1, Ordering::Relaxed);
        if progress.cancelled() {
            return Ok(None);
        }
        if refresh {
            // Expired or refused: ask download_service again (that can fail transiently too).
            match dl.urls(&[path.to_owned()]).await {
                Ok(mut fresh) if !fresh.is_empty() => (mirrors, issued) = (fresh.pop().unwrap(), Instant::now()),
                Ok(_) => {}
                Err(e) => tracing::debug!("{path}: renewing the URL failed: {e:#}"),
            }
        }
    }
}

/// Signed URLs for a batch, retrying transient download_service failures.
async fn batch_urls(dl: &impl SliceSource, paths: &[String], progress: &Progress) -> Result<Vec<Vec<String>>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match dl.urls(paths).await {
            Ok(u) => return Ok(u),
            Err(e) if attempt >= progress.retry.attempts => return Err(e.context("requesting download URLs")),
            Err(e) => {
                tracing::debug!("URL request failed ({e:#}); retrying");
                progress.retries.fetch_add(1, Ordering::Relaxed);
                progress.retrying.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(progress.retry.delay(attempt)).await;
                progress.retrying.fetch_sub(1, Ordering::Relaxed);
                if progress.cancelled() {
                    return Err(Cancelled.into());
                }
            }
        }
    }
}

struct Job {
    file: Arc<std::fs::File>,
    file_idx: usize,
    slice: SliceRef,
}

/// Install `plan` into `dest`. `compressed` says whether slices are zstd-compressed.
///
/// Cancelling (see [`Progress::cancel`]) stops at a slice boundary: slices in flight
/// finish and are recorded, so a later install picks up where this one stopped.
pub async fn install(
    dl: &mut impl SliceSource,
    plan: &Plan,
    dest: &Path,
    compressed: bool,
    progress: &Progress,
) -> Result<()> {
    let _ = compressed; // decided per slice from its bytes
    std::fs::create_dir_all(state_dir(dest))?;
    for d in &plan.dirs {
        std::fs::create_dir_all(dest.join(d))?;
    }
    let done = load_done(dest);
    let slices_done = load_slices(dest);
    let mut done_log = std::fs::OpenOptions::new().create(true).append(true).open(state_dir(dest).join("done"))?;
    let mut slice_log = std::fs::OpenOptions::new().create(true).append(true).open(state_dir(dest).join("slices"))?;

    // Open every pending file at its final size; slices are written in place.
    let mut jobs = Vec::new();
    let mut remaining: Vec<usize> = Vec::with_capacity(plan.files.len());
    for (i, f) in plan.files.iter().enumerate() {
        if done.contains(&f.path) {
            // Already installed by an earlier (interrupted) run: count it as done.
            remaining.push(0);
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            progress.written.fetch_add(f.size, Ordering::Relaxed);
            progress.downloaded.fetch_add(f.slices.iter().map(|s| s.download_size).sum::<u64>(), Ordering::Relaxed);
            continue;
        }
        let path = dest.join(&f.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path)?;
        file.set_len(f.size)?;
        let pending: Vec<&SliceRef> = f
            .slices
            .iter()
            .filter(|s| {
                let have = slices_done.contains(&slice_key(&f.path, s));
                if have {
                    progress.downloaded.fetch_add(s.download_size, Ordering::Relaxed);
                    progress.written.fetch_add(s.size, Ordering::Relaxed);
                }
                !have
            })
            .collect();
        remaining.push(pending.len());
        if pending.is_empty() {
            writeln!(done_log, "{}", f.path)?;
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let file = Arc::new(file);
        for s in pending {
            jobs.push(Job { file: file.clone(), file_idx: i, slice: s.clone() });
        }
    }
    done_log.flush()?;

    // Fetch signed URLs per batch, then download that batch in parallel.
    for batch in jobs.chunks(URL_BATCH) {
        if progress.cancelled() {
            return Err(Cancelled.into());
        }
        let paths: Vec<String> = batch.iter().map(|j| slice_path_for(plan.compression, &j.slice.sha1)).collect();
        let urls = batch_urls(&*dl, &paths, progress).await?;
        if urls.len() != paths.len() {
            bail!("asked for {} URLs, got {}", paths.len(), urls.len());
        }
        let issued = Instant::now();
        let dl_ref = &*dl;
        let results: Vec<Result<Option<&Job>>> = futures::stream::iter(batch.iter().zip(paths.iter()).zip(urls))
            .map(|((job, path), mirrors)| async move {
                if let Some(t) = &progress.throttle {
                    t.take(job.slice.download_size).await;
                }
                if progress.cancelled() {
                    return Ok(None); // stop at this slice boundary
                }
                let Some(body) = fetch_slice(dl_ref, path, &job.slice.sha1, mirrors, issued, progress).await? else {
                    return Ok(None);
                };
                progress.downloaded.fetch_add(body.len() as u64, Ordering::Relaxed);
                let job_file = job.file.clone();
                let slice = job.slice.clone();
                let written = tokio::task::spawn_blocking(move || -> Result<u64> {
                    // Stored raw when the download is as large as the data (uncompressed
                    // manifests, or slices that didn't compress); else zstd or zlib.
                    let data = if body.len() as u64 == slice.size {
                        body
                    } else if body.starts_with(&ZSTD_MAGIC) {
                        zstd::bulk::decompress(&body, slice.size as usize).context("zstd")?
                    } else if is_zlib(&body) {
                        let mut out = Vec::with_capacity(slice.size as usize);
                        std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&body[..]), &mut out).context("zlib")?;
                        out
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
                Ok::<_, anyhow::Error>(Some(job))
            })
            .buffer_unordered(PARALLEL)
            .collect()
            .await;
        // Record every slice that made it, even if another one failed for good.
        let mut failed = None;
        let mut finished = Vec::new();
        for r in results {
            match r {
                Ok(j) => finished.extend(j),
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        for job in finished {
            let idx = job.file_idx;
            remaining[idx] -= 1;
            if remaining[idx] == 0 {
                writeln!(done_log, "{}", plan.files[idx].path)?;
                progress.files_done.fetch_add(1, Ordering::Relaxed);
            } else {
                writeln!(slice_log, "{}", slice_key(&plan.files[idx].path, &job.slice))?;
            }
        }
        done_log.flush()?;
        slice_log.flush()?;
        if let Some(e) = failed {
            return Err(e);
        }
        if progress.cancelled() {
            return Err(Cancelled.into());
        }
    }
    drop(slice_log);
    let _ = std::fs::remove_file(state_dir(dest).join("slices"));
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use quiver_proto::download::{Chunk, File, Manifest, Slice};

    /// A file's bytes, cut into slices of `slice` bytes, each zstd-compressed.
    pub(crate) fn file_entry(name: &str, data: &[u8], slice: usize, cdn: &mut HashMap<String, Vec<u8>>) -> File {
        let mut f = File { name: name.into(), size: data.len() as u64, is_dir: false, ..Default::default() };
        for (i, part) in data.chunks(slice.max(1)).enumerate() {
            let body = zstd::bulk::compress(part, 3).unwrap();
            let sha = Sha1::digest(&body).to_vec();
            cdn.insert(hex::encode_upper(&sha), body.clone());
            f.slices.push(Sha1::digest(part).to_vec());
            f.slice_list.push(Slice {
                size: part.len() as u32,
                download_size: Some(body.len() as u32),
                download_sha1: Some(sha),
                file_offset: Some((i * slice) as u64),
            });
        }
        f
    }

    /// A zstd-era manifest: one required chunk with `files`, plus a `fr-FR` chunk.
    pub(crate) fn fixture(files: &[(&str, Vec<u8>)], cdn: &mut HashMap<String, Vec<u8>>) -> Manifest {
        let main = files.iter().filter(|(n, _)| !n.starts_with("fr/")).map(|(n, d)| file_entry(n, d, 1000, cdn)).collect();
        let fr = files.iter().filter(|(n, _)| n.starts_with("fr/")).map(|(n, d)| file_entry(n, d, 1000, cdn)).collect();
        Manifest {
            chunks: vec![
                Chunk { id: 0, r#type: 0, files: main, ..Default::default() },
                Chunk { id: 1, r#type: 0, files: fr, language: Some("fr-FR".into()), ..Default::default() },
            ],
            compression_method: Some(download::CompressionMethod::Zstd as i32),
            is_compressed: Some(true),
            game_version: Some("1.0".into()),
            ..Default::default()
        }
    }

    pub(crate) fn data(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 8) as u8).collect()
    }

    /// Serves slices from memory, by the hash at the end of the CDN path.
    #[derive(Default)]
    pub(crate) struct FakeCdn {
        pub slices: HashMap<String, Vec<u8>>,
        pub gets: std::sync::atomic::AtomicUsize,
        /// Set the progress's cancel flag after this many GETs.
        pub cancel_after: Option<(usize, Arc<Progress>)>,
        /// Slice hash -> how many more GETs of it fail as transient errors.
        pub flaky: Mutex<HashMap<String, usize>>,
        /// URLs are `<hash>@<generation>`; GETs of an older generation are refused (403).
        pub generation: AtomicU64,
        pub url_requests: std::sync::atomic::AtomicUsize,
    }

    impl SliceSource for FakeCdn {
        async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
            self.url_requests.fetch_add(1, Ordering::SeqCst);
            let g = self.generation.load(Ordering::SeqCst);
            Ok(paths.iter().map(|p| vec![format!("{}@{g}", p.rsplit('/').next().unwrap())]).collect())
        }
        async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError> {
            let n = self.gets.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((after, p)) = &self.cancel_after
                && n >= *after
            {
                p.cancel.store(true, Ordering::SeqCst);
            }
            tokio::task::yield_now().await;
            let (hash, g) = mirrors[0].split_once('@').unwrap();
            if g.parse::<u64>().unwrap() < self.generation.load(Ordering::SeqCst) {
                return Err(SliceError::Refresh(anyhow::anyhow!("HTTP 403 Forbidden")));
            }
            if let Some(left) = self.flaky.lock().unwrap().get_mut(hash).filter(|n| **n > 0) {
                *left -= 1;
                return Err(SliceError::Retry(anyhow::anyhow!("connection timed out")));
            }
            self.slices.get(hash).cloned().ok_or_else(|| SliceError::Fatal(anyhow::anyhow!("404")))
        }
    }

    fn quick() -> Progress {
        Progress {
            retry: RetryPolicy { base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(4), ..RetryPolicy::default() },
            ..Progress::default()
        }
    }

    #[tokio::test]
    async fn transient_failures_are_retried() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("game");
        let mut cdn = FakeCdn::default();
        let m = fixture(&[("a.bin", data(1, 5_000)), ("b.bin", data(2, 1_000))], &mut cdn.slices);
        let p = plan(&m, "en-US").unwrap();
        let a = p.files[0].slices.clone();
        // One slice times out twice, another five times (the sixth try works).
        cdn.flaky.lock().unwrap().insert(hex::encode_upper(&a[1].sha1), 2);
        cdn.flaky.lock().unwrap().insert(hex::encode_upper(&a[3].sha1), 5);
        let progress = quick();
        install(&mut cdn, &p, &dest, true, &progress).await.unwrap();
        assert_eq!(progress.retries.load(Ordering::SeqCst), 7);
        assert_eq!(progress.retrying.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), data(1, 5_000));

        // Six failures exhaust the tries: the job fails, but what finished is kept.
        std::fs::remove_dir_all(&dest).unwrap();
        cdn.flaky.lock().unwrap().insert(hex::encode_upper(&a[2].sha1), 6);
        let err = install(&mut cdn, &p, &dest, true, &quick()).await.unwrap_err();
        assert!(format!("{err:#}").contains("gave up after 6 tries"), "{err:#}");
        assert!(!is_cancelled(&err));
        let (left, _) = remaining(&p, &dest);
        assert_eq!(left, a[2].download_size, "only the failed slice is left");
        let before = cdn.gets.load(Ordering::SeqCst);
        install(&mut cdn, &p, &dest, true, &quick()).await.unwrap();
        assert_eq!(cdn.gets.load(Ordering::SeqCst) - before, 1);
        assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), data(1, 5_000));
    }

    /// Hands out URLs that expire right away the first time.
    struct Expiring(FakeCdn, std::sync::atomic::AtomicBool);

    impl SliceSource for Expiring {
        async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
            let u = self.0.urls(paths).await;
            if !self.1.swap(true, Ordering::SeqCst) {
                self.0.generation.fetch_add(1, Ordering::SeqCst);
            }
            u
        }
        async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError> {
            self.0.get(mirrors).await
        }
    }

    #[tokio::test]
    async fn expired_urls_are_renewed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("game");
        let mut cdn = FakeCdn::default();
        let m = fixture(&[("a.bin", data(5, 4_000))], &mut cdn.slices);
        let p = plan(&m, "en-US").unwrap();
        let mut src = Expiring(cdn, Default::default());
        let progress = quick();
        install(&mut src, &p, &dest, true, &progress).await.unwrap();
        assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), data(5, 4_000));
        assert_eq!(progress.retries.load(Ordering::SeqCst), 4, "each slice was refused once");
        assert_eq!(src.0.url_requests.load(Ordering::SeqCst), 1 + 4, "one batch request, one renewal per slice");

        // URLs past their age are renewed before use, without a failed GET.
        std::fs::remove_dir_all(&dest).unwrap();
        let progress = Progress { retry: RetryPolicy { url_max_age: Duration::ZERO, ..quick().retry }, ..Progress::default() };
        src.1.store(false, Ordering::SeqCst);
        install(&mut src, &p, &dest, true, &progress).await.unwrap();
        assert_eq!(progress.retries.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let r = RetryPolicy::default();
        let d: Vec<Duration> = (1..6).map(|n| r.delay(n)).collect();
        assert!(d[0] >= Duration::from_millis(750) && d[0] <= Duration::from_millis(1250));
        assert!(d[4] >= Duration::from_secs(12) && d[4] <= Duration::from_secs(20));
        assert!(r.delay(30) <= Duration::from_millis(37_500));
    }

    #[tokio::test]
    async fn install_pause_resume_update() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("game");
        let mut cdn = FakeCdn::default();
        let v1 = fixture(
            &[("a.bin", data(1, 25_000)), ("sub/b.bin", data(2, 3_500)), ("empty", vec![]), ("fr/voice.bin", data(3, 2_000))],
            &mut cdn.slices,
        );
        let plan1 = plan(&v1, "en-US").unwrap();
        assert_eq!(plan1.files.len(), 3, "fr-FR chunk left out");
        assert_eq!(languages(&v1), vec![("fr-FR".to_owned(), plan(&v1, "fr-FR").unwrap().download_bytes - plan1.download_bytes, 2_000)]);
        assert_eq!(remaining(&plan1, &dest), (plan1.download_bytes, plan1.install_bytes));

        // Pause after a few slices: what finished is kept.
        let progress = Arc::new(Progress::default());
        cdn.cancel_after = Some((5, progress.clone()));
        let err = install(&mut cdn, &plan1, &dest, true, &progress).await.unwrap_err();
        assert!(is_cancelled(&err));
        let (left, _) = remaining(&plan1, &dest);
        assert!(left < plan1.download_bytes && left > 0, "partial progress recorded ({left})");
        let before = cdn.gets.load(Ordering::SeqCst);

        // Resume fetches only what is left.
        cdn.cancel_after = None;
        let progress = Progress::default();
        install(&mut cdn, &plan1, &dest, true, &progress).await.unwrap();
        let fetched = cdn.gets.load(Ordering::SeqCst) - before;
        let total_slices: usize = plan1.files.iter().map(|f| f.slices.len()).sum();
        assert!(fetched < total_slices, "resume refetched {fetched} of {total_slices}");
        assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), data(1, 25_000));
        assert_eq!(std::fs::read(dest.join("sub/b.bin")).unwrap(), data(2, 3_500));
        assert_eq!(std::fs::read(dest.join("empty")).unwrap(), b"");
        assert_eq!(remaining(&plan1, &dest), (0, 0));
        assert!(!state_dir(&dest).join("slices").exists());

        // Update: a.bin changes in one slice, b.bin goes, c.bin is new.
        let mut a2 = data(1, 25_000);
        a2[12_345] ^= 0xff;
        let v2 = fixture(&[("a.bin", a2.clone()), ("c.bin", data(4, 1_500)), ("empty", vec![])], &mut cdn.slices);
        let plan2 = plan(&v2, "en-US").unwrap();
        let d = diff(&plan1, &plan2, &dest);
        assert_eq!(d.changed, ["a.bin"]);
        assert_eq!(d.added, ["c.bin"]);
        assert_eq!(d.removed, ["sub/b.bin"]);
        assert_eq!(d.unchanged, ["empty"]);
        assert_eq!(d.reusable.len(), 24, "only the changed slice of a.bin is downloaded");
        let a_slice = &plan2.files.iter().find(|f| f.path == "a.bin").unwrap().slices[12];
        let c_bytes: u64 = plan2.files.iter().find(|f| f.path == "c.bin").unwrap().slices.iter().map(|s| s.download_size).sum();
        assert_eq!(d.download_bytes, a_slice.download_size + c_bytes);
        assert_eq!(prepare_update(&plan1, &plan2, &dest).unwrap(), (1, 1));
        assert_eq!(remaining(&plan2, &dest).0, d.download_bytes);
        let before = cdn.gets.load(Ordering::SeqCst);
        install(&mut cdn, &plan2, &dest, true, &Progress::default()).await.unwrap();
        assert_eq!(cdn.gets.load(Ordering::SeqCst) - before, 3, "one slice of a.bin, two of c.bin");
        assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), a2);
        assert!(!dest.join("sub/b.bin").exists());
    }

    #[tokio::test]
    async fn throttle_paces() {
        let t = Throttle::new(1_000_000);
        let start = Instant::now();
        for _ in 0..4 {
            t.take(50_000).await;
        }
        // Three waits of 50 ms after the first.
        assert!(start.elapsed() >= Duration::from_millis(140), "{:?}", start.elapsed());
        t.set_rate(0);
        let start = Instant::now();
        t.take(u64::MAX / 2).await;
        assert!(start.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn free_space_of_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let free = free_space(&dir.path().join("not/yet")).unwrap();
        assert!(free > 0);
        assert!(check_space(dir.path(), u64::MAX / 4).unwrap_err().to_string().contains("not enough free space"));
        check_space(dir.path(), 0).ok(); // fits unless the test machine is nearly full
    }
}
