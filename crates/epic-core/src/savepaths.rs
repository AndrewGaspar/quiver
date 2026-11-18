//! Where an Epic game keeps the saves the launcher syncs: the catalog's `CloudSaveFolder`
//! with its path variables resolved inside the game's Proton prefix (design/STORE-EPIC.md
//! §3 C9), and which files in it count (`CloudIncludeList`, `CloudExcludeList`).
//!
//! Variables, matched without regard to case (the catalog spells them `{AppData}`,
//! `{appdata}`, `{EpicId}` …):
//!
//! | Variable | In the prefix |
//! |---|---|
//! | `{AppData}`, `{LocalAppData}` | `drive_c/users/<user>/AppData/Local` (catalogs write `{AppData}/../LocalLow` and `{AppData}/../Roaming`) |
//! | `{RoamingAppData}` | `drive_c/users/<user>/AppData/Roaming` |
//! | `{UserDir}` | `drive_c/users/<user>/Documents` (EGL's meaning: "My Games" lives under it) |
//! | `{UserProfile}` | `drive_c/users/<user>` |
//! | `{UserSavedGames}` | `drive_c/users/<user>/Saved Games` |
//! | `{InstallDir}` | the game's install folder |
//! | `{EpicID}` | the Epic account id |
//!
//! `<user>` is Proton's `steamuser` (or the prefix's only user). A Windows drive path
//! (`C:\…`) maps into `drive_c`. A macOS-only variable (`{UserLibrary}`), an unknown one, or
//! a placeholder EGL itself can't fill (`[username]`) makes the folder unresolvable, and
//! the reason says which. Each path component is matched case-insensitively against what
//! exists, as Wine does, so `locallow` finds `LocalLow`.

use std::path::{Component, Path, PathBuf};

/// What path variables resolve against.
#[derive(Debug, Clone, Copy)]
pub struct Vars<'a> {
    /// The game's Wine prefix (the folder holding `drive_c`).
    pub prefix: &'a Path,
    pub install_dir: Option<&'a Path>,
    pub epic_id: &'a str,
}

/// The prefix's Windows user: `steamuser` (Proton's), else the only other user folder.
pub fn wine_user(prefix: &Path) -> String {
    let users: Vec<String> = std::fs::read_dir(prefix.join("drive_c/users"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.eq_ignore_ascii_case("public"))
        .collect();
    if users.iter().any(|u| u == "steamuser") || users.len() != 1 {
        return "steamuser".into();
    }
    users.into_iter().next().unwrap_or_else(|| "steamuser".into())
}

/// The local folder for `template` (a `CloudSaveFolder` value), or why there is none.
pub fn resolve(template: &str, vars: Vars<'_>) -> Result<PathBuf, String> {
    let t = template.trim();
    if t.is_empty() {
        return Err("the catalog's save folder is empty".into());
    }
    if t.contains('[') && t.contains(']') {
        return Err(format!("the catalog's save folder ({t}) has a placeholder the launcher can't fill"));
    }
    let user = wine_user(vars.prefix);
    let home = PathBuf::from(format!("drive_c/users/{user}"));
    let mut text = t.replace('\\', "/");
    // `{Var}` → a path relative to the prefix (or absolute, for the install folder).
    let mut base: Option<PathBuf> = None;
    if text.find('{').is_some_and(|open| open != 0) {
        return Err(format!("the catalog's save folder ({t}) doesn't start with a known place"));
    }
    while let Some(open) = text.find('{') {
        let close = text[open..].find('}').map(|c| open + c).ok_or_else(|| format!("the catalog's save folder ({t}) has an unclosed {{"))?;
        let name = text[open + 1..close].to_ascii_lowercase();
        let value: String = match name.as_str() {
            "epicid" => vars.epic_id.to_owned(),
            _ if open != 0 => return Err(format!("the catalog's save folder ({t}) uses {{{}}} in the middle of the path", &text[open + 1..close])),
            "appdata" | "localappdata" => {
                base = Some(home.join("AppData/Local"));
                String::new()
            }
            "roamingappdata" => {
                base = Some(home.join("AppData/Roaming"));
                String::new()
            }
            "userdir" | "documents" => {
                base = Some(home.join("Documents"));
                String::new()
            }
            "userprofile" => {
                base = Some(home.clone());
                String::new()
            }
            "usersavedgames" => {
                base = Some(home.join("Saved Games"));
                String::new()
            }
            "installdir" => match vars.install_dir {
                Some(d) => {
                    base = Some(d.to_path_buf());
                    String::new()
                }
                None => return Err("the save folder is in the install folder, and the game isn't installed".into()),
            },
            "userlibrary" | "home" => return Err(format!("the catalog's save folder ({t}) is a macOS one")),
            other => return Err(format!("the catalog's save folder ({t}) uses {{{other}}}, which Quiver doesn't know")),
        };
        text.replace_range(open..=close, &value);
    }
    let rel = match base {
        Some(b) => b.join(text.trim_start_matches('/')),
        None => {
            // A Windows path written out: `C:/Users/...`.
            let lower = text.to_ascii_lowercase();
            match lower.strip_prefix("c:") {
                Some(_) => PathBuf::from("drive_c").join(text[2..].trim_start_matches('/')),
                None => return Err(format!("the catalog's save folder ({t}) isn't a path Quiver can place in a prefix")),
            }
        }
    };
    let rel = normalize(&rel).ok_or_else(|| format!("the catalog's save folder ({t}) climbs out of the prefix"))?;
    let abs = if rel.is_absolute() { rel } else { vars.prefix.join(rel) };
    Ok(match_case(&abs, vars.prefix))
}

/// `a/b/../c` → `a/c`; `None` if `..` climbs above the start.
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    Some(out.iter().collect())
}

/// Each component under `root` swapped for an existing entry that differs only in case.
fn match_case(p: &Path, root: &Path) -> PathBuf {
    let (mut cur, rest) = match p.strip_prefix(root) {
        Ok(r) => (root.to_path_buf(), r.to_path_buf()),
        Err(_) => (PathBuf::from("/"), p.strip_prefix("/").unwrap_or(p).to_path_buf()),
    };
    for c in rest.components() {
        let name = c.as_os_str();
        let next = cur.join(name);
        if next.symlink_metadata().is_ok() {
            cur = next;
            continue;
        }
        let want = name.to_string_lossy().to_lowercase();
        let found = std::fs::read_dir(&cur).ok().and_then(|rd| rd.flatten().map(|e| e.file_name()).find(|n| n.to_string_lossy().to_lowercase() == want));
        cur = cur.join(found.as_deref().unwrap_or(name));
    }
    cur
}

/// Which files of a save folder sync: the catalog's include list (empty: everything) less
/// its exclude list. Patterns are comma-separated; `*` and `?` match within a name; a
/// pattern ending in `/` names a folder and everything in it; a pattern without `/`
/// matches a file's name in any folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Filter {
    pub fn new(include: Option<&str>, exclude: Option<&str>) -> Self {
        let split = |s: Option<&str>| s.map(|s| s.split(',').map(|p| p.trim().replace('\\', "/")).filter(|p| !p.is_empty()).collect()).unwrap_or_default();
        Filter { include: split(include), exclude: split(exclude) }
    }

    /// Whether `rel` (relative, `/`-separated) syncs.
    pub fn wants(&self, rel: &str) -> bool {
        let hit = |p: &String| pattern_matches(p, rel);
        (self.include.is_empty() || self.include.iter().any(hit)) && !self.exclude.iter().any(hit)
    }
}

fn pattern_matches(pattern: &str, rel: &str) -> bool {
    let p = pattern.trim_start_matches("./").to_lowercase();
    let rel = rel.to_lowercase();
    if let Some(dir) = p.strip_suffix('/') {
        return rel.starts_with(&format!("{dir}/")) || rel.split('/').rev().skip(1).any(|d| glob(dir, d));
    }
    if p.contains('/') {
        return glob(&p, &rel);
    }
    glob(&p, rel.rsplit('/').next().unwrap_or(&rel))
}

/// `*` and `?` glob over one string (`*` doesn't cross `/` only because callers pass names).
fn glob(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(prefix: &'a Path, install: Option<&'a Path>) -> Vars<'a> {
        Vars { prefix, install_dir: install, epic_id: "8645b4947bbc4c0092a8b7236df169d1" }
    }

    #[test]
    fn catalog_folders_resolve_in_a_prefix() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        let r = |s: &str| resolve(s, vars(p, Some(Path::new("/games/Fallout")))).map(|x| x.strip_prefix(p).map(Path::to_path_buf).unwrap_or(x));
        // Real values from this machine's Epic catalog.
        assert_eq!(r("{AppData}/../LocalLow/Alt Shift/Crying Suns/saves/save0/").unwrap(), Path::new("drive_c/users/steamuser/AppData/LocalLow/Alt Shift/Crying Suns/saves/save0"));
        assert_eq!(r("{appdata}/../roaming/MudRunner/").unwrap(), Path::new("drive_c/users/steamuser/AppData/roaming/MudRunner"));
        assert_eq!(r("{UserSavedGames}/id Software/Rage 2/Saves/").unwrap(), Path::new("drive_c/users/steamuser/Saved Games/id Software/Rage 2/Saves"));
        assert_eq!(
            r("{UserDir}/My Games/Borderlands The Pre-Sequel/WillowGame/SaveData/{EpicId}/").unwrap(),
            Path::new("drive_c/users/steamuser/Documents/My Games/Borderlands The Pre-Sequel/WillowGame/SaveData/8645b4947bbc4c0092a8b7236df169d1")
        );
        assert_eq!(r("{AppData}/Remedy/AlanWake2/{EpicID}/").unwrap(), Path::new("drive_c/users/steamuser/AppData/Local/Remedy/AlanWake2/8645b4947bbc4c0092a8b7236df169d1"));
        assert_eq!(r("{InstallDir}/").unwrap(), Path::new("/games/Fallout"));
        assert_eq!(r(r"C:\Users\steamuser\Saved Games\X").unwrap(), Path::new("drive_c/Users/steamuser/Saved Games/X"));
        // What can't be placed says why.
        assert!(r(r"C:\Users[username]\AppData\Local\Textorcist").unwrap_err().contains("placeholder"));
        assert!(r("{UserLibrary}/Application Support/x/").unwrap_err().contains("macOS"));
        assert!(r("{Weird}/x").unwrap_err().contains("{weird}"));
        assert!(r("{AppData}/../../../../../../etc").unwrap_err().contains("climbs"));
        assert!(resolve("{InstallDir}/", vars(p, None)).unwrap_err().contains("isn't installed"));
    }

    #[test]
    fn components_match_existing_folders_case_insensitively() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        std::fs::create_dir_all(p.join("drive_c/users/steamuser/AppData/LocalLow/Dodge Roll/Enter the Gungeon")).unwrap();
        let got = resolve("{appdata}/../locallow/Dodge Roll/Enter the Gungeon/", vars(p, None)).unwrap();
        assert_eq!(got, p.join("drive_c/users/steamuser/AppData/LocalLow/Dodge Roll/Enter the Gungeon"));
        // A non-Proton prefix with one user.
        let t2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t2.path().join("drive_c/users/ajg")).unwrap();
        std::fs::create_dir_all(t2.path().join("drive_c/users/Public")).unwrap();
        assert_eq!(wine_user(t2.path()), "ajg");
    }

    #[test]
    fn include_and_exclude_lists() {
        let f = Filter::new(Some("savegames/,config-cloud.cfg,keybindings-*.txt"), None);
        assert!(f.wants("savegames/slot1.sav") && f.wants("config-cloud.cfg") && f.wants("keybindings-1.txt"));
        assert!(!f.wants("log.txt") && !f.wants("other/config.cfg"));
        let f = Filter::new(Some("*.sav, *.dat"), Some("critter.dat, master.dat"));
        assert!(f.wants("SAVEGAME/SLOT01/SAVE.DAT") && f.wants("a.sav"));
        assert!(!f.wants("master.dat") && !f.wants("data/critter.dat") && !f.wants("fallout.cfg"));
        let f = Filter::new(None, Some("backup_*"));
        assert!(f.wants("save.json") && !f.wants("backup_1.json"));
        assert!(Filter::default().wants("anything/at/all"));
    }
}
