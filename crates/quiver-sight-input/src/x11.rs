//! winex11's input, interposed at Xlib (design/SIGHT.md §14, "Input in the game").
//!
//! What Wine actually calls (GE-Proton11-12's `winex11.so` imports, and the Wine source of
//! GE-Proton10-34): every X event Wine handles comes off the queue in `process_events` through
//! **`XCheckIfEvent`** with its `filter_event` predicate (a mask of `QS_*` classes). It never
//! calls `XNextEvent`, `XIfEvent`, `XPending` or `XPeekEvent`; `XWindowEvent` (GraphicsExpose,
//! after a GDI copy) and `XCheckTypedWindowEvent` (SelectionNotify, PropertyNotify, for the
//! clipboard) never return input. XInput2 events arrive as `GenericEvent` cookies through the
//! same `XCheckIfEvent`; Wine reads their data with `XGetEventData`, which it resolves with
//! `dlsym` on its own libX11 handle (so it can't be interposed, and doesn't need to be: a
//! cookie nobody claims is freed by Xlib at the next read). `XWarpPointer` (`SetCursorPos`)
//! is imported directly. Proton's Wine already drops key and mouse events in
//! `process_events` while Steam's overlay is up (`__wine_steamclient_GameOverlayActivated`,
//! an event lsteamclient sets): the same idea, at the same place.
//!
//! So this interposes `XCheckIfEvent` (plus `XIfEvent` and `XNextEvent`, for anything else in
//! the process that reads X input that way, like a native game's SDL) and `XWarpPointer`:
//!
//! - Our predicate wraps Wine's. For an input event it decides ([`crate::filter`]) whether the
//!   game or the panel gets it; for the panel's it answers True, so Xlib takes it off the
//!   queue, and the wrapper forwards it and looks again. For the game's it asks Wine's
//!   predicate as usual. The verdict of the event the predicate accepted is kept, so what
//!   happens after the dequeue is exactly what the predicate decided.
//! - Classes: KeyPress/KeyRelease and ButtonPress/ButtonRelease by code (releases of what the
//!   game holds pass); MotionNotify, EnterNotify, LeaveNotify, and XInput2's motion, raw
//!   motion and touch events while capturing. XInput2 key and button presses and releases
//!   (device and raw: Wine's raw input mode, which every game that registers raw mouse input
//!   gets, sends `WM_INPUT` from `XI_RawButtonPress`/`Release`) are paired by count per class,
//!   since their codes are only in the cookie data, which a predicate can't read. Everything
//!   else passes: focus, configure, property, keymap, client messages.
//! - `XWarpPointer` is dropped while capturing: a game that re-centres the pointer every
//!   frame would otherwise drag the panel's cursor back.
//!
//! The panel's cursor follows the pointer (MotionNotify's position in the game's window,
//! forwarded as [`EV_POINTER`]); with the game's own cursor shown, they coincide.

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::OnceLock;

use quiver_perf::panel::{EV_POINTER, INPUT_X11, PanelEvent};

use crate::filter::{Input, Tracker, Verdict};
use crate::{link, real_symbol};

/// Xlib's `XEvent`: a union of 24 longs.
#[repr(C)]
pub struct XEvent {
    pub pad: [c_long; 24],
}

#[repr(C)]
struct XAny {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
}

/// XKeyEvent and XButtonEvent (the button in `keycode`'s place).
#[repr(C)]
struct XKey {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    root: c_ulong,
    subwindow: c_ulong,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    keycode: c_uint,
    same_screen: c_int,
}

#[repr(C)]
struct XCookie {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    extension: c_int,
    evtype: c_int,
    cookie: c_uint,
    data: *mut c_void,
}

const KEY_PRESS: c_int = 2;
const KEY_RELEASE: c_int = 3;
const BUTTON_PRESS: c_int = 4;
const BUTTON_RELEASE: c_int = 5;
const MOTION_NOTIFY: c_int = 6;
const ENTER_NOTIFY: c_int = 7;
const LEAVE_NOTIFY: c_int = 8;
const GENERIC_EVENT: c_int = 35;

// XInput2 event types (XI2.h).
const XI_KEY_PRESS: c_int = 2;
const XI_KEY_RELEASE: c_int = 3;
const XI_BUTTON_PRESS: c_int = 4;
const XI_BUTTON_RELEASE: c_int = 5;
const XI_RAW_KEY_PRESS: c_int = 13;
const XI_RAW_KEY_RELEASE: c_int = 14;
const XI_RAW_BUTTON_PRESS: c_int = 15;
const XI_RAW_BUTTON_RELEASE: c_int = 16;
const XI_TOUCH_END: c_int = 20;
const XI_RAW_TOUCH_END: c_int = 24;

/// Linux evdev button codes for X buttons 1, 2, 3, 8, 9.
pub fn x_button_code(button: u32) -> Option<u32> {
    Some(match button {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8 => 0x113, // BTN_SIDE
        9 => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

/// X wheel "buttons": 4 up, 5 down, 6 left, 7 right → (dx, dy) notches.
fn x_wheel(button: u32) -> Option<(f64, f64)> {
    Some(match button {
        4 => (0.0, -1.0),
        5 => (0.0, 1.0),
        6 => (-1.0, 0.0),
        7 => (1.0, 0.0),
        _ => return None,
    })
}

pub static TRACKER: Tracker = Tracker::new();

/// What an X event is for the decision. Wheel buttons are pointer events (no state).
fn classify(ev: &XEvent) -> Input {
    // SAFETY: every XEvent starts with XAnyEvent; the variants read are the ones its type says.
    let any = unsafe { &*(ev as *const XEvent).cast::<XAny>() };
    match any.type_ {
        KEY_PRESS | KEY_RELEASE => {
            let k = unsafe { &*(ev as *const XEvent).cast::<XKey>() };
            let code = k.keycode.saturating_sub(8);
            if any.type_ == KEY_PRESS { Input::Down(code) } else { Input::Up(code) }
        }
        BUTTON_PRESS | BUTTON_RELEASE => {
            let k = unsafe { &*(ev as *const XEvent).cast::<XKey>() };
            match x_button_code(k.keycode) {
                Some(c) if any.type_ == BUTTON_PRESS => Input::Down(c),
                Some(c) => Input::Up(c),
                // Wheels and unknown buttons: no held state; the panel's while capturing.
                None => Input::Pointer,
            }
        }
        MOTION_NOTIFY | ENTER_NOTIFY | LEAVE_NOTIFY => Input::Pointer,
        GENERIC_EVENT => {
            let c = unsafe { &*(ev as *const XEvent).cast::<XCookie>() };
            // Only XInput2 sends GenericEvents on Wine's displays (no Present, no XFixes
            // events there); when we know XI's opcode, other extensions pass.
            let xi = xi_opcode();
            if xi != 0 && c.extension != xi {
                return Input::Other;
            }
            match c.evtype {
                // Presses and releases by count per class (module docs).
                XI_KEY_PRESS => Input::CountDown(0),
                XI_KEY_RELEASE => Input::CountUp(0),
                XI_BUTTON_PRESS => Input::CountDown(1),
                XI_BUTTON_RELEASE => Input::CountUp(1),
                XI_RAW_KEY_PRESS => Input::CountDown(2),
                XI_RAW_KEY_RELEASE => Input::CountUp(2),
                XI_RAW_BUTTON_PRESS => Input::CountDown(3),
                XI_RAW_BUTTON_RELEASE => Input::CountUp(3),
                // Touch ends pass (a touch has no state Wine keeps across them).
                XI_TOUCH_END | XI_RAW_TOUCH_END => Input::Other,
                // Motion, enter/leave, raw motion, touch begin/update, gestures.
                6 | 7 | 8 | 17 | 18 | 19 | 22 | 23 | 27 | 28 | 30 | 31 => Input::Pointer,
                _ => Input::Other,
            }
        }
        _ => Input::Other,
    }
}

/// XInput2's major opcode on this server, when something told us (0: unknown).
fn xi_opcode() -> c_int {
    XI_OPCODE.load(std::sync::atomic::Ordering::Relaxed)
}

static XI_OPCODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Forwards a blocked event to the panel (keys and buttons by evdev code, pointer motion by
/// position, wheels as scroll notches). XInput2 events aren't forwarded: their data isn't
/// claimed (the core events carry the same input).
fn forward(ev: &XEvent, input: Input, capturing: bool) {
    if !capturing {
        return;
    }
    let t = link::now();
    // SAFETY: as in classify.
    let any = unsafe { &*(ev as *const XEvent).cast::<XAny>() };
    match any.type_ {
        KEY_PRESS | KEY_RELEASE => {
            let k = unsafe { &*(ev as *const XEvent).cast::<XKey>() };
            let code = k.keycode.saturating_sub(8);
            link::forward(&PanelEvent::key(t, code, any.type_ == KEY_PRESS, 0, k.state));
        }
        BUTTON_PRESS | BUTTON_RELEASE => {
            let k = unsafe { &*(ev as *const XEvent).cast::<XKey>() };
            if let Input::Down(c) | Input::Up(c) = input {
                link::forward(&PanelEvent::button(t, c, matches!(input, Input::Down(_))));
            } else if any.type_ == BUTTON_PRESS
                && let Some((dx, dy)) = x_wheel(k.keycode)
            {
                link::forward(&PanelEvent::scroll(t, dx, dy));
            }
        }
        MOTION_NOTIFY => {
            let k = unsafe { &*(ev as *const XEvent).cast::<XKey>() };
            link::forward(&PanelEvent { t, kind: EV_POINTER, flags: 0, code: 0, a: k.x, b: k.y });
        }
        _ => {}
    }
}

type Pred = unsafe extern "C" fn(*mut c_void, *mut XEvent, *mut c_char) -> c_int;

struct Ctx {
    pred: Option<Pred>,
    arg: *mut c_char,
    capturing: bool,
    /// The verdict and class of the last event the predicate accepted.
    last: (Verdict, Input),
}

unsafe extern "C" fn our_pred(d: *mut c_void, ev: *mut XEvent, arg: *mut c_char) -> c_int {
    // SAFETY: `arg` is our Ctx (the wrapper passes it), alive for the call.
    let ctx = unsafe { &mut *arg.cast::<Ctx>() };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: Xlib passes a valid event.
        let input = classify(unsafe { &*ev });
        let v = TRACKER.decide(input, ctx.capturing);
        (input, v)
    }));
    let (input, v) = r.unwrap_or((Input::Other, Verdict::Pass));
    if v == Verdict::Block {
        ctx.last = (v, input);
        return 1;
    }
    // SAFETY: Wine's predicate with Wine's argument, as Xlib would call it.
    let accept = match ctx.pred {
        Some(p) => unsafe { p(d, ev, ctx.arg) },
        None => 1,
    };
    if accept != 0 {
        ctx.last = (v, input);
    }
    accept
}

/// After an event came off the queue with `ctx.last`: record it, forward it if blocked.
/// Returns true when the game gets it.
fn taken(ev: &XEvent, ctx: &Ctx) -> bool {
    let (v, input) = ctx.last;
    if TRACKER.commit(input, v, ctx.capturing) {
        link::note_release();
    }
    if v == Verdict::Block {
        forward(ev, input, ctx.capturing);
        return false;
    }
    true
}

macro_rules! real {
    ($name:literal, $ty:ty) => {{
        static F: OnceLock<usize> = OnceLock::new();
        let p = *F.get_or_init(|| real_symbol(c"libX11.so.6", $name));
        // SAFETY: the real Xlib function of this name and type (0: not found).
        (p != 0).then(|| unsafe { std::mem::transmute::<usize, $ty>(p) })
    }};
}

type XCheckIfEventFn = unsafe extern "C" fn(*mut c_void, *mut XEvent, Option<Pred>, *mut c_char) -> c_int;
type XNextEventFn = unsafe extern "C" fn(*mut c_void, *mut XEvent) -> c_int;
type XWarpPointerFn = unsafe extern "C" fn(*mut c_void, c_ulong, c_ulong, c_int, c_int, c_uint, c_uint, c_int, c_int) -> c_int;
type XQueryExtensionFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_int, *mut c_int, *mut c_int) -> c_int;

/// Learns XInput2's opcode on Wine's display, once, the first time input is captured (one
/// round trip on Wine's thread, at most once per process; before that every GenericEvent is
/// treated as XInput2's, which on Wine's displays it is).
fn learn_xi(d: *mut c_void) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(q) = real!(c"XQueryExtension", XQueryExtensionFn) else { return };
        let (mut op, mut ev, mut err) = (0, 0, 0);
        // SAFETY: a valid display (Wine's, from the call we're in).
        if unsafe { q(d, c"XInputExtension".as_ptr(), &mut op, &mut ev, &mut err) } != 0 {
            XI_OPCODE.store(op, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

fn start(d: *mut c_void) -> bool {
    link::hooked(INPUT_X11);
    if link::off() {
        return false;
    }
    let capturing = link::capture_now().is_some();
    if capturing && !d.is_null() {
        learn_xi(d);
    }
    capturing
}

/// # Safety
/// Xlib's contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn XCheckIfEvent(d: *mut c_void, ev: *mut XEvent, pred: Option<Pred>, arg: *mut c_char) -> c_int {
    let Some(real) = real!(c"XCheckIfEvent", XCheckIfEventFn) else { return 0 };
    let capturing = std::panic::catch_unwind(|| start(d)).unwrap_or(false);
    if link::off() {
        // SAFETY: passing the caller's arguments through.
        return unsafe { real(d, ev, pred, arg) };
    }
    let mut ctx = Ctx { pred, arg, capturing, last: (Verdict::Pass, Input::Other) };
    loop {
        ctx.last = (Verdict::Pass, Input::Other);
        // SAFETY: our predicate with our context; the caller's display and event buffer.
        let r = unsafe { real(d, ev, Some(our_pred), (&mut ctx as *mut Ctx).cast()) };
        if r == 0 {
            return 0;
        }
        // SAFETY: Xlib filled `ev`.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| taken(unsafe { &*ev }, &ctx))).unwrap_or(true) {
            return r;
        }
    }
}

/// # Safety
/// Xlib's contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn XIfEvent(d: *mut c_void, ev: *mut XEvent, pred: Option<Pred>, arg: *mut c_char) -> c_int {
    let Some(real) = real!(c"XIfEvent", XCheckIfEventFn) else { return 0 };
    let capturing = std::panic::catch_unwind(|| start(d)).unwrap_or(false);
    if link::off() {
        // SAFETY: passing through.
        return unsafe { real(d, ev, pred, arg) };
    }
    let mut ctx = Ctx { pred, arg, capturing, last: (Verdict::Pass, Input::Other) };
    loop {
        ctx.last = (Verdict::Pass, Input::Other);
        // SAFETY: as XCheckIfEvent (this one blocks until an event matches, as Xlib's does).
        let r = unsafe { real(d, ev, Some(our_pred), (&mut ctx as *mut Ctx).cast()) };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| taken(unsafe { &*ev }, &ctx))).unwrap_or(true) {
            return r;
        }
    }
}

/// # Safety
/// Xlib's contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn XNextEvent(d: *mut c_void, ev: *mut XEvent) -> c_int {
    let Some(real) = real!(c"XNextEvent", XNextEventFn) else { return 0 };
    let capturing = std::panic::catch_unwind(|| start(d)).unwrap_or(false);
    loop {
        // SAFETY: passing the caller's arguments through.
        let r = unsafe { real(d, ev) };
        if link::off() {
            return r;
        }
        let keep = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: Xlib filled `ev`.
            let e = unsafe { &*ev };
            let input = classify(e);
            let v = TRACKER.decide(input, capturing);
            taken(e, &Ctx { pred: None, arg: std::ptr::null_mut(), capturing, last: (v, input) })
        }))
        .unwrap_or(true);
        if keep {
            return r;
        }
    }
}

/// # Safety
/// Xlib's contract.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn XWarpPointer(d: *mut c_void, src: c_ulong, dst: c_ulong, sx: c_int, sy: c_int, sw: c_uint, sh: c_uint, dx: c_int, dy: c_int) -> c_int {
    let Some(real) = real!(c"XWarpPointer", XWarpPointerFn) else { return 0 };
    if !link::off() && std::panic::catch_unwind(|| start(d)).unwrap_or(false) {
        // The panel has the pointer: the game's re-centring would fight the user.
        return 1;
    }
    // SAFETY: passing through.
    unsafe { real(d, src, dst, sx, sy, sw, sh, dx, dy) }
}
