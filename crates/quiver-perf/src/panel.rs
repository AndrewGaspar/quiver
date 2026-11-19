//! `panel.input`: the Sight panel drawn in the game (design/SIGHT.md §14, "In the game"). One
//! small file in the session dir that four parties share:
//!
//! - **quiver-sight** opens and closes the panel (`state`, `until`, `opened_ns`) and says where
//!   the panel's cursor is (`cursor`), and reads the input the game process forwards;
//! - **the input preload** (`libquiver_sight_input.so`, LD_PRELOADed into the game) keeps
//!   keyboard and mouse events from Wine while the panel captures input, and forwards them into
//!   the event ring; it says it is there (`input_pid`, `input_flags`, `input_seen_ns`);
//! - **the Sight layer** says it can draw the panel on the game's swapchain (`layer_*`) and
//!   when it last did (`drawn_ns`, `drawn_epoch`);
//! - quiverd isn't involved: quiver-sight creates the file next to `hud.image`.
//!
//! The panel's pixels go through `panel.image`, the `hud.image` format with larger limits
//! ([`crate::hud::PANEL_LIMITS`]).
//!
//! **When input is captured** ([`capture`]): the panel is open (`state` has [`PANEL_OPEN`]),
//! its deadline `until` is ahead and no more than [`PANEL_HOLD_MAX_NS`] away (quiver-sight
//! renews it a few times a second, so a quiver-sight that dies or hangs releases input within
//! that), and the layer drew this open panel within [`PANEL_DRAWN_FRESH_NS`] (or the panel
//! opened less than [`PANEL_OPEN_GRACE_NS`] ago, before the first frame could show it). A game
//! that stops presenting, a layer that can't draw, a quiver-sight that goes away: input goes
//! back to the game. The preload also releases at once when quiver-sight's pid stops existing.
//!
//! **The event ring**: [`PANEL_SLOTS`] slots of 32 bytes after the 4 KiB header; any number of
//! writers in the game process (Wine's threads, the preload's own), one reader (quiver-sight).
//! Writer: `i = reserve.fetch_add(1)`; claim slot `i % n` by CAS of `commit` from a value
//! below `i + 1` to `(i + 1) | BUSY`; write the three other words; store `commit = i + 1`
//! (release). No locks, no syscalls: the preload writes from Wine's event loop. A slot still
//! held by a writer a lap behind makes the new event drop (counted in `lost`).
//!
//! Every field is untrusted for everyone reading it (the game can write the whole file).

use core::sync::atomic::{Ordering, fence};

use crate::error::Error;
use crate::shm::{Atomics, ShmRegion};

pub const PANEL_INPUT_FILE: &str = "panel.input";
pub const PANEL_IMAGE_FILE: &str = "panel.image";

/// "QUQVPINP".
pub const PANEL_MAGIC: u64 = u64::from_le_bytes(*b"QUQVPINP");
pub const PANEL_VERSION: u16 = 1;
pub const PANEL_HEADER_BYTES: usize = 4096;
pub const PANEL_SLOT_BYTES: usize = 32;
pub const PANEL_SLOTS: u32 = 1024;
/// The whole file.
pub const PANEL_FILE_BYTES: usize = PANEL_HEADER_BYTES + PANEL_SLOTS as usize * PANEL_SLOT_BYTES;

// Offsets. Header words.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8; // u16 version, u16 header_bytes, u16 slot_bytes, u16 _
const OFF_SLOTS: usize = 16; // u32 slot_count
// quiver-sight (offset 64, a cache line).
const OFF_STATE: usize = 64;
const OFF_UNTIL: usize = 72;
const OFF_OPENED: usize = 80;
const OFF_SIGHT_PID: usize = 88;
const OFF_CURSOR: usize = 96;
const OFF_CURSOR_SPACE: usize = 104;
// The preload (offset 128).
const OFF_INPUT_PID: usize = 128;
const OFF_INPUT_FLAGS: usize = 132;
const OFF_INPUT_SEEN: usize = 136;
const OFF_CAPTURING: usize = 144;
const OFF_FORWARDED: usize = 152;
const OFF_LOST: usize = 160;
const OFF_RELEASES: usize = 168;
// The layer (offset 192).
const OFF_LAYER_PID: usize = 192;
const OFF_LAYER_FLAGS: usize = 196;
const OFF_LAYER_SEEN: usize = 200;
const OFF_DRAWN: usize = 208;
const OFF_DRAWN_EPOCH: usize = 216;
const OFF_LAYER_EXTENT: usize = 224;
// The ring's reservation counter (offset 256, its own cache line).
const OFF_RESERVE: usize = 256;

/// `state` bit: the panel is open (in the game) for the epoch in bits 8–63.
pub const PANEL_OPEN: u64 = 1;
/// `state` bit: the layer draws the panel's cursor at `cursor`.
pub const PANEL_CURSOR: u64 = 2;

/// `input_flags`: Wine's X11 event fetching is interposed (`XCheckIfEvent` was called).
pub const INPUT_X11: u32 = 1;
/// Wine's Wayland input listeners are wrapped (a `wl_keyboard` or `wl_pointer` was made).
pub const INPUT_WAYLAND: u32 = 2;
/// The preload reads relative motion on an X connection of its own (XInput2 raw motion).
pub const INPUT_X11_RAW: u32 = 4;
/// The preload hides the X cursor while capturing (XFixes, on its own connection).
pub const INPUT_CURSOR_HIDDEN: u32 = 8;
/// The preload is capturing input right now.
pub const INPUT_CAPTURING: u32 = 16;

/// `layer_flags`: the layer can draw the panel on the game's swapchain (`layer_extent`).
pub const PANEL_LAYER_CAN_DRAW: u32 = 1;

/// `until` may be at most this far ahead; further is treated as no deadline at all (not
/// capturing): a corrupt or hostile value can't pin input.
pub const PANEL_HOLD_MAX_NS: u64 = 2_000_000_000;
/// How far ahead quiver-sight sets `until` when it renews it (every tick, 250 ms).
pub const PANEL_HOLD_NS: u64 = 1_000_000_000;
/// After opening, input is captured this long before the layer must have drawn the panel.
pub const PANEL_OPEN_GRACE_NS: u64 = 1_500_000_000;
/// The layer must have drawn the open panel within this to keep input captured.
pub const PANEL_DRAWN_FRESH_NS: u64 = 1_000_000_000;
/// The layer's and the preload's `*_seen_ns` older than this: they're gone (the game exited
/// or the part isn't loaded any more).
pub const PANEL_SEEN_FRESH_NS: u64 = 3_000_000_000;

/// Event kinds.
pub const EV_KEY: u16 = 1;
pub const EV_BUTTON: u16 = 2;
/// Relative motion: `a`, `b` = dx, dy in 1/256 px of the game's window.
pub const EV_MOTION: u16 = 3;
/// Scrolling: `a`, `b` = horizontal, vertical in 1/256 steps (a wheel notch is 256; positive
/// is right / down).
pub const EV_SCROLL: u16 = 4;
/// Everything the panel thought held is up (the game's window lost the keyboard).
pub const EV_RESET: u16 = 5;
/// Absolute pointer position in the game's window (`a`, `b` px), when that's all there is.
pub const EV_POINTER: u16 = 6;

/// `flags` of key and button events.
pub const EVF_DOWN: u16 = 1;
/// A key the compositor or Wine repeats while held (not a new press).
pub const EVF_REPEAT: u16 = 2;
/// From the Wayland driver (else X11).
pub const EVF_WAYLAND: u16 = 4;

const COMMIT_BUSY: u64 = 1 << 63;

/// One input event (32 bytes in the ring).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelEvent {
    /// CLOCK_MONOTONIC_RAW ns when the preload saw it.
    pub t: i64,
    pub kind: u16,
    pub flags: u16,
    /// Keys: the Linux evdev key code (X11 keycode − 8); buttons: the evdev button code
    /// (`BTN_LEFT` 0x110, …).
    pub code: u32,
    /// Keys: the X keysym when known (0 under Wayland); motion, scroll, pointer: x.
    pub a: i32,
    /// Keys: the X modifier state when known; motion, scroll, pointer: y.
    pub b: i32,
}

impl PanelEvent {
    pub fn key(t: i64, code: u32, down: bool, keysym: u32, state: u32) -> PanelEvent {
        PanelEvent { t, kind: EV_KEY, flags: if down { EVF_DOWN } else { 0 }, code, a: keysym as i32, b: state as i32 }
    }

    pub fn button(t: i64, code: u32, down: bool) -> PanelEvent {
        PanelEvent { t, kind: EV_BUTTON, flags: if down { EVF_DOWN } else { 0 }, code, a: 0, b: 0 }
    }

    /// `dx`, `dy` in px.
    pub fn motion(t: i64, dx: f64, dy: f64) -> PanelEvent {
        PanelEvent { t, kind: EV_MOTION, flags: 0, code: 0, a: fixed(dx), b: fixed(dy) }
    }

    /// In wheel notches (positive: right, down).
    pub fn scroll(t: i64, dx: f64, dy: f64) -> PanelEvent {
        PanelEvent { t, kind: EV_SCROLL, flags: 0, code: 0, a: fixed(dx), b: fixed(dy) }
    }

    pub fn down(&self) -> bool {
        self.flags & EVF_DOWN != 0
    }

    /// `a`, `b` as px (motion) or notches (scroll).
    pub fn xy(&self) -> (f64, f64) {
        (self.a as f64 / 256.0, self.b as f64 / 256.0)
    }

    fn words(&self) -> [u64; 3] {
        [self.t as u64, self.kind as u64 | (self.flags as u64) << 16 | (self.code as u64) << 32, self.a as u32 as u64 | (self.b as u32 as u64) << 32]
    }

    fn from_words(w: [u64; 3]) -> PanelEvent {
        PanelEvent { t: w[0] as i64, kind: w[1] as u16, flags: (w[1] >> 16) as u16, code: (w[1] >> 32) as u32, a: w[2] as u32 as i32, b: (w[2] >> 32) as u32 as i32 }
    }

    /// Plausible values (the reader's check: anything the game writes is untrusted).
    pub fn check(&self) -> bool {
        match self.kind {
            EV_KEY => self.code < 0x300 && self.flags & !(EVF_DOWN | EVF_REPEAT | EVF_WAYLAND) == 0,
            EV_BUTTON => (0x100..0x300).contains(&self.code) && self.flags & !(EVF_DOWN | EVF_WAYLAND) == 0,
            EV_MOTION | EV_SCROLL | EV_POINTER => self.a.unsigned_abs() < (1 << 28) && self.b.unsigned_abs() < (1 << 28),
            EV_RESET => true,
            _ => false,
        }
    }
}

fn fixed(v: f64) -> i32 {
    if v.is_finite() { (v * 256.0).round().clamp(-(1 << 27) as f64, (1 << 27) as f64) as i32 } else { 0 }
}

/// What quiver-sight wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Control {
    pub state: u64,
    pub until: u64,
    pub opened_ns: u64,
    pub sight_pid: u32,
}

impl Control {
    pub fn open(&self) -> bool {
        self.state & PANEL_OPEN != 0
    }

    pub fn epoch(&self) -> u64 {
        self.state >> 8
    }
}

/// What the layer wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerInfo {
    pub pid: u32,
    pub flags: u32,
    pub seen_ns: u64,
    pub drawn_ns: u64,
    pub drawn_epoch: u64,
    pub extent: (u32, u32),
}

impl LayerInfo {
    /// The layer is there (seen lately) and can draw the panel at a known extent.
    pub fn can_draw(&self, now_ns: i64) -> bool {
        self.flags & PANEL_LAYER_CAN_DRAW != 0 && self.extent.0 > 0 && self.extent.1 > 0 && fresh(self.seen_ns, now_ns, PANEL_SEEN_FRESH_NS)
    }
}

/// What the preload wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputInfo {
    pub pid: u32,
    pub flags: u32,
    pub seen_ns: u64,
    pub capturing: u64,
    pub forwarded: u64,
    pub lost: u64,
    /// Releases of keys and buttons the game saw pressed, passed to it while capturing.
    pub releases: u64,
}

impl InputInfo {
    /// The preload is in the game and has hooked Wine's input (X11 or Wayland).
    pub fn ready(&self, now_ns: i64) -> bool {
        self.pid != 0 && self.flags & (INPUT_X11 | INPUT_WAYLAND) != 0 && fresh(self.seen_ns, now_ns, PANEL_SEEN_FRESH_NS)
    }
}

fn fresh(seen: u64, now_ns: i64, within: u64) -> bool {
    let now = now_ns.max(0) as u64;
    seen != 0 && seen <= now.saturating_add(1_000_000_000) && now.saturating_sub(seen) < within
}

/// The capture decision (module docs): `Some(epoch)` while input belongs to the panel.
pub fn capture(c: &Control, layer: &LayerInfo, now_ns: i64) -> Option<u64> {
    capture_why(c, layer, now_ns).ok()
}

/// [`capture`], with why not (for logs).
pub fn capture_why(c: &Control, layer: &LayerInfo, now_ns: i64) -> Result<u64, &'static str> {
    if !c.open() {
        return Err("the panel closed");
    }
    let now = now_ns.max(0) as u64;
    if c.until <= now || c.until - now > PANEL_HOLD_MAX_NS {
        return Err("failsafe: quiver-sight stopped renewing the panel's deadline");
    }
    let epoch = c.epoch();
    let drawn = layer.drawn_epoch == epoch && layer.drawn_ns != 0 && layer.drawn_ns <= now.saturating_add(1_000_000_000) && now.saturating_sub(layer.drawn_ns) < PANEL_DRAWN_FRESH_NS;
    let grace = c.opened_ns != 0 && c.opened_ns <= now && now - c.opened_ns < PANEL_OPEN_GRACE_NS;
    if drawn || grace { Ok(epoch) } else { Err("failsafe: the panel isn't in the game's frames (the layer stopped drawing it)") }
}

/// `panel.input` over a mapped region.
pub struct PanelInput<R: ShmRegion> {
    region: R,
    slots: u32,
}

/// Initializes `region` ([`PANEL_FILE_BYTES`]) as an empty `panel.input`; magic last.
pub fn init_panel_input<R: ShmRegion>(region: &R) -> Result<(), Error> {
    if !region.writable() {
        return Err(Error::ReadOnly);
    }
    if region.len() < PANEL_FILE_BYTES {
        return Err(Error::TooSmall { need: PANEL_FILE_BYTES, have: region.len() });
    }
    region.u64_at(OFF_MAGIC).store(0, Ordering::Relaxed);
    fence(Ordering::Release);
    for off in (8..PANEL_FILE_BYTES).step_by(8) {
        region.u64_at(off).store(0, Ordering::Relaxed);
    }
    region.u64_at(OFF_VERSION).store(PANEL_VERSION as u64 | (PANEL_HEADER_BYTES as u64) << 16 | (PANEL_SLOT_BYTES as u64) << 32, Ordering::Relaxed);
    region.u32_at(OFF_SLOTS).store(PANEL_SLOTS, Ordering::Relaxed);
    region.u64_at(OFF_MAGIC).store(PANEL_MAGIC, Ordering::Release);
    Ok(())
}

impl<R: ShmRegion> PanelInput<R> {
    /// Attaches to an initialized file (any party).
    pub fn new(region: R) -> Result<Self, Error> {
        if region.len() < PANEL_HEADER_BYTES {
            return Err(Error::TooSmall { need: PANEL_HEADER_BYTES, have: region.len() });
        }
        let magic = region.u64_at(OFF_MAGIC).load(Ordering::Acquire);
        if magic == 0 {
            return Err(Error::NotReady);
        }
        if magic != PANEL_MAGIC {
            return Err(Error::BadMagic(magic));
        }
        let v = region.u64_at(OFF_VERSION).load(Ordering::Relaxed);
        if v as u16 != PANEL_VERSION {
            return Err(Error::Version(v as u16));
        }
        if (v >> 16) as u16 as usize != PANEL_HEADER_BYTES || (v >> 32) as u16 as usize != PANEL_SLOT_BYTES {
            return Err(Error::Layout("header_bytes/slot_bytes"));
        }
        let slots = region.u32_at(OFF_SLOTS).load(Ordering::Relaxed);
        if slots != PANEL_SLOTS {
            return Err(Error::Layout("slot_count"));
        }
        if region.len() < PANEL_FILE_BYTES {
            return Err(Error::TooSmall { need: PANEL_FILE_BYTES, have: region.len() });
        }
        Ok(PanelInput { region, slots })
    }

    pub fn region(&self) -> &R {
        &self.region
    }

    // ---- quiver-sight ---------------------------------------------------------------------

    /// Opens the panel as `epoch` (≥ 1, new for every open), captured until `until_ns`.
    pub fn open(&self, epoch: u64, now_ns: i64, until_ns: i64, cursor: bool) {
        let r = &self.region;
        r.u64_at(OFF_UNTIL).store(until_ns.max(0) as u64, Ordering::Relaxed);
        r.u64_at(OFF_OPENED).store(now_ns.max(0) as u64, Ordering::Relaxed);
        r.u64_at(OFF_STATE).store(epoch << 8 | PANEL_OPEN | if cursor { PANEL_CURSOR } else { 0 }, Ordering::Release);
    }

    /// Renews the deadline of the open panel.
    pub fn renew(&self, until_ns: i64) {
        self.region.u64_at(OFF_UNTIL).store(until_ns.max(0) as u64, Ordering::Release);
    }

    /// Shows or hides the cursor of the open panel.
    pub fn set_cursor_shown(&self, shown: bool) {
        let s = self.region.u64_at(OFF_STATE);
        let v = s.load(Ordering::Relaxed);
        s.store(if shown { v | PANEL_CURSOR } else { v & !PANEL_CURSOR }, Ordering::Release);
    }

    /// Closes it: input goes back to the game at once.
    pub fn close(&self) {
        let s = self.region.u64_at(OFF_STATE);
        let epoch = s.load(Ordering::Relaxed) >> 8;
        s.store(epoch << 8, Ordering::Release);
        self.region.u64_at(OFF_UNTIL).store(0, Ordering::Relaxed);
    }

    pub fn set_sight_pid(&self, pid: u32) {
        self.region.u32_at(OFF_SIGHT_PID).store(pid, Ordering::Relaxed);
    }

    /// The cursor's position in the swapchain's pixels (`space` = the extent it is in).
    pub fn set_cursor(&self, x: i32, y: i32, space: (u32, u32)) {
        self.region.u64_at(OFF_CURSOR_SPACE).store(space.0 as u64 | (space.1 as u64) << 32, Ordering::Relaxed);
        self.region.u64_at(OFF_CURSOR).store(x as u32 as u64 | (y as u32 as u64) << 32, Ordering::Release);
    }

    pub fn control(&self) -> Control {
        let r = &self.region;
        let state = r.u64_at(OFF_STATE).load(Ordering::Acquire);
        Control {
            state,
            until: r.u64_at(OFF_UNTIL).load(Ordering::Acquire),
            opened_ns: r.u64_at(OFF_OPENED).load(Ordering::Relaxed),
            sight_pid: r.u32_at(OFF_SIGHT_PID).load(Ordering::Relaxed),
        }
    }

    /// The cursor (`None` when the open panel shows none), and the extent it is in.
    pub fn cursor(&self) -> Option<((i32, i32), (u32, u32))> {
        let st = self.region.u64_at(OFF_STATE).load(Ordering::Acquire);
        if st & (PANEL_OPEN | PANEL_CURSOR) != PANEL_OPEN | PANEL_CURSOR {
            return None;
        }
        let c = self.region.u64_at(OFF_CURSOR).load(Ordering::Acquire);
        let s = self.region.u64_at(OFF_CURSOR_SPACE).load(Ordering::Relaxed);
        Some(((c as u32 as i32, (c >> 32) as u32 as i32), (s as u32, (s >> 32) as u32)))
    }

    // ---- the layer ------------------------------------------------------------------------

    /// The layer's word: whether it can draw the panel, on what extent; its heartbeat.
    pub fn set_layer(&self, pid: u32, flags: u32, extent: (u32, u32), now_ns: i64) {
        let r = &self.region;
        r.u32_at(OFF_LAYER_PID).store(pid, Ordering::Relaxed);
        r.u32_at(OFF_LAYER_FLAGS).store(flags, Ordering::Relaxed);
        r.u64_at(OFF_LAYER_EXTENT).store(extent.0 as u64 | (extent.1 as u64) << 32, Ordering::Relaxed);
        r.u64_at(OFF_LAYER_SEEN).store(now_ns.max(0) as u64, Ordering::Release);
    }

    /// The layer composited the open panel of `epoch` into a frame (per present: two stores).
    #[inline]
    pub fn mark_drawn(&self, epoch: u64, now_ns: i64) {
        self.region.u64_at(OFF_DRAWN_EPOCH).store(epoch, Ordering::Relaxed);
        self.region.u64_at(OFF_DRAWN).store(now_ns.max(0) as u64, Ordering::Release);
    }

    pub fn layer(&self) -> LayerInfo {
        let r = &self.region;
        let e = r.u64_at(OFF_LAYER_EXTENT).load(Ordering::Relaxed);
        LayerInfo {
            pid: r.u32_at(OFF_LAYER_PID).load(Ordering::Relaxed),
            flags: r.u32_at(OFF_LAYER_FLAGS).load(Ordering::Relaxed),
            seen_ns: r.u64_at(OFF_LAYER_SEEN).load(Ordering::Acquire),
            drawn_ns: r.u64_at(OFF_DRAWN).load(Ordering::Acquire),
            drawn_epoch: r.u64_at(OFF_DRAWN_EPOCH).load(Ordering::Relaxed),
            extent: (e as u32, (e >> 32) as u32),
        }
    }

    /// [`capture`] on what's in the file now.
    #[inline]
    pub fn capture(&self, now_ns: i64) -> Option<u64> {
        let c = self.control();
        if !c.open() {
            return None;
        }
        capture(&c, &self.layer(), now_ns)
    }

    // ---- the preload ----------------------------------------------------------------------

    pub fn set_input(&self, pid: u32, flags: u32, now_ns: i64) {
        let r = &self.region;
        r.u32_at(OFF_INPUT_PID).store(pid, Ordering::Relaxed);
        r.u32_at(OFF_INPUT_FLAGS).store(flags, Ordering::Relaxed);
        r.u64_at(OFF_INPUT_SEEN).store(now_ns.max(0) as u64, Ordering::Release);
    }

    pub fn set_capturing(&self, epoch: u64) {
        self.region.u64_at(OFF_CAPTURING).store(epoch, Ordering::Relaxed);
    }

    pub fn note_releases(&self, n: u64) {
        self.region.u64_at(OFF_RELEASES).fetch_add(n, Ordering::Relaxed);
    }

    pub fn input(&self) -> InputInfo {
        let r = &self.region;
        InputInfo {
            pid: r.u32_at(OFF_INPUT_PID).load(Ordering::Relaxed),
            flags: r.u32_at(OFF_INPUT_FLAGS).load(Ordering::Relaxed),
            seen_ns: r.u64_at(OFF_INPUT_SEEN).load(Ordering::Acquire),
            capturing: r.u64_at(OFF_CAPTURING).load(Ordering::Relaxed),
            forwarded: r.u64_at(OFF_FORWARDED).load(Ordering::Relaxed),
            lost: r.u64_at(OFF_LOST).load(Ordering::Relaxed),
            releases: r.u64_at(OFF_RELEASES).load(Ordering::Relaxed),
        }
    }

    fn slot_off(&self, i: u64) -> usize {
        PANEL_HEADER_BYTES + (i % self.slots as u64) as usize * PANEL_SLOT_BYTES
    }

    /// Appends an event (any thread; lock-free, no syscalls). `false`: dropped (the slot is
    /// still held by a writer a lap behind).
    pub fn push(&self, e: &PanelEvent) -> bool {
        let r = &self.region;
        let i = r.u64_at(OFF_RESERVE).fetch_add(1, Ordering::Relaxed);
        let off = self.slot_off(i);
        let commit = r.u64_at(off);
        let want = i + 1;
        let cur = commit.load(Ordering::Relaxed);
        if cur & COMMIT_BUSY != 0 || cur >= want || commit.compare_exchange(cur, want | COMMIT_BUSY, Ordering::Acquire, Ordering::Relaxed).is_err() {
            r.u64_at(OFF_LOST).fetch_add(1, Ordering::Relaxed);
            return false;
        }
        for (k, w) in e.words().iter().enumerate() {
            r.u64_at(off + 8 * (k + 1)).store(*w, Ordering::Relaxed);
        }
        commit.store(want, Ordering::Release);
        r.u64_at(OFF_FORWARDED).fetch_add(1, Ordering::Relaxed);
        true
    }

    /// The reservation counter: a reader starting now begins here.
    pub fn reserved(&self) -> u64 {
        self.region.u64_at(OFF_RESERVE).load(Ordering::Acquire)
    }
}

/// quiver-sight's cursor into the ring.
#[derive(Clone, Copy, Debug, Default)]
pub struct EventReader {
    next: u64,
    stuck: u32,
    /// Events overwritten before they were read, or failing [`PanelEvent::check`].
    pub lost: u64,
}

impl EventReader {
    /// Reads from the events written after now.
    pub fn at_end<R: ShmRegion>(p: &PanelInput<R>) -> EventReader {
        EventReader { next: p.reserved(), stuck: 0, lost: 0 }
    }

    /// Calls `f` with each new, committed, plausible event in order. Bounded: at most one lap.
    pub fn poll<R: ShmRegion>(&mut self, p: &PanelInput<R>, mut f: impl FnMut(PanelEvent)) {
        let r = p.region();
        let n = p.slots as u64;
        let end = p.reserved();
        if end < self.next {
            // The counter went backwards: a new file, or a hostile one. Start over.
            self.next = end;
            return;
        }
        if end - self.next > n {
            self.lost += end - self.next - n;
            self.next = end - n;
        }
        while self.next < end {
            let i = self.next;
            let off = p.slot_off(i);
            let commit = r.u64_at(off);
            let c = commit.load(Ordering::Acquire);
            if c == i + 1 {
                let mut w = [0u64; 3];
                for (k, word) in w.iter_mut().enumerate() {
                    *word = r.u64_at(off + 8 * (k + 1)).load(Ordering::Relaxed);
                }
                fence(Ordering::Acquire);
                if commit.load(Ordering::Relaxed) != c {
                    // Claimed for a later lap mid-copy.
                    self.lost += 1;
                } else {
                    let e = PanelEvent::from_words(w);
                    if e.check() {
                        f(e);
                    } else {
                        self.lost += 1;
                    }
                }
                self.next += 1;
                self.stuck = 0;
            } else if c & !COMMIT_BUSY > i + 1 {
                // Overwritten by a later lap.
                self.lost += 1;
                self.next += 1;
                self.stuck = 0;
            } else {
                // Not committed yet: retry next poll; a writer that died mid-write is skipped
                // after a few polls.
                self.stuck += 1;
                if self.stuck > 4 {
                    self.lost += 1;
                    self.next += 1;
                    self.stuck = 0;
                    continue;
                }
                return;
            }
        }
    }
}

#[cfg(feature = "std")]
mod files {
    use super::*;
    use crate::hud::{HudReader, HudWriter, PANEL_LIMITS, hud_file_bytes, init_image};
    use crate::shm::Mapping;
    use std::io;
    use std::path::Path;

    /// Creates `panel.input` at `path` if it isn't there, and attaches (quiver-sight).
    pub fn create_panel_input(path: &Path) -> io::Result<PanelInput<Mapping>> {
        if let Ok(p) = open_panel_input(path, true) {
            return Ok(p);
        }
        let tmp = path.with_file_name(format!(".{PANEL_INPUT_FILE}.{}.{:x}.tmp", std::process::id(), crate::random_u64()));
        let made = Mapping::create(&tmp, PANEL_FILE_BYTES).and_then(|m| Ok(init_panel_input(&m)?)).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = made {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        open_panel_input(path, true)
    }

    /// Attaches to `panel.input` (the layer and the preload write their own fields, so both
    /// open it writable).
    pub fn open_panel_input(path: &Path, writable: bool) -> io::Result<PanelInput<Mapping>> {
        let m = if writable { Mapping::open_write(path, PANEL_FILE_BYTES, PANEL_FILE_BYTES)? } else { Mapping::open_read(path, PANEL_FILE_BYTES, PANEL_FILE_BYTES)? };
        Ok(PanelInput::new(m)?)
    }

    /// Creates (or replaces) `panel.image` with buffers of `buffer_bytes` (quiver-sight, when
    /// it opens the panel in the game: sized for the swapchain). Built under a temp name and
    /// renamed, so the layer never maps a half-made file.
    pub fn create_panel_image(path: &Path, buffer_bytes: u32) -> io::Result<HudWriter<Mapping>> {
        let tmp = path.with_file_name(format!(".{PANEL_IMAGE_FILE}.{}.{:x}.tmp", std::process::id(), crate::random_u64()));
        let made = Mapping::create(&tmp, hud_file_bytes(buffer_bytes))
            .and_then(|m| Ok(init_image(&m, buffer_bytes, &PANEL_LIMITS).map(|_| m)?))
            .and_then(|m| std::fs::rename(&tmp, path).map(|_| m));
        match made {
            Ok(m) => Ok(HudWriter::with_limits(m, &PANEL_LIMITS)?),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    pub fn open_panel_image_reader(path: &Path) -> io::Result<HudReader<Mapping>> {
        let m = Mapping::open_read(path, hud_file_bytes(4096), hud_file_bytes(PANEL_LIMITS.max_buffer_bytes))?;
        Ok(HudReader::with_limits(m, &PANEL_LIMITS)?)
    }
}

#[cfg(feature = "std")]
pub use files::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::HeapRegion;
    use std::sync::Arc;

    fn file() -> PanelInput<Arc<HeapRegion>> {
        let mem = Arc::new(HeapRegion::new(PANEL_FILE_BYTES));
        init_panel_input(&mem).unwrap();
        PanelInput::new(mem).unwrap()
    }

    const S: i64 = 1_000_000_000;

    #[test]
    fn header_checks() {
        let mem = HeapRegion::new(PANEL_FILE_BYTES);
        assert!(matches!(PanelInput::new(&mem), Err(Error::NotReady)));
        init_panel_input(&mem).unwrap();
        assert!(PanelInput::new(&mem).is_ok());
        mem.u32_at(OFF_SLOTS).store(7, Ordering::Relaxed);
        assert!(PanelInput::new(&mem).is_err());
        let small = HeapRegion::new(PANEL_HEADER_BYTES);
        assert!(init_panel_input(&small).is_err());
        assert_eq!(&PANEL_MAGIC.to_le_bytes(), b"QUQVPINP");
    }

    #[test]
    fn capture_rules() {
        let p = file();
        let now = 100 * S;
        assert_eq!(p.capture(now), None);
        p.open(7, now, now + S, true);
        // Opened, not drawn yet: captured during the grace.
        assert_eq!(p.capture(now + S / 2), Some(7));
        // The deadline passed (quiver-sight stopped renewing).
        assert_eq!(p.capture(now + S), None);
        p.renew(now + 2 * S);
        // Past the grace with nothing drawn: released (the game isn't presenting, or the layer
        // can't draw).
        assert_eq!(p.capture(now + 3 * S / 2), None);
        // Drawn now: captured again.
        p.mark_drawn(7, now + 3 * S / 2);
        assert_eq!(p.capture(now + 3 * S / 2 + 1), Some(7));
        // Drawn for an earlier open doesn't count.
        p.mark_drawn(6, now + 3 * S / 2);
        assert_eq!(p.capture(now + 3 * S / 2 + 1), None);
        p.mark_drawn(7, now + 3 * S / 2);
        // A frame a second old: stale.
        p.renew(now + 4 * S);
        assert_eq!(p.capture(now + 5 * S / 2 + 1), None);
        // A deadline too far ahead is no deadline.
        p.renew(now + 10 * S);
        p.mark_drawn(7, now + 3 * S);
        assert_eq!(p.capture(now + 3 * S), None);
        p.renew(now + 4 * S);
        assert_eq!(p.capture(now + 3 * S), Some(7));
        // Closed: never.
        p.close();
        assert_eq!(p.capture(now + 3 * S), None);
        assert_eq!(p.control().epoch(), 7);
    }

    #[test]
    fn cursor_and_parties() {
        let p = file();
        assert_eq!(p.cursor(), None);
        p.open(1, 10, 10 + S, true);
        p.set_cursor(-3, 1200, (2560, 1600));
        assert_eq!(p.cursor(), Some(((-3, 1200), (2560, 1600))));
        p.set_cursor_shown(false);
        assert_eq!(p.cursor(), None);
        p.set_layer(42, PANEL_LAYER_CAN_DRAW, (1920, 1080), 5 * S);
        assert!(p.layer().can_draw(6 * S));
        assert!(!p.layer().can_draw(9 * S));
        p.set_input(43, INPUT_X11, 5 * S);
        assert!(p.input().ready(5 * S));
        p.set_input(43, 0, 5 * S);
        assert!(!p.input().ready(5 * S));
    }

    #[test]
    fn ring_in_order_and_laps() {
        let p = file();
        let mut r = EventReader::at_end(&p);
        let mut got = Vec::new();
        r.poll(&p, |e| got.push(e));
        assert!(got.is_empty());
        for i in 0..10 {
            assert!(p.push(&PanelEvent::key(i, 30 + i as u32, i % 2 == 0, 0x61, 0)));
        }
        r.poll(&p, |e| got.push(e));
        assert_eq!(got.len(), 10);
        assert!(got.iter().enumerate().all(|(i, e)| e.code == 30 + i as u32 && e.down() == (i % 2 == 0) && e.a == 0x61));
        // More than a lap behind: the oldest are counted lost.
        for i in 0..(PANEL_SLOTS as i64 + 5) {
            p.push(&PanelEvent::motion(i, 1.5, -2.0));
        }
        got.clear();
        r.poll(&p, |e| got.push(e));
        assert_eq!(got.len(), PANEL_SLOTS as usize);
        assert_eq!(r.lost, 5);
        assert_eq!(got[0].xy(), (1.5, -2.0));
        // An implausible event is dropped.
        p.push(&PanelEvent { kind: 99, ..Default::default() });
        got.clear();
        r.poll(&p, |e| got.push(e));
        assert!(got.is_empty());
        assert_eq!(p.input().forwarded, 10 + PANEL_SLOTS as u64 + 5 + 1);
    }

    #[test]
    fn concurrent_writers() {
        let p = Arc::new(file());
        let mut r = EventReader::at_end(&p);
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for i in 0..2000 {
                        p.push(&PanelEvent::button(i, 0x110 + w, true));
                    }
                })
            })
            .collect();
        let mut n = 0u64;
        let mut seen = [0i64; 4];
        let mut ok = true;
        while writers.iter().any(|w| !w.is_finished()) {
            r.poll(&p, |e| {
                n += 1;
                let w = (e.code - 0x110) as usize;
                // Per writer, events come in order (lost ones skipped).
                ok &= e.t >= seen[w];
                seen[w] = e.t;
            });
        }
        for w in writers {
            w.join().unwrap();
        }
        r.poll(&p, |_| n += 1);
        assert!(ok);
        // Every event read once at most; what wasn't read was counted lost by one side or the
        // other (a slot a writer had to drop also reads as lost to the reader).
        assert!(n <= 8000 && n + r.lost + p.input().lost >= 8000, "{n} read, {} + {} lost", r.lost, p.input().lost);
    }

    #[test]
    fn panel_image_limits() {
        use crate::hud::{PANEL_LIMITS, buffer_bytes_within, init_image, HudWriter, HudReader, HudPoll, hud_file_bytes};
        use crate::HudBufferHeader;
        let (w, h) = (2560u32, 1600u32);
        let bb = buffer_bytes_within(w, h, &PANEL_LIMITS).unwrap();
        assert!(crate::hud::buffer_bytes_for(w, h).is_none(), "too big for hud.image");
        assert!(buffer_bytes_within(4096, 4096, &PANEL_LIMITS).is_some());
        assert!(buffer_bytes_within(4097, 4096, &PANEL_LIMITS).is_none());
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_image(&mem, bb, &PANEL_LIMITS).unwrap();
        // A HUD reader refuses it (too big); a panel reader takes it.
        assert!(HudReader::new(mem.clone()).is_err());
        let mut wr = HudWriter::with_limits(mem.clone(), &PANEL_LIMITS).unwrap();
        let mut rd = HudReader::with_limits(mem.clone(), &PANEL_LIMITS).unwrap();
        let mut d = HudBufferHeader::ZERO;
        (d.width, d.height, d.stride, d.extent_w, d.extent_h) = (w, h, w * 4, w, h);
        let mut px = vec![1u8; (w * 4 * h) as usize];
        assert_eq!(wr.publish(&d, &px), Ok(1));
        let mut dst = vec![0u8; px.len()];
        let (poll, rows) = rd.poll_since((w, h), &mut dst, 0);
        assert!(matches!(poll, HudPoll::Image { published: 1, .. }) && rows.is_none());
        assert!(dst.iter().all(|&b| b == 1));
        // Rows 10..20 changed since publish 1: a reader holding 1 copies only those.
        for b in &mut px[(10 * w * 4) as usize..(20 * w * 4) as usize] {
            *b = 2;
        }
        px[0] = 9; // not promised to change: a partial reader doesn't see it
        (d.dirty_y0, d.dirty_y1, d.dirty_base) = (10, 20, 1);
        wr.publish(&d, &px).unwrap();
        let (poll, rows) = rd.poll_since((w, h), &mut dst, 1);
        assert!(matches!(poll, HudPoll::Image { published: 2, .. }));
        assert_eq!(rows, Some((10, 20)));
        assert_eq!(dst[0], 1);
        assert!(dst[(10 * w * 4) as usize..(20 * w * 4) as usize].iter().all(|&b| b == 2));
        assert_eq!(dst[(20 * w * 4) as usize], 1);
        // A reader holding something else takes the whole image.
        (d.dirty_base) = 2;
        wr.publish(&d, &px).unwrap();
        let (_, rows) = rd.poll_since((w, h), &mut dst, 1);
        assert_eq!(rows, None);
        assert_eq!(dst[0], 9);
        // Bad rows: the whole image.
        d.dirty_y1 = h + 1;
        d.dirty_base = 3;
        wr.publish(&d, &px).unwrap();
        assert_eq!(rd.poll_since((w, h), &mut dst, 3).1, None);
    }
}
