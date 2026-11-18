//! The layer's Wayland keyboard (src/wayland.rs) on a real libwayland connection: a fake
//! compositor and a fake game in this process (tests/wayland), the game reading its socket
//! on its own thread the way Wine does, and the layer's keyboard added to the game's display
//! exactly as the `vkCreateWaylandSurfaceKHR` hook does. Checks that:
//!
//! - the layer binds its own seat and keyboard without any roundtrip, and the keyboard gets
//!   `enter` when the game already has focus;
//! - Shift+F3 / Ctrl+Shift+F3 are read with the same chord rules as X11, and a chord held
//!   when focus arrives is no press;
//! - the layer's events wait on its private queue until its own tick dispatches them (the
//!   layer's listeners refuse to run outside its dispatch and the state would turn
//!   `internal`: removing the `wl_proxy_set_queue` makes this test fail), the game's keyboard
//!   still gets every key, and the game's listeners only ever run on its reader thread;
//! - `vkDestroySurfaceKHR` / `vkDestroyInstance` release only the layer's keyboard and seat
//!   (the compositor sees `release`s), after which the game keeps getting keys;
//! - no seat, no keyboard and a broken connection are reported (`KEYS_WL_*`);
//! - every keymap fd the compositor sent was closed.
//!
//! One test: the layer's state is process-wide. Needs libwayland-server.so.0 and
//! libwayland-client.so.0 (skipped otherwise); no compositor, GPU or display.

mod wayland;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use quiver_perf::layout::{KEYS_WL_ACTIVE, KEYS_WL_DISPLAY_ERROR, KEYS_WL_NO_KEYBOARD, KEYS_WL_NO_SEAT, KEYS_WL_NONE, KEYS_WL_STARTING, KEYS_WL_STOPPED};
use quiver_sight_layer::keys::Action;
use quiver_sight_layer::keys::evdev::{KEY_F3, KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_RIGHTSHIFT};
use quiver_sight_layer::wayland as layer;
use wayland::{Cmd, Game, Server, ServerOpts, keymap_fds_open, socketpair, wait_for};

/// Ticks the layer (as its keys thread does while the game presents) until its state is
/// `want`; returns the presses seen on the way.
fn tick_until(want: u32) -> Vec<Action> {
    let mut presses = Vec::new();
    let ok = wait_for(3.0, || {
        presses.extend(layer::service(Instant::now()));
        layer::state() == want
    });
    assert!(ok, "state {} (wanted {want})", layer::state());
    presses
}

/// The presses of the keys just sent: ticks until `n` came (the layer's events wait on its
/// queue until a tick dispatches them; usually one tick gets them all), then once more to
/// check nothing else does.
fn presses(n: usize) -> Vec<Action> {
    let mut got = Vec::new();
    wait_for(2.0, || {
        got.extend(layer::service(Instant::now()));
        got.len() >= n
    });
    std::thread::sleep(Duration::from_millis(30));
    got.extend(layer::service(Instant::now()));
    got
}

fn pair(opts: ServerOpts) -> Option<(Server, Game)> {
    let (s, c) = socketpair();
    let server = Server::start(s, opts)?;
    let game = Game::connect(c)?;
    Some((server, game))
}

fn close(server: Server, game: Game) {
    server.send(Cmd::Kill);
    server.sync();
    game.disconnect();
    drop(server);
}

#[test]
fn keys_on_the_games_own_wayland_connection() {
    let Some((server, game)) = pair(ServerOpts { seat: true, keyboard_cap: true }) else {
        eprintln!("skipping: libwayland-server.so.0 / libwayland-client.so.0 not loadable");
        return;
    };
    let st = &server.stats;

    // The game's window already has focus when the layer starts.
    server.send(Cmd::Focus(vec![]));
    server.sync();
    assert!(wait_for(2.0, || game.seen.enters.load(Ordering::Relaxed) == 1));
    assert_eq!(layer::state(), KEYS_WL_NONE);
    assert!(layer::service(Instant::now()).is_empty(), "no surface: nothing to do");

    // vkCreateWaylandSurfaceKHR: noted; the next ticks bind seat and keyboard.
    layer::surface_created(1, 10, game.display);
    assert_eq!(layer::state(), KEYS_WL_STARTING);
    assert!(tick_until(KEYS_WL_ACTIVE).is_empty());
    assert!(wait_for(2.0, || st.keyboards_live.load(Ordering::Relaxed) == 2), "the game's keyboard and ours");
    assert_eq!(st.seats_bound.load(Ordering::Relaxed), 2);
    // The late keyboard got its enter: a chord works at once.
    server.keys(&[(KEY_LEFTSHIFT, true), (KEY_F3, true), (KEY_F3, false), (KEY_LEFTSHIFT, false)]);
    assert!(wait_for(2.0, || game.seen.keys.load(Ordering::Relaxed) == 4));
    assert_eq!(presses(1), [Action::Toggle]);

    // A burst between two ticks: the game's keyboard sees every key as it arrives; ours waits
    // on the layer's queue until the next tick, which gets the chords in order. Bare F3 and
    // a chord held across Ctrl being added are nothing.
    server.keys(&[
        (KEY_LEFTCTRL, true),
        (KEY_LEFTSHIFT, true),
        (KEY_F3, true),
        (KEY_F3, false),
        (KEY_LEFTSHIFT, false),
        (KEY_LEFTCTRL, false),
        (KEY_F3, true),
        (KEY_F3, false),
        (KEY_RIGHTSHIFT, true),
        (KEY_F3, true),
        (KEY_LEFTCTRL, true),
        (KEY_F3, false),
        (KEY_LEFTCTRL, false),
        (KEY_RIGHTSHIFT, false),
    ]);
    assert!(wait_for(2.0, || game.seen.keys.load(Ordering::Relaxed) == 18));
    assert_eq!(presses(2), [Action::Next, Action::Toggle]);

    // Focus leaves; it comes back with Shift+F3 held (the alt-tab chord): no press. F3 again
    // with Shift still held: a press.
    server.send(Cmd::Leave);
    server.send(Cmd::Focus(vec![KEY_LEFTSHIFT, KEY_F3]));
    server.keys(&[(KEY_F3, false), (KEY_F3, true), (KEY_F3, false), (KEY_LEFTSHIFT, false)]);
    assert!(wait_for(2.0, || game.seen.keys.load(Ordering::Relaxed) == 22));
    assert_eq!(presses(1), [Action::Toggle]);
    assert_eq!(game.seen.enters.load(Ordering::Relaxed), 2);

    // vkDestroySurfaceKHR of the last surface: our keyboard and seat are released at once;
    // the game's stay and keep getting keys.
    layer::surface_destroyed(1, 10);
    assert_eq!(layer::state(), KEYS_WL_STOPPED);
    server.sync();
    assert!(wait_for(2.0, || st.keyboards_live.load(Ordering::Relaxed) == 1 && st.seats_live.load(Ordering::Relaxed) == 1));
    assert_eq!((st.keyboards_released.load(Ordering::Relaxed), st.seats_released.load(Ordering::Relaxed)), (1, 1));
    server.keys(&[(KEY_LEFTSHIFT, true), (KEY_F3, true), (KEY_F3, false), (KEY_LEFTSHIFT, false)]);
    assert!(wait_for(2.0, || game.seen.keys.load(Ordering::Relaxed) == 26));
    assert!(layer::service(Instant::now()).is_empty());

    // A new surface attaches again; vkDestroyInstance with it still alive releases too.
    layer::surface_created(1, 11, game.display);
    assert!(tick_until(KEYS_WL_ACTIVE).is_empty());
    assert!(wait_for(2.0, || st.keyboards_live.load(Ordering::Relaxed) == 2));
    layer::instance_destroyed(1);
    assert_eq!(layer::state(), KEYS_WL_STOPPED);
    assert!(wait_for(2.0, || st.keyboards_live.load(Ordering::Relaxed) == 1 && st.seats_live.load(Ordering::Relaxed) == 1));
    assert_eq!(st.keyboards_released.load(Ordering::Relaxed), 2);

    assert!(!game.seen.wrong_thread.load(Ordering::Relaxed), "a game listener ran off its reader thread");
    assert!(game.seen.keymaps.load(Ordering::Relaxed) >= 1);
    close(server, game);
    assert_eq!(keymap_fds_open(), 0, "every keymap fd was closed");

    // A seat without a keyboard: the seat is bound, and after the setup timeout the state
    // says why.
    let (server, game) = pair(ServerOpts { seat: true, keyboard_cap: false }).unwrap();
    layer::surface_created(2, 20, game.display);
    assert!(wait_for(2.0, || {
        layer::service(Instant::now());
        server.stats.seats_bound.load(Ordering::Relaxed) == 2
    }));
    std::thread::sleep(Duration::from_millis(50));
    layer::service(Instant::now());
    assert_eq!(layer::state(), KEYS_WL_STARTING);
    layer::service(Instant::now() + layer::SETUP_TIMEOUT + Duration::from_secs(1));
    assert_eq!(layer::state(), KEYS_WL_NO_KEYBOARD);
    layer::surface_destroyed(2, 20);
    assert_eq!(layer::state(), KEYS_WL_STOPPED);
    assert!(wait_for(2.0, || server.stats.seats_live.load(Ordering::Relaxed) == 1));
    close(server, game);

    // No seat at all.
    let (server, game) = pair(ServerOpts { seat: false, keyboard_cap: false }).unwrap();
    layer::surface_created(3, 30, game.display);
    for _ in 0..10 {
        layer::service(Instant::now());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(layer::state(), KEYS_WL_STARTING);
    layer::service(Instant::now() + layer::SETUP_TIMEOUT + Duration::from_secs(1));
    assert_eq!(layer::state(), KEYS_WL_NO_SEAT);
    layer::surface_destroyed(3, 30);
    close(server, game);

    // The game's connection breaks while the keyboard is up: stopped, said so, not retried.
    let (server, game) = pair(ServerOpts { seat: true, keyboard_cap: true }).unwrap();
    layer::surface_created(4, 40, game.display);
    tick_until(KEYS_WL_ACTIVE);
    server.send(Cmd::Kill);
    server.sync();
    tick_until(KEYS_WL_DISPLAY_ERROR);
    for _ in 0..5 {
        layer::service(Instant::now());
    }
    assert_eq!(layer::state(), KEYS_WL_DISPLAY_ERROR);
    layer::surface_destroyed(4, 40);
    assert_eq!(layer::state(), KEYS_WL_DISPLAY_ERROR);
    game.disconnect();
    drop(server);
    assert_eq!(keymap_fds_open(), 0, "every keymap fd was closed");
}
