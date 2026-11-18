//! The Wayland HUD keys end to end through the real layer: a fake compositor and fake game
//! (tests/wayland, the game reading its socket on its own thread like Wine), a
//! `VkSurfaceKHR` made with `vkCreateWaylandSurfaceKHR` on the game's `wl_display` through the
//! layer, and a headless Vulkan app (tests/harness) presenting so the layer's keys thread
//! runs. Shift+F3 and Ctrl+Shift+F3 come from the compositor while it presents; the layer's
//! status block must count one of each and say `KEYS_WL_ACTIVE`, and `vkDestroySurfaceKHR`
//! must release the layer's keyboard and seat (and the state say `KEYS_WL_STOPPED`).
//!
//! `DISPLAY` and `WAYLAND_DISPLAY` are removed, so nothing reaches the real desktop.
//! Opt-in like tests/vulkan_keys.rs (needs a GPU with `VK_EXT_headless_surface` and a driver
//! with `VK_KHR_wayland_surface`):
//! `cargo test -p quiver-sight-layer --test vulkan_keys_wayland -- --ignored --nocapture`.

mod harness;
mod wayland;

use std::sync::atomic::Ordering;
use std::time::Duration;

use ash::vk;
use harness::*;
use quiver_perf::layout::{KEYS_NO_DISPLAY, KEYS_WL_ACTIVE, KEYS_WL_STOPPED, LAYER_FOCUS_KNOWN, LAYER_FOCUSED, LAYER_WAYLAND_SURFACE};
use quiver_perf::session::open_ring_reader;
use quiver_sight_layer::keys::evdev::{KEY_F3, KEY_LEFTCTRL, KEY_LEFTSHIFT};
use wayland::{Cmd, Game, Server, ServerOpts, socketpair, wait_for};

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface and VK_KHR_wayland_surface; see the module docs"]
fn wayland_chords_reach_the_status_block() {
    let (s, c) = socketpair();
    let Some(server) = Server::start(s, ServerOpts { seat: true, keyboard_cap: true }) else {
        eprintln!("skipping: no libwayland-server.so.0");
        return;
    };
    let game = Game::connect(c).expect("libwayland-client.so.0");
    server.send(Cmd::Focus(vec![]));
    server.sync();

    let session = TestSession::new();
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    // SAFETY: the only test in this binary; nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("QUIVER_PERF_DIR", session.dir.path());
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        for (k, v) in layer_env(scratch.path(), None) {
            std::env::set_var(k, v);
        }
    }

    // The "game's" Vulkan instance and its Wayland surface, through the layer.
    let entry = unsafe { ash::Entry::load() }.unwrap();
    let exts = [ash::khr::surface::NAME.as_ptr(), ash::khr::wayland_surface::NAME.as_ptr()];
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    let Ok(instance) = (unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts), None) }) else {
        eprintln!("skipping: no VK_KHR_wayland_surface");
        return;
    };
    let wl = ash::khr::wayland_surface::Instance::new(&entry, &instance);
    let info = vk::WaylandSurfaceCreateInfoKHR::default().display(game.display.cast()).surface(game.surface.cast());
    let surface = match unsafe { wl.create_wayland_surface(&info, None) } {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: vkCreateWaylandSurfaceKHR on the fake compositor: {e:?}");
            unsafe { instance.destroy_instance(None) };
            return;
        }
    };

    // Present (headless) so the layer's telemetry and keys thread run; the compositor types.
    let status = std::cell::RefCell::new(None);
    let in_front = std::cell::RefCell::new(None);
    let frames = 180;
    let r = run(Opts {
        frames,
        fps: 60.0,
        readback: false,
        on_frame: Some(Box::new(|f, _, _| match f {
            30 => server.keys(&[(KEY_LEFTSHIFT, true), (KEY_F3, true)]),
            45 => server.keys(&[(KEY_F3, false), (KEY_LEFTSHIFT, false)]),
            80 => server.keys(&[(KEY_LEFTCTRL, true), (KEY_LEFTSHIFT, true), (KEY_F3, true)]),
            95 => server.keys(&[(KEY_F3, false), (KEY_LEFTSHIFT, false), (KEY_LEFTCTRL, false)]),
            // Bare F3 does nothing.
            110 => server.keys(&[(KEY_F3, true)]),
            125 => server.keys(&[(KEY_F3, false)]),
            // The game's window in front (SIGHT §14), then the keyboard goes elsewhere.
            135 => *in_front.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()),
            140 => server.send(Cmd::Leave),
            f if f == frames - 1 => *status.borrow_mut() = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()),
            _ => {}
        })),
        ..Opts::default()
    });
    eprintln!("presented on {}", r.device_name);
    let s = status.into_inner().expect("a status block");
    assert_eq!(s.keys_wayland(), KEYS_WL_ACTIVE, "keys {:#x}", s.keys);
    assert_eq!(s.keys_x11(), KEYS_NO_DISPLAY);
    assert_ne!(s.flags & LAYER_WAYLAND_SURFACE, 0);
    assert_eq!((s.key_toggles, s.key_nexts), (1, 1), "one Shift+F3, one Ctrl+Shift+F3");
    assert_eq!(server.stats.keyboards_live.load(Ordering::Relaxed), 2, "the game's keyboard and the layer's");
    assert_eq!(game.seen.keys.load(Ordering::Relaxed), 12, "the game still got every key");
    let f = in_front.into_inner().expect("a status block").flags;
    assert_eq!(f & (LAYER_FOCUS_KNOWN | LAYER_FOCUSED), LAYER_FOCUS_KNOWN | LAYER_FOCUSED, "in front while it has the keyboard");
    assert_eq!(s.flags & (LAYER_FOCUS_KNOWN | LAYER_FOCUSED), LAYER_FOCUS_KNOWN, "not after leave");

    // vkDestroySurfaceKHR: the layer lets go of the display first.
    unsafe { ash::khr::surface::Instance::new(&entry, &instance).destroy_surface(surface, None) };
    server.sync();
    assert!(wait_for(2.0, || server.stats.keyboards_live.load(Ordering::Relaxed) == 1 && server.stats.seats_live.load(Ordering::Relaxed) == 1));
    assert_eq!(server.stats.keyboards_released.load(Ordering::Relaxed), 1);
    // The keys thread reports the new state within a tick; the heartbeat writes it.
    std::thread::sleep(Duration::from_millis(400));
    let s = open_ring_reader(&session.ring_path()).ok().and_then(|r| r.layer_status()).unwrap();
    assert_eq!(s.keys_wayland(), KEYS_WL_STOPPED);
    assert_eq!(s.flags & LAYER_FOCUS_KNOWN, 0, "no keyboard of the layer's: unknown");
    unsafe { instance.destroy_instance(None) };
    assert!(!game.seen.wrong_thread.load(Ordering::Relaxed));

    server.send(Cmd::Kill);
    server.sync();
    game.disconnect();
}
