//! Running installed games on the host: one Proton prefix per game, `ubi-bridge` in place
//! of Ubisoft Connect, cloud saves synced around each session.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use crate::playsession::PlaySessions;
use crate::online::Online;
use crate::cloudsave::CloudSaves;
use crate::library::{Library, ProductConfig};
use crate::{launch, savesync};

const UPC_REL: &str = "drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher";
const UPC_WIN: &str = r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher";
/// Ubisoft SDK DLLs games load from the Connect directory.
const SDK_DLLS: &[&str] = &[
    "uplay_r1.dll",
    "uplay_r164.dll",
    "uplay_r2.dll",
    "uplay_r264.dll",
    "uplay_aux_r1.dll",
    "uplay_aux_r164.dll",
    "ubiorbitapi_r2.dll",
    "ubiorbitapi_r264.dll",
    "version.txt",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Where games are installed (`<library>/<slug>`).
    pub library: PathBuf,
    /// Where per-game Wine prefixes live (`<prefixes>/<slug>`).
    pub prefixes: PathBuf,
    /// Proton build directory (contains the `proton` script).
    pub proton: PathBuf,
    /// Directory holding Ubisoft's SDK DLLs (an existing Connect install).
    pub sdk_source: PathBuf,
    /// `ubi-bridge.exe` (built for x86_64-pc-windows-gnu).
    pub bridge_exe: PathBuf,
    /// Overwritten local saves are copied here before cloud downloads replace them.
    pub save_backups: PathBuf,
    /// Extra environment for games (e.g. PROTON_ENABLE_NVAPI=1).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Report play sessions to Ubisoft (the play time shown in Connect and on the web).
    #[serde(default = "yes")]
    pub report_playtime: bool,
}

fn yes() -> bool {
    true
}

pub fn data_dir() -> PathBuf {
    static MIGRATE: std::sync::Once = std::sync::Once::new();
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"));
    let dir = base.join("quiver");
    MIGRATE.call_once(|| {
        if let Err(e) = migrate_legacy_data(&base.join("ubisoft-linux"), &dir) {
            tracing::warn!("migrating legacy data dir failed: {e:#}");
        }
    });
    dir
}

/// One-time move of the pre-Quiver data directory (`ubisoft-linux`) into `quiver`, rewriting
/// paths stored in config.json and games.json. Runs while `quiver` has no config.json yet
/// (the client may already have created it for the daemon log); entries already present in
/// `quiver` are kept and the old copies left behind.
fn migrate_legacy_data(old: &Path, new: &Path) -> Result<()> {
    if !old.is_dir() || new.join("config.json").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(new)?;
    for entry in std::fs::read_dir(old)? {
        let entry = entry?;
        let name = match entry.file_name().to_str() {
            Some("ubid.log") => "quiverd.log.old".to_owned(),
            Some(n) => n.to_owned(),
            None => continue,
        };
        let dst = new.join(&name);
        if !dst.exists() {
            std::fs::rename(entry.path(), dst)?;
        }
    }
    let _ = std::fs::remove_dir(old); // only if everything moved
    let (old_s, new_s) = (old.to_string_lossy(), new.to_string_lossy());
    for name in ["config.json", "games.json"] {
        let p = new.join(name);
        if let Ok(text) = std::fs::read_to_string(&p) {
            std::fs::write(&p, text.replace(&*old_s, &new_s))?;
        }
    }
    tracing::info!("moved {} to {}", old.display(), new.display());
    Ok(())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn compat_tools_dir() -> PathBuf {
    home().join(".local/share/Steam/compatibilitytools.d")
}

/// Installed Proton builds (directory names), oldest to newest.
pub fn list_protons() -> Vec<String> {
    find_protons().into_iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect()
}

/// A Proton build by name (directory in compatibilitytools.d) or absolute path.
pub fn resolve_proton(name: &str) -> Result<PathBuf> {
    let p = if name.starts_with('/') { PathBuf::from(name) } else { compat_tools_dir().join(name) };
    if !p.join("proton").is_file() {
        bail!("no Proton build '{name}' (installed: {})", list_protons().join(", "));
    }
    Ok(p)
}

/// Newest GE-Proton (or any Proton) in Steam's compatibilitytools.d.
fn find_proton() -> Option<PathBuf> {
    find_protons().pop()
}

fn find_protons() -> Vec<PathBuf> {
    let dir = compat_tools_dir();
    let mut builds: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("proton").is_file())
        .collect();
    // GE-Proton names sort sensibly once the numeric parts are compared numerically.
    builds.sort_by_key(|p| {
        let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let nums: Vec<u32> = name.split(|c: char| !c.is_ascii_digit()).filter_map(|s| s.parse().ok()).collect();
        (name.starts_with("GE-Proton"), nums)
    });
    builds
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.json")
    }

    /// Load the config, filling in detected defaults for a first run.
    pub fn load() -> Result<Self> {
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            return serde_json::from_str(&text).context("corrupt config.json");
        }
        let games = home().join("Games");
        let cfg = Self {
            library: games.join("ubi-library"),
            prefixes: games.join("ubi-prefixes"),
            proton: find_proton().context("no Proton found in ~/.local/share/Steam/compatibilitytools.d")?,
            sdk_source: {
                // Prefer an existing Connect install; otherwise ensure_sdk() fetches the DLLs.
                let lutris = games.join("ubisoft-connect").join(UPC_REL);
                if lutris.join("uplay_r164.dll").is_file() { lutris } else { data_dir().join("sdk") }
            },
            bridge_exe: default_bridge_exe(),
            save_backups: data_dir().join("save-backups"),
            env: vec![
                ("PROTON_ENABLE_NVAPI".into(), "1".into()),
                ("DXVK_ENABLE_NVAPI".into(), "1".into()),
            ],
            report_playtime: true,
        };
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(Self::path(), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// Next to the running binary, else the workspace build output.
fn default_bridge_exe() -> PathBuf {
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ubi-bridge.exe")));
    if let Some(p) = beside.filter(|p| p.is_file()) {
        return p;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/x86_64-pc-windows-gnu/release/ubi-bridge.exe")
}

pub use ubi_api::{Game, GameSettings};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Games {
    pub games: Vec<Game>,
}

impl Games {
    fn path() -> PathBuf {
        data_dir().join("games.json")
    }

    pub fn load() -> Result<Self> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text).context("corrupt games.json"),
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(Self::path(), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, product_id: u32) -> Option<&Game> {
        self.games.iter().find(|g| g.product_id == product_id)
    }

    pub fn upsert(&mut self, game: Game) {
        self.games.retain(|g| g.product_id != game.product_id);
        self.games.push(game);
        self.games.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

/// Directory-friendly name: "Assassin's Creed® Origins" -> "assassins-creed-origins".
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() || c == '-' || c == ':' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// Build a `Game` for a freshly installed product.
pub async fn register(cfg: &Config, lib: &mut Library, product_id: u32, install_dir: &Path, language: &str) -> Result<Game> {
    let product = lib.get(product_id).context("not in library")?.clone();
    let config = match product.configuration.clone() {
        Some(c) => c,
        None => lib.product_config(product_id).await?,
    };
    let exe = ProductConfig::parse(&config)?
        .executables()
        .into_iter()
        .next()
        .context("product config lists no executable")?;
    let name = product.name.clone().unwrap_or_else(|| product_id.to_string());
    Ok(Game {
        product_id,
        prefix: cfg.prefixes.join(slug(&name)),
        name,
        install_dir: install_dir.to_owned(),
        exe: match &exe.working_dir {
            Some(dir) => format!("{}/{}", dir.trim_end_matches(['\\', '/']), exe.path),
            None => exe.path.clone(),
        }
        .replace('\\', "/"),
        args: exe.arguments,
        language: Some(language.to_owned()),
    })
}

/// Install (or update) a product into `<library>/<slug>` and register it.
pub async fn install_game(
    cfg: &Config,
    online: &Online,
    product_id: u32,
    language: &str,
    progress: &crate::install::Progress,
    status: impl Fn(String),
) -> Result<Game> {
    use crate::{download::Downloader, install, manifest};
    status("Fetching manifest…".into());
    let demux = online.demux().await?;
    let mut lib = Library::fetch(&demux).await?;
    let product = lib.get(product_id).context("not in your library")?.clone();
    let hash = product.latest_manifest.clone().context("product has no PC build")?;
    let name = product.name.clone().unwrap_or_else(|| product_id.to_string());
    let dest = Games::load()?
        .get(product_id)
        .map(|g| g.install_dir.clone())
        .unwrap_or_else(|| cfg.library.join(slug(&name)));
    let mut dl = Downloader::open(&demux, &mut lib, product_id).await?;
    let (new_manifest, raw) = dl.manifest(&hash).await?;
    let plan = install::plan(&new_manifest, language)?;
    progress.total.store(plan.download_bytes, std::sync::atomic::Ordering::Relaxed);
    let installed_path = dest.join("uplay_install.manifest");
    if let Ok(old_raw) = std::fs::read(&installed_path) {
        if !manifest::manifest_hash(&old_raw)?.eq_ignore_ascii_case(&hash) {
            let old_plan = install::plan(&manifest::parse_manifest(&old_raw)?, language)?;
            let (kept, removed) = install::prepare_update(&old_plan, &plan, &dest)?;
            status(format!("Updating ({kept} files unchanged, {removed} removed)…"));
        }
    }
    status(format!("Downloading {:.1} GB…", plan.download_bytes as f64 / 1e9));
    std::fs::create_dir_all(&dest)?;
    install::install(&mut dl, &plan, &dest, new_manifest.is_compressed.unwrap_or(false), progress).await?;
    std::fs::write(&installed_path, &raw)?;
    let game = register(cfg, &mut lib, product_id, &dest.canonicalize()?, language).await?;
    let mut games = Games::load()?;
    games.upsert(game.clone());
    games.save()?;
    status("Installed".into());
    Ok(game)
}

/// Result of importing an existing install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub game: Game,
    pub files: usize,
    /// Files in the source folder that the manifest doesn't list (mods, logs, configs…).
    pub extras: Vec<String>,
    pub update_available: bool,
}

/// Import an existing install (e.g. from a Lutris/Connect prefix) into the library.
/// Only files listed in its `uplay_install.manifest` are copied, as reflinks when the
/// filesystem supports them; the source is left untouched.
pub async fn import_game(
    cfg: &Config,
    online: &Online,
    product_id: u32,
    source: &Path,
    language: &str,
    status: impl Fn(String),
) -> Result<ImportReport> {
    use crate::{install, manifest};
    let raw = std::fs::read(source.join("uplay_install.manifest"))
        .with_context(|| format!("{} has no uplay_install.manifest", source.display()))?;
    let parsed = manifest::parse_manifest(&raw)?;
    let demux = online.demux().await?;
    let mut lib = Library::fetch(&demux).await?;
    let product = lib.get(product_id).context("not in your library")?.clone();
    let name = product.name.clone().unwrap_or_else(|| product_id.to_string());
    // Every language's files may be present; take all chunks the source actually has.
    let mut plan = install::plan(&parsed, language)?;
    let all = install::plan(&parsed, "*")?;
    for f in all.files {
        if !plan.files.iter().any(|p| p.path == f.path) && source.join(&f.path).is_file() {
            plan.files.push(f);
        }
    }
    let dest = cfg.library.join(slug(&name));
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let listed: std::collections::HashSet<String> = plan.files.iter().map(|f| f.path.to_lowercase()).collect();
    let mut done = String::new();
    for (i, f) in plan.files.iter().enumerate() {
        let src = source.join(&f.path);
        let meta = src.metadata().with_context(|| format!("missing {}", f.path))?;
        if meta.len() != f.size {
            bail!("{}: size {} differs from manifest ({})", f.path, meta.len(), f.size);
        }
        let dst = dest.join(&f.path);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        let ok = tokio::process::Command::new("cp")
            .args(["--reflink=auto", "--preserve=timestamps"])
            .arg(&src)
            .arg(&dst)
            .status()
            .await?
            .success();
        if !ok {
            bail!("copying {} failed", f.path);
        }
        done.push_str(&f.path);
        done.push('\n');
        if i % 50 == 0 {
            status(format!("Copying files {}/{}…", i + 1, plan.files.len()));
        }
    }
    for d in &plan.dirs {
        std::fs::create_dir_all(dest.join(d))?;
    }
    std::fs::write(dest.join("uplay_install.manifest"), &raw)?;
    std::fs::create_dir_all(dest.join(".ubi-install"))?;
    std::fs::write(dest.join(".ubi-install/done"), done)?;
    let mut extras = Vec::new();
    collect_extras(source, source, &listed, &mut extras);
    extras.sort();
    let game = register(cfg, &mut lib, product_id, &dest.canonicalize()?, language).await?;
    let mut games = Games::load()?;
    games.upsert(game.clone());
    games.save()?;
    let update_available = update_available(&game, &lib);
    status(format!("Imported {} files", plan.files.len()));
    Ok(ImportReport { game, files: plan.files.len(), extras, update_available })
}

fn collect_extras(root: &Path, dir: &Path, listed: &std::collections::HashSet<String>, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        if p.is_dir() {
            collect_extras(root, &p, listed, out);
        } else if !listed.contains(&rel.to_lowercase()) && rel != "uplay_install.manifest" && rel != "uplay_install.state" {
            out.push(rel);
        }
    }
}

/// Remove a game: its files, and optionally its prefix (which holds local saves and settings).
pub fn uninstall(product_id: u32, remove_prefix: bool) -> Result<Game> {
    let mut games = Games::load()?;
    let game = games.get(product_id).cloned().context("not installed")?;
    if game.install_dir.exists() {
        std::fs::remove_dir_all(&game.install_dir)?;
    }
    if remove_prefix && game.prefix.exists() {
        std::fs::remove_dir_all(&game.prefix)?;
    }
    games.games.retain(|g| g.product_id != product_id);
    games.save()?;
    Ok(game)
}

/// Whether a newer build than the installed one is available.
pub fn update_available(game: &Game, lib: &Library) -> bool {
    let Some(latest) = lib.get(game.product_id).and_then(|p| p.latest_manifest.clone()) else { return false };
    std::fs::read(game.install_dir.join("uplay_install.manifest"))
        .ok()
        .and_then(|raw| crate::manifest::manifest_hash(&raw).ok())
        .is_some_and(|h| !h.eq_ignore_ascii_case(&latest))
}

/// Host path -> Windows path through Wine's Z: drive.
pub fn win_path(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

struct Proton<'a> {
    cfg: &'a Config,
    prefix: &'a Path,
    /// Proton build to use (per-game override or the configured default).
    build: PathBuf,
}

impl Proton<'_> {
    fn command(&self, args: &[&str]) -> Command {
        self.wrapped(&[], args)
    }

    /// `wrappers... python3 <proton> run args...`
    fn wrapped(&self, wrappers: &[String], args: &[&str]) -> Command {
        let mut cmd = match wrappers.split_first() {
            Some((first, rest)) => {
                let mut c = Command::new(first);
                c.args(rest).arg("python3");
                c
            }
            None => Command::new("python3"),
        };
        cmd.arg(self.build.join("proton")).arg("run").args(args);
        let steam = data_dir().join("steam-stub");
        let _ = std::fs::create_dir_all(&steam);
        cmd.env("STEAM_COMPAT_DATA_PATH", self.prefix)
            .env("WINEPREFIX", self.prefix)
            .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam)
            .env("STEAM_COMPAT_APP_ID", "0")
            .env("SteamAppId", "0")
            .env("SteamGameId", "0");
        for (k, v) in &self.cfg.env {
            cmd.env(k, v);
        }
        cmd
    }

    fn wineserver(&self) -> Command {
        let mut cmd = Command::new(self.build.join("files/bin/wineserver"));
        cmd.env("WINEPREFIX", self.prefix);
        cmd
    }

    async fn run_to_end(&self, args: &[&str], log: &Path) -> Result<()> {
        let out = std::fs::File::create(log)?;
        let status = self.command(args).stdout(out.try_clone()?).stderr(out).status().await?;
        let _ = self.wineserver().arg("-w").status().await;
        if !status.success() {
            bail!("proton {args:?} failed ({status}); see {}", log.display());
        }
        Ok(())
    }
}

const CONNECT_INSTALLER: &str = "https://static3.cdn.ubi.com/orbit/launcher_installer/UbisoftConnectInstaller.exe";

/// Make sure `cfg.sdk_source` holds Ubisoft's SDK DLLs, extracting them from the public
/// Ubisoft Connect installer (an NSIS archive, read with `7z`) if needed. Nothing is
/// installed or run; only the DLLs games load are kept.
pub async fn ensure_sdk(cfg: &Config) -> Result<()> {
    if cfg.sdk_source.join("uplay_r164.dll").is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(&cfg.sdk_source)?;
    let installer = cfg.sdk_source.join("UbisoftConnectInstaller.exe");
    let bytes = reqwest::get(CONNECT_INSTALLER).await?.error_for_status()?.bytes().await?;
    std::fs::write(&installer, &bytes)?;
    let mut cmd = Command::new("7z");
    cmd.args(["e", "-y", "-bso0", "-bsp0"]).arg(format!("-o{}", cfg.sdk_source.display())).arg(&installer).args(SDK_DLLS);
    let status = cmd.status().await.context("running 7z (install p7zip)")?;
    let _ = std::fs::remove_file(&installer);
    if !status.success() || !cfg.sdk_source.join("uplay_r164.dll").is_file() {
        bail!("could not extract the SDK DLLs from the Ubisoft Connect installer");
    }
    Ok(())
}

/// Create the game's prefix if needed: a fresh Proton prefix plus Ubisoft's SDK DLLs in a
/// Connect-shaped directory and the registry key the SDK loader uses to find them.
pub async fn ensure_prefix(cfg: &Config, game: &Game) -> Result<()> {
    let marker = game.prefix.join(".ubi-prefix-ready");
    if marker.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&game.prefix)?;
    let pfx_link = game.prefix.join("pfx");
    if !pfx_link.exists() {
        // Proton initializes <compat>/pfx; pointing it at the root keeps a plain Wine layout.
        std::os::unix::fs::symlink(".", &pfx_link)?;
    }
    ensure_sdk(cfg).await?;
    let proton = Proton { cfg, prefix: &game.prefix, build: cfg.proton.clone() };
    let log = game.prefix.join("ubi-setup.log");
    proton.run_to_end(&["wineboot", "-u"], &log).await?;
    let upc = game.prefix.join(UPC_REL);
    std::fs::create_dir_all(&upc)?;
    for dll in SDK_DLLS {
        let src = cfg.sdk_source.join(dll);
        if src.is_file() {
            std::fs::copy(&src, upc.join(dll)).with_context(|| format!("copy {}", src.display()))?;
        }
    }
    let key = r"HKLM\Software\Wow6432Node\Ubisoft\Launcher";
    proton
        .run_to_end(&["reg", "add", key, "/v", "InstallDir", "/d", &format!(r"{UPC_WIN}\"), "/f"], &log)
        .await?;
    std::fs::write(marker, "")?;
    Ok(())
}

#[derive(Debug, Clone)]
pub enum LaunchEvent {
    Status(String),
    Running,
    Exited(Option<i32>),
}

/// Arguments Connect hands a game it starts (read by `uplay_aux_r164.dll`).
fn uplay_arguments(product_id: u32, exe: &str, workdir: &str) -> String {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    format!(
        "-upc_uplay_id {product_id} -upc_game_version 1 -upc_exe_path {} -upc_working_directory {}",
        b64(exe),
        b64(workdir)
    )
}

/// Wait for the bridge to report that it owns its pipes.
async fn wait_for_bridge(log: &Path, bridge: &mut Child) -> Result<()> {
    for _ in 0..150 {
        if std::fs::read_to_string(log).is_ok_and(|l| l.contains("uplay_ipc_pipe")) {
            return Ok(());
        }
        if let Some(status) = bridge.try_wait()? {
            bail!("ubi-bridge exited early ({status}); see {}", log.display());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("ubi-bridge did not start; see {}", log.display())
}

/// Launch a game and wait for it to exit. `broker` is the address of a running broker.
pub async fn play(
    cfg: &Config,
    online: &Online,
    game: &Game,
    settings: &GameSettings,
    broker: &str,
    sessions: &PlaySessions,
    events: impl Fn(LaunchEvent),
) -> Result<()> {
    let build = match &settings.proton {
        Some(name) => resolve_proton(name)?,
        None => cfg.proton.clone(),
    };
    events(LaunchEvent::Status("Preparing prefix…".into()));
    ensure_prefix(cfg, game).await?;
    if !cfg.bridge_exe.is_file() {
        bail!("ubi-bridge.exe not found at {}", cfg.bridge_exe.display());
    }
    let run_dir = game.prefix.join("ubi-run");
    std::fs::create_dir_all(&run_dir)?;

    events(LaunchEvent::Status("Signing in…".into()));
    let session = online.session().await?;
    let demux = online.demux().await?;
    let mut lib = Library::fetch(&demux).await?;
    let mut ctx = launch::build(&online.auth, &session, &mut lib, game.product_id).await?;
    let saves_rel = format!("savegames/{}/{}", session.user_id, game.product_id);
    let saves_dir = game.prefix.join(UPC_REL).join(&saves_rel);
    ctx.savegame_path = Some(format!(r"{UPC_WIN}\{}", saves_rel.replace('/', "\\")));
    ctx.broker = Some(broker.to_owned());
    ctx.chunks = launch::installed_chunks(&game.install_dir).unwrap_or_default();
    ctx.language = settings.language.clone().or(game.language.clone()).or(ctx.language);
    let ctx_path = run_dir.join("context.json");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&ctx_path)?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec(&ctx)?)?;
    }

    events(LaunchEvent::Status("Syncing cloud saves…".into()));
    let mut cloud = CloudSaves::open(&demux).await?;
    let pulled = savesync::pull(&mut cloud, &mut lib, game.product_id, &saves_dir, &cfg.save_backups).await?;
    tracing::info!(?pulled.downloaded, ?pulled.kept_local, "saves pulled");

    events(LaunchEvent::Status("Starting…".into()));
    let proton = Proton { cfg, prefix: &game.prefix, build };
    let bridge_dst = game.prefix.join("drive_c/ubi/ubi-bridge.exe");
    std::fs::create_dir_all(bridge_dst.parent().unwrap())?;
    std::fs::copy(&cfg.bridge_exe, &bridge_dst)?;
    let bridge_log = run_dir.join("bridge.log");
    let _ = std::fs::remove_file(&bridge_log);
    let mut bridge = proton
        .command(&[r"C:\ubi\ubi-bridge.exe", &win_path(&ctx_path)])
        .env("UBI_BRIDGE_LOG", win_path(&bridge_log))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(run_dir.join("bridge-proton.log"))?)
        .kill_on_drop(true)
        .spawn()?;
    wait_for_bridge(&bridge_log, &mut bridge).await?;

    let exe = game.install_dir.join(&game.exe);
    let exe_win = win_path(&exe);
    let workdir_win = win_path(exe.parent().unwrap_or(&game.install_dir));
    let mut args = vec![exe_win.as_str()];
    let extra: Vec<String> = game.args.as_deref().map(|a| a.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
    args.extend(extra.iter().map(String::as_str));
    args.extend(settings.args.iter().map(String::as_str));
    let mut cmd = proton.wrapped(&settings.wrappers, &args);
    for (k, v) in &settings.env {
        cmd.env(k, v);
    }
    if !settings.dll_overrides.is_empty() {
        let overrides: Vec<String> = settings.dll_overrides.iter().map(|(k, v)| format!("{k}={v}")).collect();
        cmd.env("WINEDLLOVERRIDES", overrides.join(";"));
    }
    cmd.env("UPLAY_ARGUMENTS", uplay_arguments(game.product_id, &exe_win, &workdir_win))
        .current_dir(exe.parent().unwrap_or(&game.install_dir))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(run_dir.join("game.log"))?);
    let mut child = cmd.spawn()?;
    // Don't wait on `proton run`: its steam.exe wrapper waits for every process in the
    // prefix, including the bridge, so it never returns while the bridge is up. Watch the
    // game's own processes instead.
    let started = wait_for_game_start(&proton, &mut child).await;
    events(LaunchEvent::Running);
    if started {
        // The bridge holds a lease too once the SDK initializes; both share one session.
        let _lease = sessions.begin(game.product_id, "launcher");
        while !game_processes(&proton).is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    let status = child.try_wait().ok().flatten();
    let _ = child.start_kill();
    let _ = bridge.start_kill();
    let _ = proton.wineserver().arg("-k").status().await;
    let _ = child.wait().await;
    let _ = bridge.wait().await;
    let status = status.unwrap_or_default();
    events(LaunchEvent::Exited(status.code()));

    events(LaunchEvent::Status("Uploading cloud saves…".into()));
    let pushed = savesync::push(&mut cloud, &mut lib, game.product_id, &saves_dir).await?;
    tracing::info!(?pushed.uploaded, "saves pushed");
    events(LaunchEvent::Status(format!("Done ({} save slots uploaded)", pushed.uploaded.len())));
    Ok(())
}

/// Ask a running game to stop by shutting its prefix down.
pub async fn stop(cfg: &Config, game: &Game) -> Result<()> {
    let settings = crate::settings::current(game.product_id)?.0;
    let build = match &settings.proton {
        Some(name) => resolve_proton(name)?,
        None => cfg.proton.clone(),
    };
    let proton = Proton { cfg, prefix: &game.prefix, build };
    proton.wineserver().arg("-k").status().await?;
    Ok(())
}

/// Game processes in the prefix: Windows executables other than the bridge, Wine's own
/// services and Proton's wrappers.
fn game_processes(proton: &Proton<'_>) -> Vec<u32> {
    let prefix = format!("WINEPREFIX={}", proton.prefix.display());
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let pid = e.file_name().to_string_lossy().parse::<u32>().ok()?;
            let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
            let in_prefix = env.split(|&b| b == 0).any(|kv| kv.starts_with(prefix.as_bytes()));
            let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            let cmd = String::from_utf8_lossy(&cmd).to_lowercase();
            let is_game = cmd.contains(".exe")
                && !cmd.contains("ubi-bridge")
                && !cmd.contains("\\windows\\")
                && !cmd.contains("proton");
            (in_prefix && is_game).then_some(pid)
        })
        .collect()
}

/// Wait (up to two minutes) for the game's process to appear. False if `proton run` gave up first.
async fn wait_for_game_start(proton: &Proton<'_>, child: &mut Child) -> bool {
    for _ in 0..240 {
        if !game_processes(proton).is_empty() {
            return true;
        }
        if child.try_wait().ok().flatten().is_some() {
            return !game_processes(proton).is_empty();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}
