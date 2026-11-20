//! The fallback screenshot (design/SIGHT.md §16): the quick menu's Screenshot tile and
//! `achievements.on_unlock = toast_screenshot` ask quiverd for the game's own frame first
//! (`SightRequest::Screenshot`: the Sight layer's capture, under Sight's drawing). Where the
//! layer can't (no layer, an older one, an OpenGL game), `grim` of the game's monitor, into the
//! same folder: `<pictures>/Quiver/<game>/`. grim captures what the compositor shows: the game
//! with the HUD and any toast.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Where a game's screenshots go: `<pictures>/Quiver/<game>/` (`$XDG_PICTURES_DIR`, else
/// `~/Pictures`), as quiverd's.
pub fn folder(game: &str) -> PathBuf {
    let pictures = std::env::var_os("XDG_PICTURES_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Pictures"));
    quiver_api::sight::session::captures_dir(&pictures, game)
}

/// A file name for a shot of `game` at `stamp` ("Assassin's Creed Origins 2026-11-19 21-14-03.png"):
/// no path separators or control characters.
pub fn file_name(game: &str, stamp: &str) -> String {
    let clean: String = game.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { ' ' } else { c }).collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = if clean.is_empty() { "Game".to_owned() } else { clean.chars().take(80).collect() };
    format!("{name} {stamp}.png")
}

/// Takes it (blocking: call it off the UI thread).
pub fn take(monitor: Option<&str>, game: &str) -> Result<PathBuf> {
    take_into(monitor, game, &folder(game))
}

pub fn take_into(monitor: Option<&str>, game: &str, dir: &Path) -> Result<PathBuf> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        bail!("no Wayland display to capture");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string();
    let path = dir.join(file_name(game, &stamp));
    let mut cmd = std::process::Command::new("grim");
    if let Some(m) = monitor {
        cmd.arg("-o").arg(m);
    }
    let out = cmd.arg(&path).output().context("running grim (is it installed?)")?;
    if !out.status.success() {
        bail!("grim: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(file_name("Assassin's Creed Origins", "2026-11-19 21-14-03"), "Assassin's Creed Origins 2026-11-19 21-14-03.png");
        assert_eq!(file_name("a/b\n c", "x"), "a b c x.png");
        assert_eq!(file_name("", "x"), "Game x.png");
    }
}
