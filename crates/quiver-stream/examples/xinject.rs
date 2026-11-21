//! XTest input for nested tests (tools/nested.sh): only ever run against a private, headless
//! gamescope's X display.
//!
//! `xinject key ctrl+alt+shift+s` taps a chord; `down K` / `up K` hold or release; keys are
//! names below or X keycodes (`k66`). `click N` clicks mouse button N; `move DX DY` moves
//! the pointer relatively.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;

/// X keycodes (evdev + 8) for the names the tests use.
fn keycode(name: &str) -> Option<u8> {
    if let Some(n) = name.strip_prefix('k')
        && let Ok(v) = n.parse()
    {
        return Some(v);
    }
    let evdev: u8 = match name {
        "esc" | "escape" => 1,
        "1" => 2,
        "tab" => 15,
        "q" => 16,
        "w" => 17,
        "return" | "enter" => 28,
        "ctrl" => 29,
        "a" => 30,
        "s" => 31,
        "d" => 32,
        "shift" => 42,
        "z" => 44,
        "alt" => 56,
        "space" => 57,
        "caps" | "capslock" => 58,
        "up" => 103,
        "left" => 105,
        "right" => 106,
        "down" => 108,
        "super" => 125,
        _ => return None,
    };
    Some(evdev + 8)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let display = std::env::var("DISPLAY").unwrap_or_default();
    if matches!(display.as_str(), ":0" | ":1" | ":2" | ":3" | ":4" | ":5" | ":6" | ":7" | ":8" | ":9") || display.is_empty() {
        eprintln!("xinject: refusing display {display:?} (nested displays only)");
        std::process::exit(2);
    }
    let (conn, screen) = x11rb::connect(None).expect("connect to X");
    let root = conn.setup().roots[screen].root;
    let fake = |ty: u8, detail: u8, x: i16, y: i16| {
        conn.xtest_fake_input(ty, detail, 0, root, x, y, 0).expect("xtest").check().expect("xtest");
    };
    let keys = |spec: &str| -> Vec<u8> { spec.split('+').map(|k| keycode(k).unwrap_or_else(|| panic!("unknown key {k}"))).collect() };
    match args.first().map(String::as_str) {
        Some("key") => {
            let ks = keys(&args[1]);
            for &k in &ks {
                fake(KEY_PRESS_EVENT, k, 0, 0);
            }
            conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(60));
            for &k in ks.iter().rev() {
                fake(KEY_RELEASE_EVENT, k, 0, 0);
            }
        }
        Some("down") => keys(&args[1]).into_iter().for_each(|k| fake(KEY_PRESS_EVENT, k, 0, 0)),
        Some("up") => keys(&args[1]).into_iter().for_each(|k| fake(KEY_RELEASE_EVENT, k, 0, 0)),
        Some("click") => {
            let b: u8 = args[1].parse().expect("button");
            fake(BUTTON_PRESS_EVENT, b, 0, 0);
            conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(40));
            fake(BUTTON_RELEASE_EVENT, b, 0, 0);
        }
        Some("move") => {
            let dx: i16 = args[1].parse().expect("dx");
            let dy: i16 = args[2].parse().expect("dy");
            // detail 1: relative motion.
            fake(MOTION_NOTIFY_EVENT, 1, dx, dy);
        }
        _ => {
            eprintln!("usage: xinject key A+B | down K | up K | click N | move DX DY");
            std::process::exit(2);
        }
    }
    conn.flush().unwrap();
    conn.sync().unwrap();
}
