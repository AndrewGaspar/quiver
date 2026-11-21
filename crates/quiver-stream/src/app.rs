//! The stream window: SDL3 on the main thread (window, input, pads, the control channel and
//! the periodic Ping/Stats), the video threads ([`crate::video`]) and audio.
//!
//! Capture: input goes to the game only while captured. Capturing grabs the keyboard
//! (`SDL_SetWindowKeyboardGrab`; on Wayland that is keyboard-shortcuts-inhibit, so Super and
//! Alt+Tab reach the game) and, when "Capture mouse" is on, puts the mouse in relative mode
//! (SDL3 gives unaccelerated motion there). The window captures when it first gets focus,
//! and on a click after a release. Losing focus, opening the menu or Ctrl+Alt+Shift+Z
//! releases it, with a `ReleaseAll` to the host.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ash::vk;
use crossbeam_channel::{Receiver, unbounded};
use quiver_stream_proto::input::{InputEvent, PadKind, PadState, Stamped};
use quiver_stream_proto::{ClientCaps, Control, CursorImage, DisplayCaps, PROTOCOL_VERSION, StreamPrefs, StreamTarget, now_micros};
use sdl3_sys::events::*;
use sdl3_sys::everything as sdl;
use sdl3_sys::gamepad::*;
use sdl3_sys::keyboard::SDL_GetModState;
use sdl3_sys::scancode::*;
use tracing::{info, warn};

use crate::audio::{self, Playback};
use crate::av::Backend;
use crate::decode::VideoDecoder;
use crate::gpu::Gpu;
use crate::gpu::convert::Converter;
use crate::gpu::present::Presenter;
use crate::input::{self, Accum, ChordWatch, Hotkey, Keys};
use crate::link::{ControlIn, Outgoing, StreamLink};
use crate::ui::atlas::Atlas;
use crate::ui::menu::{Action, Menu, Nav, ROWS};
use crate::ui::overlay;
use crate::video::{self, MeasureReport, PresentPrefs, Shared};

/// How the window starts.
#[derive(Debug, Clone)]
pub struct Options {
    pub windowed: bool,
    pub size: Option<(u32, u32)>,
    pub prefs: StreamPrefs,
    pub present_mode: Option<vk::PresentModeKHR>,
    pub decoder: Option<Backend>,
    pub stats: bool,
    pub pads: bool,
    pub audio: bool,
    pub sbs: bool,
    pub target: StreamTarget,
    /// Show the first-run hint.
    pub intro: bool,
    /// `--measure`: stop after this many frames and report.
    pub measure: Option<u64>,
    /// Quit after this long (for tests).
    pub duration: Option<Duration>,
    /// Each second's numbers also go here (`connect` writes them as netlab stats lines).
    pub periods: Option<crossbeam_channel::Sender<crate::stats::Period>>,
}

fn sdl_error() -> String {
    unsafe { CStr::from_ptr(sdl::SDL_GetError()) }.to_string_lossy().into_owned()
}

struct Pad {
    gamepad: *mut SDL_Gamepad,
    slot: u8,
    kind: PadKind,
    state: PadState,
    dirty: bool,
    gyro: [f32; 3],
    accel: [f32; 3],
    chord: ChordWatch,
}

struct App {
    window: *mut sdl::SDL_Window,
    shared: Arc<Shared>,
    out: crossbeam_channel::Sender<Outgoing>,
    input_seq: u64,
    captured: bool,
    /// Capture was on when the menu opened.
    resume_capture: bool,
    focused: bool,
    ever_focused: bool,
    capture_mouse: bool,
    /// The host asked for absolute positions (a menu or desktop): relative mode off.
    host_absolute: bool,
    keys: Keys,
    accum: Accum,
    /// A click that captured: don't forward its release.
    swallow_up: Option<u8>,
    pads: HashMap<u32, Pad>,
    prefs: StreamPrefs,
    scale: f32,
    sbs: bool,
    quit: Option<String>,
    cursor: *mut sdl::SDL_Cursor,
    /// Where the pointer was when the menu first saw it: hovering picks rows only once it
    /// has moved from there (leaving relative mode warps it, which isn't the person
    /// pointing at a row).
    menu_pointer: Option<(f32, f32)>,
}

impl App {
    fn send(&mut self, event: InputEvent) {
        self.input_seq += 1;
        let _ = self.out.send(Outgoing::Input(Stamped { seq: self.input_seq, at: now_micros(), event }));
    }

    fn control(&self, c: Control) {
        let _ = self.out.send(Outgoing::Control(c));
    }

    fn overlay(&self) -> std::sync::MutexGuard<'_, overlay::OverlayState> {
        self.shared.overlay.lock().unwrap()
    }

    fn apply_mouse_mode(&self) {
        let relative = self.captured && self.capture_mouse && !self.host_absolute;
        unsafe { sdl::SDL_SetWindowRelativeMouseMode(self.window, relative) };
    }

    fn set_captured(&mut self, on: bool) {
        if on == self.captured {
            return;
        }
        self.captured = on;
        unsafe { sdl::SDL_SetWindowKeyboardGrab(self.window, on) };
        self.apply_mouse_mode();
        if !on {
            self.send(InputEvent::ReleaseAll);
            self.keys.clear();
            for p in self.pads.values_mut() {
                p.state = PadState::default();
            }
        }
        self.overlay().uncaptured_hint = !on && self.overlay().menu.is_none();
        self.shared.redraw();
    }

    fn display_caps(&self) -> DisplayCaps {
        let (mut w, mut h) = (0, 0);
        unsafe { sdl::SDL_GetWindowSizeInPixels(self.window, &mut w, &mut h) };
        let display = unsafe { sdl::SDL_GetDisplayForWindow(self.window) };
        let mode = unsafe { sdl::SDL_GetCurrentDisplayMode(display) };
        let refresh = if mode.is_null() { 60.0 } else { unsafe { (*mode).refresh_rate } };
        DisplayCaps {
            width: w as u32,
            height: h as u32,
            refresh_mhz: (refresh.max(1.0) * 1000.0).round() as u32,
            hdr: false,
            vrr: false,
            stereo_sbs: self.sbs,
            scale: self.scale,
        }
    }

    fn open_menu(&mut self, select: usize) {
        if self.overlay().menu.is_some() {
            return;
        }
        self.resume_capture = self.captured;
        self.set_captured(false);
        let stats = self.overlay().stats;
        let mut m = Menu::new(self.prefs.clone(), self.capture_mouse, stats);
        m.selected = select;
        self.menu_pointer = None;
        let mut o = self.overlay();
        o.menu = Some(m);
        o.uncaptured_hint = false;
        drop(o);
        self.shared.redraw();
    }

    fn close_menu(&mut self) {
        self.overlay().menu = None;
        if self.resume_capture && self.focused {
            self.set_captured(true);
        } else {
            self.overlay().uncaptured_hint = !self.captured;
        }
        self.shared.redraw();
    }

    fn menu_nav(&mut self, n: Nav) {
        let action = {
            let mut o = self.overlay();
            let Some(m) = o.menu.as_mut() else { return };
            m.nav(n)
        };
        self.shared.redraw();
        let Some(a) = action else { return };
        match a {
            Action::Close => self.close_menu(),
            Action::Prefs(p) => {
                self.shared.present.lock().unwrap().pacing = p.pacing;
                self.shared.present.lock().unwrap().hdr = p.hdr != Some(false);
                self.prefs = p.clone();
                self.control(Control::Prefs(p));
            }
            Action::CaptureMouse(on) => {
                self.capture_mouse = on;
                self.apply_mouse_mode();
            }
            Action::Stats(on) => self.overlay().stats = on,
            Action::Disconnect => {
                self.control(Control::Bye { reason: "disconnect".into() });
                self.quit = Some("disconnected".into());
            }
            Action::QuitGame => {
                self.control(Control::Bye { reason: quiver_stream_proto::BYE_QUIT_GAME.into() });
                self.quit = Some("quit the game".into());
            }
        }
    }

    fn hotkey(&mut self, h: Hotkey) {
        // The host saw Ctrl, Alt and Shift go down: let them go. (Releasing capture does it
        // anyway.)
        if self.captured && h == Hotkey::Stats {
            self.send(InputEvent::ReleaseAll);
            self.keys.clear();
        }
        match h {
            Hotkey::Menu => {
                if self.overlay().menu.is_some() {
                    self.close_menu();
                } else {
                    self.open_menu(0);
                }
            }
            Hotkey::Release => {
                self.set_captured(false);
                self.overlay().toast("Input released · click to capture again");
            }
            Hotkey::Stats => {
                let mut o = self.overlay();
                o.stats = !o.stats;
                drop(o);
                self.shared.redraw();
            }
        }
    }

    fn menu_hit(&self, x: f32, y: f32) -> Option<usize> {
        let (w, h) = self.shared.present.lock().unwrap().window;
        // Mouse coordinates are in window points; the layout is in pixels.
        let density = unsafe { sdl::SDL_GetWindowPixelDensity(self.window) }.max(0.1);
        let (px, py) = (x * density, y * density);
        let (rx, ry, rw, _rh, pitch) = overlay::menu_rows(self.scale, (w as f32, h as f32));
        if px < rx || px > rx + rw || py < ry {
            return None;
        }
        let i = ((py - ry) / pitch) as usize;
        (i < ROWS.len()).then_some(i)
    }

    fn key(&mut self, e: &SDL_KeyboardEvent) {
        if e.down
            && let Some(h) = input::hotkey(e.scancode, unsafe { SDL_GetModState() })
        {
            if !e.repeat {
                self.hotkey(h);
            }
            return;
        }
        if self.overlay().menu.is_some() {
            if e.down {
                let n = match e.scancode {
                    SDL_SCANCODE_UP | SDL_SCANCODE_W => Some(Nav::Up),
                    SDL_SCANCODE_DOWN | SDL_SCANCODE_S | SDL_SCANCODE_TAB => Some(Nav::Down),
                    SDL_SCANCODE_LEFT | SDL_SCANCODE_A => Some(Nav::Left),
                    SDL_SCANCODE_RIGHT | SDL_SCANCODE_D => Some(Nav::Right),
                    SDL_SCANCODE_RETURN | SDL_SCANCODE_SPACE | SDL_SCANCODE_KP_ENTER => Some(Nav::Choose),
                    SDL_SCANCODE_ESCAPE | SDL_SCANCODE_BACKSPACE => Some(Nav::Back),
                    _ => None,
                };
                if let Some(n) = n {
                    self.menu_nav(n);
                }
            }
            return;
        }
        if !self.captured {
            return;
        }
        if let Some(ev) = self.keys.event(e.scancode, e.key, e.down, e.repeat) {
            self.send(ev);
        }
    }

    fn pad_added(&mut self, id: u32) {
        if self.pads.contains_key(&id) {
            return;
        }
        let Some(slot) = input::free_slot(self.pads.values().map(|p| p.slot)) else {
            warn!("a fifth pad: ignored");
            return;
        };
        let jid = sdl::SDL_JoystickID(id);
        let gp = unsafe { SDL_OpenGamepad(jid) };
        if gp.is_null() {
            warn!("opening a gamepad: {}", sdl_error());
            return;
        }
        let kind = unsafe { input::pad_kind(SDL_GetGamepadType(gp), SDL_GetGamepadVendor(gp), SDL_GetGamepadProduct(gp)) };
        for s in [sdl::SDL_SENSOR_GYRO, sdl::SDL_SENSOR_ACCEL] {
            unsafe {
                if SDL_GamepadHasSensor(gp, s) {
                    SDL_SetGamepadSensorEnabled(gp, s, true);
                }
            }
        }
        let name = unsafe { CStr::from_ptr(SDL_GetGamepadName(gp)) }.to_string_lossy().into_owned();
        info!("pad {slot}: {name} ({kind:?})");
        self.pads.insert(
            id,
            Pad {
                gamepad: gp,
                slot,
                kind,
                state: PadState::default(),
                dirty: false,
                gyro: [0.0; 3],
                accel: [0.0; 3],
                chord: ChordWatch::default(),
            },
        );
        self.send(InputEvent::PadArrive { slot, kind });
        self.send(InputEvent::Pad { slot, state: PadState::default() });
        let kind = self.pads[&id].kind;
        self.overlay().toast(format!("Controller {}: {name} ({kind:?})", slot + 1));
        self.shared.redraw();
    }

    fn pad_removed(&mut self, id: u32) {
        if let Some(p) = self.pads.remove(&id) {
            unsafe { SDL_CloseGamepad(p.gamepad) };
            self.send(InputEvent::PadLeave { slot: p.slot });
            self.overlay().toast(format!("Controller {} disconnected", p.slot + 1));
            self.shared.redraw();
        }
    }

    fn pad_button(&mut self, id: u32, b: SDL_GamepadButton, down: bool) {
        let menu_open = self.overlay().menu.is_some();
        let Some(p) = self.pads.get_mut(&id) else {
            return;
        };
        let Some(bit) = input::pad_bit(b) else { return };
        if menu_open {
            p.state.buttons.0 = if down { p.state.buttons.0 | bit } else { p.state.buttons.0 & !bit };
            p.chord.update(p.state.buttons.0, Instant::now());
            if down {
                let n = match b {
                    SDL_GAMEPAD_BUTTON_DPAD_UP => Some(Nav::Up),
                    SDL_GAMEPAD_BUTTON_DPAD_DOWN => Some(Nav::Down),
                    SDL_GAMEPAD_BUTTON_DPAD_LEFT => Some(Nav::Left),
                    SDL_GAMEPAD_BUTTON_DPAD_RIGHT => Some(Nav::Right),
                    SDL_GAMEPAD_BUTTON_SOUTH => Some(Nav::Choose),
                    SDL_GAMEPAD_BUTTON_EAST => Some(Nav::Back),
                    _ => None,
                };
                if let Some(n) = n {
                    self.menu_nav(n);
                }
            }
            return;
        }
        p.state.buttons.0 = if down { p.state.buttons.0 | bit } else { p.state.buttons.0 & !bit };
        p.dirty = true;
        if p.chord.update(p.state.buttons.0, Instant::now()) {
            self.open_menu(0);
        }
    }

    fn pad_axis(&mut self, id: u32, axis: SDL_GamepadAxis, v: i16) {
        let menu_open = self.overlay().menu.is_some();
        if let Some(p) = self.pads.get_mut(&id) {
            if menu_open {
                return;
            }
            input::apply_axis(&mut p.state, axis, v);
            p.dirty = true;
        }
    }

    fn pad_sensor(&mut self, id: u32, sensor: i32, data: [f32; 3]) {
        let (slot, gyro, accel) = {
            let Some(p) = self.pads.get_mut(&id) else {
                return;
            };
            if sensor == sdl::SDL_SENSOR_ACCEL.0 {
                p.accel = data;
                return;
            }
            if sensor != sdl::SDL_SENSOR_GYRO.0 {
                return;
            }
            p.gyro = data;
            (p.slot, p.gyro, p.accel)
        };
        if self.captured && self.overlay().menu.is_none() {
            let (g, a) = input::motion_units(gyro, accel);
            self.send(InputEvent::PadMotion { slot, gyro: g, accel: a });
        }
    }

    /// Send pads whose state changed in this batch of events (or all, periodically).
    fn flush_pads(&mut self, all: bool) {
        if !self.focused {
            return;
        }
        let mut evs = Vec::new();
        for p in self.pads.values_mut() {
            if p.dirty || all {
                p.dirty = false;
                evs.push(InputEvent::Pad { slot: p.slot, state: p.state });
            }
        }
        for e in evs {
            self.send(e);
        }
    }

    fn rumble(&self, slot: u8, low: u16, high: u16, ms: u16) {
        if let Some(p) = self.pads.values().find(|p| p.slot == slot) {
            unsafe { SDL_RumbleGamepad(p.gamepad, low, high, ms as u32) };
        }
    }

    fn set_cursor(&mut self, shape: Option<CursorImage>) {
        unsafe {
            if !self.cursor.is_null() {
                sdl::SDL_DestroyCursor(self.cursor);
                self.cursor = std::ptr::null_mut();
            }
            match shape {
                None => {
                    self.host_absolute = false;
                    sdl::SDL_HideCursor();
                }
                Some(c) if c.relative => {
                    self.host_absolute = false;
                    sdl::SDL_HideCursor();
                }
                Some(c) => {
                    self.host_absolute = true;
                    let (w, h) = (c.width.min(256) as c_int, c.height.min(256) as c_int);
                    if c.rgba.len() >= (w * h * 4) as usize {
                        let surf = sdl::SDL_CreateSurfaceFrom(w, h, sdl::SDL_PIXELFORMAT_RGBA32, c.rgba.as_ptr() as *mut _, w * 4);
                        if !surf.is_null() {
                            self.cursor = sdl::SDL_CreateColorCursor(surf, c.hot_x as c_int, c.hot_y as c_int);
                            sdl::SDL_DestroySurface(surf);
                            if !self.cursor.is_null() {
                                sdl::SDL_SetCursor(self.cursor);
                            }
                        }
                    }
                    sdl::SDL_ShowCursor();
                }
            }
        }
        self.apply_mouse_mode();
    }

    fn handle_control(&mut self, m: ControlIn, playback: &Option<Playback>) {
        match m {
            ControlIn::Closed { reason } => self.quit = Some(reason),
            ControlIn::Transport(t) => {
                let mut w = self.shared.stats.window.lock().unwrap();
                w.packets_received += t.packets_received;
                w.packets_lost += t.packets_lost;
                w.packets_recovered += t.packets_recovered;
                w.estimated_kbps = t.estimated_kbps;
                if t.jitter_us.is_some() {
                    w.jitter_us = t.jitter_us;
                }
                drop(w);
                if let Some(o) = t.clock_offset_us {
                    self.shared.clock.lock().unwrap().external = Some(o);
                }
            }
            ControlIn::Control(c) => match c {
                Control::Welcome { host_name, version, .. } => {
                    if version / 1000 != PROTOCOL_VERSION / 1000 && version != PROTOCOL_VERSION {
                        warn!("host speaks protocol {version}, this client {PROTOCOL_VERSION}");
                    }
                    self.overlay().host_name = host_name;
                }
                Control::Configure(cfg) => {
                    info!(
                        "stream: {:?} {}×{} @ {:.2} Hz, HDR {}",
                        cfg.codec,
                        cfg.width,
                        cfg.height,
                        cfg.refresh_mhz as f32 / 1000.0,
                        cfg.color.hdr.is_some()
                    );
                    let mut o = self.overlay();
                    o.snapshot.codec = format!("{:?}", cfg.codec).to_uppercase();
                    o.snapshot.size = (cfg.width, cfg.height);
                    o.snapshot.hdr = cfg.color.hdr.is_some();
                    drop(o);
                    if cfg.audio_channels != 2 && playback.is_some() {
                        warn!("{} audio channels: this client plays stereo", cfg.audio_channels);
                    }
                    self.shared.set_config(cfg);
                    self.shared.redraw();
                }
                Control::Pong { sent, peer_now, .. } => self.shared.clock.lock().unwrap().on_pong(sent, peer_now, now_micros()),
                Control::Ping { id, sent } => self.control(Control::Pong { id, sent, peer_now: now_micros() }),
                Control::HostStats(h) => self.overlay().snapshot.host = Some(h),
                Control::Cursor { shape } => self.set_cursor(shape),
                Control::Rumble { slot, low, high, duration_ms } => self.rumble(slot, low, high, duration_ms),
                Control::Notice { text } => {
                    self.overlay().toast(text);
                    self.shared.redraw();
                }
                Control::Bye { reason } => self.quit = Some(format!("the host ended the stream: {reason}")),
                other => info!("control: {other:?}"),
            },
        }
    }

    fn event(&mut self, ev: &SDL_Event) {
        let ty = unsafe { ev.r#type };
        let t = SDL_EventType(ty);
        match t {
            SDL_EVENT_QUIT => {
                self.control(Control::Bye { reason: "disconnect".into() });
                self.quit = Some("closed".into());
            }
            SDL_EVENT_WINDOW_CLOSE_REQUESTED => {
                // Ask: leave the game running (Disconnect) or quit it.
                if self.overlay().menu.is_some() {
                    self.control(Control::Bye { reason: "disconnect".into() });
                    self.quit = Some("closed".into());
                } else {
                    self.open_menu(7);
                }
            }
            SDL_EVENT_WINDOW_FOCUS_GAINED => {
                self.focused = true;
                if !self.ever_focused {
                    self.ever_focused = true;
                    if self.overlay().menu.is_none() {
                        self.set_captured(true);
                    }
                }
            }
            SDL_EVENT_WINDOW_FOCUS_LOST => {
                self.focused = false;
                self.set_captured(false);
                self.send(InputEvent::ReleaseAll);
            }
            SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED => {
                let (w, h) = unsafe { (ev.window.data1, ev.window.data2) };
                self.shared.present.lock().unwrap().window = (w.max(1) as u32, h.max(1) as u32);
                self.shared.redraw();
                let d = self.display_caps();
                self.control(Control::Resize { display: d });
            }
            SDL_EVENT_WINDOW_EXPOSED => self.shared.redraw(),
            SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP => {
                let k = unsafe { ev.key };
                self.key(&k);
            }
            SDL_EVENT_MOUSE_MOTION => {
                let m = unsafe { ev.motion };
                let moved = match self.menu_pointer {
                    None => {
                        self.menu_pointer = Some((m.x, m.y));
                        false
                    }
                    Some((x, y)) => (m.x - x).abs() + (m.y - y).abs() > 3.0,
                };
                if self.overlay().menu.is_some() {
                    if !moved {
                    } else if let Some(i) = self.menu_hit(m.x, m.y) {
                        if let Some(menu) = self.overlay().menu.as_mut() {
                            menu.hover(i);
                        }
                        self.shared.redraw();
                    }
                } else if self.captured {
                    if self.capture_mouse && !self.host_absolute {
                        if let Some(e) = self.accum.motion(m.xrel, m.yrel) {
                            self.send(e);
                        }
                    } else {
                        let density = unsafe { sdl::SDL_GetWindowPixelDensity(self.window) }.max(0.1);
                        let rect = *self.shared.picture_rect.lock().unwrap();
                        if let Some((x, y)) = input::absolute_position(m.x * density, m.y * density, rect) {
                            self.send(InputEvent::MousePosition { x, y });
                        }
                    }
                }
            }
            SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => {
                let b = unsafe { ev.button };
                let down = t == SDL_EVENT_MOUSE_BUTTON_DOWN;
                if self.overlay().menu.is_some() {
                    if down
                        && b.button == 1
                        && let Some(i) = self.menu_hit(b.x, b.y)
                    {
                        if let Some(menu) = self.overlay().menu.as_mut() {
                            menu.hover(i);
                        }
                        self.menu_nav(Nav::Choose);
                    }
                    return;
                }
                if !self.captured {
                    if down && self.focused {
                        self.set_captured(true);
                        self.swallow_up = Some(b.button);
                    }
                    return;
                }
                if !down && self.swallow_up == Some(b.button) {
                    self.swallow_up = None;
                    return;
                }
                if let Some(button) = input::mouse_button(b.button) {
                    self.send(InputEvent::MouseButton { button, down });
                }
            }
            SDL_EVENT_MOUSE_WHEEL => {
                let w = unsafe { ev.wheel };
                if self.overlay().menu.is_some() {
                    if w.y.abs() >= 0.5 {
                        self.menu_nav(if w.y > 0.0 { Nav::Up } else { Nav::Down });
                    }
                } else if self.captured
                    && let Some(e) = self.accum.wheel(w.x, w.y)
                {
                    self.send(e);
                }
            }
            SDL_EVENT_GAMEPAD_ADDED => self.pad_added(unsafe { ev.gdevice.which.0 }),
            SDL_EVENT_GAMEPAD_REMOVED => self.pad_removed(unsafe { ev.gdevice.which.0 }),
            SDL_EVENT_GAMEPAD_BUTTON_DOWN | SDL_EVENT_GAMEPAD_BUTTON_UP => {
                let b = unsafe { ev.gbutton };
                self.pad_button(b.which.0, SDL_GamepadButton(b.button as c_int), b.down);
            }
            SDL_EVENT_GAMEPAD_AXIS_MOTION => {
                let a = unsafe { ev.gaxis };
                self.pad_axis(a.which.0, SDL_GamepadAxis(a.axis as c_int), a.value);
            }
            SDL_EVENT_GAMEPAD_SENSOR_UPDATE => {
                let s = unsafe { ev.gsensor };
                self.pad_sensor(s.which.0, s.sensor, s.data);
            }
            _ => {}
        }
    }
}

/// Run the stream window until the stream ends, the person leaves, or (`--measure`) enough
/// frames were shown.
pub fn run(link: StreamLink, opts: Options) -> Result<Option<MeasureReport>> {
    unsafe {
        sdl::SDL_SetHint(sdl::SDL_HINT_APP_ID, c"quiver-stream".as_ptr());
        sdl::SDL_SetHint(sdl::SDL_HINT_QUIT_ON_LAST_WINDOW_CLOSE, c"0".as_ptr());
        sdl::SDL_SetHint(sdl::SDL_HINT_AUDIO_DEVICE_SAMPLE_FRAMES, c"240".as_ptr());
        let mut flags = sdl::SDL_INIT_VIDEO;
        if opts.pads {
            flags |= sdl::SDL_INIT_GAMEPAD;
        }
        if opts.audio {
            flags |= sdl::SDL_INIT_AUDIO;
        }
        if !sdl::SDL_Init(flags) {
            bail!("SDL_Init: {}", sdl_error());
        }
    }
    let result = run_inner(link, opts);
    unsafe { sdl::SDL_Quit() };
    result
}

fn run_inner(link: StreamLink, opts: Options) -> Result<Option<MeasureReport>> {
    let display = unsafe { sdl::SDL_GetPrimaryDisplay() };
    let mode = unsafe { sdl::SDL_GetCurrentDisplayMode(display) };
    let (dw, dh) = if mode.is_null() { (1920, 1080) } else { unsafe { ((*mode).w as u32, (*mode).h as u32) } };
    let (w, h) = opts.size.unwrap_or(if opts.windowed { (dw * 2 / 3, dh * 2 / 3) } else { (dw, dh) });
    let mut wflags = sdl::SDL_WINDOW_VULKAN | sdl::SDL_WINDOW_HIGH_PIXEL_DENSITY | sdl::SDL_WINDOW_RESIZABLE;
    if !opts.windowed {
        wflags |= sdl::SDL_WINDOW_FULLSCREEN;
    }
    let title = CString::new("Quiver stream").unwrap();
    let window = unsafe { sdl::SDL_CreateWindow(title.as_ptr(), w as c_int, h as c_int, wflags) };
    if window.is_null() {
        bail!("SDL_CreateWindow: {}", sdl_error());
    }
    let result = with_window(window, link, opts);
    unsafe { sdl::SDL_DestroyWindow(window) };
    result
}

fn with_window(window: *mut sdl::SDL_Window, link: StreamLink, opts: Options) -> Result<Option<MeasureReport>> {
    // Vulkan: SDL's instance extensions, the surface, then the device that can present to it.
    let mut n = 0u32;
    let names = unsafe { sdl::SDL_Vulkan_GetInstanceExtensions(&mut n) };
    if names.is_null() {
        bail!("SDL_Vulkan_GetInstanceExtensions: {}", sdl_error());
    }
    let exts: Vec<CString> = (0..n as usize).map(|i| unsafe { CStr::from_ptr(*names.add(i) as *const c_char) }.to_owned()).collect();
    let (entry, instance, inst_exts) = Gpu::instance(&exts)?;
    let mut surface = vk::SurfaceKHR::null();
    if !unsafe { sdl::SDL_Vulkan_CreateSurface(window, instance.handle(), std::ptr::null(), &mut surface) } {
        unsafe { instance.destroy_instance(None) };
        bail!("SDL_Vulkan_CreateSurface: {}", sdl_error());
    }
    let gpu = Arc::new(Gpu::new(entry, instance, inst_exts, Some(surface)).context("the Vulkan device")?);
    info!("GPU: {}", gpu.name);
    let (mut pw, mut ph) = (0, 0);
    unsafe { sdl::SDL_GetWindowSizeInPixels(window, &mut pw, &mut ph) };
    let scale = unsafe { sdl::SDL_GetWindowDisplayScale(window) }.clamp(0.5, 4.0);
    let atlas = Arc::new(Atlas::new(scale));
    let shared = Shared::new(
        link.out.clone(),
        PresentPrefs {
            pacing: opts.prefs.pacing,
            mode_override: opts.present_mode,
            hdr: opts.prefs.hdr != Some(false),
            window: (pw.max(1) as u32, ph.max(1) as u32),
        },
        opts.measure.is_some(),
    );
    {
        let mut o = shared.overlay.lock().unwrap();
        o.stats = opts.stats;
        o.waiting = Some("Connecting…".into());
        if opts.intro {
            o.intro_until = Some(Instant::now() + Duration::from_secs(9));
        }
    }
    let conv = Converter::new(gpu.clone())?;
    let presenter = Presenter::new(gpu.clone(), surface, &atlas, conv.reads())?;
    let surface_hdr = presenter.surface_hdr();
    let decode = video::spawn_decode(shared.clone(), gpu.clone(), conv, link.video, opts.decoder)?;
    let present = video::spawn_present(shared.clone(), gpu.clone(), presenter, atlas.clone(), scale)?;

    let playback = if opts.audio {
        match Playback::open(2) {
            Ok(p) => {
                audio::spawn_receiver(link.audio.clone(), p.buffer.clone());
                Some(p)
            }
            Err(e) => {
                warn!("no audio: {e:#}");
                None
            }
        }
    } else {
        None
    };

    // The control channel, waking the event loop.
    let wake = unsafe { SDL_RegisterEvents(1) };
    let (ctx, crx): (_, Receiver<ControlIn>) = unbounded();
    {
        let rx = link.control.clone();
        std::thread::Builder::new().name("control-in".into()).spawn(move || {
            while let Ok(m) = rx.recv() {
                let closed = matches!(m, ControlIn::Closed { .. });
                if ctx.send(m).is_err() {
                    return;
                }
                let mut e: SDL_Event = unsafe { std::mem::zeroed() };
                e.r#type = wake;
                unsafe { SDL_PushEvent(&mut e) };
                if closed {
                    return;
                }
            }
        })?;
    }

    let mut app = App {
        window,
        shared: shared.clone(),
        out: link.out.clone(),
        input_seq: 0,
        captured: false,
        resume_capture: false,
        focused: false,
        ever_focused: false,
        capture_mouse: true,
        host_absolute: false,
        keys: Keys::default(),
        accum: Accum::default(),
        swallow_up: None,
        pads: HashMap::new(),
        prefs: opts.prefs.clone(),
        scale,
        sbs: opts.sbs,
        quit: None,
        cursor: std::ptr::null_mut(),
        menu_pointer: None,
    };
    app.overlay().uncaptured_hint = true;

    // Hello.
    let mut display = app.display_caps();
    display.hdr = surface_hdr;
    let pads: Vec<PadKind> = Vec::new();
    let caps = ClientCaps { decoders: VideoDecoder::probe(&gpu), display, audio_channels: 2, pads, microphone: false };
    app.control(Control::Hello { version: PROTOCOL_VERSION, caps, prefs: opts.prefs.clone(), target: opts.target.clone() });

    let start = Instant::now();
    let mut next_ping = Instant::now();
    let mut next_stats = Instant::now() + Duration::from_secs(1);
    let mut last_stats = Instant::now();
    let mut next_pad = Instant::now();
    let mut ping_id = 0u32;
    let mut measure_done_at: Option<Instant> = None;
    let mut ev: SDL_Event = unsafe { std::mem::zeroed() };
    loop {
        let now = Instant::now();
        let chord_pending = app.pads.values().any(|p| p.chord.pending());
        let mut due = next_ping.min(next_stats).min(next_pad);
        if chord_pending {
            due = due.min(now + Duration::from_millis(50));
        }
        let timeout = due.saturating_duration_since(now).as_millis().clamp(1, 500) as i32;
        if unsafe { SDL_WaitEventTimeout(&mut ev, timeout) } {
            app.event(&ev);
            while unsafe { SDL_PollEvent(&mut ev) } {
                app.event(&ev);
            }
        }
        app.flush_pads(false);
        while let Ok(m) = crx.try_recv() {
            app.handle_control(m, &playback);
        }
        let now = Instant::now();
        if chord_pending {
            let mut fire = false;
            for p in app.pads.values_mut() {
                fire |= p.chord.update(p.state.buttons.0, now);
            }
            if fire {
                app.open_menu(0);
            }
        }
        if now >= next_ping {
            next_ping = now + Duration::from_millis(500);
            ping_id = ping_id.wrapping_add(1);
            app.control(Control::Ping { id: ping_id, sent: now_micros() });
        }
        if now >= next_pad {
            next_pad = now + Duration::from_millis(200);
            if app.captured {
                app.flush_pads(true);
            }
        }
        if now >= next_stats {
            next_stats = now + Duration::from_secs(1);
            let secs = last_stats.elapsed().as_secs_f32();
            last_stats = now;
            let rtt = shared.clock.lock().unwrap().rtt_us();
            let period = shared.stats.period(secs, rtt);
            if let Some(tx) = &opts.periods {
                let _ = tx.send(period.clone());
            }
            let crate::stats::Period { client, fps, kbps, loss_percent: loss, .. } = period;
            if let Some(p) = &playback {
                let jb = p.buffer.lock().unwrap();
                let audio_ms = (jb.depth_us() + p.device_us()) as f32 / 1000.0;
                drop(jb);
                // A/V sync: audio waits as long as video spends on this side.
                let floor = (client.decode_us + client.present_us).saturating_sub(p.device_us());
                p.buffer.lock().unwrap().set_floor(floor);
                app.overlay().snapshot.audio_ms = audio_ms;
            }
            {
                let mut o = app.overlay();
                o.snapshot.client = client.clone();
                o.snapshot.fps = fps;
                o.snapshot.kbps = kbps;
                o.snapshot.loss_percent = loss;
                o.snapshot.decoder = shared.decoder_name.lock().unwrap().clone();
                o.snapshot.present_mode = shared.present_name.lock().unwrap().clone();
            }
            app.control(Control::Stats(client));
            shared.redraw();
        }
        if let Some(n) = opts.measure {
            if measure_done_at.is_none() && shared.frames_shown.load(Ordering::Relaxed) >= n {
                measure_done_at = Some(now);
            }
            // Give the last present waits time to land.
            if measure_done_at.is_some_and(|t| now.duration_since(t) > Duration::from_millis(400)) {
                app.quit = Some("measured".into());
            }
        }
        if opts.duration.is_some_and(|d| start.elapsed() >= d) {
            app.quit = Some("time is up".into());
        }
        if shared.quit.load(Ordering::SeqCst) && app.quit.is_none() {
            app.quit = Some("the video pipeline stopped".into());
        }
        if let Some(reason) = app.quit.take() {
            info!("stream ending: {reason}");
            break;
        }
    }
    app.set_captured(false);
    let report = video::measure_report(&shared);
    shared.stop();
    let _ = present.join();
    let _ = decode.join();
    drop(playback);
    for p in app.pads.values() {
        unsafe { SDL_CloseGamepad(p.gamepad) };
    }
    Ok(report)
}
