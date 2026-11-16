//! A game's save slots (local and cloud, side by side) and the local backups Quiver makes.
//!
//! Local slots live in `<prefix>/…/Ubisoft Game Launcher/savegames/<userId>/<productId>/`
//! as `<slot>.save` (plus `<slot>.save.upload` while an upload is pending). Backups are
//! directories `<backup root>/<productId>-<YYYYmmdd-HHMMSS>[-n]/` holding copies of
//! `<slot>.save` files and, from Quiver versions that record it, a `backup.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
pub use ubi_api::info::{CloudSlot, RestoreResult, SaveBackup, SaveSlot, SlotFile, SlotState};
use ubi_api::Game;

use crate::cloudsave::CloudSaves;
use crate::library::Library;

const META: &str = "backup.json";

/// A game's local save folder for an account.
pub fn save_dir(game: &Game, user_id: &str) -> PathBuf {
    game.prefix.join(crate::launcher::UPC_REL).join("savegames").join(user_id).join(game.product_id.to_string())
}

fn modified(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.modified().ok().map(DateTime::<Utc>::from)
}

/// `<slot>.save` files in a folder: slot, file info, pending-upload marker.
pub fn local_slots(dir: &Path) -> Result<BTreeMap<u32, (SlotFile, bool)>> {
    let mut out = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(out) };
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(slot) = name.strip_suffix(".save").and_then(|n| n.parse::<u32>().ok()) else { continue };
        let meta = e.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let pending = dir.join(format!("{name}.upload")).exists();
        out.insert(slot, (SlotFile { size: meta.len(), modified: modified(&meta) }, pending));
    }
    Ok(out)
}

/// A cloud slot as downloaded: its metadata and inflated contents.
pub struct CloudItem {
    pub info: CloudSlot,
    pub data: Vec<u8>,
}

/// Download every cloud slot (1..=max, or more if local files go higher). Slots the cloud
/// doesn't have are absent from the map.
pub async fn cloud_slots(cloud: &mut CloudSaves, lib: &mut Library, uplay_id: u32, max_local: u32) -> Result<BTreeMap<u32, CloudItem>> {
    let max = max_local.max(crate::savesync::DEFAULT_MAX_SLOT);
    let names: Vec<String> = (1..=max).map(|s| s.to_string()).collect();
    let items: Vec<(u32, &str)> = (1..=max).zip(names.iter().map(String::as_str)).collect();
    let reqs = cloud.get_requests_v2(lib, uplay_id, &items).await?;
    let mut out = BTreeMap::new();
    for (&(slot, _), req) in items.iter().zip(&reqs) {
        let (status, modified, body) = cloud.fetch(req).await?;
        match status {
            200 => {}
            404 => continue,
            s => bail!("reading cloud slot {slot}: HTTP {s}"),
        }
        let data = crate::savesync::inflate(&body)?;
        out.insert(
            slot,
            CloudItem { info: CloudSlot { size: Some(data.len() as u64), stored_size: Some(body.len() as u64), modified }, data },
        );
    }
    Ok(out)
}

/// Compare one slot's local and cloud copies. `same` is whether their bytes are equal
/// (only meaningful when both exist).
pub fn slot_state(local: Option<&SlotFile>, cloud: Option<&CloudSlot>, pending_upload: bool, same: bool) -> SlotState {
    match (local, cloud) {
        (None, None) => SlotState::Unknown,
        (Some(_), None) => SlotState::LocalOnly,
        (None, Some(_)) => SlotState::CloudOnly,
        (Some(_), Some(_)) if same => SlotState::InSync,
        (Some(_), Some(_)) if pending_upload => SlotState::LocalNewer,
        (Some(l), Some(c)) => match (l.modified, c.modified) {
            (Some(l), Some(c)) if l > c => SlotState::LocalNewer,
            (Some(l), Some(c)) if c > l => SlotState::CloudNewer,
            _ => SlotState::Differs,
        },
    }
}

/// Local and cloud slots side by side. With `cloud` `None`, only local slots are listed
/// (state `Unknown`, or `LocalNewer` for pending uploads).
pub fn merge(dir: &Path, cloud: Option<&BTreeMap<u32, CloudItem>>) -> Result<Vec<SaveSlot>> {
    let local = local_slots(dir)?;
    let mut slots: Vec<u32> = local.keys().copied().collect();
    if let Some(c) = cloud {
        slots.extend(c.keys().copied());
    }
    slots.sort_unstable();
    slots.dedup();
    let mut out = Vec::new();
    for slot in slots {
        let (file, pending) = match local.get(&slot) {
            Some((f, p)) => (Some(f.clone()), *p),
            None => (None, false),
        };
        let item = cloud.and_then(|c| c.get(&slot));
        let state = match cloud {
            None if pending => SlotState::LocalNewer,
            None => SlotState::Unknown,
            Some(_) => {
                let same = match (&file, item) {
                    (Some(f), Some(c)) if c.data.len() as u64 == f.size => {
                        std::fs::read(dir.join(format!("{slot}.save"))).map(|d| d == c.data).unwrap_or(false)
                    }
                    _ => false,
                };
                slot_state(file.as_ref(), item.map(|c| &c.info), pending, same)
            }
        };
        out.push(SaveSlot { slot, local: file, cloud: item.map(|c| c.info.clone()), pending_upload: pending, state });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Backups

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    created: Option<DateTime<Utc>>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

/// Record why a backup was made (`backup.json`).
pub fn write_meta(backup: &Path, reason: &str, account: Option<&str>, source: Option<&str>) -> Result<()> {
    let meta = Meta {
        created: Some(Utc::now()),
        reason: Some(reason.to_owned()),
        account: account.map(str::to_owned),
        source: source.map(str::to_owned),
    };
    std::fs::write(backup.join(META), serde_json::to_vec_pretty(&meta)?)?;
    Ok(())
}

/// The account a save folder belongs to (`savegames/<userId>/<productId>`).
pub fn account_of_dir(dir: &Path) -> Option<String> {
    let parent = dir.parent()?;
    (parent.parent()?.file_name()? == "savegames").then(|| parent.file_name()?.to_str().map(str::to_owned)).flatten()
}

/// A new, unused backup directory for a product (`<id>-<YYYYmmdd-HHMMSS>[-n]`), not created.
pub fn new_backup_dir(root: &Path, product_id: u32) -> PathBuf {
    let base = format!("{product_id}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let mut path = root.join(&base);
    let mut n = 2;
    while path.exists() {
        path = root.join(format!("{base}-{n}"));
        n += 1;
    }
    path
}

fn read_backup(path: &Path, id: String) -> SaveBackup {
    let meta: Meta = std::fs::read(path.join(META)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let created = meta.created.or_else(|| {
        // `<id>-YYYYmmdd-HHMMSS`, local time.
        let stamp = id.split_once('-')?.1.get(..15)?;
        let naive = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S").ok()?;
        naive.and_local_timezone(chrono::Local).single().map(|d| d.with_timezone(&Utc))
    });
    let slots = local_slots(path).map(|m| m.into_iter().map(|(s, (f, _))| (s, f.size)).collect()).unwrap_or_default();
    SaveBackup { id, path: path.to_path_buf(), created, slots, reason: meta.reason, account: meta.account }
}

/// A product's backups, newest first.
pub fn list_backups(root: &Path, product_id: u32) -> Vec<SaveBackup> {
    let prefix = format!("{product_id}-");
    let mut out: Vec<SaveBackup> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_owned();
            id.strip_prefix(&prefix)?.starts_with(|c: char| c.is_ascii_digit()).then(|| read_backup(&e.path(), id))
        })
        .collect();
    out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
    out
}

/// Copy every `<slot>.save` in `dir` into a new backup. `None` if there was nothing to copy.
pub fn backup_now(root: &Path, product_id: u32, dir: &Path, reason: &str, source: Option<&str>) -> Result<Option<SaveBackup>> {
    let slots = local_slots(dir)?;
    if slots.is_empty() {
        return Ok(None);
    }
    let dest = new_backup_dir(root, product_id);
    std::fs::create_dir_all(&dest)?;
    for slot in slots.keys() {
        let name = format!("{slot}.save");
        std::fs::copy(dir.join(&name), dest.join(&name)).with_context(|| format!("back up slot {slot}"))?;
    }
    write_meta(&dest, reason, account_of_dir(dir).as_deref(), source)?;
    let id = dest.file_name().unwrap().to_string_lossy().into_owned();
    Ok(Some(read_backup(&dest, id)))
}

/// Find a backup of `product_id` by id (or a unique prefix of one, or `latest`).
pub fn find_backup(root: &Path, product_id: u32, id: &str) -> Result<SaveBackup> {
    ensure!(!id.contains(['/', '\\']) && id != "." && id != "..", "invalid backup id {id:?}");
    let all = list_backups(root, product_id);
    if id == "latest" {
        return all.into_iter().next().context("no backups for this game");
    }
    if let Some(b) = all.iter().find(|b| b.id == id) {
        return Ok(b.clone());
    }
    let matches: Vec<&SaveBackup> = all.iter().filter(|b| b.id.contains(id)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no backup {id:?} for product {product_id} (see `quiver saves <game>`)"),
        _ => bail!("{id:?} matches several backups: {}", matches.iter().map(|b| b.id.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

/// Make a backup's slots current in `dir`. The saves there now are backed up first, and each
/// restored slot gets an upload marker so the next sync sends it to the cloud instead of
/// downloading the cloud copy over it. Slots not in the backup are left alone.
pub fn restore(root: &Path, product_id: u32, backup_id: &str, dir: &Path, source: Option<&str>) -> Result<RestoreResult> {
    let backup = find_backup(root, product_id, backup_id)?;
    ensure!(!backup.slots.is_empty(), "backup {} has no save slots", backup.id);
    if let (Some(owner), Some(current)) = (&backup.account, account_of_dir(dir))
        && owner != &current
    {
        bail!("backup {} holds another account's saves ({owner}); this game now uses {current}", backup.id);
    }
    std::fs::create_dir_all(dir)?;
    let previous = backup_now(root, product_id, dir, &format!("before restoring {}", backup.id), source)?;
    let mut restored = Vec::new();
    for &(slot, _) in &backup.slots {
        let name = format!("{slot}.save");
        let tmp = dir.join(format!("{name}.restore"));
        std::fs::copy(backup.path.join(&name), &tmp).with_context(|| format!("copy slot {slot}"))?;
        std::fs::rename(&tmp, dir.join(&name))?;
        std::fs::write(dir.join(format!("{name}.upload")), b"")?;
        restored.push(slot);
    }
    Ok(RestoreResult { restored, previous })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, data: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), data).unwrap();
    }

    #[test]
    fn states() {
        let t = |s: i64| Some(DateTime::<Utc>::from_timestamp(s, 0).unwrap());
        let l = |m| SlotFile { size: 1, modified: t(m) };
        let c = |m| CloudSlot { size: Some(1), stored_size: Some(1), modified: t(m) };
        assert_eq!(slot_state(Some(&l(5)), Some(&c(9)), false, true), SlotState::InSync);
        assert_eq!(slot_state(Some(&l(5)), Some(&c(9)), false, false), SlotState::CloudNewer);
        assert_eq!(slot_state(Some(&l(9)), Some(&c(5)), false, false), SlotState::LocalNewer);
        assert_eq!(slot_state(Some(&l(1)), Some(&c(5)), true, false), SlotState::LocalNewer);
        assert_eq!(slot_state(Some(&l(1)), None, false, false), SlotState::LocalOnly);
        assert_eq!(slot_state(None, Some(&c(1)), false, false), SlotState::CloudOnly);
        let unknown = SlotFile { size: 1, modified: None };
        assert_eq!(slot_state(Some(&unknown), Some(&c(1)), false, false), SlotState::Differs);
    }

    #[test]
    fn merge_local_and_cloud() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("savegames/user-1/3539");
        write(&dir, "1.save", b"same");
        write(&dir, "2.save", b"local edit");
        write(&dir, "2.save.upload", b"");
        write(&dir, "3.save", b"only here");
        write(&dir, "notes.txt", b"x");
        let item = |d: &[u8], m: i64| CloudItem {
            info: CloudSlot { size: Some(d.len() as u64), stored_size: Some(9), modified: DateTime::from_timestamp(m, 0) },
            data: d.to_vec(),
        };
        let far_future = 4_000_000_000;
        let cloud: BTreeMap<u32, CloudItem> =
            [(1, item(b"same", 0)), (2, item(b"cloud", far_future)), (4, item(b"cloud only", 0))].into_iter().collect();
        let rows = merge(&dir, Some(&cloud)).unwrap();
        let states: Vec<(u32, SlotState)> = rows.iter().map(|r| (r.slot, r.state)).collect();
        assert_eq!(
            states,
            [(1, SlotState::InSync), (2, SlotState::LocalNewer), (3, SlotState::LocalOnly), (4, SlotState::CloudOnly)]
        );
        assert!(rows[1].pending_upload);
        assert_eq!(rows[0].local.as_ref().unwrap().size, 4);
        assert_eq!(rows[3].cloud.as_ref().unwrap().stored_size, Some(9));
        // Without the cloud: local slots only.
        let rows = merge(&dir, None).unwrap();
        assert_eq!(rows.iter().map(|r| r.state).collect::<Vec<_>>(), [SlotState::Unknown, SlotState::LocalNewer, SlotState::Unknown]);
    }

    #[test]
    fn backup_list_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("save-backups");
        let dir = tmp.path().join("prefix/savegames/user-1/3539");
        // An old-style backup (no backup.json), as savesync::pull makes them.
        write(&root.join("3539-20261101-120000"), "1.save", b"old one");
        write(&root.join("3539-20261101-120000"), "2.save", b"old two");
        write(&root.join("5059-20261101-120000"), "1.save", b"other game");
        write(&dir, "1.save", b"current one");
        write(&dir, "3.save", b"current three");

        let list = list_backups(&root, 3539);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].slots, [(1, 7), (2, 7)]);
        assert!(list[0].created.is_some() && list[0].account.is_none());

        let r = restore(&root, 3539, "20261101", &dir, Some("cli")).unwrap();
        assert_eq!(r.restored, [1, 2]);
        assert_eq!(std::fs::read(dir.join("1.save")).unwrap(), b"old one");
        assert_eq!(std::fs::read(dir.join("2.save")).unwrap(), b"old two");
        assert_eq!(std::fs::read(dir.join("3.save")).unwrap(), b"current three");
        assert!(dir.join("1.save.upload").exists() && dir.join("2.save.upload").exists());
        assert!(!dir.join("3.save.upload").exists());

        // The saves that were there are in a new backup, recorded with why and whose.
        let prev = r.previous.unwrap();
        assert_eq!(prev.slots, [(1, 11), (3, 13)]);
        assert_eq!(prev.account.as_deref(), Some("user-1"));
        assert_eq!(prev.reason.as_deref(), Some("before restoring 3539-20261101-120000"));
        let list = list_backups(&root, 3539);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, prev.id, "newest first");

        // Undo the restore by restoring the pre-restore backup.
        restore(&root, 3539, &prev.id, &dir, None).unwrap();
        assert_eq!(std::fs::read(dir.join("1.save")).unwrap(), b"current one");

        // Guards.
        assert!(restore(&root, 3539, "../5059-20261101-120000", &dir, None).is_err());
        assert!(restore(&root, 3539, "nope", &dir, None).is_err());
        let other = tmp.path().join("prefix/savegames/user-2/3539");
        assert!(restore(&root, 3539, &prev.id, &other, None).unwrap_err().to_string().contains("another account"));
    }

    #[test]
    fn account_from_dir() {
        assert_eq!(account_of_dir(Path::new("/p/savegames/abc/3539")).as_deref(), Some("abc"));
        assert_eq!(account_of_dir(Path::new("/p/other/abc/3539")), None);
    }

    #[test]
    fn http_dates() {
        let d = crate::cloudsave::parse_http_date("Mon, 16 Nov 2026 06:09:31 GMT").unwrap();
        assert_eq!(d.to_rfc3339(), "2026-11-16T06:09:31+00:00");
        assert_eq!(crate::cloudsave::parse_http_date("Sun, 16 Nov 2026 06:09:31 GMT"), Some(d));
        assert_eq!(crate::cloudsave::parse_http_date("yesterday"), None);
    }
}
