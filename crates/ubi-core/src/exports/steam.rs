//! Steam non-Steam shortcuts: `userdata/<id>/config/shortcuts.vdf` plus grid art.
//!
//! Steam reads shortcuts.vdf when it starts and writes it back when it exits, so a change
//! written while Steam runs would be lost. While Steam runs, changes go to a staging file
//! (`<data>/steam-staged.json`) and are written the next time Quiver touches Steam's files
//! with Steam closed, on request ([`apply_staged`]), or by quiverd once it has seen Steam
//! closed for a while. Launch-option changes ([`super::steamgames`]) are staged the same way
//! and written together with these. Restarting Steam
//! ([`restart_steam`]) is only for front ends to call after the user explicitly confirms.
//!
//! Grid art isn't rewritten by Steam, so it's written straight away.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::vdf::{ShortcutFields, Shortcuts};
use super::{
    ExportKey, ExportKind, ExportRecord, Paths, SteamExportResult, SteamUser, Target, art, hash, load_records, lock,
    remove_record, upsert_record, write_atomic,
};

/// SteamID64 of account id 0 (individual accounts, public universe).
const STEAMID64_BASE: u64 = 76_561_197_960_265_728;
const FLATPAK_APP: &str = "com.valvesoftware.Steam";

/// A Steam installation.
#[derive(Debug, Clone)]
pub struct Steam {
    /// The directory holding `userdata/`, `config/`, `steam.sh`.
    pub root: PathBuf,
    /// Flatpak Steam: shortcuts run inside its sandbox, so they go through
    /// `flatpak-spawn --host`.
    pub flatpak: bool,
}

impl Steam {
    /// Native (`~/.steam/steam`, `~/.local/share/Steam`) first, then Flatpak.
    pub fn find(home: &Path) -> Option<Self> {
        let flatpak_base = home.join(".var/app").join(FLATPAK_APP);
        let candidates = [
            (home.join(".steam/steam"), false),
            (home.join(".local/share/Steam"), false),
            (flatpak_base.join(".local/share/Steam"), true),
            (flatpak_base.join("data/Steam"), true),
        ];
        candidates.into_iter().find(|(p, _)| p.join("userdata").is_dir()).map(|(p, flatpak)| Self {
            root: p.canonicalize().unwrap_or(p),
            flatpak,
        })
    }

    /// Accounts with a `userdata/<id>` directory (skipping `0`, Steam's placeholder for
    /// "nobody signed in"), named from `config/loginusers.vdf`.
    pub fn users(&self) -> Result<Vec<SteamUser>> {
        let names = self.login_users();
        let mut users: Vec<SteamUser> = std::fs::read_dir(self.root.join("userdata"))
            .context("read Steam userdata")?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let id = e.file_name().to_str()?.to_owned();
                let n: u64 = id.parse().ok().filter(|&n| n != 0)?;
                let steam_id64 = STEAMID64_BASE + n;
                let info = names.get(&steam_id64);
                Some(SteamUser {
                    id,
                    steam_id64,
                    persona_name: info.and_then(|i| i.0.clone()),
                    account_name: info.and_then(|i| i.1.clone()),
                    most_recent: info.is_some_and(|i| i.2),
                })
            })
            .collect();
        users.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(users)
    }

    /// SteamID64 -> (persona name, account name, most recent). Older clients mark the
    /// last login `MostRecent "1"`; current ones only keep a `Timestamp`, so without a
    /// `MostRecent` the newest timestamp counts.
    fn login_users(&self) -> BTreeMap<u64, (Option<String>, Option<String>, bool)> {
        let Ok(text) = std::fs::read_to_string(self.root.join("config/loginusers.vdf")) else { return BTreeMap::new() };
        let Ok(root) = super::textvdf::parse(&text) else { return BTreeMap::new() };
        let Some(users) = root.get("users") else { return BTreeMap::new() };
        let entries: Vec<(u64, &super::textvdf::Node)> =
            users.entries().iter().filter_map(|(id, u)| Some((id.parse().ok()?, u))).collect();
        let flagged = entries.iter().any(|(_, u)| u.str("MostRecent").is_some());
        let newest = entries
            .iter()
            .max_by_key(|(_, u)| u.str("Timestamp").and_then(|t| t.parse::<u64>().ok()).unwrap_or(0))
            .map(|(id, _)| *id);
        entries
            .iter()
            .map(|&(id, u)| {
                let recent = if flagged { u.str("MostRecent") == Some("1") } else { Some(id) == newest };
                (id, (u.str("PersonaName").map(str::to_owned), u.str("AccountName").map(str::to_owned), recent))
            })
            .collect()
    }

    pub fn shortcuts_path(&self, user: &str) -> PathBuf {
        self.root.join("userdata").join(user).join("config/shortcuts.vdf")
    }

    pub fn grid_dir(&self, user: &str) -> PathBuf {
        self.root.join("userdata").join(user).join("config/grid")
    }

    /// The command that runs this Steam (`flatpak run …`, `steam`, or `steam.sh`).
    fn command(&self) -> std::process::Command {
        if self.flatpak {
            let mut c = std::process::Command::new("flatpak");
            c.args(["run", FLATPAK_APP]);
            c
        } else if let Some(steam) = super::find_in_path("steam") {
            std::process::Command::new(steam)
        } else {
            std::process::Command::new(self.root.join("steam.sh"))
        }
    }
}

/// Whether a Steam client is running (any process named `steam`; Flatpak's are visible in
/// the host's /proc too).
pub fn is_running() -> bool {
    procs().any(|(comm, _)| comm == "steam")
}

/// Whether a game launched from Steam is running (Steam's `reaper SteamLaunch …`).
pub fn game_running() -> bool {
    procs().any(|(comm, cmdline)| comm == "reaper" && cmdline.contains("SteamLaunch"))
}

fn procs() -> impl Iterator<Item = (String, String)> {
    std::fs::read_dir("/proc").into_iter().flatten().filter_map(|e| {
        let e = e.ok()?;
        e.file_name().to_str()?.parse::<u32>().ok()?;
        let comm = std::fs::read_to_string(e.path().join("comm")).ok()?.trim_end().to_owned();
        let cmdline = std::fs::read(e.path().join("cmdline")).map(|b| String::from_utf8_lossy(&b).replace('\0', " ")).unwrap_or_default();
        Some((comm, cmdline))
    })
}

/// Pick the Steam user for an export.
///
/// - `requested` (id, SteamID64, persona or account name) wins;
/// - else the `remembered` choice (config.json `steam_user`), if that user still exists;
/// - else the only user;
/// - else an error listing the users: with several, the caller must choose.
pub fn choose_user(users: &[SteamUser], requested: Option<&str>, remembered: Option<&str>) -> Result<SteamUser> {
    let list = || {
        users
            .iter()
            .map(|u| format!("{} ({})", u.id, u.persona_name.as_deref().unwrap_or("?")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(r) = requested {
        let hits: Vec<&SteamUser> = users
            .iter()
            .filter(|u| {
                u.id == r
                    || u.steam_id64.to_string() == r
                    || u.persona_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(r))
                    || u.account_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(r))
            })
            .collect();
        return match hits[..] {
            [u] => Ok(u.clone()),
            [] => bail!("no Steam user '{r}' (users: {})", list()),
            _ => bail!("'{r}' matches several Steam users; use the id (users: {})", list()),
        };
    }
    if let Some(u) = remembered.and_then(|id| users.iter().find(|u| u.id == id)) {
        return Ok(u.clone());
    }
    match users {
        [u] => Ok(u.clone()),
        [] => bail!("no Steam users found (sign in to Steam once first)"),
        _ => bail!("several Steam users on this machine; choose one with --user (users: {})", list()),
    }
}

/// Our fields for a target's shortcut.
pub fn fields(target: &Target, icon: Option<&Path>, flatpak: bool) -> ShortcutFields {
    let quote = |s: &str| format!("\"{s}\"");
    let exe = target.quiver_exe.to_string_lossy().into_owned();
    let args = target.launch_args().join(" ");
    let start_dir = match target.quiver_exe.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(d) => format!("{}/", d.to_string_lossy().trim_end_matches('/')),
        None => "./".into(),
    };
    let (exe, launch_options) = if flatpak {
        // Steam's sandbox can't run host binaries directly.
        ("/usr/bin/flatpak-spawn".to_owned(), format!("--host {} {args}", quote(&exe)))
    } else {
        (exe, args)
    };
    ShortcutFields {
        appid: target.key.steam_appid(),
        app_name: target.name.clone(),
        exe: quote(&exe),
        start_dir: quote(&start_dir),
        icon: icon.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        launch_options,
    }
}

pub(crate) fn fields_hash(f: &ShortcutFields) -> String {
    hash(&serde_json::to_vec(f).unwrap_or_default())
}

// ---------------------------------------------------------------------------------------
// Grid art

/// Steam's custom-art names in `userdata/<id>/config/grid/`, keyed by the shortcut's
/// 32-bit appid in decimal (current clients; the old Big Picture 64-bit id is unused):
///
/// | file | what | Steam's size | from Ubisoft |
/// |---|---|---|---|
/// | `<appid>p.<ext>` | portrait capsule (library grid) | 600x900 | `thumb_image` (box art) |
/// | `<appid>.<ext>` | wide capsule (recent games, Big Picture) | 920x430 | `background_image` |
/// | `<appid>_hero.<ext>` | hero banner behind the game page header | 1920x620 | `background_image` |
/// | `<appid>_logo.<ext>` | logo drawn over the hero | any, transparent PNG | `logo_image` |
///
/// Steam accepts `.png` and `.jpg` for each (and `_icon` for the icon, which we set in
/// the shortcut's `icon` field instead). Files keep the CDN's extension; other extensions
/// for the same slot are removed so a stale one can't shadow ours.
pub fn grid_files(appid: u32, art: &ubi_api::Art) -> Vec<(String, String)> {
    let slots: [(&str, &Option<String>); 4] =
        [("p", &art.thumb), ("", &art.background), ("_hero", &art.background), ("_logo", &art.logo)];
    slots
        .into_iter()
        .filter_map(|(suffix, file)| {
            let file = file.as_ref()?;
            let ext = Path::new(file).extension()?.to_str()?.to_ascii_lowercase();
            Some((format!("{appid}{suffix}.{ext}"), file.clone()))
        })
        .collect()
}

const GRID_SUFFIXES: [&str; 4] = ["p", "", "_hero", "_logo"];
const GRID_EXTS: [&str; 3] = ["png", "jpg", "jpeg"];

async fn write_grid(paths: &Paths, grid: &Path, appid: u32, art: &ubi_api::Art) -> Result<()> {
    std::fs::create_dir_all(grid)?;
    for (name, file) in grid_files(appid, art) {
        let src = match art::fetch(&paths.art(), &file).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Steam art {file}: {e:#}");
                continue;
            }
        };
        let (stem, ext) = name.rsplit_once('.').unwrap();
        for other in GRID_EXTS.iter().filter(|&&x| x != ext) {
            let _ = std::fs::remove_file(grid.join(format!("{stem}.{other}")));
        }
        std::fs::copy(&src, grid.join(&name)).with_context(|| format!("copy art to {}", grid.display()))?;
    }
    Ok(())
}

fn remove_grid(grid: &Path, appid: u32) {
    for suffix in GRID_SUFFIXES {
        for ext in GRID_EXTS {
            let _ = std::fs::remove_file(grid.join(format!("{appid}{suffix}.{ext}")));
        }
    }
}

// ---------------------------------------------------------------------------------------
// Staging

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StagedChange {
    Upsert { fields: ShortcutFields },
    Remove { appid: u32 },
}

/// A shortcut change waiting for Steam to close.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Staged {
    pub shortcuts: PathBuf,
    pub user: String,
    pub key: String,
    pub name: String,
    #[serde(flatten)]
    pub change: StagedChange,
}

impl Staged {
    fn appid(&self) -> u32 {
        match &self.change {
            StagedChange::Upsert { fields } => fields.appid,
            StagedChange::Remove { appid } => *appid,
        }
    }

    pub fn describe(&self) -> String {
        let what = match self.change {
            StagedChange::Upsert { .. } => "add/update",
            StagedChange::Remove { .. } => "remove",
        };
        format!("{what} \"{}\" ({}) for Steam user {}", self.name, self.key, self.user)
    }
}

pub fn staged(paths: &Paths) -> Vec<Staged> {
    std::fs::read_to_string(paths.staged()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_staged(paths: &Paths, list: &[Staged]) -> Result<()> {
    if list.is_empty() {
        return match std::fs::remove_file(paths.staged()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    write_atomic(&paths.staged(), &serde_json::to_vec_pretty(list)?)
}

/// Apply changes to shortcuts.vdf files (grouped per file, in order) and mark the
/// matching records written. Caller holds the lock and has checked Steam isn't running.
fn apply(paths: &Paths, changes: &[Staged]) -> Result<()> {
    let mut files: Vec<&Path> = changes.iter().map(|c| c.shortcuts.as_path()).collect();
    files.sort();
    files.dedup();
    for file in files {
        let old = match std::fs::read(file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
        };
        let mut sc = Shortcuts::parse(&old).with_context(|| format!("parse {}", file.display()))?;
        for c in changes.iter().filter(|c| c.shortcuts == file) {
            match &c.change {
                StagedChange::Upsert { fields } => {
                    sc.upsert(fields);
                }
                StagedChange::Remove { appid } => {
                    sc.remove(*appid);
                }
            }
        }
        let new = sc.to_bytes();
        if new != old {
            if !old.is_empty() {
                // One backup of the file as it was before Quiver's last write.
                let _ = std::fs::write(file.with_extension("vdf.quiver-backup"), &old);
            }
            write_atomic(file, &new)?;
        }
    }
    let now = chrono::Utc::now();
    let mut records = load_records(paths);
    let mut dirty = false;
    for r in records.iter_mut().filter(|r| r.kind == ExportKind::Steam && r.staged) {
        if changes.iter().any(|c| c.key == r.key && c.shortcuts == r.path) {
            r.staged = false;
            r.written_at = now;
            dirty = true;
        }
    }
    if dirty {
        super::save_records(paths, &records)?;
    }
    Ok(())
}

/// Queue `change`, replacing any earlier staged change for the same shortcut, or (Steam
/// closed) write it together with everything staged before. Returns how many earlier
/// staged changes were written. Caller holds the lock.
fn stage_or_apply(paths: &Paths, change: Staged, running: bool) -> Result<usize> {
    let mut list = staged(paths);
    list.retain(|s| !(s.shortcuts == change.shortcuts && s.appid() == change.appid()));
    if running {
        list.push(change);
        save_staged(paths, &list)?;
        return Ok(0);
    }
    let earlier = list.len();
    list.push(change);
    apply(paths, &list)?;
    save_staged(paths, &[])?;
    Ok(earlier)
}

/// Write everything staged (Steam must be closed): shortcut changes and launch-option
/// changes ([`super::steamgames`]). Returns how many changes were written. Never starts or
/// stops Steam.
pub fn apply_staged(paths: &Paths, running: bool) -> Result<usize> {
    let _lock = lock(paths)?;
    let list = staged(paths);
    let launch = super::steamgames::staged_count(paths);
    if list.is_empty() && launch == 0 {
        return Ok(0);
    }
    if running {
        bail!("Steam is running; {} change(s) stay staged until it closes (quiver steam restart)", list.len() + launch);
    }
    if !list.is_empty() {
        apply(paths, &list)?;
        save_staged(paths, &[])?;
    }
    Ok(list.len() + super::steamgames::apply_staged_locked(paths)?)
}

/// Every change waiting for Steam to close, described: shortcuts, then launch options.
pub fn staged_descriptions(paths: &Paths) -> Vec<String> {
    let mut v: Vec<String> = staged(paths).iter().map(Staged::describe).collect();
    v.extend(super::steamgames::staged_descriptions(paths));
    v
}

// ---------------------------------------------------------------------------------------
// Export / remove

/// Add or update the target's shortcut for `user`, with grid art. `running`: whether
/// Steam is running right now ([`is_running`]); if so the shortcut is staged.
pub async fn export(paths: &Paths, steam: &Steam, user: &str, target: &Target, running: bool) -> Result<SteamExportResult> {
    let appid = target.key.steam_appid();
    let icon = match &target.art.icon {
        Some(file) => art::icon_png(&paths.icons(), file).await.map_err(|e| tracing::warn!("icon: {e:#}")).ok(),
        None => None,
    };
    write_grid(paths, &steam.grid_dir(user), appid, &target.art).await?;
    let f = fields(target, icon.as_deref(), steam.flatpak);
    let shortcuts = steam.shortcuts_path(user);
    let _lock = lock(paths)?;
    let record = ExportRecord {
        key: target.key.to_string(),
        kind: ExportKind::Steam,
        product_id: target.key.product_id,
        profile_id: target.key.profile_id.clone(),
        name: target.name.clone(),
        path: shortcuts.clone(),
        appid: Some(appid),
        steam_user: Some(user.to_owned()),
        written_at: chrono::Utc::now(),
        hash: fields_hash(&f),
        staged: running,
    };
    // A shortcut moved to another user: drop it from the old one.
    let mut moved = None;
    if let Some(old) = load_records(paths).into_iter().find(|r| r.key == record.key && r.kind == ExportKind::Steam && r.path != shortcuts)
    {
        moved = Some(Staged {
            shortcuts: old.path.clone(),
            user: old.steam_user.clone().unwrap_or_default(),
            key: old.key.clone(),
            name: old.name.clone(),
            change: StagedChange::Remove { appid },
        });
    }
    upsert_record(paths, record)?;
    let mut applied = 0;
    if let Some(m) = moved {
        if let Some(grid) = m.shortcuts.parent().map(|c| c.join("grid")) {
            remove_grid(&grid, appid);
        }
        applied += stage_or_apply(paths, m, running)?;
    }
    let change = Staged {
        shortcuts,
        user: user.to_owned(),
        key: target.key.to_string(),
        name: target.name.clone(),
        change: StagedChange::Upsert { fields: f },
    };
    applied += stage_or_apply(paths, change, running)?;
    Ok(SteamExportResult { key: target.key.to_string(), appid, steam_user: user.to_owned(), staged: running, applied_staged: applied })
}

/// Remove the key's shortcut and grid art. Uses the recorded user when there is one,
/// else `user`.
pub fn remove(paths: &Paths, steam: &Steam, user: Option<&str>, key: &ExportKey, running: bool) -> Result<SteamExportResult> {
    let appid = key.steam_appid();
    let _lock = lock(paths)?;
    let record = load_records(paths).into_iter().find(|r| r.key == key.to_string() && r.kind == ExportKind::Steam);
    let (shortcuts, user, name) = match &record {
        Some(r) => (r.path.clone(), r.steam_user.clone().unwrap_or_default(), r.name.clone()),
        None => {
            let user = user.context("no Steam shortcut recorded for this game; pass the Steam user")?;
            (steam.shortcuts_path(user), user.to_owned(), key.to_string())
        }
    };
    if let Some(grid) = shortcuts.parent().map(|c| c.join("grid")) {
        remove_grid(&grid, appid);
    }
    remove_record(paths, &key.to_string(), ExportKind::Steam)?;
    let change = Staged { shortcuts, user: user.clone(), key: key.to_string(), name, change: StagedChange::Remove { appid } };
    let applied = stage_or_apply(paths, change, running)?;
    Ok(SteamExportResult { key: key.to_string(), appid, steam_user: user, staged: running, applied_staged: applied })
}

// ---------------------------------------------------------------------------------------
// Restart

pub use ubi_api::exports::RestartReport;

/// Write everything staged (Steam closed) and check nothing is left: a change that failed to
/// write stays staged, and that's an error here rather than a quiet success.
pub(crate) fn apply_all_or_fail(paths: &Paths) -> Result<usize> {
    let applied = apply_staged(paths, false)?;
    let left = staged_descriptions(paths);
    if !left.is_empty() {
        bail!(
            "{} change(s) couldn't be written and still wait for Steam (see quiverd's log): {}",
            left.len(),
            left.join("; ")
        );
    }
    Ok(applied)
}

/// Close Steam cleanly, write staged changes (shortcuts and launch options) and start Steam again.
///
/// **Only call this after the user explicitly confirmed it, every time.** Never on the
/// daemon's own initiative (it runs this only for a confirmed `Request::SteamRestart`), as a
/// side effect, or because an agent asked. Refuses while a game launched
/// from Steam is running. If Steam doesn't exit within `timeout` nothing is written (and
/// Steam is not killed). If Steam wasn't running, only writes the staged changes. Steam is
/// started again even when writing failed; the error then says what still waits.
pub fn restart_steam(paths: &Paths, steam: &Steam, timeout: Duration) -> Result<RestartReport> {
    if game_running() {
        bail!("a game launched from Steam is running; restart Steam after it exits");
    }
    let was_running = is_running();
    if was_running {
        let status = steam
            .command()
            .arg("-shutdown")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("ask Steam to shut down")?;
        tracing::info!("steam -shutdown: {status}");
        let deadline = std::time::Instant::now() + timeout;
        while is_running() {
            if std::time::Instant::now() > deadline {
                bail!("Steam didn't exit within {}s; nothing was changed", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // Steam writes shortcuts.vdf as it exits; give it a moment after the process ends.
        std::thread::sleep(Duration::from_secs(1));
    }
    let applied = apply_all_or_fail(paths);
    if was_running {
        use std::os::unix::process::CommandExt;
        let started = steam
            .command()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .context("start Steam again");
        if let Err(e) = started {
            return Err(match applied {
                Ok(_) => e,
                Err(a) => e.context(format!("{a:#}")),
            });
        }
    }
    let applied = applied.with_context(|| {
        if was_running { "Steam was restarted, but not everything was written" } else { "not everything was written" }
    })?;
    Ok(RestartReport { was_running, applied, started: was_running })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::{ExportState, list};

    /// A fixture Steam tree: two users with loginusers.vdf, one with an existing
    /// shortcuts.vdf (another launcher's entry).
    fn fixture() -> (tempfile::TempDir, Paths, Steam) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join(".local/share/Steam");
        for id in ["0", "1001", "2002"] {
            std::fs::create_dir_all(root.join("userdata").join(id).join("config")).unwrap();
        }
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config/loginusers.vdf"),
            format!(
                "\"users\"\n{{\n\t\"{}\"\n\t{{\n\t\t\"AccountName\"\t\t\"alice\"\n\t\t\"PersonaName\"\t\t\"Alice\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t}}\n\t\"{}\"\n\t{{\n\t\t\"AccountName\"\t\t\"bob\"\n\t\t\"PersonaName\"\t\t\"Bob\"\n\t}}\n}}\n",
                STEAMID64_BASE + 1001,
                STEAMID64_BASE + 2002
            ),
        )
        .unwrap();
        let mut other = Shortcuts::default();
        other.upsert(&ShortcutFields {
            appid: 0xd6c2_369a,
            app_name: "EA App".into(),
            exe: "\"lutris\"".into(),
            start_dir: "\"/home/u\"".into(),
            icon: String::new(),
            launch_options: "lutris:rungameid/5".into(),
        });
        std::fs::write(root.join("userdata/1001/config/shortcuts.vdf"), other.to_bytes()).unwrap();
        let paths = Paths { data_home: tmp.path().join("data"), home: home.clone() };
        let steam = Steam::find(&home).unwrap();
        (tmp, paths, steam)
    }

    fn target(paths: &Paths, name: &str) -> Target {
        // Real file names from design/art/art.json (AC Chronicles China), pre-cached so
        // nothing is fetched.
        let art = ubi_api::Art {
            thumb: Some("79b8e5e2468fb3f434b622ee5c5063fb.jpg".into()),
            background: Some("fe567514ffeb0e115d6bc50623f3abd5.jpg".into()),
            logo: Some("aa3e99ad561e41b93d2d2a5a6875409c.png".into()),
            icon: Some("48edec3d91f23947b02be534159b74d1.ico".into()),
            ..Default::default()
        };
        std::fs::create_dir_all(paths.art()).unwrap();
        std::fs::create_dir_all(paths.icons()).unwrap();
        for f in [&art.thumb, &art.background, &art.logo] {
            std::fs::write(paths.art().join(f.as_ref().unwrap()), f.as_ref().unwrap().as_bytes()).unwrap();
        }
        std::fs::write(paths.icons().join(art.icon.as_ref().unwrap()), crate::exports::art::tests::fixture_ico()).unwrap();
        Target { key: ExportKey::game(1651), name: name.into(), art, wm_class: None, quiver_exe: "/opt/quiver/quiver".into() }
    }

    /// Read-only look at this machine's Steam: `cargo test -p ubi-core real_steam -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_steam_users() {
        let steam = Steam::find(&PathBuf::from(std::env::var_os("HOME").unwrap())).expect("no Steam");
        eprintln!("root {} flatpak {} running {} game {}", steam.root.display(), steam.flatpak, is_running(), game_running());
        for u in steam.users().unwrap() {
            eprintln!("{} {} {:?} recent={}", u.id, u.steam_id64, u.persona_name, u.most_recent);
        }
    }

    #[test]
    fn users_and_choice() {
        let (_tmp, _paths, steam) = fixture();
        let users = steam.users().unwrap();
        assert_eq!(users.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(), ["1001", "2002"]);
        assert_eq!(users[0].persona_name.as_deref(), Some("Alice"));
        assert!(users[0].most_recent && !users[1].most_recent);
        assert_eq!(users[1].steam_id64, 76561197960267730);

        assert!(choose_user(&users, None, None).unwrap_err().to_string().contains("several Steam users"));
        assert_eq!(choose_user(&users, None, Some("2002")).unwrap().id, "2002");
        assert!(choose_user(&users, None, Some("9999")).is_err());
        assert_eq!(choose_user(&users, Some("bob"), Some("1001")).unwrap().id, "2002");
        assert_eq!(choose_user(&users, Some("ALICE"), None).unwrap().id, "1001");
        assert_eq!(choose_user(&users, Some(&(STEAMID64_BASE + 2002).to_string()), None).unwrap().id, "2002");
        assert!(choose_user(&users, Some("carol"), None).is_err());
        assert_eq!(choose_user(&users[..1], None, None).unwrap().id, "1001");
        assert!(choose_user(&[], None, None).is_err());
    }

    #[test]
    fn finds_flatpak() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Steam::find(tmp.path()).is_none());
        let root = tmp.path().join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
        std::fs::create_dir_all(root.join("userdata")).unwrap();
        let s = Steam::find(tmp.path()).unwrap();
        assert!(s.flatpak);
        let t = Target {
            key: ExportKey::game(1),
            name: "x".into(),
            art: Default::default(),
            wm_class: None,
            quiver_exe: "/opt/q/quiver".into(),
        };
        let f = fields(&t, None, true);
        assert_eq!(f.exe, "\"/usr/bin/flatpak-spawn\"");
        assert_eq!(f.launch_options, "--host \"/opt/q/quiver\" play 1");
    }

    #[test]
    fn grid_names() {
        let art = ubi_api::Art {
            thumb: Some("a.jpg".into()),
            background: Some("b.JPG".into()),
            logo: Some("c.png".into()),
            icon: None,
            ..Default::default()
        };
        let names: Vec<String> = grid_files(42, &art).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["42p.jpg", "42.jpg", "42_hero.jpg", "42_logo.png"]);
    }

    #[tokio::test]
    async fn direct_write_when_steam_closed() {
        let (_tmp, paths, steam) = fixture();
        let t = target(&paths, "AC Chronicles China");
        let appid = t.key.steam_appid();
        let vdf_path = steam.shortcuts_path("1001");
        let before = std::fs::read(&vdf_path).unwrap();

        let r = export(&paths, &steam, "1001", &t, false).await.unwrap();
        assert!(!r.staged);
        assert_eq!(r.appid, appid);
        let sc = Shortcuts::parse(&std::fs::read(&vdf_path).unwrap()).unwrap();
        assert_eq!(sc.entries().count(), 2);
        let f = sc.fields(appid).unwrap();
        assert_eq!(f.app_name, "AC Chronicles China");
        assert_eq!(f.exe, "\"/opt/quiver/quiver\"");
        assert_eq!(f.start_dir, "\"/opt/quiver/\"");
        assert_eq!(f.launch_options, "play 1651");
        assert!(f.icon.ends_with("quiver/icons/48edec3d91f23947b02be534159b74d1.png"), "{}", f.icon);
        assert_eq!(sc.fields(0xd6c2_369a).unwrap().app_name, "EA App");
        // Backup of the original.
        assert_eq!(std::fs::read(vdf_path.with_extension("vdf.quiver-backup")).unwrap(), before);

        let grid = steam.grid_dir("1001");
        for (name, src) in [
            (format!("{appid}p.jpg"), "79b8e5e2468fb3f434b622ee5c5063fb.jpg"),
            (format!("{appid}.jpg"), "fe567514ffeb0e115d6bc50623f3abd5.jpg"),
            (format!("{appid}_hero.jpg"), "fe567514ffeb0e115d6bc50623f3abd5.jpg"),
            (format!("{appid}_logo.png"), "aa3e99ad561e41b93d2d2a5a6875409c.png"),
        ] {
            assert_eq!(std::fs::read_to_string(grid.join(&name)).unwrap(), src, "{name}");
        }
        let l = list(&paths);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].state, ExportState::Ok);

        // Rename: same appid, one entry.
        export(&paths, &steam, "1001", &target(&paths, "Assassin's Creed® Chronicles China"), false).await.unwrap();
        let sc = Shortcuts::parse(&std::fs::read(&vdf_path).unwrap()).unwrap();
        assert_eq!(sc.entries().count(), 2);
        assert_eq!(sc.fields(appid).unwrap().app_name, "Assassin's Creed® Chronicles China");

        // Edited outside Quiver.
        let mut sc = sc;
        let mut f = sc.fields(appid).unwrap();
        f.launch_options = "something else".into();
        sc.upsert(&f);
        std::fs::write(&vdf_path, sc.to_bytes()).unwrap();
        assert_eq!(list(&paths)[0].state, ExportState::Changed);

        // Remove: entry, art and record go; the other launcher's entry stays, byte-exact.
        remove(&paths, &steam, None, &t.key, false).unwrap();
        assert_eq!(std::fs::read(&vdf_path).unwrap(), before);
        assert!(std::fs::read_dir(&grid).unwrap().next().is_none());
        assert!(list(&paths).is_empty());
    }

    #[tokio::test]
    async fn staged_while_running() {
        let (_tmp, paths, steam) = fixture();
        let t = target(&paths, "AC Chronicles China");
        let appid = t.key.steam_appid();
        let vdf_path = steam.shortcuts_path("2002");
        assert!(!vdf_path.exists());

        let r = export(&paths, &steam, "2002", &t, true).await.unwrap();
        assert!(r.staged);
        assert!(!vdf_path.exists(), "nothing written while Steam runs");
        assert!(steam.grid_dir("2002").join(format!("{appid}p.jpg")).is_file(), "art is written right away");
        assert_eq!(list(&paths)[0].state, ExportState::WaitingForSteam);
        // Staging twice keeps one change (latest wins).
        export(&paths, &steam, "2002", &target(&paths, "Renamed"), true).await.unwrap();
        let s = staged(&paths);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "Renamed");

        assert!(apply_staged(&paths, true).is_err());
        assert!(!vdf_path.exists());

        // The next write with Steam closed applies the staged change too.
        let t2 = Target { key: ExportKey::game(5183), name: "AC III".into(), ..target(&paths, "x") };
        let r = export(&paths, &steam, "2002", &t2, false).await.unwrap();
        assert_eq!(r.applied_staged, 1);
        let sc = Shortcuts::parse(&std::fs::read(&vdf_path).unwrap()).unwrap();
        assert_eq!(sc.fields(appid).unwrap().app_name, "Renamed");
        assert_eq!(sc.fields(t2.key.steam_appid()).unwrap().app_name, "AC III");
        assert!(staged(&paths).is_empty());
        assert!(list(&paths).iter().all(|s| s.state == ExportState::Ok));

        // Removal while running is staged; applying on request writes it.
        remove(&paths, &steam, None, &t.key, true).unwrap();
        assert!(Shortcuts::parse(&std::fs::read(&vdf_path).unwrap()).unwrap().find(appid).is_some());
        assert_eq!(apply_staged(&paths, false).unwrap(), 1);
        let sc = Shortcuts::parse(&std::fs::read(&vdf_path).unwrap()).unwrap();
        assert!(sc.find(appid).is_none());
        assert_eq!(sc.entries().count(), 1);
        assert_eq!(apply_staged(&paths, false).unwrap(), 0);
    }

    #[tokio::test]
    async fn moving_to_another_user() {
        let (_tmp, paths, steam) = fixture();
        let t = target(&paths, "AC");
        let appid = t.key.steam_appid();
        export(&paths, &steam, "1001", &t, false).await.unwrap();
        export(&paths, &steam, "2002", &t, false).await.unwrap();
        let a = Shortcuts::parse(&std::fs::read(steam.shortcuts_path("1001")).unwrap()).unwrap();
        let b = Shortcuts::parse(&std::fs::read(steam.shortcuts_path("2002")).unwrap()).unwrap();
        assert!(a.find(appid).is_none() && b.find(appid).is_some());
        assert!(!steam.grid_dir("1001").join(format!("{appid}p.jpg")).exists());
        let l = list(&paths);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].record.steam_user.as_deref(), Some("2002"));
    }
}
