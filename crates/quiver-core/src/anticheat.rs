//! Anti-cheat in a game's install folder (design/SIGHT-HUD.md §2.4, SIGHT steer 13): the
//! Sight layer stays out of games that ship a kernel or user-mode anti-cheat unless the
//! person puts it in for that game.
//!
//! Conservative by design: a marker anywhere near the top of the install (four levels down,
//! where Unreal games keep `Binaries/Win64/EasyAntiCheat`) counts, whether or not the game
//! actually starts its anti-cheat. A false positive costs the HUD's default for one game
//! (`quiver hud enable GAME` overrides it); a false negative could cost an account.
//!
//! Read-only, bounded (at most [`MAX_ENTRIES`] directory entries), symlinks not followed.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// What was found: the anti-cheat's name and the marker, relative to the install folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub name: &'static str,
    pub marker: PathBuf,
}

impl Detected {
    /// "EasyAntiCheat (EasyAntiCheat/)".
    pub fn describe(&self) -> String {
        format!("{} ({})", self.name, self.marker.display())
    }
}

/// How deep below the install folder markers are looked for.
pub const MAX_DEPTH: usize = 4;
/// Directory entries looked at, at most (shallow ones first).
pub const MAX_ENTRIES: usize = 20_000;

/// Folder names (lowercase) that mean an anti-cheat.
const DIRS: &[(&str, &str)] = &[
    ("easyanticheat", "EasyAntiCheat"),
    ("easyanticheat_eos", "EasyAntiCheat"),
    ("battleye", "BattlEye"),
    ("gameguard", "nProtect GameGuard"),
    ("xigncode", "XIGNCODE3"),
    ("xigncode3", "XIGNCODE3"),
    ("anticheatexpert", "Anti-Cheat Expert"),
    ("equ8", "EQU8"),
    ("eaanticheat", "EA Javelin"),
];

/// File names (lowercase) that mean an anti-cheat.
const FILES: &[(&str, &str)] = &[
    ("start_protected_game.exe", "EasyAntiCheat"),
    ("easyanticheat_setup.exe", "EasyAntiCheat"),
    ("easyanticheat_eos_setup.exe", "EasyAntiCheat"),
    ("easyanticheat_x64.dll", "EasyAntiCheat"),
    ("easyanticheat_x86.dll", "EasyAntiCheat"),
    ("easyanticheat_x64.so", "EasyAntiCheat"),
    ("easyanticheat_eos.sys", "EasyAntiCheat"),
    ("beservice.exe", "BattlEye"),
    ("beservice_x64.exe", "BattlEye"),
    ("beclient.dll", "BattlEye"),
    ("beclient_x64.dll", "BattlEye"),
    ("bedaisy.sys", "BattlEye"),
    ("pnkbstra.exe", "PunkBuster"),
    ("pnkbstrb.exe", "PunkBuster"),
    ("pbsvc.exe", "PunkBuster"),
    ("gamemon.des", "nProtect GameGuard"),
    ("gamemon64.des", "nProtect GameGuard"),
    ("x3.xem", "XIGNCODE3"),
    ("eaanticheat.gameservicelauncher.exe", "EA Javelin"),
    ("eaanticheat.installer.exe", "EA Javelin"),
    ("mhyprot2.sys", "mhyprot"),
    ("mhyprot3.sys", "mhyprot"),
    ("vgc.exe", "Riot Vanguard"),
    ("vgk.sys", "Riot Vanguard"),
    ("equ8_conf.json", "EQU8"),
];

fn by_name(name: &str, is_dir: bool) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if is_dir {
        return DIRS.iter().find(|(d, _)| *d == n).map(|(_, a)| *a);
    }
    if let Some((_, a)) = FILES.iter().find(|(f, _)| *f == n) {
        return Some(a);
    }
    // BattlEye's launcher next to the game: `RainbowSix_BE.exe`, `DayZ_BE.exe`.
    (n.ends_with("_be.exe") && n.len() > "_be.exe".len()).then_some("BattlEye")
}

/// The first anti-cheat marker in `install_dir`, shallowest first; `None` when there is
/// none (or the folder can't be read).
pub fn detect(install_dir: &Path) -> Option<Detected> {
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(PathBuf::new(), 0)]);
    let mut seen = 0usize;
    while let Some((rel, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(install_dir.join(&rel)) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return None;
            }
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(a) = by_name(name, ft.is_dir()) {
                let marker = rel.join(name);
                let marker = if ft.is_dir() { PathBuf::from(format!("{}/", marker.display())) } else { marker };
                return Some(Detected { name: a, marker });
            }
            // Entries of `rel` are `depth + 1` levels down; walk into folders above the limit.
            if ft.is_dir() && depth + 1 < MAX_DEPTH {
                queue.push_back((rel.join(name), depth + 1));
            }
        }
    }
    None
}

/// Anti-cheat runtimes a Steam launch asks Proton for (Steam sets these for games whose
/// developers enabled EasyAntiCheat or BattlEye on Proton).
pub fn detect_env(env: &std::collections::BTreeMap<String, String>) -> Option<Detected> {
    [("PROTON_EAC_RUNTIME", "EasyAntiCheat"), ("PROTON_BATTLEYE_RUNTIME", "BattlEye")]
        .into_iter()
        .find(|(k, _)| env.get(*k).is_some_and(|v| !v.is_empty()))
        .map(|(k, name)| Detected { name, marker: PathBuf::from(format!("${k}")) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn markers() {
        let t = tempfile::tempdir().unwrap();
        touch(t.path(), "Game.exe");
        touch(t.path(), "data/textures/a.dds");
        assert_eq!(detect(t.path()), None, "a plain game");

        let eac = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(eac.path().join("EasyAntiCheat")).unwrap();
        assert_eq!(detect(eac.path()).unwrap().describe(), "EasyAntiCheat (EasyAntiCheat/)");

        let ue = tempfile::tempdir().unwrap();
        touch(ue.path(), "FortniteGame/Binaries/Win64/EasyAntiCheat/Settings.json");
        let d = detect(ue.path()).unwrap();
        assert_eq!((d.name, d.marker), ("EasyAntiCheat", PathBuf::from("FortniteGame/Binaries/Win64/EasyAntiCheat/")));

        let be = tempfile::tempdir().unwrap();
        touch(be.path(), "RainbowSix_BE.exe");
        assert_eq!(detect(be.path()).unwrap().name, "BattlEye");
        let be = tempfile::tempdir().unwrap();
        touch(be.path(), "bin/BEClient_x64.dll");
        assert_eq!(detect(be.path()).unwrap().name, "BattlEye");

        let spg = tempfile::tempdir().unwrap();
        touch(spg.path(), "start_protected_game.exe");
        assert_eq!(detect(spg.path()).unwrap().marker, PathBuf::from("start_protected_game.exe"));

        // Too deep to count (and not walked).
        let deep = tempfile::tempdir().unwrap();
        touch(deep.path(), "a/b/c/d/e/BattlEye/x");
        assert_eq!(detect(deep.path()), None);
        // Not "_BE.exe" on its own, nor a name merely containing it.
        let near = tempfile::tempdir().unwrap();
        touch(near.path(), "_be.exe");
        touch(near.path(), "tribe.exe");
        touch(near.path(), "battleye.txt");
        assert_eq!(detect(near.path()), None);
        assert_eq!(detect(Path::new("/nonexistent/quiver-test")), None);
    }

    #[test]
    fn steam_runtimes() {
        let mut env = std::collections::BTreeMap::new();
        assert_eq!(detect_env(&env), None);
        env.insert("PROTON_EAC_RUNTIME".to_owned(), "/s/steamapps/common/Proton EasyAntiCheat Runtime".to_owned());
        assert_eq!(detect_env(&env).unwrap().describe(), "EasyAntiCheat ($PROTON_EAC_RUNTIME)");
    }
}
