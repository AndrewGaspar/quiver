//! `libquiver_sight_input.so`: the Sight panel's input in the game (design/SIGHT.md §14, "In
//! the game"). Quiver puts it in `LD_PRELOAD` for launches with the Sight layer, the way Steam
//! preloads `gameoverlayrenderer.so`: it interposes the X11 and Wayland calls Wine's drivers
//! read input with ([`x11`], [`wayland`]), and while the panel drawn in the game captures input
//! it keeps keyboard and mouse events from Wine (the game sees none) and forwards them to
//! quiver-sight through the session dir's `panel.input` ([`quiver_perf::panel`]). The panel
//! never takes keyboard focus from the game, so exclusive-fullscreen games don't minimise.
//!
//! It has to be a preload: winex11 and winewayland bind their Xlib and libwayland symbols when
//! Wine loads them, long before a Vulkan layer is loaded, and a symbol can only be interposed
//! by an object ahead of the library in the lookup order.
//!
//! **Rules** (it runs in every process of the game's Wine prefix):
//!
//! - Inert until used: nothing runs at load; the first interposed call starts a small thread
//!   ([`link`]), and only when `QUIVER_PERF_DIR` is set. `QUIVER_SIGHT_INPUT=0` turns it off.
//! - Never blocks Wine's threads: the interposed calls load atomics, read the vDSO clock and
//!   write lock-free ring slots; the thread does the file mapping.
//! - Fails open: input is captured only while quiver-sight renews a short deadline, the layer
//!   keeps drawing the panel, and quiver-sight's process exists ([`quiver_perf::panel::capture`]).
//!   Any panic in the preload passes the event through.
//! - Nothing held gets stuck ([`filter`]): releases of keys and buttons the game saw pressed
//!   go to the game even while capturing; releases of what was pressed in the panel never do.

#![allow(clippy::missing_safety_doc)]

pub mod filter;
pub mod link;
pub mod wayland;
pub mod x11;

use std::ffi::{CStr, c_void};

/// Logs to stderr with `QUIVER_SIGHT_LOG=1` (events only, never per input event).
#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($t:tt)*) => {
        if $crate::log_enabled() {
            use std::io::Write as _;
            // One write per line: every process of the prefix appends to the same log.
            let line = format!("quiver-sight-input[{}]: {}\n", std::process::id(), format_args!($($t)*));
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    };
}

#[doc(hidden)]
pub fn log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("QUIVER_SIGHT_LOG").is_ok_and(|v| !v.is_empty() && v != "0"))
}

/// The real `name`: the next definition after this library (`RTLD_NEXT`), or, when the
/// library that has it was loaded locally (winewayland's libwayland-client is a dependency of
/// a `dlopen`ed module, not in the global scope), from that already-loaded library. 0 when
/// neither has it.
pub(crate) fn real_symbol(lib: &CStr, name: &CStr) -> usize {
    // SAFETY: plain dlsym/dlopen lookups with NUL-terminated names.
    unsafe {
        let p = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if !p.is_null() {
            return p as usize;
        }
        let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
        if h.is_null() {
            return 0;
        }
        let p: *mut c_void = libc::dlsym(h, name.as_ptr());
        // Keep the reference: the library stays loaded for as long as the caller uses it.
        p as usize
    }
}
