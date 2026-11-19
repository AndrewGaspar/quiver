//! Ubisoft games installed through Steam, and switching them to start through Quiver
//! instead of Ubisoft Connect (the "Connect patch").
//!
//! The switch is a launch option: `<quiver> --source steam steam-run --product <id> %command%`
//! (see [`crate::steamrun`]). Options written before the id was always included (no
//! `--product`), and a hand-typed `quiver steam-run %command%`, count as switched too. It lives in the Steam user's `userdata/<id>/config/localconfig.vdf`
//! at `UserLocalConfigStore/Software/Valve/Steam/apps/<appid>/LaunchOptions`. The user's own
//! options are kept: with a `%command%`, Quiver's part goes right before it; without one,
//! what was there are game arguments, so they go after `%command%`. Undoing restores what was
//! there, byte for byte when the options are still as Quiver wrote them, else by taking
//! Quiver's part out. Nothing in the game's prefix changes.
//!
//! Steam keeps `localconfig.vdf` in memory and writes it back when it exits, so like
//! shortcuts ([`super::steam`]) a change made while Steam runs is staged and written when
//! Steam is closed: on request, by the next change with Steam closed, or by
//! [`super::steam::restart_steam`] (which front ends only call after the user confirms).
//!
//! Detection walks every Steam library (`steamapps/libraryfolders.vdf`) and its
//! `appmanifest_*.acf` files. A game is a Ubisoft game when its install script registers
//! `…\Ubisoft\Launcher\Installs\<id>` or mentions Ubisoft Connect, when its folder ships
//! Connect's SDK loaders or installer, or when it is in [`crate::steamrun`]'s table. The
//! product id comes from the install script, the prefix's registry, or the table.
//!
//! State (`<data>/steam-games.json`): what Quiver patched (and the options it replaced),
//! staged changes, the app ids seen so far (only games installed later are offered), and
//! the offers made and declined.
//!
//! **Any Steam game** (design/STORE-STEAM.md §9): [`hook`] and [`unhook`] do the same for a
//! game that isn't a Ubisoft copy, with `<quiver> --source steam steam-run --app <appid>`
//! ([`hook_command`]): `steam-run` then applies the game's launch plan (tweaks, HUD, settings)
//! without swapping anything. Opt-in per game. Undoing a hook or a patch is exact: a
//! `LaunchOptions` key (or app section) Quiver added is taken out again rather than left empty.
//! (Patches recorded before that have no `added` and are undone as before.)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use quiver_api::exports::PatchState;

use super::steam::Steam;
use super::{Paths, lock, textvdf, write_atomic};
use crate::steamrun::{self, ProductSource};

/// Where launch options live in `localconfig.vdf`, above the app id.
const APPS: [&str; 5] = ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];

/// Files only Ubisoft games ship: Connect's SDK loaders and its installer.
const UBI_FILES: &[&str] = &[
    "upc_r2_loader64.dll",
    "upc_r2_loader.dll",
    "uplay_r1_loader64.dll",
    "uplay_r1_loader.dll",
    "uplay_r2_loader64.dll",
    "uplay_r2_loader.dll",
    "uplay_r164.dll",
    "uplay_r264.dll",
    "ubisoftconnectinstaller.exe",
    "uplayinstaller.exe",
];

// ---------------------------------------------------------------------------------------
// Libraries and app manifests

/// Every Steam library folder (the root first), from `steamapps/libraryfolders.vdf`.
pub fn library_folders(steam: &Steam) -> Vec<PathBuf> {
    let mut out = vec![steam.root.clone()];
    let text = std::fs::read_to_string(steam.root.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
    if let Ok(root) = textvdf::parse(&text) {
        let folders = root.get("libraryfolders").or_else(|| root.get("LibraryFolders"));
        for (key, v) in folders.map(|f| f.entries()).unwrap_or_default() {
            match v {
                textvdf::Node::Map(_) => out.extend(v.str("path").map(PathBuf::from)),
                // The old format: "1" "/path".
                textvdf::Node::Str(p) if key.parse::<u32>().is_ok() => out.push(PathBuf::from(p)),
                textvdf::Node::Str(_) => {}
            }
        }
    }
    let mut seen = BTreeSet::new();
    out.into_iter()
        .filter(|p| p.join("steamapps").is_dir())
        .filter(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
        .collect()
}

/// A game's `steamapps/appmanifest_<appid>.acf`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppManifest {
    pub app_id: u32,
    pub name: String,
    /// The library folder it's in.
    pub library: PathBuf,
    /// `<library>/steamapps/common/<installdir>`.
    pub install_dir: PathBuf,
    /// Fully installed (`StateFlags` has 4), not only being downloaded.
    pub installed: bool,
    /// SteamID64 of the account that installed it (`LastOwner`).
    pub owner: Option<u64>,
    /// Install scripts, relative to the install folder.
    pub scripts: Vec<String>,
    /// Changes when the game updates.
    pub build_id: Option<String>,
    /// `StateFlags` (Steam's `EAppState` bits, [`flags`]): 4 fully installed, 2 update
    /// required, 256 update running, 512 update paused, 1024 update started, and the
    /// download phases (1048576 downloading, 2097152 staging, 4194304 committing, …).
    pub state_flags: u32,
    /// Bytes on disk (`SizeOnDisk`).
    pub size_on_disk: u64,
    /// When Steam last started it (`LastPlayed`, unix seconds; 0 or missing: never).
    pub last_played: Option<i64>,
    /// DLC whose depots are installed (`InstalledDepots/<depot>/dlcappid`).
    pub dlc_installed: Vec<u32>,
    /// A newer build is wanted than the one installed (`TargetBuildID`).
    pub target_build_id: Option<String>,
    /// Steam's download of it (`BytesToDownload`, …, `UpdateResult`,
    /// `AutoUpdateBehavior`).
    pub download: Download,
}

/// Steam's `EAppState` bits in an app manifest's `StateFlags`.
pub mod flags {
    pub const UPDATE_REQUIRED: u32 = 2;
    pub const FULLY_INSTALLED: u32 = 4;
    pub const UPDATE_RUNNING: u32 = 256;
    pub const UPDATE_PAUSED: u32 = 512;
    pub const UPDATE_STARTED: u32 = 1024;
    pub const VALIDATING: u32 = 131072;
    pub const ADDING_FILES: u32 = 262144;
    pub const PREALLOCATING: u32 = 524288;
    pub const DOWNLOADING: u32 = 1048576;
    pub const STAGING: u32 = 2097152;
    pub const COMMITTING: u32 = 4194304;
    /// Any bit that means an update is wanted or under way.
    pub const ANY_UPDATE: u32 = UPDATE_REQUIRED | UPDATE_RUNNING | UPDATE_PAUSED | UPDATE_STARTED | DOWNLOADING | STAGING | COMMITTING | ADDING_FILES | PREALLOCATING;
}

/// What an app manifest says about Steam's current download of the app (present while
/// Steam has one, 0 otherwise; all **verified** present on real manifests, STORE-STEAM.md §2.2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Download {
    pub bytes_to_download: u64,
    pub bytes_downloaded: u64,
    pub bytes_to_stage: u64,
    pub bytes_staged: u64,
    /// Why the last update attempt failed (`UpdateResult`; 0 is success, so `None`).
    pub update_result: Option<u32>,
    /// `AutoUpdateBehavior`: 0 keep it updated, 1 only when launched, 2 high priority.
    pub auto_update_behavior: u32,
}

impl AppManifest {
    pub fn parse(text: &str, library: &Path) -> Option<Self> {
        let root = textvdf::parse(text).ok()?;
        let a = root.get("AppState")?;
        let app_id = a.str("appid")?.parse().ok()?;
        let dir = a.str("installdir").filter(|d| !d.is_empty())?;
        let flags: u32 = a.str("StateFlags").and_then(|f| f.parse().ok()).unwrap_or(0);
        let num = |k: &str| a.str(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let scripts = a.get("InstallScripts").map(|s| s.entries().iter().filter_map(|(_, v)| match v {
            textvdf::Node::Str(s) => Some(s.clone()),
            textvdf::Node::Map(_) => None,
        }).collect()).unwrap_or_default();
        Some(Self {
            app_id,
            name: a.str("name").unwrap_or(dir).to_owned(),
            library: library.to_path_buf(),
            install_dir: library.join("steamapps/common").join(dir),
            installed: flags & 4 != 0,
            owner: a.str("LastOwner").and_then(|o| o.parse().ok()).filter(|&o| o != 0),
            scripts,
            build_id: a.str("buildid").map(str::to_owned),
            state_flags: flags,
            size_on_disk: a.str("SizeOnDisk").and_then(|s| s.parse().ok()).unwrap_or(0),
            last_played: a.str("LastPlayed").and_then(|s| s.parse().ok()).filter(|&t: &i64| t > 0),
            dlc_installed: a
                .get("InstalledDepots")
                .map(|d| d.entries().iter().filter_map(|(_, v)| v.str("dlcappid")?.parse().ok()).collect())
                .unwrap_or_default(),
            target_build_id: a.str("TargetBuildID").filter(|t| !t.is_empty() && *t != "0").map(str::to_owned),
            download: Download {
                bytes_to_download: num("BytesToDownload"),
                bytes_downloaded: num("BytesDownloaded"),
                bytes_to_stage: num("BytesToStage"),
                bytes_staged: num("BytesStaged"),
                update_result: Some(num("UpdateResult") as u32).filter(|&r| r != 0),
                auto_update_behavior: num("AutoUpdateBehavior") as u32,
            },
        })
    }

    /// Steam has an update for it waiting or in progress (an update or download state
    /// flag, or a target build other than the installed one), for a game that was
    /// installed before: a first install still downloading isn't an update.
    pub fn update_pending(&self) -> bool {
        let first_install = !self.installed && self.build_id.as_deref().is_none_or(|b| b.is_empty() || b == "0");
        if first_install {
            return false;
        }
        self.state_flags & flags::ANY_UPDATE != 0 || self.target_build_id.as_ref().is_some_and(|t| Some(t) != self.build_id.as_ref())
    }

    /// Steam's update of it, as its manifest says (`None`: no update and no file check).
    /// Steam does the updating; this is for showing it.
    pub fn update(&self) -> Option<quiver_api::stores::ClientUpdate> {
        use quiver_api::stores::{ClientUpdate, ClientUpdateState as S};
        let f = self.state_flags;
        let validating = f & flags::VALIDATING != 0;
        if !self.update_pending() && !validating {
            return None;
        }
        let state = if f & flags::UPDATE_PAUSED != 0 {
            S::Paused
        } else if validating {
            S::Validating
        } else if f & (flags::STAGING | flags::COMMITTING | flags::ADDING_FILES | flags::PREALLOCATING) != 0 {
            S::Installing
        } else if f & (flags::DOWNLOADING | flags::UPDATE_RUNNING | flags::UPDATE_STARTED) != 0 {
            S::Downloading
        } else {
            S::Queued
        };
        let d = &self.download;
        Some(ClientUpdate {
            state,
            bytes_to_download: d.bytes_to_download,
            bytes_downloaded: d.bytes_downloaded,
            bytes_to_stage: d.bytes_to_stage,
            bytes_staged: d.bytes_staged,
            target_build_id: self.target_build_id.clone().filter(|t| Some(t) != self.build_id.as_ref()),
            error: d.update_result,
            on_launch: d.auto_update_behavior == 1,
            open_url: Some(super::steamlaunch::DOWNLOADS_URL.to_owned()),
        })
    }

    /// `<library>/steamapps/compatdata/<appid>/pfx`.
    pub fn prefix(&self) -> PathBuf {
        self.library.join("steamapps/compatdata").join(self.app_id.to_string()).join("pfx")
    }
}

/// The app manifests in one library folder.
pub fn app_manifests(library: &Path) -> Vec<AppManifest> {
    let Ok(rd) = std::fs::read_dir(library.join("steamapps")) else { return Vec::new() };
    let mut out: Vec<AppManifest> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("appmanifest_") && n.ends_with(".acf")
        })
        .filter_map(|e| AppManifest::parse(&std::fs::read_to_string(e.path()).ok()?, library))
        .collect();
    out.sort_by_key(|a| a.app_id);
    out
}

/// A Ubisoft game installed through Steam.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub app: AppManifest,
    pub product: Option<(u32, ProductSource)>,
}

fn has_ubisoft_files(dir: &Path) -> bool {
    let hit = |d: &Path| {
        std::fs::read_dir(d).into_iter().flatten().filter_map(|e| e.ok()).any(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            UBI_FILES.contains(&n.as_str())
        })
    };
    if hit(dir) {
        return true;
    }
    // One level down (`bin/`, `Binaries/Win64` is two, but its parent has the installer).
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .take(64)
        .any(|e| hit(&e.path()))
}

/// Whether an installed Steam app is a Ubisoft game, and which product. `None`: not one.
pub fn detect(app: &AppManifest) -> Option<Option<(u32, ProductSource)>> {
    let mut scripts: Vec<String> = app.scripts.clone();
    let own = format!("installscript_{}.vdf", app.app_id);
    if !scripts.iter().any(|s| s.eq_ignore_ascii_case(&own)) {
        scripts.push(own);
    }
    let texts: Vec<String> = scripts
        .iter()
        .filter_map(|s| std::fs::read_to_string(app.install_dir.join(s.replace('\\', "/"))).ok())
        .collect();
    if let Some(id) = texts.iter().find_map(|t| steamrun::product_from_installscript(t)) {
        return Some(Some((id, ProductSource::InstallScript)));
    }
    let mentions = texts.iter().any(|t| {
        let t = t.to_ascii_lowercase();
        t.contains("ubisoft") || t.contains("uplay")
    });
    let known = steamrun::known_product(app.app_id).is_some();
    if !mentions && !known && !has_ubisoft_files(&app.install_dir) {
        return None;
    }
    // A Ubisoft game: the prefix's registry (several MB, so only now) or the table.
    let reg = std::fs::read_to_string(app.prefix().join("system.reg")).ok();
    Some(steamrun::detect_product(app.app_id, None, reg.as_deref()))
}

/// Installed Ubisoft games in every library, by app id.
pub fn find_games(steam: &Steam) -> Vec<Found> {
    let mut out: Vec<Found> = library_folders(steam)
        .iter()
        .flat_map(|l| app_manifests(l))
        .filter(|a| a.installed)
        .filter_map(|app| detect(&app).map(|product| Found { app, product }))
        .collect();
    out.sort_by_key(|f| f.app.app_id);
    out.dedup_by_key(|f| f.app.app_id);
    out
}

/// The `userdata/<id>` of the account that installed it, if it's one of `users`.
pub fn owner_user(app: &AppManifest, users: &[quiver_api::exports::SteamUser]) -> Option<String> {
    let owner = app.owner?;
    users.iter().find(|u| u.steam_id64 == owner).map(|u| u.id.clone())
}

// ---------------------------------------------------------------------------------------
// Launch options

/// Shell-like words with their byte ranges: quotes and backslashes group, `'…'` is literal.
fn words(s: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some(&(start, c)) = it.peek() {
        if c.is_whitespace() {
            it.next();
            continue;
        }
        let mut w = String::new();
        let mut end = start;
        while let Some(&(i, c)) = it.peek() {
            if c.is_whitespace() {
                break;
            }
            it.next();
            end = i + c.len_utf8();
            match c {
                '\'' => {
                    for (i, c) in it.by_ref() {
                        end = i + c.len_utf8();
                        if c == '\'' {
                            break;
                        }
                        w.push(c);
                    }
                }
                '"' => {
                    while let Some((i, c)) = it.next() {
                        end = i + c.len_utf8();
                        match c {
                            '"' => break,
                            '\\' => {
                                if let Some((i, n)) = it.next() {
                                    end = i + n.len_utf8();
                                    w.push(n);
                                }
                            }
                            c => w.push(c),
                        }
                    }
                }
                '\\' => {
                    if let Some((i, n)) = it.next() {
                        end = i + n.len_utf8();
                        w.push(n);
                    }
                }
                c => w.push(c),
            }
        }
        out.push((start, end, w));
    }
    out
}

/// `a` as one shell word.
pub fn shell_quote(a: &str) -> String {
    if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

/// Quiver's part of the launch options, in front of `%command%`. Quiver always passes the
/// product when it patches (a game whose product isn't known can't be patched); `None` gives
/// the generic form shown for typing by hand.
pub fn command(quiver: &Path, product: Option<u32>) -> String {
    let mut s = format!("{} --source steam steam-run", shell_quote(&quiver.to_string_lossy()));
    if let Some(p) = product {
        s.push_str(&format!(" --product {p}"));
    }
    s
}

const COMMAND: &str = "%command%";

/// Where Quiver's part is: from a `…/quiver` word through `steam-run` and its options, up to
/// `%command%` (or the end). Also finds one typed by hand (`quiver steam-run %command%`).
pub fn find_patch(opts: &str) -> Option<(usize, usize)> {
    let w = words(opts);
    let cmd = w.iter().position(|x| x.2 == COMMAND);
    let upto = cmd.unwrap_or(w.len());
    let start = (0..upto).find(|&i| {
        Path::new(&w[i].2).file_name().is_some_and(|n| n == "quiver") && (i + 1..upto).any(|j| w[j].2 == "steam-run")
    })?;
    let end = match cmd {
        Some(c) => w[c].0,
        None => w[upto - 1].1,
    };
    Some((w[start].0, end))
}

/// Whether Quiver can add its part to these options: `%command%` must be a word of its own
/// (inside a quoted `bash -c "… %command%"` there's no safe place to put it).
pub fn check_patchable(current: &str) -> Result<()> {
    if current.contains(COMMAND) && !words(current).iter().any(|w| w.2 == COMMAND) && find_patch(current).is_none() {
        bail!("its launch options use %command% inside quotes, so Quiver can't add itself safely; add `quiver --source steam steam-run` before %command% by hand");
    }
    Ok(())
}

/// The launch options with Quiver's part added (unchanged if it's there already). Check
/// [`check_patchable`] first.
pub fn patched(current: &str, cmd: &str) -> String {
    if find_patch(current).is_some() {
        return current.to_owned();
    }
    match words(current).iter().find(|x| x.2 == COMMAND) {
        Some(c) => format!("{}{cmd} {}", &current[..c.0], &current[c.0..]),
        None if current.trim().is_empty() => format!("{cmd} {COMMAND}"),
        // No %command%: Steam passes what's there to the game as arguments.
        None => format!("{cmd} {COMMAND} {}", current.trim()),
    }
}

/// The launch options without Quiver's part: what Quiver replaced, when they are still as
/// Quiver wrote them; else the same options with Quiver's part taken out.
pub fn unpatched(current: &str, record: Option<&PatchRecord>) -> String {
    if let Some(r) = record.filter(|r| r.written == current) {
        return r.original.clone();
    }
    let Some((s, e)) = find_patch(current) else { return current.to_owned() };
    let out = format!("{}{}", &current[..s], &current[e..]);
    if out.trim() == COMMAND { String::new() } else { out }
}

// ---------------------------------------------------------------------------------------
// localconfig.vdf

pub fn localconfig(steam: &Steam, user: &str) -> PathBuf {
    steam.root.join("userdata").join(user).join("config/localconfig.vdf")
}

/// An app's launch options in a `localconfig.vdf` text ("" when it has none).
pub fn launch_options_in(text: &str, app_id: u32) -> String {
    let Ok(root) = textvdf::parse(text) else { return String::new() };
    let mut n = &root;
    for k in APPS {
        match n.get(k) {
            Some(c) => n = c,
            None => return String::new(),
        }
    }
    n.get(&app_id.to_string()).and_then(|a| a.str("LaunchOptions")).unwrap_or_default().to_owned()
}

/// The text with an app's launch options set; every other byte stays.
pub fn set_launch_options_in(text: &str, app_id: u32, value: &str) -> Result<String> {
    let id = app_id.to_string();
    let mut path = APPS.to_vec();
    path.extend([id.as_str(), "LaunchOptions"]);
    textvdf::set_string(text, &path, value)
}

/// A user's launch options for an app ("" without a localconfig.vdf).
pub fn launch_options(steam: &Steam, user: &str, app_id: u32) -> String {
    std::fs::read_to_string(localconfig(steam, user)).map(|t| launch_options_in(&t, app_id)).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// State

/// Launch options Quiver changed: what was there, and what it wrote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PatchRecord {
    pub user: String,
    pub app_id: u32,
    pub original: String,
    pub written: String,
    pub at: DateTime<Utc>,
    /// A hook added the `LaunchOptions` key (or the app's whole section): undoing it takes
    /// that out again. Older records (and Ubisoft patches) leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<Added>,
}

/// What a hook added to `localconfig.vdf` that wasn't there.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Added {
    /// `LaunchOptions` in the app's section.
    Key,
    /// The app's section under `apps`.
    Section,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StagedOp {
    /// Add `command` in front of `%command%`.
    Patch { command: String },
    Unpatch,
}

/// A launch-options change waiting for Steam to close. The new options are worked out when
/// it's written, from what the file says then.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StagedPatch {
    pub file: PathBuf,
    pub user: String,
    pub app_id: u32,
    pub name: String,
    #[serde(flatten)]
    pub op: StagedOp,
    /// Any Steam game's hook ([`hook`]), not a Ubisoft copy's Connect switch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hook: bool,
}

impl StagedPatch {
    pub fn describe(&self) -> String {
        let what = match (&self.op, self.hook) {
            (StagedOp::Patch { .. }, _) => "start through Quiver:",
            (StagedOp::Unpatch, false) => "start through Ubisoft Connect again:",
            (StagedOp::Unpatch, true) => "start without Quiver again:",
        };
        format!("{what} \"{}\" (Steam app {}) for Steam user {}", self.name, self.app_id, self.user)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// The first look recorded what was installed then; only later installs are offered.
    #[serde(default)]
    pub initialized: bool,
    #[serde(default)]
    pub seen: BTreeSet<u32>,
    /// Offered and not answered.
    #[serde(default)]
    pub offers: BTreeSet<u32>,
    /// "Not now": never offered again.
    #[serde(default)]
    pub declined: BTreeSet<u32>,
    #[serde(default)]
    pub patches: Vec<PatchRecord>,
    #[serde(default)]
    pub staged: Vec<StagedPatch>,
}

impl State {
    pub fn record(&self, user: &str, app_id: u32) -> Option<&PatchRecord> {
        self.patches.iter().find(|r| r.user == user && r.app_id == app_id)
    }

    pub fn staged_op(&self, user: &str, app_id: u32) -> Option<&StagedOp> {
        self.staged.iter().find(|s| s.user == user && s.app_id == app_id).map(|s| &s.op)
    }

    /// Where a game stands, given its launch options now.
    pub fn patch_state(&self, user: &str, app_id: u32, current: &str) -> PatchState {
        match self.staged_op(user, app_id) {
            Some(StagedOp::Patch { .. }) => return PatchState::Waiting,
            Some(StagedOp::Unpatch) => return PatchState::WaitingUndo,
            None => {}
        }
        match (find_patch(current).is_some(), self.record(user, app_id).is_some()) {
            (true, true) => PatchState::On,
            (true, false) => PatchState::ByHand,
            (false, _) => PatchState::Off,
        }
    }
}

fn state_path(paths: &Paths) -> PathBuf {
    paths.quiver().join("steam-games.json")
}

pub fn load(paths: &Paths) -> State {
    std::fs::read_to_string(state_path(paths)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save(paths: &Paths, st: &State) -> Result<()> {
    write_atomic(&state_path(paths), &serde_json::to_vec_pretty(st)?)
}

pub fn staged_descriptions(paths: &Paths) -> Vec<String> {
    load(paths).staged.iter().map(StagedPatch::describe).collect()
}

// ---------------------------------------------------------------------------------------
// Changing launch options

/// What [`patch`] or [`unpatch`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The launch options now (or once Steam closes, when staged).
    pub launch_options: String,
    pub staged: bool,
    /// Already so: nothing written or staged.
    pub unchanged: bool,
    /// A change for this game was waiting for Steam and is dropped (e.g. undo before Steam
    /// restarted).
    pub dropped_staged: bool,
}

/// Whether an app's section and its `LaunchOptions` key are in a `localconfig.vdf` text.
fn presence(text: &str, app_id: u32) -> Option<Added> {
    let Ok(root) = textvdf::parse(text) else { return None };
    let mut n = &root;
    for k in APPS {
        match n.get(k) {
            Some(c) => n = c,
            None => return Some(Added::Section),
        }
    }
    match n.get(&app_id.to_string()) {
        None => Some(Added::Section),
        Some(a) if a.get("LaunchOptions").is_none() => Some(Added::Key),
        Some(_) => None,
    }
}

/// The text with what a hook added for `app_id` taken out again (`None`: nothing to take
/// out, or the app's section has more in it now than the hook put there).
fn remove_added(text: &str, app_id: u32, added: Added) -> Result<Option<String>> {
    let id = app_id.to_string();
    let mut section = APPS.to_vec();
    section.push(id.as_str());
    let mut key = section.clone();
    key.push("LaunchOptions");
    if added == Added::Section {
        let only_ours = textvdf::parse(text).ok().is_some_and(|root| {
            let mut n = &root;
            for k in &section {
                match n.get(k) {
                    Some(c) => n = c,
                    None => return false,
                }
            }
            n.entries().len() == 1 && n.get("LaunchOptions").is_some()
        });
        if only_ours {
            return textvdf::remove(text, &section);
        }
    }
    textvdf::remove(text, &key)
}

/// Apply one op to `file` (Steam closed; caller holds the lock). Returns the new options.
/// Undone exactly ([`Added`]), for hooks and Ubisoft copies' patches alike (STORE-STEAM §9, G5).
fn write_op(st: &mut State, file: &Path, user: &str, app_id: u32, op: &StagedOp) -> Result<String> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("Steam has no settings for user {user} yet ({}); start Steam and sign in once", file.display())
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    let current = launch_options_in(&text, app_id);
    let new = match op {
        StagedOp::Patch { command } => {
            check_patchable(&current)?;
            patched(&current, command)
        }
        StagedOp::Unpatch => unpatched(&current, st.record(user, app_id)),
    };
    let added = match op {
        StagedOp::Patch { .. } => presence(&text, app_id),
        _ => None,
    };
    // Undoing a hook that added the key (or section), with the options still as written:
    // take it out again.
    let removal = match (op, st.record(user, app_id)) {
        (StagedOp::Unpatch, Some(r)) if r.written == current && new.is_empty() => match r.added {
            Some(a) => remove_added(&text, app_id, a).with_context(|| format!("edit {}", file.display()))?,
            None => None,
        },
        _ => None,
    };
    if new != current {
        let out = match removal {
            Some(out) => out,
            None => set_launch_options_in(&text, app_id, &new).with_context(|| format!("edit {}", file.display()))?,
        };
        // Check the edit reads back before replacing Steam's file.
        if launch_options_in(&out, app_id) != new {
            bail!("editing {} didn't read back; left it alone", file.display());
        }
        // One backup of the file as it was before Quiver's last write. It holds Steam's
        // app and offline tickets, CDN tokens and cloud key: owner-only.
        let _ = crate::private::write_private(&file.with_extension("vdf.quiver-backup"), text.as_bytes());
        write_atomic(file, out.as_bytes())?;
    }
    st.patches.retain(|r| !(r.user == user && r.app_id == app_id));
    if let StagedOp::Patch { .. } = op
        && new != current
    {
        st.patches.push(PatchRecord { user: user.to_owned(), app_id, original: current, written: new.clone(), at: Utc::now(), added });
    }
    Ok(new)
}

#[allow(clippy::too_many_arguments)]
fn change(paths: &Paths, steam: &Steam, user: &str, app_id: u32, name: &str, op: StagedOp, running: bool, hook: bool) -> Result<Change> {
    let _lock = lock(paths)?;
    let mut st = load(paths);
    let file = localconfig(steam, user);
    let current = launch_options(steam, user, app_id);
    let wanted = match &op {
        StagedOp::Patch { command } => {
            check_patchable(&current)?;
            patched(&current, command)
        }
        StagedOp::Unpatch => unpatched(&current, st.record(user, app_id)),
    };
    let before = st.staged.len();
    st.staged.retain(|s| !(s.user == user && s.app_id == app_id));
    let dropped_staged = st.staged.len() != before;
    st.offers.remove(&app_id);
    if wanted == current {
        // Already so (an earlier staged change for it is dropped above).
        if let StagedOp::Unpatch = op {
            st.patches.retain(|r| !(r.user == user && r.app_id == app_id));
        }
        save(paths, &st)?;
        return Ok(Change { launch_options: current, staged: false, unchanged: true, dropped_staged });
    }
    if running {
        st.staged.push(StagedPatch { file, user: user.to_owned(), app_id, name: name.to_owned(), op, hook });
        save(paths, &st)?;
        return Ok(Change { launch_options: wanted, staged: true, unchanged: false, dropped_staged });
    }
    // Steam is closed: write this, and anything staged before.
    let new = write_op(&mut st, &file, user, app_id, &op)?;
    apply_list(&mut st)?;
    save(paths, &st)?;
    Ok(Change { launch_options: new, staged: false, unchanged: false, dropped_staged })
}

/// Make the game start through Quiver for `user`: `cmd` ([`command`]) goes in front of
/// `%command%`. `running`: Steam is running ([`super::steam::is_running`]); then it's staged.
pub fn patch(paths: &Paths, steam: &Steam, user: &str, app_id: u32, name: &str, cmd: &str, running: bool) -> Result<Change> {
    if steam.flatpak {
        bail!("Steam from Flatpak can't start Quiver from inside its sandbox; this needs Steam installed natively");
    }
    change(paths, steam, user, app_id, name, StagedOp::Patch { command: cmd.to_owned() }, running, false)
}

/// Put the launch options back the way they were before Quiver (or a hand edit) added
/// `quiver … steam-run`.
pub fn unpatch(paths: &Paths, steam: &Steam, user: &str, app_id: u32, name: &str, running: bool) -> Result<Change> {
    change(paths, steam, user, app_id, name, StagedOp::Unpatch, running, false)
}

// ---------------------------------------------------------------------------------------
// Any Steam game: the hook (design/STORE-STEAM.md §9)

/// The hook's part of the launch options: `<quiver> --source steam steam-run --app <appid>`.
/// `--app` tells `steam-run` the game is a plain Steam game (no Ubisoft product to look for).
pub fn hook_command(quiver: &Path, app_id: u32) -> String {
    format!("{} --app {app_id}", command(quiver, None))
}

/// Make any Steam game start through `quiver steam-run` (`cmd`: [`hook_command`]). Staged
/// while Steam runs; [`unhook`] undoes it exactly.
pub fn hook(paths: &Paths, steam: &Steam, user: &str, app_id: u32, name: &str, cmd: &str, running: bool) -> Result<Change> {
    if steam.flatpak {
        bail!("Steam from Flatpak can't start Quiver from inside its sandbox; this needs Steam installed natively");
    }
    change(paths, steam, user, app_id, name, StagedOp::Patch { command: cmd.to_owned() }, running, true)
}

/// Undo [`hook`] (or a hand-typed `quiver steam-run`): the launch options as they were,
/// byte for byte while they're still as the hook wrote them.
pub fn unhook(paths: &Paths, steam: &Steam, user: &str, app_id: u32, name: &str, running: bool) -> Result<Change> {
    change(paths, steam, user, app_id, name, StagedOp::Unpatch, running, true)
}

/// An installed (or installing) Steam app, from any library.
pub fn find_app(steam: &Steam, app_id: u32) -> Option<AppManifest> {
    library_folders(steam).iter().flat_map(|l| app_manifests(l)).find(|a| a.app_id == app_id)
}

/// Where a Steam game's hook stands (`Request::SteamHookStatus`), from Steam's files
/// (read-only). `user`: whose launch options (`None`: several users, none chosen).
/// `quiver`: the `quiver` to write ([`quiver_for_steam`]). `product`: a Ubisoft copy's
/// product, when known (its hook is the Connect switch, with `--product`).
pub fn hook_status(paths: &Paths, steam: &Steam, app: &AppManifest, user: Option<&str>, quiver: Option<&Path>, product: Option<u32>, running: bool) -> quiver_api::exports::SteamHook {
    let launch_options = user.map(|u| launch_options(steam, u, app.app_id)).unwrap_or_default();
    let hookable = !steam.flatpak && quiver.is_some() && user.is_some() && check_patchable(&launch_options).is_ok();
    let st = load(paths);
    let state = match user {
        Some(u) => st.patch_state(u, app.app_id, &launch_options),
        None => PatchState::Unknown,
    };
    let command = match (quiver, product) {
        (Some(q), Some(id)) => command(q, Some(id)),
        (Some(q), None) => hook_command(q, app.app_id),
        (None, _) => String::new(),
    };
    let note = if steam.flatpak {
        Some("Steam from Flatpak can't start Quiver from inside its sandbox".to_owned())
    } else if quiver.is_none() {
        Some("The quiver command wasn't found (install Quiver to ~/.local/bin)".to_owned())
    } else if user.is_none() {
        Some("Several Steam users: choose which one plays it (--user)".to_owned())
    } else if !app.installed {
        Some("Steam is still downloading it".to_owned())
    } else if let Err(e) = check_patchable(&launch_options) {
        Some(format!("{e:#}"))
    } else {
        None
    };
    quiver_api::exports::SteamHook {
        app_id: app.app_id,
        name: app.name.clone(),
        state,
        steam_user: user.map(str::to_owned),
        launch_options,
        hookable,
        note,
        command,
        product_id: product,
        steam_running: running,
        staged: false,
        unchanged: false,
        dropped_staged: false,
    }
}


/// Write the staged list (Steam closed). Returns how many were written; a change that
/// fails stays staged (its error is logged).
fn apply_list(st: &mut State) -> Result<usize> {
    let list = std::mem::take(&mut st.staged);
    let mut n = 0;
    for s in list {
        match write_op(st, &s.file, &s.user, s.app_id, &s.op) {
            Ok(_) => n += 1,
            Err(e) => {
                tracing::warn!("{}: {e:#}", s.describe());
                st.staged.push(s);
            }
        }
    }
    Ok(n)
}

pub(crate) fn staged_count(paths: &Paths) -> usize {
    load(paths).staged.len()
}

/// Write staged launch-option changes. Steam must be closed; caller holds the lock.
pub(crate) fn apply_staged_locked(paths: &Paths) -> Result<usize> {
    let mut st = load(paths);
    if st.staged.is_empty() {
        return Ok(0);
    }
    let n = apply_list(&mut st)?;
    save(paths, &st)?;
    Ok(n)
}

// ---------------------------------------------------------------------------------------
// Offers

/// Record the Ubisoft games installed now (`ids`). The first time, they are only noted;
/// later, ones not seen before become offers (unless `offer` is false, `skip` says they
/// already use Quiver, or they were declined). Games no longer installed are forgotten
/// from the offers. Returns the new offers.
pub fn note_installed(paths: &Paths, ids: &[u32], offer: bool, skip: impl Fn(u32) -> bool) -> Result<Vec<u32>> {
    let _lock = lock(paths)?;
    let mut st = load(paths);
    let before = st.clone();
    let mut new = Vec::new();
    if st.initialized {
        for &id in ids.iter().filter(|id| !st.seen.contains(id)) {
            if offer && !st.declined.contains(&id) && !skip(id) {
                st.offers.insert(id);
                new.push(id);
            }
        }
    }
    st.initialized = true;
    st.seen.extend(ids.iter().copied());
    st.offers.retain(|id| ids.contains(id));
    if st != before {
        save(paths, &st)?;
    }
    Ok(new)
}

/// "Not now": the game isn't offered again.
pub fn dismiss_offer(paths: &Paths, app_id: u32) -> Result<()> {
    let _lock = lock(paths)?;
    let mut st = load(paths);
    st.offers.remove(&app_id);
    st.declined.insert(app_id);
    save(paths, &st)
}

/// Forget every pending offer (offers were turned off).
pub fn clear_offers(paths: &Paths) -> Result<()> {
    let _lock = lock(paths)?;
    let mut st = load(paths);
    if st.offers.is_empty() {
        return Ok(());
    }
    st.offers.clear();
    save(paths, &st)
}

/// The `quiver` to put in Steam's launch options, as an absolute path (Steam's `PATH`
/// usually lacks `~/.local/bin`): next to the running executable (quiverd and quiver are
/// installed side by side), else `~/.local/bin/quiver`, else the first on `PATH`.
pub fn quiver_for_steam(home: &Path) -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("quiver")));
    beside
        .filter(|p| p.is_file())
        .or_else(|| Some(home.join(".local/bin/quiver")).filter(|p| p.is_file()))
        .or_else(|| super::find_in_path("quiver"))
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Quiver writes now: always with the product id.
    const Q: &str = "/h/.local/bin/quiver --source steam steam-run --product 66088";
    /// What earlier versions wrote when steam-run could find the product itself.
    const OLD: &str = "/h/.local/bin/quiver --source steam steam-run";

    /// Steam's update state, from the manifest fields Steam writes while it updates.
    #[test]
    fn update_states() {
        use quiver_api::stores::ClientUpdateState as S;
        let m = |flags: u32, build: &str, extra: &str| {
            let text = format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"620\"\n\t\"name\"\t\t\"Portal 2\"\n\t\"StateFlags\"\t\t\"{flags}\"\n\t\"installdir\"\t\t\"Portal 2\"\n\t\"buildid\"\t\t\"{build}\"\n{extra}}}\n");
            AppManifest::parse(&text, Path::new("/lib")).unwrap()
        };
        // Installed and current: no update.
        assert!(m(4, "100", "").update().is_none());
        // Steam knows of one, not started yet.
        let q = m(6, "100", "\t\"TargetBuildID\"\t\t\"101\"\n\t\"BytesToDownload\"\t\t\"3100000000\"\n").update().unwrap();
        assert_eq!((q.state, q.target_build_id.as_deref(), q.bytes_to_download), (S::Queued, Some("101"), 3_100_000_000));
        assert_eq!(q.describe("Steam"), "Update queued in Steam of 3.1 GB");
        assert_eq!(q.open_url.as_deref(), Some(super::super::steamlaunch::DOWNLOADS_URL));
        // Downloading (1026: update started, update required; the installed bit is off).
        let d = m(1026 | flags::DOWNLOADING, "100", "\t\"BytesToDownload\"\t\t\"2000\"\n\t\"BytesDownloaded\"\t\t\"500\"\n").update().unwrap();
        assert_eq!((d.state, d.fraction()), (S::Downloading, Some(0.25)));
        assert_eq!(d.describe("Steam"), "Steam is updating it · 25% of less than 0.1 MB");
        // Small patches in MB, not "0.0 GB" (Marque & Reprisal's 5.6 MB update).
        let small = m(6, "100", "\t\"TargetBuildID\"\t\t\"101\"\n\t\"BytesToDownload\"\t\t\"5577232\"\n").update().unwrap();
        assert_eq!(small.describe("Steam"), "Update queued in Steam of 5.6 MB");
        let mid = m(6, "100", "\t\"TargetBuildID\"\t\t\"101\"\n\t\"BytesToDownload\"\t\t\"268014400\"\n").update().unwrap();
        assert_eq!(mid.describe("Steam"), "Update queued in Steam of 268 MB");
        // Staging, paused, failed, only-on-launch.
        assert_eq!(m(1026 | flags::STAGING, "100", "\t\"BytesToStage\"\t\t\"10\"\n\t\"BytesStaged\"\t\t\"5\"\n").update().unwrap().fraction(), Some(0.5));
        assert_eq!(m(1538, "100", "").update().unwrap().state, S::Paused);
        let f = m(6, "100", "\t\"UpdateResult\"\t\t\"12\"\n\t\"AutoUpdateBehavior\"\t\t\"1\"\n").update().unwrap();
        assert_eq!((f.error, f.on_launch), (Some(12), true));
        assert!(f.describe("Steam").contains("updates it when you play") && f.describe("Steam").contains("error 12"));
        // A first install still downloading is not an update.
        let first = m(1026 | flags::DOWNLOADING, "0", "\t\"BytesToDownload\"\t\t\"2000\"\n");
        assert!(!first.update_pending() && first.update().is_none() && !first.installed);
        // Steam checking the files of a current install shows too.
        assert_eq!(m(4 | flags::VALIDATING, "100", "").update().unwrap().state, S::Validating);
    }

    #[test]
    fn word_spans() {
        let s = r#"DXVK_CONFIG="a=1;b = 2" '/x y/quiver' a\ b %command% -bar"#;
        let w = words(s);
        let vals: Vec<&str> = w.iter().map(|x| x.2.as_str()).collect();
        assert_eq!(vals, ["DXVK_CONFIG=a=1;b = 2", "/x y/quiver", "a b", "%command%", "-bar"]);
        assert_eq!(&s[w[1].0..w[1].1], "'/x y/quiver'");
        assert_eq!(&s[w[3].0..w[3].1], "%command%");
        assert!(words("   ").is_empty());
    }

    #[test]
    fn patching_keeps_the_users_options() {
        // Nothing there.
        assert_eq!(patched("", Q), format!("{Q} %command%"));
        assert_eq!(patched("  ", Q), format!("{Q} %command%"));
        // Env and arguments around %command%.
        assert_eq!(patched("FOO=1 %command% -bar", Q), format!("FOO=1 {Q} %command% -bar"));
        let dxvk = r#"DXVK_CONFIG="dxgi.maxFrameRate=240;d3d9.maxFrameRate=240,dxgi.syncInterval = 0" %command%"#;
        assert_eq!(patched(dxvk, Q), format!(r#"DXVK_CONFIG="dxgi.maxFrameRate=240;d3d9.maxFrameRate=240,dxgi.syncInterval = 0" {Q} %command%"#));
        // A wrapper stays outermost.
        assert_eq!(patched("gamemoderun %command%", Q), format!("gamemoderun {Q} %command%"));
        // No %command%: they were game arguments.
        assert_eq!(patched("-skipvideos", Q), format!("{Q} %command% -skipvideos"));
        // Already patched (by Quiver or by hand): unchanged.
        let p = patched("FOO=1 %command% -bar", Q);
        assert_eq!(patched(&p, Q), p);
        assert_eq!(patched("quiver steam-run %command%", Q), "quiver steam-run %command%");
        let old = format!("FOO=1 {OLD} %command% -bar");
        assert_eq!(patched(&old, Q), old, "the form without --product counts as patched");
        let other = "PROTON_LOG=1 '/opt/my quiver/quiver' --source x steam-run --product 5 %command%";
        assert_eq!(patched(other, Q), other);
        // %command% inside a quoted wrapper: refused rather than guessed.
        assert!(check_patchable(r#"bash -c "exec %command% -x""#).is_err());
        assert!(check_patchable("FOO=1 %command%").is_ok() && check_patchable("-skipvideos").is_ok());
        // Not Quiver: a word merely containing "quiver", or quiver without steam-run.
        assert!(find_patch("/x/quiver-app %command%").is_none());
        assert!(find_patch("quiver play 5 %command%").is_none());
        assert!(find_patch("%command% quiver steam-run").is_none());
    }

    #[test]
    fn unpatching_restores() {
        let rec = |original: &str, written: &str| PatchRecord {
            user: "1".into(),
            app_id: 2,
            original: original.into(),
            written: written.into(),
            at: Utc::now(),
            added: None,
        };
        for original in ["", "FOO=1 %command% -bar", "-skipvideos", "gamemoderun %command%", "  odd  spacing %command%"] {
            // As Quiver wrote it (now, or before --product was always added): exactly what was there.
            for cmd in [Q, OLD] {
                let w = patched(original, cmd);
                assert_eq!(unpatched(&w, Some(&rec(original, &w))), original, "{original:?} {cmd:?}");
            }
        }
        // Edited since (or patched by hand): Quiver's part comes out, the rest stays.
        let r = rec("FOO=1 %command% -bar", &patched("FOO=1 %command% -bar", Q));
        assert_eq!(unpatched(&format!("FOO=2 {Q} %command% -bar"), Some(&r)), "FOO=2 %command% -bar");
        assert_eq!(unpatched("quiver steam-run %command%", None), "");
        assert_eq!(unpatched("X=1 ~/.local/bin/quiver --source steam steam-run %command% -y", None), "X=1 %command% -y");
        assert_eq!(unpatched(&format!("{Q} %command% -skipvideos"), None), "%command% -skipvideos");
        assert_eq!(unpatched(&format!("{OLD} %command% -skipvideos"), None), "%command% -skipvideos");
        // Not patched: unchanged.
        assert_eq!(unpatched("FOO=1 %command%", None), "FOO=1 %command%");
        // The command Quiver writes.
        assert_eq!(command(Path::new("/h/.local/bin/quiver"), Some(66088)), Q);
        assert_eq!(command(Path::new("/h/.local/bin/quiver"), None), OLD);
        assert_eq!(
            command(Path::new("/h/My Games/quiver"), Some(66088)),
            "'/h/My Games/quiver' --source steam steam-run --product 66088"
        );
    }

    const MANIFEST: &str = r#""AppState"
{
	"appid"		"3751950"
	"universe"		"1"
	"name"		"Assassin's Creed Black Flag Resynced"
	"StateFlags"		"4"
	"installdir"		"Assassin's Creed Black Flag Resynced"
	"buildid"		"25795103"
	"LastOwner"		"76561197960266729"
	"SizeOnDisk"		"52076634521"
	"LastPlayed"		"1763300000"
	"TargetBuildID"		"25795103"
	"InstalledDepots"
	{
		"3751951"
		{
			"manifest"		"2685713943691137149"
		}
		"3751960"
		{
			"manifest"		"1"
			"dlcappid"		"3751961"
		}
	}
	"InstallScripts"
	{
		"3751951"		"installscript_3751950.vdf"
	}
}
"#;

    const SCRIPT: &str = r#""InstallScript"
{
	"Registry"
	{
		"HKEY_LOCAL_MACHINE\\SOFTWARE\\Ubisoft\\Launcher\\Installs\\66088"
		{
			"string" { "english" { "language" "en-US" } }
		}
	}
}
"#;

    /// A Steam tree with two libraries: BF Resynced (install script with the id), a game
    /// with only Connect's loader (product from its prefix's registry), a game being
    /// downloaded, and a non-Ubisoft game.
    pub(crate) fn fixture() -> (tempfile::TempDir, Paths, Steam) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join(".local/share/Steam");
        let lib2 = tmp.path().join("games/SteamLibrary");
        for d in [root.join("steamapps"), lib2.join("steamapps"), root.join("userdata/1001/config"), root.join("config")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            root.join("config/loginusers.vdf"),
            "\"users\"\n{\n\t\"76561197960266729\"\n\t{\n\t\t\"AccountName\"\t\t\"alice\"\n\t\t\"PersonaName\"\t\t\"Alice\"\n\t}\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                root.display(),
                lib2.display()
            ),
        )
        .unwrap();
        // 1. BF Resynced in the main library.
        std::fs::write(root.join("steamapps/appmanifest_3751950.acf"), MANIFEST).unwrap();
        let bf = root.join("steamapps/common/Assassin's Creed Black Flag Resynced");
        std::fs::create_dir_all(&bf).unwrap();
        std::fs::write(bf.join("installscript_3751950.vdf"), SCRIPT).unwrap();
        // 2. Only the R2 loader in bin/, and a registry in its prefix.
        let m2 = MANIFEST.replace("3751950", "552520").replace("Assassin's Creed Black Flag Resynced", "Far Cry 5");
        std::fs::write(lib2.join("steamapps/appmanifest_552520.acf"), m2).unwrap();
        let fc5 = lib2.join("steamapps/common/Far Cry 5/bin");
        std::fs::create_dir_all(&fc5).unwrap();
        std::fs::write(fc5.join("upc_r2_loader64.dll"), b"x").unwrap();
        let pfx = lib2.join("steamapps/compatdata/552520/pfx");
        std::fs::create_dir_all(&pfx).unwrap();
        std::fs::write(pfx.join("system.reg"), "[Software\\\\Wow6432Node\\\\Ubisoft\\\\Launcher\\\\Installs\\\\1803] 1\n").unwrap();
        // 3. Still downloading.
        let m3 = MANIFEST.replace("3751950", "2225070").replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"1026\"").replace("Assassin's Creed Black Flag Resynced", "Prince of Persia");
        std::fs::write(root.join("steamapps/appmanifest_2225070.acf"), m3).unwrap();
        // 4. Not Ubisoft.
        let m4 = MANIFEST.replace("3751950", "620").replace("Assassin's Creed Black Flag Resynced", "Portal 2");
        std::fs::write(root.join("steamapps/appmanifest_620.acf"), m4).unwrap();
        std::fs::create_dir_all(root.join("steamapps/common/Portal 2/bin")).unwrap();
        std::fs::write(root.join("steamapps/common/Portal 2/bin/steam_api64.dll"), b"x").unwrap();
        std::fs::write(
            root.join("userdata/1001/config/localconfig.vdf"),
            "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"552520\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"PROTON_LOG=1 %command% -skipintro\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        let paths = Paths { data_home: tmp.path().join("data"), home: home.clone() };
        let steam = Steam::find(&home).unwrap();
        (tmp, paths, steam)
    }

    #[test]
    fn finds_ubisoft_games_in_every_library() {
        let (_tmp, _paths, steam) = fixture();
        assert_eq!(library_folders(&steam).len(), 2);
        let found = find_games(&steam);
        let got: Vec<(u32, Option<(u32, ProductSource)>)> = found.iter().map(|f| (f.app.app_id, f.product)).collect();
        assert_eq!(got, [(552520, Some((1803, ProductSource::Registry))), (3751950, Some((66088, ProductSource::InstallScript)))]);
        let bf = &found[1].app;
        assert_eq!(bf.name, "Assassin's Creed Black Flag Resynced");
        assert_eq!(bf.owner, Some(76561197960266729));
        assert_eq!((bf.size_on_disk, bf.last_played, bf.dlc_installed.as_slice()), (52_076_634_521, Some(1_763_300_000), &[3751961][..]));
        assert!(!bf.update_pending() && bf.installed);
        let pop = library_folders(&steam).iter().flat_map(|l| app_manifests(l)).find(|m| m.app_id == 2225070).unwrap();
        assert!(pop.update_pending() && !pop.installed, "StateFlags 1026: update running");
        assert!(bf.install_dir.ends_with("steamapps/common/Assassin's Creed Black Flag Resynced"));
        let users = steam.users().unwrap();
        assert_eq!(owner_user(bf, &users).as_deref(), Some("1001"));
    }

    #[test]
    fn patch_and_unpatch_with_steam_closed() {
        let (_tmp, paths, steam) = fixture();
        let file = localconfig(&steam, "1001");
        let before = std::fs::read_to_string(&file).unwrap();
        // A world-readable backup left by an older Quiver.
        let backup = file.with_extension("vdf.quiver-backup");
        std::fs::write(&backup, "stale").unwrap();
        std::fs::set_permissions(&backup, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        // An app with options already.
        let c = patch(&paths, &steam, "1001", 552520, "Far Cry 5", Q, false).unwrap();
        assert!(!c.staged && !c.unchanged);
        assert_eq!(c.launch_options, format!("PROTON_LOG=1 {Q} %command% -skipintro"));
        assert_eq!(launch_options(&steam, "1001", 552520), c.launch_options);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), before);
        // It holds Steam's tickets and tokens: owner-only.
        assert_eq!(crate::private::tests::mode(&backup), 0o600);
        let st = load(&paths);
        assert_eq!(st.patch_state("1001", 552520, &c.launch_options), PatchState::On);
        // Twice: nothing to do.
        assert!(patch(&paths, &steam, "1001", 552520, "Far Cry 5", Q, false).unwrap().unchanged);
        // An app without a section yet.
        let c = patch(&paths, &steam, "1001", 3751950, "BF", Q, false).unwrap();
        assert_eq!(c.launch_options, format!("{Q} %command%"));
        // Undo both: the file is byte-for-byte as before (the new app keeps an empty value).
        unpatch(&paths, &steam, "1001", 552520, "Far Cry 5", false).unwrap();
        assert_eq!(launch_options(&steam, "1001", 552520), "PROTON_LOG=1 %command% -skipintro");
        let c = unpatch(&paths, &steam, "1001", 3751950, "BF", false).unwrap();
        assert_eq!(c.launch_options, "");
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after.replace("\t\t\t\t\t\"3751950\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"\"\n\t\t\t\t\t}\n", ""), before);
        assert!(load(&paths).patches.is_empty());
        // Undo of one patched by hand.
        let hand = std::fs::read_to_string(&file).unwrap().replace("PROTON_LOG=1 %command%", "PROTON_LOG=1 quiver steam-run %command%");
        std::fs::write(&file, hand).unwrap();
        assert_eq!(load(&paths).patch_state("1001", 552520, &launch_options(&steam, "1001", 552520)), PatchState::ByHand);
        unpatch(&paths, &steam, "1001", 552520, "Far Cry 5", false).unwrap();
        assert_eq!(launch_options(&steam, "1001", 552520), "PROTON_LOG=1 %command% -skipintro");
        // Patched by an earlier Quiver, without --product: still on, left as it is, and undone exactly.
        let c = patch(&paths, &steam, "1001", 552520, "Far Cry 5", OLD, false).unwrap();
        assert_eq!(c.launch_options, format!("PROTON_LOG=1 {OLD} %command% -skipintro"));
        let c = patch(&paths, &steam, "1001", 552520, "Far Cry 5", Q, false).unwrap();
        assert!(c.unchanged);
        assert_eq!(c.launch_options, format!("PROTON_LOG=1 {OLD} %command% -skipintro"));
        assert_eq!(load(&paths).patch_state("1001", 552520, &c.launch_options), PatchState::On);
        unpatch(&paths, &steam, "1001", 552520, "Far Cry 5", false).unwrap();
        assert_eq!(launch_options(&steam, "1001", 552520), "PROTON_LOG=1 %command% -skipintro");
        assert!(load(&paths).patches.is_empty());
        // No localconfig.vdf for this user: refused, not created.
        std::fs::create_dir_all(steam.root.join("userdata/2002/config")).unwrap();
        assert!(patch(&paths, &steam, "2002", 552520, "Far Cry 5", Q, false).unwrap_err().to_string().contains("start Steam"));
        assert!(!localconfig(&steam, "2002").exists());
    }

    /// What a Steam restart writes: a change that can't be written stays staged, and that
    /// is a failure (not "Steam restarted").
    #[test]
    fn restart_apply_fails_when_something_stays_staged() {
        let (_tmp, paths, steam) = fixture();
        patch(&paths, &steam, "1001", 552520, "Far Cry 5", Q, true).unwrap();
        assert_eq!(super::super::steam::apply_all_or_fail(&paths).unwrap(), 1);
        assert_eq!(launch_options(&steam, "1001", 552520), format!("PROTON_LOG=1 {Q} %command% -skipintro"));
        // Nothing staged: fine, nothing written.
        assert_eq!(super::super::steam::apply_all_or_fail(&paths).unwrap(), 0);
        // Its file is gone by the time Steam is closed: the change stays, and it's an error.
        unpatch(&paths, &steam, "1001", 552520, "Far Cry 5", true).unwrap();
        std::fs::remove_file(localconfig(&steam, "1001")).unwrap();
        let e = super::super::steam::apply_all_or_fail(&paths).unwrap_err().to_string();
        assert!(e.contains("1 change(s) couldn't be written") && e.contains("Far Cry 5"), "{e}");
        assert_eq!(load(&paths).staged.len(), 1);
    }

    #[test]
    fn staged_while_steam_runs() {
        let (_tmp, paths, steam) = fixture();
        let file = localconfig(&steam, "1001");
        let before = std::fs::read_to_string(&file).unwrap();
        let c = patch(&paths, &steam, "1001", 552520, "Far Cry 5", Q, true).unwrap();
        assert!(c.staged);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "nothing written while Steam runs");
        let st = load(&paths);
        assert_eq!(st.patch_state("1001", 552520, &launch_options(&steam, "1001", 552520)), PatchState::Waiting);
        assert_eq!(staged_descriptions(&paths), ["start through Quiver: \"Far Cry 5\" (Steam app 552520) for Steam user 1001"]);
        // Undo while still staged: the staged patch is just dropped.
        let c = unpatch(&paths, &steam, "1001", 552520, "Far Cry 5", true).unwrap();
        assert!(c.unchanged && c.dropped_staged);
        assert!(load(&paths).staged.is_empty());
        patch(&paths, &steam, "1001", 552520, "Far Cry 5", Q, true).unwrap();
        // The shortcut code's apply writes these too (restart, or "write them now").
        assert!(super::super::steam::apply_staged(&paths, true).is_err());
        assert_eq!(super::super::steam::apply_staged(&paths, false).unwrap(), 1);
        assert_eq!(launch_options(&steam, "1001", 552520), format!("PROTON_LOG=1 {Q} %command% -skipintro"));
        assert_eq!(load(&paths).patch_state("1001", 552520, &launch_options(&steam, "1001", 552520)), PatchState::On);
        // Undo staged, then written by the next change with Steam closed.
        let c = unpatch(&paths, &steam, "1001", 552520, "Far Cry 5", true).unwrap();
        assert_eq!((c.staged, c.launch_options.as_str()), (true, "PROTON_LOG=1 %command% -skipintro"));
        assert_eq!(load(&paths).patch_state("1001", 552520, &launch_options(&steam, "1001", 552520)), PatchState::WaitingUndo);
        patch(&paths, &steam, "1001", 3751950, "BF", Q, false).unwrap();
        assert_eq!(launch_options(&steam, "1001", 552520), "PROTON_LOG=1 %command% -skipintro");
        assert!(load(&paths).staged.is_empty());
    }

    /// The hook for any Steam game, on fixture copies only.
    const HQ: &str = "/h/.local/bin/quiver --source steam steam-run --app 620";

    #[test]
    fn ubisoft_patches_are_undone_byte_for_byte_too() {
        let (_tmp, paths, steam) = fixture();
        let file = localconfig(&steam, "1001");
        let before = std::fs::read_to_string(&file).unwrap();
        // No section for the app yet: the patch adds one (G5), and undoing takes it out again.
        patch(&paths, &steam, "1001", 620, "Portal 2", HQ, false).unwrap();
        assert_eq!(load(&paths).record("1001", 620).unwrap().added, Some(Added::Section));
        unpatch(&paths, &steam, "1001", 620, "Portal 2", false).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "exactly as before");
    }

    #[test]
    fn hooks_any_steam_game_and_undoes_it_byte_for_byte() {
        let (_tmp, paths, steam) = fixture();
        let file = localconfig(&steam, "1001");
        let before = std::fs::read_to_string(&file).unwrap();
        assert_eq!(hook_command(Path::new("/h/.local/bin/quiver"), 620), HQ);
        let portal = find_app(&steam, 620).unwrap();
        let q = Path::new("/h/.local/bin/quiver");
        let s = hook_status(&paths, &steam, &portal, Some("1001"), Some(q), None, false);
        assert_eq!((s.state, s.hookable, s.command.as_str(), s.name.as_str()), (PatchState::Off, true, HQ, "Portal 2"));
        assert!(!s.hooked());

        // No section for the app yet: the hook adds one, and undoing takes it out again.
        let c = hook(&paths, &steam, "1001", 620, "Portal 2", HQ, false).unwrap();
        assert_eq!(c.launch_options, format!("{HQ} %command%"));
        assert_eq!(load(&paths).record("1001", 620).unwrap().added, Some(Added::Section));
        let s = hook_status(&paths, &steam, &portal, Some("1001"), Some(q), None, false);
        assert!(s.hooked() && s.state == PatchState::On);
        let c = unhook(&paths, &steam, "1001", 620, "Portal 2", false).unwrap();
        assert_eq!(c.launch_options, "");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "exactly as before");
        assert!(load(&paths).patches.is_empty());

        // Options of the user's own: kept around the hook, and back exactly.
        hook(&paths, &steam, "1001", 552520, "Far Cry 5", HQ, false).unwrap();
        assert_eq!(launch_options(&steam, "1001", 552520), format!("PROTON_LOG=1 {HQ} %command% -skipintro"));
        unhook(&paths, &steam, "1001", 552520, "Far Cry 5", false).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);

        // A section without LaunchOptions (Steam keeps play time there): only the key comes
        // and goes.
        let with_section = before.replace(
            "\t\t\t\t\t\"552520\"\n",
            "\t\t\t\t\t\"620\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"Playtime\"\t\t\"60\"\n\t\t\t\t\t}\n\t\t\t\t\t\"552520\"\n",
        );
        std::fs::write(&file, &with_section).unwrap();
        hook(&paths, &steam, "1001", 620, "Portal 2", HQ, false).unwrap();
        assert_eq!(load(&paths).record("1001", 620).unwrap().added, Some(Added::Key));
        unhook(&paths, &steam, "1001", 620, "Portal 2", false).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), with_section);

        // Steam wrote more into the section the hook added: undoing leaves Steam's part.
        std::fs::write(&file, &before).unwrap();
        hook(&paths, &steam, "1001", 620, "Portal 2", HQ, false).unwrap();
        let grown = std::fs::read_to_string(&file).unwrap().replace(
            &format!("\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{HQ} %command%\"\n"),
            &format!("\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{HQ} %command%\"\n"),
        );
        std::fs::write(&file, &grown).unwrap();
        unhook(&paths, &steam, "1001", 620, "Portal 2", false).unwrap();
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.contains("\"LastPlayed\"\t\t\"1\"") && !after.contains("steam-run"), "{after}");
        assert_eq!(launch_options_in(&after, 620), "");

        // Quoted %command%: not hookable, and refused.
        std::fs::write(&file, &before).unwrap();
        hook(&paths, &steam, "1001", 620, "Portal 2", HQ, false).unwrap();
        unhook(&paths, &steam, "1001", 620, "Portal 2", false).unwrap();
        let quoted = set_launch_options_in(&before, 620, r#"bash -c "exec %command%""#).unwrap();
        std::fs::write(&file, &quoted).unwrap();
        let s = hook_status(&paths, &steam, &portal, Some("1001"), Some(q), None, false);
        assert!(!s.hookable && s.note.as_deref().unwrap_or_default().contains("quotes"));
        assert!(hook(&paths, &steam, "1001", 620, "Portal 2", HQ, false).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), quoted);
        // No quiver, or no user: not hookable; a Ubisoft copy gets the --product command.
        let s = hook_status(&paths, &steam, &portal, None, None, None, false);
        assert!(!s.hookable && s.state == PatchState::Unknown && s.command.is_empty());
        let bf = find_app(&steam, 3751950).unwrap();
        let s = hook_status(&paths, &steam, &bf, Some("1001"), Some(q), Some(66088), false);
        assert_eq!(s.command, Q);
    }

    #[test]
    fn hooks_wait_for_steam_to_close() {
        let (_tmp, paths, steam) = fixture();
        let file = localconfig(&steam, "1001");
        let before = std::fs::read_to_string(&file).unwrap();
        let c = hook(&paths, &steam, "1001", 620, "Portal 2", HQ, true).unwrap();
        assert!(c.staged);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "nothing written while Steam runs");
        assert_eq!(load(&paths).patch_state("1001", 620, ""), PatchState::Waiting);
        assert_eq!(staged_descriptions(&paths), ["start through Quiver: \"Portal 2\" (Steam app 620) for Steam user 1001"]);
        // Off before Steam closed: the staged change is dropped, nothing was ever written.
        let c = unhook(&paths, &steam, "1001", 620, "Portal 2", true).unwrap();
        assert!(c.unchanged && c.dropped_staged);
        // On, written once Steam closed; off staged, then written: exactly as before.
        hook(&paths, &steam, "1001", 620, "Portal 2", HQ, true).unwrap();
        assert_eq!(super::super::steam::apply_staged(&paths, false).unwrap(), 1);
        assert_eq!(launch_options(&steam, "1001", 620), format!("{HQ} %command%"));
        let c = unhook(&paths, &steam, "1001", 620, "Portal 2", true).unwrap();
        assert!(c.staged);
        assert_eq!(load(&paths).patch_state("1001", 620, &launch_options(&steam, "1001", 620)), PatchState::WaitingUndo);
        assert_eq!(staged_descriptions(&paths), ["start without Quiver again: \"Portal 2\" (Steam app 620) for Steam user 1001"]);
        assert_eq!(super::super::steam::apply_staged(&paths, false).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        // A state file from before hooks (no `hook`, no `added`) still reads.
        let st: State = serde_json::from_str(r#"{"patches":[{"user":"1","app_id":2,"original":"","written":"x","at":"2026-11-01T00:00:00Z"}],"staged":[{"file":"/f","user":"1","app_id":2,"name":"n","op":"unpatch"}]}"#).unwrap();
        assert_eq!((st.patches[0].added, st.staged[0].hook), (None, false));
    }

    #[test]
    fn offers_only_for_later_installs() {
        let (_tmp, paths, _steam) = fixture();
        // First look: noted, not offered.
        assert!(note_installed(&paths, &[1, 2], true, |_| false).unwrap().is_empty());
        // A new one is offered once; one already using Quiver isn't.
        assert_eq!(note_installed(&paths, &[1, 2, 3, 4], true, |id| id == 4).unwrap(), [3]);
        assert!(note_installed(&paths, &[1, 2, 3, 4], true, |_| false).unwrap().is_empty());
        assert_eq!(load(&paths).offers.iter().copied().collect::<Vec<_>>(), [3]);
        // "Not now" sticks, even after an uninstall and reinstall.
        dismiss_offer(&paths, 3).unwrap();
        assert!(load(&paths).offers.is_empty());
        note_installed(&paths, &[1, 2], true, |_| false).unwrap();
        let mut st = load(&paths);
        st.seen.remove(&3);
        save(&paths, &st).unwrap();
        assert!(note_installed(&paths, &[1, 2, 3], true, |_| false).unwrap().is_empty());
        // Offers off: noted, not offered.
        assert!(note_installed(&paths, &[1, 2, 3, 5], false, |_| false).unwrap().is_empty());
        assert!(load(&paths).seen.contains(&5));
        // Uninstalled games leave the offers.
        assert_eq!(note_installed(&paths, &[1, 2, 3, 5, 6], true, |_| false).unwrap(), [6]);
        note_installed(&paths, &[1], true, |_| false).unwrap();
        assert!(load(&paths).offers.is_empty());
    }

    /// Read-only look at this machine's Steam:
    /// `cargo test -p quiver-core real_steam_games -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_steam_games() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let steam = Steam::find(&home).expect("no Steam");
        let users = steam.users().unwrap();
        for l in library_folders(&steam) {
            eprintln!("library {}", l.display());
        }
        let t = std::time::Instant::now();
        let found = find_games(&steam);
        eprintln!("{} Ubisoft games in {:?}", found.len(), t.elapsed());
        for f in found {
            let user = owner_user(&f.app, &users);
            let opts = user.as_deref().map(|u| launch_options(&steam, u, f.app.app_id)).unwrap_or_default();
            eprintln!("{} {:?} {:?} user {:?} options {opts:?}", f.app.app_id, f.app.name, f.product, user);
        }
        for u in &users {
            let text = std::fs::read_to_string(localconfig(&steam, &u.id)).unwrap_or_default();
            textvdf::parse(&text).expect("localconfig.vdf parses");
            // An edit of every app's options round-trips without touching anything else.
            let out = set_launch_options_in(&text, 3751950, &patched(&launch_options_in(&text, 3751950), Q)).unwrap();
            let back = set_launch_options_in(&out, 3751950, &launch_options_in(&text, 3751950)).unwrap();
            let had = text.contains("\"3751950\"\n\t\t\t\t\t{") && !launch_options_in(&text, 3751950).is_empty();
            eprintln!("user {}: localconfig {} bytes, round trip exact: {}", u.id, text.len(), back == text || !had);
        }
    }
}
