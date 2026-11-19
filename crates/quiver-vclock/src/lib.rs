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
    }

    /// `vc_thread`.
    #[repr(C)]
    #[derive(Default)]
    pub struct VcThread {
        pub frame: u64,
        pub reads: u64,
    }

    unsafe extern "C" {
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
}
