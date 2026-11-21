//! The client's pads as uinput Xbox 360 pads ([`quiver_pad::uinput`]), with rumble sent back.
//!
//! Where they may appear is [`crate::seat::decide`]'s call: hidden by the stream pad rule (the
//! default wherever a person is seated), plain on an opted-in headless server, or not at all.
//! Hidden pads are checked like the lab's: a canary device first, which the rule must be seen
//! to have hidden before any real pad is made; every node after udev; and every node again
//! every 5 s. A node that turns out openable by this process (and so by the desktop) gets every
//! pad destroyed at once.
//!
//! A pad thread owns the devices: it applies the client's pad events as evdev frames, answers
//! the game's force-feedback uploads (a game's upload blocks until answered), and turns what
//! the game plays into [`Control::Rumble`].

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_pad::model::{Axis, Button, Change};
use quiver_pad::uinput::Pad;
use quiver_stream_proto::{Control, InputEvent, PadButtons, PadState};
use tokio::sync::mpsc;

use crate::sandbox;
use crate::seat::PadMode;

pub const MAX_PADS: u8 = 4;

/// The protocol's XInput-shaped state as the uinput pad's.
pub fn to_pad_state(s: &PadState) -> quiver_pad::PadState {
    let mut p = quiver_pad::PadState::default();
    let map = [
        (PadButtons::DPAD_UP, Button::Up),
        (PadButtons::DPAD_DOWN, Button::Down),
        (PadButtons::DPAD_LEFT, Button::Left),
        (PadButtons::DPAD_RIGHT, Button::Right),
        (PadButtons::START, Button::Start),
        (PadButtons::BACK, Button::Back),
        (PadButtons::LEFT_THUMB, Button::Ls),
        (PadButtons::RIGHT_THUMB, Button::Rs),
        (PadButtons::LEFT_SHOULDER, Button::Lb),
        (PadButtons::RIGHT_SHOULDER, Button::Rb),
        (PadButtons::GUIDE, Button::Guide),
        (PadButtons::A, Button::A),
        (PadButtons::B, Button::B),
        (PadButtons::X, Button::X),
        (PadButtons::Y, Button::Y),
    ];
    for (bit, b) in map {
        if s.buttons.has(bit) {
            p.apply(&Change::Press { button: b });
        }
    }
    let stick = |v: i16| (v as f32 / 32767.0).clamp(-1.0, 1.0);
    for (axis, v) in [(Axis::Lx, stick(s.lx)), (Axis::Ly, stick(s.ly)), (Axis::Rx, stick(s.rx)), (Axis::Ry, stick(s.ry)), (Axis::Lt, s.lt as f32 / 255.0), (Axis::Rt, s.rt as f32 / 255.0)] {
        p.apply(&Change::Axis { axis, value: v });
    }
    p
}

/// What the game's last rumble means for the client: (low, high, duration ms), or all zero to
/// stop. An effect without a length lasts until stopped; it is sent for a second at a time
/// and refreshed.
pub fn rumble_message(r: &quiver_pad::uinput::Rumble) -> (u16, u16, u16) {
    if !r.playing {
        return (0, 0, 0);
    }
    let lv = |f: f32| (f.clamp(0.0, 1.0) * 65535.0).round() as u16;
    (lv(r.strong), lv(r.weak), if r.length_ms == 0 { 1000 } else { r.length_ms })
}

enum Cmd {
    Event(InputEvent),
    Quit,
}

/// The pad thread.
pub struct PadHost {
    tx: std_mpsc::Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub mode: PadMode,
}

struct Slot {
    pad: Pad,
    nodes: Vec<PathBuf>,
    sent: (u16, u16, u16),
    sent_at: Instant,
}

impl PadHost {
    /// Start the pad thread. For hidden pads, first prove the rule hides a canary device.
    pub fn start(mode: PadMode, session: &str, control: mpsc::Sender<Control>) -> Result<PadHost> {
        let uinput = Path::new("/dev/uinput");
        if let PadMode::Hidden { gid } = mode {
            let canary = Pad::canary(uinput, &format!("{}{session} canary", sandbox::NAME_PREFIX), &format!("{}{session}/canary", sandbox::PHYS_PREFIX))?;
            let n = canary.nodes()?;
            sandbox::wait_hidden(&[n.event.clone()], gid, Duration::from_secs(3)).context("the stream pad rule didn't hide the canary device")?;
        }
        let (tx, rx) = std_mpsc::channel();
        let session = session.to_owned();
        let m = mode.clone();
        let thread = std::thread::Builder::new().name("pads".into()).spawn(move || pad_thread(m, session, rx, control))?;
        Ok(PadHost { tx, thread: Some(thread), mode })
    }

    pub fn send(&self, ev: InputEvent) {
        let _ = self.tx.send(Cmd::Event(ev));
    }
}

impl Drop for PadHost {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn make(mode: &PadMode, session: &str, k: u8) -> Result<Slot> {
    let pad = Pad::create(Path::new("/dev/uinput"), &sandbox::pad_name(session, k), &sandbox::pad_phys(session, k))?;
    let n = pad.nodes()?;
    let nodes: Vec<PathBuf> = std::iter::once(n.event).chain(n.js).collect();
    if let PadMode::Hidden { gid } = mode
        && let Err(e) = sandbox::wait_hidden(&nodes, *gid, Duration::from_secs(3))
    {
        drop(pad);
        bail!("pad {}: {e:#}", k + 1);
    }
    Ok(Slot { pad, nodes, sent: (0, 0, 0), sent_at: Instant::now() })
}

fn pad_thread(mode: PadMode, session: String, rx: std_mpsc::Receiver<Cmd>, control: mpsc::Sender<Control>) {
    let mut slots: Vec<Option<Slot>> = (0..MAX_PADS).map(|_| None).collect();
    let notice = |text: String| {
        tracing::warn!("{text}");
        let _ = control.try_send(Control::Notice { text });
    };
    let mut last_check = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(Cmd::Quit) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Cmd::Event(ev)) => match ev {
                InputEvent::PadArrive { slot, .. } if slot < MAX_PADS => {
                    if let PadMode::Off(why) = &mode {
                        notice(format!("pads aren't available: {why}"));
                    } else if slots[slot as usize].is_none() {
                        match make(&mode, &session, slot) {
                            Ok(s) => slots[slot as usize] = Some(s),
                            Err(e) => notice(format!("{e:#}")),
                        }
                    }
                }
                InputEvent::PadLeave { slot } if slot < MAX_PADS => slots[slot as usize] = None,
                InputEvent::Pad { slot, state } if slot < MAX_PADS => {
                    if let Some(s) = slots[slot as usize].as_mut()
                        && let Err(e) = s.pad.set(to_pad_state(&state))
                    {
                        tracing::warn!("pad {slot}: {e:#}");
                    }
                }
                InputEvent::ReleaseAll => {
                    for s in slots.iter_mut().flatten() {
                        let _ = s.pad.set(quiver_pad::PadState::default());
                    }
                }
                _ => {}
            },
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
        for (k, s) in slots.iter_mut().enumerate() {
            let Some(s) = s else { continue };
            s.pad.service();
            let m = rumble_message(&s.pad.rumble);
            let refresh = m.2 == 1000 && s.sent_at.elapsed() > Duration::from_millis(500);
            if m != s.sent || (m.0 | m.1 != 0 && refresh) {
                s.sent = m;
                s.sent_at = Instant::now();
                let _ = control.try_send(Control::Rumble { slot: k as u8, low: m.0, high: m.1, duration_ms: m.2 });
            }
        }
        if let PadMode::Hidden { gid } = mode
            && last_check.elapsed() > Duration::from_secs(5)
        {
            last_check = Instant::now();
            let bad = slots.iter().flatten().flat_map(|s| s.nodes.iter()).find_map(|n| match sandbox::node_state(n) {
                Ok(st) => sandbox::problem(&st, gid).map(|p| format!("{}: {p}", n.display())),
                Err(e) => Some(format!("{e:#}")),
            });
            if let Some(p) = bad {
                slots.iter_mut().for_each(|s| *s = None);
                notice(format!("stream pads removed: {p}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xinput_state_maps_onto_the_360_pad() {
        let s = PadState { buttons: PadButtons(PadButtons::A | PadButtons::DPAD_UP | PadButtons::GUIDE | PadButtons::PADDLE1), lx: 32767, ly: -32768, rx: 0, ry: 16384, lt: 255, rt: 0 };
        let p = to_pad_state(&s);
        assert!(p.pressed(Button::A) && p.pressed(Button::Up) && p.pressed(Button::Guide));
        assert!(!p.pressed(Button::B));
        assert_eq!((p.lx, p.ly, p.lt, p.rt), (1.0, -1.0, 1.0, 0.0));
        assert!((p.ry - 0.5).abs() < 0.001);
        // What a game reads back through XInput.
        let (b, lt, _, lx, ly, _, _) = p.xinput();
        assert_eq!(b, 0x1000 | 0x0001 | 0x0400);
        assert_eq!((lt, lx), (255, 32767));
        assert!(ly <= -32767);
    }

    #[test]
    fn rumble_messages() {
        let mut r = quiver_pad::uinput::Rumble { strong: 1.0, weak: 0.5, length_ms: 200, playing: true, ..Default::default() };
        assert_eq!(rumble_message(&r), (65535, 32768, 200));
        r.length_ms = 0;
        assert_eq!(rumble_message(&r).2, 1000);
        r.playing = false;
        assert_eq!(rumble_message(&r), (0, 0, 0));
    }
}
