//! Builds on disk and on the CDN: installing and updating a game with pre-flight checks,
//! previews of what an install or update takes, and verify/repair of an install.
//!
//! An install folder records the build its files belong to: `uplay_install.manifest`
//! once finished, and `.quiver-install/target` (the manifest hash) while a download is under
//! way. Manifests fetched from the CDN are cached in `<data>/manifests/` until the build
//! is installed, so an install that is interrupted and resumed after Ubisoft published a
//! newer build can still tell which of its files are right.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use quiver_api::downloads::{InstallPreview, LanguageSize, UpdatePreview, VerifyReport};
use quiver_proto::download;

use crate::demux::Demux;
use crate::download::Downloader;
use crate::install::{self, Plan, Progress, human_bytes};
use crate::launcher::{Config, Game, Games, data_dir, register, slug};
use crate::library::Library;
use crate::manifest;
use crate::online::Online;
use crate::verify::{self, Verification};

const INSTALLED: &str = "uplay_install.manifest";

fn cache_path(hash: &str) -> PathBuf {
    let dir = if cfg!(test) { std::env::temp_dir().join("quiver-test-manifests") } else { data_dir().join("manifests") };
    dir.join(format!("{}.manifest", hash.to_uppercase()))
}

/// A cached manifest, if present and intact.
fn cached(hash: &str) -> Option<Vec<u8>> {
    let raw = std::fs::read(cache_path(hash)).ok()?;
    manifest::manifest_hash(&raw).ok().filter(|h| h.eq_ignore_ascii_case(hash)).map(|_| raw)
}

/// A manifest by hash: from the cache, else the CDN (and then cached).
async fn fetch_manifest(dl: &mut Downloader, hash: &str) -> Result<(download::Manifest, Vec<u8>)> {
    if let Some(raw) = cached(hash)
        && let Ok(m) = manifest::parse_manifest(&raw)
    {
        return Ok((m, raw));
    }
    let (m, raw) = dl.manifest(hash).await?;
    if manifest::manifest_hash(&raw).is_ok_and(|h| h.eq_ignore_ascii_case(hash)) {
        let path = cache_path(hash);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(path, &raw);
    }
    Ok((m, raw))
}

/// The latest build of a product and a downloader for it.
pub struct Latest {
    pub lib: Library,
    pub dl: Downloader,
    pub name: String,
    pub hash: String,
    pub manifest: download::Manifest,
    pub raw: Vec<u8>,
    _demux: Demux,
}

impl Latest {
    pub async fn fetch(online: &Arc<Online>, product_id: u32) -> Result<Self> {
        let demux = online.demux().await?;
        let mut lib = Library::fetch(&demux).await?;
        let product = lib.get(product_id).ok_or_else(|| crate::retry::permanent("not in your library"))?.clone();
        let hash = product.latest_manifest.clone().ok_or_else(|| crate::retry::permanent("product has no PC build"))?;
        let name = product.name.clone().unwrap_or_else(|| product_id.to_string());
        let mut dl = Downloader::open_online(online, &demux, &mut lib, product_id).await?;
        let (manifest, raw) = fetch_manifest(&mut dl, &hash).await?;
        Ok(Self { lib, dl, name, hash, manifest, raw, _demux: demux })
    }
}

/// Where a product is (or will be) installed.
pub fn dest_for(cfg: &Config, product_id: u32, name: &str) -> PathBuf {
    Games::load()
        .ok()
        .and_then(|g| g.get(product_id).map(|g| g.install_dir.clone()))
        .unwrap_or_else(|| cfg.library.join(slug(name)))
}

/// The build `dest`'s files belong to (an unfinished download's target, else the installed
/// build), and its manifest if that is still at hand.
fn on_disk(dest: &Path) -> Option<(String, Option<Vec<u8>>)> {
    let installed = std::fs::read(dest.join(INSTALLED)).ok();
    let installed_hash = installed.as_deref().and_then(|r| manifest::manifest_hash(r).ok());
    let build = install::target(dest).or(installed_hash.clone())?;
    let raw = match installed {
        Some(raw) if installed_hash.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(&build)) => Some(raw),
        _ => cached(&build),
    };
    Some((build, raw))
}

enum Base {
    /// Nothing recorded, or the same build: continue from the done lists.
    Resume,
    /// Files of another build are on disk.
    Update { old: Box<Plan>, kept: usize, removed: usize },
    /// Files of an unknown build (its manifest is gone): download everything again.
    Restart,
}

/// What installing `plan` (build `hash`) into `dest` still takes. Changes nothing.
struct Pending {
    base: Base,
    download: u64,
    write: u64,
}

fn pending(dest: &Path, hash: &str, plan: &Plan, language: &str) -> Result<Pending> {
    Ok(match on_disk(dest) {
        Some((build, _)) if build.eq_ignore_ascii_case(hash) => {
            let (download, write) = install::remaining(plan, dest);
            Pending { base: Base::Resume, download, write }
        }
        Some((_, Some(raw))) => {
            let old = install::plan(&manifest::parse_manifest(&raw)?, language)?;
            let d = install::diff(&old, plan, dest);
            Pending {
                base: Base::Update { old: Box::new(old), kept: d.unchanged.len(), removed: d.removed.len() },
                download: d.download_bytes,
                write: d.write_bytes,
            }
        }
        Some((_, None)) => Pending { base: Base::Restart, download: plan.download_bytes, write: plan.install_bytes },
        None => {
            let (download, write) = install::remaining(plan, dest);
            Pending { base: Base::Resume, download, write }
        }
    })
}

/// Install (or update) a product into `<library>/<slug>` and register it. Refuses to start
/// when the download doesn't fit on the library's filesystem.
pub async fn install_game(
    cfg: &Config,
    online: &Arc<Online>,
    product_id: u32,
    language: &str,
    progress: &Progress,
    status: impl Fn(String),
) -> Result<Game> {
    status("Fetching manifest…".into());
    let mut latest = Latest::fetch(online, product_id).await?;
    let dest = dest_for(cfg, product_id, &latest.name);
    let plan = install::plan(&latest.manifest, language)?;
    progress.total.store(plan.download_bytes, Ordering::Relaxed);
    let p = pending(&dest, &latest.hash, &plan, language)?;
    install::check_space(&dest, p.write)?;
    std::fs::create_dir_all(&dest)?;
    match &p.base {
        Base::Resume => {}
        Base::Update { old, kept, removed } => {
            install::prepare_update(old, &plan, &dest)?;
            status(format!("Updating ({kept} files unchanged, {removed} removed)…"));
        }
        Base::Restart => install::reset_progress(&dest)?,
    }
    install::set_target(&dest, &latest.hash)?;
    status(format!("Downloading {}…", human_bytes(p.download)));
    install::install(&mut latest.dl, &plan, &dest, latest.manifest.is_compressed.unwrap_or(false), progress).await?;
    progress.finalizing.store(true, Ordering::Relaxed);
    std::fs::write(dest.join(INSTALLED), &latest.raw)?;
    let _ = std::fs::remove_file(cache_path(&latest.hash));
    let game = register(cfg, &mut latest.lib, product_id, &dest.canonicalize()?, language).await?;
    let mut games = Games::load()?;
    games.upsert(game.clone());
    games.save()?;
    status("Installed".into());
    Ok(game)
}

fn installed_version(dest: &Path) -> Option<String> {
    manifest::load_manifest(&dest.join(INSTALLED)).ok().and_then(|m| m.game_version)
}

/// What updating an installed game to `latest` changes.
pub fn update_preview(game: &Game, latest: &Latest, language: &str) -> Result<UpdatePreview> {
    let raw = std::fs::read(game.install_dir.join(INSTALLED))
        .with_context(|| format!("{} has no {INSTALLED}", game.install_dir.display()))?;
    let installed_hash = manifest::manifest_hash(&raw)?;
    let mut out = UpdatePreview {
        product_id: game.product_id,
        game: None,
        name: game.name.clone(),
        installed_version: manifest::parse_manifest(&raw)?.game_version,
        latest_version: latest.manifest.game_version.clone(),
        up_to_date: installed_hash.eq_ignore_ascii_case(&latest.hash),
        download_bytes: 0,
        write_bytes: 0,
        changed: Vec::new(),
        added: Vec::new(),
        removed: Vec::new(),
        unchanged: 0,
        notes: Vec::new(),
    };
    if out.up_to_date {
        return Ok(out);
    }
    let old = install::plan(&manifest::parse_manifest(&raw)?, language)?;
    let new = install::plan(&latest.manifest, language)?;
    let d = install::diff(&old, &new, &game.install_dir);
    out.download_bytes = d.download_bytes;
    out.write_bytes = d.write_bytes;
    out.unchanged = d.unchanged.len();
    // Slices of changed files already in place aren't downloaded (`install::diff`).
    let sizes: std::collections::HashMap<&str, &install::PlannedFile> = new.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let kept: u64 = d.reusable.iter().filter_map(|(p, i)| sizes.get(p.as_str())?.slices.get(*i)).map(|s| s.size).sum();
    if kept > 0 {
        out.notes.push(format!("{} of the changed files' slices is already on disk and isn't downloaded again", install::human_bytes(kept)));
    }
    (out.changed, out.added, out.removed) = (d.changed, d.added, d.removed);
    Ok(out)
}

/// Sizes, free space and languages for installing (or updating) a product.
pub fn install_preview(cfg: &Config, product_id: u32, latest: &Latest, language: &str) -> Result<InstallPreview> {
    let dest = dest_for(cfg, product_id, &latest.name);
    let plan = install::plan(&latest.manifest, language)?;
    let p = pending(&dest, &latest.hash, &plan, language)?;
    let free = install::free_space(&dest)?;
    let margin = install::space_margin(p.write);
    let game = Games::load()?.get(product_id).cloned();
    let update = match &game {
        Some(g) if g.install_dir.join(INSTALLED).is_file() => Some(update_preview(g, latest, language)?),
        _ => None,
    };
    Ok(InstallPreview {
        product_id,
        game: None,
        name: latest.name.clone(),
        dest,
        version: latest.manifest.game_version.clone(),
        language: language.to_owned(),
        languages: install::languages(&latest.manifest)
            .into_iter()
            .map(|(code, download_bytes, install_bytes)| LanguageSize { code, download_bytes, install_bytes })
            .collect(),
        download_bytes: plan.download_bytes,
        install_bytes: plan.install_bytes,
        remaining_download_bytes: p.download,
        required_bytes: p.write,
        free_bytes: free,
        margin_bytes: margin,
        fits: free >= p.write + margin,
        update,
    })
}

/// The plan an installed game is checked against: its installed build in its language,
/// plus files of other languages that are present (imports often carry every language).
pub fn verify_plan(game: &Game, language: &str) -> Result<Plan> {
    let dest = &game.install_dir;
    let raw = std::fs::read(dest.join(INSTALLED))
        .with_context(|| format!("{} has no {INSTALLED}; nothing to verify against", dest.display()))?;
    if let Some(t) = install::target(dest)
        && !manifest::manifest_hash(&raw)?.eq_ignore_ascii_case(&t)
    {
        bail!("an update of {} is unfinished; resume it (quiver install) before verifying", game.name);
    }
    let m = manifest::parse_manifest(&raw)?;
    let mut plan = install::plan(&m, language)?;
    let have: std::collections::HashSet<String> = plan.files.iter().map(|f| f.path.clone()).collect();
    for f in install::plan(&m, "*")?.files {
        if !have.contains(&f.path) && dest.join(&f.path).symlink_metadata().is_ok() {
            plan.install_bytes += f.size;
            plan.download_bytes += f.slices.iter().map(|s| s.download_size).sum::<u64>();
            plan.files.push(f);
        }
    }
    Ok(plan)
}

/// Hash a game's files against its manifest (blocking).
pub fn verify_game(game: &Game, language: &str, progress: &Progress) -> Result<(Plan, Verification)> {
    let plan = verify_plan(game, language)?;
    let mut v = verify::verify(&plan, &game.install_dir, progress)?;
    v.report.product_id = game.product_id;
    v.report.at = Some(chrono::Utc::now());
    v.report.version = installed_version(&game.install_dir);
    Ok((plan, v))
}

pub enum RepairPhase {
    Verifying,
    /// Re-downloading this many bytes.
    Downloading(u64),
}

/// Verify a game, then re-download only its missing or corrupt slices.
pub async fn repair_game(
    online: &Arc<Online>,
    game: &Game,
    language: &str,
    progress: Arc<Progress>,
    phase: impl Fn(RepairPhase),
) -> Result<VerifyReport> {
    phase(RepairPhase::Verifying);
    let (g, lang, p) = (game.clone(), language.to_owned(), progress.clone());
    let (plan, v) = tokio::task::spawn_blocking(move || verify_game(&g, &lang, &p)).await??;
    if v.report.is_ok() {
        return Ok(v.report);
    }
    let dest = &game.install_dir;
    let repair = verify::repair_plan(&plan, &v);
    install::check_space(dest, repair.install_bytes)?;
    phase(RepairPhase::Downloading(repair.download_bytes));
    progress.total.store(repair.download_bytes, Ordering::Relaxed);
    progress.downloaded.store(0, Ordering::Relaxed);
    let demux = online.demux().await?;
    let mut lib = Library::fetch(&demux).await?;
    let mut dl = Downloader::open_online(online, &demux, &mut lib, game.product_id).await?;
    verify::prepare_repair(dest, &repair, &v)?;
    install::install(&mut dl, &repair, dest, true, &progress).await?;
    let mut report = v.report;
    report.repaired_files = repair.files.len();
    report.repaired_bytes = repair.download_bytes;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::tests::{FakeCdn, data, fixture};

    fn signed(m: &download::Manifest) -> Vec<u8> {
        use prost::Message;
        use std::io::Write;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&m.encode_to_vec()).unwrap();
        let body = z.finish().unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(b"SIGN");
        out.extend_from_slice(&body);
        out
    }

    #[tokio::test]
    async fn pending_follows_the_build_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("g");
        let mut cdn = FakeCdn::default();
        let v1 = fixture(&[("a.bin", data(1, 5_000)), ("b.bin", data(2, 2_000))], &mut cdn.slices);
        let raw1 = signed(&v1);
        let h1 = manifest::manifest_hash(&raw1).unwrap();
        let plan1 = install::plan(&v1, "en-US").unwrap();

        // Fresh.
        let p = pending(&dest, &h1, &plan1, "en-US").unwrap();
        assert!(matches!(p.base, Base::Resume));
        assert_eq!((p.download, p.write), (plan1.download_bytes, plan1.install_bytes));

        // Installed v1, then v2 changes b.bin.
        install::set_target(&dest, &h1).unwrap();
        install::install(&mut cdn, &plan1, &dest, true, &Progress::default()).await.unwrap();
        std::fs::write(dest.join(INSTALLED), &raw1).unwrap();
        let p = pending(&dest, &h1, &plan1, "en-US").unwrap();
        assert_eq!((p.download, p.write), (0, 0));

        let v2 = fixture(&[("a.bin", data(1, 5_000)), ("b.bin", data(3, 2_000))], &mut cdn.slices);
        let h2 = manifest::manifest_hash(&signed(&v2)).unwrap();
        let plan2 = install::plan(&v2, "en-US").unwrap();
        let p = pending(&dest, &h2, &plan2, "en-US").unwrap();
        assert!(matches!(p.base, Base::Update { kept: 1, removed: 0, .. }));
        assert_eq!(p.write, 2_000);

        // An unfinished download of a build whose manifest is gone: start over.
        install::set_target(&dest, "0123").unwrap();
        let p = pending(&dest, &h2, &plan2, "en-US").unwrap();
        assert!(matches!(p.base, Base::Restart));

        // Verify refuses while an update is half done.
        let game = Game {
            product_id: 1,
            name: "G".into(),
            install_dir: dest.clone(),
            prefix: dir.path().join("pfx"),
            exe: "a.bin".into(),
            args: None,
            language: None,
            game: None,
        };
        assert!(verify_plan(&game, "en-US").unwrap_err().to_string().contains("unfinished"));
        install::set_target(&dest, &h1).unwrap();
        let (_, v) = verify_game(&game, "en-US", &Progress::default()).unwrap();
        assert!(v.report.is_ok());
        assert_eq!(v.report.version.as_deref(), Some("1.0"));
    }

    /// Read-only: hash a real installed game when QUIVER_VERIFY_REAL=<install dir> is set.
    #[test]
    #[ignore]
    fn verify_real_install() {
        let Some(dir) = std::env::var_os("QUIVER_VERIFY_REAL") else { return };
        let dest = PathBuf::from(dir);
        let game = Game {
            product_id: 0,
            name: "real".into(),
            install_dir: dest.clone(),
            prefix: dest.clone(),
            exe: String::new(),
            args: None,
            language: None,
            game: None,
        };
        let lang = std::env::var("QUIVER_VERIFY_LANG").unwrap_or_else(|_| "en-US".into());
        let start = std::time::Instant::now();
        let (plan, v) = verify_game(&game, &lang, &Progress::default()).unwrap();
        eprintln!(
            "{} files, {} in {:.1}s: ok {}, missing {:?}, corrupt {:?}, unverified {}",
            plan.files.len(),
            human_bytes(plan.install_bytes),
            start.elapsed().as_secs_f64(),
            v.report.ok,
            v.report.missing,
            v.report.corrupt,
            v.report.unverified.len()
        );
    }
}
