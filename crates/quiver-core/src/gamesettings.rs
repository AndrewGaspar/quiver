//! Which prefix folders hold a game's settings, which of their subfolders hold saves, and
//! snapshots of those folders for a new profile (design/PROFILES.md decisions 4–6).
//!
//! A profile with its own settings has `profile_config` naming these folders: each is
//! overlaid in the launch with the profile's copy (`mods/<dir>/profiles/<id>/prefix/<dir>`),
//! except its save subfolders, which stay the prefix's ([`crate::overlay::OverlaySpec::shared`]).
//!
//! Sources, first match per folder wins: [`KNOWN`] (audited), the folders of prefix files that
//! mods' config patches and Options surfaces name, and folders the game made in its own
//! prefix's user folders ([`infer`]). Every Ubisoft and Epic game Quiver installs gets a
//! prefix of its own, and Steam gives each Proton app one, so a folder there that isn't
//! Wine's or a runtime's is the game's.

use std::collections::BTreeSet;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use anyhow::{Context, Result};

/// A game's settings folder Quiver knows.
#[derive(Debug, Clone, Copy)]
pub struct Known {
    /// A game ref (`ubisoft:3539`, `epic:<app>`, `steam:<appid>`).
    pub game: &'static str,
    /// Relative to the prefix.
    pub dir: &'static str,
    /// Subfolders holding saves (kept shared), relative to `dir`.
    pub shared: &'static [&'static str],
    /// Where this was checked.
    pub audited: &'static str,
}

/// Audited settings folders, per game.
pub const KNOWN: &[Known] = &[
    Known {
        game: "epic:7315f614b0504081bcdceef138075c9c",
        dir: "drive_c/users/steamuser/Documents/Remedy/AlanWakeRemastered",
        shared: &["savegames"],
        audited: "2026-11-19: this machine's prefix: resolution.xml, keybinds.xml, config/ and savegames/savegame_aw1 (the saves, which Epic's catalog doesn't name)",
    },
    Known {
        game: "ubisoft:3539",
        dir: "drive_c/users/steamuser/Documents/Assassin's Creed Origins",
        shared: &[],
        audited: "2026-11-19: this machine's prefix: ACO.ini, ControlMappings.xml (saves are Connect's savegames)",
    },
    Known {
        game: "ubisoft:420",
        dir: "drive_c/users/steamuser/Documents/My Games/Far Cry 4",
        shared: &[],
        audited: "2026-11-19: this machine's prefix and the fc4-raw-mouse mod's GamerProfile.xml (per-account subfolders)",
    },
    Known {
        game: "ubisoft:1651",
        dir: "drive_c/users/steamuser/Documents/My Games/UnrealEngine3/ACCGame",
        shared: &[],
        audited: "2026-11-19: this machine's prefix and the built-in mods' PCConsole-*.ini",
    },
    Known {
        game: "ubisoft:1847",
        dir: "drive_c/users/steamuser/Documents/My Games/ACIndia",
        shared: &[],
        audited: "2026-11-19: the acc-india-skip-intro mod's PCConsole-ACCEngine.ini",
    },
];

/// The catalog's entries for a game.
pub fn known(game: &str) -> impl Iterator<Item = &'static Known> + '_ {
    KNOWN.iter().filter(move |k| k.game == game)
}

/// Folders in a user's profile folders that are Wine's, a runtime's or a launcher's, never
/// a game's (compared case-insensitively).
const NOT_GAMES: &[&str] = &[
    // Wine and Windows
    "microsoft", "temp", "downloads", "music", "pictures", "templates", "videos", "my music", "my pictures", "my videos",
    "desktop", "favorites", "links", "saved games", "contacts", "searches", "fonts", "packages", "programs", "crashdumps",
    "history", "temporary internet files", "wine", "openxr", "openvr",
    // graphics runtimes and caches
    "dxvk", "vkd3d-proton", "d3dscache", "nvidia", "nvidia corporation", "mesa_shader_cache", "amd", "intel",
    // launchers and their runtimes
    "ubisoft game launcher", "epicgameslauncher", "epic games", "epiconlineservices", "steam", "valve", "gog.com",
    "galaxyclient", "ea desktop", "electronic arts", "origin", "unrealenginelauncher", "comms", "cef",
];

fn not_a_game(name: &str) -> bool {
    let n = name.to_lowercase();
    NOT_GAMES.contains(&n.as_str()) || n.starts_with('.')
}

fn subdirs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

fn has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
}

/// The game's folders in a prefix's user folders, prefix-relative and sorted: each folder
/// in `Documents`, `Documents/My Games`, `AppData/Local`, `AppData/LocalLow` and
/// `AppData/Roaming` that isn't Wine's or a runtime's. A folder with no files and exactly
/// one subfolder is a publisher's (`Documents/Remedy`), so its subfolder is the game's.
pub fn infer(prefix: &Path) -> Vec<String> {
    let users = prefix.join("drive_c/users");
    let mut out = BTreeSet::new();
    for user in subdirs(&users).into_iter().filter(|u| !u.eq_ignore_ascii_case("public")) {
        for parent in ["Documents", "Documents/My Games", "AppData/Local", "AppData/LocalLow", "AppData/Roaming"] {
            let base = users.join(&user).join(parent);
            for name in subdirs(&base) {
                if not_a_game(&name) || (parent == "Documents" && name.eq_ignore_ascii_case("my games")) {
                    continue;
                }
                let mut rel = format!("drive_c/users/{user}/{parent}/{name}");
                let inner = subdirs(&base.join(&name));
                if inner.len() == 1 && !has_files(&base.join(&name)) && !not_a_game(&inner[0]) {
                    rel = format!("{rel}/{}", inner[0]);
                }
                out.insert(rel);
            }
        }
    }
    out.into_iter().collect()
}

/// Subfolders of `dir` (up to two levels down) whose name says they hold saves, relative to
/// `dir`. A save folder's own subfolders aren't listed again.
pub fn save_subdirs(dir: &Path) -> Vec<String> {
    let is_save = |n: &str| n.to_lowercase().contains("save");
    let mut out = Vec::new();
    for a in subdirs(dir) {
        if is_save(&a) {
            out.push(a);
            continue;
        }
        for b in subdirs(&dir.join(&a)) {
            if is_save(&b) {
                out.push(format!("{a}/{b}"));
            }
        }
    }
    out
}

/// The largest file a snapshot copies; bigger ones are left to the overlay (copied when the
/// game writes them).
pub const SNAPSHOT_MAX: u64 = 8 << 20;

/// Whether a snapshot leaves a path (relative to the settings folder) out: saves, crash
/// dumps, logs and caches.
fn skipped(rel: &str, shared: &[String]) -> bool {
    if shared.iter().any(|s| rel == s || rel.starts_with(&format!("{s}/"))) {
        return true;
    }
    let lower = rel.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name.ends_with(".dmp") || name.ends_with(".log") || name.contains(".quiver-") {
        return true;
    }
    lower.split('/').any(|c| c.ends_with("cache") || matches!(c, "crashes" | "crashdumps" | "report" | "reports" | "logs"))
}

/// Copy a settings folder's files into a profile's copy, for a new profile's snapshot.
/// `layers` are where the source's files are, topmost first (a profile's own copy, then
/// the prefix's folder): a file comes from the first layer that has it, and a whiteout
/// (overlayfs's deletion mark) in a layer hides it below. Files already in `dest` are kept.
/// Returns the files copied (relative).
pub fn snapshot(layers: &[&Path], dest: &Path, shared: &[String]) -> Result<Vec<String>> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut copied = Vec::new();
    for layer in layers {
        if !layer.is_dir() {
            continue;
        }
        let mut stack = vec![layer.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let e = e?;
                let path = e.path();
                let rel = path.strip_prefix(layer).unwrap_or(&path).to_string_lossy().into_owned();
                if skipped(&rel, shared) {
                    continue;
                }
                let ty = e.file_type()?;
                if ty.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !seen.insert(rel.clone()) {
                    continue;
                }
                if ty.is_char_device() || !ty.is_file() {
                    continue; // a whiteout hides the file below; symlinks and others aren't settings
                }
                if e.metadata()?.len() > SNAPSHOT_MAX {
                    continue;
                }
                let to = dest.join(&rel);
                if to.exists() {
                    continue;
                }
                std::fs::create_dir_all(to.parent().context("snapshot path")?)?;
                std::fs::copy(&path, &to).with_context(|| format!("copying {}", path.display()))?;
                // Keep the time, so "changed since" reads the same in both.
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    let _ = std::fs::File::options().write(true).open(&to).and_then(|f| f.set_modified(t));
                }
                copied.push(rel);
            }
        }
    }
    copied.sort();
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn infers_the_games_folders() {
        let t = tempfile::tempdir().unwrap();
        let u = t.path().join("drive_c/users/steamuser");
        for d in ["Documents/Downloads", "AppData/Local/Microsoft", "AppData/Local/dxvk", "AppData/Roaming/Microsoft", "AppData/Local/Ubisoft Game Launcher"] {
            std::fs::create_dir_all(u.join(d)).unwrap();
        }
        touch(&u.join("Documents/Remedy/AlanWakeRemastered/resolution.xml"), "<r/>");
        touch(&u.join("Documents/Remedy/AlanWakeRemastered/savegames/savegame_aw1/savegame.aws"), "s");
        touch(&u.join("Documents/My Games/Far Cry 4/1234/GamerProfile.xml"), "x");
        touch(&u.join("Documents/My Games/Far Cry 4/settings.ini"), "x");
        touch(&u.join("AppData/LocalLow/Ubisoft/Rabbids Coding/prefs"), "x");
        touch(&t.path().join("drive_c/users/Public/Documents/Shared/x"), "x");
        assert_eq!(
            infer(t.path()),
            [
                "drive_c/users/steamuser/AppData/LocalLow/Ubisoft/Rabbids Coding",
                "drive_c/users/steamuser/Documents/My Games/Far Cry 4",
                "drive_c/users/steamuser/Documents/Remedy/AlanWakeRemastered",
            ]
        );
        assert_eq!(save_subdirs(&u.join("Documents/Remedy/AlanWakeRemastered")), ["savegames"]);
        assert!(save_subdirs(&u.join("Documents/My Games/Far Cry 4")).is_empty());
    }

    #[test]
    fn snapshot_layers_saves_and_caches() {
        let t = tempfile::tempdir().unwrap();
        let (own, prefix, dest) = (t.path().join("own"), t.path().join("prefix"), t.path().join("dest"));
        touch(&prefix.join("resolution.xml"), "prefix");
        touch(&prefix.join("keybinds.xml"), "keys");
        touch(&prefix.join("savegames/s1/save.aws"), "save");
        touch(&prefix.join("cache/shader.bin"), "c");
        touch(&prefix.join("crash.dmp"), "d");
        touch(&own.join("resolution.xml"), "own");
        touch(&dest.join("keybinds.xml"), "already");
        let copied = snapshot(&[&own, &prefix], &dest, &["savegames".into()]).unwrap();
        assert_eq!(copied, ["resolution.xml"]);
        assert_eq!(std::fs::read_to_string(dest.join("resolution.xml")).unwrap(), "own");
        assert_eq!(std::fs::read_to_string(dest.join("keybinds.xml")).unwrap(), "already");
        assert!(!dest.join("savegames").exists() && !dest.join("cache").exists() && !dest.join("crash.dmp").exists());
    }

    #[test]
    fn catalog_entries_are_well_formed() {
        for k in KNOWN {
            assert!(quiver_api::GameRef::parse(k.game).is_ok(), "{}", k.game);
            assert!(k.dir.starts_with("drive_c/users/steamuser/"), "{}", k.dir);
            assert!(!k.audited.is_empty());
        }
    }
}
