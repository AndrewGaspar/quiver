//! Whether the game's window is the one in front, as the game's own connections see it
//! (design/SIGHT.md §14: the Sight panel is tied to the game window). `quiver-sight` uses it
//! to find the game among the compositor's toplevels and, on compositors without a toplevel
//! protocol, to tell when the game is in front again.
//!
//! - **Wayland windows** ([`crate::wayland`]): the layer's own `wl_keyboard` on the game's
//!   connection: focused between `enter` and `leave`. That is keyboard focus, so a layer-shell
//!   surface holding the keyboard (the panel itself) counts as "not focused".
//! - **X11 windows** (XWayland): `_NET_ACTIVE_WINDOW` on the root window, read on a
//!   connection of the layer's own, compared with the game's Vulkan surface windows and their
//!   ancestors (Wine renders into a client window inside its toplevel), else with the active
//!   window's `_NET_WM_PID`. Hyprland's XWayland window manager keeps the game active while a
//!   layer surface has the keyboard, and sets it to none when a Wayland window is focused.
//!
//! **XCB, not Xlib.** An Xlib request on a window that's just been destroyed raises an X error,
//! and Xlib errors go to the process-wide handler, which in a Wine game is Wine's (it asserts
//! on unexpected errors). The layer's X11 focus connection is plain XCB: errors come back as a
//! reply and are freed, nothing global is touched. `libxcb.so.1` is `dlopen`ed (libX11 needs
//! it anyway); there's no link-time dependency.
//!
//! Read on the keys thread ([`crate::keys`]) every tick, also while the game isn't presenting
//! (a game hidden on another workspace may stop), and written to the status block at once on
//! a change (`LAYER_FOCUS_KNOWN`, `LAYER_FOCUSED`).

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::Mutex;

/// The game's X windows its Vulkan surfaces were made for (from `vkCreateXlibSurfaceKHR` /
/// `vkCreateXcbSurfaceKHR`), newest last. A few at most.
static X11_WINDOWS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
const MAX_WINDOWS: usize = 8;

/// The game made a Vulkan surface for X window `w`.
pub fn note_x11_window(w: u64) {
    let Ok(w) = u32::try_from(w) else { return };
    if w == 0 {
        return;
    }
    let mut v = X11_WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
    if !v.contains(&w) {
        if v.len() == MAX_WINDOWS {
            v.remove(0);
        }
        v.push(w);
    }
}

pub fn x11_windows() -> Vec<u32> {
    X11_WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Both sources together: in front if either says so, not if one says not and none says so,
/// unknown if neither can tell.
pub fn combine(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        _ => None,
    }
}

/// `active` (the root's `_NET_ACTIVE_WINDOW`) is the game's: one of its surface windows or an
/// ancestor of one (`ancestors[i]` are window `i`'s, parent first, up to the root).
pub fn active_is_game(active: u32, windows: &[(u32, Vec<u32>)]) -> bool {
    active != 0 && windows.iter().any(|(w, up)| *w == active || up.contains(&active))
}

// ---- XCB, loaded at run time --------------------------------------------------------------

#[repr(C)]
struct Cookie {
    sequence: c_uint,
}

#[repr(C)]
struct ScreenIterator {
    data: *mut u32, // xcb_screen_t*, whose first field is `root`
    rem: c_int,
    index: c_int,
}

#[repr(C)]
struct InternAtomReply {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    atom: u32,
}

#[repr(C)]
struct GetPropertyReply {
    response_type: u8,
    format: u8,
    sequence: u16,
    length: u32,
    type_: u32,
    bytes_after: u32,
    value_len: u32,
    pad0: [u8; 12],
}

#[repr(C)]
struct QueryTreeReply {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    root: u32,
    parent: u32,
    children_len: u16,
    pad1: [u8; 14],
}

const ATOM_WINDOW: u32 = 33;
const ATOM_CARDINAL: u32 = 6;

type Conn = c_void;

struct Xcb {
    connect: unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut Conn,
    has_error: unsafe extern "C" fn(*mut Conn) -> c_int,
    get_setup: unsafe extern "C" fn(*mut Conn) -> *const c_void,
    roots_iterator: unsafe extern "C" fn(*const c_void) -> ScreenIterator,
    screen_next: unsafe extern "C" fn(*mut ScreenIterator),
    intern_atom: unsafe extern "C" fn(*mut Conn, u8, u16, *const c_char) -> Cookie,
    intern_atom_reply: unsafe extern "C" fn(*mut Conn, Cookie, *mut *mut c_void) -> *mut InternAtomReply,
    get_property: unsafe extern "C" fn(*mut Conn, u8, u32, u32, u32, u32, u32) -> Cookie,
    get_property_reply: unsafe extern "C" fn(*mut Conn, Cookie, *mut *mut c_void) -> *mut GetPropertyReply,
    get_property_value: unsafe extern "C" fn(*const GetPropertyReply) -> *mut c_void,
    get_property_value_length: unsafe extern "C" fn(*const GetPropertyReply) -> c_int,
    query_tree: unsafe extern "C" fn(*mut Conn, u32) -> Cookie,
    query_tree_reply: unsafe extern "C" fn(*mut Conn, Cookie, *mut *mut c_void) -> *mut QueryTreeReply,
    disconnect: unsafe extern "C" fn(*mut Conn),
}

/// A `dlsym` result as the function pointer type `F` it is (the field it's stored in).
unsafe fn as_fn<F: Copy>(p: *mut c_void) -> F {
    const { assert!(size_of::<F>() == size_of::<*mut c_void>()) };
    // SAFETY: the caller's symbol has type F; sizes checked above.
    unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) }
}

impl Xcb {
    /// `dlopen("libxcb.so.1")`, never closed.
    fn load() -> Option<Xcb> {
        // SAFETY: plain dlopen/dlsym; the types are libxcb's documented prototypes.
        unsafe {
            let h = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if h.is_null() {
                return None;
            }
            let sym = |n: &CStr| {
                let p = libc::dlsym(h, n.as_ptr());
                (!p.is_null()).then_some(p)
            };
            macro_rules! f {
                ($n:literal) => {
                    as_fn(sym($n)?)
                };
            }
            Some(Xcb {
                connect: f!(c"xcb_connect"),
                has_error: f!(c"xcb_connection_has_error"),
                get_setup: f!(c"xcb_get_setup"),
                roots_iterator: f!(c"xcb_setup_roots_iterator"),
                screen_next: f!(c"xcb_screen_next"),
                intern_atom: f!(c"xcb_intern_atom"),
                intern_atom_reply: f!(c"xcb_intern_atom_reply"),
                get_property: f!(c"xcb_get_property"),
                get_property_reply: f!(c"xcb_get_property_reply"),
                get_property_value: f!(c"xcb_get_property_value"),
                get_property_value_length: f!(c"xcb_get_property_value_length"),
                query_tree: f!(c"xcb_query_tree"),
                query_tree_reply: f!(c"xcb_query_tree_reply"),
                disconnect: f!(c"xcb_disconnect"),
            })
        }
    }
}

/// Frees what libxcb handed us (`malloc`ed replies and errors).
unsafe fn free<T>(p: *mut T) {
    if !p.is_null() {
        // SAFETY: libxcb allocates replies and errors with malloc.
        unsafe { libc::free(p.cast()) };
    }
}

/// The layer's own XCB connection for reading the active window.
pub struct X11Focus {
    x: Xcb,
    c: *mut Conn,
    root: u32,
    net_active_window: u32,
    net_wm_pid: u32,
    /// Each surface window's ancestors, found once per window.
    tree: Vec<(u32, Vec<u32>)>,
    pid: u32,
}

// SAFETY: one thread at a time (the keys thread, or a test).
unsafe impl Send for X11Focus {}

impl Drop for X11Focus {
    fn drop(&mut self) {
        // SAFETY: our connection, disconnected once.
        unsafe { (self.x.disconnect)(self.c) };
    }
}

impl X11Focus {
    /// Connects to `display` (`None`: `DISPLAY`). `None` when libxcb or the server isn't
    /// there, or the window manager doesn't do `_NET_ACTIVE_WINDOW`'s atom.
    pub fn open(display: Option<&CStr>) -> Option<X11Focus> {
        let x = Xcb::load()?;
        let mut screen: c_int = 0;
        // SAFETY: a NUL-terminated name or NULL; a valid out pointer.
        let c = unsafe { (x.connect)(display.map_or(std::ptr::null(), CStr::as_ptr), &mut screen) };
        // SAFETY: xcb_connect never returns NULL (an error connection instead).
        if c.is_null() || unsafe { (x.has_error)(c) } != 0 {
            if !c.is_null() {
                // SAFETY: frees the error connection.
                unsafe { (x.disconnect)(c) };
            }
            return None;
        }
        // SAFETY: the setup of a live connection; the iterator walks its screens.
        let root = unsafe {
            let mut it = (x.roots_iterator)((x.get_setup)(c));
            for _ in 0..screen.max(0) {
                if it.rem <= 0 {
                    break;
                }
                (x.screen_next)(&mut it);
            }
            if it.data.is_null() || it.rem <= 0 { 0 } else { *it.data }
        };
        let mut f = X11Focus { x, c, root, net_active_window: 0, net_wm_pid: 0, tree: Vec::new(), pid: std::process::id() };
        if root == 0 {
            return None;
        }
        f.net_active_window = f.atom(c"_NET_ACTIVE_WINDOW")?;
        f.net_wm_pid = f.atom(c"_NET_WM_PID").unwrap_or(0);
        Some(f)
    }

    /// An existing atom (`only_if_exists`: a WM that never set it has no active window).
    fn atom(&self, name: &CStr) -> Option<u32> {
        let n = name.to_bytes();
        // SAFETY: our connection; the reply and any error are freed.
        unsafe {
            let ck = (self.x.intern_atom)(self.c, 1, n.len() as u16, n.as_ptr().cast());
            let mut err = std::ptr::null_mut();
            let r = (self.x.intern_atom_reply)(self.c, ck, &mut err);
            free(err);
            if r.is_null() {
                return None;
            }
            let a = (*r).atom;
            free(r);
            (a != 0).then_some(a)
        }
    }

    /// One 32-bit value of `prop` (of type `ty`) on `w`; `None` when unset or `w` is gone.
    fn prop32(&self, w: u32, prop: u32, ty: u32) -> Option<u32> {
        // SAFETY: our connection; the reply's value is read within its length, then freed.
        unsafe {
            let ck = (self.x.get_property)(self.c, 0, w, prop, ty, 0, 1);
            let mut err = std::ptr::null_mut();
            let r = (self.x.get_property_reply)(self.c, ck, &mut err);
            free(err);
            if r.is_null() {
                return None;
            }
            let v = if (*r).format == 32 && (*r).type_ == ty && (self.x.get_property_value_length)(r) >= 4 {
                Some(std::ptr::read_unaligned((self.x.get_property_value)(r) as *const u32))
            } else {
                None
            };
            free(r);
            v
        }
    }

    /// `w`'s ancestors, parent first, up to the root (empty when `w` is gone).
    fn ancestors(&self, w: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = w;
        // A handful of levels at most (Wine: client window → whole window → root).
        for _ in 0..16 {
            // SAFETY: our connection; the reply is freed.
            let parent = unsafe {
                let ck = (self.x.query_tree)(self.c, cur);
                let mut err = std::ptr::null_mut();
                let r = (self.x.query_tree_reply)(self.c, ck, &mut err);
                free(err);
                if r.is_null() {
                    return Vec::new();
                }
                let p = (*r).parent;
                free(r);
                p
            };
            if parent == 0 || parent == self.root {
                break;
            }
            out.push(parent);
            cur = parent;
        }
        out
    }

    /// The connection broke (the X server went away).
    pub fn broken(&self) -> bool {
        // SAFETY: our connection.
        unsafe { (self.x.has_error)(self.c) != 0 }
    }

    /// Whether the active window is the game's: `None` when the window manager doesn't say
    /// (no `_NET_ACTIVE_WINDOW` on the root) or the connection broke.
    pub fn focused(&mut self, windows: &[u32]) -> Option<bool> {
        if self.broken() {
            return None;
        }
        let active = self.prop32(self.root, self.net_active_window, ATOM_WINDOW)?;
        if active == 0 {
            return Some(false);
        }
        // New surface windows: their ancestors, once (the window exists right after the
        // surface was made; if it's gone already, it can't be in front either).
        self.tree.retain(|(w, _)| windows.contains(w));
        for &w in windows {
            if !self.tree.iter().any(|(x, _)| *x == w) {
                let up = self.ancestors(w);
                self.tree.push((w, up));
            }
        }
        if active_is_game(active, &self.tree) {
            return Some(true);
        }
        // No surface window matched (or none known): the active window's pid. Wine sets
        // `_NET_WM_PID` on its toplevels; a pid namespace (pressure-vessel) is the same on
        // both sides, since the game wrote it.
        Some(self.net_wm_pid != 0 && self.prop32(active, self.net_wm_pid, ATOM_CARDINAL) == Some(self.pid))
    }

    /// The root window (tests set `_NET_ACTIVE_WINDOW` on it: Xvfb has no window manager).
    #[doc(hidden)]
    pub fn root(&self) -> u32 {
        self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combining_sources() {
        assert_eq!(combine(None, None), None);
        assert_eq!(combine(Some(false), None), Some(false));
        assert_eq!(combine(None, Some(true)), Some(true));
        assert_eq!(combine(Some(false), Some(true)), Some(true));
        assert_eq!(combine(Some(false), Some(false)), Some(false));
    }

    #[test]
    fn active_window_matching() {
        // Wine: the surface is on a client window (0x3) inside the toplevel (0x2).
        let tree = vec![(0x3, vec![0x2])];
        assert!(active_is_game(0x2, &tree));
        assert!(active_is_game(0x3, &tree));
        assert!(!active_is_game(0x4, &tree));
        assert!(!active_is_game(0, &tree));
        assert!(!active_is_game(0x2, &[]));
    }

    #[test]
    fn surface_windows_are_kept_few_and_unique() {
        for w in [0u64, 1, 2, 2, 3, 4, 5, 6, 7, 8, 9, u64::MAX] {
            note_x11_window(w);
        }
        let v = x11_windows();
        assert_eq!(v.len(), MAX_WINDOWS);
        assert_eq!(v.first(), Some(&2));
        assert_eq!(v.last(), Some(&9));
        X11_WINDOWS.lock().unwrap().clear();
    }
}
