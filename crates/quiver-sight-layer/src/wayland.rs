//! The HUD's keys in Wayland-native windows (SIGHT-HUD §1.3, as built): winewayland and
//! native Wayland games never show XWayland their keys, so the layer reads them from a
//! `wl_keyboard` of its own on the game's own `wl_display`, as MangoHud does.
//!
//! **What it makes.** When the game creates a Wayland `VkSurfaceKHR`, the surface hook keeps
//! the create info's `wl_display*` ([`surface_created`]). The keys thread
//! ([`crate::keys`]) then, on its next tick while the game presents ([`service`]):
//!
//! 1. finds the `libwayland-client.so.0` the game already loaded (`RTLD_NOLOAD`: the display
//!    must be driven by the copy that made it; there's no link-time dependency);
//! 2. makes a private event queue on that display (`wl_display_create_queue`) and gets the
//!    registry through a proxy wrapper set to that queue, so the registry, and the seat and
//!    keyboard made from it, exist on our queue from their first event;
//! 3. binds the first `wl_seat` (≤ v5) and, when it has a keyboard, gets a `wl_keyboard`.
//!    The compositor sends key events to every `wl_keyboard` of the client whose surface has
//!    focus (Hyprland: `CSeatManager::sendKeyboardKey` loops over every seat resource of the
//!    focused client; wlroots: every keyboard of the `wlr_seat_client`), and a keyboard made
//!    while the client has focus gets an `enter` at once. So ours sees keys exactly while the
//!    game's window has focus, alongside the game's own keyboard, which still gets them all.
//!
//! **Sharing the game's connection safely.** The rule is that the layer never reads the
//! socket and never dispatches anything but its own queue:
//!
//! - libwayland is thread-safe for this split: whoever reads the socket (Wine's
//!   `waylanddrv_unix_read_events` thread, which loops on `wl_display_dispatch_queue` on
//!   Wine's queue; a native game's event pump; the driver's WSI) sorts each event onto the
//!   queue of the proxy it's for, under the display's mutex. Events for our proxies land on
//!   our queue and wait there; nothing else ever dispatches our queue, and we never dispatch
//!   theirs, so no app listener runs on our thread and none of ours on theirs.
//! - We only call `wl_display_dispatch_queue_pending(display, our_queue)` (dispatches what's
//!   already queued; never reads, never blocks) and `wl_display_flush` (non-blocking
//!   `sendmsg(MSG_DONTWAIT)`; an `EAGAIN` is retried next tick). Requests (`get_registry`,
//!   `bind`, `get_keyboard`, `release`) are marshalled under the display mutex like any
//!   other thread's.
//! - **No roundtrip.** MangoHud does two `wl_display_roundtrip_queue`s on our kind of queue
//!   when the surface is created. That's allowed by libwayland's read protocol
//!   (`prepare_read_queue` / `read_events`: concurrent readers wait for the last one to read),
//!   but it blocks the caller until the compositor answers, and while it sits prepared in
//!   `poll` it's one of the readers Wine's thread waits for in `wl_display_read_events`. We
//!   don't need it: the game already reads the socket continuously (it couldn't run
//!   otherwise), so the registry's globals and the seat's capabilities arrive on our queue
//!   by themselves and the binding finishes over a few ticks. Never taking part in reading is
//!   what guarantees we can't stall the game's reader.
//! - Listeners run only inside our dispatch, on the keys thread, under [`SHARED`]'s lock;
//!   they catch panics (none may unwind into libwayland) and touch only our state. As a
//!   guard, a listener that finds itself outside our dispatch (impossible unless an object
//!   ended up on another queue) touches nothing and the next tick tears down (`internal`).
//!   The keymap fd the compositor sends is closed unread (raw evdev codes need no keymap).
//! - Locks: ours, then libwayland's display mutex inside its calls; libwayland never holds
//!   its mutex while calling a listener, and nothing of the game's takes ours, so no cycle.
//!   The app thread takes ours only in the surface and instance hooks, briefly (nothing done
//!   under it blocks).
//!
//! **Lifetime.** The objects live while a Wayland surface on that display does. The
//! `vkDestroySurfaceKHR` of the display's last surface (or `vkDestroyInstance` of an
//! instance that still has some) tears them down first, synchronously, before the call goes
//! down the chain: the app may disconnect the display right after, and the Vulkan spec
//! requires the display to outlive the surface. Teardown destroys only our objects, children
//! first: the keyboard (`release`, v3+), the seat (`release`, v5+), the registry, then the
//! queue (after its proxies, as libwayland requires). Swapchains don't matter: dispatching
//! is gated on presents, and a recreated swapchain uses the same surface. At process exit an
//! `atexit` handler stops dispatching and forgets the objects without calling libwayland
//! (the game may already have disconnected).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use quiver_perf::layout::{
    KEYS_WL_ACTIVE, KEYS_WL_DISPLAY_ERROR, KEYS_WL_INTERNAL, KEYS_WL_NO_KEYBOARD, KEYS_WL_NO_LIBWAYLAND, KEYS_WL_NO_SEAT, KEYS_WL_NONE, KEYS_WL_STARTING, KEYS_WL_STOPPED,
};

use crate::keys::{Action, EvdevKeys};
use crate::log;

/// How long the seat and keyboard may take to appear before the state says why not.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Highest `wl_seat` version bound: v5 has `release` for both seat and keyboard, and the
/// keyboard events through `repeat_info` (v4), which the listener below covers.
const SEAT_VERSION: u32 = 5;
const WL_SEAT_CAPABILITY_KEYBOARD: u32 = 2;
// Request opcodes (wayland.xml).
const WL_DISPLAY_GET_REGISTRY: u32 = 1;
const WL_REGISTRY_BIND: u32 = 0;
const WL_SEAT_GET_KEYBOARD: u32 = 1;
const WL_SEAT_RELEASE: u32 = 3;
const WL_KEYBOARD_RELEASE: u32 = 0;

type Proxy = c_void;

#[repr(C)]
struct WlArray {
    size: usize,
    alloc: usize,
    data: *mut c_void,
}

/// The head of `struct wl_interface`.
#[repr(C)]
struct WlInterfaceHead {
    name: *const c_char,
    version: c_int,
}

// ---- libwayland-client, the game's copy ---------------------------------------------------

type FnPtrPtr = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnPtr = unsafe extern "C" fn(*mut c_void);
type FnPtrInt = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnPtrPtrInt = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type FnPtrPtr0 = unsafe extern "C" fn(*mut c_void, *mut c_void);
type FnAddListener = unsafe extern "C" fn(*mut Proxy, *const c_void, *mut c_void) -> c_int;
type FnMarshalConstructor = unsafe extern "C" fn(*mut Proxy, u32, *const c_void, ...) -> *mut Proxy;
type FnMarshalConstructorVersioned = unsafe extern "C" fn(*mut Proxy, u32, *const c_void, u32, ...) -> *mut Proxy;
type FnMarshal = unsafe extern "C" fn(*mut Proxy, u32, ...);

struct Lib {
    create_queue: FnPtrPtr,
    queue_destroy: FnPtr,
    dispatch_queue_pending: FnPtrPtrInt,
    flush: FnPtrInt,
    get_error: FnPtrInt,
    create_wrapper: FnPtrPtr,
    wrapper_destroy: FnPtr,
    set_queue: FnPtrPtr0,
    add_listener: FnAddListener,
    marshal_constructor: FnMarshalConstructor,
    marshal_constructor_versioned: FnMarshalConstructorVersioned,
    marshal: FnMarshal,
    proxy_destroy: FnPtr,
    registry_iface: *const c_void,
    seat_iface: *const c_void,
    keyboard_iface: *const c_void,
}

// SAFETY: function pointers and pointers to libwayland's immutable interface tables.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

impl Lib {
    /// The `libwayland-client.so.0` already in the process, or `None`. Never `dlclose`d.
    /// Everything used is in libwayland ≥ 1.11 (2016; `wl_proxy_create_wrapper`).
    fn get() -> Option<&'static Lib> {
        static LIB: OnceLock<&'static Lib> = OnceLock::new();
        if let Some(l) = LIB.get() {
            return Some(l);
        }
        // SAFETY: plain dlopen/dlsym; the types are libwayland-client's public prototypes.
        let lib = unsafe { Self::load() }?;
        Some(LIB.get_or_init(|| Box::leak(Box::new(lib))))
    }

    unsafe fn load() -> Option<Lib> {
        // SAFETY: see get.
        unsafe {
            let h = libc::dlopen(c"libwayland-client.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
            if h.is_null() {
                return None;
            }
            let sym = |n: &CStr| {
                let p = libc::dlsym(h, n.as_ptr());
                (!p.is_null()).then_some(p)
            };
            macro_rules! f {
                ($n:literal, $t:ty) => {
                    std::mem::transmute::<*mut c_void, $t>(sym($n)?)
                };
            }
            Some(Lib {
                create_queue: f!(c"wl_display_create_queue", FnPtrPtr),
                queue_destroy: f!(c"wl_event_queue_destroy", FnPtr),
                dispatch_queue_pending: f!(c"wl_display_dispatch_queue_pending", FnPtrPtrInt),
                flush: f!(c"wl_display_flush", FnPtrInt),
                get_error: f!(c"wl_display_get_error", FnPtrInt),
                create_wrapper: f!(c"wl_proxy_create_wrapper", FnPtrPtr),
                wrapper_destroy: f!(c"wl_proxy_wrapper_destroy", FnPtr),
                set_queue: f!(c"wl_proxy_set_queue", FnPtrPtr0),
                add_listener: f!(c"wl_proxy_add_listener", FnAddListener),
                marshal_constructor: f!(c"wl_proxy_marshal_constructor", FnMarshalConstructor),
                marshal_constructor_versioned: f!(c"wl_proxy_marshal_constructor_versioned", FnMarshalConstructorVersioned),
                marshal: f!(c"wl_proxy_marshal", FnMarshal),
                proxy_destroy: f!(c"wl_proxy_destroy", FnPtr),
                registry_iface: sym(c"wl_registry_interface")?,
                seat_iface: sym(c"wl_seat_interface")?,
                keyboard_iface: sym(c"wl_keyboard_interface")?,
            })
        }
    }
}

// ---- Listeners ---------------------------------------------------------------------------

/// Our objects and what their events told us. Owned by a [`Conn`] (a `Box` behind a raw
/// pointer, which is the listeners' `data`); only touched on the thread holding [`SHARED`].
struct Listen {
    lib: &'static Lib,
    registry: *mut Proxy,
    seat: *mut Proxy,
    /// The seat's registry name, for `global_remove`.
    seat_name: u32,
    seat_version: u32,
    /// The seat's last `capabilities`.
    caps: Option<u32>,
    keyboard: *mut Proxy,
    keys: EvdevKeys,
    presses: Vec<Action>,
    /// Requests were made: flush after dispatching.
    flush: bool,
    /// A request failed or a listener panicked: tear down.
    broken: bool,
}

impl Listen {
    fn global(&mut self, name: u32, interface: &CStr, version: u32) {
        if !self.seat.is_null() || interface != c"wl_seat" {
            return;
        }
        let v = version.min(SEAT_VERSION);
        // SAFETY: our registry; `wl_registry.bind(name, interface, version, new_id)` as
        // wayland-client-protocol.h's wl_registry_bind marshals it. The new proxy inherits
        // the registry's queue (ours).
        unsafe {
            let iface_name = (*(self.lib.seat_iface as *const WlInterfaceHead)).name;
            let seat = (self.lib.marshal_constructor_versioned)(self.registry, WL_REGISTRY_BIND, self.lib.seat_iface, v, name, iface_name, v, std::ptr::null::<c_void>());
            if seat.is_null() {
                self.broken = true;
                return;
            }
            (self.lib.add_listener)(seat, (&SEAT_LISTENER as *const SeatListener).cast(), (self as *mut Listen).cast());
            self.seat = seat;
        }
        self.seat_name = name;
        self.seat_version = v;
        self.flush = true;
    }

    fn global_remove(&mut self, name: u32) {
        if !self.seat.is_null() && name == self.seat_name {
            self.release_seat();
        }
    }

    fn capabilities(&mut self, caps: u32) {
        self.caps = Some(caps);
        let has = caps & WL_SEAT_CAPABILITY_KEYBOARD != 0;
        if has && self.keyboard.is_null() {
            // SAFETY: our seat; wl_seat_get_keyboard's marshalling. Inherits our queue.
            unsafe {
                let kb = (self.lib.marshal_constructor)(self.seat, WL_SEAT_GET_KEYBOARD, self.lib.keyboard_iface, std::ptr::null::<c_void>());
                if kb.is_null() {
                    self.broken = true;
                    return;
                }
                (self.lib.add_listener)(kb, (&KEYBOARD_LISTENER as *const KeyboardListener).cast(), (self as *mut Listen).cast());
                self.keyboard = kb;
            }
            self.flush = true;
        } else if !has && !self.keyboard.is_null() {
            self.release_keyboard();
        }
    }

    fn release_keyboard(&mut self) {
        if self.keyboard.is_null() {
            return;
        }
        // SAFETY: our keyboard, destroyed once. `release` exists from v3 (the keyboard has the
        // seat's version); before that the proxy is only destroyed locally.
        unsafe {
            if self.seat_version >= 3 {
                (self.lib.marshal)(self.keyboard, WL_KEYBOARD_RELEASE);
            }
            (self.lib.proxy_destroy)(self.keyboard);
        }
        self.keyboard = std::ptr::null_mut();
        self.keys.leave();
        self.flush = true;
    }

    fn release_seat(&mut self) {
        self.release_keyboard();
        if self.seat.is_null() {
            return;
        }
        // SAFETY: our seat, after its keyboard; `release` exists from v5.
        unsafe {
            if self.seat_version >= 5 {
                (self.lib.marshal)(self.seat, WL_SEAT_RELEASE);
            }
            (self.lib.proxy_destroy)(self.seat);
        }
        self.seat = std::ptr::null_mut();
        self.caps = None;
        self.flush = true;
    }
}

thread_local! {
    /// Set on the keys thread while it dispatches our queue.
    static IN_DISPATCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One of our listeners ran outside our own dispatch (it can't, by construction: a guard).
static MISDISPATCHED: AtomicBool = AtomicBool::new(false);

/// Runs a listener body on our [`Listen`]; a panic marks it broken instead of unwinding into
/// libwayland.
unsafe fn with(data: *mut c_void, f: impl FnOnce(&mut Listen)) {
    if data.is_null() {
        return;
    }
    if !IN_DISPATCH.with(|d| d.get()) {
        // Not our thread: don't touch the Listen (the keys thread owns it); the next tick
        // tears down.
        MISDISPATCHED.store(true, Ordering::Relaxed);
        return;
    }
    let l = data as *mut Listen;
    // SAFETY: `data` is the Listen this proxy was given, alive until teardown destroys the
    // proxy; listeners only run inside our dispatch, which holds SHARED.
    if std::panic::catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *l }))).is_err() {
        unsafe { (*l).broken = true };
    }
}

#[repr(C)]
struct RegistryListener {
    global: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32, *const c_char, u32),
    global_remove: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32),
}

#[repr(C)]
struct SeatListener {
    capabilities: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32),
    name: unsafe extern "C" fn(*mut c_void, *mut Proxy, *const c_char),
}

#[repr(C)]
struct KeyboardListener {
    keymap: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32, i32, u32),
    enter: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32, *mut Proxy, *mut WlArray),
    leave: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32, *mut Proxy),
    key: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32, u32, u32, u32),
    modifiers: unsafe extern "C" fn(*mut c_void, *mut Proxy, u32, u32, u32, u32, u32),
    repeat_info: unsafe extern "C" fn(*mut c_void, *mut Proxy, i32, i32),
}

static REGISTRY_LISTENER: RegistryListener = RegistryListener { global: on_global, global_remove: on_global_remove };
static SEAT_LISTENER: SeatListener = SeatListener { capabilities: on_capabilities, name: on_seat_name };
static KEYBOARD_LISTENER: KeyboardListener =
    KeyboardListener { keymap: on_keymap, enter: on_enter, leave: on_leave, key: on_key, modifiers: on_modifiers, repeat_info: on_repeat_info };

unsafe extern "C" fn on_global(data: *mut c_void, _: *mut Proxy, name: u32, interface: *const c_char, version: u32) {
    if interface.is_null() {
        return;
    }
    // SAFETY: libwayland passes a NUL-terminated string for the event's duration.
    let iface = unsafe { CStr::from_ptr(interface) };
    unsafe { with(data, |l| l.global(name, iface, version)) }
}

unsafe extern "C" fn on_global_remove(data: *mut c_void, _: *mut Proxy, name: u32) {
    unsafe { with(data, |l| l.global_remove(name)) }
}

unsafe extern "C" fn on_capabilities(data: *mut c_void, _: *mut Proxy, caps: u32) {
    unsafe { with(data, |l| l.capabilities(caps)) }
}

unsafe extern "C" fn on_seat_name(_: *mut c_void, _: *mut Proxy, _: *const c_char) {}

unsafe extern "C" fn on_keymap(_: *mut c_void, _: *mut Proxy, _format: u32, fd: i32, _size: u32) {
    // Ours to close (libwayland dup'd it for this event); raw evdev codes need no keymap.
    if fd >= 0 {
        // SAFETY: an fd this event gave us.
        unsafe { libc::close(fd) };
    }
}

unsafe extern "C" fn on_enter(data: *mut c_void, _: *mut Proxy, _serial: u32, _surface: *mut Proxy, keys: *mut WlArray) {
    // SAFETY: the event's wl_array of u32 evdev codes, valid for the call.
    let held: &[u32] = unsafe {
        match keys.as_ref() {
            Some(a) if !a.data.is_null() && a.size >= 4 => std::slice::from_raw_parts(a.data as *const u32, a.size / 4),
            _ => &[],
        }
    };
    unsafe { with(data, |l| l.keys.enter(held)) }
}

unsafe extern "C" fn on_leave(data: *mut c_void, _: *mut Proxy, _serial: u32, _surface: *mut Proxy) {
    unsafe { with(data, |l| l.keys.leave()) }
}

unsafe extern "C" fn on_key(data: *mut c_void, _: *mut Proxy, _serial: u32, _time: u32, key: u32, state: u32) {
    unsafe {
        with(data, |l| {
            if let Some(a) = l.keys.key(key, state) {
                l.presses.push(a);
            }
        })
    }
}

unsafe extern "C" fn on_modifiers(_: *mut c_void, _: *mut Proxy, _: u32, _: u32, _: u32, _: u32, _: u32) {}

unsafe extern "C" fn on_repeat_info(_: *mut c_void, _: *mut Proxy, _: i32, _: i32) {}

// ---- Connections -------------------------------------------------------------------------

/// Our objects on one display.
struct Conn {
    lib: &'static Lib,
    display: *mut c_void,
    queue: *mut c_void,
    listen: *mut Listen,
    since: Instant,
}

// SAFETY: used only under SHARED's lock (by the keys thread, or the app thread destroying a
// surface); libwayland allows proxies and queues to be used from any thread.
unsafe impl Send for Conn {}

impl Conn {
    /// Sets up on `display` (the game's): queue, then the registry on it. Errors are
    /// `KEYS_WL_*`. No blocking call: the game's own reader brings the replies.
    unsafe fn attach(display: *mut c_void, now: Instant) -> Result<Conn, u32> {
        let lib = Lib::get().ok_or(KEYS_WL_NO_LIBWAYLAND)?;
        // SAFETY: the game's live display (its Wayland surface exists); our own queue and
        // wrapper. The wrapper makes get_registry's new proxy start on our queue, so none of
        // its events can go to the game's queue.
        unsafe {
            let queue = (lib.create_queue)(display);
            if queue.is_null() {
                return Err(KEYS_WL_INTERNAL);
            }
            let wrapper = (lib.create_wrapper)(display);
            if wrapper.is_null() {
                (lib.queue_destroy)(queue);
                return Err(KEYS_WL_INTERNAL);
            }
            (lib.set_queue)(wrapper, queue);
            let registry = (lib.marshal_constructor)(wrapper, WL_DISPLAY_GET_REGISTRY, lib.registry_iface, std::ptr::null::<c_void>());
            (lib.wrapper_destroy)(wrapper);
            if registry.is_null() {
                (lib.queue_destroy)(queue);
                return Err(KEYS_WL_INTERNAL);
            }
            let listen = Box::into_raw(Box::new(Listen {
                lib,
                registry,
                seat: std::ptr::null_mut(),
                seat_name: 0,
                seat_version: 0,
                caps: None,
                keyboard: std::ptr::null_mut(),
                keys: EvdevKeys::new(),
                presses: Vec::with_capacity(8),
                flush: false,
                broken: false,
            }));
            // Before any dispatch of our queue, so no registry event is missed.
            (lib.add_listener)(registry, (&REGISTRY_LISTENER as *const RegistryListener).cast(), listen.cast());
            (lib.flush)(display);
            Ok(Conn { lib, display, queue, listen, since: now })
        }
    }

    /// Destroys our objects (children first, then the queue). The display must be alive.
    unsafe fn teardown(self) {
        // SAFETY: our objects on a live display; after this nothing refers to `listen`.
        unsafe {
            let mut l = Box::from_raw(self.listen);
            l.release_seat();
            (self.lib.proxy_destroy)(l.registry);
            (self.lib.flush)(self.display);
            (self.lib.queue_destroy)(self.queue);
        }
    }
}

// ---- Shared state ------------------------------------------------------------------------

struct Surf {
    instance: u64,
    surface: u64,
    display: usize,
}

struct Shared {
    /// The game's live Wayland surfaces.
    surfaces: Vec<Surf>,
    conn: Option<Conn>,
    /// Displays not to retry (a fatal error, no libwayland, broken setup).
    failed: Vec<usize>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { surfaces: Vec::new(), conn: None, failed: Vec::new() });
/// `KEYS_WL_*`, for the status block.
static STATE: AtomicU32 = AtomicU32::new(KEYS_WL_NONE);
/// Set at process exit: no more libwayland calls.
static EXITING: AtomicBool = AtomicBool::new(false);

fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|p| p.into_inner())
}

fn set_state(s: u32) {
    if STATE.swap(s, Ordering::Relaxed) != s {
        log!("keys: Wayland {}", state_name(s));
    }
}

fn state_name(s: u32) -> &'static str {
    match s {
        KEYS_WL_NONE => "none",
        KEYS_WL_ACTIVE => "keyboard active",
        KEYS_WL_STARTING => "starting",
        KEYS_WL_NO_LIBWAYLAND => "libwayland-client.so.0 not loaded in the game",
        KEYS_WL_NO_SEAT => "no wl_seat",
        KEYS_WL_NO_KEYBOARD => "the seat has no keyboard",
        KEYS_WL_DISPLAY_ERROR => "display error; stopped",
        KEYS_WL_STOPPED => "surfaces destroyed; stopped",
        _ => "internal error",
    }
}

/// The Wayland keyboard's state (`KEYS_WL_*`).
pub fn state() -> u32 {
    STATE.load(Ordering::Relaxed)
}

/// The keys thread died: nothing will service the keyboard any more.
pub fn fail_internal() {
    set_state(KEYS_WL_INTERNAL);
}

/// The game created a Wayland surface on `display` (from `vkCreateWaylandSurfaceKHR`'s
/// create info). Only noted; the keys thread attaches.
pub fn surface_created(instance: u64, surface: u64, display: *mut c_void) {
    if display.is_null() {
        return;
    }
    let mut s = shared();
    s.surfaces.push(Surf { instance, surface, display: display as usize });
    if matches!(state(), KEYS_WL_NONE | KEYS_WL_STOPPED) {
        set_state(KEYS_WL_STARTING);
    }
}

/// `vkDestroySurfaceKHR`, before it goes down the chain.
pub fn surface_destroyed(instance: u64, surface: u64) {
    let mut s = shared();
    s.surfaces.retain(|x| !(x.instance == instance && x.surface == surface));
    release_unused(&mut s);
}

/// `vkDestroyInstance`, before it goes down the chain: its surfaces are gone (or leaked).
pub fn instance_destroyed(instance: u64) {
    let mut s = shared();
    s.surfaces.retain(|x| x.instance != instance);
    release_unused(&mut s);
}

/// Tears down when no live surface uses our display any more (the app may disconnect it as
/// soon as we return).
fn release_unused(s: &mut Shared) {
    if let Some(c) = &s.conn
        && !s.surfaces.iter().any(|x| x.display == c.display as usize)
    {
        let c = s.conn.take().expect("checked");
        // After exit started, at_exit already dropped it (leaking the objects).
        if !EXITING.load(Ordering::Relaxed) {
            // SAFETY: the display's last surface is being destroyed now, so it's still alive.
            unsafe { c.teardown() };
        }
        log!("keys: Wayland objects released (their display's last surface is gone)");
    }
    // A display that failed and has no surface left may be gone; its address can be reused.
    let Shared { surfaces, failed, .. } = s;
    failed.retain(|d| surfaces.iter().any(|x| x.display == *d));
    if s.conn.is_none() && s.surfaces.is_empty() && matches!(state(), KEYS_WL_ACTIVE | KEYS_WL_STARTING | KEYS_WL_NO_SEAT | KEYS_WL_NO_KEYBOARD) {
        set_state(KEYS_WL_STOPPED);
    } else if s.conn.is_none() && !s.surfaces.is_empty() && state() == KEYS_WL_ACTIVE {
        // Another display's surface remains: the next tick attaches there.
        set_state(KEYS_WL_STARTING);
    }
}

/// One tick of the keys thread, while the game presents: attach if a surface needs it,
/// dispatch our queue (only what's already queued), flush our requests, update the state.
/// Returns the chord presses since the last tick, in order.
pub fn service(now: Instant) -> Vec<Action> {
    if EXITING.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut s = shared();
    if EXITING.load(Ordering::Relaxed) {
        return Vec::new();
    }
    if s.conn.is_none() {
        let Some(d) = s.surfaces.iter().map(|x| x.display).find(|d| !s.failed.contains(d)) else { return Vec::new() };
        // SAFETY: `d` belongs to a live surface (destroying it needs SHARED, which we hold).
        match unsafe { Conn::attach(d as *mut c_void, now) } {
            Ok(c) => {
                log!("keys: Wayland: private queue on the game's display {d:#x}");
                register_atexit();
                s.conn = Some(c);
                set_state(KEYS_WL_STARTING);
            }
            Err(why) => {
                s.failed.push(d);
                set_state(why);
                return Vec::new();
            }
        }
    }
    let c = s.conn.as_mut().expect("attached");
    // SAFETY: our queue on a live display; only our listeners run, on this thread.
    let (ok, presses, broken) = unsafe {
        IN_DISPATCH.with(|d| d.set(true));
        let r = (c.lib.dispatch_queue_pending)(c.display, c.queue);
        IN_DISPATCH.with(|d| d.set(false));
        let ok = r >= 0 && (c.lib.get_error)(c.display) == 0;
        let l = &mut *c.listen;
        let presses = std::mem::take(&mut l.presses);
        if ok && l.flush && ((c.lib.flush)(c.display) >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN)) {
            l.flush = false;
        }
        (ok, presses, l.broken || MISDISPATCHED.swap(false, Ordering::Relaxed))
    };
    if !ok || broken {
        let c = s.conn.take().expect("attached");
        s.failed.push(c.display as usize);
        // SAFETY: still alive (its surface is); after a fatal error libwayland makes the
        // release requests no-ops and only frees our proxies.
        unsafe { c.teardown() };
        set_state(if ok { KEYS_WL_INTERNAL } else { KEYS_WL_DISPLAY_ERROR });
        return presses;
    }
    // SAFETY: as above.
    let l = unsafe { &*c.listen };
    let st = if !l.keyboard.is_null() {
        KEYS_WL_ACTIVE
    } else if now.saturating_duration_since(c.since) < SETUP_TIMEOUT {
        KEYS_WL_STARTING
    } else if l.seat.is_null() {
        KEYS_WL_NO_SEAT
    } else if l.caps.is_some() {
        KEYS_WL_NO_KEYBOARD
    } else {
        KEYS_WL_STARTING
    };
    set_state(st);
    presses
}

fn register_atexit() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // SAFETY: a plain C function; the layer stays mapped (-z nodelete).
        unsafe { libc::atexit(at_exit) };
    });
}

/// At exit the game may disconnect its display (in a handler that runs before or after
/// ours): stop dispatching and drop our [`Conn`] without touching libwayland. A `Conn` has
/// no `Drop`, so its objects simply leak with the process.
extern "C" fn at_exit() {
    EXITING.store(true, Ordering::Relaxed);
    // Waits (≤ 100 ms) for a dispatch in flight; dispatching never blocks, so it's brief.
    for _ in 0..100 {
        match SHARED.try_lock() {
            Ok(mut s) => {
                s.conn = None;
                return;
            }
            Err(TryLockError::Poisoned(p)) => {
                p.into_inner().conn = None;
                return;
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}
