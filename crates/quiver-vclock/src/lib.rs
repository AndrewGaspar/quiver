//! `quiver lab`'s fixed-step virtual clock (design/LAB.md, "The virtual clock").
//!
//! With the clock on, a lab instance's game is told that every frame took exactly `1/rate`
//! seconds, however long it really took. That gives two things:
//!
//! - **faster than real time** when frames are cheap: a game that renders 600 fps at a 60 Hz
//!   virtual rate plays its simulation 10x as fast;
//! - **repeatable** when the machine is busy: below the virtual rate the game runs slower than
//!   real time but behaves exactly as at the virtual rate, frame for frame.
//!
//! The work is a Windows DLL (`win/hook.c`, the clock's arithmetic in `win/vclock.c`) that the
//! lab drops into the instance's install clone as a proxy for a system DLL the game imports.
//! This crate embeds that DLL, built with mingw for x86_64 and i686 and for each proxy name
//! ([`dll`]), links the same `vclock.c` for the host ([`timing`], and the tests), and reads the
//! DLL's report ([`Report`]).
//!
//! Prior art: the virtual clock of the ds-decomp project's RIG shim (`docs/17-virtual-clock.md`
//! there): hook the game's import tables, not the exports, so Wine's and DXVK's own clocks stay
//! real; advance exactly one frame per Present; creep, escape and never-backwards so a game
//! waiting on the clock can't hang.

use std::path::Path;

use serde::Serialize;

include!(concat!(env!("OUT_DIR"), "/dlls.rs"));

pub mod control;

/// The system DLLs the hook can stand in for, in the order the lab prefers them.
pub const PROXIES: &[&str] = &["version", "winmm", "dinput8", "dsound"];

/// A string every build of the DLL carries, so the lab can recognise (and remove) one it left.
pub const MARKER: &[u8] = b"QUIVER-VCLOCK-DLL";

/// Files the DLL writes into its folder (`QUIVER_VCLOCK_DIR`, else `QUIVER_LAB_RUN_DIR`).
pub const LOG_FILE: &str = "vclock.log";
pub const STATUS_FILE: &str = "vclock.status";

/// The DLL for a game's architecture (`x64` or `x86`, as [`quiver_core`'s PE facts] say) as
/// a proxy for `proxy`.dll, or `None` when this build couldn't make it.
pub fn dll(arch: &str, proxy: &str) -> Option<&'static [u8]> {
    DLLS.iter().find(|(a, p, _)| *a == arch && *p == proxy).map(|(_, _, b)| *b)
}

/// Why [`dll`] is `None` for `arch`, if it is.
pub fn unavailable(arch: &str) -> Option<String> {
    match arch {
        "x64" | "x86" if MISSING.contains(&arch) => Some(format!(
            "this Quiver was built without {} (mingw-w64), so it has no {arch} virtual-clock DLL",
            if arch == "x64" { "x86_64-w64-mingw32-gcc" } else { "i686-w64-mingw32-gcc" }
        )),
        "x64" | "x86" => None,
        a => Some(format!("the virtual clock supports x64 and x86 games, not {a}")),
    }
}

/// Whether a file is one of these DLLs (any build).
pub fn is_ours(bytes: &[u8]) -> bool {
    bytes.windows(MARKER.len()).any(|w| w == MARKER)
}

mod ffi {
    use std::ffi::c_void;

    pub type RealNow = extern "C" fn(*mut c_void) -> i64;

    /// `vc_clock` (win/vclock.h), field for field.
    #[repr(C)]
    pub struct VcClock {
        pub rate: f64,
        pub real_freq: i64,
        pub ticks_per_frame: i64,
        pub vfreq: i64,
        pub creep_per_query: i64,
        pub stuck_real: i64,
        pub creep_after: u64,
        pub stuck_ms: u32,
        pub real_origin: i64,
        pub real_now: Option<RealNow>,
        pub ctx: *mut c_void,
        pub engaged: i32,
        pub vnow: i64,
        pub frame_base: i64,
        pub real_frame_start: i64,
        pub generation: u64,
        pub frame_max_reads: u64,
        pub frame_escaped: u32,
        pub engaged_real: i64,
        pub engaged_v: i64,
        pub frames: u64,
        pub queries_total: u64,
        pub queries_max: u64,
        pub creep_frames: u64,
        pub escapes: u64,
        pub overruns: u64,
        pub sleeps: u64,
        pub sleep_ms_requested: u64,
        pub sleep_ms_slept: u64,
        pub sleeps_real: u64,
        pub hold: i32,
        pub held: i32,
        pub hold_at: u64,
        pub hold_timeout_real: i64,
        pub held_since: i64,
        pub held_v: i64,
        pub pub_lock: i32,
        pub pad0: i32,
        pub pauses: u64,
        pub steps: u64,
        pub stepped: u64,
        pub holds: u64,
        pub held_real: i64,
        pub held_reads: u64,
        pub leaked: i64,
        pub auto_resumes: u64,
    }

    /// `vc_thread`.
    #[repr(C)]
    #[derive(Default)]
    pub struct VcThread {
        pub frame: u64,
        pub reads: u64,
    }

    /// `vc_vhook` (win/vclock.h): one vtable hook's entry.
    #[repr(C)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub struct VcVhook {
        pub vt: *mut *mut c_void,
        pub hook: *mut c_void,
        pub orig: *mut c_void,
        pub slot: u32,
        pub noted: u32,
    }

    /// `vc_vtreg`: the vtable hook registry.
    #[repr(C)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub struct VcVtreg {
        pub e: [VcVhook; 128],
        pub n: i32,
    }

    unsafe extern "C" {
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_vt_plan(r: *mut VcVtreg, vt: *mut *mut c_void, slot: u32, cur: *mut c_void, hook: *mut c_void, entry: *mut *mut VcVhook) -> i32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_vt_orig(r: *const VcVtreg, vt: *mut *mut c_void, slot: u32, hook: *mut c_void) -> *mut c_void;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_vtreg_size() -> u64;
        pub fn vc_timing(rate: f64, real_freq: i64, tpf: *mut i64, vfreq: *mut i64) -> i32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_init(c: *mut VcClock, rate: f64, real_freq: i64, creep_after: u32, stuck_ms: u32, real_now: RealNow, ctx: *mut c_void) -> i32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_now(c: *mut VcClock, t: *mut VcThread) -> i64;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_present(c: *mut VcClock);
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_sleep(c: *mut VcClock, requested: u32, max_ms: u32, shorten: i32) -> u32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_ticks_ms(c: *const VcClock, ticks: i64) -> u64;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_ticks_100ns(c: *const VcClock, ticks: i64) -> u64;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_virtual_seconds(c: *mut VcClock) -> f64;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_real_seconds(c: *mut VcClock) -> f64;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_hold_cmd(c: *mut VcClock, cmd: i32, arg: u64, timeout_ms: u32) -> u64;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_must_hold(c: *mut VcClock) -> i32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_hold_begin(c: *mut VcClock);
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_hold_end(c: *mut VcClock);
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_hold_expired(c: *mut VcClock) -> i32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_ctl_attach(k: *mut super::control::Page, c: *mut VcClock, pid: u64);
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_ctl_poll(k: *mut super::control::Page, c: *mut VcClock, cmd: *mut u64, arg: *mut u64) -> i32;
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn vc_ctl_publish(k: *mut super::control::Page, c: *mut VcClock);
    }
}

/// Wine's performance counter frequency (and Windows' on most machines since Windows 10).
pub const WINE_QPF: i64 = 10_000_000;

/// What a rate means on a counter, from the same C the DLL runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Timing {
    pub rate: f64,
    pub ticks_per_frame: i64,
    /// What QueryPerformanceFrequency reports to the game.
    pub qpf: i64,
    pub real_qpf: i64,
    /// Every frame is exactly `1/rate` s to a QPC reader (`ticks_per_frame * rate == qpf`).
    pub exact_qpc: bool,
    /// The frame period is a whole number of milliseconds, so timeGetTime and GetTickCount
    /// readers see the same delta every frame (125 Hz, 100 Hz, 62.5 Hz...).
    pub exact_ms: bool,
    pub period_ms: f64,
}

pub fn timing(rate: f64, real_qpf: i64) -> Option<Timing> {
    let (mut tpf, mut qpf) = (0i64, 0i64);
    // SAFETY: plain arithmetic on two out-pointers to locals.
    if unsafe { ffi::vc_timing(rate, real_qpf, &mut tpf, &mut qpf) } == 0 {
        return None;
    }
    let period_ms = 1000.0 / rate;
    Some(Timing {
        rate,
        ticks_per_frame: tpf,
        qpf,
        real_qpf,
        exact_qpc: (tpf as f64 * rate - qpf as f64).abs() < 1e-6,
        exact_ms: (period_ms - period_ms.round()).abs() < 1e-9,
        period_ms,
    })
}

/// A rate: `60`, `62.5`, `125Hz`. Positive and at most 10 kHz.
pub fn parse_rate(s: &str) -> Result<f64, String> {
    let t = s.trim().trim_end_matches(['H', 'h', 'z', 'Z']).trim();
    let r: f64 = t.parse().map_err(|_| format!("'{s}': a virtual frame rate in Hz, e.g. 60 or 62.5"))?;
    if !(r.is_finite() && r > 0.0 && r <= 10_000.0) {
        return Err(format!("'{s}': the rate must be above 0 and at most 10000 Hz"));
    }
    Ok(r)
}

/// The DLL's `vclock.status`: rewritten every half second while the game runs and once
/// at exit.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    /// `installed`, `engaged`, `periodic` or `exit`.
    pub why: String,
    pub pid: u32,
    pub proxy: String,
    pub arch: String,
    pub rate: f64,
    pub ticks_per_frame: i64,
    pub qpf: i64,
    pub real_qpf: i64,
    pub engaged: bool,
    /// The present path that drives the clock (`dxgi`, `d3d9`, `d3d8`, `opengl`, `vulkan`, `none`).
    pub present_path: String,
    pub ignored_presents: u64,
    /// Imports and vtable methods hooked.
    pub hooked: String,
    pub patched_slots: u64,
    pub vtable_hooks: u64,
    pub modules: String,
    pub frames: u64,
    /// Since engagement.
    pub virtual_s: f64,
    pub real_s: f64,
    pub speedup: f64,
    pub queries: u64,
    pub worst_frame_queries: u64,
    pub creep_after: u64,
    pub creep_frames: u64,
    pub stuck_ms: u64,
    pub escapes: u64,
    pub overruns: u64,
    pub sleeps: u64,
    pub sleep_ms_requested: u64,
    pub sleep_ms_slept: u64,
    pub sleep_max_ms: u64,
    pub sync0: bool,
    pub since_install_s: f64,
    /// The distinct steps between successive values the game read from the performance
    /// counter (ticks), most common first, and how many steps fit no bin.
    pub census_qpc: Vec<(i64, u64)>,
    pub census_qpc_other: u64,
    /// The same for the millisecond clocks (timeGetTime, GetTickCount).
    pub census_ms: Vec<(i64, u64)>,
    pub census_ms_other: u64,
    /// Sleeps left as asked (threads other than the presenting one, by default).
    pub sleeps_real: u64,
    /// Whose sleeps get shorter: `present` (the presenting thread's), `all`, `none`.
    pub sleep_mode: String,
    /// The control page is mapped (pause and step reach this process).
    pub control: bool,
    /// `running`, `holding` (asked to hold; the frame in flight hasn't presented) or `held`.
    pub hold_state: String,
    pub hold_at: u64,
    /// Holds started (a pause, or a step from running).
    pub pauses: u64,
    /// Frames asked for by steps.
    pub steps: u64,
    /// Presents while a hold was on.
    pub stepped: u64,
    /// Times the game blocked at its Present.
    pub holds: u64,
    pub held_s: f64,
    pub held_reads: u64,
    /// Virtual ticks time moved while held (0 by design).
    pub leaked_ticks: i64,
    pub auto_resumes: u64,
}

fn parse_census(v: &str) -> Vec<(i64, u64)> {
    let mut out: Vec<(i64, u64)> = v
        .split(',')
        .filter_map(|b| {
            let (d, n) = b.split_once(':')?;
            Some((d.trim().parse().ok()?, n.trim().parse().ok()?))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// `16 ms x5990 (99.8%), 32 ms x10`: a census, most common first.
fn census_line(bins: &[(i64, u64)], other: u64, unit: &str) -> String {
    let total: u64 = bins.iter().map(|b| b.1).sum::<u64>() + other;
    let mut parts: Vec<String> = bins.iter().take(6).map(|(d, n)| format!("{d}{unit} x{n} ({:.1}%)", *n as f64 * 100.0 / total.max(1) as f64)).collect();
    if bins.len() > 6 {
        parts.push(format!("{} more value(s)", bins.len() - 6));
    }
    if other > 0 {
        parts.push(format!("{other} other"));
    }
    parts.join(", ")
}

impl Report {
    pub fn parse(text: &str) -> Report {
        let mut r = Report::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let n = || v.trim().parse::<u64>().unwrap_or(0);
            let i = || v.trim().parse::<i64>().unwrap_or(0);
            let f = || v.trim().parse::<f64>().unwrap_or(0.0);
            let s = || if v == "-" { String::new() } else { v.to_owned() };
            match k {
                "why" => r.why = s(),
                "pid" => r.pid = n() as u32,
                "proxy" => r.proxy = s(),
                "arch" => r.arch = s(),
                "rate" => r.rate = f(),
                "ticks_per_frame" => r.ticks_per_frame = i(),
                "qpf" => r.qpf = i(),
                "real_qpf" => r.real_qpf = i(),
                "engaged" => r.engaged = n() != 0,
                "present_path" => r.present_path = s(),
                "ignored_presents" => r.ignored_presents = n(),
                "hooked" => r.hooked = s(),
                "patched_slots" => r.patched_slots = n(),
                "vtable_hooks" => r.vtable_hooks = n(),
                "modules" => r.modules = s(),
                "frames" => r.frames = n(),
                "virtual_s" => r.virtual_s = f(),
                "real_s" => r.real_s = f(),
                "speedup" => r.speedup = f(),
                "queries" => r.queries = n(),
                "worst_frame_queries" => r.worst_frame_queries = n(),
                "creep_after" => r.creep_after = n(),
                "creep_frames" => r.creep_frames = n(),
                "stuck_ms" => r.stuck_ms = n(),
                "escapes" => r.escapes = n(),
                "overruns" => r.overruns = n(),
                "sleeps" => r.sleeps = n(),
                "sleep_ms_requested" => r.sleep_ms_requested = n(),
                "sleep_ms_slept" => r.sleep_ms_slept = n(),
                "sleep_max_ms" => r.sleep_max_ms = n(),
                "sync0" => r.sync0 = n() != 0,
                "since_install_s" => r.since_install_s = f(),
                "census_qpc" => r.census_qpc = parse_census(v),
                "census_qpc_other" => r.census_qpc_other = n(),
                "census_ms" => r.census_ms = parse_census(v),
                "census_ms_other" => r.census_ms_other = n(),
                "sleeps_real" => r.sleeps_real = n(),
                "sleep_mode" => r.sleep_mode = s(),
                "control" => r.control = n() != 0,
                "hold_state" => r.hold_state = s(),
                "hold_at" => r.hold_at = n(),
                "pauses" => r.pauses = n(),
                "steps" => r.steps = n(),
                "stepped" => r.stepped = n(),
                "holds" => r.holds = n(),
                "held_s" => r.held_s = f(),
                "held_reads" => r.held_reads = n(),
                "leaked_ticks" => r.leaked_ticks = i(),
                "auto_resumes" => r.auto_resumes = n(),
                _ => {}
            }
        }
        r
    }

    /// The report in a run folder, if the DLL wrote one.
    pub fn read(dir: &Path) -> Option<Report> {
        std::fs::read_to_string(dir.join(STATUS_FILE)).ok().map(|t| Report::parse(&t))
    }

    /// One line: engaged or not, frames, virtual vs real time, speed-up.
    pub fn summary(&self) -> String {
        if !self.engaged {
            let hooked = if self.hooked.is_empty() { "nothing hooked yet".to_owned() } else { format!("hooked: {}", self.hooked) };
            return format!(
                "virtual clock {:.4} Hz via {}, NOT engaged: no Present seen yet, so the game still runs on real time ({hooked}; {:.1} s since load)",
                self.rate, self.proxy, self.since_install_s
            );
        }
        let pace = if self.speedup >= 1.0 {
            "faster than real time".to_owned()
        } else if self.speedup > 0.0 {
            format!("slower than real time: frames take longer than 1/{} s here, and the game behaves as at {} Hz all the same", self.rate, self.rate)
        } else {
            "no time measured yet".to_owned()
        };
        format!(
            "virtual clock engaged at {:.4} Hz via {} ({} presents): {} frames, {:.1} s virtual in {:.1} s real = {:.2}x ({pace})",
            self.rate, self.proxy, self.present_path, self.frames, self.virtual_s, self.real_s, self.speedup
        )
    }

    /// The safety valves, counted.
    pub fn counters(&self) -> String {
        format!(
            "creeping frames {} (creep after {} reads by one thread in a frame; the most: {}), real-time escapes {} (after {} ms), overruns {}, sleeps shortened {} ({} ms asked, {} ms slept; shortened on: {}), kept real {}, presents ignored from a second path {}",
            self.creep_frames,
            self.creep_after,
            self.worst_frame_queries,
            self.escapes,
            self.stuck_ms,
            self.overruns,
            self.sleeps,
            self.sleep_ms_requested,
            self.sleep_ms_slept,
            match self.sleep_mode.as_str() {
                "all" => "every thread",
                "none" => "no thread",
                _ => "the presenting thread",
            },
            self.sleeps_real,
            self.ignored_presents
        )
    }

    /// Pause and step (`quiver lab clock`), once they were used: the state and the counters.
    pub fn hold_line(&self) -> Option<String> {
        let used = self.pauses > 0 || self.holds > 0 || matches!(self.hold_state.as_str(), "held" | "holding");
        if !used {
            return None;
        }
        let state = match self.hold_state.as_str() {
            "held" => format!("HELD at frame {} (virtual time stopped)", self.frames),
            "holding" => format!("holding: stops at frame {}", self.hold_at),
            _ => "running free".into(),
        };
        Some(format!(
            "pause/step: {state}; {} hold(s) started, {} step frame(s) asked, {} present(s) while holding, blocked {} time(s), {:.1} s held (left out of the speed-up), {} clock read(s) while held (frozen; no creep, no escape), leaked {} tick(s){}",
            self.pauses,
            self.steps,
            self.stepped,
            self.holds,
            self.held_s,
            self.held_reads,
            self.leaked_ticks,
            if self.auto_resumes > 0 { format!(", {} hold(s) ended by their timeout", self.auto_resumes) } else { String::new() }
        ))
    }

    /// What the game saw: the steps between successive clock values.
    pub fn census(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.census_qpc.is_empty() || self.census_qpc_other > 0 {
            let frame = if self.qpf > 0 { format!(" (a frame is {} ticks at QPF {})", self.ticks_per_frame, self.qpf) } else { String::new() };
            v.push(format!("performance counter steps: {}{frame}", census_line(&self.census_qpc, self.census_qpc_other, " ticks")));
        }
        if !self.census_ms.is_empty() || self.census_ms_other > 0 {
            v.push(format!("millisecond clock steps: {}", census_line(&self.census_ms, self.census_ms_other, " ms")));
        }
        v
    }

    /// Every step the game read from the performance counter was exactly one frame.
    pub fn exact(&self) -> bool {
        self.engaged && self.census_qpc_other == 0 && self.census_qpc.iter().all(|(d, _)| *d == self.ticks_per_frame)
    }

    /// Whether the run's timing can be repeated: false once a frame was released to real time
    /// by the escape (its time followed the machine, not the frame count).
    pub fn deterministic(&self) -> bool {
        self.escapes == 0
    }

    /// What to make of the numbers.
    pub fn verdict(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.engaged {
            v.push("no Present was hooked: the game's present path isn't one the virtual clock knows (D3D8/9, DXGI, OpenGL SwapBuffers, Vulkan), or it presents from a module it doesn't patch; see vclock.log".into());
            return v;
        }
        if !self.deterministic() {
            v.push(format!(
                "NOT FULLY DETERMINISTIC: {} frame(s) took over {} ms of real time and were released to real time (the escape); their timing followed the machine, so another run can differ there (a loading screen without presents, or frames slower than the escape: raise clock.stuck_ms)",
                self.escapes, self.stuck_ms
            ));
        }
        if self.leaked_ticks != 0 {
            v.push(format!(
                "virtual time moved {} tick(s) ({:.6} s) while the game was held: pause/step slipped there (a race between a clock read and the hold)",
                self.leaked_ticks,
                if self.qpf > 0 { self.leaked_ticks as f64 / self.qpf as f64 } else { 0.0 }
            ));
        }
        if self.creep_frames > 0 {
            v.push(format!(
                "{} of {} frames crept (a thread read the clock over {} times in one frame: a busy-wait): their time isn't exactly 1/{} s, though it depends on the reads, not the machine (if it's a polling thread rather than a wait, raise clock.creep_after; if its sleeps are shortened, clock.sleeps=present)",
                self.creep_frames, self.frames, self.creep_after, self.rate
            ));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::ffi::*;
    use super::*;

    /// A clock with a hand-driven real counter (10 MHz).
    struct Fake {
        c: Box<VcClock>,
        real: Box<AtomicI64>,
        /// The "main" thread's reads.
        t: Box<VcThread>,
    }

    extern "C" fn read_fake(ctx: *mut c_void) -> i64 {
        // SAFETY: ctx is the Box<AtomicI64> the Fake owns.
        unsafe { (*(ctx as *const AtomicI64)).load(Ordering::SeqCst) }
    }

    impl Fake {
        fn new(rate: f64, creep_after: u32, stuck_ms: u32) -> Fake {
            let real = Box::new(AtomicI64::new(1_000_000_000));
            // SAFETY: VcClock is plain data; vc_init fills it.
            let mut c: Box<VcClock> = Box::new(unsafe { std::mem::zeroed() });
            let ok = unsafe { vc_init(&mut *c, rate, WINE_QPF, creep_after, stuck_ms, read_fake, &*real as *const AtomicI64 as *mut c_void) };
            assert_eq!(ok, 1);
            Fake { c, real, t: Box::default() }
        }
        fn advance_real_ms(&self, ms: i64) {
            self.real.fetch_add(ms * 10_000, Ordering::SeqCst);
        }
        fn now(&mut self) -> i64 {
            unsafe { vc_now(&mut *self.c, &mut *self.t) }
        }
        fn now_on(&mut self, t: &mut VcThread) -> i64 {
            unsafe { vc_now(&mut *self.c, t) }
        }
        fn present(&mut self) {
            unsafe { vc_present(&mut *self.c) }
        }
        fn ms(&self, t: i64) -> u64 {
            unsafe { vc_ticks_ms(&*self.c, t) }
        }
    }

    #[test]
    fn ticks_per_frame_are_whole_and_the_frequency_follows() {
        let t = timing(60.0, WINE_QPF).unwrap();
        assert_eq!((t.ticks_per_frame, t.qpf), (166_667, 10_000_020));
        assert!(t.exact_qpc && !t.exact_ms);
        let t = timing(125.0, WINE_QPF).unwrap();
        assert_eq!((t.ticks_per_frame, t.qpf), (80_000, 10_000_000));
        assert!(t.exact_qpc && t.exact_ms);
        let t = timing(62.5, WINE_QPF).unwrap();
        assert_eq!((t.ticks_per_frame, t.qpf), (160_000, 10_000_000));
        assert!(t.exact_qpc && t.exact_ms && t.period_ms == 16.0);
        let t = timing(165.0, WINE_QPF).unwrap();
        assert_eq!(t.ticks_per_frame * 165, t.qpf);
        // A rate that isn't a whole number can't always be exact.
        let t = timing(59.94, WINE_QPF).unwrap();
        assert!(!t.exact_qpc, "{t:?}");
        // A slow counter still gets at least one tick per frame.
        assert_eq!(timing(1000.0, 100).unwrap().ticks_per_frame, 1);
        assert!(timing(0.0, WINE_QPF).is_none() && timing(-1.0, WINE_QPF).is_none() && timing(60.0, 0).is_none());
    }

    #[test]
    fn rates_parse() {
        assert_eq!(parse_rate("60").unwrap(), 60.0);
        assert_eq!(parse_rate("62.5Hz").unwrap(), 62.5);
        assert!(parse_rate("0").is_err() && parse_rate("fast").is_err() && parse_rate("1e9").is_err());
    }

    #[test]
    fn each_frame_is_exactly_one_period_however_long_it_took() {
        let mut f = Fake::new(62.5, 4096, 250);
        // Pass-through before the first Present: real time.
        let t0 = f.now();
        f.advance_real_ms(100);
        assert_eq!(f.now() - t0, 1_000_000);
        f.present(); // engages, at the next whole virtual second
        assert_eq!(f.now(), WINE_QPF);
        let mut last = f.now();
        let mut last_ms = f.ms(last);
        for frame in 0..1000 {
            // Real frames of 1..=20 ms: the game sees 16 ms (160000 ticks) every time.
            f.advance_real_ms(1 + frame % 20);
            for _ in 0..10 {
                assert_eq!(f.now(), last, "time moved inside a frame");
            }
            f.present();
            let now = f.now();
            assert_eq!(now - last, 160_000);
            let ms = f.ms(now);
            assert_eq!(ms - last_ms, 16);
            (last, last_ms) = (now, ms);
        }
        assert_eq!(f.c.frames, 1000);
        assert_eq!((f.c.creep_frames, f.c.escapes, f.c.overruns), (0, 0, 0));
        // 1000 frames of 16 ms = 16 s virtual in 10.5 s real.
        let (vs, rs) = unsafe { (vc_virtual_seconds(&mut *f.c), vc_real_seconds(&mut *f.c)) };
        assert!((vs - 16.0).abs() < 1e-9, "{vs}");
        assert!((rs - 10.5).abs() < 1e-9, "{rs}");
    }

    #[test]
    fn a_busy_wait_terminates_by_creeping() {
        let mut f = Fake::new(60.0, 4096, 250);
        f.present();
        f.present();
        let tpf = f.c.ticks_per_frame;
        // A limiter that wants two frames of time before it presents (it can only ever see
        // one per Present): spins on the clock, and real time doesn't move at all.
        let start = f.now();
        let mut spins = 0u64;
        while f.now() - start < 2 * tpf {
            spins += 1;
            assert!(spins < 10_000, "the creep never released the wait");
        }
        // 4096 queries at the grid, then tpf/1024 per query: ~2048 more for two frames.
        assert!((4096..4096 + 2100).contains(&spins), "{spins}");
        let before = f.now();
        f.present();
        assert_eq!(f.c.creep_frames, 1);
        // Past the grid already: the Present keeps the larger value (an overrun), never goes back.
        assert_eq!(f.c.overruns, 1);
        assert!(f.now() >= before);
        // And the next frame is on the grid again.
        let t = f.now();
        f.present();
        assert_eq!(f.now() - t, tpf);
        assert_eq!(f.c.creep_frames, 1);
    }

    #[test]
    fn many_threads_reading_a_little_dont_creep_but_one_spinning_does() {
        let mut f = Fake::new(62.5, 4096, 1000);
        f.present();
        // 64 threads (a job system, timer threads polling with real sleeps) each read the clock
        // 1000 times a frame: 64000 reads a frame in all, none of them a spin.
        let mut threads: Vec<VcThread> = (0..64).map(|_| VcThread::default()).collect();
        for _ in 0..50 {
            let base = f.now();
            for t in threads.iter_mut() {
                for _ in 0..1000 {
                    assert_eq!(f.now_on(t), base, "time moved inside a frame");
                }
            }
            f.present();
            assert_eq!(f.now() - base, 160_000);
        }
        assert_eq!((f.c.creep_frames, f.c.overruns), (0, 0));
        assert_eq!(f.c.queries_max, 1000);
        // One background thread spinning past creep_after does creep (and its wait ends),
        // whether or not it is the presenting thread.
        let start = f.now();
        let mut spin = VcThread::default();
        let mut n = 0;
        while f.now_on(&mut spin) - start < 160_000 {
            n += 1;
            assert!(n < 10_000);
        }
        f.present();
        assert_eq!(f.c.creep_frames, 1);
        // A thread's count starts again each frame.
        let t = f.now();
        for _ in 0..4000 {
            assert_eq!(f.now_on(&mut spin), t);
        }
        f.present();
        assert_eq!(f.c.creep_frames, 1);
    }

    #[test]
    fn a_stuck_frame_escapes_to_real_time() {
        let mut f = Fake::new(60.0, 1_000_000, 250);
        f.present();
        let t0 = f.now();
        // A loading screen: few queries, no Present, real time passes.
        f.advance_real_ms(200);
        assert_eq!(f.now(), t0, "frozen before stuck_ms");
        f.advance_real_ms(100); // 300 ms into the frame
        let t1 = f.now();
        // Released: virtual = base + (300 - 250) ms (in virtual ticks, at a QPF 2 ppm off).
        assert!((t1 - t0 - 500_001).abs() <= 1, "{}", t1 - t0);
        assert_eq!(f.c.escapes, 1);
        f.advance_real_ms(1000);
        let t2 = f.now();
        assert!((t2 - t1 - 10_000_020).abs() <= 1);
        assert_eq!(f.c.escapes, 1, "one escape per frame");
        // The Present after it: past the grid, so it stays (overrun), never backwards.
        f.present();
        assert_eq!(f.c.overruns, 1);
        assert!(f.now() >= t2);
        let t3 = f.now();
        f.advance_real_ms(5);
        f.present();
        assert_eq!(f.now() - t3, f.c.ticks_per_frame);
    }

    #[test]
    fn time_never_goes_backwards() {
        let mut f = Fake::new(144.0, 64, 250);
        let mut last = 0;
        for i in 0..5000u64 {
            if i % 7 == 0 {
                f.advance_real_ms((i % 13) as i64 * 30);
            }
            if i % 3 == 0 {
                f.present();
            }
            let n = f.now();
            assert!(n >= last, "went back at {i}: {last} -> {n}");
            last = n;
        }
        assert!(f.c.creep_frames + f.c.escapes + f.c.overruns > 0);
    }

    #[test]
    fn sleeps_shorten_and_never_move_time() {
        let mut f = Fake::new(60.0, 4096, 250);
        // Before engagement a Sleep is a Sleep.
        assert_eq!(unsafe { vc_sleep(&mut *f.c, 10, 0, 1) }, 10);
        f.present();
        let t = f.now();
        assert_eq!(unsafe { vc_sleep(&mut *f.c, 10, 0, 1) }, 0);
        assert_eq!(unsafe { vc_sleep(&mut *f.c, 10, 2, 1) }, 2);
        assert_eq!(unsafe { vc_sleep(&mut *f.c, 1, 2, 1) }, 1);
        // Not shortened (a thread that doesn't present): as asked, counted apart.
        assert_eq!(unsafe { vc_sleep(&mut *f.c, 100, 0, 0) }, 100);
        assert_eq!(f.now(), t);
        assert_eq!((f.c.sleeps, f.c.sleep_ms_requested, f.c.sleep_ms_slept, f.c.sleeps_real), (3, 21, 3, 1));
    }

    #[test]
    fn filetime_and_ms_scale_without_overflow() {
        let f = Fake::new(62.5, 4096, 250);
        let day = 86_400 * 10_000_000i64 * 30; // a month of ticks
        assert_eq!(unsafe { vc_ticks_100ns(&*f.c, day) }, day as u64);
        assert_eq!(f.ms(day), (day / 10_000) as u64);
    }

    #[test]
    fn reports_parse_and_explain() {
        let text = "version=1\nwhy=periodic\nproxy=version.dll\narch=x64\nrate=62.500000\nticks_per_frame=160000\nqpf=10000000\nengaged=1\npresent_path=dxgi\nhooked=QueryPerformanceCounter dxgi.Present\nframes=6000\nvirtual_s=96.0\nreal_s=12.0\nspeedup=8.0\ncreep_after=4096\ncreep_frames=2\nstuck_ms=1000\nescapes=3\noverruns=3\ncensus_qpc=160000:5990,156:40\ncensus_qpc_other=0\ncensus_ms=16:5999\ncensus_ms_other=0\n";
        let r = Report::parse(text);
        assert!(r.engaged && r.frames == 6000 && r.speedup == 8.0 && r.present_path == "dxgi");
        assert!(r.summary().contains("96.0 s virtual in 12.0 s real = 8.00x (faster than real time)"), "{}", r.summary());
        assert!(r.counters().contains("real-time escapes 3"));
        assert!(!r.deterministic() && r.verdict().iter().any(|v| v.starts_with("NOT FULLY DETERMINISTIC")));
        assert_eq!(r.census_qpc, vec![(160000, 5990), (156, 40)]);
        assert!(!r.exact());
        let c = r.census();
        assert!(c[0].contains("160000 ticks x5990 (99.3%)") && c[1].contains("16 ms x5999 (100.0%)"), "{c:?}");
        // Slower than real time is normal: it says so, and isn't flagged.
        assert!(r.counters().contains("shortened on: the presenting thread"), "{}", r.counters());
        let slow = Report::parse("rate=62.5\nengaged=1\nproxy=version.dll\nticks_per_frame=160000\nframes=100\nvirtual_s=1.6\nreal_s=2.5\nspeedup=0.64\ncensus_qpc=160000:99\n");
        assert!(slow.summary().contains("0.64x (slower than real time") && slow.verdict().is_empty() && slow.exact() && slow.deterministic(), "{}", slow.summary());
        let idle = Report::parse("rate=60\nproxy=winmm.dll\nengaged=0\nhooked=-\n");
        assert!(idle.summary().contains("NOT engaged") && idle.verdict()[0].contains("no Present"));
    }

    #[test]
    fn the_dlls_are_built_and_marked() {
        for arch in ["x64", "x86"] {
            if let Some(why) = unavailable(arch) {
                eprintln!("skipping {arch}: {why}");
                continue;
            }
            for p in PROXIES {
                let b = dll(arch, p).unwrap_or_else(|| panic!("no {p} {arch}"));
                assert!(b.starts_with(b"MZ") && is_ours(b), "{p} {arch}");
            }
        }
        assert!(!is_ours(b"MZ plain dll"));
        assert!(unavailable("arm64").is_some());
    }

    // ---- Pause and step ------------------------------------------------------------------

    impl Fake {
        fn cmd(&mut self, cmd: i32, arg: u64) -> u64 {
            unsafe { vc_hold_cmd(&mut *self.c, cmd, arg, 0) }
        }
        fn must_hold(&mut self) -> bool {
            unsafe { vc_must_hold(&mut *self.c) != 0 }
        }
        fn hold_begin(&mut self) {
            unsafe { vc_hold_begin(&mut *self.c) }
        }
        fn hold_end(&mut self) {
            unsafe { vc_hold_end(&mut *self.c) }
        }
    }

    const PAUSE: i32 = 1;
    const STEP: i32 = 2;
    const RESUME: i32 = 3;

    /// What the hook does at a Present (hook.c's on_present), with the controller's part done
    /// by `held` each time the game blocks: it may advance real time, read the clock from
    /// other threads, and must eventually issue a command that releases the game.
    fn present_gated(f: &mut Fake, held: &mut dyn FnMut(&mut Fake)) {
        while f.must_hold() {
            f.hold_begin();
            held(f);
            f.hold_end();
        }
        f.present();
        while f.must_hold() {
            f.hold_begin();
            let mut n = 0;
            while f.must_hold() {
                held(f);
                n += 1;
                assert!(n < 1000, "the controller never released the game");
            }
            f.hold_end();
        }
    }

    #[test]
    fn a_pause_holds_at_the_end_of_the_frame_in_flight_and_steps_count_presents() {
        let mut f = Fake::new(62.5, 4096, 1000);
        f.present();
        for _ in 0..10 {
            f.present();
        }
        assert_eq!(f.c.frames, 10);
        // Pause mid-frame 11: the frame finishes, presents, the clock moves on, then it holds.
        assert_eq!(f.cmd(PAUSE, 0), 11);
        assert!(!f.must_hold(), "the frame in flight presents first");
        f.present();
        assert!(f.must_hold());
        f.hold_begin();
        let frozen = f.now();
        assert_eq!(frozen, WINE_QPF + 11 * 160_000);
        // Ten seconds of real time held, another thread hammering the clock: nothing moves,
        // no creep, no escape.
        let mut other = VcThread::default();
        f.advance_real_ms(10_000);
        for _ in 0..100_000 {
            assert_eq!(f.now_on(&mut other), frozen);
            assert_eq!(f.now(), frozen);
        }
        assert_eq!((f.c.escapes, f.c.creep_frames), (0, 0));
        assert_eq!(f.c.held_reads, 200_001, "the frozen read too");
        // A second pause while held changes nothing; step 3 holds three presents later.
        assert_eq!(f.cmd(PAUSE, 0), 11);
        assert_eq!(f.cmd(STEP, 3), 14);
        assert!(!f.must_hold());
        f.hold_end();
        assert_eq!(f.c.leaked, 0);
        assert!((f.c.held_real - 10_000 * 10_000).abs() < 10, "{}", f.c.held_real);
        // The frame after the release starts its real time at the release: no escape.
        assert_eq!(f.now(), frozen);
        for k in 1..=3 {
            f.advance_real_ms(5);
            f.present();
            assert_eq!(f.now(), frozen + k * 160_000);
            assert_eq!(f.must_hold(), k == 3);
        }
        assert_eq!(f.c.escapes, 0);
        // Steps add up while one is in progress.
        f.hold_begin();
        assert_eq!(f.cmd(STEP, 2), 16);
        f.hold_end();
        f.present();
        assert_eq!(f.cmd(STEP, 5), 21, "a step while stepping extends it");
        // Pause stops a step early: at the end of the frame in flight.
        assert_eq!(f.cmd(PAUSE, 0), 16);
        f.present();
        assert!(f.must_hold());
        f.hold_begin();
        assert_eq!(f.cmd(RESUME, 0), 0);
        assert!(!f.must_hold());
        f.hold_end();
        for _ in 0..5 {
            f.present();
            assert!(!f.must_hold());
        }
        // A step from running pauses: the frame in flight is the first of the K.
        let at = f.c.frames;
        assert_eq!(f.cmd(STEP, 2), at + 2);
        f.present();
        assert!(!f.must_hold());
        f.present();
        assert!(f.must_hold());
        assert_eq!((f.c.pauses, f.c.steps, f.c.holds), (2, 3 + 2 + 5 + 2, 3));
        assert_eq!(f.c.stepped, 1 + 3 + 2 + 2, "presents while a hold was on");
        assert_eq!(f.c.leaked, 0);
    }

    #[test]
    fn a_pause_before_the_first_present_holds_at_frame_one() {
        let mut f = Fake::new(62.5, 4096, 1000);
        assert_eq!(f.cmd(PAUSE, 0), 1);
        f.present(); // engagement doesn't count as a frame
        assert!(!f.must_hold());
        f.present();
        assert!(f.must_hold() && f.c.frames == 1);
    }

    #[test]
    fn the_hold_times_out_only_when_asked_to() {
        let mut f = Fake::new(62.5, 4096, 1000);
        f.present();
        f.cmd(PAUSE, 0);
        f.present();
        f.hold_begin();
        f.advance_real_ms(3_600_000);
        assert_eq!(unsafe { vc_hold_expired(&mut *f.c) }, 0, "a deliberate pause never times out by default");
        unsafe { vc_hold_cmd(&mut *f.c, STEP, 1, 0) };
        f.hold_end();
        f.present();
        assert!(f.must_hold());
        unsafe { vc_hold_cmd(&mut *f.c, PAUSE, 0, 2000) };
        f.hold_begin();
        f.advance_real_ms(1999);
        assert_eq!(unsafe { vc_hold_expired(&mut *f.c) }, 0);
        f.advance_real_ms(2);
        assert_eq!(unsafe { vc_hold_expired(&mut *f.c) }, 1);
        assert!(!f.must_hold());
        f.hold_end();
        assert_eq!(f.c.auto_resumes, 1);
    }

    /// One frame of a stand-in game: reads at the start of the frame, sometimes a busy-wait for
    /// two frames (which can only end by creeping), a few reads by a background thread, real
    /// time passing by an amount that depends on `i` (a busy machine).
    fn game_frame(f: &mut Fake, bg: &mut VcThread, i: u64, trace: &mut Vec<i64>) {
        let t0 = f.now();
        trace.push(t0);
        trace.push(unsafe { vc_ticks_ms(&*f.c, t0) } as i64);
        if i % 37 == 5 {
            let mut n = 0;
            while f.now() - t0 < 2 * f.c.ticks_per_frame {
                n += 1;
                assert!(n < 100_000);
            }
            trace.push(f.now());
        }
        for _ in 0..(i % 5) {
            f.now_on(bg);
        }
        f.advance_real_ms(1 + (i * 7919 % 23) as i64);
    }

    #[test]
    fn stepping_gives_the_same_virtual_time_as_running_free() {
        const FRAMES: u64 = 600;
        let free = {
            let mut f = Fake::new(62.5, 4096, 1000);
            let mut bg = VcThread::default();
            let mut trace = Vec::new();
            f.present();
            for i in 0..FRAMES {
                game_frame(&mut f, &mut bg, i, &mut trace);
                f.present();
            }
            (trace, f.c.creep_frames, f.c.overruns, f.c.escapes, f.c.frames)
        };
        assert!(free.1 > 0, "the busy-waits crept");
        // The same game, paused after frame 3, stepped in uneven amounts with long real-time
        // holds in between (longer than the escape) and other threads reading the clock
        // while held, then resumed for the rest.
        let mut f = Fake::new(62.5, 4096, 1000);
        let mut bg = VcThread::default();
        let mut poller = VcThread::default();
        let mut trace = Vec::new();
        let script = [1u64, 12, 1, 7, 64, 3, 100, 1, 1, 2, 250];
        let mut next = 0usize;
        let mut released_at = Vec::new();
        f.present();
        for i in 0..FRAMES {
            if i == 3 {
                f.cmd(PAUSE, 0);
            }
            game_frame(&mut f, &mut bg, i, &mut trace);
            present_gated(&mut f, &mut |f: &mut Fake| {
                let frozen = f.now_on(&mut poller);
                f.advance_real_ms(2500);
                for _ in 0..10_000 {
                    assert_eq!(f.now_on(&mut poller), frozen, "time moved while held");
                }
                released_at.push(f.c.frames);
                if next < script.len() {
                    f.cmd(STEP, script[next]);
                    next += 1;
                } else {
                    f.cmd(RESUME, 0);
                }
            });
        }
        assert_eq!(next, script.len(), "the whole script ran");
        assert_eq!(released_at.len(), script.len() + 1);
        // Held exactly where the steps said: after frame 4, then 4 + 1, + 12, ...
        let mut want = vec![4u64];
        for s in script {
            want.push(want.last().unwrap() + s);
        }
        assert_eq!(released_at, want);
        assert_eq!(trace, free.0, "the virtual-time sequence differs from running free");
        assert_eq!((f.c.creep_frames, f.c.overruns, f.c.escapes, f.c.frames), (free.1, free.2, free.3, free.4));
        assert_eq!(f.c.leaked, 0);
        assert!(f.c.held_reads >= 10_000 * want.len() as u64);
    }

    #[test]
    fn the_control_page_carries_commands_and_state() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join(control::CTL_FILE);
        let page = control::Control::create(&path).unwrap();
        assert!(control::Control::open(t.path()).is_err(), "not claimed by a clock yet");
        let mut f = Fake::new(62.5, 4096, 1000);
        // A command written before the game's process claims the page was for another one.
        page.send(control::Cmd::Pause, 0, 0);
        unsafe { vc_ctl_attach(page.page_ptr(), &mut *f.c, 4242) };
        let ctl = control::Control::open(t.path()).unwrap();
        let (mut cmd, mut arg) = (0u64, 0u64);
        assert_eq!(unsafe { vc_ctl_poll(ctl.page_ptr(), &mut *f.c, &mut cmd, &mut arg) }, 0);
        f.present();
        for _ in 0..20 {
            f.present();
        }
        unsafe { vc_ctl_publish(ctl.page_ptr(), &mut *f.c) };
        let s = ctl.snapshot();
        assert_eq!((s.state.as_str(), s.engaged, s.frames, s.vticks, s.pid), ("running", true, 20, 20 * 160_000, 4242));
        assert_eq!((s.ticks_per_frame, s.qpf, s.rate()), (160_000, 10_000_000, 62.5));
        assert!((s.virtual_s - 0.32).abs() < 1e-12);
        // step 12 from running: acknowledged with its target.
        let seq = ctl.send(control::Cmd::Step, 12, 0);
        assert!(!ctl.acked(seq));
        assert_eq!(unsafe { vc_ctl_poll(ctl.page_ptr(), &mut *f.c, &mut cmd, &mut arg) }, 1);
        assert_eq!((cmd, arg), (2, 12));
        assert!(ctl.acked(seq));
        let s = ctl.snapshot();
        assert_eq!((s.state.as_str(), s.hold_at, s.ack_hold_at, s.ack_frames, s.ack_cmd), ("holding", 32, 32, 20, Some(control::Cmd::Step)));
        // Applied once.
        assert_eq!(unsafe { vc_ctl_poll(ctl.page_ptr(), &mut *f.c, &mut cmd, &mut arg) }, 0);
        for _ in 0..12 {
            f.present();
        }
        f.hold_begin();
        unsafe { vc_ctl_publish(ctl.page_ptr(), &mut *f.c) };
        let s = ctl.snapshot();
        assert!(s.held() && s.frames == 32 && s.vticks == 32 * 160_000 && s.steps == 12 && s.pauses == 1 && s.holds == 1, "{s:?}");
        let seq = ctl.send(control::Cmd::Resume, 0, 0);
        assert_eq!(unsafe { vc_ctl_poll(ctl.page_ptr(), &mut *f.c, &mut cmd, &mut arg) }, 1);
        f.hold_end();
        unsafe { vc_ctl_publish(ctl.page_ptr(), &mut *f.c) };
        let s = ctl.snapshot();
        assert!(ctl.acked(seq) && s.running() && s.hold_at == 0 && s.leaked == 0, "{s:?}");
    }

    struct Shared(*mut VcClock, *mut control::Page);
    unsafe impl Send for Shared {}
    unsafe impl Sync for Shared {}

    /// The hook's loop on real threads: a game thread presenting as fast as it can and holding
    /// the way hook.c does (polling the page), a background thread reading the clock
    /// throughout, and the controller on the test's thread.
    #[test]
    fn pause_and_step_across_threads() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};
        let t = tempfile::tempdir().unwrap();
        let page = control::Control::create(&t.path().join(control::CTL_FILE)).unwrap();
        let mut f = Fake::new(62.5, 4096, 1000);
        unsafe { vc_ctl_attach(page.page_ptr(), &mut *f.c, 1) };
        let ctl = control::Control::open(t.path()).unwrap();
        let sh = Arc::new(Shared(&mut *f.c, page.page_ptr()));
        let stop = Arc::new(AtomicBool::new(false));
        let real = &*f.real as *const AtomicI64 as usize;
        let game = {
            let (sh, stop) = (sh.clone(), stop.clone());
            std::thread::spawn(move || {
                let (c, k) = (sh.0, sh.1);
                let mut me = VcThread::default();
                let mut seen = Vec::new();
                unsafe { vc_present(c) };
                let real = unsafe { &*(real as *const AtomicI64) };
                while !stop.load(Ordering::SeqCst) {
                    seen.push(unsafe { vc_now(c, &mut me) });
                    real.fetch_add(20_000, Ordering::SeqCst); // 2 ms a frame
                    unsafe {
                        vc_ctl_poll(k, c, std::ptr::null_mut(), std::ptr::null_mut());
                        vc_present(c);
                        vc_ctl_publish(k, c);
                        if vc_must_hold(c) != 0 {
                            vc_hold_begin(c);
                            vc_ctl_publish(k, c);
                            while vc_must_hold(c) != 0 && !stop.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_micros(200));
                                vc_ctl_poll(k, c, std::ptr::null_mut(), std::ptr::null_mut());
                            }
                            vc_hold_end(c);
                            vc_ctl_publish(k, c);
                        }
                    }
                }
                seen
            })
        };
        let reader = {
            let (sh, stop) = (sh.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut me = VcThread::default();
                while !stop.load(Ordering::SeqCst) {
                    unsafe { vc_now(sh.0, &mut me) };
                    std::thread::sleep(Duration::from_micros(20));
                }
            })
        };
        let wait = |what: &dyn Fn(&control::Snapshot) -> bool| {
            let end = Instant::now() + Duration::from_secs(20);
            loop {
                let s = ctl.snapshot();
                if what(&s) {
                    return s;
                }
                assert!(Instant::now() < end, "timed out at {s:?}");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        std::thread::sleep(Duration::from_millis(20));
        let seq = ctl.send(control::Cmd::Pause, 0, 0);
        let s = wait(&|s| s.ack_seq >= seq && s.held());
        // Held: frozen across real time while the reader keeps reading.
        f.real.fetch_add(50_000_000, Ordering::SeqCst); // 5 s
        std::thread::sleep(Duration::from_millis(100));
        let s2 = ctl.snapshot();
        assert_eq!((s2.frames, s2.vticks), (s.frames, s.vticks));
        assert_eq!(s.vticks, s.frames as i64 * 160_000);
        let mut at = s.frames;
        for k in [1u64, 12, 1, 30] {
            let seq = ctl.send(control::Cmd::Step, k, 0);
            let s = wait(&|s| s.ack_seq >= seq && s.held() && s.frames >= s.ack_hold_at && s.frames > at);
            assert_eq!(s.frames, at + k, "step {k}");
            assert_eq!(s.vticks, s.frames as i64 * 160_000, "exactly 1/62.5 s per frame");
            at = s.frames;
        }
        let seq = ctl.send(control::Cmd::Resume, 0, 0);
        wait(&|s| s.ack_seq >= seq && s.running() && s.frames > at + 100);
        stop.store(true, Ordering::SeqCst);
        let seen = game.join().unwrap();
        reader.join().unwrap();
        let s = ctl.snapshot();
        assert_eq!((s.leaked, s.escapes), (0, 0), "{s:?}");
        assert!(s.held_reads > 0, "the reader read while held");
        // The game thread saw exactly one frame per step, throughout.
        for w in seen.windows(2) {
            assert_eq!(w[1] - w[0], 160_000);
        }
    }

    #[test]
    fn reports_carry_pause_and_step() {
        let r = Report::parse("rate=62.5\nengaged=1\nproxy=version.dll\nticks_per_frame=160000\nqpf=10000000\nframes=40\ncontrol=1\nhold_state=held\nhold_at=40\npauses=1\nsteps=13\nstepped=14\nholds=3\nheld_s=12.5\nheld_reads=900\nleaked_ticks=0\nauto_resumes=0\n");
        assert!(r.control && r.hold_state == "held" && r.steps == 13);
        let l = r.hold_line().unwrap();
        assert!(l.contains("HELD at frame 40") && l.contains("13 step frame(s)") && l.contains("leaked 0 tick(s)"), "{l}");
        assert!(r.verdict().is_empty());
        let leaky = Report::parse("rate=62.5\nengaged=1\nqpf=10000000\nholds=1\nleaked_ticks=5000\n");
        assert!(leaky.verdict().iter().any(|v| v.contains("moved 5000 tick(s)")));
        assert!(Report::parse("rate=62.5\nengaged=1\n").hold_line().is_none());
    }

    // The vtable hook registry (vc_vt_plan, vc_vt_orig in win/vclock.c), on fake vtables whose
    // "functions" are ids: D the translation layer's (DXVK's), H ours, P and Q other modules'
    // hooks (Pharos's d3d11.dll proxy), each calling what it saved.
    const VT_NEW: i32 = 0;
    const VT_OURS: i32 = 1;
    const VT_WRAPPED: i32 = 2;
    const VT_RESTORED: i32 = 3;
    const VT_FULL: i32 = 4;
    const D: usize = 0x1000;
    const H: usize = 0x2000;
    const P: usize = 0x3000;
    const Q: usize = 0x4000;
    const SLOT: usize = 10; // IDXGIFactory::CreateSwapChain

    fn registry() -> Box<VcVtreg> {
        // SAFETY: all-zero is the C side's empty registry (pointers null, n 0).
        Box::new(unsafe { std::mem::zeroed() })
    }

    fn vtable(f: usize) -> Box<[*mut c_void; 16]> {
        let mut vt = Box::new([std::ptr::null_mut(); 16]);
        vt[SLOT] = f as *mut c_void;
        vt
    }

    /// What vt_hook does: ask the registry, write our hook when told to.
    fn hook(r: &mut VcVtreg, vt: &mut [*mut c_void; 16], h: usize) -> i32 {
        // SAFETY: the registry and the vtable are live; nothing is dereferenced through the ids.
        let plan = unsafe { vc_vt_plan(r, vt.as_mut_ptr(), SLOT as u32, vt[SLOT], h as *mut c_void, std::ptr::null_mut()) };
        if plan == VT_NEW || plan == VT_RESTORED {
            vt[SLOT] = h as *mut c_void;
        }
        plan
    }

    /// Another module hooking the slot the usual way: save what's there, write its own.
    fn foreign(saved: &mut std::collections::HashMap<usize, usize>, vt: &mut [*mut c_void; 16], f: usize) {
        saved.insert(f, vt[SLOT] as usize);
        vt[SLOT] = f as *mut c_void;
    }

    /// A call through the slot: who ran, in order. Panics on a loop or a dead end.
    fn call(r: &VcVtreg, vt: &mut [*mut c_void; 16], saved: &std::collections::HashMap<usize, usize>) -> String {
        let mut f = vt[SLOT] as usize;
        let mut trace = Vec::new();
        loop {
            assert!(trace.len() < 12, "the chain loops: {trace:?}");
            match f {
                D => {
                    trace.push("D");
                    return trace.join(">");
                }
                H => {
                    trace.push("H");
                    // SAFETY: as in hook().
                    f = unsafe { vc_vt_orig(r, vt.as_mut_ptr(), SLOT as u32, H as *mut c_void) } as usize;
                    assert_ne!(f, 0, "our hook has no original: {trace:?}");
                }
                x => {
                    trace.push(if x == P { "P" } else { "Q" });
                    f = saved[&x];
                }
            }
        }
    }

    #[test]
    fn the_registry_mirror_matches() {
        // SAFETY: a plain function.
        assert_eq!(unsafe { vc_vtreg_size() } as usize, std::mem::size_of::<VcVtreg>());
    }

    #[test]
    fn another_modules_hook_over_ours_stays_in_the_chain() {
        // Origins with Pharos: the clock hooks the factory's CreateSwapChain (from the game's
        // CreateDXGIFactory import), Pharos's proxy hooks it over ours, then the clock sees the
        // device's factory (same class, same vtable) after D3D11CreateDevice and is asked again.
        let (mut r, mut vt, mut saved) = (registry(), vtable(D), std::collections::HashMap::new());
        assert_eq!(hook(&mut r, &mut vt, H), VT_NEW);
        assert_eq!(call(&r, &mut vt, &saved), "H>D");
        foreign(&mut saved, &mut vt, P);
        assert_eq!(call(&r, &mut vt, &saved), "P>H>D");
        assert_eq!(hook(&mut r, &mut vt, H), VT_WRAPPED, "left alone");
        assert_eq!(vt[SLOT] as usize, P);
        assert_eq!(call(&r, &mut vt, &saved), "P>H>D", "Pharos sees the call and ours runs once");
        // Every further object of the class asks again: nothing changes, no new entries.
        for _ in 0..50 {
            assert_eq!(hook(&mut r, &mut vt, H), VT_WRAPPED);
        }
        assert_eq!(r.n, 1);
        // A third module over both.
        foreign(&mut saved, &mut vt, Q);
        assert_eq!(hook(&mut r, &mut vt, H), VT_WRAPPED);
        assert_eq!(call(&r, &mut vt, &saved), "Q>P>H>D");
        // They unhook (each restores what it saved): ours is back on top, still ours.
        vt[SLOT] = saved[&Q] as *mut c_void;
        vt[SLOT] = saved[&P] as *mut c_void;
        assert_eq!(hook(&mut r, &mut vt, H), VT_OURS);
        assert_eq!(call(&r, &mut vt, &saved), "H>D");
        // Someone puts the original back under us: hooked again, same entry.
        vt[SLOT] = D as *mut c_void;
        assert_eq!(hook(&mut r, &mut vt, H), VT_RESTORED);
        assert_eq!((call(&r, &mut vt, &saved).as_str(), r.n), ("H>D", 1));
    }

    #[test]
    fn a_hook_already_there_is_chained_onto() {
        // Pharos first, then the clock: ours goes in front and calls Pharos's.
        let (mut r, mut vt, mut saved) = (registry(), vtable(D), std::collections::HashMap::new());
        foreign(&mut saved, &mut vt, P);
        assert_eq!(hook(&mut r, &mut vt, H), VT_NEW);
        assert_eq!(call(&r, &mut vt, &saved), "H>P>D");
        assert_eq!(hook(&mut r, &mut vt, H), VT_OURS, "asked again: nothing to do");
        assert_eq!((call(&r, &mut vt, &saved).as_str(), r.n), ("H>P>D", 1));
    }

    #[test]
    fn vtables_are_told_apart_and_copies_find_their_original() {
        let (mut r, saved) = (registry(), std::collections::HashMap::new());
        let (mut a, mut b) = (vtable(D), vtable(D + 8));
        assert_eq!(hook(&mut r, &mut a, H), VT_NEW);
        // A copy of a hooked vtable (a per-object vtable some hookers make): its slot is ours
        // but it isn't registered. It gets the one original this hook has for the slot.
        let mut copy = Box::new(*a);
        assert_eq!(call(&r, &mut copy, &saved), "H>D");
        // A second class with another original: copies are ambiguous now, so none is guessed.
        assert_eq!(hook(&mut r, &mut b, H), VT_NEW);
        // SAFETY: as in hook().
        let o = unsafe { vc_vt_orig(&*r, copy.as_mut_ptr(), SLOT as u32, H as *mut c_void) };
        assert!(o.is_null());
        // The registered ones still get their own.
        // SAFETY: as in hook().
        let (oa, ob) = unsafe { (vc_vt_orig(&*r, a.as_mut_ptr(), SLOT as u32, H as *mut c_void), vc_vt_orig(&*r, b.as_mut_ptr(), SLOT as u32, H as *mut c_void)) };
        assert_eq!((oa as usize, ob as usize), (D, D + 8));
    }

    #[test]
    fn a_full_registry_leaves_slots_alone() {
        let mut r = registry();
        let mut vts: Vec<Box<[*mut c_void; 16]>> = (0..129).map(|_| vtable(D)).collect();
        for (i, vt) in vts.iter_mut().enumerate() {
            let want = if i < 128 { VT_NEW } else { VT_FULL };
            assert_eq!(hook(&mut r, vt, H), want, "vtable {i}");
        }
        assert_eq!(vts[128][SLOT] as usize, D);
    }
}
