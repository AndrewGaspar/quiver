//! The panel's own settings, changed from the Session and Achievements sections. They are
//! Sight's settings (`quiver_api::sight::SightSettings`, `$XDG_DATA_HOME/quiver/sight.toml`),
//! which quiverd owns: the panel reads them and asks quiverd to change one
//! (`SightRequest::SettingsSet`, design/SIGHT.md §16); the change comes back to every
//! quiver-sight part as `Event::SightSettingsChanged`.
//!
//! ```toml
//! [toasts]
//! dnd = true                    # Do not disturb: the toast lane holds its toasts (errors show)
//!
//! [achievements]
//! on_unlock = "toast"           # "toast" (default), "toast_screenshot", "off"
//! hidden = "masked"             # "masked" (default) or "shown": hidden achievements' names
//! ```

use quiver_api::sight::{HiddenAchievements, SightSettings};
use serde_json::Value;

/// What happens when the game unlocks an achievement.
pub use quiver_api::sight::OnUnlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prefs {
    /// Do not disturb (`toasts.dnd`).
    pub dnd: bool,
    pub on_unlock: OnUnlock,
    /// Hidden achievements' names and descriptions are shown (else masked until revealed).
    pub show_hidden: bool,
}

impl Prefs {
    pub fn of(s: &SightSettings) -> Prefs {
        Prefs { dnd: s.toasts.dnd, on_unlock: s.achievements.on_unlock, show_hidden: s.achievements.hidden == HiddenAchievements::Shown }
    }
}

/// One of the settings, as a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pref {
    Dnd,
    OnUnlock,
    Hidden,
}

/// The settings as the file says now (read only: quiverd writes it).
pub fn load() -> Prefs {
    Prefs::of(&quiver_api::sight::load_settings_at(&quiver_api::sight::settings_path()))
}

/// The `SettingsSet` that saves `which` as `prefs` has it: (dotted key, value).
pub fn setting(prefs: &Prefs, which: Pref) -> (String, Value) {
    match which {
        Pref::Dnd => ("toasts.dnd".into(), Value::Bool(prefs.dnd)),
        Pref::OnUnlock => ("achievements.on_unlock".into(), Value::String(prefs.on_unlock.key().into())),
        Pref::Hidden => ("achievements.hidden".into(), Value::String(if prefs.show_hidden { "shown" } else { "masked" }.into())),
    }
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
    fn steps_become_settings() {
        let mut prefs = Prefs::default();
        let mut s = SightSettings::default();
        assert_eq!(Prefs::of(&s), prefs);
        for (which, dir) in [(Pref::Dnd, 0), (Pref::OnUnlock, 1), (Pref::Hidden, -1)] {
            step(&mut prefs, which, dir);
            let (k, v) = setting(&prefs, which);
            s.set(&k, &v).unwrap();
        }
        assert_eq!(prefs, Prefs { dnd: true, on_unlock: OnUnlock::ToastScreenshot, show_hidden: true });
        assert_eq!(Prefs::of(&s), prefs, "what quiverd stores reads back the same");
        // The file C's rows wrote reads the same through SightSettings.
        let file = "pad_chord = true\n[toasts]\ndnd = true\n[achievements]\non_unlock = \"toast_screenshot\"\nhidden = \"shown\"\n";
        assert_eq!(Prefs::of(&SightSettings::from_toml(file)), prefs);
        let panel: crate::panel::Settings = toml::from_str(file).unwrap();
        assert!(panel.pad_chord, "the pad chord setting still reads");
        // A, Left and Right cycle the choice.
        step(&mut prefs, Pref::OnUnlock, 0);
        assert_eq!(prefs.on_unlock, OnUnlock::Off);
        step(&mut prefs, Pref::OnUnlock, 1);
        assert_eq!(prefs.on_unlock, OnUnlock::Toast);
        step(&mut prefs, Pref::OnUnlock, -1);
        assert_eq!(prefs.on_unlock, OnUnlock::Off);
        // A broken file reads as the defaults.
        assert_eq!(Prefs::of(&SightSettings::from_toml("[[[ nope")), Prefs::default());
    }
}
