//! The HUD keys end to end in the layer: a real Vulkan app (tests/harness, headless) with the
//! layer loaded and `DISPLAY` pointing at a private Xvfb; Shift+F3 and Ctrl+Shift+F3 typed
//! into Xvfb with XTEST while it presents; the layer's status block must count one of each
//! (what quiverd's ring thread turns into `hud toggle` / `hud next`).
//!
//! Opt-in like tests/vulkan.rs (needs a GPU with `VK_EXT_headless_surface`, and Xvfb):
//! `cargo test -p quiver-sight-layer --test vulkan_keys -- --ignored --nocapture`.

mod harness;
mod xvfb;

use harness::*;
use quiver_perf::layout::{KEYS_ACTIVE, LAYER_FOCUS_KNOWN, LAYER_FOCUSED, LAYER_WAYLAND_SURFACE, LAYER_X11_SURFACE};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::wrapper::ConnectionExt as _;
use quiver_perf::session::open_ring_reader;
use quiver_sight_layer::keys::{X11Keys, keysym};

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface and Xvfb; see the module docs"]
fn chords_reach_the_status_block() {
    let Some(x) = xvfb::Xvfb::start() else { return };
    let codes = {
        let k = X11Keys::open(Some(&x.name())).unwrap();
        [k.keycode(keysym::F3), k.keycode(keysym::SHIFT_L), k.keycode(keysym::CONTROL_L)]
    };
    let [f3, shift, ctrl] = codes;
    let kb = x.keyboard();
    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    // SAFETY: the only test in this binary; nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
        std::env::set_var("DISPLAY", &x.display);
        for (k, v) in layer_env(scratch.path(), None) {
            std::env::set_var(k, v);
        }
    }
    let status = std::cell::RefCell::new(None);
    // 60 fps for 3 s; each chord held for ~250 ms, so a 50 ms poll can't miss it.
    let frames = 180;
    run(Opts {
        frames,
        fps: 60.0,
        readback: false,
        on_frame: Some(Box::new(|f, _, _| match f {
            30 => {
                kb.key(shift, true);
                kb.key(f3, true);
            }
            45 => {
                kb.key(f3, false);
                kb.key(shift, false);
            }
            80 => {
                kb.key(ctrl, true);
                kb.key(shift, true);
                kb.key(f3, true);
            }
            95 => {
                kb.key(f3, false);
                kb.key(shift, false);
                kb.key(ctrl, false);
            }
            // Bare F3 does nothing.
            110 => kb.key(f3, true),
            125 => kb.key(f3, false),
            f if f == frames - 1 => *status.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()),
            _ => {}
        })),
        ..Opts::default()
    });
    let s = status.into_inner().expect("a status block");
    assert_eq!(s.keys, KEYS_ACTIVE);
    assert_eq!((s.key_toggles, s.key_nexts), (1, 1), "one Shift+F3, one Ctrl+Shift+F3");
    // Headless: neither an X11 nor a Wayland surface.
    assert_eq!(s.flags & (LAYER_X11_SURFACE | LAYER_WAYLAND_SURFACE), 0);

    // An XCB window on the Xvfb: the surface hook notes an X11 surface. Not every driver
    // presents to Xvfb (no DRI3); then this part is skipped.
    let status = std::cell::RefCell::new(None);
    // Whether the window is in front (SIGHT §14): a window manager's _NET_ACTIVE_WINDOW,
    // set by the test (Xvfb has no window manager); the atom exists before the layer looks.
    let (xc, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&x.name())).unwrap();
    let root = xc.setup().roots[screen].root;
    let net_active = xc.intern_atom(false, b"_NET_ACTIVE_WINDOW").unwrap().reply().unwrap().atom;
    let set_active = |w: u32| {
        xc.change_property32(PropMode::REPLACE, root, net_active, AtomEnum::WINDOW, &[w]).unwrap();
        xc.sync().unwrap();
    };
    let in_front = std::cell::RefCell::new(None);
    let windowed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(Opts {
            frames: 60,
            fps: 60.0,
            readback: false,
            surface: Surface::Window { title: "quiver-sight-keys-test".into() },
            extent: (320, 240),
            on_frame: Some(Box::new(|f, _, _| match f {
                // The test window is the root's only child (no window manager to reparent it).
                10 => set_active(xc.query_tree(root).unwrap().reply().unwrap().children.last().copied().unwrap_or(0)),
                30 => *in_front.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()),
                32 => set_active(0),
                59 => *status.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()),
                _ => {}
            })),
            ..Opts::default()
        })
    }));
    match (windowed, status.into_inner()) {
        (Ok(r), Some(s)) => {
            eprintln!("windowed on {}", r.device_name);
            assert_ne!(s.flags & LAYER_X11_SURFACE, 0, "the XCB surface was noted");
            assert_eq!(s.flags & LAYER_WAYLAND_SURFACE, 0);
            let f = in_front.into_inner().expect("a status block").flags;
            assert_eq!(f & (LAYER_FOCUS_KNOWN | LAYER_FOCUSED), LAYER_FOCUS_KNOWN | LAYER_FOCUSED, "the active window is the game's");
            assert_eq!(s.flags & (LAYER_FOCUS_KNOWN | LAYER_FOCUSED), LAYER_FOCUS_KNOWN, "no active window");
        }
        _ => eprintln!("skipping the windowed part: no device presents to Xvfb here"),
    }
}
