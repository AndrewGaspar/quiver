//! `gpu.ctl`: the lab's GPU limit page (design/LAB.md §15), shared by an instance's
//! supervisor (on the host) and the Sight layer in the lab game (`QUIVER_LAB_GPU=<path>`).
//!
//! The supervisor creates the file in the run folder before the launch and writes what it
//! wants: a low queue priority for devices created from now on, and the minimum time between
//! two presents (the governor's frame pacing; 0 = none). The layer writes what happened: the
//! priority each device got, presents and the time it held them back. Every field is an
//! aligned atomic, so either side reads it whole; nothing else is in the page.

use core::sync::atomic::Ordering;

use crate::shm::{Atomics, ShmRegion};

/// The environment variable naming the page (a path in the container).
pub const ENV: &str = "QUIVER_LAB_GPU";
/// The page's file name in a run folder.
pub const FILE: &str = "gpu.ctl";
pub const LEN: usize = 4096;
/// `QGPU`, little-endian.
pub const MAGIC: u32 = 0x5550_4751;
pub const VERSION: u32 = 1;

// Offsets (bytes). Supervisor-written first, then layer-written.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
/// u64: the minimum interval between presents, ns (0: no pacing).
const OFF_INTERVAL_NS: usize = 8;
/// u32: [`PRIORITY_LOW`] or [`PRIORITY_NONE`], read when a device is created.
const OFF_PRIORITY_WANTED: usize = 16;
/// u32: what the last device got ([`Priority`]).
const OFF_PRIORITY_STATE: usize = 20;
/// u64: presents through the layer.
const OFF_PRESENTS: usize = 24;
/// u64: total time presents were held back, ns.
const OFF_HELD_NS: usize = 32;
/// u32: the presenting process (its pid in the container).
const OFF_PID: usize = 40;
/// u32: devices created through the layer.
const OFF_DEVICES: usize = 44;
/// u64: presents that were held back.
const OFF_HELD: usize = 48;

pub const PRIORITY_NONE: u32 = 0;
pub const PRIORITY_LOW: u32 = 1;

/// What the layer did about the queue priority of the devices it saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// No device created yet.
    NotYet,
    /// Every queue was created at `VK_QUEUE_GLOBAL_PRIORITY_LOW`.
    Low,
    /// The driver has neither `VK_KHR_global_priority` nor `VK_EXT_global_priority`.
    Unsupported,
    /// The driver refused a device with it; the device was made without it.
    Refused,
    /// Not asked for.
    NotAsked,
    /// The app chose its own priority for some queue (left alone).
    AppChose,
    /// The driver has the extension, but its queue families offer no level below the default
    /// (`VkQueueFamilyGlobalPriorityPropertiesKHR`: NVIDIA 620 lists only MEDIUM).
    NotOffered,
}

impl Priority {
    pub fn to_u32(self) -> u32 {
        match self {
            Priority::NotYet => 0,
            Priority::Low => 1,
            Priority::Unsupported => 2,
            Priority::Refused => 3,
            Priority::NotAsked => 4,
            Priority::AppChose => 5,
            Priority::NotOffered => 6,
        }
    }
    pub fn from_u32(v: u32) -> Priority {
        match v {
            1 => Priority::Low,
            2 => Priority::Unsupported,
            3 => Priority::Refused,
            4 => Priority::NotAsked,
            5 => Priority::AppChose,
            6 => Priority::NotOffered,
            _ => Priority::NotYet,
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Priority::NotYet => "no Vulkan device yet",
            Priority::Low => "priority low",
            Priority::Unsupported => "priority: the driver has no global priority",
            Priority::Refused => "priority: the driver refused low",
            Priority::NotAsked => "priority default",
            Priority::AppChose => "priority: the game chose its own",
            Priority::NotOffered => "priority: the driver offers nothing below the default (medium only)",
        }
    }
}

/// A snapshot of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub interval_ns: u64,
    pub priority_wanted: u32,
    pub priority: Priority,
    pub presents: u64,
    pub held: u64,
    pub held_ns: u64,
    pub pid: u32,
    pub devices: u32,
}

/// The page over a mapped region of at least [`LEN`] bytes.
pub struct Ctl<R: ShmRegion> {
    r: R,
}

impl<R: ShmRegion> Ctl<R> {
    /// Attaches to an initialised page (`None`: too small or not one).
    pub fn attach(r: R) -> Option<Ctl<R>> {
        if r.len() < LEN {
            return None;
        }
        let c = Ctl { r };
        (c.r.u32_at(OFF_MAGIC).load(Ordering::Acquire) == MAGIC && c.r.u32_at(OFF_VERSION).load(Ordering::Relaxed) == VERSION).then_some(c)
    }

    /// Initialises a fresh (zeroed) page: the supervisor, before the launch.
    pub fn init(r: R, priority_wanted: u32) -> Option<Ctl<R>> {
        if r.len() < LEN || !r.writable() {
            return None;
        }
        r.u32_at(OFF_VERSION).store(VERSION, Ordering::Relaxed);
        r.u32_at(OFF_PRIORITY_WANTED).store(priority_wanted, Ordering::Relaxed);
        r.u32_at(OFF_MAGIC).store(MAGIC, Ordering::Release);
        Some(Ctl { r })
    }

    pub fn interval_ns(&self) -> u64 {
        self.r.u64_at(OFF_INTERVAL_NS).load(Ordering::Relaxed)
    }
    pub fn set_interval_ns(&self, ns: u64) {
        self.r.u64_at(OFF_INTERVAL_NS).store(ns, Ordering::Relaxed);
    }
    pub fn priority_wanted(&self) -> u32 {
        self.r.u32_at(OFF_PRIORITY_WANTED).load(Ordering::Relaxed)
    }
    pub fn set_priority(&self, p: Priority) {
        self.r.u32_at(OFF_PRIORITY_STATE).store(p.to_u32(), Ordering::Relaxed);
        self.r.u32_at(OFF_DEVICES).fetch_add(1, Ordering::Relaxed);
    }
    /// One present; `held_ns` is how long it was held back (0: not at all).
    pub fn presented(&self, pid: u32, held_ns: u64) {
        self.r.u64_at(OFF_PRESENTS).fetch_add(1, Ordering::Relaxed);
        if held_ns > 0 {
            self.r.u64_at(OFF_HELD).fetch_add(1, Ordering::Relaxed);
            self.r.u64_at(OFF_HELD_NS).fetch_add(held_ns, Ordering::Relaxed);
        }
        self.r.u32_at(OFF_PID).store(pid, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            interval_ns: self.interval_ns(),
            priority_wanted: self.priority_wanted(),
            priority: Priority::from_u32(self.r.u32_at(OFF_PRIORITY_STATE).load(Ordering::Relaxed)),
            presents: self.r.u64_at(OFF_PRESENTS).load(Ordering::Relaxed),
            held: self.r.u64_at(OFF_HELD).load(Ordering::Relaxed),
            held_ns: self.r.u64_at(OFF_HELD_NS).load(Ordering::Relaxed),
            pid: self.r.u32_at(OFF_PID).load(Ordering::Relaxed),
            devices: self.r.u32_at(OFF_DEVICES).load(Ordering::Relaxed),
        }
    }
}

/// The frame pacer's arithmetic: when may the present arriving at `now` go, given the last
/// deadline and the interval? Returns the new deadline (the present waits until then when
/// it's in the future). A present that comes late resets the schedule rather than letting
/// the next ones through in a burst; a late one up to an interval behind keeps the cadence.
pub fn next_deadline(last_deadline: i64, now: i64, interval_ns: u64) -> i64 {
    if interval_ns == 0 {
        return now;
    }
    let i = interval_ns.min(i64::MAX as u64 / 4) as i64;
    let want = last_deadline.saturating_add(i);
    if want < now - i { now } else { want }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::shm::HeapRegion;

    #[test]
    fn page_round_trip() {
        let r = std::sync::Arc::new(HeapRegion::new(LEN));
        assert!(Ctl::attach(r.clone()).is_none(), "zeroed: not a page yet");
        let sup = Ctl::init(r.clone(), PRIORITY_LOW).unwrap();
        let layer = Ctl::attach(r.clone()).unwrap();
        assert_eq!(layer.priority_wanted(), PRIORITY_LOW);
        sup.set_interval_ns(25_000_000);
        assert_eq!(layer.interval_ns(), 25_000_000);
        layer.set_priority(Priority::Low);
        layer.presented(42, 0);
        layer.presented(42, 3_000_000);
        let s = sup.snapshot();
        assert_eq!((s.priority, s.presents, s.held, s.held_ns, s.pid, s.devices), (Priority::Low, 2, 1, 3_000_000, 42, 1));
        for p in [Priority::NotYet, Priority::Low, Priority::Unsupported, Priority::Refused, Priority::NotAsked, Priority::AppChose, Priority::NotOffered] {
            assert_eq!(Priority::from_u32(p.to_u32()), p);
        }
        assert!(Ctl::attach(HeapRegion::new(64)).is_none());
    }

    #[test]
    fn pacing() {
        // No interval: go now.
        assert_eq!(next_deadline(0, 1000, 0), 1000);
        // Steady: one interval after the last deadline.
        assert_eq!(next_deadline(1_000, 1_500, 10_000), 11_000);
        // A little late: the cadence holds (the next one goes at once, then back on the grid).
        assert_eq!(next_deadline(1_000, 15_000, 10_000), 11_000);
        // Far behind (a loading screen): restart from now, no burst.
        assert_eq!(next_deadline(1_000, 1_000_000, 10_000), 1_000_000);
        // 100 presents arriving instantly take 99 intervals.
        let (mut d, now) = (0i64, 0i64);
        let mut t = now;
        for _ in 0..100 {
            d = next_deadline(d, t, 1000);
            t = t.max(d);
        }
        assert_eq!(t, 100 * 1000);
    }
}
