//! A test program for the command target (`quiver-stream-host --test-window`): an X11 window
//! that fills the screen, draws the frame stamp each frame with the time it drew it, and logs
//! the input it receives.
//!
//! Run inside the stream's gamescope, it measures what no camera is needed for:
//! - **the display's own latency**: the stamp carries the moment the program drew the frame,
//!   so the decoded stream shows how long gamescope took to hand it to capture;
//! - **input injection**: every key, button, pointer motion and XI2 raw motion event it gets
//!   is appended to `$QUIVER_TESTWIN_LOG` as one line (`key 38 down`, `raw 5 -3`, …).

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use quiver_stream_proto::stamp::{self, Stamp};
use quiver_stream_proto::now_micros;
use x11rb::connection::Connection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Rectangle, WindowClass};

pub fn run(fps: u32) -> Result<()> {
    let (conn, screen) = x11rb::connect(None).context("connect to $DISPLAY")?;
    let s = conn.setup().roots[screen].clone();
    let (w, h) = (s.width_in_pixels, s.height_in_pixels);
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        s.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(s.black_pixel).event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::EXPOSURE),
    )?;
    conn.change_property8(xproto::PropMode::REPLACE, win, xproto::AtomEnum::WM_NAME, xproto::AtomEnum::STRING, b"quiver-stream test window")?;
    let gc = |pixel: u32| -> Result<u32> {
        let g = conn.generate_id()?;
        conn.create_gc(g, win, &CreateGCAux::new().foreground(pixel))?;
        Ok(g)
    };
    let (white, black, gray) = (gc(s.white_pixel)?, gc(s.black_pixel)?, gc(0x40_40_40)?);
    // XI2 raw motion on the root, as Wine's raw input reads it.
    let raw = conn.xinput_xi_query_version(2, 2)?.reply().is_ok();
    if raw {
        conn.xinput_xi_select_events(s.root, &[xinput::EventMask { deviceid: 1, mask: vec![xinput::XIEventMask::RAW_MOTION] }])?;
    }
    conn.map_window(win)?;
    conn.flush()?;
    let mut log = std::env::var_os("QUIVER_TESTWIN_LOG").map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p)).transpose()?;
    let mut line = |l: String| {
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{l}");
        }
    };
    line(format!("ready {w}x{h} raw={raw}"));
    let period = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
    let mut next = std::time::Instant::now();
    let mut seq = 0u64;
    loop {
        while let Some(ev) = conn.poll_for_event()? {
            match ev {
                Event::KeyPress(e) => line(format!("key {} down", e.detail)),
                Event::KeyRelease(e) => line(format!("key {} up", e.detail)),
                Event::ButtonPress(e) => line(format!("button {} down", e.detail)),
                Event::ButtonRelease(e) => line(format!("button {} up", e.detail)),
                Event::MotionNotify(e) => line(format!("motion {} {}", e.event_x, e.event_y)),
                Event::XinputRawMotion(e) => {
                    let v: Vec<f64> = e.axisvalues_raw.iter().map(|f| f.integral as f64 + f.frac as f64 / 4_294_967_296.0).collect();
                    // Valuators 0 and 1 are x and y, when present in the mask.
                    let m = e.valuator_mask.first().copied().unwrap_or(0);
                    let mut it = v.into_iter();
                    let x = if m & 1 != 0 { it.next().unwrap_or(0.0) } else { 0.0 };
                    let y = if m & 2 != 0 { it.next().unwrap_or(0.0) } else { 0.0 };
                    line(format!("raw {x:.0} {y:.0}"));
                }
                _ => {}
            }
        }
        let captured = now_micros();
        let band = stamp::layout(w as u32, h as u32).map(|l| l.size().1).unwrap_or(0) as u16;
        conn.poly_fill_rectangle(win, gray, &[Rectangle { x: 0, y: 0, width: w, height: band }])?;
        let mut on = Vec::new();
        let mut off = Vec::new();
        stamp::paint(w as u32, h as u32, Stamp { seq, captured }, |x, y, cw, ch, white| {
            let r = Rectangle { x: x as i16, y: y as i16, width: cw as u16, height: ch as u16 };
            if white { on.push(r) } else { off.push(r) }
        });
        conn.poly_fill_rectangle(win, white, &on)?;
        conn.poly_fill_rectangle(win, black, &off)?;
        // A moving block below, so every frame changes.
        let bx = ((seq * 8) % (w as u64).saturating_sub(64).max(1)) as i16;
        conn.poly_fill_rectangle(win, black, &[Rectangle { x: 0, y: band as i16 + 8, width: w, height: 64 }])?;
        conn.poly_fill_rectangle(win, white, &[Rectangle { x: bx, y: band as i16 + 8, width: 64, height: 64 }])?;
        conn.flush()?;
        seq += 1;
        next += period;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// The placeholder a [`crate::Target::Display`] runs until the program someone else starts
/// shows up (`quiver-stream-host --placeholder`): it paints a black full-screen window for a
/// moment, so the display's screencast starts and capture connects, then removes it and waits
/// (keeping gamescope up) until the stream ends. The program's own window is then the only
/// one, and gamescope shows it.
pub fn placeholder() -> Result<()> {
    let (conn, screen) = x11rb::connect(None).context("connect to $DISPLAY")?;
    let s = conn.setup().roots[screen].clone();
    let win = conn.generate_id()?;
    conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, win, s.root, 0, 0, s.width_in_pixels, s.height_in_pixels, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new().background_pixel(s.black_pixel))?;
    conn.change_property8(xproto::PropMode::REPLACE, win, xproto::AtomEnum::WM_NAME, xproto::AtomEnum::STRING, b"quiver-stream placeholder")?;
    conn.map_window(win)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(s.black_pixel))?;
    for _ in 0..60 {
        conn.poly_fill_rectangle(win, gc, &[Rectangle { x: 0, y: 0, width: s.width_in_pixels, height: s.height_in_pixels }])?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(33));
    }
    conn.destroy_window(win)?;
    conn.flush()?;
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
