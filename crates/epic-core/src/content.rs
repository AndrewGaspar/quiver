//! Installing an Epic build from its manifest (design/STORE-EPIC.md §3 C4): download the
//! chunks the files need, each once, and cut the files out of them.
//!
//! - **Only manifest paths are written** (relative, no `..`, no NUL; `\` read as `/`), and
//!   [`uninstall`] removes only manifest files, so nothing else in the folder is touched.
//! - **Chunks** are fetched in the order files first use them, `workers` at a time, through
//!   the shared bandwidth limit, each tried again with backoff ([`RetryPolicy`]); each is
//!   checked (SHA-1, guid) before use. A chunk stays in memory until the last file using
//!   it is written; past [`MEMORY_BUDGET`] chunks wait on disk in
//!   `.quiver-install/epic-chunks/` instead, deleted when done.
//! - **Files** are written one at a time to `<path>.quiver-part`, checked against their
//!   SHA-1 and renamed into place; executables get mode 0755.
//! - **Resume and update**: `.quiver-install/epic-state.json` records every finished file
//!   with its SHA-1. A restarted install skips them; an update keeps files whose SHA-1 is
//!   the same (by the record, or the old manifest) and still on disk at the right size,
//!   rebuilds the rest, and removes files the new build doesn't have.
//! - **Smaller updates**: a changed file's parts that its old copy already holds (same
//!   chunk, same range) are copied from that copy, once it hashes as the old manifest says
//!   ([`reusable_parts`]); and Epic's delta manifest for the two builds, when it publishes
//!   one, re-describes changed files mostly in terms of the old build's chunks ([`delta`]).
//!
//! [`RetryPolicy`]: quiver_core::install::RetryPolicy

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use futures::StreamExt;
use futures::future::BoxFuture;
use quiver_core::install::{Cancelled, Progress};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::manifest::{ChunkInfo, FileEntry, Guid, Manifest, Secrets, guid_hex};

/// Decoded chunks kept in memory at most; more wait on disk.
pub const MEMORY_BUDGET: usize = 256 << 20;
/// [`MEMORY_BUDGET`], lowered by tests to exercise spilling.
static BUDGET: AtomicUsize = AtomicUsize::new(MEMORY_BUDGET);
/// Downloaded chunk files larger than this are refused.
const MAX_CHUNK_DOWNLOAD: usize = 128 << 20;

/// Where chunk files come from (Epic's CDN, or a test).
pub trait ChunkSource: Send + Sync {
    /// The chunk file at `rel_path` (`ChunksV4/12/…chunk`) under the build's base URL.
    fn fetch<'a>(&'a self, rel_path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>>;
}

/// Chunks over HTTPS from the manifest API's base URLs, tried in order (the one that last
/// worked first).
pub struct HttpChunks {
    client: reqwest::Client,
    bases: Vec<String>,
    preferred: AtomicUsize,
}

impl HttpChunks {
    pub fn new(base_urls: Vec<String>, user_agent: &str) -> Result<Self> {
        ensure!(!base_urls.is_empty(), "no download locations for this build");
        // A connection that stops sending is given up on after STALL (and the chunk tried again
        // on a fresh one), not after the whole-request timeout: chunks are written in order, so
        // one stalled chunk held every worker behind it, and throughput fell to ~0 for up to
        // 2 minutes on a flaky Wi-Fi link. The bandwidth limit waits before a request, never
        // between reads, so a read gap this long is a real stall.
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STALL_TIMEOUT)
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(HttpChunks { client, bases: base_urls.into_iter().map(|b| b.trim_end_matches('/').to_owned()).collect(), preferred: AtomicUsize::new(0) })
    }
}

/// Connecting to a CDN host, and the longest gap between reads of a response.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// A URL's host, for messages (never the path or query, which can carry signatures).
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

impl ChunkSource for HttpChunks {
    fn fetch<'a>(&'a self, rel_path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let n = self.bases.len();
            let first = self.preferred.load(Ordering::Relaxed) % n;
            let mut last = String::new();
            for k in 0..n {
                let i = (first + k) % n;
                let base = &self.bases[i];
                let url = format!("{base}/{rel_path}");
                let why = match self.client.get(&url).send().await {
                    Ok(r) if r.status().is_success() => match read_capped(r).await {
                        Ok(b) => {
                            self.preferred.store(i, Ordering::Relaxed);
                            return Ok(b);
                        }
                        Err(e) => e,
                    },
                    Ok(r) => format!("HTTP {}", r.status().as_u16()),
                    Err(e) if e.is_timeout() => "timed out".into(),
                    Err(e) if e.is_connect() => "couldn't connect".into(),
                    Err(_) => "the request failed".into(),
                };
                last = format!("{}: {why}", host(base));
            }
            bail!("chunk {rel_path}: {last}")
        })
    }
}

async fn read_capped(mut r: reqwest::Response) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(r.content_length().unwrap_or(0).min(MAX_CHUNK_DOWNLOAD as u64) as usize);
    loop {
        match r.chunk().await {
            Ok(Some(b)) => {
                if out.len() + b.len() > MAX_CHUNK_DOWNLOAD {
                    return Err("the reply is too large for a chunk".into());
                }
                out.extend_from_slice(&b);
            }
            Ok(None) => return Ok(out),
            Err(_) => return Err("the reply was cut off".into()),
        }
    }
}

/// One install, update or repair.
pub struct Job<'a> {
    pub manifest: &'a Manifest,
    /// The installed build's manifest, for an update.
    pub old: Option<&'a Manifest>,
    pub dir: &'a Path,
    pub tags: Option<&'a [String]>,
    pub secrets: &'a Secrets,
    pub source: &'a dyn ChunkSource,
    pub progress: &'a Progress,
    /// Chunk downloads at once.
    pub workers: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub files_written: usize,
    pub files_kept: usize,
    pub files_removed: usize,
    pub bytes_downloaded: u64,
    /// Bytes of rewritten files copied from their old copies instead of downloaded.
    pub bytes_reused: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub ok: usize,
    pub bad: Vec<String>,
    pub missing: Vec<String>,
}

/// What an install or update would do, before it starts (previews, free space).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Manifest paths to (re)build.
    pub write: Vec<String>,
    pub keep: Vec<String>,
    /// Files of the old build (or earlier runs) the new one doesn't have.
    pub remove: Vec<String>,
    /// Chunk bytes to download.
    pub download_bytes: u64,
    /// File bytes to write.
    pub write_bytes: u64,
    /// Of `write_bytes`, bytes that replace files already on disk.
    pub replaced_bytes: u64,
    /// Of `write_bytes`, bytes copied from the old build's copy of the same file instead
    /// of downloaded ([`reusable_parts`]).
    pub reused_bytes: u64,
    /// Files rebuilt partly from their old copy: manifest path → for each part, where its
    /// bytes start in the old file (`None`: from the chunk).
    pub(crate) reuse: BTreeMap<String, Vec<Option<u64>>>,
}

/// A manifest path as a relative path under the install folder, or an error for anything
/// that could leave it.
pub fn safe_rel(p: &str) -> Result<PathBuf> {
    let p = p.replace('\\', "/");
    ensure!(!p.is_empty() && !p.contains('\0') && !p.starts_with('/'), "unsafe path in manifest: {p:?}");
    let mut out = PathBuf::new();
    for c in p.split('/').filter(|c| !c.is_empty() && *c != ".") {
        ensure!(c != ".." && !c.contains(':'), "unsafe path in manifest: {p:?}");
        out.push(c);
    }
    ensure!(out.components().all(|c| matches!(c, Component::Normal(_))) && out.components().next().is_some(), "unsafe path in manifest: {p:?}");
    Ok(out)
}

fn state_dir(dir: &Path) -> PathBuf {
    dir.join(".quiver-install")
}

fn spill_dir(dir: &Path) -> PathBuf {
    state_dir(dir).join("epic-chunks")
}

/// The resume record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    build_version: String,
    /// [`fingerprint`] of the manifest being installed.
    manifest: String,
    /// Finished files: manifest path → SHA-1 (hex).
    files: BTreeMap<String, String>,
}

fn state_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("epic-state.json")
}

fn load_state(dir: &Path) -> State {
    std::fs::read(state_path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_state(dir: &Path, s: &State) -> Result<()> {
    let path = state_path(dir);
    std::fs::create_dir_all(path.parent().expect("parent"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(s)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// What identifies a build's file set.
pub fn fingerprint(m: &Manifest) -> String {
    let mut h = Sha1::new();
    h.update(m.version.to_le_bytes());
    h.update(m.meta.build_version.as_bytes());
    for f in &m.files {
        h.update(f.path.as_bytes());
        h.update([0]);
        h.update(f.sha1);
    }
    hex::encode(h.finalize())
}

fn on_disk(dir: &Path, f: &FileEntry) -> bool {
    let Ok(rel) = safe_rel(&f.path) else { return false };
    let p = dir.join(rel);
    if !f.symlink_target.is_empty() {
        return std::fs::read_link(&p).is_ok_and(|t| t == Path::new(&f.symlink_target.replace('\\', "/")));
    }
    std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() && m.len() == f.size)
}

/// What [`install`] would do for `manifest` in `dir` (with `old` installed there). Old
/// copies of changed files are trusted by their size here (a preview); [`install`] hashes
/// them before it reads from them.
pub fn plan(manifest: &Manifest, old: Option<&Manifest>, dir: &Path, tags: Option<&[String]>) -> Result<Plan> {
    let record = load_state(dir);
    plan_with(manifest, old, dir, tags, &record, None)
}

/// Where each part of `new` can be read from `old` (the previous build's file at the same
/// path): a part whose chunk range lies inside a part of `old` cut from the same chunk is
/// the same bytes, because a chunk guid names one immutable window of data. `None` when no
/// part can be.
///
/// Same path only: the old file stays in place until the new one is renamed over it, so
/// it can be read while its replacement is written. Data another file had is downloaded.
pub fn reusable_parts(new: &FileEntry, old: &FileEntry) -> Option<Vec<Option<u64>>> {
    if !new.symlink_target.is_empty() || !old.symlink_target.is_empty() {
        return None;
    }
    let mut by_guid: HashMap<Guid, Vec<&crate::manifest::ChunkPart>> = HashMap::new();
    for q in &old.parts {
        by_guid.entry(q.guid).or_default().push(q);
    }
    let out: Vec<Option<u64>> = new
        .parts
        .iter()
        .map(|p| {
            let (s, e) = (p.offset as u64, p.offset as u64 + p.size as u64);
            by_guid
                .get(&p.guid)?
                .iter()
                .find(|q| q.offset as u64 <= s && e <= q.offset as u64 + q.size as u64 && q.file_offset + q.size as u64 <= old.size)
                .map(|q| q.file_offset + (s - q.offset as u64))
        })
        .collect();
    out.iter().any(Option::is_some).then_some(out)
}

/// Whether the old build's copy of a file is on disk as the old manifest describes it (by
/// size; [`install`] also checks its SHA-1).
fn old_copy_present(dir: &Path, old: &FileEntry) -> bool {
    old.size > 0 && old.symlink_target.is_empty() && on_disk(dir, old)
}

/// `trusted`: the files whose old copies were hashed and match (`None`: a preview, which
/// trusts every old copy of the right size).
fn plan_with(manifest: &Manifest, old: Option<&Manifest>, dir: &Path, tags: Option<&[String]>, record: &State, trusted: Option<&HashSet<String>>) -> Result<Plan> {
    let mut p = Plan::default();
    let old_sha: HashMap<&str, [u8; 20]> = old.map(|o| o.files.iter().map(|f| (f.path.as_str(), f.sha1)).collect()).unwrap_or_default();
    let old_files: HashMap<PathBuf, &FileEntry> = old.map(|o| o.files.iter().filter_map(|f| Some((safe_rel(&f.path).ok()?, f))).collect()).unwrap_or_default();
    let wanted = manifest.files_for(tags);
    let mut keys = HashSet::new();
    let mut chunks = HashSet::new();
    for f in &wanted {
        let rel = safe_rel(&f.path)?;
        keys.insert(rel.clone());
        let sha = hex::encode(f.sha1);
        let recorded = record.files.get(&f.path).is_some_and(|s| *s == sha);
        let same_as_old = old_sha.get(f.path.as_str()).is_some_and(|s| *s == f.sha1);
        if (recorded || same_as_old) && on_disk(dir, f) {
            p.keep.push(f.path.clone());
            continue;
        }
        p.write.push(f.path.clone());
        p.write_bytes += f.size;
        if let Ok(m) = std::fs::symlink_metadata(dir.join(&rel)) {
            p.replaced_bytes += m.len().min(f.size);
        }
        let reuse = old_files
            .get(&rel)
            .filter(|o| trusted.is_none_or(|t| t.contains(&f.path)) && old_copy_present(dir, o))
            .and_then(|o| reusable_parts(f, o));
        for (i, part) in f.parts.iter().enumerate() {
            if reuse.as_ref().is_some_and(|r| r[i].is_some()) {
                p.reused_bytes += part.size as u64;
            } else if chunks.insert(part.guid) {
                p.download_bytes += manifest.chunk(&part.guid).map_or(0, |c| c.file_size);
            }
        }
        if let Some(r) = reuse {
            p.reuse.insert(f.path.clone(), r);
        }
    }
    let mut gone: Vec<String> = old.map(|o| o.files.iter().map(|f| f.path.clone()).collect()).unwrap_or_default();
    gone.extend(record.files.keys().cloned());
    let mut seen = HashSet::new();
    for g in gone {
        let Ok(rel) = safe_rel(&g) else { continue };
        if !keys.contains(&rel) && seen.insert(rel) {
            p.remove.push(g);
        }
    }
    Ok(p)
}

/// An update made smaller by Epic's delta manifest for the two builds.
#[derive(Debug, Clone)]
pub struct Delta {
    /// The new build's manifest with the delta's file entries and chunks folded in: the
    /// same files (paths and SHA-1s), cut from chunks that mostly repeat the old build's
    /// data, so more of each changed file is reused from disk.
    pub manifest: Manifest,
    /// Files the delta re-describes.
    pub files: usize,
}

/// A build id as a path segment (Epic's are base64-like), so it can't steer the URL.
fn plain_build_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '+'))
}

/// Epic's delta manifest for updating from `old` to `new`, if it publishes one: fetched
/// from `Deltas/<new build id>/<old build id>.delta` next to the chunks (no session, as
/// legendary's `get_delta_manifest`), parsed with the build's keys and folded in with
/// [`combine_delta`]. Most builds have none; any failure means no delta, never a failed
/// update.
pub async fn delta(source: &dyn ChunkSource, new: &Manifest, old: &Manifest, secrets: &Secrets) -> Option<Delta> {
    let (to, from) = (&new.meta.build_id, &old.meta.build_id);
    if to == from || !plain_build_id(to) || !plain_build_id(from) {
        return None;
    }
    let bytes = match source.fetch(&format!("Deltas/{to}/{from}.delta")).await {
        Ok(b) => b,
        Err(e) => {
            tracing::info!(app = %new.meta.app_name, "no delta manifest {from} → {to} ({e:#}); updating from the full manifest");
            return None;
        }
    };
    let d = match crate::manifest::parse(&bytes, secrets) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(app = %new.meta.app_name, "the delta manifest {from} → {to} doesn't read ({e:#}); updating from the full manifest");
            return None;
        }
    };
    let (manifest, files) = combine_delta(new, old, &d)?;
    tracing::info!(app = %new.meta.app_name, files, "using Epic's delta manifest {from} → {to}");
    Some(Delta { manifest, files })
}

/// `new` with `delta`'s file entries and chunks folded in (legendary's `combine_manifests`,
/// read as a spec), defensively: a delta entry replaces a file's only when the new build
/// has that path with the same SHA-1 (so the result is still the new build, checked file
/// by file as it is written), its parts add up to the file's size and every chunk they cut
/// is listed (by the delta, the new build, or the old one) with the range inside it.
/// Chunks are fetched at paths that depend on the manifest version, so a delta from
/// another chunk folder is not used. `None` when nothing in the delta is usable.
pub fn combine_delta(new: &Manifest, old: &Manifest, delta: &Manifest) -> Option<(Manifest, usize)> {
    let dir = crate::manifest::chunk_dir(new.version);
    if crate::manifest::chunk_dir(delta.version) != dir {
        tracing::info!("the delta manifest is version {} (chunks under another folder than version {}); not used", delta.version, new.version);
        return None;
    }
    let old_chunks = (crate::manifest::chunk_dir(old.version) == dir).then_some(old);
    let lookup = |g: &Guid| -> Option<ChunkInfo> {
        new.chunk(g).or_else(|| delta.chunk(g)).or_else(|| old_chunks.and_then(|o| o.chunk(g))).cloned()
    };
    let mut out = new.clone();
    let by_path: HashMap<String, usize> = out.files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
    let mut known: HashSet<Guid> = out.chunks.iter().map(|c| c.guid).collect();
    let mut used = 0;
    for df in &delta.files {
        let Some(&i) = by_path.get(&df.path) else { continue };
        let nf = &out.files[i];
        if df.sha1 != nf.sha1 || nf.sha1 == [0; 20] || !nf.symlink_target.is_empty() || !df.symlink_target.is_empty() || nf.parts == df.parts {
            continue;
        }
        if df.parts.iter().map(|p| p.size as u64).sum::<u64>() != nf.size {
            continue;
        }
        let Some(infos) = df
            .parts
            .iter()
            .map(|p| lookup(&p.guid).filter(|c| p.offset as u64 + p.size as u64 <= c.window_size as u64))
            .collect::<Option<Vec<ChunkInfo>>>()
        else {
            continue;
        };
        for c in infos {
            if known.insert(c.guid) {
                out.chunks.push(c);
            }
        }
        let mut parts = df.parts.clone();
        let mut at = 0;
        for p in &mut parts {
            p.file_offset = at;
            at += p.size as u64;
        }
        out.files[i].parts = parts;
        used += 1;
    }
    out.reindex();
    (used > 0).then_some((out, used))
}

/// Bytes still to write for an install of `manifest` into `dir`.
pub fn needed_bytes(manifest: &Manifest, old: Option<&Manifest>, dir: &Path, tags: Option<&[String]>) -> Result<u64> {
    plan(manifest, old, dir, tags).map(|p| p.write_bytes)
}

/// Install `job.manifest` into `job.dir`: a fresh install, a resumed one, or an update
/// from `job.old`.
pub async fn install(job: Job<'_>) -> Result<Report> {
    let mut record = load_state(job.dir);
    let fp = fingerprint(job.manifest);
    if record.manifest != fp {
        // Another build's record: what it says is finished counts only where the SHA-1
        // matches (plan_with checks), and it says which files are no longer wanted.
        record.build_version = job.manifest.meta.build_version.clone();
        record.manifest = fp;
    }
    let mut plan = plan_with(job.manifest, job.old, job.dir, job.tags, &record, None)?;
    if !plan.reuse.is_empty()
        && let Some(old) = job.old
    {
        // Read only from old copies that are what the old manifest says: hash them first
        // (a damaged or half-replaced one is downloaded instead).
        let candidates: Vec<(String, PathBuf, [u8; 20])> = plan
            .reuse
            .keys()
            .filter_map(|p| {
                let rel = safe_rel(p).ok()?;
                let o = old.files.iter().find(|o| safe_rel(&o.path).ok().as_ref() == Some(&rel))?;
                (o.sha1 != [0; 20]).then(|| (p.clone(), job.dir.join(&rel), o.sha1))
            })
            .collect();
        let trusted = hash_old_copies(candidates, job.progress).await?;
        plan = plan_with(job.manifest, job.old, job.dir, job.tags, &record, Some(&trusted))?;
    }
    let write: HashSet<String> = plan.write.iter().cloned().collect();
    run(&job, plan, record, |f| write.contains(&f.path)).await
}

/// The paths (of `(path, file, sha1)`) whose file on disk has that SHA-1, hashed on a
/// blocking thread; stops when the progress is cancelled.
async fn hash_old_copies(candidates: Vec<(String, PathBuf, [u8; 20])>, progress: &Progress) -> Result<HashSet<String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let mut task = tokio::task::spawn_blocking(move || -> Result<HashSet<String>> {
        let mut ok = HashSet::new();
        let mut buf = vec![0u8; 1 << 20];
        for (path, file, want) in candidates {
            let Ok(mut f) = std::fs::File::open(&file) else { continue };
            let mut sha = Sha1::new();
            let good = loop {
                if stop2.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                match f.read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => sha.update(&buf[..n]),
                    Err(_) => break false,
                }
            };
            if good && <[u8; 20]>::from(sha.finalize()) == want {
                ok.insert(path);
            } else {
                tracing::info!(file = %file.display(), "the old copy doesn't match the installed build; downloading it instead");
            }
        }
        Ok(ok)
    });
    loop {
        tokio::select! {
            r = &mut task => return r.context("checking old files")?,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if progress.cancelled() {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Rebuild only `bad` (manifest paths, from [`verify`]) in `job.dir`.
pub async fn repair(job: Job<'_>, bad: &[String]) -> Result<Report> {
    let mut record = load_state(job.dir);
    record.manifest = fingerprint(job.manifest);
    record.build_version = job.manifest.meta.build_version.clone();
    let bad: HashSet<&str> = bad.iter().map(String::as_str).collect();
    let mut plan = Plan::default();
    let mut chunks = HashSet::new();
    for f in job.manifest.files_for(job.tags) {
        if bad.contains(f.path.as_str()) {
            safe_rel(&f.path)?;
            record.files.remove(&f.path);
            plan.write.push(f.path.clone());
            plan.write_bytes += f.size;
            for p in &f.parts {
                if chunks.insert(p.guid) {
                    plan.download_bytes += job.manifest.chunk(&p.guid).map_or(0, |c| c.file_size);
                }
            }
        } else {
            plan.keep.push(f.path.clone());
        }
    }
    run(&job, plan, record, |f| bad.contains(f.path.as_str())).await
}

enum Held {
    Mem(Arc<Vec<u8>>),
    Disk(PathBuf),
}

/// Bytes of the files, mirrored into the shared progress from blocking work.
struct Mirror {
    local: Arc<AtomicU64>,
    base: u64,
}

impl Mirror {
    fn sync(&self, into: &AtomicU64) {
        into.store(self.base + self.local.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

async fn run(job: &Job<'_>, plan: Plan, mut record: State, writes: impl Fn(&FileEntry) -> bool) -> Result<Report> {
    let m = job.manifest;
    let progress = job.progress;
    if progress.cancelled() {
        return Err(Cancelled.into());
    }
    let files: Vec<&FileEntry> = m.files_for(job.tags).into_iter().filter(|f| writes(f)).collect();
    // Parts read from old copies, by index into `files`.
    let reuse: HashMap<usize, Vec<Option<u64>>> =
        files.iter().enumerate().filter_map(|(i, f)| plan.reuse.get(&f.path).filter(|r| r.len() == f.parts.len()).map(|r| (i, r.clone()))).collect();
    // Chunks in the order files first use them, and how many parts use each.
    let mut order: Vec<Guid> = Vec::new();
    let mut refs: HashMap<Guid, usize> = HashMap::new();
    for (fi, f) in files.iter().enumerate() {
        for (pi, p) in f.parts.iter().enumerate() {
            if reuse.get(&fi).is_some_and(|r| r[pi].is_some()) {
                continue;
            }
            let n = refs.entry(p.guid).or_insert(0);
            if *n == 0 {
                ensure!(m.chunk(&p.guid).is_some(), "the manifest lists no chunk {} (used by {})", guid_hex(&p.guid), f.path);
                order.push(p.guid);
            }
            *n += 1;
        }
    }
    progress.total.store(plan.download_bytes, Ordering::Relaxed);
    progress.downloaded.store(0, Ordering::Relaxed);
    progress.written.store(0, Ordering::Relaxed);
    progress.files_done.store(plan.keep.len() as u64, Ordering::Relaxed);
    let need = plan.write_bytes.saturating_sub(plan.replaced_bytes);
    if need > 0 {
        quiver_core::install::check_space(job.dir, need)?;
    }
    std::fs::create_dir_all(job.dir).with_context(|| format!("create {}", job.dir.display()))?;
    for f in &files {
        record.files.remove(&f.path);
    }
    // The record keeps what stays and what is still to be removed (so a crash before the
    // removal still knows those files).
    let keep: HashSet<&str> = plan.keep.iter().map(String::as_str).collect();
    let gone: HashSet<&str> = plan.remove.iter().map(String::as_str).collect();
    record.files.retain(|p, _| keep.contains(p.as_str()) || gone.contains(p.as_str()));
    for f in m.files_for(job.tags) {
        if keep.contains(f.path.as_str()) {
            record.files.insert(f.path.clone(), hex::encode(f.sha1));
        }
    }
    save_state(job.dir, &record)?;

    let secrets = Arc::new(job.secrets.clone());
    let downloaded = Arc::new(AtomicU64::new(0));
    let result = fetch_and_build(job, &files, reuse, order, refs, &secrets, &downloaded, &mut record).await;
    let _ = std::fs::remove_dir_all(spill_dir(job.dir));
    let saved = save_state(job.dir, &record);
    let written = result?;
    saved?;
    // Files the new build doesn't have.
    let mut removed = 0;
    let mut parents = Vec::new();
    for p in &plan.remove {
        let Ok(rel) = safe_rel(p) else { continue };
        let path = job.dir.join(&rel);
        if std::fs::symlink_metadata(&path).is_ok() && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
        record.files.remove(p);
        parents.extend(rel.parent().map(Path::to_path_buf));
    }
    remove_empty_dirs(job.dir, parents);
    save_state(job.dir, &record)?;
    Ok(Report {
        files_written: written,
        files_kept: plan.keep.len(),
        files_removed: removed,
        bytes_downloaded: downloaded.load(Ordering::Relaxed),
        bytes_reused: plan.reused_bytes,
    })
}

/// Remove `dirs` (relative) and their parents while empty, never `root` itself.
fn remove_empty_dirs(root: &Path, dirs: Vec<PathBuf>) {
    let mut all: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let mut cur = Some(d.as_path());
        while let Some(c) = cur.filter(|c| !c.as_os_str().is_empty()) {
            all.push(c.to_path_buf());
            cur = c.parent();
        }
    }
    all.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    all.dedup();
    for d in all {
        let _ = std::fs::remove_dir(root.join(d)); // fails unless empty
    }
}

/// Download the chunks in `order` and hand them to a [`Builder`], which writes `files` as
/// their chunks arrive. Finished files go into `record` (saved every few seconds), also
/// when the install stops early. Returns the files written.
#[allow(clippy::too_many_arguments)]
async fn fetch_and_build(
    job: &Job<'_>,
    files: &[&FileEntry],
    reuse: HashMap<usize, Vec<Option<u64>>>,
    order: Vec<Guid>,
    refs: HashMap<Guid, usize>,
    secrets: &Arc<Secrets>,
    downloaded: &Arc<AtomicU64>,
    record: &mut State,
) -> Result<usize> {
    let m = job.manifest;
    let progress = job.progress;
    let mut chunks = futures::stream::iter(order.into_iter().map(|g| {
        let info = m.chunk(&g).cloned().expect("checked");
        let secrets = secrets.clone();
        let downloaded = downloaded.clone();
        async move {
            let data = fetch_chunk(job, &info, &secrets, &downloaded).await?;
            Ok::<_, anyhow::Error>((g, data))
        }
    }))
    .buffered(job.workers.max(1));
    let written = Mirror { local: Arc::new(AtomicU64::new(0)), base: 0 };
    let builder = Builder {
        dir: job.dir.to_path_buf(),
        files: files.iter().map(|f| (*f).clone()).collect(),
        reuse,
        old: None,
        stop: Arc::new(AtomicBool::new(false)),
        refs,
        held: HashMap::new(),
        in_memory: 0,
        next: 0,
        open: None,
        last_disk: None,
        written: written.local.clone(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<(Guid, Arc<Vec<u8>>)>(8);
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let stop = builder.stop.clone();
    let mut writer = tokio::task::spawn_blocking(move || builder.run(rx, done_tx));
    let mut tx = Some(tx);
    let mut last_save = Instant::now();
    let mut failed: Option<anyhow::Error> = None;
    let note = |i: usize, record: &mut State| {
        let f = files[i];
        record.files.insert(f.path.clone(), hex::encode(f.sha1));
        progress.files_done.fetch_add(1, Ordering::Relaxed);
    };
    loop {
        if progress.cancelled() && failed.is_none() {
            failed = Some(Cancelled.into());
        }
        if failed.is_some() {
            stop.store(true, Ordering::Relaxed);
            tx = None;
        }
        tokio::select! {
            c = chunks.next(), if tx.is_some() => match c {
                None => tx = None,
                Some(Err(e)) => failed = Some(e),
                Some(Ok(c)) => {
                    // A closed channel means the writer stopped; its result says why.
                    if tx.as_ref().expect("guarded").send(c).await.is_err() {
                        tx = None;
                    }
                }
            },
            d = done_rx.recv() => match d {
                Some(i) => {
                    note(i, record);
                    written.sync(&progress.written);
                    if last_save.elapsed() > Duration::from_secs(2) {
                        save_state(job.dir, record)?;
                        last_save = Instant::now();
                    }
                }
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => written.sync(&progress.written),
        }
    }
    let r = (&mut writer).await.context("writing files")?;
    while let Ok(i) = done_rx.try_recv() {
        note(i, record);
    }
    written.sync(&progress.written);
    match failed {
        Some(e) => Err(e),
        None => r,
    }
}
/// One chunk, downloaded, decoded and checked, tried again with backoff.
async fn fetch_chunk(job: &Job<'_>, info: &ChunkInfo, secrets: &Arc<Secrets>, downloaded: &AtomicU64) -> Result<Arc<Vec<u8>>> {
    let progress = job.progress;
    let path = info.path(job.manifest.version);
    let mut attempt = 0u32;
    loop {
        if progress.cancelled() {
            return Err(Cancelled.into());
        }
        if let Some(t) = &progress.throttle {
            t.take(info.file_size.max(1)).await;
        }
        let tried: Result<(Vec<u8>, u64)> = async {
            let raw = job.source.fetch(&path).await?;
            let len = raw.len() as u64;
            let (info, secrets) = (info.clone(), secrets.clone());
            let data = tokio::task::spawn_blocking(move || crate::chunk::decode(&raw, Some(&info), &secrets)).await??;
            Ok((data, len))
        }
        .await;
        match tried {
            Ok((data, len)) => {
                progress.downloaded.fetch_add(len, Ordering::Relaxed);
                progress.received.fetch_add(len, Ordering::Relaxed);
                downloaded.fetch_add(len, Ordering::Relaxed);
                return Ok(Arc::new(data));
            }
            Err(e) => {
                attempt += 1;
                if attempt >= progress.retry.attempts.max(1) {
                    return Err(e.context(format!("downloading chunk {} ({attempt} tries)", guid_hex(&info.guid))));
                }
                tracing::info!(chunk = guid_hex(&info.guid), attempt, "Epic chunk failed, trying again: {e:#}");
                progress.retries.fetch_add(1, Ordering::Relaxed);
                progress.retrying.fetch_add(1, Ordering::Relaxed);
                let until = Instant::now() + progress.retry.delay(attempt);
                while Instant::now() < until && !progress.cancelled() {
                    tokio::time::sleep((until - Instant::now()).min(Duration::from_millis(100))).await;
                }
                progress.retrying.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// The writing side of an install, on a blocking thread of its own: it takes chunks as
/// they arrive (in the order files first use them) and writes files progressively, part
/// by part, so even a file of many GB needs only the chunks not yet written into it.
struct Builder {
    dir: PathBuf,
    files: Vec<FileEntry>,
    /// Parts of `files` (by index) read from the file's old copy: where each starts in it.
    reuse: HashMap<usize, Vec<Option<u64>>>,
    /// The old copy of the file being written, while parts are read from it.
    old: Option<(usize, std::fs::File)>,
    /// Set when the install stops (cancel or a failed download).
    stop: Arc<AtomicBool>,
    /// Parts still to be written from each chunk.
    refs: HashMap<Guid, usize>,
    held: HashMap<Guid, Held>,
    in_memory: usize,
    next: usize,
    open: Option<Open>,
    /// The last chunk read back from disk, for the next parts cut from it.
    last_disk: Option<(Guid, Arc<Vec<u8>>)>,
    written: Arc<AtomicU64>,
}

struct Open {
    part: PathBuf,
    out: std::io::BufWriter<std::fs::File>,
    sha: Sha1,
    idx: usize,
}

impl Builder {
    fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<(Guid, Arc<Vec<u8>>)>, done: tokio::sync::mpsc::UnboundedSender<usize>) -> Result<usize> {
        let stop = self.stop.clone();
        let r = (|| {
            self.advance(&done)?;
            while let Some((g, data)) = rx.blocking_recv() {
                if stop.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                self.hold(g, data)?;
                self.advance(&done)?;
            }
            if stop.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            ensure!(self.next >= self.files.len(), "chunks for {} never arrived", self.files[self.next].path);
            Ok(self.next)
        })();
        if r.is_err()
            && let Some(o) = self.open.take()
        {
            drop(o.out);
            let _ = std::fs::remove_file(&o.part);
        }
        r
    }

    fn hold(&mut self, g: Guid, data: Arc<Vec<u8>>) -> Result<()> {
        if self.in_memory + data.len() > BUDGET.load(Ordering::Relaxed) {
            let dir = spill_dir(&self.dir);
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(guid_hex(&g));
            std::fs::write(&path, &*data).with_context(|| format!("write {}", path.display()))?;
            self.held.insert(g, Held::Disk(path));
        } else {
            self.in_memory += data.len();
            self.held.insert(g, Held::Mem(data));
        }
        Ok(())
    }

    fn chunk(&mut self, g: &Guid) -> Result<Option<Arc<Vec<u8>>>> {
        match self.held.get(g) {
            None => Ok(None),
            Some(Held::Mem(a)) => Ok(Some(a.clone())),
            Some(Held::Disk(p)) => {
                if let Some((lg, a)) = &self.last_disk
                    && lg == g
                {
                    return Ok(Some(a.clone()));
                }
                let a = Arc::new(std::fs::read(p).with_context(|| format!("read {}", p.display()))?);
                self.last_disk = Some((*g, a.clone()));
                Ok(Some(a))
            }
        }
    }

    /// A part of `g` was written: let go of the chunk once nothing else needs it.
    fn used(&mut self, g: &Guid) {
        let Some(n) = self.refs.get_mut(g) else { return };
        *n -= 1;
        if *n > 0 {
            return;
        }
        match self.held.remove(g) {
            Some(Held::Mem(a)) => self.in_memory -= a.len(),
            Some(Held::Disk(p)) => {
                let _ = std::fs::remove_file(p);
                if self.last_disk.as_ref().is_some_and(|(lg, _)| lg == g) {
                    self.last_disk = None;
                }
            }
            None => {}
        }
    }

    /// Write whatever the chunks held so far allow; report each finished file.
    fn advance(&mut self, done: &tokio::sync::mpsc::UnboundedSender<usize>) -> Result<()> {
        while self.next < self.files.len() {
            let f = self.files[self.next].clone();
            let rel = safe_rel(&f.path)?;
            let dest = self.dir.join(&rel);
            if self.open.is_none() {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
                }
                if !f.symlink_target.is_empty() {
                    make_symlink(&rel, &dest, &f)?;
                    let _ = done.send(self.next);
                    self.next += 1;
                    continue;
                }
                let mut name = dest.file_name().context("file name")?.to_os_string();
                name.push(".quiver-part");
                let part = dest.with_file_name(name);
                let file = std::fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;
                self.open = Some(Open { part, out: std::io::BufWriter::with_capacity(1 << 20, file), sha: Sha1::new(), idx: 0 });
            }
            loop {
                let idx = self.open.as_ref().expect("open").idx;
                let Some(p) = f.parts.get(idx) else { break };
                if let Some(at) = self.reuse.get(&self.next).and_then(|r| r[idx]) {
                    // The same bytes from the old copy, which stays in place until this
                    // file is renamed over it.
                    if self.stop.load(Ordering::Relaxed) {
                        return Err(Cancelled.into());
                    }
                    if self.old.as_ref().is_none_or(|(i, _)| *i != self.next) {
                        let file = std::fs::File::open(&dest).with_context(|| format!("open the old {}", dest.display()))?;
                        self.old = Some((self.next, file));
                    }
                    let mut buf = vec![0u8; p.size as usize];
                    use std::os::unix::fs::FileExt;
                    self.old.as_ref().expect("opened").1.read_exact_at(&mut buf, at).with_context(|| format!("read the old {}", dest.display()))?;
                    let o = self.open.as_mut().expect("open");
                    o.out.write_all(&buf)?;
                    o.sha.update(&buf);
                    o.idx += 1;
                    self.written.fetch_add(p.size as u64, Ordering::Relaxed);
                    continue;
                }
                let Some(data) = self.chunk(&p.guid)? else { return Ok(()) };
                let (s, e) = (p.offset as usize, p.offset as usize + p.size as usize);
                ensure!(e <= data.len(), "{}: part {s}..{e} is outside chunk {} ({} bytes)", f.path, guid_hex(&p.guid), data.len());
                let o = self.open.as_mut().expect("open");
                o.out.write_all(&data[s..e])?;
                o.sha.update(&data[s..e]);
                o.idx += 1;
                self.written.fetch_add(p.size as u64, Ordering::Relaxed);
                self.used(&p.guid);
            }
            let o = self.open.take().expect("open");
            // Let go of the old copy before the new file replaces it.
            self.old = None;
            finish_file(o, &dest, &f)?;
            let _ = done.send(self.next);
            self.next += 1;
        }
        Ok(())
    }
}

fn make_symlink(rel: &Path, dest: &Path, f: &FileEntry) -> Result<()> {
    let target = PathBuf::from(f.symlink_target.replace('\\', "/"));
    // The link must stay inside the install folder.
    let mut depth = rel.components().count() as i64 - 1;
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth -= 1,
            Component::CurDir => {}
            _ => bail!("unsafe symlink in manifest: {} -> {}", f.path, f.symlink_target),
        }
        ensure!(depth >= 0, "unsafe symlink in manifest: {} -> {}", f.path, f.symlink_target);
    }
    let _ = std::fs::remove_file(dest);
    std::os::unix::fs::symlink(&target, dest).with_context(|| format!("link {}", dest.display()))
}

fn finish_file(o: Open, dest: &Path, f: &FileEntry) -> Result<()> {
    let Open { part, out, sha, .. } = o;
    let r = (|| -> Result<()> {
        out.into_inner().map_err(|e| e.into_error())?.sync_data().ok();
        let got: [u8; 20] = sha.finalize().into();
        ensure!(f.sha1 == [0; 20] || got == f.sha1, "{} doesn't match its SHA-1 after writing", f.path);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(if f.executable() { 0o755 } else { 0o644 }))?;
        ensure!(!std::fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir()), "{} is a folder on disk", dest.display());
        std::fs::rename(&part, dest).with_context(|| format!("rename into {}", dest.display()))?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    r
}
/// Check every file against its SHA-1. Missing and mismatching files are listed by
/// manifest path.
pub async fn verify(manifest: &Manifest, dir: &Path, tags: Option<&[String]>, progress: &Progress) -> Result<VerifyReport> {
    let files: Vec<FileEntry> = manifest.files_for(tags).into_iter().cloned().collect();
    progress.hash_total.store(files.iter().map(|f| f.size).sum(), Ordering::Relaxed);
    progress.hashed.store(0, Ordering::Relaxed);
    let hashed = Mirror { local: Arc::new(AtomicU64::new(0)), base: 0 };
    let cancel = Arc::new(AtomicBool::new(false));
    let (dir, counter, stop) = (dir.to_path_buf(), hashed.local.clone(), cancel.clone());
    let mut task = tokio::task::spawn_blocking(move || verify_blocking(&files, &dir, &counter, &stop));
    loop {
        tokio::select! {
            r = &mut task => {
                hashed.sync(&progress.hashed);
                let r = r.context("verifying")?;
                if progress.cancelled() {
                    return Err(Cancelled.into());
                }
                return r;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                hashed.sync(&progress.hashed);
                if progress.cancelled() {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

fn verify_blocking(files: &[FileEntry], dir: &Path, hashed: &AtomicU64, stop: &AtomicBool) -> Result<VerifyReport> {
    let mut r = VerifyReport::default();
    let mut buf = vec![0u8; 1 << 20];
    for f in files {
        if stop.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let Ok(rel) = safe_rel(&f.path) else {
            r.bad.push(f.path.clone());
            continue;
        };
        let p = dir.join(rel);
        if !f.symlink_target.is_empty() {
            if on_disk(dir, f) {
                r.ok += 1;
            } else {
                r.missing.push(f.path.clone());
            }
            continue;
        }
        let Ok(mut file) = std::fs::File::open(&p) else {
            r.missing.push(f.path.clone());
            continue;
        };
        let mut sha = Sha1::new();
        let mut len = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
            len += n as u64;
            hashed.fetch_add(n as u64, Ordering::Relaxed);
        }
        let got: [u8; 20] = sha.finalize().into();
        if len == f.size && (f.sha1 == [0; 20] || got == f.sha1) {
            r.ok += 1;
        } else {
            r.bad.push(f.path.clone());
        }
    }
    Ok(r)
}

/// Remove the manifest's files from `dir` (and folders left empty, and Quiver's resume
/// record); nothing else. Returns how many files were removed.
pub fn uninstall(manifest: &Manifest, dir: &Path) -> Result<usize> {
    let mut n = 0;
    let mut parents = Vec::new();
    for f in &manifest.files {
        let Ok(rel) = safe_rel(&f.path) else { continue };
        let p = dir.join(&rel);
        let mut part_name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        part_name.push(".quiver-part");
        let _ = std::fs::remove_file(p.with_file_name(part_name));
        if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.is_dir()) {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
            n += 1;
        }
        parents.extend(rel.parent().map(Path::to_path_buf));
    }
    let _ = std::fs::remove_file(state_path(dir));
    let _ = std::fs::remove_dir_all(spill_dir(dir));
    let _ = std::fs::remove_dir(state_dir(dir));
    remove_empty_dirs(dir, parents);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests::build;
    use std::sync::Mutex;

    /// Chunks from memory, counting fetches; paths in `fail_once` fail the first time.
    #[derive(Default)]
    struct Mem {
        chunks: HashMap<String, Vec<u8>>,
        fetched: Mutex<Vec<String>>,
        fail_once: Mutex<HashSet<String>>,
        /// Cancel this progress after this many fetches.
        cancel_after: Option<(usize, Arc<Progress>)>,
    }

    impl ChunkSource for Mem {
        fn fetch<'a>(&'a self, rel: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                let n = {
                    let mut f = self.fetched.lock().unwrap();
                    f.push(rel.to_owned());
                    f.len()
                };
                if let Some((k, p)) = &self.cancel_after
                    && n >= *k
                {
                    p.cancel.store(true, Ordering::Relaxed);
                }
                if self.fail_once.lock().unwrap().remove(rel) {
                    bail!("flaky");
                }
                self.chunks.get(rel).cloned().with_context(|| format!("no chunk {rel}"))
            })
        }
    }

    fn quick() -> Progress {
        let p = Progress::default();
        // Progress::default() has the default policy; make retries instant for tests.
        let mut retry = p.retry;
        retry.base_delay = Duration::from_millis(1);
        retry.max_delay = Duration::from_millis(2);
        Progress { retry, ..p }
    }

    fn files_v1() -> Vec<(&'static str, Vec<u8>, &'static [&'static str])> {
        vec![
            ("Bin/Game.exe", (0..3000u32).map(|i| (i * 7 % 256) as u8).collect(), &[]),
            ("Data\\a.pak", vec![1u8; 5000], &[]),
            ("Data/b.pak", (0..2500u32).map(|i| (i % 13) as u8).collect(), &[]),
            ("Lang/fr.pak", vec![9u8; 700], &["fr"]),
            ("empty.ini", Vec::new(), &[]),
        ]
    }

    fn fixture(files: &[(&'static str, Vec<u8>, &'static [&'static str])], version: u32) -> (Manifest, Mem) {
        let refs: Vec<(&str, &[u8], &[&str])> = files.iter().map(|(p, d, t)| (*p, d.as_slice(), *t)).collect();
        let (m, chunks) = build(version, &refs, 1024, None);
        (m, Mem { chunks, ..Default::default() })
    }

    fn job<'a>(m: &'a Manifest, old: Option<&'a Manifest>, dir: &'a Path, src: &'a Mem, p: &'a Progress) -> Job<'a> {
        static NO: std::sync::LazyLock<Secrets> = std::sync::LazyLock::new(Secrets::new);
        Job { manifest: m, old, dir, tags: None, secrets: &NO, source: src, progress: p, workers: 3 }
    }

    fn read(dir: &Path, p: &str) -> Vec<u8> {
        std::fs::read(dir.join(safe_rel(p).unwrap())).unwrap()
    }

    #[tokio::test]
    async fn installs_verifies_repairs_and_uninstalls() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Game");
        let files = files_v1();
        let (m, src) = fixture(&files, 21);
        src.fail_once.lock().unwrap().insert(m.chunks[1].path(21));
        let p = quick();
        let r = install(job(&m, None, &dir, &src, &p)).await.unwrap();
        assert_eq!((r.files_written, r.files_kept, r.files_removed), (5, 0, 0));
        for (path, data, _) in &files {
            assert_eq!(&read(&dir, path), data, "{path}");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("Bin/Game.exe")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(p.retries.load(Ordering::Relaxed), 1);
        assert_eq!(p.downloaded.load(Ordering::Relaxed), m.download_size(None));
        assert_eq!(r.bytes_downloaded, p.downloaded.load(Ordering::Relaxed));
        // Each chunk once (plus the one retry).
        assert_eq!(src.fetched.lock().unwrap().len(), m.chunks.len() + 1);
        // Installing again does nothing.
        let r = install(job(&m, None, &dir, &src, &p)).await.unwrap();
        assert_eq!((r.files_written, r.files_kept), (0, 5));

        // Verify finds a damaged and a missing file; repair fixes exactly those.
        std::fs::write(dir.join("Data/b.pak"), b"garbage").unwrap();
        std::fs::remove_file(dir.join("Lang/fr.pak")).unwrap();
        std::fs::write(dir.join("mine.sav"), b"not Epic's").unwrap();
        let v = verify(&m, &dir, None, &p).await.unwrap();
        assert_eq!((v.ok, v.bad.clone(), v.missing.clone()), (3, vec!["Data/b.pak".to_owned()], vec!["Lang/fr.pak".to_owned()]));
        assert_eq!(p.hash_total.load(Ordering::Relaxed), m.install_size(None));
        let bad: Vec<String> = v.bad.iter().chain(&v.missing).cloned().collect();
        let r = repair(job(&m, None, &dir, &src, &p), &bad).await.unwrap();
        assert_eq!((r.files_written, r.files_kept), (2, 3));
        let v = verify(&m, &dir, None, &p).await.unwrap();
        assert_eq!((v.ok, v.bad.len(), v.missing.len()), (5, 0, 0));

        // Uninstall leaves the player's file and the folder.
        assert_eq!(uninstall(&m, &dir).unwrap(), 5);
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["mine.sav"]);
    }

    #[tokio::test]
    async fn cancel_then_resume_skips_finished_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let files = files_v1();
        let (m, mut src) = fixture(&files, 18);
        let p = Arc::new(quick());
        src.cancel_after = Some((5, p.clone()));
        let e = install(Job { workers: 1, ..job(&m, None, &dir, &src, &p) }).await.unwrap_err();
        assert!(quiver_core::install::is_cancelled(&e), "{e:#}");
        let first: Vec<String> = src.fetched.lock().unwrap().clone();
        let state = load_state(&dir);
        assert!(!state.files.is_empty() && state.files.len() < 5, "{:?}", state.files);
        // Resume: finished files aren't rebuilt, and their own chunks aren't fetched again.
        src.cancel_after = None;
        src.fetched.lock().unwrap().clear();
        let p = quick();
        let r = install(job(&m, None, &dir, &src, &p)).await.unwrap();
        assert_eq!(r.files_kept, state.files.len());
        assert_eq!(r.files_kept + r.files_written, 5);
        let again = src.fetched.lock().unwrap().clone();
        assert!(again.len() < m.chunks.len(), "refetched {} of {} (first run {})", again.len(), m.chunks.len(), first.len());
        for (path, data, _) in &files {
            assert_eq!(&read(&dir, path), data);
        }
        assert!(!spill_dir(&dir).exists());
    }

    #[tokio::test]
    async fn updates_keep_change_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let v1 = files_v1();
        let (m1, src1) = fixture(&v1, 22);
        let p = quick();
        install(job(&m1, None, &dir, &src1, &p)).await.unwrap();
        // v2: the exe changes, b.pak goes, c.pak arrives, the rest stay.
        let mut v2 = files_v1();
        v2[0].1 = b"MZ new build".to_vec();
        v2.remove(2);
        v2.push(("Data/c.pak", vec![3u8; 4000], &[]));
        let (mut m2, src2) = fixture(&v2, 22);
        m2.meta.build_version = "2.0".into();
        // The record is the authority even without the old manifest...
        let r = install(job(&m2, None, &dir, &src2, &p)).await.unwrap();
        assert_eq!((r.files_written, r.files_kept, r.files_removed), (2, 3, 1));
        assert!(!dir.join("Data/b.pak").exists());
        for (path, data, _) in &v2 {
            assert_eq!(&read(&dir, path), data);
        }
        // ...and the old manifest is enough without the record.
        std::fs::remove_file(state_path(&dir)).unwrap();
        let (m3, src3) = fixture(&v1, 22);
        let r = install(job(&m3, Some(&m2), &dir, &src3, &p)).await.unwrap();
        assert_eq!((r.files_written, r.files_kept, r.files_removed), (2, 3, 1));
        assert!(!dir.join("Data/c.pak").exists());
    }

    #[tokio::test]
    async fn install_tags_and_hostile_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let (m, src) = fixture(&files_v1(), 21);
        let p = quick();
        let tags: Vec<String> = Vec::new();
        let r = install(Job { tags: Some(&tags), ..job(&m, None, &dir, &src, &p) }).await.unwrap();
        assert_eq!(r.files_written, 4);
        assert!(!dir.join("Lang/fr.pak").exists());
        // Adding the language later writes just that file.
        let fr = vec!["fr".to_owned()];
        let r = install(Job { tags: Some(&fr), ..job(&m, None, &dir, &src, &p) }).await.unwrap();
        assert_eq!((r.files_written, r.files_kept), (1, 4));
        for bad in ["../x", "/etc/passwd", "a/../../x", "C:/x", "a\0b", ""] {
            assert!(safe_rel(bad).is_err(), "{bad:?}");
        }
        assert_eq!(safe_rel("Engine\\Binaries\\./x.dll").unwrap(), PathBuf::from("Engine/Binaries/x.dll"));
        let (mut evil, src) = fixture(&[("../escape.txt", b"x".to_vec(), &[])], 21);
        evil.files[0].path = "../escape.txt".into();
        assert!(install(job(&evil, None, &dir, &src, &p)).await.is_err());
        assert!(!tmp.path().join("escape.txt").exists());
    }

    /// Past the memory budget, chunks wait on disk and are deleted once used.
    #[tokio::test]
    async fn chunks_spill_to_disk_past_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        // One big file over many chunks, then a small one sharing nothing.
        let big: Vec<u8> = (0..40_000u32).map(|i| (i % 241) as u8).collect();
        let files = vec![("big.pak", big.clone(), &[][..]), ("small.txt", b"tail".to_vec(), &[][..])];
        let (m, src) = fixture(&files, 21);
        BUDGET.store(3000, Ordering::Relaxed);
        let p = quick();
        let r = install(Job { workers: 8, ..job(&m, None, &dir, &src, &p) }).await;
        BUDGET.store(MEMORY_BUDGET, Ordering::Relaxed);
        assert_eq!(r.unwrap().files_written, 2);
        assert_eq!(read(&dir, "big.pak"), big);
        assert_eq!(p.written.load(Ordering::Relaxed), big.len() as u64 + 4);
        assert!(!spill_dir(&dir).exists());
    }

    #[tokio::test]
    async fn damaged_chunks_fail_after_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let (m, mut src) = fixture(&files_v1(), 21);
        let path = m.chunks[0].path(21);
        let raw = src.chunks.get_mut(&path).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 0x55;
        let p = quick();
        let e = install(job(&m, None, &dir, &src, &p)).await.unwrap_err();
        assert!(format!("{e:#}").contains("tries"), "{e:#}");
        assert_eq!(p.retries.load(Ordering::Relaxed), (p.retry.attempts - 1) as u64);
    }

    /// Chunks over HTTP: two base URLs, the first refusing.
    #[tokio::test]
    async fn http_chunks_fall_back_to_the_next_base() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (m, src) = fixture(&files_v1(), 22);
        let chunks = Arc::new(src.chunks.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { break };
                let chunks = chunks.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let body = path.strip_prefix("/good/").and_then(|p| chunks.get(p));
                    let resp = match body {
                        Some(b) => {
                            let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len()).into_bytes();
                            r.extend_from_slice(b);
                            r
                        }
                        None => b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = s.write_all(&resp).await;
                });
            }
        });
        let http = HttpChunks::new(vec![format!("http://{addr}/bad?sig=secret"), format!("http://{addr}/good")], "test").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let p = quick();
        let r = install(Job { manifest: &m, old: None, dir: &dir, tags: None, secrets: &Secrets::new(), source: &http, progress: &p, workers: 4 }).await.unwrap();
        assert_eq!(r.files_written, 5);
        let e = http.fetch("ChunksV5/nope.chunk").await.unwrap_err().to_string();
        assert!(e.contains("HTTP 403") && !e.contains("sig=secret"), "{e}");
    }

    #[tokio::test]
    async fn encrypted_chunks_need_the_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let secret = [7, 7, 7, 7];
        let key = [0x11u8; 32];
        let data = vec![4u8; 3000];
        let (m, chunks) = build(24, &[("x.bin", &data, &[])], 1024, Some((secret, key)));
        assert!(m.chunks[0].path(24).contains("/") && !m.chunks[0].path(24).contains("plain"));
        let src = Mem { chunks, ..Default::default() };
        let mut p = quick();
        p.retry.attempts = 1;
        assert!(install(job(&m, None, &dir, &src, &p)).await.is_err());
        let secrets = Secrets::from([(guid_hex(&secret), hex::encode(key))]);
        let r = install(Job { secrets: &secrets, ..job(&m, None, &dir, &src, &p) }).await.unwrap();
        assert_eq!(r.files_written, 1);
        assert_eq!(read(&dir, "x.bin"), data);
    }

    fn pattern(seed: u32, len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(2654435761).wrapping_add(seed * 7919) >> 13) as u8).collect()
    }

    type Build = (Manifest, Mem);
    type Files = Vec<(&'static str, Vec<u8>, &'static [&'static str])>;

    /// v1 and v2 of a game whose big file changes in one window, and whose small file
    /// changes completely.
    fn two_builds() -> (Build, Build, Files) {
        let big = pattern(1, 20 * 1024);
        let v1 = vec![("Data/big.pak", big.clone(), &[][..]), ("Data/small.txt", b"small v1".to_vec(), &[][..])];
        let mut big2 = big;
        big2[5000..5010].copy_from_slice(b"PATCHED!!!");
        let v2 = vec![("Data/big.pak", big2, &[][..]), ("Data/small.txt", b"small v2".to_vec(), &[][..])];
        let (mut m1, s1) = fixture(&v1, 22);
        let (mut m2, s2) = fixture(&v2, 22);
        (m1.meta.build_version, m1.meta.build_id) = ("1.0".into(), "b1".into());
        (m2.meta.build_version, m2.meta.build_id) = ("1.1".into(), "b2".into());
        ((m1, s1), (m2, s2), v2)
    }

    #[tokio::test]
    async fn updates_read_unchanged_parts_from_the_old_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let ((m1, s1), (m2, s2), v2) = two_builds();
        let p = quick();
        install(job(&m1, None, &dir, &s1, &p)).await.unwrap();
        // The preview counts only the changed windows: the big file's patched one and the
        // last one, which it shares with the small file.
        let plan = plan(&m2, Some(&m1), &dir, None).unwrap();
        assert_eq!(plan.write.len(), 2);
        assert!(plan.reused_bytes >= 18 * 1024 && plan.download_bytes < m2.download_size(None) / 4, "{plan:?}");
        let r = install(job(&m2, Some(&m1), &dir, &s2, &p)).await.unwrap();
        assert_eq!((r.files_written, r.bytes_reused), (2, plan.reused_bytes));
        assert_eq!(r.bytes_downloaded, plan.download_bytes);
        let fetched = s2.fetched.lock().unwrap().len();
        assert!(fetched <= 3 && fetched < m2.chunks.len(), "fetched {fetched} of {}", m2.chunks.len());
        for (path, data, _) in &v2 {
            assert_eq!(&read(&dir, path), data, "{path}");
        }
        assert!(verify(&m2, &dir, None, &p).await.unwrap().bad.is_empty());
    }

    #[tokio::test]
    async fn a_damaged_old_copy_is_downloaded_instead() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let ((m1, s1), (m2, s2), v2) = two_builds();
        let p = quick();
        install(job(&m1, None, &dir, &s1, &p)).await.unwrap();
        // Same size, different bytes where the new build expects the old data.
        let mut big = read(&dir, "Data/big.pak");
        big[100] ^= 0xFF;
        std::fs::write(dir.join("Data/big.pak"), &big).unwrap();
        // A preview can't tell (it doesn't hash); the install does.
        assert!(plan(&m2, Some(&m1), &dir, None).unwrap().reused_bytes > 0);
        let r = install(job(&m2, Some(&m1), &dir, &s2, &p)).await.unwrap();
        assert_eq!(r.bytes_reused, 0);
        assert_eq!(s2.fetched.lock().unwrap().len(), m2.chunks.len());
        for (path, data, _) in &v2 {
            assert_eq!(&read(&dir, path), data, "{path}");
        }
    }

    #[tokio::test]
    async fn an_interrupted_update_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let ((m1, s1), (m2, mut s2), v2) = two_builds();
        let p = quick();
        install(job(&m1, None, &dir, &s1, &p)).await.unwrap();
        let stop = Arc::new(quick());
        s2.cancel_after = Some((1, stop.clone()));
        let e = install(Job { workers: 1, ..job(&m2, Some(&m1), &dir, &s2, &stop) }).await.unwrap_err();
        assert!(quiver_core::install::is_cancelled(&e), "{e:#}");
        assert!(!dir.join("Data/big.pak.quiver-part").exists());
        s2.cancel_after = None;
        let r = install(job(&m2, Some(&m1), &dir, &s2, &p)).await.unwrap();
        assert_eq!(r.files_written + r.files_kept, 2);
        for (path, data, _) in &v2 {
            assert_eq!(&read(&dir, path), data, "{path}");
        }
        // Once done, nothing is left to do.
        let again = plan(&m2, Some(&m1), &dir, None).unwrap();
        assert!(again.write.is_empty() && again.download_bytes == 0, "{again:?}");
    }

    /// A v2 whose bytes are all shifted (100 new bytes in front), so its full manifest
    /// shares no chunk with v1; and Epic's delta, which cuts the same file from v1's chunks
    /// plus one small new chunk. Returns v1, v2, the delta, its chunks and v2's file.
    fn shifted() -> (Build, Build, Manifest, HashMap<String, Vec<u8>>, Vec<u8>) {
        let old = pattern(3, 8 * 1024);
        let head = pattern(9, 100);
        let new: Vec<u8> = head.iter().chain(&old).copied().collect();
        let (mut m1, s1) = fixture(&[("big.pak", old, &[])], 22);
        let (mut m2, s2) = fixture(&[("big.pak", new.clone(), &[])], 22);
        (m1.meta.build_version, m1.meta.build_id) = ("1.0".into(), "b1".into());
        (m2.meta.build_version, m2.meta.build_id) = ("1.1".into(), "b2".into());
        let (mut d, dchunks) = build(22, &[("big.pak", &head, &[])], 1024, None);
        let f = &mut d.files[0];
        f.sha1 = m2.files[0].sha1;
        f.size = new.len() as u64;
        f.parts.extend(m1.files[0].parts.iter().map(|q| crate::manifest::ChunkPart { file_offset: q.file_offset + 100, ..q.clone() }));
        // The delta lists only its own chunk: v1's are found in the old manifest.
        ((m1, s1), (m2, s2), d, dchunks, new)
    }

    #[tokio::test]
    async fn deltas_are_used_when_epic_has_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("G");
        let ((m1, s1), (m2, mut s2), d, dchunks, new) = shifted();
        let p = quick();
        install(job(&m1, None, &dir, &s1, &p)).await.unwrap();
        // No delta published: the full manifest, every chunk.
        assert!(delta(&s2, &m2, &m1, &Secrets::new()).await.is_none());
        assert_eq!(s2.fetched.lock().unwrap().as_slice(), ["Deltas/b2/b1.delta"]);
        assert_eq!(plan(&m2, Some(&m1), &dir, None).unwrap().reused_bytes, 0);
        // Published: one small chunk downloaded, the rest copied from the old file.
        s2.chunks.insert("Deltas/b2/b1.delta".into(), crate::manifest::write(&d, true));
        s2.chunks.extend(dchunks);
        s2.fetched.lock().unwrap().clear();
        let dl = delta(&s2, &m2, &m1, &Secrets::new()).await.expect("delta");
        assert_eq!(dl.files, 1);
        assert_eq!(fingerprint(&dl.manifest), fingerprint(&m2), "still the new build");
        let pl = plan(&dl.manifest, Some(&m1), &dir, None).unwrap();
        assert_eq!(pl.reused_bytes, 8 * 1024);
        assert!(pl.download_bytes < m2.download_size(None) / 4, "{pl:?}");
        let r = install(job(&dl.manifest, Some(&m1), &dir, &s2, &p)).await.unwrap();
        assert_eq!((r.files_written, r.bytes_reused), (1, 8 * 1024));
        assert_eq!(s2.fetched.lock().unwrap().len(), 2, "the delta and its one chunk: {:?}", s2.fetched.lock().unwrap());
        assert_eq!(read(&dir, "big.pak"), new);
        // A build to itself has no delta (nothing is fetched).
        assert!(delta(&s2, &m2, &m2, &Secrets::new()).await.is_none());
        assert_eq!(s2.fetched.lock().unwrap().len(), 2);
    }

    #[test]
    fn unusable_delta_entries_are_ignored() {
        let ((m1, _), (m2, _), d, _, _) = shifted();
        assert!(combine_delta(&m2, &m1, &d).is_some());
        // Not the new build's file.
        let mut bad = d.clone();
        bad.files[0].sha1[0] ^= 1;
        assert!(combine_delta(&m2, &m1, &bad).is_none());
        // Parts that don't add up to the file.
        let mut bad = d.clone();
        bad.files[0].parts.pop();
        assert!(combine_delta(&m2, &m1, &bad).is_none());
        // A chunk nobody lists.
        let mut bad = d.clone();
        bad.files[0].parts[1].guid = [1, 2, 3, 4];
        assert!(combine_delta(&m2, &m1, &bad).is_none());
        // A range outside its chunk.
        let mut bad = d.clone();
        bad.files[0].parts[0].offset = 1000;
        assert!(combine_delta(&m2, &m1, &bad).is_none());
        // Chunks under another folder (ChunksV4, not V5): not used.
        let mut bad = d.clone();
        bad.version = 21;
        assert!(combine_delta(&m2, &m1, &bad).is_none());
        // Without v1's chunk list (an old manifest under another folder), the parts cut
        // from v1's chunks can't be placed.
        let mut old = m1.clone();
        old.version = 18;
        assert!(combine_delta(&m2, &old, &d).is_none());
        // Hostile build ids never go into a URL.
        assert!(plain_build_id("WwT6AI3BHrp4sbmvMF3D-Q==") && !plain_build_id("../x") && !plain_build_id("a/b") && !plain_build_id("a?x") && !plain_build_id(""));
    }
}
