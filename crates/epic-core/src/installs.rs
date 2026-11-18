//! Epic games Quiver installed (design/STORE-EPIC.md §9.7): `<data>/epic/installed.json`
//! holds one record per game (folder, prefix, build, launch executable, prerequisites,
//! installed DLC), and `<data>/epic/manifests/<app>.manifest` the installed build's
//! manifest, written decrypted and without Epic's keys, so verify, update and uninstall
//! work from it without asking Epic.
//!
//! Neither holds a secret: exchange codes, ownership tokens and manifest keys live in
//! memory for one launch or one download.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A build's prerequisite installer (`manifest.meta.prereq_*`), run once per prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prereq {
    pub ids: Vec<String>,
    pub name: String,
    /// Relative to the install folder.
    pub path: String,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub app_name: String,
    pub title: String,
    pub namespace: String,
    pub catalog_item_id: String,
    pub dir: PathBuf,
    pub prefix: PathBuf,
    /// The launch executable, relative to `dir`, with `/` separators.
    pub exe: String,
    /// The manifest's launch arguments (`launch_command`).
    #[serde(default)]
    pub launch_command: String,
    /// Epic's build version.
    pub version: String,
    /// SHA-1 of the manifest file it was installed from.
    #[serde(default)]
    pub manifest_hash: String,
    #[serde(default)]
    pub size: u64,
    /// Install tags chosen (`None`: everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// DLC apps installed into `dir`.
    #[serde(default)]
    pub dlc: Vec<InstalledDlc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prereq: Option<Prereq>,
    /// The prerequisite installer ran in `prefix` (or there is none).
    #[serde(default)]
    pub prereq_done: bool,
    #[serde(default)]
    pub can_run_offline: bool,
    #[serde(default)]
    pub ownership_token: bool,
    /// The account it was downloaded with.
    #[serde(default)]
    pub account: String,
    pub installed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// Adopted from an existing folder (verified against the manifest) rather than downloaded.
    #[serde(default)]
    pub adopted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledDlc {
    pub app_name: String,
    pub title: String,
    pub version: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    games: Vec<Installed>,
}

/// `<data>/epic/installed.json` and `<data>/epic/manifests/`.
#[derive(Debug, Clone)]
pub struct Installs {
    dir: PathBuf,
}

impl Installs {
    pub fn new(epic_dir: &Path) -> Self {
        Installs { dir: epic_dir.to_owned() }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("installed.json")
    }

    pub fn manifest_path(&self, app_name: &str) -> PathBuf {
        self.dir.join("manifests").join(format!("{}.manifest", safe(app_name)))
    }

    pub fn list(&self) -> Result<Vec<Installed>> {
        match std::fs::read(self.path()) {
            Ok(b) => Ok(serde_json::from_slice::<File>(&b).with_context(|| format!("read {}", self.path().display()))?.games),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path().display())),
        }
    }

    pub fn get(&self, app_name: &str) -> Option<Installed> {
        self.list().ok()?.into_iter().find(|g| g.app_name == app_name)
    }

    fn save(&self, games: Vec<Installed>) -> Result<()> {
        write_atomic(&self.path(), &serde_json::to_vec_pretty(&File { games })?)
    }

    pub fn upsert(&self, game: Installed) -> Result<()> {
        let mut all = self.list()?;
        all.retain(|g| g.app_name != game.app_name);
        all.push(game);
        all.sort_by_key(|g| g.title.to_lowercase());
        self.save(all)
    }

    pub fn remove(&self, app_name: &str) -> Result<Option<Installed>> {
        let mut all = self.list()?;
        let gone = all.iter().position(|g| g.app_name == app_name).map(|i| all.remove(i));
        if gone.is_some() {
            self.save(all)?;
            let _ = std::fs::remove_file(self.manifest_path(app_name));
        }
        Ok(gone)
    }

    pub fn save_manifest(&self, app_name: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.manifest_path(app_name), bytes)
    }

    pub fn manifest(&self, app_name: &str) -> Result<Vec<u8>> {
        let p = self.manifest_path(app_name);
        std::fs::read(&p).with_context(|| format!("read {}", p.display()))
    }
}

/// An app name as a file name (Epic's are letters, digits and a few marks; anything
/// else becomes `_`).
fn safe(app: &str) -> String {
    app.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect::<String>().trim_start_matches('.').to_owned()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no parent")?)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(app: &str, title: &str) -> Installed {
        Installed {
            app_name: app.into(),
            title: title.into(),
            namespace: "ns".into(),
            catalog_item_id: "item".into(),
            dir: "/g/x".into(),
            prefix: "/p/x".into(),
            exe: "bin/x.exe".into(),
            launch_command: String::new(),
            version: "1.0".into(),
            manifest_hash: String::new(),
            size: 1,
            tags: None,
            dlc: Vec::new(),
            prereq: None,
            prereq_done: true,
            can_run_offline: false,
            ownership_token: false,
            account: "a".into(),
            installed_at: Utc::now(),
            updated_at: None,
            adopted: false,
        }
    }

    #[test]
    fn records_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let i = Installs::new(tmp.path());
        assert!(i.list().unwrap().is_empty() && i.get("A").is_none());
        i.upsert(rec("B", "beta")).unwrap();
        i.upsert(rec("A", "Alpha")).unwrap();
        let mut b = rec("B", "beta");
        b.version = "2.0".into();
        i.upsert(b).unwrap();
        let all = i.list().unwrap();
        assert_eq!(all.iter().map(|g| (g.app_name.as_str(), g.version.as_str())).collect::<Vec<_>>(), [("A", "1.0"), ("B", "2.0")]);
        i.save_manifest("../evil/A", b"m").unwrap();
        assert!(i.manifest_path("../evil/A").starts_with(tmp.path().join("manifests")));
        i.save_manifest("A", b"m").unwrap();
        assert_eq!(i.manifest("A").unwrap(), b"m");
        assert_eq!(i.remove("A").unwrap().unwrap().title, "Alpha");
        assert!(i.manifest("A").is_err() && i.remove("A").unwrap().is_none());
    }
}
