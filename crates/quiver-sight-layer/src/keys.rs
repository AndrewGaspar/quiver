//! The HUD's keys, read in the game (SIGHT-HUD §1.3, as built): Shift+F3 shows or hides the
//! HUD, Ctrl+Shift+F3 steps to the next preset. Like MangoHud and vkBasalt, the layer polls
//! the X server's keymap (`XQueryKeymap`) on its own Display connection, about every 50 ms
//! while the game presents. XWayland only reports keys while one of its windows has
//! keyboard focus, so the keys only work while an X11 (Wine/XWayland) window is focused.
//!
//! A press counts on the trigger key's edge (up → down) with exactly the chord's modifiers
//! held: Ctrl+Shift+F3 is "next", never also "toggle", and holding a chord doesn't repeat.
//! Presses go into the status block (`key_toggles`, `key_nexts`); quiverd's ring thread
//! sees the counters change and does what `quiver hud toggle` / `next` do for the session.
//!
//! **Threading.** The poll runs on its own thread, not in `vkQueuePresentKHR`: an X round
//! trip can block (a stalled XWayland, an unreachable `DISPLAY`), and the present path makes
//! no syscalls beyond Vulkan's (lib.rs). The thread polls only when presents have happened
//! since its last poll, so it follows the present cadence, capped at 20 Hz.
//!
//! **Loading.** `libX11.so.6` is `dlopen`ed when polling starts: no link-time dependency, so
//! the layer loads where X11 isn't installed (and in pressure-vessel, whose runtime has its
//! own copy). Any failure turns polling off for the process and says why in the status
//! block's `keys` (`KEYS_*`), which `quiver hud status` and `quiver hud keybinds` show.
//!
//! **Not covered** (follow-ups): Wayland-native windows (winewayland, native Wayland games)
//! need a `wl_keyboard` listener on the game's own connection, as MangoHud does; the layer
//! only notes that such a surface exists (`LAYER_WAYLAND_SURFACE`). The chords are fixed
//! ([`DEFAULT_BINDINGS`]); there's no user setting for the HUD's keys yet.

use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use quiver_perf::layout::{
    KEYS_ACTIVE, KEYS_INTERNAL, KEYS_IO_ERROR, KEYS_NO_DISPLAY, KEYS_NO_KEYCODES, KEYS_NO_LIBX11, KEYS_OFF, KEYS_OPEN_FAILED, LAYER_WAYLAND_SURFACE, LAYER_X11_SURFACE,
};

use crate::log;
use crate::telemetry::Telemetry;

/// Between polls (at most 20 a second).
pub const POLL_EVERY: Duration = Duration::from_millis(50);

/// Surface kinds the game created (`LAYER_WAYLAND_SURFACE`, `LAYER_X11_SURFACE`), for the
/// status block's flags. Set by the instance hooks, before any telemetry exists.
pub static SURFACES: AtomicU32 = AtomicU32::new(0);

pub fn note_surface(flag: u32) {
    if flag & (LAYER_WAYLAND_SURFACE | LAYER_X11_SURFACE) != 0 && SURFACES.fetch_or(flag, Ordering::Relaxed) & flag == 0 {
        log!("{} surface created", if flag == LAYER_WAYLAND_SURFACE { "Wayland" } else { "X11" });
    }
}

// ---- Chords (no X here: unit-tested with fake keymaps) ------------------------------------

/// What a chord does: what `quiver hud toggle` / `quiver hud next` do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Next,
}

pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_SUPER: u8 = 8;

/// X keysyms (X11/keysymdef.h).
pub mod keysym {
    pub const F3: u64 = 0xffc0;
    pub const SHIFT_L: u64 = 0xffe1;
    pub const SHIFT_R: u64 = 0xffe2;
    pub const CONTROL_L: u64 = 0xffe3;
    pub const CONTROL_R: u64 = 0xffe4;
    pub const ALT_L: u64 = 0xffe9;
    pub const ALT_R: u64 = 0xffea;
    pub const SUPER_L: u64 = 0xffeb;
    pub const SUPER_R: u64 = 0xffec;
}

/// The modifier keys and the bit each sets. Alt and Super are tracked so that, say,
/// Alt+Shift+F3 isn't taken for Shift+F3.
pub const MODIFIER_KEYS: [(u64, u8); 8] = [
    (keysym::SHIFT_L, MOD_SHIFT),
    (keysym::SHIFT_R, MOD_SHIFT),
    (keysym::CONTROL_L, MOD_CTRL),
    (keysym::CONTROL_R, MOD_CTRL),
    (keysym::ALT_L, MOD_ALT),
    (keysym::ALT_R, MOD_ALT),
    (keysym::SUPER_L, MOD_SUPER),
    (keysym::SUPER_R, MOD_SUPER),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub keysym: u64,
    /// Exactly these modifiers (`MOD_*`) must be held.
    pub mods: u8,
    pub action: Action,
}

/// SIGHT-HUD decision 4; quiverd's `perf::keybinds::binds` describes the same two.
pub const DEFAULT_BINDINGS: [Binding; 2] = [
    Binding { keysym: keysym::F3, mods: MOD_SHIFT, action: Action::Toggle },
    Binding { keysym: keysym::F3, mods: MOD_CTRL | MOD_SHIFT, action: Action::Next },
];

/// A 256-bit keymap, as `XQueryKeymap` returns it: bit `code % 8` of byte `code / 8` is set
/// while keycode `code` is down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keymap(pub [u8; 32]);

impl Keymap {
    pub fn down(&self, code: u8) -> bool {
        code != 0 && self.0[code as usize / 8] & (1 << (code % 8)) != 0
    }

    /// A keymap with these keycodes down (tests).
    pub fn with(codes: &[u8]) -> Keymap {
        let mut k = Keymap::default();
        for &c in codes {
            k.0[c as usize / 8] |= 1 << (c % 8);
        }
        k
    }
}

/// Bindings and modifiers resolved to keycodes (0: not in the keymap).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keycodes {
    pub bindings: Vec<(u8, u8, Action)>,
    pub modifiers: Vec<(u8, u8)>,
}

impl Keycodes {
    /// With `lookup` mapping a keysym to its keycode (`XKeysymToKeycode`; 0 for none).
    /// `None` when no binding's key or a modifier one needs is in the keymap.
    pub fn resolve(bindings: &[Binding], mut lookup: impl FnMut(u64) -> u8) -> Option<Keycodes> {
        let modifiers: Vec<(u8, u8)> = MODIFIER_KEYS.iter().map(|&(sym, bit)| (lookup(sym), bit)).filter(|(c, _)| *c != 0).collect();
        let have = modifiers.iter().fold(0u8, |a, (_, b)| a | b);
        let bindings: Vec<(u8, u8, Action)> =
            bindings.iter().map(|b| (lookup(b.keysym), b.mods, b.action)).filter(|&(c, mods, _)| c != 0 && mods & have == mods).collect();
        (!bindings.is_empty()).then_some(Keycodes { bindings, modifiers })
    }
}

/// Edge detection over successive keymaps.
pub struct Chords {
    codes: Keycodes,
    /// Each binding's key was down at the last poll.
    was_down: Vec<bool>,
    primed: bool,
}

impl Chords {
    pub fn new(codes: Keycodes) -> Chords {
        let n = codes.bindings.len();
        Chords { codes, was_down: vec![false; n], primed: false }
    }

    /// The modifiers held in `km`.
    pub fn mods(&self, km: &Keymap) -> u8 {
        self.codes.modifiers.iter().filter(|(c, _)| km.down(*c)).fold(0, |a, (_, b)| a | b)
    }

    /// One poll: the action whose key went down since the last poll with exactly its
    /// modifiers held. The first poll only records what's held (a key already down when
    /// polling starts is no press).
    pub fn step(&mut self, km: &Keymap) -> Option<Action> {
        let mods = self.mods(km);
        let mut out = None;
        for (i, &(code, want, action)) in self.codes.bindings.iter().enumerate() {
            let down = km.down(code);
            if self.primed && down && !self.was_down[i] && mods == want && out.is_none() {
                out = Some(action);
            }
            self.was_down[i] = down;
        }
        self.primed = true;
        out
    }
}

// ---- libX11, loaded at run time ----------------------------------------------------------

type Display = c_void;
type XIOErrorExitHandler = unsafe extern "C" fn(*mut Display, *mut c_void);
type XOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut Display;
/// Returns a `KeyCode` (unsigned char).
type XKeysymToKeycode = unsafe extern "C" fn(*mut Display, c_ulong) -> u8;
type XQueryKeymap = unsafe extern "C" fn(*mut Display, *mut c_char) -> c_int;
type XSetIOErrorExitHandler = unsafe extern "C" fn(*mut Display, Option<XIOErrorExitHandler>, *mut c_void);

struct Xlib {
    open_display: XOpenDisplay,
    keysym_to_keycode: XKeysymToKeycode,
    query_keymap: XQueryKeymap,
    /// libX11 ≥ 1.7: without it a broken connection runs Xlib's default exit.
    set_io_error_exit_handler: Option<XSetIOErrorExitHandler>,
}

impl Xlib {
    /// `dlopen("libX11.so.6")`. Never `dlclose`d: the layer stays mapped (`-z nodelete`) and
    /// so does its connection.
    fn load() -> Option<Xlib> {
        // SAFETY: plain dlopen/dlsym; the symbols' types are Xlib's documented prototypes.
        unsafe {
            let h = libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if h.is_null() {
                return None;
            }
            let sym = |n: &CStr| {
                let p = libc::dlsym(h, n.as_ptr());
                (!p.is_null()).then_some(p)
            };
            Some(Xlib {
                open_display: std::mem::transmute::<*mut c_void, XOpenDisplay>(sym(c"XOpenDisplay")?),
                keysym_to_keycode: std::mem::transmute::<*mut c_void, XKeysymToKeycode>(sym(c"XKeysymToKeycode")?),
                query_keymap: std::mem::transmute::<*mut c_void, XQueryKeymap>(sym(c"XQueryKeymap")?),
                set_io_error_exit_handler: sym(c"XSetIOErrorExitHandler").map(|p| std::mem::transmute::<*mut c_void, XSetIOErrorExitHandler>(p)),
            })
        }
    }
}

/// Set by the IO error exit handler: the connection is dead.
static IO_ERROR: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn on_io_error(_: *mut Display, _: *mut c_void) {
    // Returning (instead of Xlib's default exit()) leaves the Display marked dead: every
    // later call on it returns at once.
    IO_ERROR.store(true, Ordering::Relaxed);
}

/// The layer's own X connection, used only by the keys thread.
pub struct X11Keys {
    x: Xlib,
    dpy: *mut Display,
}

// SAFETY: the connection is used by one thread at a time (the keys thread, or a test).
unsafe impl Send for X11Keys {}

impl X11Keys {
    /// Loads libX11 and connects to `display` (`None`: `DISPLAY`). Errors are `KEYS_*`.
    pub fn open(display: Option<&CStr>) -> Result<X11Keys, u32> {
        let x = Xlib::load().ok_or(KEYS_NO_LIBX11)?;
        // SAFETY: a NUL-terminated name or NULL.
        let dpy = unsafe { (x.open_display)(display.map_or(std::ptr::null(), CStr::as_ptr)) };
        if dpy.is_null() {
            return Err(KEYS_OPEN_FAILED);
        }
        if let Some(set) = x.set_io_error_exit_handler {
            // SAFETY: our live Display; the handler only stores an atomic.
            unsafe { set(dpy, Some(on_io_error), std::ptr::null_mut()) };
        }
        Ok(X11Keys { x, dpy })
    }

    /// The keycode of `sym` (0: not in the keymap).
    pub fn keycode(&self, sym: u64) -> u8 {
        // SAFETY: our live Display.
        unsafe { (self.x.keysym_to_keycode)(self.dpy, sym as c_ulong) }
    }

    pub fn keycodes(&self, bindings: &[Binding]) -> Option<Keycodes> {
        Keycodes::resolve(bindings, |s| self.keycode(s))
    }

    /// The keymap now; `None` once the connection broke.
    pub fn keymap(&self) -> Option<Keymap> {
        if IO_ERROR.load(Ordering::Relaxed) {
            return None;
        }
        let mut km = [0 as c_char; 32];
        // SAFETY: our live Display and a 32-byte buffer.
        let ok = unsafe { (self.x.query_keymap)(self.dpy, km.as_mut_ptr()) };
        if IO_ERROR.load(Ordering::Relaxed) || ok == 0 {
            return None;
        }
        Some(Keymap(km.map(|b| b as u8)))
    }
}

// ---- The thread --------------------------------------------------------------------------

/// `QUIVER_SIGHT_KEYS=0` turns polling off.
fn disabled_by_env() -> bool {
    std::env::var(crate::env::KEYS).is_ok_and(|v| v == "0")
}

/// Starts polling for the session's telemetry (once per process).
pub fn start(t: &'static Telemetry) {
    if disabled_by_env() {
        return t.set_keys(KEYS_OFF);
    }
    // Checked here so "no DISPLAY" doesn't cost a thread.
    if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
        log!("keys: DISPLAY not set; no in-game HUD keys");
        return t.set_keys(KEYS_NO_DISPLAY);
    }
    let spawned = std::thread::Builder::new().name("quiver-sight-keys".into()).stack_size(256 << 10).spawn(move || {
        if std::panic::catch_unwind(|| run(t)).is_err() {
            log!("keys: panic; polling off");
            t.set_keys(KEYS_INTERNAL);
        }
    });
    if let Err(e) = spawned {
        log!("keys: no thread: {e}");
        t.set_keys(KEYS_INTERNAL);
    }
}

fn run(t: &'static Telemetry) {
    let x = match X11Keys::open(None) {
        Ok(x) => x,
        Err(why) => {
            log!("keys: {}", if why == KEYS_NO_LIBX11 { "libX11.so.6 not loadable" } else { "XOpenDisplay failed" });
            return t.set_keys(why);
        }
    };
    let Some(codes) = x.keycodes(&DEFAULT_BINDINGS) else {
        log!("keys: F3/Shift not in the X keymap");
        return t.set_keys(KEYS_NO_KEYCODES);
    };
    log!("keys: polling ({:?})", codes.bindings);
    t.set_keys(KEYS_ACTIVE);
    let mut chords = Chords::new(codes);
    let mut presents = t.presents.load(Ordering::Relaxed);
    loop {
        std::thread::sleep(POLL_EVERY);
        // Only while the game presents: a game that stopped presenting has no HUD to change
        // and, usually, no focus.
        let p = t.presents.load(Ordering::Relaxed);
        if p == presents {
            continue;
        }
        presents = p;
        let Some(km) = x.keymap() else {
            log!("keys: the X connection broke; polling off");
            return t.set_keys(KEYS_IO_ERROR);
        };
        if let Some(a) = chords.step(&km) {
            log!("keys: {a:?}");
            t.key_pressed(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Codes as on a PC keyboard under evdev: F3 69, Shift_L 50, Shift_R 62, Control_L 37,
    // Control_R 105, Alt_L 64, Super_L 133.
    const F3: u8 = 69;
    const SHL: u8 = 50;
    const SHR: u8 = 62;
    const CTL: u8 = 37;
    const CTR: u8 = 105;
    const ALT: u8 = 64;
    const SUP: u8 = 133;

    fn lookup(sym: u64) -> u8 {
        match sym {
            keysym::F3 => F3,
            keysym::SHIFT_L => SHL,
            keysym::SHIFT_R => SHR,
            keysym::CONTROL_L => CTL,
            keysym::CONTROL_R => CTR,
            keysym::ALT_L => ALT,
            keysym::SUPER_L => SUP,
            _ => 0,
        }
    }

    fn chords() -> Chords {
        Chords::new(Keycodes::resolve(&DEFAULT_BINDINGS, lookup).unwrap())
    }

    /// Feeds keymaps (lists of held codes) and returns what each poll did.
    fn run(c: &mut Chords, polls: &[&[u8]]) -> Vec<Option<Action>> {
        polls.iter().map(|held| c.step(&Keymap::with(held))).collect()
    }

    #[test]
    fn keymap_bits() {
        let k = Keymap::with(&[F3, 255, 8]);
        assert!(k.down(F3) && k.down(255) && k.down(8));
        assert!(!k.down(SHL) && !k.down(0));
        assert_eq!(k.0[F3 as usize / 8], 1 << (F3 % 8));
    }

    #[test]
    fn shift_f3_toggles_on_the_edge_only() {
        let mut c = chords();
        let got = run(&mut c, &[&[], &[SHL], &[SHL, F3], &[SHL, F3], &[SHL, F3], &[SHL], &[SHL, F3], &[]]);
        assert_eq!(got, [None, None, Some(Action::Toggle), None, None, None, Some(Action::Toggle), None]);
        // Either Shift.
        assert_eq!(run(&mut c, &[&[SHR], &[SHR, F3]]), [None, Some(Action::Toggle)]);
    }

    #[test]
    fn ctrl_shift_f3_is_next_and_never_toggle() {
        let mut c = chords();
        let got = run(&mut c, &[&[], &[CTL, SHL], &[CTL, SHL, F3], &[CTL, SHL, F3], &[]]);
        assert_eq!(got, [None, None, Some(Action::Next), None, None]);
        assert_eq!(run(&mut c, &[&[CTR, SHR, F3], &[]]), [Some(Action::Next), None]);
        // Shift+F3 held, then Ctrl added: F3 didn't go down again, so nothing.
        assert_eq!(run(&mut c, &[&[SHL, F3], &[CTL, SHL, F3], &[]]), [Some(Action::Toggle), None, None]);
    }

    #[test]
    fn other_modifiers_or_none_do_nothing() {
        let mut c = chords();
        let got = run(&mut c, &[&[], &[F3], &[], &[CTL, F3], &[], &[ALT, SHL, F3], &[], &[SUP, SHL, F3], &[], &[ALT, CTL, SHL, F3], &[]]);
        assert!(got.iter().all(Option::is_none), "{got:?}");
    }

    #[test]
    fn a_chord_held_when_polling_starts_is_no_press() {
        let mut c = chords();
        assert_eq!(run(&mut c, &[&[SHL, F3], &[SHL, F3], &[SHL], &[SHL, F3]]), [None, None, None, Some(Action::Toggle)]);
    }

    #[test]
    fn resolving_keycodes() {
        // No Super, no Alt: still fine.
        let k = Keycodes::resolve(&DEFAULT_BINDINGS, |s| if matches!(s, keysym::F3 | keysym::SHIFT_L | keysym::CONTROL_L) { lookup(s) } else { 0 }).unwrap();
        assert_eq!(k.bindings, [(F3, MOD_SHIFT, Action::Toggle), (F3, MOD_CTRL | MOD_SHIFT, Action::Next)]);
        assert_eq!(k.modifiers, [(SHL, MOD_SHIFT), (CTL, MOD_CTRL)]);
        // No Ctrl: only the toggle (its chord can't be held).
        let k = Keycodes::resolve(&DEFAULT_BINDINGS, |s| if matches!(s, keysym::F3 | keysym::SHIFT_L) { lookup(s) } else { 0 }).unwrap();
        assert_eq!(k.bindings, [(F3, MOD_SHIFT, Action::Toggle)]);
        // No F3, or no Shift: nothing to poll for.
        assert!(Keycodes::resolve(&DEFAULT_BINDINGS, |s| if s == keysym::F3 { 0 } else { lookup(s) }).is_none());
        assert!(Keycodes::resolve(&DEFAULT_BINDINGS, |s| if matches!(s, keysym::SHIFT_L | keysym::SHIFT_R) { 0 } else { lookup(s) }).is_none());
    }
}
