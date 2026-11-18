//! Installing, updating, verifying, repairing, adopting, uninstalling and launching Epic
//! games (design/STORE-EPIC.md §9.7): [`Epic`]'s methods that tie the account (a token),
//! the build API (manifest, keys), the content engine ([`crate::content`]) and the
//! records ([`crate::installs`]) together. quiverd runs them from its download queue and
//! its play path; nothing here knows about jobs.
//!
//! - **Folders:** a game installs to `<library>/<FolderName or title slug>` and gets its
//!   own prefix `<prefixes>/<title slug>` (a `-epic` suffix when a Ubisoft game already
//!   uses that name).
//! - **Builds:** each install asks Epic for the current `Live` Windows build, downloads
//!   its manifest (checked against Epic's SHA-1) and decrypts it with the build's keys,
//!   which are used for this download only. The installed manifest is kept decrypted
//!   ([`crate::installs`]), so verify and uninstall need no network.
//! - **DLC:** owned DLC that has a build of its own installs into the game's folder after
//!   the game. DLC without one is an entitlement the game checks with Epic itself.
//! - **Launch:** [`Epic::launch_material`] mints a fresh exchange code and, when the
//!   catalog says `OwnershipToken`, an ownership token, and returns EGL's arguments.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use quiver_api::downloads::{CorruptFile, InstallPreview, UpdatePreview, VerifyReport};
use quiver_api::{GameRef, StoreId};
use quiver_core::install::Progress;
use quiver_core::secret::Secret;

use crate::Epic;
use crate::api::BuildElement;
use crate::content::{self, ChunkSource, HttpChunks};
use crate::installs::{Installed, InstalledDlc, Prereq};
use crate::library::Title;
use crate::manifest::{self, Manifest, Secrets};

/// Where games and prefixes go (Quiver's `library` and `prefixes` settings).
#[derive(Debug, Clone)]
pub struct Roots {
    pub library: PathBuf,
    pub prefixes: PathBuf,
    /// Prefix folders already used (Ubisoft games'), to avoid sharing one.
    pub taken_prefixes: Vec<PathBuf>,
}

/// Download workers per game.
const WORKERS: usize = 8;

/// A build as fetched for one operation: its manifest, chunk locations and keys.
pub struct Build {
    pub element: BuildElement,
    pub manifest: Manifest,
    pub secrets: Secrets,
}

impl Build {
    fn source(&self, user_agent: &str) -> Result<HttpChunks> {
        HttpChunks::new(self.element.base_urls(), user_agent)
    }
}

/// What [`Epic::launch_material`] returns: the arguments a launch adds, with the code in
/// them (never log `args`; `shown` is the same list with the secrets replaced).
pub struct LaunchMaterial {
    pub args: Vec<String>,
    pub shown: Vec<String>,
    /// The ownership token and the file name it goes to (`<ns><item>.ovt`), when the game
    /// wants one; the caller writes it 0600 at the path it put in `-epicovt=`.
    pub ownership_token: Option<(String, Secret<Vec<u8>>)>,
}

/// An app name as a game ref.
pub fn game_ref(app: &str) -> Result<GameRef> {
    GameRef::new(StoreId::Epic, app).map_err(|e| anyhow::anyhow!("{app}: {e}"))
}

/// The arguments EGL passes (legendary `get_launch_parameters`, read as a spec), in its
/// order: the manifest's and catalog's arguments first, then the licence.
#[allow(clippy::too_many_arguments)]
pub fn egl_args(
    launch_command: &str,
    additional: Option<&str>,
    app: &str,
    code: Option<&str>,
    ovt_path: Option<&str>,
    display_name: &str,
    account_id: &str,
    locale: &str,
    namespace: &str,
) -> Vec<String> {
    let mut v: Vec<String> = launch_command.split_whitespace().map(str::to_owned).collect();
    if let Some(a) = additional {
        v.extend(a.split_whitespace().map(str::to_owned));
    }
    v.extend([
        "-AUTH_LOGIN=unused".to_owned(),
        format!("-AUTH_PASSWORD={}", code.unwrap_or("")),
        "-AUTH_TYPE=exchangecode".to_owned(),
        format!("-epicapp={app}"),
        "-epicenv=Prod".to_owned(),
    ]);
    if let Some(p) = ovt_path {
        v.push(format!("-epicovt={p}"));
    }
    v.extend([
        "-EpicPortal".to_owned(),
        format!("-epicusername={display_name}"),
        format!("-epicuserid={account_id}"),
        format!("-epiclocale={locale}"),
        format!("-epicsandboxid={namespace}"),
    ]);
    v
}

/// `args` with the exchange code replaced, for job details and logs.
pub fn redact(args: &[String]) -> Vec<String> {
    args.iter().map(|a| if a.starts_with("-AUTH_PASSWORD=") && a.len() > "-AUTH_PASSWORD=".len() { "-AUTH_PASSWORD=<redacted>".to_owned() } else { a.clone() }).collect()
}

impl Epic {
    /// Installed Epic games.
    pub fn installed(&self) -> Result<Vec<Installed>> {
        self.installs.list()
    }

    pub fn installed_game(&self, app: &str) -> Option<Installed> {
        self.installs.get(app)
    }

    /// The installed manifest (decrypted, as saved).
    pub fn installed_manifest(&self, app: &str) -> Result<Manifest> {
        manifest::parse(&self.installs.manifest(app)?, &Secrets::new())
    }

    /// A title in the library (owned by a signed-in account), for installing.
    async fn owned(&self, app: &str) -> Result<Title> {
        let lib = self.library(false).await?;
        match lib.title(app) {
            Some(t) => Ok(t.clone()),
            None => match lib.hidden(app) {
                Some(h) => bail!("epic:{app} isn't a game Quiver installs: {}", h.reason),
                None => bail!("epic:{app} is not in your Epic library"),
            },
        }
    }

    /// The account to use for a game: the one it was installed with, else its first owner.
    fn account_for(&self, t: &Title, installed: Option<&Installed>) -> Result<String> {
        if let Some(a) = installed.map(|i| i.account.clone()).filter(|a| !a.is_empty() && t.owners.contains(a)) {
            return Ok(a);
        }
        let accounts = self.accounts()?;
        let default = accounts.iter().find(|a| a.default).map(|a| a.account_id.clone());
        default.filter(|d| t.owners.contains(d)).or_else(|| t.owners.first().cloned()).context("no signed-in Epic account owns it")
    }

    /// The current build of an app, its manifest decrypted.
    pub async fn fetch_build(&self, account: &str, namespace: &str, catalog_item_id: &str, app: &str) -> Result<Build> {
        let token = self.access_token(account).await?;
        let element = self.api.build_info(token.expose(), namespace, catalog_item_id, app).await?;
        let bytes = self.api.manifest_file(&element).await?;
        let secrets: Secrets = element.secrets.clone().unwrap_or_default();
        let manifest = manifest::parse(&bytes, &secrets).with_context(|| format!("read the manifest of {app}"))?;
        Ok(Build { element, manifest, secrets })
    }

    /// Where a title installs and its prefix.
    pub fn folders(&self, t: &Title, roots: &Roots) -> (PathBuf, PathBuf) {
        let name = t.attributes.folder_name.clone().filter(|f| is_plain_name(f)).unwrap_or_else(|| quiver_core::launcher::slug(&t.title));
        let dir = roots.library.join(name);
        let mut prefix = roots.prefixes.join(quiver_core::launcher::slug(&t.title));
        if roots.taken_prefixes.contains(&prefix) {
            prefix = roots.prefixes.join(format!("{}-epic", quiver_core::launcher::slug(&t.title)));
        }
        (dir, prefix)
    }

    /// Sizes and free space for installing (or updating) `app`.
    pub async fn install_preview(&self, app: &str, roots: &Roots) -> Result<InstallPreview> {
        let t = self.owned(app).await?;
        let installed = self.installed_game(app);
        let account = self.account_for(&t, installed.as_ref())?;
        let build = self.fetch_build(&account, &t.namespace, &t.catalog_item_id, app).await?;
        let m = &build.manifest;
        let tags = installed.as_ref().and_then(|i| i.tags.clone());
        let dest = installed.as_ref().map(|i| i.dir.clone()).unwrap_or_else(|| self.folders(&t, roots).0);
        let install = m.install_size(tags.as_deref());
        let download = m.download_size(tags.as_deref());
        let update = match &installed {
            Some(i) => Some(self.diff(i, m, tags.as_deref())?),
            None => None,
        };
        let (remaining_download, required) = match &update {
            Some(u) => (u.download_bytes, u.write_bytes),
            None => (download, install),
        };
        let free = quiver_core::install::free_space(&dest)?;
        let margin = quiver_core::install::space_margin(required);
        Ok(InstallPreview {
            product_id: 0,
            game: Some(game_ref(app)?),
            name: t.title.clone(),
            dest,
            version: Some(m.meta.build_version.clone()),
            language: String::new(),
            languages: Vec::new(),
            download_bytes: download,
            install_bytes: install,
            remaining_download_bytes: remaining_download,
            required_bytes: required,
            free_bytes: free,
            margin_bytes: margin,
            fits: free >= required + margin,
            update,
        })
    }

    /// What updating an installed game to `latest` changes.
    fn diff(&self, i: &Installed, latest: &Manifest, tags: Option<&[String]>) -> Result<UpdatePreview> {
        let old = self.installed_manifest(&i.app_name)?;
        let old_files: HashMap<&str, &manifest::FileEntry> = old.files_for(tags).into_iter().map(|f| (f.path.as_str(), f)).collect();
        let (mut changed, mut added, mut unchanged, mut write, mut chunks) = (Vec::new(), Vec::new(), 0usize, 0u64, std::collections::HashSet::new());
        let new_files = latest.files_for(tags);
        for f in &new_files {
            match old_files.get(f.path.as_str()) {
                Some(o) if o.sha1 == f.sha1 => unchanged += 1,
                Some(_) => changed.push(f.path.clone()),
                None => added.push(f.path.clone()),
            }
            if old_files.get(f.path.as_str()).is_none_or(|o| o.sha1 != f.sha1) {
                write += f.size;
                chunks.extend(f.parts.iter().map(|p| p.guid));
            }
        }
        let new_paths: std::collections::HashSet<&str> = new_files.iter().map(|f| f.path.as_str()).collect();
        let removed: Vec<String> = old_files.keys().filter(|p| !new_paths.contains(**p)).map(|p| (*p).to_owned()).collect();
        let download = chunks.iter().filter_map(|g| latest.chunk(g)).map(|c| c.file_size).sum();
        Ok(UpdatePreview {
            product_id: 0,
            game: Some(game_ref(&i.app_name)?),
            name: i.title.clone(),
            installed_version: Some(i.version.clone()),
            latest_version: Some(latest.meta.build_version.clone()),
            up_to_date: changed.is_empty() && added.is_empty() && removed.is_empty() && i.version == latest.meta.build_version,
            download_bytes: download,
            write_bytes: write,
            changed,
            added,
            removed,
            unchanged,
        })
    }

    /// What updating an installed game would change.
    pub async fn update_preview(&self, app: &str) -> Result<UpdatePreview> {
        let i = self.installed_game(app).with_context(|| format!("epic:{app} isn't installed"))?;
        let t = self.owned(app).await?;
        let account = self.account_for(&t, Some(&i))?;
        let build = self.fetch_build(&account, &t.namespace, &t.catalog_item_id, app).await?;
        self.diff(&i, &build.manifest, i.tags.as_deref())
    }

    /// Install `app`, or update it to the current build, then its owned DLC that has a
    /// build. Resumes a partial install. `status` gets lines for people.
    pub async fn install(&self, app: &str, roots: &Roots, progress: &Progress, status: &(dyn Fn(String) + Send + Sync)) -> Result<(Installed, Vec<String>)> {
        let t = self.owned(app).await?;
        if let Some(tp) = &t.third_party {
            bail!("{} is managed by {}: install it there", t.title, tp.client_name());
        }
        let existing = self.installed_game(app);
        let account = self.account_for(&t, existing.as_ref())?;
        status("Reading the build…".into());
        let build = self.fetch_build(&account, &t.namespace, &t.catalog_item_id, app).await?;
        let (dir, prefix) = match &existing {
            Some(i) => (i.dir.clone(), i.prefix.clone()),
            None => self.folders(&t, roots),
        };
        let old = match &existing {
            Some(i) => self.installed_manifest(app).ok().filter(|_| i.dir.is_dir()),
            None => None,
        };
        let tags = existing.as_ref().and_then(|i| i.tags.clone());
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        status(if existing.is_some() { format!("Updating to {}…", build.manifest.meta.build_version) } else { "Downloading…".into() });
        let source = build.source(&self.api.config.user_agent)?;
        let report = content::install(content::Job {
            manifest: &build.manifest,
            old: old.as_ref(),
            dir: &dir,
            tags: tags.as_deref(),
            secrets: &build.secrets,
            source: &source,
            progress,
            workers: WORKERS,
        })
        .await?;
        let m = &build.manifest;
        self.installs.save_manifest(app, &manifest::write(m, true))?;
        let prereq = (!m.meta.prereq_path.trim().is_empty()).then(|| Prereq {
            ids: m.meta.prereq_ids.clone(),
            name: m.meta.prereq_name.clone(),
            path: m.meta.prereq_path.replace('\\', "/"),
            args: m.meta.prereq_args.clone(),
        });
        let prereq_done = match (&existing, &prereq) {
            (_, None) => true,
            (Some(i), Some(p)) => i.prereq_done && i.prereq.as_ref().is_some_and(|q| q.ids == p.ids),
            (None, Some(_)) => false,
        };
        let now = Utc::now();
        let mut record = Installed {
            app_name: app.to_owned(),
            title: t.title.clone(),
            namespace: t.namespace.clone(),
            catalog_item_id: t.catalog_item_id.clone(),
            dir: dir.clone(),
            prefix,
            exe: m.meta.launch_exe.replace('\\', "/").trim_start_matches('/').to_owned(),
            launch_command: m.meta.launch_command.clone(),
            version: m.meta.build_version.clone(),
            manifest_hash: build.element.hash.clone(),
            size: m.install_size(tags.as_deref()),
            tags,
            dlc: existing.as_ref().map(|i| i.dlc.clone()).unwrap_or_default(),
            prereq,
            prereq_done,
            can_run_offline: t.attributes.can_run_offline,
            ownership_token: t.attributes.ownership_token,
            account: account.clone(),
            installed_at: existing.as_ref().map(|i| i.installed_at).unwrap_or(now),
            updated_at: existing.as_ref().map(|_| now),
            adopted: existing.as_ref().is_some_and(|i| i.adopted),
        };
        anyhow::ensure!(!record.exe.is_empty(), "the manifest names no launch executable");
        self.installs.upsert(record.clone())?;
        let mut notes = vec![format!(
            "{} files written, {} kept, {} removed ({} downloaded)",
            report.files_written,
            report.files_kept,
            report.files_removed,
            quiver_core::install::human_bytes(report.bytes_downloaded)
        )];
        // DLC with content of its own goes into the game's folder.
        for d in t.dlc.iter().filter(|d| d.owned) {
            let Some(dlc_app) = d.app_name.clone() else { continue };
            match self.install_dlc(&account, &t, &dlc_app, &d.catalog_item_id, &dir, progress, status).await {
                Ok(Some(v)) => {
                    record.dlc.retain(|x| x.app_name != dlc_app);
                    record.dlc.push(InstalledDlc { app_name: dlc_app.clone(), title: d.title.clone(), version: v });
                    notes.push(format!("DLC {}: installed", d.title));
                }
                Ok(None) => {}
                Err(e) if quiver_core::install::is_cancelled(&e) => return Err(e),
                Err(e) => notes.push(format!("DLC {}: not installed ({e:#})", d.title)),
            }
        }
        self.installs.upsert(record.clone())?;
        Ok((record, notes))
    }

    /// Install one DLC's build into the game's folder. `None` when it has no build (an
    /// entitlement only).
    #[allow(clippy::too_many_arguments)]
    async fn install_dlc(&self, account: &str, t: &Title, dlc_app: &str, dlc_item: &str, dir: &Path, progress: &Progress, status: &(dyn Fn(String) + Send + Sync)) -> Result<Option<String>> {
        let build = match self.fetch_build(account, &t.namespace, dlc_item, dlc_app).await {
            Ok(b) => b,
            Err(e) => {
                tracing::info!(app = dlc_app, "Epic DLC has no build: {e:#}");
                return Ok(None);
            }
        };
        if build.manifest.files.is_empty() {
            return Ok(None);
        }
        status(format!("Installing DLC {}…", if t.title.is_empty() { dlc_app } else { dlc_app }));
        let old = self.installed_manifest(dlc_app).ok();
        let source = build.source(&self.api.config.user_agent)?;
        content::install(content::Job {
            manifest: &build.manifest,
            old: old.as_ref(),
            dir,
            tags: None,
            secrets: &build.secrets,
            source: &source,
            progress,
            workers: WORKERS,
        })
        .await?;
        self.installs.save_manifest(dlc_app, &manifest::write(&build.manifest, true))?;
        Ok(Some(build.manifest.meta.build_version.clone()))
    }

    /// Hash the installed files against the installed manifest (no network).
    pub async fn verify(&self, app: &str, progress: &Progress) -> Result<VerifyReport> {
        let i = self.installed_game(app).with_context(|| format!("epic:{app} isn't installed"))?;
        let m = self.installed_manifest(app)?;
        let r = content::verify(&m, &i.dir, i.tags.as_deref(), progress).await?;
        let files = m.files_for(i.tags.as_deref());
        let size_of = |p: &str| files.iter().find(|f| f.path == p).map(|f| f.size).unwrap_or(0);
        Ok(VerifyReport {
            product_id: 0,
            game: Some(game_ref(app)?),
            at: Some(Utc::now()),
            version: Some(i.version.clone()),
            files: files.len(),
            bytes: files.iter().map(|f| f.size).sum(),
            ok: r.ok,
            missing: r.missing.clone(),
            corrupt: r
                .bad
                .iter()
                .map(|p| {
                    let expected = size_of(p);
                    let size = std::fs::metadata(i.dir.join(p)).map(|m| m.len()).unwrap_or(0);
                    CorruptFile { path: p.clone(), size, expected_size: expected, bad_slices: 1, bad_bytes: expected }
                })
                .collect(),
            unverified: Vec::new(),
            repaired_files: 0,
            repaired_bytes: 0,
        })
    }

    /// Verify, then download only what is missing or corrupt. A game whose build moved on
    /// is updated instead (which rebuilds every file that differs).
    pub async fn repair(&self, app: &str, roots: &Roots, progress: &Progress, status: &(dyn Fn(String) + Send + Sync)) -> Result<VerifyReport> {
        let i = self.installed_game(app).with_context(|| format!("epic:{app} isn't installed"))?;
        status("Verifying…".into());
        let mut report = self.verify(app, progress).await?;
        if report.is_ok() {
            return Ok(report);
        }
        let t = self.owned(app).await?;
        let account = self.account_for(&t, Some(&i))?;
        let build = self.fetch_build(&account, &t.namespace, &t.catalog_item_id, app).await?;
        if build.manifest.meta.build_version != i.version {
            status(format!("The build moved on ({} → {}): updating…", i.version, build.manifest.meta.build_version));
            self.install(app, roots, progress, status).await?;
            let mut after = self.verify(app, progress).await?;
            after.repaired_files = report.missing.len() + report.corrupt.len();
            after.repaired_bytes = progress.downloaded.load(std::sync::atomic::Ordering::Relaxed);
            return Ok(after);
        }
        let bad: Vec<String> = report.missing.iter().cloned().chain(report.corrupt.iter().map(|c| c.path.clone())).collect();
        status(format!("Re-downloading {} file(s)…", bad.len()));
        let source = build.source(&self.api.config.user_agent)?;
        let r = content::repair(
            content::Job {
                manifest: &build.manifest,
                old: None,
                dir: &i.dir,
                tags: i.tags.as_deref(),
                secrets: &build.secrets,
                source: &source,
                progress,
                workers: WORKERS,
            },
            &bad,
        )
        .await?;
        report.repaired_files = r.files_written;
        report.repaired_bytes = r.bytes_downloaded;
        Ok(report)
    }

    /// Take an existing folder (another launcher's install of this game, or a copy) as
    /// Quiver's install, in place: the current build's manifest is checked against it and
    /// only missing or different files are downloaded. Nothing outside the manifest's
    /// files is touched, and no other launcher's records or sessions are read.
    pub async fn adopt(&self, app: &str, path: &Path, roots: &Roots, progress: &Progress, status: &(dyn Fn(String) + Send + Sync)) -> Result<(Installed, VerifyReport)> {
        anyhow::ensure!(path.is_dir(), "{} is not a folder", path.display());
        anyhow::ensure!(self.installed_game(app).is_none(), "epic:{app} is already installed; uninstall it first to adopt another folder");
        let t = self.owned(app).await?;
        let account = self.account_for(&t, None)?;
        status("Reading the build…".into());
        let build = self.fetch_build(&account, &t.namespace, &t.catalog_item_id, app).await?;
        status("Checking the folder against the manifest…".into());
        let checked = content::verify(&build.manifest, path, None, progress).await?;
        anyhow::ensure!(
            checked.ok > 0,
            "none of {}'s files in {} match the current build: is it the right folder?",
            t.title,
            path.display()
        );
        let bad: Vec<String> = checked.missing.iter().cloned().chain(checked.bad.iter().cloned()).collect();
        let mut repaired = (0, 0);
        if !bad.is_empty() {
            status(format!("Downloading {} missing or different file(s)…", bad.len()));
            let source = build.source(&self.api.config.user_agent)?;
            let r = content::repair(
                content::Job { manifest: &build.manifest, old: None, dir: path, tags: None, secrets: &build.secrets, source: &source, progress, workers: WORKERS },
                &bad,
            )
            .await?;
            repaired = (r.files_written, r.bytes_downloaded);
        }
        let (_, prefix) = self.folders(&t, roots);
        let m = &build.manifest;
        self.installs.save_manifest(app, &manifest::write(m, true))?;
        let record = Installed {
            app_name: app.to_owned(),
            title: t.title.clone(),
            namespace: t.namespace.clone(),
            catalog_item_id: t.catalog_item_id.clone(),
            dir: path.to_owned(),
            prefix,
            exe: m.meta.launch_exe.replace('\\', "/").trim_start_matches('/').to_owned(),
            launch_command: m.meta.launch_command.clone(),
            version: m.meta.build_version.clone(),
            manifest_hash: build.element.hash.clone(),
            size: m.install_size(None),
            tags: None,
            dlc: Vec::new(),
            prereq: (!m.meta.prereq_path.trim().is_empty()).then(|| Prereq {
                ids: m.meta.prereq_ids.clone(),
                name: m.meta.prereq_name.clone(),
                path: m.meta.prereq_path.replace('\\', "/"),
                args: m.meta.prereq_args.clone(),
            }),
            prereq_done: m.meta.prereq_path.trim().is_empty(),
            can_run_offline: t.attributes.can_run_offline,
            ownership_token: t.attributes.ownership_token,
            account,
            installed_at: Utc::now(),
            updated_at: None,
            adopted: true,
        };
        self.installs.upsert(record.clone())?;
        let mut report = self.verify(app, progress).await?;
        report.repaired_files = repaired.0;
        report.repaired_bytes = repaired.1;
        Ok((record, report))
    }

    /// Remove an installed game's files (only its manifest's, then empty folders) and its
    /// record; the prefix too when asked.
    pub fn uninstall(&self, app: &str, remove_prefix: bool) -> Result<Installed> {
        let i = self.installed_game(app).with_context(|| format!("epic:{app} isn't installed"))?;
        if let Ok(m) = self.installed_manifest(app) {
            content::uninstall(&m, &i.dir)?;
        }
        for d in &i.dlc {
            if let Ok(m) = self.installed_manifest(&d.app_name) {
                content::uninstall(&m, &i.dir)?;
            }
            let _ = std::fs::remove_file(self.installs.manifest_path(&d.app_name));
        }
        let _ = std::fs::remove_dir_all(quiver_core::install::state_dir(&i.dir));
        let _ = std::fs::remove_dir(&i.dir);
        if remove_prefix && i.prefix.is_dir() {
            std::fs::remove_dir_all(&i.prefix).with_context(|| format!("remove {}", i.prefix.display()))?;
        }
        self.installs.remove(app)?;
        Ok(i)
    }

    /// Mark the prerequisites as installed in the game's prefix.
    pub fn prereq_done(&self, app: &str) -> Result<()> {
        let mut i = self.installed_game(app).context("not installed")?;
        i.prereq_done = true;
        self.installs.upsert(i)
    }

    /// The arguments a launch adds: EGL's, with a freshly minted exchange code (and an
    /// ownership token when the game wants one). `ovt_path` is where the caller will
    /// write the token, as the game sees it (a Windows path). Offline: no code, only for
    /// games Epic allows offline.
    pub async fn launch_material(&self, app: &str, ovt_path: impl Fn(&str) -> String, offline: bool) -> Result<LaunchMaterial> {
        let i = self.installed_game(app).with_context(|| format!("epic:{app} isn't installed"))?;
        let lib = self.library(false).await.ok();
        let t = lib.as_ref().and_then(|l| l.title(app).cloned());
        let account = match &t {
            Some(t) => self.account_for(t, Some(&i))?,
            None => i.account.clone(),
        };
        let entry = self.accounts()?.into_iter().find(|a| a.account_id == account).with_context(|| format!("the Epic account that installed {} isn't signed in", i.title))?;
        if offline && !i.can_run_offline {
            bail!("{} needs Epic online to start (Epic doesn't allow it offline)", i.title);
        }
        let (code, ovt) = if offline {
            (None, None)
        } else {
            let token = self.access_token(&account).await?;
            let code = self.api.exchange_code(token.expose()).await.context("mint an exchange code")?;
            let ovt = if i.ownership_token {
                let t = self.api.ownership_token(token.expose(), &account, &i.namespace, &i.catalog_item_id).await.context("get the ownership token")?;
                Some((format!("{}{}.ovt", i.namespace, i.catalog_item_id), t))
            } else {
                None
            };
            (Some(code), ovt)
        };
        let ovt_arg = ovt.as_ref().map(|(name, _)| ovt_path(name));
        let additional = t.as_ref().and_then(|t| t.attributes.additional_command_line.clone());
        let locale = self.api.config.locale.clone();
        let args = egl_args(
            &i.launch_command,
            additional.as_deref(),
            app,
            code.as_ref().map(|c| c.expose().as_str()),
            ovt_arg.as_deref(),
            &entry.display_name,
            &account,
            &locale,
            &i.namespace,
        );
        Ok(LaunchMaterial { shown: redact(&args), args, ownership_token: ovt })
    }
}

/// A folder name from the catalog that is safe to use as one path component.
fn is_plain_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0']) && s.len() <= 128
}

/// Chunks from a fixed set (tests).
pub struct MemChunks(pub Arc<HashMap<String, Vec<u8>>>);

impl ChunkSource for MemChunks {
    fn fetch<'a>(&'a self, rel_path: &'a str) -> futures::future::BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move { self.0.get(rel_path).cloned().with_context(|| format!("no chunk {rel_path}")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egl_arguments_in_order_and_redacted() {
        let a = egl_args("-nolauncher", Some("-dx12"), "Kestrel", Some("c0de"), Some(r"Z:\p\quiver-run\nsitem.ovt"), "Player One", "acc", "en-US", "ns");
        assert_eq!(
            a,
            [
                "-nolauncher",
                "-dx12",
                "-AUTH_LOGIN=unused",
                "-AUTH_PASSWORD=c0de",
                "-AUTH_TYPE=exchangecode",
                "-epicapp=Kestrel",
                "-epicenv=Prod",
                r"-epicovt=Z:\p\quiver-run\nsitem.ovt",
                "-EpicPortal",
                "-epicusername=Player One",
                "-epicuserid=acc",
                "-epiclocale=en-US",
                "-epicsandboxid=ns",
            ]
        );
        let shown = redact(&a);
        assert!(shown.contains(&"-AUTH_PASSWORD=<redacted>".to_owned()) && !shown.iter().any(|s| s.contains("c0de")));
        // Offline: an empty password, nothing to redact.
        let off = egl_args("", None, "Kestrel", None, None, "P", "acc", "en-US", "ns");
        assert!(off.contains(&"-AUTH_PASSWORD=".to_owned()) && redact(&off) == off);
    }

    #[test]
    fn folder_names() {
        assert!(is_plain_name("AlanWakeRemastered") && !is_plain_name("../x") && !is_plain_name("a/b") && !is_plain_name(""));
    }
}
