//! Running installed games on the host: one Proton prefix per game, `quiver-bridge` in place
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

pub const UPC_REL: &str = "drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher";
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
    /// `quiver-bridge.exe` (built for x86_64-pc-windows-gnu), when set by hand. Unset (the
    /// default) means it is looked up at each launch, so one config works for a dev build
    /// and the package; see [`Config::bridge_exe`].
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "bridge_override")]
    pub bridge_exe: Option<PathBuf>,
    /// Overwritten local saves are copied here before cloud downloads replace them.
    pub save_backups: PathBuf,
    /// Extra environment for games (e.g. PROTON_ENABLE_NVAPI=1).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Report play sessions to Ubisoft (the play time shown in Connect and on the web).
    #[serde(default = "yes")]
    pub report_playtime: bool,
    /// Steam user (account id, `userdata/<id>`) that gets Steam shortcuts, chosen on the
    /// first export when the machine has several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_user: Option<String>,
    /// Queue, bandwidth and auto-update settings.
    #[serde(default)]
    pub downloads: quiver_api::downloads::DownloadSettings,
    /// Mods v2: apply enabled built-in `fix` mods to vanilla launches too. `None`: not
    /// decided yet (a first-launch prompt asks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes_on_vanilla: Option<bool>,
    /// First-run setup, once done or skipped (every front end's welcome flow checks it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run: Option<quiver_api::setup::FirstRun>,
    /// Offer to start a Ubisoft game newly installed in Steam through Quiver instead of
    /// Ubisoft Connect (`exports::steamgames`).
    #[serde(default = "yes")]
    pub steam_offers: bool,
    /// Keep Epic cloud saves fresh between sessions: quiverd checks the cloud now and then
    /// and downloads what's newer there, so games are ready to play offline (never
    /// uploads on its own; design/STORE-EPIC.md §9.12).
    #[serde(default = "yes")]
    pub epic_saves_background: bool,
}

fn yes() -> bool {
    true
}

/// `new`, after moving `old` there if only `old` exists: Quiver's own files in a prefix or
/// install folder that were named `ubi-*` before the rename to Quiver. If the move fails the
/// old path is used as it is (read old, write new; see [`quiver_api::renamed`]).
pub fn migrate_renamed(new: PathBuf, old: &Path) -> PathBuf {
    if new.symlink_metadata().is_err()
        && old.symlink_metadata().is_ok()
        && let Err(e) = std::fs::rename(old, &new)
    {
        tracing::warn!("moving {} to {} failed: {e}", old.display(), new.display());
        return old.to_path_buf();
    }
    new
}

/// The per-launch logs and state in a game's prefix (`quiver-run`; `ubi-run` before the
/// rename, moved on first use).
pub fn run_dir(prefix: &Path) -> PathBuf {
    migrate_renamed(prefix.join("quiver-run"), &prefix.join("ubi-run"))
}

/// Where the bridge goes in a prefix (`C:\quiver\quiver-bridge.exe`), relative to the prefix.
pub const PREFIX_BRIDGE: &str = "drive_c/quiver/quiver-bridge.exe";
/// The bridge's path in a prefix before the rename (`C:\ubi\ubi-bridge.exe`), kept as a link
/// to [`PREFIX_BRIDGE`] for existing launch scripts, Steam launch options and containers.
pub const LEGACY_PREFIX_BRIDGE: &str = "drive_c/ubi/ubi-bridge.exe";

/// Copy the bridge into the prefix at [`PREFIX_BRIDGE`] and point [`LEGACY_PREFIX_BRIDGE`]
/// at it (a relative symlink, so it holds wherever the prefix is mounted). The link is best
/// effort: failing it only affects scripts that still name the old path.
pub fn install_prefix_bridge(prefix: &Path, bridge_exe: &Path) -> Result<PathBuf> {
    let dst = prefix.join(PREFIX_BRIDGE);
    std::fs::create_dir_all(dst.parent().unwrap())?;
    std::fs::copy(bridge_exe, &dst).with_context(|| format!("copy {} to {}", bridge_exe.display(), dst.display()))?;
    let legacy = prefix.join(LEGACY_PREFIX_BRIDGE);
    let target = Path::new("../quiver/quiver-bridge.exe");
    if std::fs::read_link(&legacy).ok().as_deref() != Some(target) {
        let linked = std::fs::create_dir_all(legacy.parent().unwrap())
            .and_then(|()| match std::fs::remove_file(&legacy) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            })
            .and_then(|()| std::os::unix::fs::symlink(target, &legacy));
        if let Err(e) = linked {
            tracing::warn!("linking {} to the bridge failed: {e}", legacy.display());
        }
    }
    Ok(dst)
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

pub fn home() -> PathBuf {
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
            // Folders from before the rename to Quiver are kept when they are there.
            library: quiver_api::renamed(games.join("quiver-library"), &games.join("ubi-library")),
            prefixes: quiver_api::renamed(games.join("quiver-prefixes"), &games.join("ubi-prefixes")),
            proton: find_proton().context("no Proton found in ~/.local/share/Steam/compatibilitytools.d")?,
            sdk_source: {
                // Prefer an existing Connect install; otherwise ensure_sdk() fetches the DLLs.
                let lutris = games.join("ubisoft-connect").join(UPC_REL);
                if lutris.join("uplay_r164.dll").is_file() { lutris } else { data_dir().join("sdk") }
            },
            bridge_exe: None,
            save_backups: data_dir().join("save-backups"),
            env: vec![
                ("PROTON_ENABLE_NVAPI".into(), "1".into()),
                ("DXVK_ENABLE_NVAPI".into(), "1".into()),
            ],
            report_playtime: true,
            steam_user: None,
            downloads: Default::default(),
            fixes_on_vanilla: None,
            first_run: None,
            steam_offers: true,
            epic_saves_background: true,
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

impl Config {
    /// The bridge to copy into the prefix: the override in config.json if there is one,
    /// else the first of [`bridge_candidates`] that exists.
    pub fn bridge_exe(&self) -> Result<PathBuf> {
        match &self.bridge_exe {
            Some(p) if p.is_file() => Ok(p.clone()),
            Some(p) => bail!("quiver-bridge.exe not found at {} (bridge_exe in config.json)", p.display()),
            None => {
                let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
                let candidates = bridge_candidates(exe_dir.as_deref());
                find_bridge_exe(&candidates, Path::is_file).with_context(|| {
                    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
                    format!("quiver-bridge.exe not found (looked for {})", tried.join(", "))
                })
            }
        }
    }
}

/// The cross build in this checkout (what `cargo build --release -p quiver-bridge --target
/// x86_64-pc-windows-gnu` writes).
fn dev_bridge_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/x86_64-pc-windows-gnu/release/quiver-bridge.exe")
}

/// Where `quiver-bridge.exe` may be, best first, for a binary in `exe_dir`: beside it, then
/// `../lib/quiver/` from it (the package: `/usr/bin/quiverd` -> `/usr/lib/quiver/`), then
/// the checkout's cross build (a dev binary in `target/release`).
pub fn bridge_candidates(exe_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = exe_dir {
        out.push(dir.join("quiver-bridge.exe"));
        if let Some(prefix) = dir.parent() {
            out.push(prefix.join("lib/quiver/quiver-bridge.exe"));
        }
    }
    out.push(dev_bridge_exe());
    out
}

/// The first of `candidates` that `exists`.
fn find_bridge_exe(candidates: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|p| exists(p)).cloned()
}

/// `bridge_exe` as stored. Configs written before it became optional hold the automatic
/// pick of the time, the checkout's cross build spelled
/// `<checkout>/crates/ubi-core/../../target/.../ubi-bridge.exe` (before the crates were
/// renamed to `quiver-*`, so it never names `quiver-core`); that reads as unset, so the package uses
/// its own bridge rather than a dev build's (and a dev build still finds its own). Any other
/// path is an override someone chose and is kept.
fn bridge_override<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<PathBuf>, D::Error> {
    let p = Option::<PathBuf>::deserialize(d)?;
    Ok(p.filter(|p| !is_legacy_default_bridge(p)))
}

fn is_legacy_default_bridge(p: &Path) -> bool {
    p.ends_with("crates/ubi-core/../../target/x86_64-pc-windows-gnu/release/ubi-bridge.exe")
}

pub use quiver_api::{Game, GameSettings};

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
        game: None,
    })
}

/// Install (or update) a product into `<library>/<slug>` and register it (see
/// [`crate::builds`]: resumes, updates in place, checks free space first).
pub use crate::builds::install_game;

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
    let state = crate::install::state_dir(&dest);
    std::fs::create_dir_all(&state)?;
    std::fs::write(state.join("done"), done)?;
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
            .env("SteamGameId", "0")
            // Keep compiled shaders: the NVIDIA driver otherwise trims its disk cache to
            // 1 GB, evicting a big game's shaders between sessions (multi-second stalls when
            // they recompile). Steam sets the same. Config and game env can override.
            .env("__GL_SHADER_DISK_CACHE", "1")
            .env("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP", "1")
            .env("MESA_SHADER_CACHE_MAX_SIZE", "10G");
        // Steam Input: don't let Wine read the physical controllers Steam already mirrors
        // as its virtual pad (doubled inputs). Config and game env still override.
        for (k, v) in steam_input_env() {
            cmd.env(k, v);
        }
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
    let marker = migrate_renamed(game.prefix.join(".quiver-prefix-ready"), &game.prefix.join(".ubi-prefix-ready"));
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
    let log = game.prefix.join("quiver-setup.log");
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
    /// Lines worth keeping with the launch (config patches applied, pending or undone).
    Detail(Vec<String>),
    /// The game's processes (host pids), when they change (options write through them).
    Pids(Vec<u32>),
}

/// One line per config patch status, for a launch's job detail.
pub fn describe_patches(statuses: &[quiver_api::mods::PatchStatus]) -> Vec<String> {
    use quiver_api::mods::PatchState;
    statuses
        .iter()
        .map(|s| {
            let state = match s.state {
                PatchState::Applied => "applied",
                PatchState::Unchanged => "already set",
                PatchState::Pending => "pending until first run",
                PatchState::Undone => "undone",
                PatchState::Failed => "FAILED",
                PatchState::Unknown => "not checked",
            };
            let file = s.files.first().map_or(s.file.as_str(), |f| f.rel.as_str());
            let detail = if s.detail.is_empty() || s.state == PatchState::Pending { String::new() } else { format!(" ({})", s.detail) };
            format!("config {}: {file} [{}] {state}{detail}", s.mod_id, s.target)
        })
        .collect()
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
            bail!("quiver-bridge exited early ({status}); see {}", log.display());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("quiver-bridge did not start; see {}", log.display())
}

/// Where a profile's SDK saves lived before save stores, relative to the Connect directory
/// in the prefix: Connect's own `savegames/<user>/<product>`, or a sibling folder of the
/// profile's own. Launches now use their save store's folder (`Plan::saves_folder`, see
/// [`crate::savestores`]); a store made from such a folder keeps it.
pub fn saves_rel(user_id: &str, product_id: u32, profile: Option<&str>) -> String {
    match profile {
        None => format!("savegames/{user_id}/{product_id}"),
        Some(id) => format!("savegames/{user_id}/{product_id}-{id}"),
    }
}

/// Launch a game as `plan` says (see [`crate::mods::ModStore::plan`]) and wait for it to
/// exit. `broker` is the address of a running broker. With mounts (a mod profile, or
/// vanilla with fixes that change files), the bridge and the game run in a user + mount
/// namespace where the mods are mounted over the install folder and the profile's own
/// prefix folders over the prefix's (see [`crate::overlay`]). Config patches are applied
/// just before, to the files as the launch will see them. Saves are the profile's own or
/// vanilla's as the plan says, synced with the cloud as it says.
#[allow(clippy::too_many_arguments)]
pub async fn play(
    cfg: &Config,
    online: &Online,
    game: &Game,
    plan: &crate::mods::Plan,
    broker: &str,
    sessions: &PlaySessions,
    events: impl Fn(LaunchEvent),
) -> Result<()> {
    let settings = &plan.api.settings;
    let profile = plan.api.profile.as_ref();
    let build = match &settings.proton {
        Some(name) => resolve_proton(name)?,
        None => cfg.proton.clone(),
    };
    events(LaunchEvent::Status("Preparing prefix…".into()));
    ensure_prefix(cfg, game).await?;
    let bridge_exe = cfg.bridge_exe()?;
    let run_dir = run_dir(&game.prefix);
    std::fs::create_dir_all(&run_dir)?;

    events(LaunchEvent::Status("Signing in…".into()));
    let session = online.session().await?;
    let demux = online.demux().await?;
    let mut lib = Library::fetch(&demux).await?;
    let mut ctx = launch::build(&online.auth, &session, &mut lib, game.product_id).await?;
    let saves_rel = format!("savegames/{}/{}", session.user_id, plan.saves_folder);
    let cloud_sync = plan.api.saves.cloud;
    let saves_dir = game.prefix.join(UPC_REL).join(&saves_rel);
    ctx.savegame_path = Some(format!(r"{UPC_WIN}\{}", saves_rel.replace('/', "\\")));
    ctx.broker = Some(broker.to_owned());
    ctx.chunks = launch::installed_chunks(&game.install_dir).unwrap_or_default();
    ctx.language = settings.language.clone().or(game.language.clone()).or(ctx.language);
    ctx.profile = profile.map(|p| p.id.clone());
    // One session id for the whole launch: the daemon's (Options) id when it assigned one, so
    // the bridge, the mod pipe and Options all name the same session.
    if let Some(s) = plan.session.as_ref().filter(|s| quiver_proto::modsdk::is_session_id(s)) {
        ctx.session = Some(s.clone());
        ctx.mods_pipe = Some(quiver_proto::modsdk::pipe_name(Some(s)));
    }
    let ctx_path = run_dir.join("context.json");
    crate::private::write_private(&ctx_path, &serde_json::to_vec(&ctx)?)?;

    let mut cloud = if cloud_sync {
        events(LaunchEvent::Status("Syncing cloud saves…".into()));
        let mut cloud = CloudSaves::open(&demux).await?;
        let pulled = savesync::pull(&mut cloud, &mut lib, game.product_id, &saves_dir, &cfg.save_backups).await?;
        tracing::info!(?pulled.downloaded, ?pulled.kept_local, "saves pulled");
        Some(cloud)
    } else {
        tracing::info!(saves = %saves_dir.display(), "cloud saves off for this profile");
        None
    };

    // A wineserver left over from an earlier session would serve this one with its own
    // view of the install folder (it opens files for its clients), so start clean, using
    // the build that started it (wineservers only talk to their own version).
    let previous = Proton { cfg, prefix: &game.prefix, build: session_build(game).unwrap_or_else(|| build.clone()) };
    let _ = previous.wineserver().arg("-k").status().await;
    let _ = previous.wineserver().arg("-w").status().await;
    record_session_build(game, &build)?;
    let proton = Proton { cfg, prefix: &game.prefix, build };
    // Config patches go to the files as this launch will see them, before anything is
    // mounted (never under a live overlay). A broken record is reported, not fatal.
    if !plan.patches.is_empty() {
        events(LaunchEvent::Status("Applying config patches…".into()));
    }
    match plan.apply_patches() {
        Ok(st) => {
            tracing::info!(?st, "config patches");
            if !st.is_empty() {
                events(LaunchEvent::Detail(describe_patches(&st)));
            }
        }
        Err(e) => {
            tracing::warn!("config patches: {e:#}");
            events(LaunchEvent::Detail(vec![format!("config patches not applied: {e:#}")]));
        }
    }
    let ns = if plan.overlays.is_empty() {
        None
    } else {
        let what = profile.map_or_else(|| "fixes".to_owned(), |p| p.name.clone());
        events(LaunchEvent::Status(format!("Mounting mods ({what})…")));
        for o in &plan.overlays {
            // A profile's own prefix folder may not exist yet (the game hasn't run).
            std::fs::create_dir_all(&o.target)?;
        }
        tracing::info!(profile = ?profile.map(|p| &p.id), mounts = ?plan.api.mounts, "mounting overlays");
        Some(crate::overlay::Namespace::create_many(&plan.overlays)?)
    };

    events(LaunchEvent::Status("Starting…".into()));
    install_prefix_bridge(&game.prefix, &bridge_exe)?;
    let bridge_log = run_dir.join("bridge.log");
    let _ = std::fs::remove_file(&bridge_log);
    let mut bridge = proton.command(&[r"C:\quiver\quiver-bridge.exe", &win_path(&ctx_path)]);
    if let Some(ns) = &ns {
        // First into the namespace: its wineserver must start in there.
        ns.enter(bridge.as_std_mut(), None)?;
    }
    if let Some(s) = &plan.session {
        bridge.env("QUIVER_SESSION", s);
    }
    // The bridge is the prefix's first Wine process, so the wineserver and Wine's services
    // (winedevice's controller bus, …) start with *its* environment. Give it the game's
    // env settings, or settings such as PROTON_DISABLE_HIDRAW never reach winebus.
    for (k, v) in &settings.env {
        bridge.env(k, v);
    }
    let mut bridge = bridge
        .env("QUIVER_BRIDGE_LOG", win_path(&bridge_log))
        // The name bridges before the rename read (a `bridge_exe` override may be one).
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
    let workdir = exe.parent().unwrap_or(&game.install_dir);
    if let Some(ns) = &ns {
        ns.enter(cmd.as_std_mut(), Some(workdir))?;
    }
    if let Some(p) = profile {
        cmd.env("QUIVER_PROFILE", &p.id);
    }
    // The bridge is the game's sibling here, not its parent, so the game learns the mod
    // pipe the bridge created (from the same context) from us.
    if let Some(s) = &ctx.session {
        cmd.env("QUIVER_SESSION", s);
    }
    if let Some(pipe) = &ctx.mods_pipe {
        cmd.env("QUIVER_MODS_PIPE", pipe);
    }
    cmd.env("UPLAY_ARGUMENTS", uplay_arguments(game.product_id, &exe_win, &workdir_win))
        .current_dir(workdir)
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
        let _lease = sessions.begin(online.user_id(), game.product_id, crate::playsession::LAUNCHER_HOLDER);
        let mut pids = game_processes(&proton);
        events(LaunchEvent::Pids(pids.clone()));
        while !pids.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let now = game_processes(&proton);
            if now != pids {
                events(LaunchEvent::Pids(now.clone()));
            }
            pids = now;
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

    drop(ns);
    let Some(cloud) = cloud.as_mut() else {
        events(LaunchEvent::Status("Done (cloud saves off for this profile)".into()));
        return Ok(());
    };
    events(LaunchEvent::Status("Uploading cloud saves…".into()));
    let pushed = savesync::push(cloud, &mut lib, game.product_id, &saves_dir).await?;
    tracing::info!(?pushed.uploaded, "saves pushed");
    events(LaunchEvent::Status(format!("Done ({} save slots uploaded)", pushed.uploaded.len())));
    Ok(())
}

/// What another store adds to a launch of one of its games (design/STORES.md §3.6): the
/// licence arguments (Epic's exchange code, kept out of logs by the caller), environment,
/// and installers to run once per prefix (Epic's prerequisites).
#[derive(Default)]
pub struct StoreLaunch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Run in the prefix before the game, each to the end (a failure is reported, not fatal).
    pub setup: Vec<SetupRun>,
}

/// An installer run once in a prefix.
pub struct SetupRun {
    pub name: String,
    /// Host path of the executable.
    pub exe: PathBuf,
    pub args: Vec<String>,
    /// Written in the prefix's run folder when it succeeds, so it runs once.
    pub marker: String,
}

/// Create a plain Proton prefix for a game of another store (no Ubisoft SDK).
/// The marker [`ensure_plain_prefix`] leaves in a prefix it set up (nothing should be
/// written into a prefix before it is there).
pub const PLAIN_PREFIX_READY: &str = ".quiver-prefix-ready";

pub async fn ensure_plain_prefix(cfg: &Config, prefix: &Path, build: &Path) -> Result<()> {
    let marker = prefix.join(PLAIN_PREFIX_READY);
    if marker.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(prefix)?;
    let pfx_link = prefix.join("pfx");
    if !pfx_link.exists() {
        std::os::unix::fs::symlink(".", &pfx_link)?;
    }
    let proton = Proton { cfg, prefix, build: build.to_owned() };
    proton.run_to_end(&["wineboot", "-u"], &prefix.join("quiver-setup.log")).await?;
    std::fs::write(marker, "")?;
    Ok(())
}

/// Launch another store's game (Epic) the way Quiver launches its own: the game's Proton
/// prefix and build, the plan's mods (overlays), config patches, settings (env, DLL
/// overrides, arguments, wrappers) and session id, plus what the store adds
/// ([`StoreLaunch`]). No bridge, no Ubisoft services. Waits for the game to exit; the
/// caller records the session (local play time).
pub async fn play_store_game(cfg: &Config, game: &Game, plan: &crate::mods::Plan, store: StoreLaunch, events: impl Fn(LaunchEvent)) -> Result<()> {
    let settings = &plan.api.settings;
    let profile = plan.api.profile.as_ref();
    let build = match &settings.proton {
        Some(name) => resolve_proton(name)?,
        None => cfg.proton.clone(),
    };
    events(LaunchEvent::Status("Preparing prefix…".into()));
    ensure_plain_prefix(cfg, &game.prefix, &build).await?;
    let run_dir = run_dir(&game.prefix);
    std::fs::create_dir_all(&run_dir)?;
    let previous = Proton { cfg, prefix: &game.prefix, build: session_build(game).unwrap_or_else(|| build.clone()) };
    let _ = previous.wineserver().arg("-k").status().await;
    let _ = previous.wineserver().arg("-w").status().await;
    record_session_build(game, &build)?;
    let proton = Proton { cfg, prefix: &game.prefix, build };
    for step in &store.setup {
        let marker = run_dir.join(&step.marker);
        if marker.exists() {
            continue;
        }
        events(LaunchEvent::Status(format!("Installing {}…", step.name)));
        let exe = win_path(&step.exe);
        let mut args = vec![exe.as_str()];
        args.extend(step.args.iter().map(String::as_str));
        match proton.run_to_end(&args, &run_dir.join(format!("{}.log", step.marker))).await {
            Ok(()) => std::fs::write(&marker, "")?,
            Err(e) => events(LaunchEvent::Detail(vec![format!("{} didn't finish: {e:#}", step.name)])),
        }
    }
    if !plan.patches.is_empty() {
        events(LaunchEvent::Status("Applying config patches…".into()));
    }
    match plan.apply_patches() {
        Ok(st) if !st.is_empty() => events(LaunchEvent::Detail(describe_patches(&st))),
        Ok(_) => {}
        Err(e) => events(LaunchEvent::Detail(vec![format!("config patches not applied: {e:#}")])),
    }
    let ns = if plan.overlays.is_empty() {
        None
    } else {
        let what = profile.map_or_else(|| "fixes".to_owned(), |p| p.name.clone());
        events(LaunchEvent::Status(format!("Mounting mods ({what})…")));
        for o in &plan.overlays {
            std::fs::create_dir_all(&o.target)?;
        }
        Some(crate::overlay::Namespace::create_many(&plan.overlays)?)
    };

    events(LaunchEvent::Status("Starting…".into()));
    let exe = game.install_dir.join(&game.exe);
    let exe_win = win_path(&exe);
    let mut args = vec![exe_win.as_str()];
    let extra: Vec<String> = game.args.as_deref().map(|a| a.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
    args.extend(extra.iter().map(String::as_str));
    args.extend(store.args.iter().map(String::as_str));
    args.extend(settings.args.iter().map(String::as_str));
    let mut cmd = proton.wrapped(&settings.wrappers, &args);
    for (k, v) in store.env.iter().map(|(k, v)| (k, v)).chain(&settings.env) {
        cmd.env(k, v);
    }
    if !settings.dll_overrides.is_empty() {
        let overrides: Vec<String> = settings.dll_overrides.iter().map(|(k, v)| format!("{k}={v}")).collect();
        cmd.env("WINEDLLOVERRIDES", overrides.join(";"));
    }
    let workdir = exe.parent().unwrap_or(&game.install_dir);
    if let Some(ns) = &ns {
        ns.enter(cmd.as_std_mut(), Some(workdir))?;
    }
    if let Some(p) = profile {
        cmd.env("QUIVER_PROFILE", &p.id);
    }
    if let Some(s) = &plan.session {
        cmd.env("QUIVER_SESSION", s);
    }
    cmd.current_dir(workdir).stdout(Stdio::null()).stderr(std::fs::File::create(run_dir.join("game.log"))?).kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let started = wait_for_game_start(&proton, &mut child).await;
    if !started {
        let _ = child.start_kill();
        let _ = proton.wineserver().arg("-k").status().await;
        bail!("the game didn't start; see {}", run_dir.join("game.log").display());
    }
    events(LaunchEvent::Running);
    let mut pids = game_processes(&proton);
    events(LaunchEvent::Pids(pids.clone()));
    while !pids.is_empty() {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let now = game_processes(&proton);
        if now != pids {
            events(LaunchEvent::Pids(now.clone()));
        }
        pids = now;
    }
    let status = child.try_wait().ok().flatten();
    let _ = child.start_kill();
    let _ = proton.wineserver().arg("-k").status().await;
    let _ = child.wait().await;
    events(LaunchEvent::Exited(status.and_then(|s| s.code())));
    drop(ns);
    events(LaunchEvent::Status("Done".into()));
    Ok(())
}

/// Where the Proton build of the game's latest session is recorded.
fn session_build_path(game: &Game) -> PathBuf {
    run_dir(&game.prefix).join("proton-build")
}

fn record_session_build(game: &Game, build: &Path) -> Result<()> {
    let path = session_build_path(game);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, build.as_os_str().as_encoded_bytes())?;
    Ok(())
}

/// The Proton build that started the game's latest session (a mod profile may override
/// the game's), if it is still installed.
fn session_build(game: &Game) -> Option<PathBuf> {
    let text = std::fs::read_to_string(session_build_path(game)).ok()?;
    let build = PathBuf::from(text.trim());
    build.join("files/bin/wineserver").is_file().then_some(build)
}

/// Ask a running game to stop by shutting its prefix down, with the wineserver of the
/// Proton build that launched it.
pub async fn stop(cfg: &Config, game: &Game) -> Result<()> {
    let build = match session_build(game) {
        Some(b) => b,
        None => match &crate::settings::current(game.product_id)?.0.proton {
            Some(name) => resolve_proton(name)?,
            None => cfg.proton.clone(),
        },
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
                && !cmd.contains("quiver-bridge")
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

/// Steam's virtual gamepad (the pad Steam Input exposes while it's running).
const STEAM_VIRTUAL_PAD: (u16, u16) = (0x28de, 0x11ff);

/// When Steam Input is mirroring Valve controllers as its virtual pad, hide the physical
/// controllers from Wine (both its hidraw and SDL paths), as Steam does for its own games.
/// Without the virtual pad (Steam not running) nothing is hidden, so the controller still
/// works directly.
pub fn steam_input_env() -> Vec<(&'static str, String)> {
    let devices = input_devices(std::path::Path::new("/sys/class/input"));
    steam_input_env_for(&devices)
}

fn steam_input_env_for(devices: &[(u16, u16)]) -> Vec<(&'static str, String)> {
    if !devices.contains(&STEAM_VIRTUAL_PAD) {
        return Vec::new();
    }
    let mut physical: Vec<(u16, u16)> =
        devices.iter().copied().filter(|&(v, p)| v == 0x28de && (v, p) != STEAM_VIRTUAL_PAD).collect();
    physical.sort();
    physical.dedup();
    if physical.is_empty() {
        return Vec::new();
    }
    let list = |upper: bool| {
        physical
            .iter()
            .map(|(v, p)| if upper { format!("0x{v:04X}/0x{p:04X}") } else { format!("0x{v:04x}/0x{p:04x}") })
            .collect::<Vec<_>>()
            .join(",")
    };
    vec![
        ("PROTON_DISABLE_HIDRAW", list(true)),
        ("SDL_GAMECONTROLLER_IGNORE_DEVICES", list(false)),
        ("SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD", "1".into()),
    ]
}

/// (vendor, product) of every input device under `dir` (`/sys/class/input/input*/id`).
fn input_devices(dir: &std::path::Path) -> Vec<(u16, u16)> {
    let hex = |p: std::path::PathBuf| std::fs::read_to_string(p).ok().and_then(|s| u16::from_str_radix(s.trim(), 16).ok());
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((hex(e.path().join("id/vendor"))?, hex(e.path().join("id/product"))?)))
        .collect()
}

#[cfg(test)]
mod steam_input_tests {
    use super::*;

    #[test]
    fn hides_physical_valve_pads_only_when_steam_mirrors_them() {
        let none = steam_input_env_for(&[(0x28de, 0x1302), (0x093a, 0x0274)]);
        assert!(none.is_empty(), "no virtual pad: leave the controller visible");
        let env = steam_input_env_for(&[(0x28de, 0x1302), (0x28de, 0x11ff), (0x28de, 0x1303), (0x28de, 0x1302)]);
        assert_eq!(env[0], ("PROTON_DISABLE_HIDRAW", "0x28DE/0x1302,0x28DE/0x1303".to_string()));
        assert_eq!(env[1], ("SDL_GAMECONTROLLER_IGNORE_DEVICES", "0x28de/0x1302,0x28de/0x1303".to_string()));
        assert!(steam_input_env_for(&[(0x28de, 0x11ff)]).is_empty(), "virtual pad alone: nothing to hide");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_json(bridge: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "library": "/g/lib",
            "prefixes": "/g/pfx",
            "proton": "/p/GE-Proton10-34",
            "sdk_source": "/g/sdk",
            "save_backups": "/d/save-backups",
        });
        if let Some(b) = bridge {
            v["bridge_exe"] = b.into();
        }
        v.to_string()
    }

    #[test]
    fn bridge_candidates_cover_package_and_dev_layouts() {
        let c = bridge_candidates(Some(Path::new("/usr/bin")));
        assert_eq!(c[0], Path::new("/usr/bin/quiver-bridge.exe"));
        assert_eq!(c[1], Path::new("/usr/lib/quiver/quiver-bridge.exe"));
        assert_eq!(c[2], dev_bridge_exe());
        assert_eq!(c.len(), 3);
        assert_eq!(bridge_candidates(None), vec![dev_bridge_exe()]);
    }

    #[test]
    fn find_bridge_prefers_beside_then_installed_then_dev() {
        let c = bridge_candidates(Some(Path::new("/usr/bin")));
        let installed = Path::new("/usr/lib/quiver/quiver-bridge.exe");
        let dev = dev_bridge_exe();
        // The package, with a leftover cross build in the checkout it was built from.
        let found = find_bridge_exe(&c, |p| p == installed || p == dev);
        assert_eq!(found.as_deref(), Some(installed));
        // Beside the binary wins over both.
        let found = find_bridge_exe(&c, |_| true);
        assert_eq!(found.as_deref(), Some(Path::new("/usr/bin/quiver-bridge.exe")));
        // A dev binary in target/release: only the cross build exists.
        let c = bridge_candidates(Some(Path::new("/src/quiver/target/release")));
        assert_eq!(c[1], Path::new("/src/quiver/target/lib/quiver/quiver-bridge.exe"));
        assert_eq!(find_bridge_exe(&c, |p| p == dev), Some(dev.clone()));
        assert_eq!(find_bridge_exe(&c, |_| false), None);
    }

    #[test]
    fn legacy_default_bridge_reads_as_auto() {
        let old = "/home/u/code/ubisoft-linux/crates/ubi-core/../../target/x86_64-pc-windows-gnu/release/ubi-bridge.exe";
        let cfg: Config = serde_json::from_str(&config_json(Some(old))).unwrap();
        assert_eq!(cfg.bridge_exe, None);
        // Saving drops it, and the result still loads.
        let saved = serde_json::to_value(&cfg).unwrap();
        assert!(saved.get("bridge_exe").is_none());
        let again: Config = serde_json::from_value(saved).unwrap();
        assert_eq!(again.bridge_exe, None);
    }

    #[test]
    fn missing_bridge_reads_as_auto() {
        let cfg: Config = serde_json::from_str(&config_json(None)).unwrap();
        assert_eq!(cfg.bridge_exe, None);
    }

    #[test]
    fn chosen_bridge_is_kept() {
        for p in ["/opt/bridges/quiver-bridge.exe", "/src/q/target/x86_64-pc-windows-gnu/debug/quiver-bridge.exe"] {
            let cfg: Config = serde_json::from_str(&config_json(Some(p))).unwrap();
            assert_eq!(cfg.bridge_exe.as_deref(), Some(Path::new(p)));
            let saved = serde_json::to_value(&cfg).unwrap();
            assert_eq!(saved["bridge_exe"], p);
        }
    }

    #[test]
    fn prefix_bridge_keeps_the_old_path_as_a_link() {
        let t = tempfile::tempdir().unwrap();
        let (prefix, src) = (t.path().join("pfx"), t.path().join("quiver-bridge.exe"));
        std::fs::write(&src, b"MZ new").unwrap();
        // A prefix from before the rename: a copy of the old bridge at C:\ubi.
        std::fs::create_dir_all(prefix.join("drive_c/ubi")).unwrap();
        std::fs::write(prefix.join(LEGACY_PREFIX_BRIDGE), b"MZ old").unwrap();
        for _ in 0..2 {
            let dst = install_prefix_bridge(&prefix, &src).unwrap();
            assert_eq!(dst, prefix.join(PREFIX_BRIDGE));
            assert_eq!(std::fs::read(&dst).unwrap(), b"MZ new");
            assert!(prefix.join(LEGACY_PREFIX_BRIDGE).symlink_metadata().unwrap().is_symlink());
            assert_eq!(std::fs::read(prefix.join(LEGACY_PREFIX_BRIDGE)).unwrap(), b"MZ new");
        }
    }

    #[test]
    fn renamed_state_moves_once() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("ubi-run")).unwrap();
        std::fs::write(t.path().join("ubi-run/proton-build"), b"x").unwrap();
        let dir = run_dir(t.path());
        assert_eq!(dir, t.path().join("quiver-run"));
        assert_eq!(std::fs::read(dir.join("proton-build")).unwrap(), b"x");
        assert!(!t.path().join("ubi-run").exists());
        // Both there (an old build ran after the move): the new one wins, the old is left.
        std::fs::create_dir(t.path().join("ubi-run")).unwrap();
        assert_eq!(run_dir(t.path()), t.path().join("quiver-run"));
        assert!(t.path().join("ubi-run").exists());
    }

    #[test]
    fn chosen_bridge_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("quiver-bridge.exe");
        let mut cfg: Config = serde_json::from_str(&config_json(Some(exe.to_str().unwrap()))).unwrap();
        assert!(cfg.bridge_exe().unwrap_err().to_string().contains("not found at"));
        std::fs::write(&exe, b"MZ").unwrap();
        assert_eq!(cfg.bridge_exe().unwrap(), exe);
        cfg.bridge_exe = None;
        // The automatic pick is only ever a file that's there.
        if let Ok(p) = cfg.bridge_exe() {
            assert!(p.is_file());
        }
    }
}
