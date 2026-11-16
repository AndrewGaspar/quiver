//! Steam's own art for an app, from its local library cache (read-only).
//!
//! Layouts seen in `<Steam>/appcache/librarycache` (checked against a real cache of 670
//! apps, 2026-11):
//!
//! - current: `<appid>/<sha1>/<file>` (each image in its own hashed folder: e.g.
//!   `library_capsule.jpg` 300×450, `library_hero.jpg` 1920×620, `library_hero_blur.jpg`,
//!   `logo.png`, `library_header.jpg` 460×215), plus `<appid>/<sha1>.jpg`, the 32×32 icon;
//! - flat per app: `<appid>/library_600x900.jpg`, `library_hero.jpg`, `logo.png`,
//!   `header.jpg`, `<sha1>.jpg`;
//! - old: `librarycache/<appid>_library_600x900.jpg`, `<appid>_library_hero.jpg`,
//!   `<appid>_logo.png`, `<appid>_header.jpg`, `<appid>_icon.jpg`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use ubi_api::art::SteamArt;

/// File names per slot, best first.
const CAPSULE: &[&str] = &["library_600x900_2x.jpg", "library_600x900.jpg", "library_capsule.jpg"];
const HERO: &[&str] = &["library_hero.jpg"];
const LOGO: &[&str] = &["logo.png", "logo_2x.png"];
const HEADER: &[&str] = &["header.jpg", "library_header.jpg"];

fn is_sha1(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every image file of `app_id` in the cache: (file name, path, modified).
fn files(cache: &Path, app_id: u32) -> Vec<(String, PathBuf, SystemTime)> {
    let mut out = Vec::new();
    let mut add = |name: String, path: PathBuf| {
        if let Ok(m) = std::fs::metadata(&path)
            && m.is_file()
            && m.len() > 0
        {
            out.push((name, path, m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        }
    };
    let dir = cache.join(app_id.to_string());
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if path.is_dir() {
            if is_sha1(&name) {
                for f in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                    add(f.file_name().to_string_lossy().into_owned(), f.path());
                }
            }
        } else {
            add(name, path);
        }
    }
    // Old flat layout: `<appid>_<file>`.
    let prefix = format!("{app_id}_");
    for name in CAPSULE.iter().chain(HERO).chain(LOGO).chain(HEADER).chain(&["icon.jpg"]) {
        add(name.to_string(), cache.join(format!("{prefix}{name}")));
    }
    out
}

/// The newest file with the first name of `names` that exists.
fn pick(files: &[(String, PathBuf, SystemTime)], names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|n| files.iter().filter(|f| f.0 == *n).max_by_key(|f| f.2).map(|f| f.1.clone()))
}

/// Steam's art for `app_id` under `steam_root` (the folder holding `appcache/`). Empty when
/// Steam never cached any.
pub fn library_art(steam_root: &Path, app_id: u32) -> SteamArt {
    let cache = steam_root.join("appcache/librarycache");
    let files = files(&cache, app_id);
    if files.is_empty() {
        return SteamArt::default();
    }
    let icon = files
        .iter()
        .filter(|f| f.0 == "icon.jpg" || f.0.strip_suffix(".jpg").is_some_and(is_sha1))
        .max_by_key(|f| f.2)
        .map(|f| f.1.clone());
    SteamArt { capsule: pick(&files, CAPSULE), hero: pick(&files, HERO), logo: pick(&files, LOGO), header: pick(&files, HEADER), icon }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn hashed_folders() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("appcache/librarycache/3751950");
        let h = |n: u8| format!("{:040x}", n);
        touch(&app.join(format!("{}.jpg", h(7))));
        touch(&app.join(h(1)).join("library_capsule.jpg"));
        touch(&app.join(h(2)).join("library_hero.jpg"));
        touch(&app.join(h(2)).join("library_hero_blur.jpg"));
        touch(&app.join(h(3)).join("logo.png"));
        touch(&app.join(h(4)).join("library_header.jpg"));
        // Not a hash folder: ignored.
        touch(&app.join("other/logo.png"));
        let a = library_art(root.path(), 3751950);
        assert_eq!(a.capsule, Some(app.join(h(1)).join("library_capsule.jpg")));
        assert_eq!(a.hero, Some(app.join(h(2)).join("library_hero.jpg")));
        assert_eq!(a.logo, Some(app.join(h(3)).join("logo.png")));
        assert_eq!(a.header, Some(app.join(h(4)).join("library_header.jpg")));
        assert_eq!(a.icon, Some(app.join(format!("{}.jpg", h(7)))));
        assert!(library_art(root.path(), 1).is_empty());
    }

    #[test]
    fn flat_and_old_layouts() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("appcache/librarycache");
        touch(&cache.join("10180/library_600x900.jpg"));
        touch(&cache.join("10180/library_hero.jpg"));
        touch(&cache.join("10180/header.jpg"));
        let a = library_art(root.path(), 10180);
        assert_eq!(a.capsule, Some(cache.join("10180/library_600x900.jpg")));
        assert_eq!(a.header, Some(cache.join("10180/header.jpg")));
        assert_eq!((a.logo, a.icon), (None, None));

        touch(&cache.join("582160_library_600x900.jpg"));
        touch(&cache.join("582160_logo.png"));
        touch(&cache.join("582160_icon.jpg"));
        let a = library_art(root.path(), 582160);
        assert_eq!(a.capsule, Some(cache.join("582160_library_600x900.jpg")));
        assert_eq!(a.logo, Some(cache.join("582160_logo.png")));
        assert_eq!(a.icon, Some(cache.join("582160_icon.jpg")));
        assert_eq!(a.hero, None);
    }

    #[test]
    fn empty_files_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("appcache/librarycache/5/logo.png");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"").unwrap();
        assert!(library_art(root.path(), 5).is_empty());
    }

    /// `STEAM_ROOT=~/.local/share/Steam STEAM_APP=3751950 cargo test -p ubi-core real_cache -- --ignored --nocapture`
    /// prints what a real cache maps to (read-only).
    #[test]
    #[ignore]
    fn real_cache() {
        let root = std::env::var("STEAM_ROOT").expect("STEAM_ROOT");
        let app: u32 = std::env::var("STEAM_APP").expect("STEAM_APP").parse().unwrap();
        println!("{:#?}", library_art(Path::new(&root), app));
    }
}
