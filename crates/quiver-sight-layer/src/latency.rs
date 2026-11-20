//! `VK_NV_low_latency2` as the application drives it (SIGHT-HUD §3.17): under Proton,
//! dxvk-nvapi implements NVIDIA Reflex with it for DXVK and vkd3d-proton, and native Vulkan
//! games call it themselves. The layer intercepts three of its commands, always passing them
//! down unchanged:
//!
//! - `vkSetLatencySleepModeNV`: the Reflex mode (low latency, boost) and `minimumIntervalUs`,
//!   Reflex's frame cap;
//! - `vkSetLatencyMarkerNV`: simulation / input / present markers with their `presentID`
//!   (the application-rendered frame), timestamped with the layer's own clock
//!   (CLOCK_MONOTONIC_RAW) when the call arrives;
//! - `vkLatencySleepNV`: counted (the spec asks for exactly one per application-rendered
//!   frame), so presents per sleep is the frame-generation multiplier the layer can see.
//!
//! **Frame attribution** follows the spec's rules for `vkQueuePresentKHR` ("Frame
//! Attribution" in the `VK_NV_low_latency2` section of the WSI chapter): a present belongs to
//! the presentID of an open `OUT_OF_BAND_PRESENT_START` marker, else to the most recent
//! `PRESENT_START` marker's presentID if no present took it yet, else to no frame. The first
//! present attributed to a presentID ends that frame: its PC latency sample is the time from
//! the frame's `SIMULATION_START` (its `INPUT_SAMPLE` when marked) to the layer's
//! `vkQueuePresentKHR` returning.
//!
//! **Frame counting** (application frames vs presents): by `vkLatencySleepNV` calls while the
//! app sleeps (the first present after each sleep starts a frame), else by present markers as
//! attributed above, else not at all (no number rather than a guess). Only the HUD's target
//! swapchain is counted; layers above Sight (Smooth Motion, lsfg-vk) add presents the layer
//! never sees.
//!
//! **Timings** (`vkGetLatencyTimingsNV`) are only read when `QUIVER_SIGHT_REFLEX_TIMINGS=1`:
//! the spec doesn't say whether a query disturbs the application's own (dxvk-nvapi's
//! `NvAPI_D3D_GetLatency`, the game's Reflex stats), so the layer doesn't call it by default.
//!
//! Cost: the hooks do one device lookup (a read lock), a swapchain lookup, a clock read and a
//! few atomics; markers take a small mutex for the frame table (never held across a call).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ash::vk;
use quiver_perf::LayerStatus;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::layout::{
    REFLEX_BOOST, REFLEX_COUNTING_MARKERS, REFLEX_COUNTING_SLEEP, REFLEX_LOW_LATENCY, REFLEX_MARKERS, REFLEX_MODE_SET, REFLEX_OUT_OF_BAND, REFLEX_SLEEPING, REFLEX_TIMINGS,
};

use crate::guard;
use crate::state::{self, DeviceData, lock};

/// Markers or sleeps older than this: not live any more.
const LIVE_NS: i64 = 1_000_000_000;
/// A latency sample above this is a stall or a reset, not latency.
const MAX_LATENCY_NS: i64 = 1_000_000_000;
const SLOTS: usize = 32;

/// `1`: read `vkGetLatencyTimingsNV` reports on the heartbeat thread.
pub const ENV_TIMINGS: &str = "QUIVER_SIGHT_REFLEX_TIMINGS";

pub fn timings_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(ENV_TIMINGS).is_ok_and(|v| v == "1"))
}

/// The next layer's `VK_NV_low_latency2` commands (`None` when the device lacks them).
#[derive(Clone, Copy, Default)]
pub struct Fns {
    pub set_sleep_mode: Option<vk::PFN_vkSetLatencySleepModeNV>,
    pub sleep: Option<vk::PFN_vkLatencySleepNV>,
    pub set_marker: Option<vk::PFN_vkSetLatencyMarkerNV>,
    pub get_timings: Option<vk::PFN_vkGetLatencyTimingsNV>,
}

impl Fns {
    /// # Safety
    /// `load` returns the next layer's pointers for those names (or null).
    pub unsafe fn load(load: impl Fn(&std::ffi::CStr) -> *const std::ffi::c_void) -> Fns {
        // SAFETY: each pointer is the named command's, of the matching PFN type.
        unsafe {
            Fns {
                set_sleep_mode: cast(load(c"vkSetLatencySleepModeNV")),
                sleep: cast(load(c"vkLatencySleepNV")),
                set_marker: cast(load(c"vkSetLatencyMarkerNV")),
                get_timings: cast(load(c"vkGetLatencyTimingsNV")),
            }
        }
    }
}

unsafe fn cast<T: Copy>(p: *const std::ffi::c_void) -> Option<T> {
    // SAFETY: T is a function pointer type of the same size as a data pointer.
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*const std::ffi::c_void, T>(&p) })
}

#[derive(Clone, Copy, Default)]
struct FrameTimes {
    id: u64,
    sim_start: i64,
    input: i64,
}

#[derive(Default)]
struct Table {
    slots: [FrameTimes; SLOTS],
}

impl Table {
    fn slot(&mut self, id: u64) -> &mut FrameTimes {
        let s = &mut self.slots[(id % SLOTS as u64) as usize];
        if s.id != id {
            *s = FrameTimes { id, ..Default::default() };
        }
        s
    }

    fn start(&self, id: u64) -> Option<i64> {
        let s = &self.slots[(id % SLOTS as u64) as usize];
        (s.id == id).then_some(if s.input > 0 { s.input } else { s.sim_start }).filter(|t| *t > 0)
    }
}

/// One swapchain's Reflex state. Written by the app's latency calls (any thread) and by the
/// present path; read by the heartbeat thread into the status block.
#[derive(Default)]
pub struct Latency {
    /// `REFLEX_MODE_SET | REFLEX_LOW_LATENCY | REFLEX_BOOST` of the last sleep mode.
    mode: AtomicU32,
    min_interval_us: AtomicU32,
    markers: AtomicU64,
    sleeps: AtomicU64,
    last_present_marker: AtomicI64,
    last_sleep: AtomicI64,
    oob_seen: AtomicBool,
    /// The most recent `PRESENT_START` presentID no present took yet (0: none).
    pending: AtomicU64,
    /// An open `OUT_OF_BAND_PRESENT_START`'s presentID (0: none).
    oob_open: AtomicU64,
    table: Mutex<Table>,
    // The present path's (one present at a time per swapchain: it holds the swapchain lock).
    last_attributed: AtomicU64,
    sleeps_seen: AtomicU64,
    counting: AtomicU32,
    pub app_frames: AtomicU64,
    pub app_presents: AtomicU64,
    pc_frames: AtomicU64,
    pc_sum_us: AtomicU64,
    // The timings reader's.
    render_frames: AtomicU64,
    render_sum_us: AtomicU64,
    split_frames: AtomicU64,
    split_sum_us: [AtomicU64; 4],
    report_pc_frames: AtomicU64,
    report_pc_sum_us: AtomicU64,
    last_report: AtomicU64,
}

/// A report's game-to-render latency split into stages that add up to it, µs (simulation,
/// render submit, queue, GPU; `LATENCY_SPLIT_STAGES`): simulation start → end → render
/// submit end → GPU start → GPU end. A GPU that starts before the submit ends (it takes work
/// as it comes) leaves no queue and the GPU stage starts at the submit's end. `None` when a
/// stamp is missing or they're out of order.
pub fn split(r: &vk::LatencyTimingsFrameReportNV<'_>) -> Option<[u64; 4]> {
    let (s0, s1, submit, g0, g1) = (r.sim_start_time_us, r.sim_end_time_us, r.render_submit_end_time_us, r.gpu_render_start_time_us, r.gpu_render_end_time_us);
    if s0 == 0 || s1 < s0 || submit < s1 || g0 == 0 || g1 < submit || g1 < g0 || g1 - s0 >= MAX_LATENCY_NS as u64 / 1000 {
        return None;
    }
    let gpu_from = g0.max(submit);
    Some([s1 - s0, submit - s1, gpu_from - submit, g1 - gpu_from])
}

/// A report's PC latency to present, µs: the input sample (else simulation start) to the
/// present's end.
pub fn report_pc(r: &vk::LatencyTimingsFrameReportNV<'_>) -> Option<u64> {
    let start = if r.input_sample_time_us > 0 { r.input_sample_time_us } else { r.sim_start_time_us };
    (start > 0 && r.present_end_time_us > start && r.present_end_time_us - start < MAX_LATENCY_NS as u64 / 1000).then(|| r.present_end_time_us - start)
}

impl Latency {
    fn set_mode(&self, info: Option<&vk::LatencySleepModeInfoNV<'_>>) {
        let (mut bits, interval) = (REFLEX_MODE_SET, info.map_or(0, |i| i.minimum_interval_us));
        if let Some(i) = info {
            if i.low_latency_mode != vk::FALSE {
                bits |= REFLEX_LOW_LATENCY;
            }
            if i.low_latency_boost != vk::FALSE {
                bits |= REFLEX_BOOST;
            }
        }
        self.mode.store(bits, Ordering::Relaxed);
        self.min_interval_us.store(interval, Ordering::Relaxed);
    }

    fn marker(&self, id: u64, marker: vk::LatencyMarkerNV, now: i64) {
        self.markers.fetch_add(1, Ordering::Relaxed);
        match marker {
            vk::LatencyMarkerNV::SIMULATION_START => lock(&self.table).slot(id).sim_start = now,
            vk::LatencyMarkerNV::INPUT_SAMPLE => {
                let mut t = lock(&self.table);
                let s = t.slot(id);
                if s.input == 0 {
                    s.input = now;
                }
            }
            vk::LatencyMarkerNV::PRESENT_START => {
                self.pending.store(id, Ordering::Relaxed);
                self.last_present_marker.store(now, Ordering::Relaxed);
            }
            vk::LatencyMarkerNV::OUT_OF_BAND_PRESENT_START => {
                self.oob_open.store(id, Ordering::Relaxed);
                self.oob_seen.store(true, Ordering::Relaxed);
                self.last_present_marker.store(now, Ordering::Relaxed);
            }
            vk::LatencyMarkerNV::OUT_OF_BAND_PRESENT_END => self.oob_open.store(0, Ordering::Relaxed),
            _ => {}
        }
    }

    fn slept(&self, now: i64) {
        self.sleeps.fetch_add(1, Ordering::Relaxed);
        self.last_sleep.store(now, Ordering::Relaxed);
    }

    /// One present of this swapchain, after it returned at `ret`. Returns whether it starts
    /// an application-rendered frame (`None`: not counting).
    pub fn on_present(&self, ret: i64) -> Option<bool> {
        // Attribution (the spec's rules for vkQueuePresentKHR).
        let oob = self.oob_open.load(Ordering::Relaxed);
        let attributed = if oob != 0 {
            Some((oob, self.last_attributed.swap(oob, Ordering::Relaxed) != oob))
        } else {
            match self.pending.swap(0, Ordering::Relaxed) {
                0 => None,
                id => {
                    self.last_attributed.store(id, Ordering::Relaxed);
                    Some((id, true))
                }
            }
        };
        if let Some((id, true)) = attributed {
            // Uncontended unless a marker is being written this instant: then skip the sample.
            let start = self.table.try_lock().ok().and_then(|t| t.start(id));
            if let Some(s) = start
                && ret > s
                && ret - s < MAX_LATENCY_NS
            {
                self.pc_frames.fetch_add(1, Ordering::Relaxed);
                self.pc_sum_us.fetch_add(((ret - s) / 1000) as u64, Ordering::Relaxed);
            }
        }
        // Counting.
        let sleeping = ret - self.last_sleep.load(Ordering::Relaxed) < LIVE_NS && self.sleeps.load(Ordering::Relaxed) > 0;
        let marked = ret - self.last_present_marker.load(Ordering::Relaxed) < LIVE_NS && self.markers.load(Ordering::Relaxed) > 0;
        let first = if sleeping {
            self.counting.store(REFLEX_COUNTING_SLEEP, Ordering::Relaxed);
            let n = self.sleeps.load(Ordering::Relaxed);
            self.sleeps_seen.swap(n, Ordering::Relaxed) != n
        } else if marked {
            self.counting.store(REFLEX_COUNTING_MARKERS, Ordering::Relaxed);
            matches!(attributed, Some((_, true)))
        } else {
            self.counting.store(0, Ordering::Relaxed);
            return None;
        };
        self.app_presents.fetch_add(1, Ordering::Relaxed);
        if first {
            self.app_frames.fetch_add(1, Ordering::Relaxed);
        }
        Some(first)
    }

    /// The status block's Reflex fields at `now`.
    pub fn fill(&self, s: &mut LayerStatus, now: i64) {
        let mut f = self.mode.load(Ordering::Relaxed);
        if now - self.last_present_marker.load(Ordering::Relaxed) < LIVE_NS && self.markers.load(Ordering::Relaxed) > 0 {
            f |= REFLEX_MARKERS;
        }
        if now - self.last_sleep.load(Ordering::Relaxed) < LIVE_NS && self.sleeps.load(Ordering::Relaxed) > 0 {
            f |= REFLEX_SLEEPING;
        }
        if f & (REFLEX_MARKERS | REFLEX_SLEEPING) != 0 {
            f |= self.counting.load(Ordering::Relaxed);
        }
        if self.oob_seen.load(Ordering::Relaxed) {
            f |= REFLEX_OUT_OF_BAND;
        }
        if timings_enabled() {
            f |= REFLEX_TIMINGS;
        }
        s.reflex_flags = f;
        s.reflex_min_interval_us = self.min_interval_us.load(Ordering::Relaxed);
        s.reflex_markers = self.markers.load(Ordering::Relaxed);
        s.reflex_sleeps = self.sleeps.load(Ordering::Relaxed);
        s.app_frames = self.app_frames.load(Ordering::Relaxed);
        s.app_presents = self.app_presents.load(Ordering::Relaxed);
        s.pc_latency_frames = self.pc_frames.load(Ordering::Relaxed);
        s.pc_latency_sum_us = self.pc_sum_us.load(Ordering::Relaxed);
        s.render_latency_frames = self.render_frames.load(Ordering::Relaxed);
        s.render_latency_sum_us = self.render_sum_us.load(Ordering::Relaxed);
        s.split_frames = self.split_frames.load(Ordering::Relaxed);
        s.split_sum_us = std::array::from_fn(|i| self.split_sum_us[i].load(Ordering::Relaxed));
        s.report_pc_frames = self.report_pc_frames.load(Ordering::Relaxed);
        s.report_pc_sum_us = self.report_pc_sum_us.load(Ordering::Relaxed);
    }

    /// Game-to-render latency from timing reports (oldest first): simulation start → GPU
    /// render end, for reports newer than the last one used; its split into stages
    /// ([`split`]) and the reports' PC latency to present ([`report_pc`]).
    pub fn take_reports(&self, reports: &[vk::LatencyTimingsFrameReportNV<'_>]) {
        let mut last = self.last_report.load(Ordering::Relaxed);
        for r in reports {
            if r.present_id <= last {
                continue;
            }
            if r.sim_start_time_us > 0 && r.gpu_render_end_time_us > r.sim_start_time_us {
                let us = r.gpu_render_end_time_us - r.sim_start_time_us;
                if us < MAX_LATENCY_NS as u64 / 1000 {
                    self.render_frames.fetch_add(1, Ordering::Relaxed);
                    self.render_sum_us.fetch_add(us, Ordering::Relaxed);
                }
            }
            if let Some(parts) = split(r) {
                // The sums before the count, so a reader never sees a frame without its parts.
                for (sum, us) in self.split_sum_us.iter().zip(parts) {
                    sum.fetch_add(us, Ordering::Relaxed);
                }
                self.split_frames.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(us) = report_pc(r) {
                self.report_pc_sum_us.fetch_add(us, Ordering::Relaxed);
                self.report_pc_frames.fetch_add(1, Ordering::Relaxed);
            }
            last = r.present_id;
        }
        self.last_report.store(last, Ordering::Relaxed);
    }
}

/// The HUD target swapchain's Reflex state (set by `state::retarget`), for the status block
/// and the timings reader: (device, swapchain, state).
pub static TARGET: Mutex<Option<(vk::Device, vk::SwapchainKHR, Arc<Latency>)>> = Mutex::new(None);

/// Held around `vkGetLatencyTimingsNV` and around destroying a swapchain or device, so the
/// heartbeat thread never queries a swapchain being destroyed.
pub static QUERY: Mutex<()> = Mutex::new(());

/// The status block's Reflex fields from the target swapchain (zero without one).
pub fn fill_status(s: &mut LayerStatus, now: i64) {
    if let Some((_, _, l)) = lock(&TARGET).as_ref() {
        l.fill(s, now);
    }
}

/// Heartbeat thread, opt-in: reads the target swapchain's timing reports.
pub fn poll_timings() {
    if !timings_enabled() {
        return;
    }
    let Some((device, swapchain, lat)) = lock(&TARGET).clone() else { return };
    if now_minus(lat.last_present_marker.load(Ordering::Relaxed)) > LIVE_NS {
        return;
    }
    let _q = lock(&QUERY);
    // Still alive? (Destruction takes QUERY first, then removes it from the lists.)
    let Some(d) = state::device(device) else { return };
    if d.latency(swapchain).is_none() {
        return;
    }
    let Some(get) = d.ll2.get_timings else { return };
    let mut reports = [vk::LatencyTimingsFrameReportNV::default(); 64];
    let mut info = vk::GetLatencyMarkerInfoNV { timing_count: reports.len() as u32, p_timings: reports.as_mut_ptr(), ..Default::default() };
    // SAFETY: a live swapchain of a live device (QUERY held); 64 initialized reports.
    unsafe { get(device, swapchain, &mut info) };
    let n = (info.timing_count as usize).min(reports.len());
    lat.take_reports(&reports[..n]);
}

fn now_minus(t: i64) -> i64 {
    mono_raw_ns() - t
}

/// The swapchain's state, if the layer registered it.
fn find(device: vk::Device, swapchain: vk::SwapchainKHR) -> (Option<Arc<DeviceData>>, Option<Arc<Latency>>) {
    let d = state::device(device);
    let l = d.as_ref().and_then(|d| d.latency(swapchain));
    (d, l)
}

/// A command of a device that didn't enable the extension (the app took the pointer through
/// vkGetInstanceProcAddr): look it up now.
fn late<T: Copy>(d: Option<&Arc<DeviceData>>, device: vk::Device, name: &std::ffi::CStr) -> Option<T> {
    let d = d?;
    // SAFETY: a valid query down the chain.
    let p = unsafe { (d.next_gdpa)(device, name.as_ptr()) }.map_or(std::ptr::null(), |f| f as *const std::ffi::c_void);
    // SAFETY: the named command's pointer.
    unsafe { cast(p) }
}

pub unsafe extern "system" fn set_latency_sleep_mode_nv(device: vk::Device, swapchain: vk::SwapchainKHR, info: *const vk::LatencySleepModeInfoNV<'_>) -> vk::Result {
    let (d, l) = find(device, swapchain);
    let Some(next) = d.as_ref().and_then(|d| d.ll2.set_sleep_mode).or_else(|| late(d.as_ref(), device, c"vkSetLatencySleepModeNV")) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: the app's arguments, unchanged.
    let r = unsafe { next(device, swapchain, info) };
    if r == vk::Result::SUCCESS
        && let Some(l) = l
    {
        // SAFETY: NULL or the app's valid struct (NULL turns everything off, per the spec).
        guard("vkSetLatencySleepModeNV", || l.set_mode(unsafe { info.as_ref() }));
    }
    r
}

pub unsafe extern "system" fn latency_sleep_nv(device: vk::Device, swapchain: vk::SwapchainKHR, info: *const vk::LatencySleepInfoNV<'_>) -> vk::Result {
    let (d, l) = find(device, swapchain);
    let Some(next) = d.as_ref().and_then(|d| d.ll2.sleep).or_else(|| late(d.as_ref(), device, c"vkLatencySleepNV")) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    if let Some(l) = &l {
        guard("vkLatencySleepNV", || l.slept(mono_raw_ns()));
    }
    // SAFETY: the app's arguments, unchanged. It returns at once (the app waits on the
    // semaphore), so counting before or after is the same.
    unsafe { next(device, swapchain, info) }
}

pub unsafe extern "system" fn set_latency_marker_nv(device: vk::Device, swapchain: vk::SwapchainKHR, info: *const vk::SetLatencyMarkerInfoNV<'_>) {
    let now = mono_raw_ns();
    let (d, l) = find(device, swapchain);
    if let (Some(l), false) = (&l, info.is_null()) {
        // SAFETY: the app's valid struct.
        let (id, marker) = unsafe { ((*info).present_id, (*info).marker) };
        guard("vkSetLatencyMarkerNV", || l.marker(id, marker, now));
    }
    if let Some(next) = d.as_ref().and_then(|d| d.ll2.set_marker).or_else(|| late(d.as_ref(), device, c"vkSetLatencyMarkerNV")) {
        // SAFETY: the app's arguments, unchanged.
        unsafe { next(device, swapchain, info) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn mode(low: bool, boost: bool, us: u32) -> vk::LatencySleepModeInfoNV<'static> {
        vk::LatencySleepModeInfoNV::default().low_latency_mode(low).low_latency_boost(boost).minimum_interval_us(us)
    }

    /// `n` frames with `extra` generated presents each, as DLSS-G presents them (generated
    /// first, then the real one), at 100 µs a step from `t`.
    fn frames(l: &Latency, mut t: i64, first_id: u64, n: u64, extra: u32, sleep: bool, markers: bool) -> i64 {
        for id in first_id..first_id + n {
            if sleep {
                l.slept(t);
            }
            if markers {
                l.marker(id, vk::LatencyMarkerNV::SIMULATION_START, t);
                l.marker(id, vk::LatencyMarkerNV::SIMULATION_END, t + 2 * MS);
                l.marker(id, vk::LatencyMarkerNV::PRESENT_START, t + 5 * MS);
            }
            for _ in 0..=extra {
                t += 3 * MS;
                l.on_present(t + 5 * MS);
            }
            t += 5 * MS;
        }
        t
    }

    fn status(l: &Latency, now: i64) -> LayerStatus {
        let mut s = LayerStatus::ZERO;
        l.fill(&mut s, now);
        s
    }

    #[test]
    fn sleep_mode_is_reported() {
        let l = Latency::default();
        assert_eq!(status(&l, 0).reflex_flags & REFLEX_MODE_SET, 0);
        l.set_mode(Some(&mode(true, true, 6250)));
        let s = status(&l, 0);
        assert_eq!(s.reflex_flags & (REFLEX_MODE_SET | REFLEX_LOW_LATENCY | REFLEX_BOOST), REFLEX_MODE_SET | REFLEX_LOW_LATENCY | REFLEX_BOOST);
        assert_eq!(s.reflex_min_interval_us, 6250);
        // NULL: everything off (spec).
        l.set_mode(None);
        let s = status(&l, 0);
        assert_eq!(s.reflex_flags & (REFLEX_LOW_LATENCY | REFLEX_BOOST), 0);
        assert_eq!(s.reflex_min_interval_us, 0);
    }

    #[test]
    fn no_markers_no_sleeps_no_numbers() {
        let l = Latency::default();
        let t = frames(&l, 1000 * MS, 1, 50, 1, false, false);
        let s = status(&l, t);
        assert_eq!((s.app_frames, s.app_presents, s.pc_latency_frames), (0, 0, 0));
        assert_eq!(s.reflex_flags & (REFLEX_COUNTING_SLEEP | REFLEX_COUNTING_MARKERS), 0);
    }

    #[test]
    fn markers_count_frames_and_latency() {
        let l = Latency::default();
        // 2× frame generation: one extra present per frame, no OOB markers.
        let t = frames(&l, 1000 * MS, 1, 100, 1, false, true);
        let s = status(&l, t);
        assert_eq!(s.reflex_flags & REFLEX_COUNTING_MARKERS, REFLEX_COUNTING_MARKERS);
        assert_eq!((s.app_frames, s.app_presents), (100, 200));
        // Sim start → the first present after PRESENT_START returned: 3 + 5 ms.
        assert_eq!(s.pc_latency_frames, 100);
        assert_eq!(s.pc_latency_sum_us / s.pc_latency_frames, 8000);
        // A second later, nothing live: flags say so, counters stay.
        let s = status(&l, t + 2_000 * MS);
        assert_eq!(s.reflex_flags & (REFLEX_MARKERS | REFLEX_COUNTING_MARKERS), 0);
        assert_eq!(s.app_frames, 100);
    }

    #[test]
    fn sleeps_count_frames_without_markers() {
        let l = Latency::default();
        // 4× (MFG): three extra presents per sleep.
        let t = frames(&l, 1000 * MS, 1, 60, 3, true, false);
        let s = status(&l, t);
        assert_eq!(s.reflex_flags & REFLEX_COUNTING_SLEEP, REFLEX_COUNTING_SLEEP);
        assert_eq!((s.app_frames, s.app_presents), (60, 240));
        assert_eq!(s.pc_latency_frames, 0);
    }

    #[test]
    fn out_of_band_markers_group_presents() {
        let l = Latency::default();
        let mut t = 1000 * MS;
        for id in 1..=10u64 {
            l.marker(id, vk::LatencyMarkerNV::SIMULATION_START, t);
            l.marker(id, vk::LatencyMarkerNV::PRESENT_START, t);
            for _ in 0..3 {
                l.marker(id, vk::LatencyMarkerNV::OUT_OF_BAND_PRESENT_START, t);
                t += MS;
                assert!(l.on_present(t).is_some());
                l.marker(id, vk::LatencyMarkerNV::OUT_OF_BAND_PRESENT_END, t);
            }
        }
        let s = status(&l, t);
        assert_ne!(s.reflex_flags & REFLEX_OUT_OF_BAND, 0);
        assert_eq!((s.app_frames, s.app_presents), (10, 30));
    }

    #[test]
    fn input_sample_starts_the_latency_and_timings_add_up() {
        let l = Latency::default();
        l.marker(7, vk::LatencyMarkerNV::SIMULATION_START, 100 * MS);
        l.marker(7, vk::LatencyMarkerNV::INPUT_SAMPLE, 102 * MS);
        l.marker(7, vk::LatencyMarkerNV::PRESENT_START, 110 * MS);
        assert_eq!(l.on_present(112 * MS), Some(true));
        let s = status(&l, 112 * MS);
        assert_eq!(s.pc_latency_sum_us, 10_000);
        let r = |id, sim, gpu| vk::LatencyTimingsFrameReportNV { present_id: id, sim_start_time_us: sim, gpu_render_end_time_us: gpu, ..Default::default() };
        l.take_reports(&[r(1, 100, 20_100), r(2, 0, 9), r(3, 50, 10_050)]);
        // Read again (the same reports, as NVIDIA returns its last 64): counted once.
        l.take_reports(&[r(2, 0, 9), r(3, 50, 10_050), r(4, 10, 15_010)]);
        let s = status(&l, 112 * MS);
        assert_eq!((s.render_latency_frames, s.render_latency_sum_us), (3, 45_000));
        // Without the other stamps nothing is split, and no present end: no PC latency.
        assert_eq!((s.split_frames, s.report_pc_frames), (0, 0));
    }

    /// A full report: simulation 6 ms, submit 9, queue 4, GPU 9 (the mock-up's numbers).
    fn full(id: u64, t0: u64) -> vk::LatencyTimingsFrameReportNV<'static> {
        vk::LatencyTimingsFrameReportNV {
            present_id: id,
            input_sample_time_us: t0 + 500,
            sim_start_time_us: t0,
            sim_end_time_us: t0 + 6_000,
            render_submit_start_time_us: t0 + 6_200,
            render_submit_end_time_us: t0 + 15_000,
            present_start_time_us: t0 + 15_100,
            present_end_time_us: t0 + 15_500,
            gpu_render_start_time_us: t0 + 19_000,
            gpu_render_end_time_us: t0 + 28_000,
            ..Default::default()
        }
    }

    #[test]
    fn timing_reports_split_into_stages_that_add_up() {
        assert_eq!(split(&full(1, 1_000)), Some([6_000, 9_000, 4_000, 9_000]));
        let total: u64 = split(&full(1, 1_000)).unwrap().iter().sum();
        assert_eq!(total, 28_000, "the split adds up to the game-to-render latency");
        // The GPU started before the submit ended: no queue, the GPU stage from the submit.
        let early = vk::LatencyTimingsFrameReportNV { gpu_render_start_time_us: 1_000 + 12_000, ..full(1, 1_000) };
        assert_eq!(split(&early), Some([6_000, 9_000, 0, 13_000]));
        // A missing or out-of-order stamp: not split.
        assert_eq!(split(&vk::LatencyTimingsFrameReportNV { gpu_render_start_time_us: 0, ..full(1, 1_000) }), None);
        assert_eq!(split(&vk::LatencyTimingsFrameReportNV { sim_end_time_us: 500, ..full(1, 1_000) }), None);
        // PC latency to present from the input sample, else the simulation start.
        assert_eq!(report_pc(&full(1, 1_000)), Some(15_000));
        assert_eq!(report_pc(&vk::LatencyTimingsFrameReportNV { input_sample_time_us: 0, ..full(1, 1_000) }), Some(15_500));
        assert_eq!(report_pc(&vk::LatencyTimingsFrameReportNV { present_end_time_us: 0, ..full(1, 1_000) }), None);
        // Into the status block, each report once.
        let l = Latency::default();
        l.take_reports(&[full(1, 1_000), full(2, 40_000)]);
        l.take_reports(&[full(2, 40_000), full(3, 80_000)]);
        let s = status(&l, 0);
        assert_eq!(s.split_frames, 3);
        assert_eq!(s.split_sum_us, [18_000, 27_000, 12_000, 27_000]);
        assert_eq!((s.report_pc_frames, s.report_pc_sum_us), (3, 45_000));
        assert_eq!((s.render_latency_frames, s.render_latency_sum_us), (3, 84_000));
    }
}
