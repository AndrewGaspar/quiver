//! Whether the game's X11 window is in front (src/focus.rs) against a real X server: a
//! private Xvfb, windows made with x11rb, and `_NET_ACTIVE_WINDOW` / `_NET_WM_PID` set by the
//! test the way a window manager (Hyprland's XWayland WM) and Wine do. Checks Wine's layout
//! (the Vulkan surface on a client window inside the toplevel), the pid fallback, and that
//! destroyed windows are answers, not X errors (the layer's XCB connection never reaches
//! Xlib's process-wide error handler, which in a Wine game is Wine's). Skipped without Xvfb.

mod xvfb;

use quiver_sight_layer::focus::X11Focus;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, WindowClass};
use x11rb::wrapper::ConnectionExt as _;

#[test]
fn active_window_through_xvfb() {
    let Some(x) = xvfb::Xvfb::start() else { return };
    let (c, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&x.name())).unwrap();
    let root = c.setup().roots[screen].root;

    // No window manager has made the atom yet: the layer can't tell.
    assert!(X11Focus::open(Some(&x.name())).is_none(), "no _NET_ACTIVE_WINDOW atom: unknown");

    let atom = |n: &str| c.intern_atom(false, n.as_bytes()).unwrap().reply().unwrap().atom;
    let active = atom("_NET_ACTIVE_WINDOW");
    let pid_atom = atom("_NET_WM_PID");
    let win = |parent| {
        let w = c.generate_id().unwrap();
        c.create_window(0, w, parent, 0, 0, 64, 64, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new()).unwrap();
        w
    };
    // Wine: the toplevel ("whole window") and the client window Vulkan presents to.
    let top = win(root);
    let client = win(top);
    let other = win(root);
    let set_active = |w: u32| {
        c.change_property32(PropMode::REPLACE, root, active, AtomEnum::WINDOW, &[w]).unwrap();
        c.sync().unwrap();
    };

    let mut f = X11Focus::open(Some(&x.name())).expect("the layer's XCB connection");
    assert_eq!(f.root(), root);
    // The atom exists but the root has no value yet: unknown.
    assert_eq!(f.focused(&[client]), None);

    set_active(top);
    assert_eq!(f.focused(&[client]), Some(true), "the toplevel around the surface's window");
    set_active(client);
    assert_eq!(f.focused(&[client]), Some(true));
    set_active(other);
    assert_eq!(f.focused(&[client]), Some(false), "another window");
    set_active(0);
    assert_eq!(f.focused(&[client]), Some(false), "a Wayland window is focused");

    // No surface window known: the active window's _NET_WM_PID.
    set_active(other);
    assert_eq!(f.focused(&[]), Some(false));
    c.change_property32(PropMode::REPLACE, other, pid_atom, AtomEnum::CARDINAL, &[std::process::id()]).unwrap();
    c.sync().unwrap();
    assert_eq!(f.focused(&[]), Some(true), "Wine's _NET_WM_PID is ours");

    // Destroyed windows: the surface's (its ancestors can't be found) and the active one
    // (reading its _NET_WM_PID fails). Answers, no X error reaches anyone.
    c.destroy_window(client).unwrap();
    c.destroy_window(other).unwrap();
    c.sync().unwrap();
    let mut f = X11Focus::open(Some(&x.name())).unwrap();
    set_active(other);
    assert_eq!(f.focused(&[client]), Some(false));
    set_active(top);
    assert_eq!(f.focused(&[client]), Some(false), "the surface's window is gone");
    assert!(!f.broken());

    // The server going away is "unknown", not a crash.
    drop(x);
    assert_eq!(f.focused(&[client]), None);
    assert!(f.broken());
}

#[test]
fn no_server_is_none() {
    assert!(X11Focus::open(Some(c":4093")).is_none());
}
