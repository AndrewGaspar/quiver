//! The app theme: design tokens as GPUI colors, loaded from the current Omarchy theme and
//! re-loaded live when `theme.generation` changes. Only reads Omarchy's files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, Global, Hsla, Rgba, SharedString};

use crate::palette::{Color, Palette, Tokens};

/// Every token the components read, as GPUI colors. Built once per theme load; components
/// never mix colors themselves.
#[derive(Clone, Debug)]
pub struct Theme {
    pub name: SharedString,
    /// Where the palette came from: `None` = the built-in "Quiver dark".
    pub omarchy: Option<SharedString>,
    pub dark: bool,
    pub ink0: Hsla,
    pub ink1: Hsla,
    pub ink2: Hsla,
    pub ink3: Hsla,
    pub ink4: Hsla,
    pub ink5: Hsla,
    pub line1: Hsla,
    pub line2: Hsla,
    pub tx1: Hsla,
    pub tx2: Hsla,
    pub tx3: Hsla,
    pub tx4: Hsla,
    pub acc: Hsla,
    pub acc_hi: Hsla,
    pub acc_ink: Hsla,
    pub acc_soft: Hsla,
    pub acc_line: Hsla,
    /// Play button gradient: accent mixed 18% toward white, and 12% toward black.
    pub acc_top: Hsla,
    pub acc_deep: Hsla,
    /// Brand mark squares (accent over ink-4).
    pub mark1: Hsla,
    pub mark2: Hsla,
    pub sel: Hsla,
    pub ok: Hsla,
    pub ok_soft: Hsla,
    pub warn: Hsla,
    pub warn_soft: Hsla,
    pub bad: Hsla,
    pub bad_soft: Hsla,
    pub who_gui: Hsla,
    pub who_cli: Hsla,
    pub who_agent: Hsla,
    pub who_sys: Hsla,
    pub focus: Hsla,
    pub focus_halo: Hsla,
    pub code_s: Hsla,
    pub scrim: Hsla,
    /// Track behind progress bars: `tx-1` at 12%.
    pub track: Hsla,
}

impl Global for Theme {}

pub fn hsla(c: Color) -> Hsla {
    Rgba { r: c.r as f32 / 255.0, g: c.g as f32 / 255.0, b: c.b as f32 / 255.0, a: c.a }.into()
}

/// A fixed color as `0xrrggbb` with alpha.
pub fn rgba(hex: u32, a: f32) -> Hsla {
    hsla(Color::hex_u32(hex).with_alpha(a))
}

impl Theme {
    pub fn from_tokens(t: &Tokens, name: impl Into<SharedString>, omarchy: Option<SharedString>) -> Self {
        let h = hsla;
        Self {
            name: name.into(),
            omarchy,
            dark: t.dark,
            ink0: h(t.ink0),
            ink1: h(t.ink1),
            ink2: h(t.ink2),
            ink3: h(t.ink3),
            ink4: h(t.ink4),
            ink5: h(t.ink5),
            line1: h(t.line1),
            line2: h(t.line2),
            tx1: h(t.tx1),
            tx2: h(t.tx2),
            tx3: h(t.tx3),
            tx4: h(t.tx4),
            acc: h(t.acc),
            acc_hi: h(t.acc_hi),
            acc_ink: h(t.acc_ink),
            acc_soft: h(t.acc_soft),
            acc_line: h(t.acc_line),
            acc_top: h(t.acc.mix(Color::WHITE, 0.18)),
            acc_deep: h(t.acc.mix(Color::BLACK, 0.12)),
            mark1: h(t.ink4.mix(t.acc, 0.3)),
            mark2: h(t.ink4.mix(t.acc, 0.65)),
            sel: h(t.sel),
            ok: h(t.ok),
            ok_soft: h(t.ok_soft),
            warn: h(t.warn),
            warn_soft: h(t.warn_soft),
            bad: h(t.bad),
            bad_soft: h(t.bad_soft),
            who_gui: h(t.who_gui),
            who_cli: h(t.who_cli),
            who_agent: h(t.who_agent),
            who_sys: h(t.who_sys),
            focus: h(t.focus),
            focus_halo: h(t.focus_halo),
            code_s: h(t.code_s),
            scrim: h(t.scrim),
            track: h(t.tx1.with_alpha(0.12)),
        }
    }

    pub fn builtin() -> Self {
        Self::from_tokens(&Tokens::builtin(), "Quiver dark", None)
    }
}

/// Text and controls over game art: light-on-dark in every theme (the mock-up's `.onart`).
pub mod on_art {
    use super::rgba;
    use gpui::Hsla;

    pub fn tx1() -> Hsla {
        rgba(0xf3f5f9, 1.0)
    }
    pub fn tx2() -> Hsla {
        rgba(0xf3f5f9, 0.8)
    }
    pub fn tx3() -> Hsla {
        rgba(0xf3f5f9, 0.62)
    }
    /// Translucent control fill / border over art.
    pub fn fill() -> Hsla {
        rgba(0xffffff, 0.08)
    }
    pub fn line() -> Hsla {
        rgba(0xffffff, 0.14)
    }
    /// "Glass" panel over art (no backdrop blur in GPUI: a darker translucent fill).
    pub fn glass() -> Hsla {
        rgba(0x0c0e14, 0.72)
    }
    pub fn tag() -> Hsla {
        rgba(0x080a10, 0.78)
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

// ---------------------------------------------------------------------------------------------
// Loading

/// Omarchy's current-theme state directory (holds `theme/colors.toml` and
/// `theme.generation`). `QUIVER_OMARCHY_STATE` overrides it, for tests and screenshots.
pub fn omarchy_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("QUIVER_OMARCHY_STATE") {
        return PathBuf::from(d);
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("omarchy/current")
}

/// `QUIVER_THEME=builtin` ignores Omarchy and uses "Quiver dark".
fn forced_builtin() -> bool {
    std::env::var("QUIVER_THEME").is_ok_and(|v| v == "builtin")
}

pub fn read_generation(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("theme.generation")).ok().map(|s| s.trim().to_owned())
}

/// Load the theme for `dir`, falling back to the built-in palette (with a log line) when
/// there is no Omarchy theme or it can't be used.
pub fn load(dir: &Path) -> Theme {
    if forced_builtin() {
        return Theme::builtin();
    }
    let colors = dir.join("theme/colors.toml");
    let text = match std::fs::read_to_string(&colors) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("no Omarchy theme at {}: {e}", colors.display());
            return Theme::builtin();
        }
    };
    match Palette::parse(&text) {
        Ok(p) => {
            let name = std::fs::read_to_string(dir.join("theme.name"))
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Omarchy".into());
            Theme::from_tokens(&Tokens::derive(&p), name.clone(), Some(name.into()))
        }
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", colors.display());
            Theme::builtin()
        }
    }
}

/// Watch `dir` for theme switches. Calls `changed` (on a background thread) whenever
/// `theme.generation` changes. Uses inotify, with a slow poll as a safety net for
/// directories that are replaced wholesale or appear later.
pub fn watch(dir: PathBuf, changed: impl Fn() + Send + 'static) {
    use notify::{RecursiveMode, Watcher};
    std::thread::Builder::new()
        .name("theme-watch".into())
        .spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let mut watcher = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
                let _ = tx.send(());
            })
            .ok();
            let mut watching = false;
            let mut last = read_generation(&dir);
            loop {
                if !watching {
                    if let Some(w) = watcher.as_mut() {
                        watching = w.watch(&dir, RecursiveMode::NonRecursive).is_ok();
                    }
                }
                // Wake on an inotify event, or every few seconds regardless.
                let woke = rx.recv_timeout(Duration::from_secs(3));
                if woke.is_ok() {
                    // Omarchy writes several files per switch; let it finish.
                    std::thread::sleep(Duration::from_millis(120));
                    while rx.try_recv().is_ok() {}
                }
                let now = read_generation(&dir);
                if now != last {
                    last = now;
                    // The directory may have been replaced: re-arm the watch.
                    watching = false;
                    changed();
                }
            }
        })
        .expect("spawn theme watcher");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_omarchy_dir_and_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(load(dir).name.as_ref(), "Quiver dark");
        std::fs::create_dir_all(dir.join("theme")).unwrap();
        std::fs::write(dir.join("theme/colors.toml"), include_str!("../tests/fixtures/themes/jade-horse.toml")).unwrap();
        std::fs::write(dir.join("theme.name"), "jade-horse\n").unwrap();
        let t = load(dir);
        assert_eq!(t.name.as_ref(), "jade-horse");
        assert!(t.dark);
        std::fs::write(dir.join("theme/colors.toml"), "nonsense").unwrap();
        assert_eq!(load(dir).name.as_ref(), "Quiver dark");
    }

    #[test]
    fn watcher_fires_on_generation_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join("theme.generation"), "1").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        watch(dir.clone(), move || {
            let _ = tx.send(());
        });
        std::thread::sleep(Duration::from_millis(300));
        // Unrelated files don't count.
        std::fs::write(dir.join("other"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());
        std::fs::write(dir.join("theme.generation"), "2").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("watcher saw the new generation");
    }
}
