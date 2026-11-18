//! The in-game HUD keys against a real X server: a private Xvfb, key presses injected with
//! XTEST, read back through the layer's own `dlopen`ed libX11 (`XQueryKeymap`,
//! `XKeysymToKeycode`) and resolved to chords. Skipped when Xvfb isn't installed.

mod xvfb;

use std::time::Duration;

use quiver_sight_layer::keys::{Action, Chords, DEFAULT_BINDINGS, X11Keys, keysym};

#[test]
fn chords_through_xvfb() {
    let Some(x) = xvfb::Xvfb::start() else { return };
    let keys = X11Keys::open(Some(&x.name())).expect("the layer's X connection");
    let codes = keys.keycodes(&DEFAULT_BINDINGS).expect("F3, Shift and Ctrl in Xvfb's keymap");
    let f3 = keys.keycode(keysym::F3);
    let shift = keys.keycode(keysym::SHIFT_L);
    let ctrl = keys.keycode(keysym::CONTROL_R);
    assert!(f3 != 0 && shift != 0 && ctrl != 0);

    let kb = x.keyboard();
    let key = |code, down| kb.key(code, down);
    let mut chords = Chords::new(codes);
    let mut poll = || {
        std::thread::sleep(Duration::from_millis(5));
        chords.step(&keys.keymap().expect("XQueryKeymap"))
    };

    assert_eq!(poll(), None, "nothing held");
    key(shift, true);
    assert_eq!(poll(), None, "Shift alone");
    key(f3, true);
    assert_eq!(poll(), Some(Action::Toggle), "Shift+F3");
    assert_eq!(poll(), None, "held: no repeat");
    key(f3, false);
    key(ctrl, true);
    assert_eq!(poll(), None);
    key(f3, true);
    assert_eq!(poll(), Some(Action::Next), "Ctrl+Shift+F3 is next, not toggle");
    key(f3, false);
    key(ctrl, false);
    key(shift, false);
    assert_eq!(poll(), None);
    key(f3, true);
    assert_eq!(poll(), None, "bare F3");
    key(f3, false);
}

#[test]
fn no_server_is_an_error_not_a_crash() {
    // A display nobody serves: XOpenDisplay fails, and the layer reports it.
    let r = X11Keys::open(Some(c":4093"));
    assert_eq!(r.err(), Some(quiver_perf::layout::KEYS_OPEN_FAILED));
}
