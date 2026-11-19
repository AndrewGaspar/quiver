//! Epic's launcher cloud saves (EGS savesync) in Quiver's save model (design/STORE-EPIC.md
//! §3 C9, STORES.md §3.7).
//!
//! **The cloud.** Per account and app, `datastorage …/api/v1/access/egstore/savesync/
//! <account>/<app>/` lists the files the cloud holds: save manifests
//! (`…/<app>/manifests/<UTC time>.manifest`) and their chunks (`…/<app>/ChunksV4/…`), each
//! with a pre-signed `readLink`. Every upload is a whole new version: a manifest of every
//! file in the save folder and the chunks it needs, written through `writeLink`s the same
//! URL hands out for names Quiver posts. Older versions stay in the cloud; Quiver never
//! deletes any. The formats are [`crate::savefmt`]'s.
//!
//! **Here.** One cloud store per game: its real save folder in its prefix (the catalog's
//! `CloudSaveFolder`, [`crate::savepaths`]), with the catalog's include and exclude lists.
//! There are no other stores for Epic games yet: profiles share the real folder (as with
//! Steam games), because a second store needs the folder mounted per launch.
//!
//! **Sync** (the rules STORES.md §3.7 sets for every store Quiver syncs):
//! - each file is compared with the newest cloud version and with the **base**, the state
//!   both sides had at the last sync (`saves/<app>.json`, per account): changed on one side
//!   → that side wins; changed on both → a conflict, which nothing settles on its own;
//! - with no base yet (the first sync on this machine), times decide as EGL does (the
//!   file's time against the version's, ±60 s; within that, a conflict);
//! - before a launch, and between sessions ([`crate::fresh`]): download only; after a
//!   session and on request: download, then upload;
//! - before anything is downloaded over a file here, the files it replaces are copied to a
//!   backup (`<save_backups>/epic-<app>/<time>/`);
//! - deletions aren't synced: a file deleted here isn't downloaded again, and the next
//!   upload's version leaves it out (older versions keep it);
//! - uploads wait for the person's first yes ([`crate::progress::Writes`]), then happen on
//!   their own. Each upload is read back before the base moves.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, NaiveDateTime, Utc};
use quiver_api::epicgame::{CloudVersion, EpicBackup, EpicSaves, FileState, LastSync, Resolve, SaveFile, SaveMode, SaveSide, SyncReport, SyncState};
use serde::{Deserialize, Serialize};

use crate::Epic;
use crate::api::{Api, urlencode};
use crate::http::{Method, Request};
use crate::progress::{WriteKind, attribute, read_json, systime, write_json};
use crate::savefmt::{self, Guid, Manifest};
use crate::savepaths::{self, Filter, Vars};

pub const DATASTORAGE_HOST: &str = "https://datastorage-public-service-liveegs.live.use1a.on.epicgames.com";
/// Within this, a local file and a cloud version are "the same age" (EGL's rule).
const SAME_AGE_SECS: i64 = 60;
/// The format of a version's name.
const VERSION_FORMAT: &str = "%Y.%m.%d-%H.%M.%S";

// ---- the savesync calls

/// One file in the cloud listing.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CloudFile {
    pub read_link: Option<String>,
    pub write_link: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
    pub length: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Listing {
    files: HashMap<String, CloudFile>,
}

fn savesync_url(account: &str, app: &str) -> String {
    format!("{DATASTORAGE_HOST}/api/v1/access/egstore/savesync/{}/{}/", urlencode(account), urlencode(app))
}

impl Api {
    /// Every file the cloud holds for the app (manifests and chunks), by key.
    pub async fn savesync_list(&self, token: &str, account: &str, app: &str) -> Result<HashMap<String, CloudFile>> {
        let l: Listing = self.json(Request::new("savesync list", Method::Get, savesync_url(account, app)).bearer(token)).await?;
        Ok(l.files)
    }

    /// Links for files by name relative to the app (`manifests/<v>.manifest`,
    /// `ChunksV4/…`), to write them (or read them).
    pub async fn savesync_links(&self, token: &str, account: &str, app: &str, names: &[String]) -> Result<HashMap<String, CloudFile>> {
        let body = serde_json::json!({ "files": names });
        let l: Listing = self.json(Request::new("savesync links", Method::Post, savesync_url(account, app)).bearer(token).json(&body)).await?;
        Ok(l.files)
    }

    /// A pre-signed link: no token goes with it.
    pub async fn fetch_link(&self, label: &'static str, url: &str) -> Result<Vec<u8>> {
        Ok(self.call(Request::new(label, Method::Get, url)).await?.body)
    }

    pub async fn put_link(&self, label: &'static str, url: &str, data: Vec<u8>) -> Result<()> {
        self.call(Request::new(label, Method::Put, url).body("application/octet-stream", data)).await.map(|_| ())
    }
}

/// A key's file name and whether it is a manifest: `…/<app>/manifests/<v>.manifest`.
pub(crate) fn manifest_name(key: &str) -> Option<&str> {
    let (dir, file) = key.rsplit_once('/')?;
    (dir.ends_with("/manifests") || dir == "manifests").then_some(file).and_then(|f| f.strip_suffix(".manifest"))
}

/// The cloud key of a chunk of the manifest at `manifest_key`: the folder above
/// `manifests/` plus the chunk's path.
fn chunk_key(manifest_key: &str, chunk_path: &str) -> String {
    match manifest_key.rfind("/manifests/") {
        Some(i) => format!("{}/{chunk_path}", &manifest_key[..i]),
        None => chunk_path.to_owned(),
    }
}

/// A version's time from its name (`2026.11.18-10.15.00`, UTC).
pub fn version_time(name: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(name, VERSION_FORMAT).ok().map(|t| t.and_utc())
}

// ---- local state

/// Where a game's saves are on this machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub prefix: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<PathBuf>,
}

/// The files both sides had at the last sync.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Base {
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Relative path → SHA-1 (hex).
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub by: String,
}

/// `saves/<app>.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameState {
    pub place: Option<Place>,
    /// Per Epic account.
    pub base: BTreeMap<String, Base>,
    /// Background freshness per Epic account ([`crate::fresh`]).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub fresh: BTreeMap<String, crate::fresh::Fresh>,
}

pub(crate) fn state_path(epic_dir: &Path, app: &str) -> PathBuf {
    epic_dir.join("saves").join(format!("{}.json", sanitize(app)))
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect()
}

/// A file here.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalFile {
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub sha1: String,
}

/// The save folder's files that sync, by relative path (`/`-separated).
pub fn scan(dir: &Path, filter: &Filter) -> Result<BTreeMap<String, LocalFile>> {
    let mut out = BTreeMap::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
            let e = e?;
            let t = e.file_type()?;
            let path = e.path();
            if t.is_dir() {
                stack.push(path);
                continue;
            }
            if !t.is_file() {
                continue; // symbolic links and the like aren't saves
            }
            let rel = path.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
            if !filter.wants(&rel) {
                continue;
            }
            let data = std::fs::read(&path)?;
            let m = e.metadata()?;
            out.insert(rel, LocalFile { size: data.len() as u64, modified: m.modified().ok().map(systime), sha1: hex::encode(savefmt::sha1(&data)) });
        }
    }
    Ok(out)
}

/// The newest cloud version.
#[derive(Debug, Clone)]
pub struct Cloud {
    pub versions: Vec<CloudVersion>,
    pub latest: Option<(String, Manifest)>,
    pub files: HashMap<String, CloudFile>,
}

impl Cloud {
    pub fn latest_name(&self) -> Option<&str> {
        self.latest.as_ref().map(|(k, _)| manifest_name(k).unwrap_or(k))
    }

    fn latest_time(&self) -> Option<DateTime<Utc>> {
        let (k, _) = self.latest.as_ref()?;
        manifest_name(k).and_then(version_time).or_else(|| self.files.get(k).and_then(|f| f.last_modified))
    }
}

/// Each file's state against the newest cloud version and the base.
pub fn compare(local: &BTreeMap<String, LocalFile>, cloud: Option<&Manifest>, cloud_time: Option<DateTime<Utc>>, base: Option<&Base>) -> Vec<SaveFile> {
    let cloud_files: BTreeMap<&str, &savefmt::FileEntry> = cloud.map(|m| m.files.iter().map(|f| (f.name.as_str(), f)).collect()).unwrap_or_default();
    let mut names: Vec<&str> = local.keys().map(String::as_str).chain(cloud_files.keys().copied()).collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let l = local.get(name);
            let c = cloud_files.get(name).copied();
            let c_sha = c.map(|c| hex::encode(c.sha));
            let b = base.and_then(|b| b.files.get(name));
            let state = match (l, &c_sha) {
                (Some(l), Some(c)) if &l.sha1 == c => FileState::InSync,
                (None, None) => FileState::InSync,
                _ if base.is_some() => {
                    let l_changed = l.map(|l| &l.sha1) != b;
                    let c_changed = c_sha.as_ref() != b;
                    match (l.is_some(), c.is_some(), l_changed, c_changed) {
                        // Deleted in the cloud (an upload left it out): kept here, uploaded again.
                        (true, false, _, _) => FileState::LocalNewer,
                        (false, true, true, false) => FileState::DeletedHere,
                        (false, true, _, true) => FileState::CloudNewer,
                        (_, _, true, false) => FileState::LocalNewer,
                        (_, _, false, true) => FileState::CloudNewer,
                        _ => FileState::Conflict,
                    }
                }
                (Some(_), None) => FileState::LocalNewer,
                (None, Some(_)) => FileState::CloudNewer,
                (Some(l), Some(_)) => match (l.modified, cloud_time) {
                    (Some(lt), Some(ct)) if (lt - ct).num_seconds() >= SAME_AGE_SECS => FileState::LocalNewer,
                    (Some(lt), Some(ct)) if (ct - lt).num_seconds() >= SAME_AGE_SECS => FileState::CloudNewer,
                    _ => FileState::Conflict,
                },
            };
            SaveFile {
                path: name.to_owned(),
                local: l.map(|l| SaveSide { size: l.size, modified: l.modified, sha1: l.sha1.clone() }),
                cloud: c.map(|c| SaveSide { size: c.size(), modified: cloud_time, sha1: hex::encode(c.sha) }),
                state,
            }
        })
        .collect()
}

pub fn overall(files: &[SaveFile]) -> SyncState {
    if files.is_empty() {
        return SyncState::Empty;
    }
    let has = |s: FileState| files.iter().any(|f| f.state == s);
    if has(FileState::Conflict) {
        SyncState::Conflict
    } else if has(FileState::LocalNewer) && has(FileState::CloudNewer) {
        SyncState::Both
    } else if has(FileState::LocalNewer) {
        SyncState::LocalNewer
    } else if has(FileState::CloudNewer) {
        SyncState::CloudNewer
    } else {
        SyncState::InSync
    }
}

/// When a sync runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Download only.
    BeforeLaunch,
    /// Between sessions ([`crate::fresh`]): download only, the pre-launch rules; never
    /// while the game is being played.
    Background,
    AfterExit,
    /// A session's upload that couldn't go at exit, tried again between sessions (only
    /// once uploads are allowed; never while the game is being played).
    Retry,
    /// Someone asked (`source`: `cli`, `gui`, …). `first_upload_ok`: the person said yes
    /// to the first upload ever.
    Manual { resolve: Option<Resolve>, first_upload_ok: bool },
}

impl Trigger {
    fn by(self) -> &'static str {
        match self {
            Trigger::BeforeLaunch => "before launch",
            Trigger::Background => "in the background",
            Trigger::AfterExit => "after exit",
            Trigger::Retry => "after exit (retried)",
            Trigger::Manual { .. } => "on request",
        }
    }

    /// Whether this sync may upload (it still needs the first yes).
    pub fn uploads(self) -> bool {
        !matches!(self, Trigger::BeforeLaunch | Trigger::Background)
    }

    /// Syncs nobody waits for, which stand aside while the game is being played.
    fn between_sessions(self) -> bool {
        matches!(self, Trigger::Background | Trigger::Retry)
    }
}

/// A game's save location for one account, all that a sync needs.
#[derive(Debug, Clone)]
pub struct Location {
    pub app: String,
    pub account_id: String,
    pub account_name: String,
    pub template: String,
    pub dir: PathBuf,
    pub filter: Filter,
    pub max_mb: Option<u64>,
    /// `CloudSaveFolder` (and `_MAC`), written into each uploaded manifest.
    pub custom: BTreeMap<String, String>,
}

/// What the catalog says about a game's saves.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub template: Option<String>,
    pub filter: Filter,
    pub max_mb: Option<u64>,
    pub custom: BTreeMap<String, String>,
}

impl Spec {
    pub fn of(item: Option<&crate::api::CatalogItem>) -> Spec {
        let Some(item) = item else { return Spec::default() };
        let template = attribute(item, "CloudSaveFolder");
        let mut custom = BTreeMap::new();
        if let Some(t) = &template {
            custom.insert("CloudSaveFolder".to_owned(), t.clone());
        }
        if let Some(m) = attribute(item, "CloudSaveFolder_MAC") {
            custom.insert("CloudSaveFolder_MAC".to_owned(), m);
        }
        Spec {
            template,
            filter: Filter::new(attribute(item, "CloudIncludeList").as_deref(), attribute(item, "CloudExcludeList").as_deref()),
            max_mb: attribute(item, "MaxSizeMB").and_then(|m| m.parse().ok()),
            custom,
        }
    }
}

fn backup_id(t: DateTime<Utc>) -> String {
    t.format("%Y%m%d-%H%M%S").to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BackupMeta {
    created: Option<DateTime<Utc>>,
    reason: Option<String>,
    account: Option<String>,
}

const BACKUP_META: &str = ".quiver-backup.json";

/// Copy `files` (relative) from `dir` into a new backup.
fn back_up(root: &Path, app: &str, dir: &Path, files: &[String], reason: &str, account: &str) -> Result<Option<EpicBackup>> {
    let present: Vec<&String> = files.iter().filter(|f| dir.join(f).is_file()).collect();
    if present.is_empty() {
        return Ok(None);
    }
    let now = Utc::now();
    let base = root.join(format!("epic-{}", sanitize(app)));
    let mut id = backup_id(now);
    let mut n = 1;
    while base.join(&id).exists() {
        n += 1;
        id = format!("{}-{n}", backup_id(now));
    }
    let to = base.join(&id);
    let mut bytes = 0;
    for f in &present {
        let dst = to.join(f);
        std::fs::create_dir_all(dst.parent().context("backup path")?)?;
        bytes += std::fs::copy(dir.join(f), &dst)?;
    }
    write_json(&to.join(BACKUP_META), &BackupMeta { created: Some(now), reason: Some(reason.to_owned()), account: Some(account.to_owned()) })?;
    Ok(Some(EpicBackup { id, path: to, created: Some(now), files: present.len(), bytes, reason: Some(reason.to_owned()) }))
}

/// The game's backups, newest first.
pub fn backups(root: &Path, app: &str) -> Vec<EpicBackup> {
    let base = root.join(format!("epic-{}", sanitize(app)));
    let mut out: Vec<EpicBackup> = std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let path = e.path();
            let meta: BackupMeta = read_json(&path.join(BACKUP_META)).unwrap_or_default();
            let files = scan(&path, &Filter::default()).unwrap_or_default();
            let files: Vec<&LocalFile> = files.iter().filter(|(k, _)| k.as_str() != BACKUP_META).map(|(_, v)| v).collect();
            EpicBackup {
                id: e.file_name().to_string_lossy().into_owned(),
                created: meta.created.or_else(|| e.metadata().ok().and_then(|m| m.modified().ok()).map(systime)),
                files: files.len(),
                bytes: files.iter().map(|f| f.size).sum(),
                reason: meta.reason,
                path,
            }
        })
        .collect();
    out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
    out
}

fn write_file(dir: &Path, rel: &str, data: &[u8], mtime: Option<DateTime<Utc>>) -> Result<()> {
    ensure!(!rel.split('/').any(|c| c == ".." || c.is_empty() && rel.starts_with('/')), "a cloud save names a file outside its folder ({rel})");
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().context("save path")?)?;
    let tmp = path.with_file_name(format!(".{}.quiver-download", path.file_name().and_then(|n| n.to_str()).unwrap_or("save")));
    std::fs::write(&tmp, data)?;
    if let Some(t) = mtime {
        let f = std::fs::File::options().write(true).open(&tmp)?;
        let _ = f.set_modified(t.into());
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

impl Epic {
    /// Remember where an Epic game's prefix and install are (stage 2 calls this at install
    /// and before each launch), so its saves can be shown and synced outside launches.
    pub fn remember_place(&self, app: &str, prefix: &Path, install_dir: Option<&Path>) -> Result<()> {
        let path = state_path(&self.dir, app);
        let mut st: GameState = read_json(&path)?;
        let place = Place { prefix: prefix.to_path_buf(), install_dir: install_dir.map(Path::to_path_buf) };
        if st.place.as_ref() != Some(&place) {
            st.place = Some(place);
            write_json(&path, &st)?;
        }
        Ok(())
    }

    pub(crate) fn game_state(&self, app: &str) -> Result<GameState> {
        read_json(&state_path(&self.dir, app))
    }

    /// Change a game's state file in place (under the game's sync lock or not: each
    /// write reads the file first, so fields it doesn't touch stay as they are).
    pub(crate) fn edit_state(&self, app: &str, f: impl FnOnce(&mut GameState)) -> Result<()> {
        let path = state_path(&self.dir, app);
        let mut st: GameState = read_json(&path)?;
        f(&mut st);
        write_json(&path, &st)
    }

    fn set_base(&self, app: &str, account: &str, base: Base) -> Result<()> {
        let path = state_path(&self.dir, app);
        let mut st: GameState = read_json(&path)?;
        st.base.insert(account.to_owned(), base);
        write_json(&path, &st)
    }

    fn backup_root(&self) -> PathBuf {
        self.p3.backups.lock().unwrap().clone()
    }

    /// A game's save spec and account; the location too, once its prefix is known.
    pub(crate) async fn locate(&self, app: &str) -> Result<(Spec, (String, String), std::result::Result<Location, String>)> {
        let t = self.title_of(app).await?;
        let spec = Spec::of(self.catalog_item(&t).as_ref());
        let owner = self.owner_of(&t)?;
        let loc = match (&spec.template, self.game_state(app)?.place) {
            (None, _) => Err("the catalog names no save folder".to_owned()),
            (Some(_), None) => Err("known once the game is installed and launched through Quiver".to_owned()),
            (Some(tpl), Some(p)) => savepaths::resolve(tpl, Vars { prefix: &p.prefix, install_dir: p.install_dir.as_deref(), epic_id: &owner.0 }).map(|dir| Location {
                app: app.to_owned(),
                account_id: owner.0.clone(),
                account_name: owner.1.clone(),
                template: tpl.clone(),
                dir,
                filter: spec.filter.clone(),
                max_mb: spec.max_mb,
                custom: spec.custom.clone(),
            }),
        };
        Ok((spec, owner, loc))
    }

    /// The location for a launch: `prefix` and `install_dir` are the launch's (and are
    /// remembered). `None`: the game's saves sync in game.
    pub async fn launch_location(&self, app: &str, prefix: &Path, install_dir: Option<&Path>) -> Result<Option<Location>> {
        self.remember_place(app, prefix, install_dir)?;
        let (spec, _, loc) = self.locate(app).await?;
        if spec.template.is_none() {
            return Ok(None);
        }
        loc.map(Some).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// The cloud's files and its newest version's manifest.
    pub async fn read_cloud(&self, loc: &Location) -> Result<Cloud> {
        let token = self.access_token(&loc.account_id).await?;
        let files = self.api.savesync_list(token.expose(), &loc.account_id, &loc.app).await?;
        let mut manifests: Vec<(&String, &CloudFile)> = files.iter().filter(|(k, _)| manifest_name(k).is_some()).collect();
        // Names are UTC times, so they sort in time order; the listing's time breaks ties.
        manifests.sort_by(|a, b| manifest_name(a.0).cmp(&manifest_name(b.0)).then(a.1.last_modified.cmp(&b.1.last_modified)));
        let mut versions: Vec<CloudVersion> = manifests
            .iter()
            .map(|(k, f)| {
                let name = manifest_name(k).unwrap_or(k).to_owned();
                CloudVersion { at: version_time(&name).or(f.last_modified), name, files: 0, bytes: 0 }
            })
            .collect();
        let latest = match manifests.last() {
            Some((k, f)) => {
                let link = f.read_link.as_deref().context("the cloud gave no link to its newest save version")?;
                let m = savefmt::read_manifest(&self.api.fetch_link("savesync manifest", link).await?).context("read the newest cloud save version")?;
                if let Some(v) = versions.last_mut() {
                    v.files = m.files.len();
                    v.bytes = m.bytes();
                }
                Some(((*k).clone(), m))
            }
            None => None,
        };
        versions.reverse();
        Ok(Cloud { versions, latest, files })
    }

    /// Download files of the newest version.
    async fn download(&self, cloud: &Cloud, names: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
        let Some((key, m)) = &cloud.latest else { return Ok(Vec::new()) };
        let wanted: Vec<&savefmt::FileEntry> = m.files.iter().filter(|f| names.contains(&f.name)).collect();
        let mut guids: Vec<Guid> = wanted.iter().flat_map(|f| f.parts.iter().map(|p| p.guid)).collect();
        guids.sort();
        guids.dedup();
        let mut chunks = HashMap::new();
        for c in m.chunks.iter().filter(|c| guids.binary_search(&c.guid).is_ok()) {
            let ck = chunk_key(key, &c.path(m.feature_level));
            let link = cloud.files.get(&ck).and_then(|f| f.read_link.as_deref()).with_context(|| format!("the cloud is missing a chunk of its newest save version ({ck})"))?;
            let (guid, data) = savefmt::read_chunk(&self.api.fetch_link("savesync chunk", link).await?)?;
            ensure!(guid == c.guid, "the cloud served the wrong chunk for {ck}");
            chunks.insert(guid, data);
        }
        let sub = Manifest { files: wanted.into_iter().cloned().collect(), ..m.clone() };
        savefmt::assemble(&sub, &chunks)
    }

    /// Upload the save folder as a new version, read it back, return its name and files.
    async fn upload(&self, loc: &Location, cloud: &Cloud, local: &BTreeMap<String, LocalFile>) -> Result<(String, BTreeMap<String, String>)> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(local.len());
        for rel in local.keys() {
            files.push((rel.clone(), std::fs::read(loc.dir.join(rel))?));
        }
        files.sort_by_key(|(n, _)| n.to_lowercase());
        let total: u64 = files.iter().map(|(_, d)| d.len() as u64).sum();
        if let Some(max) = loc.max_mb {
            ensure!(total <= max * 1024 * 1024, "the saves are {} MB, over Epic's {max} MB limit for this game; not uploaded", total / (1024 * 1024));
        }
        // Named after the newest file (as EGL does), and never older than what's there.
        let newest = local.values().filter_map(|f| f.modified).max().unwrap_or_else(Utc::now);
        let mut name = newest.min(Utc::now()).format(VERSION_FORMAT).to_string();
        // Names sort in time order: a new version must sort after every one there.
        if let Some(last) = cloud.versions.iter().map(|v| v.name.as_str()).max().filter(|last| name.as_str() <= *last) {
            let after = version_time(last).map(|t| t + chrono::Duration::seconds(1)).unwrap_or_else(Utc::now);
            name = after.max(Utc::now()).format(VERSION_FORMAT).to_string();
        }
        let pkg = savefmt::package(&format!("{}{}", loc.app, loc.account_id), &name, loc.custom.clone(), &files, Guid::random)?;
        let manifest_rel = format!("manifests/{name}.manifest");
        let mut names: Vec<String> = pkg.chunks.iter().map(|(p, _)| p.clone()).collect();
        names.push(manifest_rel.clone());
        let token = self.access_token(&loc.account_id).await?;
        let links = self.api.savesync_links(token.expose(), &loc.account_id, &loc.app, &names).await?;
        let link = |n: &str| -> Result<String> {
            links
                .get(n)
                .or_else(|| links.iter().find(|(k, _)| k.ends_with(&format!("/{n}"))).map(|(_, v)| v))
                .and_then(|f| f.write_link.clone())
                .with_context(|| format!("Epic gave no upload link for {n}"))
        };
        // Chunks first: a version is never visible before its chunks are.
        for (p, data) in &pkg.chunks {
            self.api.put_link("savesync chunk upload", &link(p)?, data.clone()).await?;
        }
        self.api.put_link("savesync manifest upload", &link(&manifest_rel)?, pkg.manifest_file.clone()).await?;
        // Read it back.
        let after = self.api.savesync_list(token.expose(), &loc.account_id, &loc.app).await?;
        let (_, f) = after.iter().find(|(k, _)| manifest_name(k) == Some(name.as_str())).context("the upload isn't in the cloud's listing")?;
        let back = self.api.fetch_link("savesync manifest", f.read_link.as_deref().context("no link to the uploaded version")?).await?;
        ensure!(savefmt::read_manifest(&back)? == pkg.manifest, "the uploaded version reads back differently");
        let shas = pkg.manifest.files.iter().map(|f| (f.name.clone(), hex::encode(f.sha))).collect();
        Ok((name, shas))
    }

    /// Sync one game's saves.
    pub async fn sync_saves(&self, loc: &Location, trigger: Trigger) -> Result<SyncReport> {
        let lock = self.p3.syncing.lock().unwrap().entry(loc.app.clone()).or_default().clone();
        let _one = lock.lock().await;
        let mut report = SyncReport::default();
        if trigger.between_sessions() && self.is_playing(&loc.app) {
            report.held = Some("the game is being played: its saves sync when it exits".into());
            return Ok(report);
        }
        let cloud = self.read_cloud(loc).await?;
        let base = self.game_state(&loc.app)?.base.get(&loc.account_id).cloned();
        let local = scan(&loc.dir, &loc.filter)?;
        let manifest = cloud.latest.as_ref().map(|(_, m)| m);
        let files = compare(&local, manifest, cloud.latest_time(), base.as_ref());
        let resolve = match trigger {
            Trigger::Manual { resolve, .. } => resolve,
            _ => None,
        };
        let mut down: Vec<String> = Vec::new();
        let mut up = false;
        for f in &files {
            match (f.state, resolve) {
                (FileState::CloudNewer, _) | (FileState::Conflict, Some(Resolve::KeepCloud)) => down.push(f.path.clone()),
                (FileState::LocalNewer, Some(Resolve::KeepCloud)) if f.cloud.is_some() => down.push(f.path.clone()),
                (FileState::LocalNewer, _) | (FileState::Conflict, Some(Resolve::KeepLocal)) => up = true,
                (FileState::Conflict, None) => report.conflicts.push(f.path.clone()),
                _ => {}
            }
        }
        if !report.conflicts.is_empty() {
            // Nothing moves while the same files changed on both sides.
            report.held = Some(format!("{} file(s) changed both here and in the cloud: choose which to keep in the game's Saves", report.conflicts.len()));
            return Ok(report);
        }
        let latest_name = cloud.latest_name().map(str::to_owned);
        if !down.is_empty() {
            report.backup = back_up(&self.backup_root(), &loc.app, &loc.dir, &down, "before cloud download", &loc.account_id)?;
            let got = self.download(&cloud, &down).await?;
            let when = cloud.latest_time();
            for (rel, data) in &got {
                write_file(&loc.dir, rel, data, when)?;
            }
            report.downloaded = got.into_iter().map(|(n, _)| n).collect();
        }
        // The base after downloading: files that now match the cloud, plus the base's
        // view of files only changed here (so they still upload).
        let local = scan(&loc.dir, &loc.filter)?;
        let cloud_sha: BTreeMap<String, String> = manifest.map(|m| m.files.iter().map(|f| (f.name.clone(), hex::encode(f.sha))).collect()).unwrap_or_default();
        let mut next = base.clone().unwrap_or_default().files;
        for (name, sha) in &cloud_sha {
            if local.get(name).map(|l| &l.sha1) == Some(sha) || !local.contains_key(name) {
                next.insert(name.clone(), sha.clone());
            }
        }
        next.retain(|n, _| cloud_sha.contains_key(n) || local.contains_key(n));
        if base.is_none() {
            // A first sync: files only here aren't in the cloud yet.
            next.retain(|n, _| cloud_sha.contains_key(n));
        }
        let mut new_base = Base { at: Utc::now(), version: latest_name.clone(), files: next, by: trigger.by().to_owned() };
        if up && trigger.uploads() {
            let writes = self.writes()?;
            let ok = writes.saves.open() || matches!(trigger, Trigger::Manual { first_upload_ok: true, .. });
            if !ok {
                report.held = Some("uploads wait for your first yes: Sync in the game's Saves, or `quiver epic saves <game> sync`".into());
            } else {
                if !writes.saves.open() {
                    self.allow_writes(WriteKind::Saves)?;
                }
                let (name, shas) = self.upload(loc, &cloud, &local).await?;
                self.mark_verified(WriteKind::Saves)?;
                report.uploaded = files.iter().filter(|f| matches!(f.state, FileState::LocalNewer | FileState::Conflict) && local.contains_key(&f.path)).map(|f| f.path.clone()).collect();
                report.version = Some(name.clone());
                new_base = Base { at: Utc::now(), version: Some(name), files: shas, by: trigger.by().to_owned() };
            }
        }
        self.set_base(&loc.app, &loc.account_id, new_base)?;
        if trigger.uploads() && report.held.is_none() {
            // Whatever a session left to upload is up now.
            self.clear_upload_pending(&loc.app, &loc.account_id)?;
        }
        Ok(report)
    }

    /// `EpicGameRequest::Saves`.
    pub async fn saves_report(&self, app: &str, with_cloud: bool) -> Result<EpicSaves> {
        let game = crate::provider::game_ref(app).context("not an Epic app name")?;
        let (spec, (account_id, account_name), loc) = self.locate(app).await?;
        let mut r = EpicSaves {
            mode: if spec.template.is_some() { SaveMode::Launcher } else { SaveMode::InGame },
            account: Some(account_name),
            account_id: Some(account_id.clone()),
            folder: spec.template.clone(),
            include: spec.filter.include.clone(),
            exclude: spec.filter.exclude.clone(),
            max_mb: spec.max_mb,
            uploads: self.writes()?.saves,
            backups: backups(&self.backup_root(), app),
            ..EpicSaves::new(game)
        };
        if spec.template.is_none() {
            r.state = SyncState::Unknown;
            // The launcher's cloud can still hold files for the app (from an older catalog that
            // named a folder): say so rather than leave "none" ambiguous.
            let mut found = None;
            if with_cloud {
                let l = Location {
                    app: app.to_owned(),
                    account_id: account_id.clone(),
                    account_name: String::new(),
                    template: String::new(),
                    dir: PathBuf::new(),
                    filter: spec.filter.clone(),
                    max_mb: spec.max_mb,
                    custom: spec.custom.clone(),
                };
                match self.read_cloud(&l).await {
                    Ok(c) => {
                        r.cloud = c.versions.first().cloned();
                        r.cloud_versions = c.versions.len();
                        found = Some(c.versions.len());
                    }
                    Err(e) => r.cloud_error = Some(format!("{e:#}")),
                }
            }
            let launcher = match found {
                Some(0) => " Epic's launcher cloud has no saves for it on this account.",
                Some(_) => " Epic's launcher cloud does hold saves for it on this account (from when its catalog named a folder); Quiver can't place them without a folder.",
                None => "",
            };
            r.next = Some(format!("Saves sync in game: Epic's catalog names no save folder for the launcher, so cloud saves, if the game has them, go through Epic Online Services inside the game.{launcher}"));
            return Ok(r);
        }
        let base = self.game_state(app)?.base.get(&account_id).cloned();
        r.last_sync = base.as_ref().map(|b| LastSync { at: b.at, version: b.version.clone(), by: b.by.clone() });
        r.background = Some(self.freshness(app, &account_id, loc.as_ref().ok().and_then(|l| self.not_checked(l)).or_else(|| loc.as_ref().err().cloned())));
        let local = match &loc {
            Ok(l) => {
                r.dir = Some(l.dir.clone());
                scan(&l.dir, &l.filter)?
            }
            Err(why) => {
                r.dir_note = Some(why.clone());
                BTreeMap::new()
            }
        };
        let cloud = match (&loc, with_cloud) {
            (Ok(l), true) => match self.read_cloud(l).await {
                Ok(c) => Some(c),
                Err(e) => {
                    r.cloud_error = Some(format!("{e:#}"));
                    None
                }
            },
            (Err(_), true) => {
                // No local folder yet, but the cloud can still be listed.
                let l = Location {
                    app: app.to_owned(),
                    account_id: account_id.clone(),
                    account_name: String::new(),
                    template: spec.template.clone().unwrap_or_default(),
                    dir: PathBuf::new(),
                    filter: spec.filter.clone(),
                    max_mb: spec.max_mb,
                    custom: spec.custom.clone(),
                };
                match self.read_cloud(&l).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        r.cloud_error = Some(format!("{e:#}"));
                        None
                    }
                }
            }
            _ => None,
        };
        if let Some(c) = &cloud {
            r.cloud = c.versions.first().cloned();
            r.cloud_versions = c.versions.len();
            r.files = compare(&local, c.latest.as_ref().map(|(_, m)| m), c.latest_time(), base.as_ref());
            if loc.is_err() {
                for f in &mut r.files {
                    f.state = FileState::Unknown;
                }
            }
            r.state = if loc.is_err() { SyncState::Unknown } else { overall(&r.files) };
        } else {
            r.files = local
                .iter()
                .map(|(p, l)| SaveFile { path: p.clone(), local: Some(SaveSide { size: l.size, modified: l.modified, sha1: l.sha1.clone() }), cloud: None, state: FileState::Unknown })
                .collect();
            r.state = SyncState::Unknown;
        }
        r.next = Some(
            match r.state {
                SyncState::InSync => "In sync with Epic's cloud.",
                SyncState::Empty => "No saves here or in the cloud yet.",
                SyncState::LocalNewer if !r.uploads.open() => "Newer here: the first upload waits for your yes (Sync now).",
                SyncState::LocalNewer => "Newer here: uploaded after the next session, or now with Sync.",
                SyncState::CloudNewer if self.background_saves() => "Newer in the cloud: downloaded at the next background check or before the next launch (this machine's copies are backed up first).",
                SyncState::CloudNewer => "Newer in the cloud: downloaded before the next launch (this machine's copies are backed up first).",
                SyncState::Both => "Different files changed on each side: a sync takes both.",
                SyncState::Conflict => "The same files changed here and in the cloud: choose which to keep. Nothing syncs until then.",
                SyncState::Unknown => "Not compared with the cloud.",
            }
            .to_owned(),
        );
        Ok(r)
    }

    /// The location of a game's saves, or why there is none.
    pub async fn location(&self, app: &str) -> Result<Location> {
        let (spec, _, loc) = self.locate(app).await?;
        if spec.template.is_none() {
            bail!("epic:{app}'s saves sync in game (Epic's catalog names no save folder for the launcher)");
        }
        loc.map_err(|e| anyhow::anyhow!("epic:{app}'s save folder isn't known: {e}"))
    }

    /// Put a backup's files back; the files there now are backed up first. They upload
    /// with the next sync (they are newer here than the base).
    pub async fn restore_saves(&self, app: &str, backup: &str) -> Result<(EpicBackup, Option<EpicBackup>)> {
        let loc = self.location(app).await?;
        let list = backups(&self.backup_root(), app);
        let b = match backup {
            "latest" => list.first(),
            q => {
                let hits: Vec<&EpicBackup> = list.iter().filter(|b| b.id == q).chain(list.iter().filter(|b| b.id != q && b.id.contains(q))).collect();
                match hits.as_slice() {
                    [] => None,
                    [one, ..] if one.id == q => Some(*one),
                    [one] => Some(*one),
                    _ => bail!("{q} matches several backups; use its whole id"),
                }
            }
        }
        .cloned()
        .with_context(|| format!("no backup {backup} of epic:{app}'s saves"))?;
        let files: Vec<String> = scan(&b.path, &Filter::default())?.into_keys().filter(|k| k != BACKUP_META).collect();
        let previous = back_up(&self.backup_root(), app, &loc.dir, &files, &format!("before restoring {}", b.id), &loc.account_id)?;
        for rel in &files {
            let data = std::fs::read(b.path.join(rel))?;
            write_file(&loc.dir, rel, &data, None)?;
        }
        Ok((b, previous))
    }
}
