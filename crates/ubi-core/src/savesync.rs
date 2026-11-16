//! Cloud save sync for one game's local save folder (`savegames/<userId>/<uplayId>/`).
//!
//! Formats: a local slot is `<slot>.save`; its cloud copy is the whole file zlib-compressed,
//! stored as item `<slot>` (`<userId>/<uplayId>/<slot>.savegame`). When the SDK writes a
//! slot it also creates `<slot>.save.upload`, meaning "local changes not yet uploaded"
//! (Connect deletes the marker after uploading).
//!
//! Rules:
//! - pull: cloud wins, except for slots with a pending `.upload` marker. Overwritten local
//!   files are copied to a backup directory first.
//! - push: upload every slot with a marker, verify by downloading it back, drop the marker.
//! - deletions are not synced yet (logged only).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cloudsave::CloudSaves;
use crate::library::Library;

/// Highest slot number probed in the cloud when no local file hints at more.
const DEFAULT_MAX_SLOT: u32 = 16;

#[derive(Debug, Default)]
pub struct Report {
    pub downloaded: Vec<u32>,
    pub unchanged: Vec<u32>,
    pub kept_local: Vec<u32>,
    pub uploaded: Vec<u32>,
    pub backup_dir: Option<PathBuf>,
}

fn slot_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("{slot}.save"))
}

fn marker_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("{slot}.save.upload"))
}

fn local_slots(dir: &Path) -> Result<Vec<u32>> {
    let mut slots = Vec::new();
    if dir.is_dir() {
        for e in std::fs::read_dir(dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".save").and_then(|n| n.parse().ok()) {
                slots.push(n);
            }
        }
    }
    slots.sort_unstable();
    Ok(slots)
}

fn pending_uploads(dir: &Path) -> Result<Vec<u32>> {
    let mut slots = Vec::new();
    if dir.is_dir() {
        for e in std::fs::read_dir(dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".save.upload").and_then(|n| n.parse().ok()) {
                slots.push(n);
            }
        }
    }
    slots.sort_unstable();
    Ok(slots)
}

fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).read_to_end(&mut out).context("inflate cloud save")?;
    Ok(out)
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

/// Download cloud slots into `dir`.
pub async fn pull(cloud: &mut CloudSaves, lib: &mut Library, uplay_id: u32, dir: &Path, backup_root: &Path) -> Result<Report> {
    std::fs::create_dir_all(dir)?;
    let max = local_slots(dir)?.into_iter().max().unwrap_or(0).max(DEFAULT_MAX_SLOT);
    let names: Vec<String> = (1..=max).map(|s| s.to_string()).collect();
    let items: Vec<(u32, &str)> = (1..=max).zip(names.iter().map(String::as_str)).collect();
    let reqs = cloud.get_requests_v2(lib, uplay_id, &items).await?;
    let pending = pending_uploads(dir)?;
    let mut report = Report::default();
    for (&(slot, _), req) in items.iter().zip(&reqs) {
        let (status, body) = cloud.execute(req, None).await?;
        match status {
            200 => {}
            404 => continue,
            s => bail!("downloading slot {slot}: HTTP {s}"),
        }
        let remote = inflate(&body)?;
        let path = slot_path(dir, slot);
        let local = std::fs::read(&path).ok();
        if local.as_deref() == Some(remote.as_slice()) {
            report.unchanged.push(slot);
            continue;
        }
        if pending.contains(&slot) {
            tracing::warn!(slot, "local slot has unsynced changes; keeping it over the cloud copy");
            report.kept_local.push(slot);
            continue;
        }
        if let Some(old) = local {
            let backup = report.backup_dir.get_or_insert_with(|| {
                backup_root.join(format!("{uplay_id}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")))
            });
            std::fs::create_dir_all(&*backup)?;
            std::fs::write(backup.join(format!("{slot}.save")), old)?;
        }
        let tmp = dir.join(format!("{slot}.save.download"));
        std::fs::write(&tmp, &remote)?;
        std::fs::rename(&tmp, &path)?;
        report.downloaded.push(slot);
    }
    Ok(report)
}

/// Upload slots that the game changed (those with an `.upload` marker).
pub async fn push(cloud: &mut CloudSaves, lib: &mut Library, uplay_id: u32, dir: &Path) -> Result<Report> {
    let mut report = Report::default();
    let pending = pending_uploads(dir)?;
    let mut bodies = Vec::new();
    for &slot in &pending {
        match std::fs::read(slot_path(dir, slot)) {
            Ok(data) => bodies.push((slot, data)),
            Err(_) => tracing::warn!(slot, "upload marker without a save file (deleted slot?); not synced"),
        }
    }
    if bodies.is_empty() {
        return Ok(report);
    }
    let compressed: Vec<(u32, Vec<u8>)> =
        bodies.iter().map(|(s, d)| Ok((*s, deflate(d)?))).collect::<Result<_>>()?;
    let items: Vec<(u32, &[u8])> = compressed.iter().map(|(s, c)| (*s, c.as_slice())).collect();
    let puts = cloud.put_requests_v2(lib, uplay_id, &items).await?;
    for ((slot, body), req) in compressed.iter().zip(&puts) {
        let (status, rsp) = cloud.execute(req, Some(body.clone())).await?;
        if !(200..300).contains(&status) {
            bail!("uploading slot {slot}: HTTP {status}: {}", String::from_utf8_lossy(&rsp));
        }
    }
    // Verify before dropping markers: the cloud copy must inflate to the local file.
    let names: Vec<String> = bodies.iter().map(|(s, _)| s.to_string()).collect();
    let gets: Vec<(u32, &str)> = bodies.iter().map(|(s, _)| *s).zip(names.iter().map(String::as_str)).collect();
    let reqs = cloud.get_requests_v2(lib, uplay_id, &gets).await?;
    for ((slot, local), req) in bodies.iter().zip(&reqs) {
        let (status, body) = cloud.execute(req, None).await?;
        if status != 200 || inflate(&body)? != *local {
            bail!("slot {slot} did not verify after upload (HTTP {status}); marker kept");
        }
        std::fs::remove_file(marker_path(dir, *slot))?;
        report.uploaded.push(*slot);
    }
    Ok(report)
}
