//! winewayland's input, interposed at libwayland-client (design/SIGHT.md §14, "Input in the
//! game").
//!
//! What Wine calls (GE-Proton11-12's `winewayland.so` imports; the Wine source of
//! GE-Proton10-34, `dlls/winewayland.drv/wayland_keyboard.c`, `wayland_pointer.c`): its input
//! arrives through listeners it registers with `wl_keyboard_add_listener`,
//! `wl_pointer_add_listener` and `zwp_relative_pointer_v1_add_listener`, inline wrappers of
//! **`wl_proxy_add_listener`**, which it imports from libwayland-client. Its handlers turn
//! `key`, `button`, `motion`, `axis_discrete` and `relative_motion` into
//! `NtUserSendHardwareInput`; `enter`/`leave` track focus (`leave` releases every key);
//! `modifiers` only updates its xkb state.
//!
//! So this interposes `wl_proxy_add_listener`: for those three interfaces it installs a
//! *dispatcher* instead (`wl_proxy_add_dispatcher`, with Wine's listener as the dispatcher's
//! data and Wine's user data as the proxy's, so `wl_proxy_get_user_data` and
//! `wl_proxy_get_listener` answer as before) and calls Wine's handlers itself, or doesn't:
//!
//! - `wl_keyboard.key`, `wl_pointer.button`: by code ([`crate::filter`]): presses go to the
//!   panel while capturing, releases of what the game holds go to the game;
//! - `wl_pointer.motion`, the `axis*` events, `zwp_relative_pointer_v1.relative_motion`: the
//!   panel's while capturing;
//! - `enter`, `leave`, `keymap`, `modifiers`, `repeat_info`, `frame`: always Wine's (focus
//!   stays Wine's business; `modifiers` only feeds its xkb state).
//!
//! Every other proxy's listener goes through untouched (the Sight layer's own `wl_keyboard`
//! resolves libwayland through its own handle, so it never comes here). The Wayland cursor
//! isn't touched: with the pointer locked the game has hidden it, and otherwise it sits where
//! the panel's cursor follows the pointer's motion.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use quiver_perf::panel::{EV_RESET, EVF_WAYLAND, INPUT_WAYLAND, PanelEvent};

use crate::filter::{Input, Tracker, Verdict};
use crate::{link, real_symbol};

/// `union wl_argument`: pointer-sized.
#[repr(C)]
#[derive(Clone, Copy)]
pub union Arg {
    pub i: i32,
    pub u: u32,
    pub p: *mut c_void,
}

type Dispatcher = unsafe extern "C" fn(*const c_void, *mut c_void, u32, *const c_void, *mut Arg) -> c_int;
type AddListenerFn = unsafe extern "C" fn(*mut c_void, *const *const c_void, *mut c_void) -> c_int;
type AddDispatcherFn = unsafe extern "C" fn(*mut c_void, Dispatcher, *const c_void, *mut c_void) -> c_int;
type GetUserDataFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type GetClassFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;

macro_rules! real {
    ($name:literal, $ty:ty) => {{
        static F: OnceLock<usize> = OnceLock::new();
        let p = *F.get_or_init(|| real_symbol(c"libwayland-client.so.0", $name));
        // SAFETY: the real libwayland-client function of this name and type (0: not found).
        (p != 0).then(|| unsafe { std::mem::transmute::<usize, $ty>(p) })
    }};
}

pub static TRACKER: Tracker = Tracker::new();

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Iface {
    Keyboard,
    Pointer,
    Relative,
}

/// Which wrapped interface a class name is.
pub fn iface(name: &[u8]) -> Option<Iface> {
    match name {
        b"wl_keyboard" => Some(Iface::Keyboard),
        b"wl_pointer" => Some(Iface::Pointer),
        b"zwp_relative_pointer_v1" => Some(Iface::Relative),
        _ => None,
    }
}

/// Events per interface the dispatchers know (wl_keyboard v10, wl_pointer v10,
/// zwp_relative_pointer_v1 v1). A later event is passed only if... it can't be: its
/// signature is unknown; it's dropped (logged once).
const KEYBOARD_EVENTS: u32 = 6;
const POINTER_EVENTS: u32 = 11;

/// What an event is (opcode, the key/button code and state where it has one).
pub fn classify(i: Iface, opcode: u32, args: &[Arg]) -> Input {
    // SAFETY (args): the dispatcher passes the event's arguments; only indexes the event's
    // signature has are read.
    unsafe {
        match (i, opcode) {
            // key(serial, time, key, state): state 0 released, 1 pressed, 2 repeated.
            (Iface::Keyboard, 3) => {
                if args[3].u == 0 {
                    Input::Up(args[2].u)
                } else {
                    Input::Down(args[2].u)
                }
            }
            // leave: Wine releases everything.
            (Iface::Keyboard, 2) => Input::Reset,
            // button(serial, time, button, state).
            (Iface::Pointer, 3) => {
                if args[3].u == 0 {
                    Input::Up(args[2].u)
                } else {
                    Input::Down(args[2].u)
                }
            }
            // motion, axis, axis_source, axis_stop, axis_discrete, axis_value120,
            // axis_relative_direction.
            (Iface::Pointer, 2 | 4 | 6 | 7 | 8 | 9 | 10) => Input::Pointer,
            (Iface::Relative, 0) => Input::Pointer,
            _ => Input::Other,
        }
    }
}

/// The event as the panel gets it.
fn forward(i: Iface, opcode: u32, args: &[Arg], input: Input) {
    let t = link::now();
    let fx = |a: Arg| unsafe { a.i } as f64 / 256.0;
    // SAFETY (args): as in classify.
    unsafe {
        let e = match (i, opcode, input) {
            (Iface::Keyboard, 3, Input::Down(c) | Input::Up(c)) => {
                let mut e = PanelEvent::key(t, c, matches!(input, Input::Down(_)), 0, 0);
                e.flags |= EVF_WAYLAND | if args[3].u == 2 { quiver_perf::panel::EVF_REPEAT } else { 0 };
                e
            }
            (Iface::Keyboard, 2, _) => PanelEvent { t, kind: EV_RESET, flags: EVF_WAYLAND, ..Default::default() },
            (Iface::Pointer, 3, Input::Down(c) | Input::Up(c)) => {
                let mut e = PanelEvent::button(t, c, matches!(input, Input::Down(_)));
                e.flags |= EVF_WAYLAND;
                e
            }
            // motion(time, sx, sy): surface coordinates; the panel uses the deltas.
            (Iface::Pointer, 2, _) => PanelEvent { t, kind: quiver_perf::panel::EV_POINTER, flags: EVF_WAYLAND, code: 0, a: (fx(args[1]) * 256.0) as i32, b: (fx(args[2]) * 256.0 ) as i32 },
            // axis_discrete(axis, discrete): 0 vertical, 1 horizontal.
            (Iface::Pointer, 8, _) => {
                let n = args[1].i as f64;
                if args[0].u == 0 { PanelEvent::scroll(t, 0.0, n) } else { PanelEvent::scroll(t, n, 0.0) }
            }
            // axis_value120(axis, value120).
            (Iface::Pointer, 9, _) => {
                let n = args[1].i as f64 / 120.0;
                if args[0].u == 0 { PanelEvent::scroll(t, 0.0, n) } else { PanelEvent::scroll(t, n, 0.0) }
            }
            // relative_motion(utime_hi, utime_lo, dx, dy, dx_unaccel, dy_unaccel).
            (Iface::Relative, 0, _) => PanelEvent::motion(t, fx(args[2]), fx(args[3])),
            _ => return,
        };
        link::forward(&e);
    }
}

/// Calls Wine's listener entry `opcode` with the event's arguments.
///
/// # Safety
/// `listener` is the listener Wine registered for a proxy of interface `i`, `data` its user
/// data, `args` the event's arguments.
unsafe fn call(i: Iface, listener: *const *const c_void, data: *mut c_void, proxy: *mut c_void, opcode: u32, a: &[Arg]) {
    // SAFETY: the listener has an entry per event the proxy's version can receive, and the
    // compositor only sends those; NULL entries are skipped (libwayland would crash on them).
    let f = unsafe { *listener.add(opcode as usize) };
    if f.is_null() {
        return;
    }
    type P = *mut c_void;
    // SAFETY: each event's C signature (wayland.xml, relative-pointer-unstable-v1.xml); the
    // arguments' union members are the ones the signature's types say.
    unsafe {
        match (i, opcode) {
            // keymap(format u, fd h, size u)
            (Iface::Keyboard, 0) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, i32, u32)>(f)(data, proxy, a[0].u, a[1].i, a[2].u),
            // enter(serial u, surface o, keys a)
            (Iface::Keyboard, 1) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, P, P)>(f)(data, proxy, a[0].u, a[1].p, a[2].p),
            // leave(serial u, surface o)
            (Iface::Keyboard, 2) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, P)>(f)(data, proxy, a[0].u, a[1].p),
            // key(serial, time, key, state)
            (Iface::Keyboard, 3) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, u32, u32, u32)>(f)(data, proxy, a[0].u, a[1].u, a[2].u, a[3].u),
            // modifiers(serial, depressed, latched, locked, group)
            (Iface::Keyboard, 4) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, u32, u32, u32, u32)>(f)(data, proxy, a[0].u, a[1].u, a[2].u, a[3].u, a[4].u),
            // repeat_info(rate i, delay i)
            (Iface::Keyboard, 5) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, i32, i32)>(f)(data, proxy, a[0].i, a[1].i),
            // enter(serial u, surface o, sx f, sy f)
            (Iface::Pointer, 0) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, P, i32, i32)>(f)(data, proxy, a[0].u, a[1].p, a[2].i, a[3].i),
            // leave(serial u, surface o)
            (Iface::Pointer, 1) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, P)>(f)(data, proxy, a[0].u, a[1].p),
            // motion(time u, sx f, sy f)
            (Iface::Pointer, 2) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, i32, i32)>(f)(data, proxy, a[0].u, a[1].i, a[2].i),
            // button(serial, time, button, state)
            (Iface::Pointer, 3) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, u32, u32, u32)>(f)(data, proxy, a[0].u, a[1].u, a[2].u, a[3].u),
            // axis(time u, axis u, value f)
            (Iface::Pointer, 4) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, u32, i32)>(f)(data, proxy, a[0].u, a[1].u, a[2].i),
            // frame()
            (Iface::Pointer, 5) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P)>(f)(data, proxy),
            // axis_source(source u)
            (Iface::Pointer, 6) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32)>(f)(data, proxy, a[0].u),
            // axis_stop(time u, axis u)
            (Iface::Pointer, 7) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, u32)>(f)(data, proxy, a[0].u, a[1].u),
            // axis_discrete(axis u, discrete i), axis_value120(axis u, value120 i),
            // axis_relative_direction(axis u, direction u)
            (Iface::Pointer, 8..=10) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, i32)>(f)(data, proxy, a[0].u, a[1].i),
            // relative_motion(utime_hi u, utime_lo u, dx f, dy f, dx_unaccel f, dy_unaccel f)
            (Iface::Relative, 0) => std::mem::transmute::<*const c_void, unsafe extern "C" fn(P, P, u32, u32, i32, i32, i32, i32)>(f)(data, proxy, a[0].u, a[1].u, a[2].i, a[3].i, a[4].i, a[5].i),
            _ => {}
        }
    }
}

/// Arguments per event (enough to slice the args array safely).
fn nargs(i: Iface, opcode: u32) -> usize {
    match (i, opcode) {
        (Iface::Keyboard, 0) => 3,
        (Iface::Keyboard, 1) => 3,
        (Iface::Keyboard, 2) => 2,
        (Iface::Keyboard, 3) => 4,
        (Iface::Keyboard, 4) => 5,
        (Iface::Keyboard, 5) => 2,
        (Iface::Pointer, 0) => 4,
        (Iface::Pointer, 1) => 2,
        (Iface::Pointer, 2) => 3,
        (Iface::Pointer, 3) => 4,
        (Iface::Pointer, 4) => 3,
        (Iface::Pointer, 5) => 0,
        (Iface::Pointer, 6) => 1,
        (Iface::Pointer, 7) => 2,
        (Iface::Pointer, 8..=10) => 2,
        (Iface::Relative, 0) => 6,
        _ => 0,
    }
}

fn known(i: Iface, opcode: u32) -> bool {
    match i {
        Iface::Keyboard => opcode < KEYBOARD_EVENTS,
        Iface::Pointer => opcode < POINTER_EVENTS,
        Iface::Relative => opcode == 0,
    }
}

/// The dispatcher installed on Wine's input proxies.
unsafe extern "C" fn dispatch(listener: *const c_void, proxy: *mut c_void, opcode: u32, _msg: *const c_void, args: *mut Arg) -> c_int {
    let Some(get_class) = real!(c"wl_proxy_get_class", GetClassFn) else { return 0 };
    let Some(get_data) = real!(c"wl_proxy_get_user_data", GetUserDataFn) else { return 0 };
    // SAFETY: libwayland passes the proxy it dispatches for.
    let class = unsafe { CStr::from_ptr(get_class(proxy)) };
    let Some(i) = iface(class.to_bytes()) else { return 0 };
    if !known(i, opcode) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::log!("{} event {opcode} isn't one this build knows: dropped", class.to_string_lossy());
        }
        return 0;
    }
    let n = nargs(i, opcode);
    // SAFETY: libwayland's argument array for this event has its signature's count.
    let a: &[Arg] = if n == 0 || args.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(args, n) } };
    let decided = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        link::hooked(INPUT_WAYLAND);
        let capturing = !link::off() && link::capture_now().is_some();
        let input = classify(i, opcode, a);
        let v = TRACKER.decide(input, capturing);
        if TRACKER.commit(input, v, capturing) {
            link::note_release();
        }
        if capturing && (v == Verdict::Block || input == Input::Reset) {
            forward(i, opcode, a, input);
        }
        v
    }))
    .unwrap_or(Verdict::Pass);
    if decided == Verdict::Pass {
        // SAFETY: Wine's listener and user data for this proxy, with the event's arguments.
        unsafe { call(i, listener.cast(), get_data(proxy), proxy, opcode, a) };
    }
    0
}

/// # Safety
/// libwayland-client's contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wl_proxy_add_listener(proxy: *mut c_void, listener: *const *const c_void, data: *mut c_void) -> c_int {
    let Some(real) = real!(c"wl_proxy_add_listener", AddListenerFn) else { return -1 };
    let wrap = !link::off()
        && !proxy.is_null()
        && !listener.is_null()
        && std::panic::catch_unwind(|| {
            let get_class = real!(c"wl_proxy_get_class", GetClassFn)?;
            // SAFETY: a live proxy (the caller's).
            let class = unsafe { CStr::from_ptr(get_class(proxy)) };
            iface(class.to_bytes())
        })
        .ok()
        .flatten()
        .is_some();
    if wrap
        && std::env::var_os(link::ENV_INPUT).is_none_or(|v| v != "0")
        && let Some(add) = real!(c"wl_proxy_add_dispatcher", AddDispatcherFn)
    {
        link::hooked(INPUT_WAYLAND);
        // SAFETY: Wine's listener becomes the dispatcher's data; its user data the proxy's.
        return unsafe { add(proxy, dispatch, listener.cast(), data) };
    }
    // SAFETY: passing through.
    unsafe { real(proxy, listener, data) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_codes() {
        assert_eq!(iface(b"wl_keyboard"), Some(Iface::Keyboard));
        assert_eq!(iface(b"wl_pointer"), Some(Iface::Pointer));
        assert_eq!(iface(b"zwp_relative_pointer_v1"), Some(Iface::Relative));
        assert_eq!(iface(b"wl_seat"), None);
        let a = |v: [u32; 4]| v.map(|u| Arg { u });
        assert_eq!(classify(Iface::Keyboard, 3, &a([1, 2, 30, 1])), Input::Down(30));
        assert_eq!(classify(Iface::Keyboard, 3, &a([1, 2, 30, 2])), Input::Down(30), "repeated");
        assert_eq!(classify(Iface::Keyboard, 3, &a([1, 2, 30, 0])), Input::Up(30));
        assert_eq!(classify(Iface::Keyboard, 2, &a([1, 0, 0, 0])), Input::Reset);
        assert_eq!(classify(Iface::Keyboard, 4, &a([1, 0, 0, 0])), Input::Other, "modifiers are Wine's");
        assert_eq!(classify(Iface::Pointer, 3, &a([1, 2, 0x110, 1])), Input::Down(0x110));
        assert_eq!(classify(Iface::Pointer, 0, &a([1, 0, 0, 0])), Input::Other, "enter is Wine's");
        assert_eq!(classify(Iface::Pointer, 5, &[]), Input::Other, "frame is Wine's");
        assert_eq!(classify(Iface::Pointer, 2, &a([1, 0, 0, 0])), Input::Pointer);
        assert_eq!(classify(Iface::Relative, 0, &a([0, 0, 0, 0])), Input::Pointer);
        assert!(known(Iface::Pointer, 10) && !known(Iface::Pointer, 11) && !known(Iface::Relative, 1));
    }
}
