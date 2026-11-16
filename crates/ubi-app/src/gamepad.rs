//! Gamepad input through gilrs (evdev, read-only: devices are never grabbed, so hyprpad and
//! Steam Input keep working). Buttons map to the same [`Input`]s the keyboard produces.
//! The Guide button is ignored entirely: hyprpad owns it.
//!
//! The mapping, stick dead zones and hold-to-repeat are pure and unit-tested; only
//! [`spawn`] touches hardware.

use std::time::{Duration, Instant};

use gilrs::{Axis, Button};

use crate::nav::Input;

#[derive(Clone, Debug, PartialEq)]
pub enum PadEvent {
    Input(Input),
    /// Right stick: scroll by this many pixels (positive = down).
    Scroll(f32),
    Connected(String),
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    pub fn input(self) -> Input {
        match self {
            Dir::Up => Input::Up,
            Dir::Down => Input::Down,
            Dir::Left => Input::Left,
            Dir::Right => Input::Right,
        }
    }
}

/// What a button press means. D-pad directions go through [`Repeat`] instead.
pub fn map_button(b: Button) -> Option<Input> {
    Some(match b {
        Button::South => Input::Activate,
        Button::East => Input::Back,
        Button::North => Input::Menu,
        Button::West => Input::PlayOptions,
        Button::LeftTrigger => Input::PrevSection,
        Button::RightTrigger => Input::NextSection,
        Button::LeftTrigger2 => Input::PrevTab,
        Button::RightTrigger2 => Input::NextTab,
        Button::Start => Input::Palette,
        // Guide belongs to hyprpad. Select, stick clicks and the rest are unused for now.
        _ => return None,
    })
}

pub fn dpad_dir(b: Button) -> Option<Dir> {
    match b {
        Button::DPadUp => Some(Dir::Up),
        Button::DPadDown => Some(Dir::Down),
        Button::DPadLeft => Some(Dir::Left),
        Button::DPadRight => Some(Dir::Right),
        _ => None,
    }
}

/// Stick position -> direction, with hysteresis: a direction engages past `ENGAGE` and
/// holds until the stick falls back under `RELEASE`, so a stick resting near the edge of
/// the dead zone doesn't chatter.
pub const ENGAGE: f32 = 0.6;
pub const RELEASE: f32 = 0.35;

pub fn stick_dir(x: f32, y: f32, current: Option<Dir>) -> Option<Dir> {
    // gilrs: +y is up.
    let (ax, ay) = (x.abs(), y.abs());
    let along = |d: Dir| match d {
        Dir::Up => y,
        Dir::Down => -y,
        Dir::Left => -x,
        Dir::Right => x,
    };
    if let Some(d) = current {
        if along(d) >= RELEASE && (ax.max(ay) < ENGAGE || along(d) >= ax.max(ay) * 0.7) {
            return Some(d);
        }
    }
    if ax.max(ay) < ENGAGE {
        return None;
    }
    Some(if ax > ay {
        if x > 0.0 { Dir::Right } else { Dir::Left }
    } else if y > 0.0 {
        Dir::Up
    } else {
        Dir::Down
    })
}

/// Hold-to-repeat for one held direction: fire on press, again after `DELAY`, then every
/// `INTERVAL`.
#[derive(Debug, Default)]
pub struct Repeat {
    held: Option<(Dir, Instant)>,
    fired: u32,
}

pub const DELAY: Duration = Duration::from_millis(380);
pub const INTERVAL: Duration = Duration::from_millis(110);

impl Repeat {
    /// Set the held direction. Returns a direction to fire immediately on a new press.
    pub fn set(&mut self, dir: Option<Dir>, now: Instant) -> Option<Dir> {
        match (self.held, dir) {
            (Some((h, _)), Some(d)) if h == d => None,
            (_, Some(d)) => {
                self.held = Some((d, now));
                self.fired = 0;
                Some(d)
            }
            (_, None) => {
                self.held = None;
                None
            }
        }
    }

    pub fn held(&self) -> Option<Dir> {
        self.held.map(|(d, _)| d)
    }

    /// When the next repeat is due, if a direction is held.
    pub fn next_due(&self) -> Option<Instant> {
        self.held.map(|(_, since)| since + DELAY + INTERVAL * self.fired)
    }

    /// Fire a repeat if one is due.
    pub fn poll(&mut self, now: Instant) -> Option<Dir> {
        let due = self.next_due()?;
        if now >= due {
            self.fired += 1;
            self.held()
        } else {
            None
        }
    }
}

/// Right-stick scrolling speed at full deflection, in pixels per 16 ms tick.
const SCROLL_SPEED: f32 = 28.0;
const SCROLL_DEAD: f32 = 0.2;

pub fn scroll_step(y: f32) -> Option<f32> {
    if y.abs() < SCROLL_DEAD {
        return None;
    }
    // Ease in: small deflections scroll slowly. Stick up (+y) scrolls up (negative).
    let t = (y.abs() - SCROLL_DEAD) / (1.0 - SCROLL_DEAD);
    Some(-y.signum() * t * t * SCROLL_SPEED)
}

/// Combines the D-pad and the left stick into one repeating direction.
#[derive(Debug, Default)]
pub struct Directions {
    dpad: Vec<Dir>,
    stick: Option<Dir>,
    repeat: Repeat,
}

impl Directions {
    fn current(&self) -> Option<Dir> {
        self.dpad.last().copied().or(self.stick)
    }

    pub fn dpad(&mut self, d: Dir, pressed: bool, now: Instant) -> Option<Dir> {
        self.dpad.retain(|x| *x != d);
        if pressed {
            self.dpad.push(d);
        }
        let cur = self.current();
        self.repeat.set(cur, now)
    }

    pub fn stick(&mut self, x: f32, y: f32, now: Instant) -> Option<Dir> {
        self.stick = stick_dir(x, y, self.stick);
        let cur = self.current();
        self.repeat.set(cur, now)
    }

    pub fn poll(&mut self, now: Instant) -> Option<Dir> {
        self.repeat.poll(now)
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.repeat.next_due()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Read gamepads on a background thread and hand events to `send`. Returns immediately.
/// If gilrs can't start (no udev, no permission), the app simply has no gamepad input.
pub fn spawn(send: impl Fn(PadEvent) + Send + 'static) {
    std::thread::Builder::new()
        .name("gamepad".into())
        .spawn(move || {
            let mut gilrs = match gilrs::GilrsBuilder::new().with_default_filters(false).build() {
                Ok(g) => g,
                Err(e) => {
                    tracing::info!("no gamepad support: {e}");
                    return;
                }
            };
            for (_, pad) in gilrs.gamepads() {
                send(PadEvent::Connected(pad.name().to_owned()));
            }
            let mut dirs = Directions::default();
            let (mut lx, mut ly, mut ry) = (0.0f32, 0.0f32, 0.0f32);
            loop {
                let now = Instant::now();
                let mut timeout = Duration::from_millis(250);
                if let Some(due) = dirs.next_due() {
                    timeout = timeout.min(due.saturating_duration_since(now));
                }
                if scroll_step(ry).is_some() {
                    timeout = timeout.min(Duration::from_millis(16));
                }
                let ev = gilrs.next_event_blocking(Some(timeout));
                let now = Instant::now();
                if let Some(ev) = ev {
                    use gilrs::EventType::*;
                    match ev.event {
                        ButtonPressed(b, _) => {
                            if let Some(d) = dpad_dir(b) {
                                if let Some(d) = dirs.dpad(d, true, now) {
                                    send(PadEvent::Input(d.input()));
                                }
                            } else if let Some(i) = map_button(b) {
                                send(PadEvent::Input(i));
                            }
                        }
                        ButtonReleased(b, _) => {
                            if let Some(d) = dpad_dir(b) {
                                dirs.dpad(d, false, now);
                            }
                        }
                        AxisChanged(axis, v, _) => match axis {
                            Axis::LeftStickX | Axis::LeftStickY => {
                                if axis == Axis::LeftStickX {
                                    lx = v
                                } else {
                                    ly = v
                                }
                                if let Some(d) = dirs.stick(lx, ly, now) {
                                    send(PadEvent::Input(d.input()));
                                }
                            }
                            Axis::RightStickY => ry = v,
                            // Some pads report the D-pad as a hat axis.
                            Axis::DPadX | Axis::DPadY => {
                                let (x, y) = if axis == Axis::DPadX { (v, 0.0) } else { (0.0, v) };
                                if let Some(d) = dirs.stick(x, y, now) {
                                    send(PadEvent::Input(d.input()));
                                }
                            }
                            _ => {}
                        },
                        Connected => {
                            let name = gilrs.gamepad(ev.id).name().to_owned();
                            send(PadEvent::Connected(name));
                        }
                        Disconnected => {
                            dirs.clear();
                            send(PadEvent::Disconnected);
                        }
                        _ => {}
                    }
                }
                if let Some(d) = dirs.poll(now) {
                    send(PadEvent::Input(d.input()));
                }
                if let Some(dy) = scroll_step(ry) {
                    send(PadEvent::Scroll(dy));
                }
            }
        })
        .expect("spawn gamepad thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_like_the_proposal() {
        assert_eq!(map_button(Button::South), Some(Input::Activate));
        assert_eq!(map_button(Button::East), Some(Input::Back));
        assert_eq!(map_button(Button::North), Some(Input::Menu));
        assert_eq!(map_button(Button::West), Some(Input::PlayOptions));
        assert_eq!(map_button(Button::LeftTrigger), Some(Input::PrevSection));
        assert_eq!(map_button(Button::RightTrigger), Some(Input::NextSection));
        assert_eq!(map_button(Button::LeftTrigger2), Some(Input::PrevTab));
        assert_eq!(map_button(Button::RightTrigger2), Some(Input::NextTab));
        assert_eq!(map_button(Button::Start), Some(Input::Palette));
        // Never the Guide button.
        assert_eq!(map_button(Button::Mode), None);
        assert_eq!(dpad_dir(Button::Mode), None);
        assert_eq!(dpad_dir(Button::DPadLeft), Some(Dir::Left));
    }

    #[test]
    fn stick_has_dead_zone_and_hysteresis() {
        assert_eq!(stick_dir(0.3, 0.1, None), None);
        assert_eq!(stick_dir(0.7, 0.1, None), Some(Dir::Right));
        assert_eq!(stick_dir(-0.1, -0.9, None), Some(Dir::Down));
        assert_eq!(stick_dir(0.1, 0.9, None), Some(Dir::Up));
        // Held right: drifting back to 0.45 keeps it, under 0.35 releases.
        assert_eq!(stick_dir(0.45, 0.0, Some(Dir::Right)), Some(Dir::Right));
        assert_eq!(stick_dir(0.3, 0.0, Some(Dir::Right)), None);
        // A clear turn to another direction switches.
        assert_eq!(stick_dir(0.1, 0.95, Some(Dir::Right)), Some(Dir::Up));
    }

    #[test]
    fn repeat_fires_on_press_then_after_delay_then_every_interval() {
        let t0 = Instant::now();
        let mut r = Repeat::default();
        assert_eq!(r.set(Some(Dir::Down), t0), Some(Dir::Down));
        // Same direction again is not a new press.
        assert_eq!(r.set(Some(Dir::Down), t0 + Duration::from_millis(10)), None);
        assert_eq!(r.poll(t0 + DELAY - Duration::from_millis(1)), None);
        assert_eq!(r.poll(t0 + DELAY), Some(Dir::Down));
        assert_eq!(r.poll(t0 + DELAY + Duration::from_millis(50)), None);
        assert_eq!(r.poll(t0 + DELAY + INTERVAL), Some(Dir::Down));
        assert_eq!(r.set(None, t0 + Duration::from_secs(1)), None);
        assert_eq!(r.poll(t0 + Duration::from_secs(5)), None);
    }

    #[test]
    fn dpad_and_stick_share_one_direction() {
        let t0 = Instant::now();
        let mut d = Directions::default();
        assert_eq!(d.dpad(Dir::Left, true, t0), Some(Dir::Left));
        // Pressing a second D-pad direction takes over; releasing it returns to the first.
        assert_eq!(d.dpad(Dir::Up, true, t0), Some(Dir::Up));
        assert_eq!(d.dpad(Dir::Up, false, t0), Some(Dir::Left));
        assert_eq!(d.dpad(Dir::Left, false, t0), None);
        assert_eq!(d.stick(0.0, -0.8, t0), Some(Dir::Down));
        assert_eq!(d.stick(0.0, -0.9, t0), None);
        assert_eq!(d.stick(0.0, 0.0, t0), None);
        assert_eq!(d.poll(t0 + Duration::from_secs(2)), None);
    }

    #[test]
    fn right_stick_scrolls_with_ease_in() {
        assert_eq!(scroll_step(0.1), None);
        let slow = scroll_step(-0.4).unwrap();
        let fast = scroll_step(-1.0).unwrap();
        assert!(slow > 0.0 && fast > slow);
        assert!(scroll_step(1.0).unwrap() < 0.0);
    }
}
