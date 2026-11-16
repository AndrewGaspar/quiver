//! First-run setup: the first-run record and storage paths (`Request::Setup`), finding
//! existing Ubisoft Connect installs (`Request::ScanInstalls`) and the app-level choice
//! about recommended fixes on unmodded games (`Request::FixesOnVanilla`).
//!
//! The scan only reads: Lutris game configs, `~/Games/*` Wine and Proton prefixes and
//! Steam's `compatdata` prefixes. In each prefix it reads Connect's own install records
//! (`Software\Wow6432Node\Ubisoft\Launcher\Installs\<id>` in `system.reg`) and lists
//! `…/Ubisoft Game Launcher/games/*`. Each game folder's `uplay_install.manifest` gives the
//! files an import would copy; nothing is written anywhere.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ubi_api::setup::{FinishSetup, FirstRun, FoundInstall, InstallSource, MatchedBy, SetupState};
use ubi_api::{Game, JobKind, LibraryEntry, Request};
use ubi_core::launcher::{self, Config, Games};

use crate::Daemon;

/// Connect's game folder inside a prefix's `drive_c`.
const CONNECT_GAMES: &str = "Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games";

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    Ok(match req {
        Request::Setup { library, prefixes, finish } => json!(setup(daemon, source, library, prefixes, finish).await?),
        Request::ScanInstalls => {
            // Ownership needs a signed-in account; without one, installs are still listed.
            let lib = match daemon.library(false).await {
                Ok(l) => Some(l),
                Err(e) => {
                    tracing::info!("scan without the library: {e:#}");
                    None
                }
            };
            let installed = Games::load()?.games;
            let home = launcher::home();
            let found = tokio::task::spawn_blocking(move || scan(&Roots::for_home(&home), lib.as_deref(), &installed)).await?;
            daemon.log_activity(source, format!("scanned for existing installs: {} found", found.len()), None);
            json!(found)
        }
        Request::FixesOnVanilla { set } => {
            let mut cfg = Config::load()?;
            if let Some(choice) = set {
                cfg.fixes_on_vanilla = choice.value();
                cfg.save()?;
                let what = match cfg.fixes_on_vanilla {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "ask later",
                };
                daemon.log_activity(source, format!("recommended fixes on unmodded games: {what}"), None);
            }
            json!(cfg.fixes_on_vanilla)
        }
        other => bail!("not a setup request: {other:?}"),
    })
}

/// Free and total bytes on the filesystem holding `path` (or its nearest existing parent).
fn disk(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64 * st.f_frsize as u64, st.f_blocks as u64 * st.f_frsize as u64))
}

/// `~/x` and relative paths made absolute.
fn expand(p: &Path) -> Result<PathBuf> {
    let s = p.to_string_lossy();
    let p = if s == "~" {
        launcher::home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        launcher::home().join(rest)
    } else {
        p.to_owned()
    };
    if !p.is_absolute() {
        bail!("{} is not an absolute path", p.display());
    }
    Ok(p)
}

/// Whether the storage paths are fixed: anything installed, queued or being imported.
fn paths_locked(daemon: &Daemon, installed: usize) -> bool {
    installed > 0
        || !daemon.downloads.list().is_empty()
        || daemon.jobs.lock().unwrap().values().any(|j| j.kind == JobKind::Import && !j.done)
}

async fn setup(
    daemon: &Arc<Daemon>,
    source: &str,
    library: Option<PathBuf>,
    prefixes: Option<PathBuf>,
    finish: Option<FinishSetup>,
) -> Result<SetupState> {
    let mut cfg = Config::load()?;
    let installed = Games::load()?.games.len();
    let locked = paths_locked(daemon, installed);
    let mut restarting = false;
    let library = library.map(|p| expand(&p)).transpose()?.filter(|p| *p != daemon.cfg.library);
    let prefixes = prefixes.map(|p| expand(&p)).transpose()?.filter(|p| *p != daemon.cfg.prefixes);
    if library.is_some() || prefixes.is_some() {
        if locked {
            bail!("games are installed, queued or importing, so the library and prefix folders can't change now");
        }
        if !daemon.sessions.list().is_empty() {
            bail!("a game is running through a Quiver bridge; stop it first");
        }
        let mut what = Vec::new();
        if let Some(p) = library {
            std::fs::create_dir_all(&p).with_context(|| format!("create {}", p.display()))?;
            what.push(format!("library {}", p.display()));
            cfg.library = p;
        }
        if let Some(p) = prefixes {
            std::fs::create_dir_all(&p).with_context(|| format!("create {}", p.display()))?;
            what.push(format!("prefixes {}", p.display()));
            cfg.prefixes = p;
        }
        cfg.save()?;
        daemon.log_activity(source, format!("storage: {}; restarting quiverd to use it", what.join(", ")), None);
        restarting = true;
    }
    if let Some(f) = finish {
        cfg.first_run = Some(FirstRun { at: chrono::Utc::now(), source: source.to_owned(), skipped: f.skipped, steam_on_install: f.steam_on_install });
        cfg.save()?;
        daemon.log_activity(source, if f.skipped { "skipped first-run setup".into() } else { "finished first-run setup".into() }, None);
    }
    let lib = disk(&cfg.library);
    let pre = disk(&cfg.prefixes);
    let state = SetupState {
        first_run: cfg.first_run.clone(),
        library: cfg.library.clone(),
        prefixes: cfg.prefixes.clone(),
        library_free: lib.map(|d| d.0),
        library_total: lib.map(|d| d.1),
        prefixes_free: pre.map(|d| d.0),
        prefixes_total: pre.map(|d| d.1),
        installed,
        paths_locked: locked,
        fixes_on_vanilla: cfg.fixes_on_vanilla,
        restarting,
    };
    if restarting {
        // Paths are read once at start; the next client request starts a new daemon.
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = std::fs::remove_file(ubi_api::socket_path());
            std::process::exit(0);
        });
    }
    Ok(state)
}

// ---------------------------------------------------------------------------------------
// Scan

/// Where to look (a stand-in home in tests).
pub(crate) struct Roots {
    /// Folders holding Lutris game configs (`*.yml`).
    lutris: Vec<PathBuf>,
    /// Folders whose children may be Wine prefixes (`~/Games`).
    games: Vec<PathBuf>,
    /// Steam roots (with `steamapps/compatdata`).
    steam: Vec<PathBuf>,
    home: PathBuf,
}

impl Roots {
    pub(crate) fn for_home(home: &Path) -> Self {
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        Self {
            lutris: vec![data.join("lutris/games"), config.join("lutris/games")],
            games: vec![home.join("Games")],
            steam: vec![data.join("Steam"), home.join(".steam/steam")],
            home: home.to_owned(),
        }
    }
}

/// A prefix to look in, and how it was found.
struct Prefix {
    path: PathBuf,
    source: InstallSource,
    label: String,
    lutris_games: Vec<String>,
}

fn tilde(home: &Path, p: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn is_prefix(p: &Path) -> bool {
    p.join("drive_c").is_dir()
}

/// Lutris games that run Ubisoft Connect: prefix -> game names.
fn lutris_prefixes(dirs: &[PathBuf], home: &Path) -> BTreeMap<PathBuf, Vec<String>> {
    let mut out: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "yml")).collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            let Ok(y) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { continue };
            let s = |v: &serde_yaml::Value| v.as_str().unwrap_or_default().to_owned();
            let game = &y["game"];
            let exe = s(&game["exe"]).to_lowercase();
            let connect = exe.contains("ubisoftconnect") || exe.contains("upc.exe") || exe.contains("uplay")
                || s(&y["requires"]) == "ubisoft-connect"
                || s(&y["service"]) == "ubisoft"
                || s(&y["game_slug"]) == "ubisoft-connect";
            let prefix = s(&game["prefix"]);
            if !connect || prefix.is_empty() {
                continue;
            }
            let prefix = match prefix.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(prefix),
            };
            let name = s(&y["name"]);
            let names = out.entry(prefix).or_default();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    out
}

fn prefixes(r: &Roots) -> Vec<Prefix> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    let mut out: Vec<Prefix> = Vec::new();
    let mut add = |out: &mut Vec<Prefix>, p: Prefix| {
        if !is_prefix(&p.path) {
            return;
        }
        let key = canon(&p.path);
        if let Some(&i) = seen.get(&key) {
            // Found again (e.g. a Lutris prefix under ~/Games): keep the first, richer label.
            let o: &mut Prefix = &mut out[i];
            for g in p.lutris_games {
                if !o.lutris_games.contains(&g) {
                    o.lutris_games.push(g);
                }
            }
            return;
        }
        seen.insert(key, out.len());
        out.push(p);
    };
    for (path, games) in lutris_prefixes(&r.lutris, &r.home) {
        // Name it after the Connect entry itself when there is one.
        let main = games.iter().find(|g| g.to_lowercase().contains("connect")).or(games.first()).cloned().unwrap_or_default();
        let label = format!("Lutris · {main}");
        add(&mut out, Prefix { path, source: InstallSource::Lutris, label, lutris_games: games });
    }
    for root in &r.games {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for d in dirs {
            for p in [d.clone(), d.join("pfx")] {
                let label = format!("Wine prefix · {}", tilde(&r.home, &d));
                add(&mut out, Prefix { path: p, source: InstallSource::Wine, label, lutris_games: Vec::new() });
            }
        }
    }
    for steam in &r.steam {
        let Ok(rd) = std::fs::read_dir(steam.join("steamapps/compatdata")) else { continue };
        let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        dirs.sort();
        for d in dirs {
            let appid = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let label = format!("Steam · compatdata {appid}");
            add(&mut out, Prefix { path: d.join("pfx"), source: InstallSource::Steam, label, lutris_games: Vec::new() });
        }
    }
    out
}

/// Connect's install records in a prefix's `system.reg`: product id -> (InstallDir as
/// written, language).
fn registry_installs(prefix: &Path) -> Vec<(u32, String, Option<String>)> {
    let Ok(text) = std::fs::read(prefix.join("system.reg")) else { return Vec::new() };
    let text = String::from_utf8_lossy(&text);
    let mut out = Vec::new();
    let mut cur: Option<(u32, Option<String>, Option<String>)> = None;
    let unq = |v: &str| v.trim().trim_matches('"').replace("\\\\", "\\");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('[') {
            if let Some((id, dir, lang)) = cur.take()
                && let Some(dir) = dir
            {
                out.push((id, dir, lang));
            }
            let key = rest.split(']').next().unwrap_or_default().to_lowercase();
            if let Some(id) = key.strip_prefix("software\\\\wow6432node\\\\ubisoft\\\\launcher\\\\installs\\\\").or_else(|| key.strip_prefix("software\\\\ubisoft\\\\launcher\\\\installs\\\\"))
                && let Ok(id) = id.parse::<u32>()
            {
                cur = Some((id, None, None));
            }
            continue;
        }
        if let Some((_, dir, lang)) = cur.as_mut() {
            if let Some(v) = line.strip_prefix("\"InstallDir\"=") {
                *dir = Some(unq(v));
            } else if let Some(v) = line.strip_prefix("\"Language\"=") {
                *lang = Some(unq(v));
            }
        }
    }
    if let Some((id, Some(dir), lang)) = cur {
        out.push((id, dir, lang));
    }
    out
}

/// A Windows path in a prefix (`C:/Program Files…/`) on this machine.
fn unix_path(prefix: &Path, win: &str) -> Option<PathBuf> {
    let win = win.replace('\\', "/");
    let (drive, rest) = win.split_once(':')?;
    let drive = drive.to_lowercase();
    let base = if drive == "c" { prefix.join("drive_c") } else { std::fs::canonicalize(prefix.join("dosdevices").join(format!("{drive}:"))).ok()? };
    let rest = rest.trim_start_matches('/').trim_end_matches('/');
    Some(if rest.is_empty() { base } else { base.join(rest) })
}

/// Lower-case letters and digits only, for matching folder names to product names.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Bytes and files of a manifest's files that are present, and the version.
fn manifest_size(dir: &Path) -> Option<(u64, usize, Option<String>)> {
    let raw = std::fs::read(dir.join("uplay_install.manifest")).ok()?;
    let m = ubi_core::manifest::parse_manifest(&raw).ok()?;
    let (mut bytes, mut files) = (0u64, 0usize);
    let mut seen = std::collections::HashSet::new();
    for f in m.chunks.iter().flat_map(|c| &c.files).filter(|f| !f.is_dir) {
        let rel = f.name.replace('\\', "/");
        if !seen.insert(rel.to_lowercase()) {
            continue;
        }
        if let Ok(meta) = dir.join(&rel).metadata() {
            bytes += meta.len();
            files += 1;
        }
    }
    Some((bytes, files, m.game_version.clone()))
}

/// Bytes and files in a folder (capped, for folders without a manifest).
fn walk_size(dir: &Path) -> (u64, usize) {
    let (mut bytes, mut files) = (0u64, 0usize);
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                files += 1;
                if files > 50_000 {
                    return (bytes, files);
                }
            }
        }
    }
    (bytes, files)
}

/// Find existing installs. `library`: the owned games (`None`: unknown); `installed`: what
/// Quiver already has.
pub(crate) fn scan(r: &Roots, library: Option<&[LibraryEntry]>, installed: &[Game]) -> Vec<FoundInstall> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let mut out: Vec<FoundInstall> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for pf in prefixes(r) {
        // Connect's records first (they carry the product id), then any other folder.
        let mut dirs: Vec<(PathBuf, Option<u32>, Option<String>)> = registry_installs(&pf.path)
            .into_iter()
            .filter_map(|(id, dir, lang)| Some((unix_path(&pf.path, &dir)?, Some(id), lang)))
            .collect();
        if let Ok(rd) = std::fs::read_dir(pf.path.join("drive_c").join(CONNECT_GAMES)) {
            let mut extra: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
            extra.sort();
            dirs.extend(extra.into_iter().map(|p| (p, None, None)));
        }
        for (dir, reg_id, language) in dirs {
            // An uninstalled game keeps its record but not its folder.
            if !dir.is_dir() || !seen.insert(canon(&dir)) {
                continue;
            }
            let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (product_id, matched_by) = match reg_id {
                Some(id) => (Some(id), Some(MatchedBy::Registry)),
                None => match library.map(|l| match_name(&folder, l)) {
                    Some(Some(id)) => (Some(id), Some(MatchedBy::Name)),
                    _ => (None, None),
                },
            };
            let entry = product_id.and_then(|id| library?.iter().find(|e| e.product_id == id));
            let owned = match (library, product_id) {
                (Some(_), Some(_)) => Some(entry.is_some()),
                (Some(_), None) => Some(false),
                (None, _) => None,
            };
            let mine = product_id.and_then(|id| installed.iter().find(|g| g.product_id == id));
            let manifest = manifest_size(&dir);
            let (size_bytes, files, version) = match &manifest {
                Some((b, f, v)) => (*b, *f, v.clone()),
                None => {
                    let (b, f) = walk_size(&dir);
                    (b, f, None)
                }
            };
            let note = if let Some(g) = mine {
                Some(format!("Already in Quiver ({})", tilde(&r.home, &g.install_dir)))
            } else if manifest.is_none() {
                Some("No uplay_install.manifest: Connect didn't finish installing it, or uninstalled it".to_owned())
            } else if product_id.is_none() && owned.is_none() {
                Some("Sign in to match it to your games".to_owned())
            } else if product_id.is_none() {
                Some("Doesn't match a game you own".to_owned())
            } else if owned.is_none() {
                Some("Sign in to check that you own it".to_owned())
            } else if owned == Some(false) {
                Some("Not owned by a signed-in account".to_owned())
            } else if files == 0 {
                Some("None of the manifest's files are there".to_owned())
            } else {
                None
            };
            out.push(FoundInstall {
                path: dir,
                prefix: pf.path.clone(),
                source: pf.source,
                source_label: pf.label.clone(),
                lutris_games: pf.lutris_games.clone(),
                product_id,
                matched_by,
                name: entry.map(|e| e.name.clone()).unwrap_or(folder),
                language,
                version,
                size_bytes,
                files,
                owned,
                installed: mine.is_some(),
                importable: note.is_none(),
                note,
            });
        }
    }
    out.sort_by(|a, b| b.importable.cmp(&a.importable).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The owned game a folder name stands for: the same name ignoring case, marks and
/// punctuation, else the only game whose name contains it (or is contained in it).
fn match_name(folder: &str, library: &[LibraryEntry]) -> Option<u32> {
    let f = norm(folder);
    if f.len() < 3 {
        return None;
    }
    if let Some(e) = library.iter().find(|e| norm(&e.name) == f) {
        return Some(e.product_id);
    }
    let close: Vec<u32> = library
        .iter()
        .filter(|e| {
            let n = norm(&e.name);
            n.len() >= 3 && (n.contains(&f) || f.contains(&n))
        })
        .map(|e| e.product_id)
        .collect();
    (close.len() == 1).then(|| close[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use std::io::Write;
    use ubi_core::proto::download;

    fn manifest(files: &[(&str, u64)]) -> Vec<u8> {
        let m = download::Manifest {
            game_version: Some("1.5.6".into()),
            chunks: vec![download::Chunk {
                files: files
                    .iter()
                    .map(|(n, s)| download::File { name: n.to_string(), size: *s, ..Default::default() })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
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

    fn entry(id: u32, name: &str) -> LibraryEntry {
        LibraryEntry {
            product_id: id,
            name: name.into(),
            installed: None,
            update_available: false,
            running: false,
            art: Default::default(),
            owned_by: vec!["me".into()],
        }
    }

    fn write(p: &Path, data: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    #[test]
    fn finds_connect_installs_in_lutris_wine_and_steam_prefixes() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let roots = Roots {
            lutris: vec![home.join("lutris")],
            games: vec![home.join("Games")],
            steam: vec![home.join("Steam")],
            home: home.to_owned(),
        };
        // A Lutris Connect prefix with two installs Connect recorded and one it didn't.
        let pfx = home.join("Games/ubisoft-connect");
        let games = pfx.join("drive_c").join(CONNECT_GAMES);
        write(&games.join("Assassin's Creed Origins/uplay_install.manifest"), &manifest(&[("ACOrigins.exe", 5), ("data/a.forge", 7), ("missing.bin", 3)]));
        write(&games.join("Assassin's Creed Origins/ACOrigins.exe"), b"12345");
        write(&games.join("Assassin's Creed Origins/data/a.forge"), b"1234567");
        write(&games.join("Assassin's Creed Origins/d3d11.dll"), b"mod");
        write(&games.join("Far Cry 5/uplay_install.manifest"), &manifest(&[("FC5.exe", 2)]));
        write(&games.join("Far Cry 5/FC5.exe"), b"12");
        write(&games.join("Assassin's Creed III Remastered/ACIII.exe.orig"), b"x");
        write(
            &pfx.join("system.reg"),
            b"WINE REGISTRY Version 2\n\n[Software\\\\Wow6432Node\\\\Ubisoft\\\\Launcher\\\\Installs\\\\3539] 1794712439\n#time=1\n\"InstallDir\"=\"C:/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Assassin's Creed Origins/\"\n\"Language\"=\"fr-FR\"\n\n[Software\\\\Wow6432Node\\\\Ubisoft\\\\Launcher\\\\Installs\\\\420] 1\n\"InstallDir\"=\"C:/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Far Cry 4/\"\n\n[Software\\\\Other] 1\n\"InstallDir\"=\"nope\"\n",
        );
        let yml = format!(
            "game:\n  args: uplay://launch/3539/0\n  exe: drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/UbisoftConnect.exe\n  prefix: {}\nname: Assassin's Creed Origins (DLSS)\nrequires: ubisoft-connect\n",
            pfx.display()
        );
        write(&home.join("lutris/origins.yml"), yml.as_bytes());
        write(&home.join("lutris/other.yml"), b"game:\n  exe: foo.exe\n  prefix: /elsewhere\nname: Not Ubisoft\n");
        // A Proton prefix in Steam with Watch Dogs 2.
        let steam_games = home.join("Steam/steamapps/compatdata/3127/pfx/drive_c").join(CONNECT_GAMES);
        write(&steam_games.join("WATCH_DOGS2/uplay_install.manifest"), &manifest(&[("bin/WatchDogs2.exe", 4)]));
        write(&steam_games.join("WATCH_DOGS2/bin/WatchDogs2.exe"), b"1234");
        // A prefix with nothing Ubisoft in it.
        std::fs::create_dir_all(home.join("Games/ea-app/drive_c")).unwrap();

        let lib = vec![entry(3539, "Assassin's Creed® Origins"), entry(1803, "Far Cry® 5"), entry(2688, "Watch_Dogs® 2"), entry(720, "Assassin's Creed® Unity")];
        let installed = vec![Game {
            product_id: 1803,
            name: "Far Cry 5".into(),
            install_dir: home.join("Games/ubi-library/Far Cry 5"),
            prefix: home.join("Games/ubi-prefixes/far-cry-5"),
            exe: "FC5.exe".into(),
            args: None,
            language: None,
        }];
        let found = scan(&roots, Some(&lib), &installed);
        let by = |n: &str| found.iter().find(|f| norm(&f.name).contains(&norm(n))).unwrap_or_else(|| panic!("{n} not found in {found:#?}"));

        let o = by("Origins");
        assert_eq!((o.product_id, o.matched_by), (Some(3539), Some(MatchedBy::Registry)));
        assert_eq!(o.language.as_deref(), Some("fr-FR"));
        assert_eq!((o.size_bytes, o.files), (12, 2), "only the manifest's files that are there");
        assert_eq!(o.version.as_deref(), Some("1.5.6"));
        assert_eq!(o.source, InstallSource::Lutris);
        assert_eq!(o.source_label, "Lutris · Assassin's Creed Origins (DLSS)");
        assert_eq!(o.lutris_games, ["Assassin's Creed Origins (DLSS)"]);
        assert!(o.importable && o.note.is_none() && o.owned == Some(true));

        let f = by("Far Cry 5");
        assert_eq!(f.matched_by, Some(MatchedBy::Name));
        assert!(f.installed && !f.importable);
        assert!(f.note.as_deref().unwrap().starts_with("Already in Quiver (~/Games/ubi-library/Far Cry 5)"));

        let w = by("Watch_Dogs");
        assert_eq!((w.product_id, w.source), (Some(2688), InstallSource::Steam));
        assert_eq!(w.source_label, "Steam · compatdata 3127");
        assert!(w.importable);

        let a = by("III Remastered");
        assert_eq!(a.product_id, None);
        assert!(!a.importable && a.note.as_deref().unwrap().contains("uplay_install.manifest"));
        // Far Cry 4's record has no folder: not listed. The prefix is listed once.
        assert_eq!(found.len(), 4);
        assert!(found[0].importable && found[1].importable, "importable first");

        // Not signed in: listed, but nothing can be imported or name-matched.
        let found = scan(&roots, None, &[]);
        let o = found.iter().find(|f| f.product_id == Some(3539)).unwrap();
        assert_eq!(o.owned, None);
        assert!(!o.importable && o.note.as_deref() == Some("Sign in to check that you own it"));
        assert!(found.iter().all(|f| !f.importable));
    }

    #[test]
    fn names_and_paths() {
        let lib = vec![entry(1, "Assassin's Creed® Origins"), entry(2, "Far Cry® 5"), entry(3, "Far Cry® 4")];
        assert_eq!(match_name("Assassin's Creed Origins", &lib), Some(1));
        assert_eq!(match_name("Far Cry 5", &lib), Some(2));
        assert_eq!(match_name("Far Cry", &lib), None, "ambiguous");
        assert_eq!(match_name("xx", &lib), None);
        let p = Path::new("/p");
        assert_eq!(unix_path(p, "C:/Program Files (x86)/x/").unwrap(), Path::new("/p/drive_c/Program Files (x86)/x"));
        assert_eq!(unix_path(p, "C:\\Games\\y").unwrap(), Path::new("/p/drive_c/Games/y"));
        assert!(unix_path(p, "nope").is_none());
        assert_eq!(expand(Path::new("/a/b")).unwrap(), Path::new("/a/b"));
        assert!(expand(Path::new("rel")).is_err());
    }

    /// `cargo test -p ubid scan_this_home -- --ignored --nocapture`: scan the real home
    /// (read-only) and print what it finds, without ownership.
    #[test]
    #[ignore]
    fn scan_this_home() {
        // Without Quiver's own records: this must not touch ~/.local/share/quiver.
        let t = std::time::Instant::now();
        let found = scan(&Roots::for_home(&launcher::home()), None, &[]);
        for f in &found {
            println!(
                "{:?} {} [{}] {} · {} files, {} bytes · {}",
                f.product_id,
                f.name,
                f.source_label,
                f.path.display(),
                f.files,
                f.size_bytes,
                f.note.as_deref().unwrap_or("importable")
            );
        }
        println!("{} found in {:?}", found.len(), t.elapsed());
    }
}
