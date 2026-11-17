//! Launcher exports: a game (later, a pinned mod profile) as a `.desktop` entry and as a
//! Steam non-Steam shortcut.
//!
//! Every export is keyed by a stable [`ExportKey`], `quiver:<product_id>[/<profile_id>]`.
//! File names and the Steam appid derive from the key, never from the name, so renames
//! rewrite the entries in place. Each export Quiver writes is recorded in
//! `<data>/exports.json` (with a content hash), so one deleted or edited outside Quiver
//! shows up as missing or changed.
//!
//! Steam reads `shortcuts.vdf` at startup and writes it back on exit, so while Steam runs
//! shortcut changes are staged (`<data>/steam-staged.json`) and written the next time
//! Quiver touches Steam's files with Steam closed. Nothing here restarts Steam except
//! [`steam::restart_steam`], which runs only after the user confirms (in a front end, or in
//! quiverd for a confirmed `SteamRestart` request).

pub mod art;
pub mod desktop;
pub mod steam;
pub mod steamart;
pub mod steamgames;
pub mod steamlaunch;
pub mod textvdf;
pub mod vdf;

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use quiver_api::exports::{
    ExportKind, ExportRecord, ExportState, ExportStatus, SteamExportResult, SteamUser, SteamUsers,
};
use quiver_api::Art;

/// `quiver:<product_id>[/<profile_id>]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExportKey {
    pub product_id: u32,
    /// A mod profile's stable id. Profiles don't exist yet; always `None` for now.
    pub profile_id: Option<String>,
}

impl ExportKey {
    pub fn game(product_id: u32) -> Self {
        Self { product_id, profile_id: None }
    }

    pub fn parse(s: &str) -> Result<Self> {
        let rest = s.strip_prefix("quiver:").with_context(|| format!("not an export key: {s}"))?;
        let (id, profile) = match rest.split_once('/') {
            Some((id, p)) => (id, Some(p.to_owned())),
            None => (rest, None),
        };
        let key = Self { product_id: id.parse().with_context(|| format!("bad product id in {s}"))?, profile_id: profile };
        key.check()?;
        Ok(key)
    }

    fn check(&self) -> Result<()> {
        if let Some(p) = &self.profile_id {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                bail!("bad profile id '{p}'");
            }
        }
        Ok(())
    }

    /// `quiver-<product>[-<profile>]`, the stem of the .desktop file and cached icon.
    pub fn file_stem(&self) -> String {
        match &self.profile_id {
            Some(p) => format!("quiver-{}-{p}", self.product_id),
            None => format!("quiver-{}", self.product_id),
        }
    }

    /// Steam shortcut appid: `crc32(key) | 0x80000000`, written explicitly so it
    /// doesn't follow the shortcut's name (Steam's own default is crc32(exe + name)).
    pub fn steam_appid(&self) -> u32 {
        crc32fast::hash(self.to_string().as_bytes()) | 0x8000_0000
    }
}

impl fmt::Display for ExportKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.profile_id {
            Some(p) => write!(f, "quiver:{}/{p}", self.product_id),
            None => write!(f, "quiver:{}", self.product_id),
        }
    }
}

/// Where exports and Quiver's own state go. Built from the environment in production;
/// tests point it at a temporary tree.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `$XDG_DATA_HOME` (default `~/.local/share`).
    pub data_home: PathBuf,
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        Self { data_home, home }
    }

    pub fn quiver(&self) -> PathBuf {
        self.data_home.join("quiver")
    }

    pub fn applications(&self) -> PathBuf {
        self.data_home.join("applications")
    }

    /// Cached `icon_image`s and their PNG conversions.
    pub fn icons(&self) -> PathBuf {
        self.quiver().join("icons")
    }

    /// Cached Ubisoft art (box art, background, logo) for Steam's grid.
    pub fn art(&self) -> PathBuf {
        self.quiver().join("art")
    }

    pub fn records(&self) -> PathBuf {
        self.quiver().join("exports.json")
    }

    pub fn staged(&self) -> PathBuf {
        self.quiver().join("steam-staged.json")
    }

    fn lock_file(&self) -> PathBuf {
        self.quiver().join("exports.lock")
    }
}

/// What an export points at.
#[derive(Debug, Clone)]
pub struct Target {
    pub key: ExportKey,
    /// Shown in the launcher and Steam.
    pub name: String,
    /// Ubisoft art file names (product config).
    pub art: Art,
    /// The game window's WM class, when known (Wine names windows after the exe).
    pub wm_class: Option<String>,
    /// The `quiver` CLI that launches it.
    pub quiver_exe: PathBuf,
}

impl Target {
    /// Arguments for `quiver`: `play <product_id> [--profile-id <id>]`.
    pub fn launch_args(&self) -> Vec<String> {
        let mut args = vec!["play".to_owned(), self.key.product_id.to_string()];
        if let Some(p) = &self.key.profile_id {
            // TODO: `quiver play --profile-id` doesn't exist yet (mod profiles).
            args.extend(["--profile-id".to_owned(), p.clone()]);
        }
        args
    }
}

/// The `quiver` CLI: next to the running executable (the daemon and CLI are built side
/// by side), else the first on `PATH`, else plain `quiver`.
pub fn quiver_exe() -> PathBuf {
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("quiver")));
    if let Some(p) = beside.filter(|p| p.is_file()) {
        return p;
    }
    find_in_path("quiver").unwrap_or_else(|| PathBuf::from("quiver"))
}

pub(crate) fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

/// WM class for a game exe (`bin/ACOrigins.exe` -> `acorigins.exe`, as Wine sets it).
pub fn wm_class_for_exe(exe: &str) -> Option<String> {
    let base = exe.rsplit(['/', '\\']).next()?;
    (!base.is_empty()).then(|| base.to_lowercase())
}

// ---------------------------------------------------------------------------------------
// Records and locking

/// Serializes changes to exports.json, the staging file and shortcuts.vdf between the
/// daemon and the CLI (which applies staged changes itself during `quiver steam restart`).
pub(crate) fn lock(paths: &Paths) -> Result<File> {
    std::fs::create_dir_all(paths.quiver())?;
    let f = File::options().create(true).truncate(false).write(true).open(paths.lock_file())?;
    f.lock().context("lock exports")?;
    Ok(f)
}

pub fn load_records(paths: &Paths) -> Vec<ExportRecord> {
    std::fs::read_to_string(paths.records())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_records(paths: &Paths, records: &[ExportRecord]) -> Result<()> {
    write_atomic(&paths.records(), &serde_json::to_vec_pretty(records)?)
}

pub(crate) fn upsert_record(paths: &Paths, record: ExportRecord) -> Result<()> {
    let mut all = load_records(paths);
    all.retain(|r| !(r.key == record.key && r.kind == record.kind));
    all.push(record);
    all.sort_by(|a, b| a.key.cmp(&b.key).then((a.kind as u8).cmp(&(b.kind as u8))));
    save_records(paths, &all)
}

pub(crate) fn remove_record(paths: &Paths, key: &str, kind: ExportKind) -> Result<()> {
    let mut all = load_records(paths);
    let before = all.len();
    all.retain(|r| !(r.key == key && r.kind == kind));
    if all.len() != before { save_records(paths, &all) } else { Ok(()) }
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    use sha1::Digest;
    hex::encode(sha1::Sha1::digest(bytes))
}

/// Every recorded export with its current state.
pub fn list(paths: &Paths) -> Vec<ExportStatus> {
    load_records(paths)
        .into_iter()
        .map(|record| {
            let state = match record.kind {
                _ if record.staged => ExportState::WaitingForSteam,
                ExportKind::Desktop => match std::fs::read(&record.path) {
                    Err(_) => ExportState::Missing,
                    Ok(b) if hash(&b) != record.hash => ExportState::Changed,
                    Ok(_) => ExportState::Ok,
                },
                ExportKind::Steam => {
                    let fields = std::fs::read(&record.path)
                        .ok()
                        .and_then(|b| vdf::Shortcuts::parse(&b).ok())
                        .and_then(|s| s.fields(record.appid.unwrap_or_default()));
                    match fields {
                        None => ExportState::Missing,
                        Some(f) if steam::fields_hash(&f) != record.hash => ExportState::Changed,
                        Some(_) => ExportState::Ok,
                    }
                }
            };
            ExportStatus { record, state }
        })
        .collect()
}

/// Write via a temporary file in the same directory and rename over the target.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.quiver-tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let k = ExportKey::game(5059);
        assert_eq!(k.to_string(), "quiver:5059");
        assert_eq!(k.file_stem(), "quiver-5059");
        let p = ExportKey::parse("quiver:5059/p7f3k2q").unwrap();
        assert_eq!(p.profile_id.as_deref(), Some("p7f3k2q"));
        assert_eq!(p.file_stem(), "quiver-5059-p7f3k2q");
        assert_eq!(ExportKey::parse(&p.to_string()).unwrap(), p);
        assert!(ExportKey::parse("quiver:5059/../x").is_err());
        assert!(ExportKey::parse("steam:1").is_err());
        // crc32("quiver:5059") = 0xb4eb15f9 (checked with Python zlib.crc32); top bit set.
        assert_eq!(k.steam_appid(), crc32fast::hash(b"quiver:5059") | 0x8000_0000);
        assert_eq!(k.steam_appid(), 0xb4eb_15f9);
        assert_ne!(k.steam_appid(), p.steam_appid());
    }

    #[test]
    fn launch_args_and_wm_class() {
        let t = Target {
            key: ExportKey { product_id: 5059, profile_id: Some("p1".into()) },
            name: "x".into(),
            art: Art::default(),
            wm_class: None,
            quiver_exe: "/q".into(),
        };
        assert_eq!(t.launch_args(), ["play", "5059", "--profile-id", "p1"]);
        assert_eq!(wm_class_for_exe("bin/ACOrigins.exe").as_deref(), Some("acorigins.exe"));
        assert_eq!(wm_class_for_exe(r"x\ACIII.exe").as_deref(), Some("aciii.exe"));
    }
}
