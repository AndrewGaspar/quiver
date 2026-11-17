//! The HUD's keys as Hyprland binds (SIGHT-HUD §1.3, decision 4): Shift+F3 toggles,
//! Ctrl+Shift+F3 cycles presets, bound only while a Quiver game is focused (SIGHT §6:
//! `hyprctl keyword bind` / `unbind` on focus changes).
//!
//! This version generates the binds and tracks when they would apply; it never runs
//! `hyprctl keyword` (the binder's applier only records what it would do).

use quiver_api::perf::{HudKeybinds, Keybind};

pub fn binds(quiver: &str) -> Vec<Keybind> {
    vec![
        Keybind { mods: "SHIFT".into(), key: "F3".into(), command: format!("{quiver} hud toggle"), description: "Show or hide the HUD".into() },
        Keybind { mods: "CTRL SHIFT".into(), key: "F3".into(), command: format!("{quiver} hud next"), description: "Next HUD preset".into() },
    ]
}

/// `hyprctl` arguments that add the binds (on focus) and remove them (on blur).
pub fn hyprctl_commands(binds: &[Keybind]) -> (Vec<String>, Vec<String>) {
    let on = binds.iter().map(|b| format!("keyword bind {},{},exec,{}", b.mods, b.key, b.command)).collect();
    let off = binds.iter().map(|b| format!("keyword unbind {},{}", b.mods, b.key)).collect();
    (on, off)
}

/// Hyprland config for binding them statically instead (they then fire everywhere).
pub fn conf(binds: &[Keybind]) -> String {
    let mut s = String::from("# Quiver HUD (static: these fire over every window; quiverd binds them only over Quiver games)\n");
    for b in binds {
        s.push_str(&format!("bindd = {}, {}, {}, exec, {}\n", b.mods, b.key, b.description, b.command));
    }
    s
}

/// The `quiver` executable to put in binds: the CLI next to quiverd, else from PATH.
pub fn quiver_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("quiver")))
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "quiver".into())
}

/// Where binds would be applied. The only applier in this version records.
pub trait Applier: Send {
    fn run(&mut self, hyprctl_args: &[String]);
}

/// Records what it would run (and logs it at debug).
#[derive(Default)]
pub struct DryRun {
    pub ran: Vec<String>,
}

impl Applier for DryRun {
    fn run(&mut self, args: &[String]) {
        for a in args {
            tracing::debug!("hud keybinds (not applied): hyprctl {a}");
            self.ran.push(a.clone());
        }
        if self.ran.len() > 64 {
            self.ran.drain(..self.ran.len() - 64);
        }
    }
}

/// Binds while a game is focused, unbinds when focus leaves.
pub struct Binder {
    binds: Vec<Keybind>,
    bound: bool,
    applier: Box<dyn Applier>,
}

impl Binder {
    pub fn new(binds: Vec<Keybind>, applier: Box<dyn Applier>) -> Binder {
        Binder { binds, bound: false, applier }
    }

    pub fn dry_run() -> Binder {
        Binder::new(binds(&quiver_exe()), Box::new(DryRun::default()))
    }

    /// Focus changed (or was re-checked). Runs the transition, if any.
    pub fn focus(&mut self, game_focused: bool) {
        if game_focused == self.bound {
            return;
        }
        let (on, off) = hyprctl_commands(&self.binds);
        self.applier.run(if game_focused { &on } else { &off });
        self.bound = game_focused;
    }

    pub fn describe(&self) -> HudKeybinds {
        let (on_focus, on_blur) = hyprctl_commands(&self.binds);
        HudKeybinds { binds: self.binds.clone(), on_focus, on_blur, conf: conf(&self.binds), game_focused: self.bound, applied: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Rec(Arc<Mutex<Vec<String>>>);
    impl Applier for Rec {
        fn run(&mut self, a: &[String]) {
            self.0.lock().unwrap().extend(a.iter().cloned());
        }
    }

    #[test]
    fn binds_only_while_focused() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut b = Binder::new(binds("quiver"), Box::new(Rec(log.clone())));
        b.focus(false);
        assert!(log.lock().unwrap().is_empty());
        b.focus(true);
        b.focus(true);
        assert_eq!(*log.lock().unwrap(), ["keyword bind SHIFT,F3,exec,quiver hud toggle", "keyword bind CTRL SHIFT,F3,exec,quiver hud next"]);
        b.focus(false);
        assert_eq!(log.lock().unwrap()[2..], ["keyword unbind SHIFT,F3", "keyword unbind CTRL SHIFT,F3"]);
        let d = b.describe();
        assert!(!d.applied && !d.game_focused);
        assert!(d.conf.contains("bindd = SHIFT, F3, Show or hide the HUD, exec, quiver hud toggle"));
    }
}
