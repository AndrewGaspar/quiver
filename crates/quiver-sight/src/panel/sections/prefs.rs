//! The panel's own settings in Sight's settings file (`$XDG_DATA_HOME/quiver/sight.toml`, the
//! file `pad_chord` is in), changed from the Session and Achievements sections:
//!
//! ```toml
//! [toasts]
//! dnd = true                    # Do not disturb: the toast lane holds its toasts
//!
//! [achievements]
//! on_unlock = "toast"           # "toast" (default), "toast_screenshot", "off"
//! hidden = "masked"             # "masked" (default) or "shown": hidden achievements' names
//! ```
//!
//! The toast lane reads `toasts.dnd` and `achievements.on_unlock` from the same file. A write
//! changes only its own key: the rest of the file stays as it was.

use std::path::{Path, PathBuf};

/// What happens when the game unlocks an achievement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnUnlock {
    #[default]
    Toast,
    ToastScreenshot,
    Off,
}

impl OnUnlock {
    pub const ALL: [OnUnlock; 3] = [OnUnlock::Toast, OnUnlock::ToastScreenshot, OnUnlock::Off];

    pub fn key(self) -> &'static str {
        match self {
            OnUnlock::Toast => "toast",
            OnUnlock::ToastScreenshot => "toast_screenshot",
            OnUnlock::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OnUnlock::Toast => "Toast",
            OnUnlock::ToastScreenshot => "Toast and screenshot",
            OnUnlock::Off => "Nothing",
        }
    }

    pub fn parse(s: &str) -> Option<OnUnlock> {
        OnUnlock::ALL.into_iter().find(|o| o.key() == s.trim())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prefs {
    /// Do not disturb (`toasts.dnd`).
    pub dnd: bool,
    pub on_unlock: OnUnlock,
    /// Hidden achievements' names and descriptions are shown (else masked until revealed).
    pub show_hidden: bool,
}

/// One of the settings, as a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pref {
    Dnd,
    OnUnlock,
    Hidden,
}

pub fn path() -> PathBuf {
    super::super::settings_path()
}

pub fn parse(text: &str) -> Prefs {
    let v: toml::Table = toml::from_str(text).unwrap_or_default();
    let get = |t: &str, k: &str| v.get(t).and_then(|x| x.get(k)).cloned();
    Prefs {
        dnd: get("toasts", "dnd").and_then(|x| x.as_bool()).unwrap_or(false),
        on_unlock: get("achievements", "on_unlock").and_then(|x| x.as_str().and_then(OnUnlock::parse)).unwrap_or_default(),
        show_hidden: get("achievements", "hidden").and_then(|x| x.as_str().map(|s| s == "shown")).unwrap_or(false),
    }
}

pub fn load_from(p: &Path) -> Prefs {
    parse(&std::fs::read_to_string(p).unwrap_or_default())
}

pub fn load() -> Prefs {
    load_from(&path())
}

/// `text` with `[table] key = value` set, everything else kept.
pub fn set_in(text: &str, table: &str, key: &str, value: toml::Value) -> String {
    let mut v: toml::Table = toml::from_str(text).unwrap_or_default();
    let t = v.entry(table.to_owned()).or_insert_with(|| toml::Value::Table(Default::default()));
    if !t.is_table() {
        *t = toml::Value::Table(Default::default());
    }
    if let Some(t) = t.as_table_mut() {
        t.insert(key.to_owned(), value);
    }
    toml::to_string(&v).unwrap_or_default()
}

/// Writes one setting to `p` (atomically).
pub fn save_to(p: &Path, prefs: &Prefs, which: Pref) -> std::io::Result<()> {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let (table, key, value) = match which {
        Pref::Dnd => ("toasts", "dnd", toml::Value::Boolean(prefs.dnd)),
        Pref::OnUnlock => ("achievements", "on_unlock", toml::Value::String(prefs.on_unlock.key().into())),
        Pref::Hidden => ("achievements", "hidden", toml::Value::String(if prefs.show_hidden { "shown" } else { "masked" }.into())),
    };
    let out = set_in(&text, table, key, value);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, p)
}

/// Left (`dir` < 0), Right (> 0) or A (0) on a setting's row.
pub fn step(prefs: &mut Prefs, which: Pref, dir: i32) {
    match which {
        Pref::Dnd => prefs.dnd = !prefs.dnd,
        Pref::Hidden => prefs.show_hidden = !prefs.show_hidden,
        Pref::OnUnlock => prefs.on_unlock = crate::panel::state::cycle(&OnUnlock::ALL, &prefs.on_unlock, dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_keep_the_rest() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sight.toml");
        std::fs::write(&p, "pad_chord = true\n").unwrap();
        assert_eq!(load_from(&p), Prefs::default());
        let mut prefs = Prefs::default();
        step(&mut prefs, Pref::Dnd, 0);
        save_to(&p, &prefs, Pref::Dnd).unwrap();
        step(&mut prefs, Pref::OnUnlock, 1);
        save_to(&p, &prefs, Pref::OnUnlock).unwrap();
        step(&mut prefs, Pref::Hidden, -1);
        save_to(&p, &prefs, Pref::Hidden).unwrap();
        let back = load_from(&p);
        assert_eq!(back, Prefs { dnd: true, on_unlock: OnUnlock::ToastScreenshot, show_hidden: true });
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("pad_chord = true"), "{text}");
        assert!(text.contains("[toasts]") && text.contains("dnd = true"), "{text}");
        assert!(text.contains("on_unlock = \"toast_screenshot\"") && text.contains("hidden = \"shown\""), "{text}");
        // The pad chord setting still reads.
        let s: crate::panel::Settings = toml::from_str(&text).unwrap();
        assert!(s.pad_chord);
        // A, Left and Right cycle the choice.
        step(&mut prefs, Pref::OnUnlock, 0);
        assert_eq!(prefs.on_unlock, OnUnlock::Off);
        step(&mut prefs, Pref::OnUnlock, 1);
        assert_eq!(prefs.on_unlock, OnUnlock::Toast);
        step(&mut prefs, Pref::OnUnlock, -1);
        assert_eq!(prefs.on_unlock, OnUnlock::Off);
        // A broken file reads as the defaults.
        assert_eq!(parse("[[[ nope"), Prefs::default());
    }
}
