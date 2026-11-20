//! A uinput device laid out as an Xbox 360 controller ([`crate::model`]), with rumble.
//!
//! The device lives as long as its file descriptor: when the process holding it exits, for
//! any reason, the kernel destroys it. That is what keeps a crashed pad host from leaving pads
//! behind.
//!
//! Rumble: a game's force-feedback upload (EVIOCSFF on the event node) blocks until the uinput
//! owner answers it, so [`Pad::service`] must run whenever the fd is readable. Uploads and
//! erases are acknowledged, and plays are recorded ([`Rumble`]) for `quiver lab pad status`.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{self, ABS_AXES, BUS_USB, EV_ABS, EV_FF, EV_KEY, EV_SYN, FF_RUMBLE, KEYS, PRODUCT, PadState, VENDOR, VERSION};

const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((b'U' as u64) << 8) | nr
}
const W: u64 = 1;
const R: u64 = 2;
const UI_DEV_CREATE: u64 = ioc(0, 1, 0);
const UI_DEV_SETUP: u64 = ioc(W, 3, std::mem::size_of::<UinputSetup>() as u64);
const UI_ABS_SETUP: u64 = ioc(W, 4, std::mem::size_of::<UinputAbsSetup>() as u64);
const UI_SET_EVBIT: u64 = ioc(W, 100, 4);
const UI_SET_KEYBIT: u64 = ioc(W, 101, 4);
const UI_SET_ABSBIT: u64 = ioc(W, 103, 4);
const UI_SET_FFBIT: u64 = ioc(W, 107, 4);
const UI_SET_PHYS: u64 = ioc(W, 108, std::mem::size_of::<*const u8>() as u64);
const UI_BEGIN_FF_UPLOAD: u64 = ioc(R | W, 200, std::mem::size_of::<UinputFfUpload>() as u64);
const UI_END_FF_UPLOAD: u64 = ioc(W, 201, std::mem::size_of::<UinputFfUpload>() as u64);
const UI_BEGIN_FF_ERASE: u64 = ioc(R | W, 202, std::mem::size_of::<UinputFfErase>() as u64);
const UI_END_FF_ERASE: u64 = ioc(W, 203, std::mem::size_of::<UinputFfErase>() as u64);
const fn ui_get_sysname(len: u64) -> u64 {
    ioc(R, 44, len)
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

/// `struct ff_effect`: type, id, direction, trigger, replay, then a union whose largest
/// member (ff_periodic_effect) holds a pointer.
#[repr(C)]
#[derive(Clone, Copy)]
struct FfEffect {
    kind: u16,
    id: i16,
    direction: u16,
    trigger_button: u16,
    trigger_interval: u16,
    replay_length: u16,
    replay_delay: u16,
    u: [u64; 4],
}

#[repr(C)]
struct UinputFfUpload {
    request_id: u32,
    retval: i32,
    effect: FfEffect,
    old: FfEffect,
}

#[repr(C)]
struct UinputFfErase {
    request_id: u32,
    retval: i32,
    effect_id: u32,
}

#[repr(C)]
struct InputEvent {
    sec: libc::time_t,
    usec: libc::suseconds_t,
    kind: u16,
    code: u16,
    value: i32,
}

fn ioctl(fd: RawFd, req: u64, arg: usize) -> std::io::Result<i32> {
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r) }
}

/// The last rumble the game asked for.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Rumble {
    /// 0..1 each.
    pub strong: f32,
    pub weak: f32,
    /// The effect's length, ms (0: until stopped).
    pub length_ms: u16,
    pub playing: bool,
    pub uploads: u64,
    pub plays: u64,
    /// Unix ms of the last play or stop.
    pub at_ms: u64,
}

/// Where a pad is in the host's /dev and /sys.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Nodes {
    /// `input17`
    pub sysname: String,
    /// `/dev/input/event23`
    pub event: PathBuf,
    /// `/dev/input/js3` (joydev), when the kernel made one.
    pub js: Option<PathBuf>,
}

pub struct Pad {
    file: File,
    pub name: String,
    pub phys: String,
    pub state: PadState,
    pub rumble: Rumble,
    effects: Vec<(i16, f32, f32, u16)>,
}

impl Pad {
    /// Make the device (it appears at once; the caller checks udev hid it, [`crate::rule`]).
    pub fn create(uinput: &Path, name: &str, phys: &str) -> Result<Pad> {
        Pad::make(uinput, name, phys, true)
    }

    /// A device with the same name and phys but no keys, axes or rumble: not a joystick, so no
    /// rule tags it `uaccess` and nothing on the host binds it. The pad host makes one first and
    /// requires the lab pad rule to have run on it before it makes a real pad, so a rule that
    /// doesn't match never meets a joystick.
    pub fn canary(uinput: &Path, name: &str, phys: &str) -> Result<Pad> {
        Pad::make(uinput, name, phys, false)
    }

    fn make(uinput: &Path, name: &str, phys: &str, gamepad: bool) -> Result<Pad> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(uinput)
            .with_context(|| format!("open {} (needs your ACL on it, as Steam's udev rule gives)", uinput.display()))?;
        let fd = file.as_raw_fd();
        ioctl(fd, UI_SET_EVBIT, EV_SYN as usize)?;
        if gamepad {
            for ev in [EV_KEY, EV_ABS, EV_FF] {
                ioctl(fd, UI_SET_EVBIT, ev as usize)?;
            }
            for k in KEYS {
                ioctl(fd, UI_SET_KEYBIT, k as usize)?;
            }
            ioctl(fd, UI_SET_FFBIT, FF_RUMBLE as usize)?;
            for (code, min, max, fuzz, flat) in ABS_AXES {
                ioctl(fd, UI_SET_ABSBIT, code as usize)?;
                let a = UinputAbsSetup { code, absinfo: InputAbsinfo { value: 0, minimum: min, maximum: max, fuzz, flat, resolution: 0 } };
                ioctl(fd, UI_ABS_SETUP, &a as *const _ as usize).context("UI_ABS_SETUP")?;
            }
        }
        let c = CString::new(phys)?;
        ioctl(fd, UI_SET_PHYS, c.as_ptr() as usize).context("UI_SET_PHYS")?;
        // The canary isn't an Xbox pad either (no VID/PID a hwdb or Steam rule could match).
        let id = if gamepad { InputId { bustype: BUS_USB, vendor: VENDOR, product: PRODUCT, version: VERSION } } else { InputId { bustype: 0x06, vendor: 0, product: 0, version: 0 } };
        let mut setup = UinputSetup { id, name: [0; 80], ff_effects_max: if gamepad { 16 } else { 0 } };
        let n = name.as_bytes();
        setup.name[..n.len().min(79)].copy_from_slice(&n[..n.len().min(79)]);
        ioctl(fd, UI_DEV_SETUP, &setup as *const _ as usize).context("UI_DEV_SETUP")?;
        ioctl(fd, UI_DEV_CREATE, 0).context("UI_DEV_CREATE")?;
        Ok(Pad { file, name: name.to_owned(), phys: phys.to_owned(), state: PadState::default(), rumble: Rumble::default(), effects: Vec::new() })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// `inputN`, and its event and js nodes from sysfs.
    pub fn nodes(&self) -> Result<Nodes> {
        let mut buf = [0u8; 64];
        ioctl(self.fd(), ui_get_sysname(buf.len() as u64), buf.as_mut_ptr() as usize).context("UI_GET_SYSNAME")?;
        let sysname = String::from_utf8_lossy(&buf[..buf.iter().position(|b| *b == 0).unwrap_or(buf.len())]).into_owned();
        nodes_of(Path::new("/sys/devices/virtual/input"), &sysname)
    }

    /// Move to `next`, writing only what changed. Returns how many events were written.
    pub fn set(&mut self, next: PadState) -> Result<usize> {
        let ev = next.events_from(&self.state);
        if ev.is_empty() {
            return Ok(0);
        }
        let mut bytes = Vec::with_capacity(ev.len() * std::mem::size_of::<InputEvent>());
        for (kind, code, value) in &ev {
            let e = InputEvent { sec: 0, usec: 0, kind: *kind, code: *code, value: *value };
            bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(&e as *const _ as *const u8, std::mem::size_of::<InputEvent>()) });
        }
        (&self.file).write_all(&bytes).context("write to uinput")?;
        self.state = next;
        Ok(ev.len())
    }

    /// Answer force-feedback requests and record plays. Call when the fd is readable.
    pub fn service(&mut self) {
        let mut buf = [0u8; std::mem::size_of::<InputEvent>() * 16];
        loop {
            let n = match (&self.file).read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            for chunk in buf[..n].chunks_exact(std::mem::size_of::<InputEvent>()) {
                let e: InputEvent = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
                match (e.kind, e.code) {
                    (EV_UINPUT, UI_FF_UPLOAD) => self.upload(e.value as u32),
                    (EV_UINPUT, UI_FF_ERASE) => self.erase(e.value as u32),
                    (EV_FF, id) => self.play(id as i16, e.value),
                    _ => {}
                }
            }
        }
    }

    fn upload(&mut self, request_id: u32) {
        let mut up: UinputFfUpload = unsafe { std::mem::zeroed() };
        up.request_id = request_id;
        if ioctl(self.fd(), UI_BEGIN_FF_UPLOAD, &mut up as *mut _ as usize).is_err() {
            return;
        }
        if up.effect.kind == FF_RUMBLE {
            // ff_rumble_effect: strong_magnitude, weak_magnitude (u16 each), first in the union.
            let strong = (up.effect.u[0] & 0xffff) as f32 / 65535.0;
            let weak = ((up.effect.u[0] >> 16) & 0xffff) as f32 / 65535.0;
            self.effects.retain(|(id, ..)| *id != up.effect.id);
            self.effects.push((up.effect.id, strong, weak, up.effect.replay_length));
            up.retval = 0;
        } else {
            up.retval = -libc::EINVAL;
        }
        self.rumble.uploads += 1;
        let _ = ioctl(self.fd(), UI_END_FF_UPLOAD, &up as *const _ as usize);
    }

    fn erase(&mut self, request_id: u32) {
        let mut er = UinputFfErase { request_id, retval: 0, effect_id: 0 };
        if ioctl(self.fd(), UI_BEGIN_FF_ERASE, &mut er as *mut _ as usize).is_err() {
            return;
        }
        self.effects.retain(|(id, ..)| *id as u32 != er.effect_id);
        er.retval = 0;
        let _ = ioctl(self.fd(), UI_END_FF_ERASE, &er as *const _ as usize);
    }

    fn play(&mut self, id: i16, value: i32) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        if let Some((_, strong, weak, len)) = self.effects.iter().find(|(eid, ..)| *eid == id) {
            self.rumble.strong = *strong;
            self.rumble.weak = *weak;
            self.rumble.length_ms = *len;
        }
        self.rumble.playing = value > 0;
        self.rumble.plays += (value > 0) as u64;
        self.rumble.at_ms = now;
    }
}

/// The event and js nodes under `<root>/<sysname>/` (`/sys/devices/virtual/input/input17`).
pub fn nodes_of(root: &Path, sysname: &str) -> Result<Nodes> {
    let dir = root.join(sysname);
    let mut event = None;
    let mut js = None;
    for e in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("event") && n[5..].bytes().all(|b| b.is_ascii_digit()) {
            event = Some(PathBuf::from("/dev/input").join(&n));
        } else if n.starts_with("js") && n[2..].bytes().all(|b| b.is_ascii_digit()) {
            js = Some(PathBuf::from("/dev/input").join(&n));
        }
    }
    let Some(event) = event else { bail!("{} has no event node", dir.display()) };
    Ok(Nodes { sysname: sysname.to_owned(), event, js })
}

/// Lab pads present on this host (from sysfs): (sysname, phys, name).
pub fn present(root: &Path) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.starts_with("input") {
            continue;
        }
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default().trim().to_owned();
        let phys = read("phys");
        if phys.starts_with(model::PHYS_PREFIX) {
            out.push((n, phys, read("name")));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_kernel() {
        // x86_64 Linux: linux/uinput.h and linux/input.h.
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        assert_eq!(std::mem::size_of::<FfEffect>(), 48);
        assert_eq!(std::mem::size_of::<UinputFfUpload>(), 104);
        assert_eq!(std::mem::size_of::<UinputFfErase>(), 12);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
        assert_eq!(UI_ABS_SETUP, 0x401c5504);
        assert_eq!(UI_SET_PHYS, 0x4008556c);
        assert_eq!(UI_BEGIN_FF_UPLOAD, 0xc06855c8);
        assert_eq!(UI_END_FF_UPLOAD, 0x406855c9);
        assert_eq!(UI_BEGIN_FF_ERASE, 0xc00c55ca);
        assert_eq!(ui_get_sysname(64), 0x8040552c);
    }

    #[test]
    fn nodes_and_present_from_sysfs() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("input17");
        std::fs::create_dir_all(d.join("event23")).unwrap();
        std::fs::create_dir_all(d.join("js3")).unwrap();
        std::fs::create_dir_all(d.join("capabilities")).unwrap();
        std::fs::write(d.join("phys"), "quiver-lab/awr/0\n").unwrap();
        std::fs::write(d.join("name"), "Quiver Lab Pad awr\n").unwrap();
        let other = t.path().join("input3");
        std::fs::create_dir_all(other.join("event3")).unwrap();
        std::fs::write(other.join("phys"), "usb-0000:00:14.0-2/input0\n").unwrap();
        let n = nodes_of(t.path(), "input17").unwrap();
        assert_eq!(n.event, PathBuf::from("/dev/input/event23"));
        assert_eq!(n.js, Some(PathBuf::from("/dev/input/js3")));
        assert_eq!(present(t.path()), vec![("input17".into(), "quiver-lab/awr/0".into(), "Quiver Lab Pad awr".into())]);
    }
}
