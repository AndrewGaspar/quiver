//! A game's digital extras (soundtracks, art books, wallpapers): owned items with builds of
//! their own, listed on their game's page ([`crate::library::Title::extras`]) and, when
//! asked, downloaded into a folder **next to** the game's, never into it:
//!
//! ```text
//! <library>/DeathStranding/                       the game
//! <library>/DeathStranding Extras/DeathStrandingSound/   its soundtrack
//! ```
//!
//! Each download is the extra's current `Live` build through the content engine, like a
//! game install (resumable, checked against the manifest). Records are in
//! `<data>/epic/extras.json`, the manifests beside the games' (`manifests/<app>.manifest`).
//! Uninstalling the game leaves its extras where they are.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::stores::Extra;
use quiver_core::install::Progress;
use serde::{Deserialize, Serialize};

use crate::Epic;
use crate::content;
use crate::library::{ExtraInfo, Title};
use crate::manifest;
use crate::ops::{Roots, is_plain_name};
use crate::progress::{read_json, write_json};

/// Download workers per extra.
const WORKERS: usize = 4;

/// An extra Quiver downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledExtra {
    pub app_name: String,
    /// The game's app name.
    pub game: String,
    pub title: String,
    pub dir: PathBuf,
    pub version: String,
    pub installed_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Records {
    #[serde(default)]
    extras: Vec<InstalledExtra>,
}

fn records_path(epic_dir: &Path) -> PathBuf {
    epic_dir.join("extras.json")
}

/// The folder next to a game's that holds its extras: `<game folder> Extras`.
pub fn extras_root(game_dir: &Path) -> PathBuf {
    let name = game_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Game".into());
    game_dir.with_file_name(format!("{name} Extras"))
}

/// One extra's folder under [`extras_root`]: the catalog's `FolderName`, else its title.
pub fn extra_dir(game_dir: &Path, e: &ExtraInfo) -> PathBuf {
    let name = e.folder_name.clone().filter(|f| is_plain_name(f)).unwrap_or_else(|| quiver_core::launcher::slug(&e.title));
    extras_root(game_dir).join(name)
}

impl Epic {
    pub fn installed_extras(&self) -> Vec<InstalledExtra> {
        read_json::<Records>(&records_path(&self.dir)).map(|r| r.extras).unwrap_or_default()
    }

    fn record_extra(&self, rec: InstalledExtra) -> Result<()> {
        let path = records_path(&self.dir);
        let mut r: Records = read_json(&path)?;
        r.extras.retain(|x| x.app_name != rec.app_name);
        r.extras.push(rec);
        write_json(&path, &r)
    }

    /// Where a title's game lives: installed, else where it would install (with `roots`).
    fn game_dir(&self, t: &Title, roots: Option<&Roots>) -> Option<PathBuf> {
        match self.installed_game(&t.app_name).or_else(|| t.apps.iter().find_map(|a| self.installed_game(a))) {
            Some(i) => Some(i.dir),
            None => roots.map(|r| self.folders(t, r).0),
        }
    }

    /// The page's view of a title's extras.
    pub fn extras_of(&self, t: &Title, roots: Option<&Roots>) -> Vec<Extra> {
        let installed = self.installed_extras();
        let game_dir = self.game_dir(t, roots);
        t.extras
            .iter()
            .map(|e| {
                let done = installed.iter().find(|x| x.app_name == e.app_name);
                let dir = done.map(|x| x.dir.clone()).or_else(|| game_dir.as_deref().map(|g| extra_dir(g, e)));
                Extra {
                    id: e.app_name.clone(),
                    name: e.title.clone(),
                    dir,
                    installed: done.map(|x| x.version.clone()),
                    installable: true,
                    note: Some(if done.is_some() { "Downloaded next to the game's folder".into() } else { "Its own download, next to the game's folder (not part of the game)".into() }),
                }
            })
            .collect()
    }

    /// Download (or update) one extra of the game `app` into its folder next to the
    /// game's. `extra` is its app name or catalog item id.
    pub async fn install_extra(&self, app: &str, extra: &str, roots: &Roots, progress: &Progress, status: &(dyn Fn(String) + Send + Sync)) -> Result<InstalledExtra> {
        let t = self.owned(app).await?;
        let e = t
            .extras
            .iter()
            .find(|e| e.app_name == extra || e.catalog_item_id == extra)
            .cloned()
            .with_context(|| format!("{} has no extra {extra} (see quiver epic extras epic:{app})", t.title))?;
        let installed_game = self.installed_game(&t.app_name);
        let account = match e.owners.iter().find(|o| t.owners.contains(o)).or(e.owners.first()) {
            Some(a) => a.clone(),
            None => self.account_for(&t, installed_game.as_ref())?,
        };
        status(format!("Reading {}…", e.title));
        let build = self.fetch_build(&account, &t.namespace, &e.catalog_item_id, &e.app_name).await?;
        if build.manifest.files.is_empty() {
            bail!("Epic has no files for {}: nothing to download", e.title);
        }
        let previous = self.installed_extras().into_iter().find(|x| x.app_name == e.app_name);
        let dir = previous.as_ref().map(|x| x.dir.clone()).unwrap_or_else(|| extra_dir(&self.game_dir(&t, Some(roots)).unwrap_or_else(|| self.folders(&t, roots).0), &e));
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let old = previous.as_ref().and_then(|_| self.installed_manifest(&e.app_name).ok());
        status(format!("Downloading {}…", e.title));
        let source = build.source(&self.api.config.user_agent)?;
        content::install(content::Job {
            manifest: &build.manifest,
            old: old.as_ref(),
            dir: &dir,
            tags: None,
            secrets: &build.secrets,
            source: &source,
            progress,
            workers: WORKERS,
        })
        .await?;
        self.installs.save_manifest(&e.app_name, &manifest::write(&build.manifest, true))?;
        let rec = InstalledExtra {
            app_name: e.app_name.clone(),
            game: t.app_name.clone(),
            title: e.title.clone(),
            dir,
            version: build.manifest.meta.build_version.clone(),
            installed_at: Utc::now(),
        };
        self.record_extra(rec.clone())?;
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_sit_next_to_the_game() {
        let e = ExtraInfo { app_name: "84067e3e".into(), catalog_item_id: "i".into(), title: "DEATH STRANDING DIGITAL SOUNDTRACK".into(), folder_name: Some("DeathStrandingSound".into()), owners: vec![] };
        let game = Path::new("/g/lib/DeathStranding");
        assert_eq!(extra_dir(game, &e), Path::new("/g/lib/DeathStranding Extras/DeathStrandingSound"));
        let bad = ExtraInfo { folder_name: Some("../escape".into()), ..e };
        assert_eq!(extra_dir(game, &bad), Path::new("/g/lib/DeathStranding Extras").join(quiver_core::launcher::slug("DEATH STRANDING DIGITAL SOUNDTRACK")));
    }
}
