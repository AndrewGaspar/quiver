//! The HUD's keys (SIGHT-HUD §1.3, decision 4): Shift+F3 toggles, Ctrl+Shift+F3 cycles
//! presets.
//!
//! **In the game** (as built): the Sight layer polls X11's keymap on its own connection
//! (quiver-sight-layer `keys.rs`) and, for Wayland-native windows, reads a `wl_keyboard` of
//! its own on the game's Wayland connection (`wayland.rs`), and counts chord presses in its
//! ring's status block; the
//! ring thread turns new presses into [`super::PerfHub::hud_keys`], which does what
//! `quiver hud toggle` / `quiver hud next` do. [`layer_keys`] says whether a layer reads
//! them, or why not.
//!
//! **Hyprland binds** are the fallback for games without the layer (anti-cheat, OpenGL;
//! the desktop presenter): this module generates them and tracks
//! when they would apply (SIGHT §6: `hyprctl keyword bind` / `unbind` on focus changes),
//! but never runs `hyprctl keyword` (the binder's applier only records what it would do).
//! The chords are fixed here and in the layer: there's no user setting for them yet.

use quiver_api::perf::{HudKeybinds, HudKeysState, Keybind};
use quiver_perf::layout::{
    KEYS_ACTIVE, KEYS_INTERNAL, KEYS_IO_ERROR, KEYS_NO_DISPLAY, KEYS_NO_KEYCODES, KEYS_NO_LIBX11, KEYS_OFF, KEYS_OPEN_FAILED, KEYS_UNKNOWN, KEYS_WL_ACTIVE, KEYS_WL_DISPLAY_ERROR,
    KEYS_WL_INTERNAL, KEYS_WL_NO_KEYBOARD, KEYS_WL_NO_LIBWAYLAND, KEYS_WL_NO_SEAT, KEYS_WL_STARTING, KEYS_WL_STOPPED, LAYER_WAYLAND_SURFACE, LAYER_X11_SURFACE, LayerStatus,
};

/// `HudKeybinds::handler`.
pub const HANDLER_LAYER: &str = "layer";

/// Whether a layer reads the HUD's keys, from its status block: (`HudKeysState::keys`, a
/// sentence). The layer has two sources: X11 keymap polling (`keys_x11`) and, for
/// Wayland-native windows, a `wl_keyboard` on the game's own connection (`keys_wayland`).
pub fn layer_keys(s: &LayerStatus) -> (&'static str, &'static str) {
    let (x11, wl) = (s.keys_x11(), s.keys_wayland());
    if x11 == KEYS_OFF {
        return ("off", "turned off in the game (QUIVER_SIGHT_KEYS=0)");
    }
    if wl == KEYS_WL_ACTIVE {
        return if x11 == KEYS_ACTIVE && s.flags & LAYER_X11_SURFACE != 0 {
            ("active_wayland", "read in the game by the Sight layer, from the game's own Wayland connection and X11 keymap polling")
        } else {
            ("active_wayland", "read in the game by the Sight layer, from a keyboard on the game's own Wayland connection")
        };
    }
    let wayland_only = s.flags & LAYER_WAYLAND_SURFACE != 0 && s.flags & LAYER_X11_SURFACE == 0;
    if wayland_only {
        return match wl {
            KEYS_WL_STARTING => ("starting", "the game's window is Wayland-native; the layer is binding a keyboard on the game's connection"),
            KEYS_WL_NO_LIBWAYLAND => {
                ("no_libwayland", "the game's window is Wayland-native, but libwayland-client.so.0 isn't loaded in the game, so the layer can't share its connection")
            }
            KEYS_WL_NO_SEAT => ("no_seat", "the game's window is Wayland-native, but the compositor offers no wl_seat"),
            KEYS_WL_NO_KEYBOARD => ("no_keyboard", "the game's window is Wayland-native, but the compositor's seat has no keyboard"),
            KEYS_WL_DISPLAY_ERROR => ("wayland_error", "the game's Wayland connection failed; key reading stopped"),
            KEYS_WL_STOPPED => ("stopped", "the game destroyed its Wayland surfaces; key reading stopped"),
            KEYS_WL_INTERNAL => ("internal", "key reading on the game's Wayland connection failed in the layer"),
            // A layer from before Wayland keys.
            _ => ("wayland", "the game's window is Wayland-native and this Sight layer can't read keys there (it predates Wayland keys), and XWayland never sees them"),
        };
    }
    match x11 {
        KEYS_ACTIVE => ("active", "read in the game by the Sight layer (X11 keymap polling)"),
        KEYS_OFF => ("off", "turned off in the game (QUIVER_SIGHT_KEYS=0)"),
        KEYS_NO_DISPLAY => ("no_display", "DISPLAY isn't set in the game, so there's no X11 to read keys from"),
        KEYS_NO_LIBX11 => ("no_libx11", "libX11.so.6 couldn't be loaded in the game"),
        KEYS_OPEN_FAILED => ("open_failed", "the layer couldn't connect to the X server (DISPLAY)"),
        KEYS_NO_KEYCODES => ("no_keycodes", "F3 or Shift isn't in the X keymap"),
        KEYS_IO_ERROR => ("io_error", "the layer's X connection broke; key reading stopped"),
        KEYS_INTERNAL => ("internal", "key reading failed in the layer"),
        KEYS_UNKNOWN => ("unknown", "the layer hasn't reported key reading (starting, or a layer from before in-game keys)"),
        _ => ("unknown", "the layer reported an unknown key state"),
    }
}

/// The per-game line of `HudKeybinds::games`.
pub fn keys_state(session: &str, product_id: u32, game: Option<quiver_api::GameRef>, pid: u32, s: &LayerStatus) -> HudKeysState {
    let (keys, detail) = layer_keys(s);
    HudKeysState { session: session.to_owned(), product_id, game, pid, keys: keys.into(), detail: detail.into() }
}

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

/// Hyprland config for binding them statically, for games the layer can't read keys in
/// (they then fire over every window).
pub fn conf(binds: &[Keybind]) -> String {
    let mut s = String::from("# Quiver HUD fallback for games without the Sight layer (static: these fire over every window)\n");
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

    /// The keys, with each running game's in-game state (`games`).
    pub fn describe(&self, games: Vec<HudKeysState>) -> HudKeybinds {
        let (on_focus, on_blur) = hyprctl_commands(&self.binds);
        HudKeybinds {
            binds: self.binds.clone(),
            handler: HANDLER_LAYER.into(),
            games,
            on_focus,
            on_blur,
            conf: conf(&self.binds),
            game_focused: self.bound,
            applied: false,
        }
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
        let d = b.describe(vec![]);
        assert!(!d.applied && !d.game_focused);
        assert_eq!(d.handler, "layer");
        assert!(d.conf.contains("bindd = SHIFT, F3, Show or hide the HUD, exec, quiver hud toggle"));
    }

    #[test]
    fn layer_key_states() {
        let mut s = LayerStatus::ZERO;
        assert_eq!(layer_keys(&s).0, "unknown");
        s.keys = KEYS_ACTIVE;
        s.flags = LAYER_X11_SURFACE;
        assert_eq!(layer_keys(&s).0, "active");
        // Wayland and X11 surfaces: the X11 one may have focus.
        s.flags |= LAYER_WAYLAND_SURFACE;
        assert_eq!(layer_keys(&s).0, "active");
        s.flags = LAYER_WAYLAND_SURFACE;
        assert_eq!(layer_keys(&s).0, "wayland");
        s.keys = KEYS_NO_DISPLAY;
        assert_eq!(layer_keys(&s).0, "wayland");
        s.flags = 0;
        assert_eq!(layer_keys(&s).0, "no_display");
        s.keys = KEYS_NO_LIBX11;
        assert_eq!(layer_keys(&s).0, "no_libx11");
        s.keys = 99;
        assert_eq!(layer_keys(&s).0, "unknown");
    }

    #[test]
    fn wayland_key_states() {
        use quiver_perf::layout::KEYS_WL_SHIFT;
        let wl = |x11: u32, w: u32, flags: u32| {
            let mut s = LayerStatus::ZERO;
            s.keys = x11 | w << KEYS_WL_SHIFT;
            s.flags = flags;
            layer_keys(&s).0
        };
        let (w, x, both) = (LAYER_WAYLAND_SURFACE, LAYER_X11_SURFACE, LAYER_WAYLAND_SURFACE | LAYER_X11_SURFACE);
        // AC Origins under GE-Proton on winewayland: DISPLAY set (X11 polling runs, useless),
        // only Wayland surfaces; the layer's keyboard reads them.
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_ACTIVE, w), "active_wayland");
        assert_eq!(wl(KEYS_NO_DISPLAY, KEYS_WL_ACTIVE, w), "active_wayland");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_ACTIVE, both), "active_wayland");
        assert!(layer_keys(&{
            let mut s = LayerStatus::ZERO;
            s.keys = KEYS_ACTIVE | KEYS_WL_ACTIVE << KEYS_WL_SHIFT;
            s.flags = both;
            s
        })
        .1
        .contains("X11"));
        // Why not, when the window is Wayland-only.
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_STARTING, w), "starting");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_NO_LIBWAYLAND, w), "no_libwayland");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_NO_SEAT, w), "no_seat");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_NO_KEYBOARD, w), "no_keyboard");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_DISPLAY_ERROR, w), "wayland_error");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_STOPPED, w), "stopped");
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_INTERNAL, w), "internal");
        // An older layer (no Wayland byte): it truly can't.
        assert_eq!(wl(KEYS_ACTIVE, 0, w), "wayland");
        // An X11 surface as well: X11 polling may have the focus, so its state shows.
        assert_eq!(wl(KEYS_ACTIVE, KEYS_WL_NO_SEAT, both), "active");
        assert_eq!(wl(KEYS_ACTIVE, 0, x), "active");
        // Off is off for both.
        assert_eq!(wl(KEYS_OFF, 0, w), "off");
        assert_eq!(wl(KEYS_OFF, KEYS_WL_ACTIVE, w), "off");
    }
}
