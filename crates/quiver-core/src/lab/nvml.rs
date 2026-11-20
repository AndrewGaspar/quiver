//! NVIDIA's management library, loaded at run time (`libnvidia-ml.so.1` comes with the
//! driver): per-process GPU utilization, which the lab's GPU governor holds an instance's
//! share to (design/LAB.md §15, [`super::limits::Governor`]).
//!
//! `nvmlDeviceGetProcessUtilization` returns, per process that used the GPU since a timestamp,
//! the share of time the SMs (3D and compute) were busy with its work. Processes are host pids,
//! containers included. NVML opens the GPU's device nodes, which keeps a dGPU awake while
//! initialized, so the supervisor initializes it only while a run is up under a GPU share.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_uint, c_void};

type Ret = c_int;
type Device = *mut c_void;

const NVML_SUCCESS: Ret = 0;
const NVML_ERROR_INSUFFICIENT_SIZE: Ret = 7;
const NVML_ERROR_NOT_FOUND: Ret = 6;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcessSample {
    pid: c_uint,
    time_stamp: u64,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
}

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

struct Fns {
    init: unsafe extern "C" fn() -> Ret,
    shutdown: unsafe extern "C" fn() -> Ret,
    handle: unsafe extern "C" fn(c_uint, *mut Device) -> Ret,
    util: unsafe extern "C" fn(Device, *mut Utilization) -> Ret,
    process_util: unsafe extern "C" fn(Device, *mut ProcessSample, *mut c_uint, u64) -> Ret,
    clock: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
    max_clock: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
}

/// `NVML_CLOCK_SM`.
const CLOCK_SM: c_int = 1;

/// One reading: per-process SM busy percent since the last one, and the whole GPU's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    pub by_pid: BTreeMap<u32, f64>,
    pub device: Option<f64>,
    /// The SM clock now over its maximum (0–1). Utilization is time busy at the current clock:
    /// a GPU that clocked down for a light load shows a frame as more "percent" than at full
    /// clock, so shares are scaled by this to be shares of the GPU's full speed.
    pub clock_ratio: Option<f64>,
}

impl Reading {
    /// The share of `pids` (summed).
    pub fn share_of(&self, pids: &[u32]) -> f64 {
        pids.iter().filter_map(|p| self.by_pid.get(p)).sum()
    }

    /// A busy percentage as a share of the GPU at full clock.
    pub fn at_full_clock(&self, busy: f64) -> f64 {
        busy * self.clock_ratio.unwrap_or(1.0)
    }
}

pub struct Nvml {
    lib: *mut c_void,
    f: Fns,
    dev: Device,
    last_ts: u64,
}

// SAFETY: NVML is thread-safe; the supervisor uses it from one thread.
unsafe impl Send for Nvml {}

impl Nvml {
    /// Loads the library and opens GPU 0 (`Err`: why not).
    pub fn open() -> Result<Nvml, String> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; symbols are cast to their documented
        // NVML signatures.
        unsafe {
            let lib = libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return Err("no libnvidia-ml.so.1 (no NVIDIA driver)".into());
            }
            macro_rules! sym {
                ($name:literal, $t:ty) => {{
                    let p = libc::dlsym(lib, $name.as_ptr());
                    if p.is_null() {
                        libc::dlclose(lib);
                        return Err(format!("libnvidia-ml.so.1 has no {}", $name.to_string_lossy()));
                    }
                    std::mem::transmute::<*mut c_void, $t>(p)
                }};
            }
            let f = Fns {
                init: sym!(c"nvmlInit_v2", unsafe extern "C" fn() -> Ret),
                shutdown: sym!(c"nvmlShutdown", unsafe extern "C" fn() -> Ret),
                handle: sym!(c"nvmlDeviceGetHandleByIndex_v2", unsafe extern "C" fn(c_uint, *mut Device) -> Ret),
                util: sym!(c"nvmlDeviceGetUtilizationRates", unsafe extern "C" fn(Device, *mut Utilization) -> Ret),
                process_util: sym!(c"nvmlDeviceGetProcessUtilization", unsafe extern "C" fn(Device, *mut ProcessSample, *mut c_uint, u64) -> Ret),
                clock: sym!(c"nvmlDeviceGetClockInfo", unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret),
                max_clock: sym!(c"nvmlDeviceGetMaxClockInfo", unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret),
            };
            let r = (f.init)();
            if r != NVML_SUCCESS {
                libc::dlclose(lib);
                return Err(format!("nvmlInit failed ({r})"));
            }
            let mut dev: Device = std::ptr::null_mut();
            let r = (f.handle)(0, &mut dev);
            if r != NVML_SUCCESS {
                (f.shutdown)();
                libc::dlclose(lib);
                return Err(format!("no NVIDIA GPU 0 ({r})"));
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
            Ok(Nvml { lib, f, dev, last_ts: now })
        }
    }

    /// Every sample the driver holds newer than `since_us` (pid, timestamp µs, SM %), for
    /// checking what NVML reports.
    pub fn raw(&self, since_us: u64) -> Vec<(u32, u64, u32)> {
        // SAFETY: as in read.
        unsafe {
            let mut n: c_uint = 0;
            let r = (self.f.process_util)(self.dev, std::ptr::null_mut(), &mut n, since_us);
            if r == NVML_ERROR_NOT_FOUND || (r != NVML_ERROR_INSUFFICIENT_SIZE && r != NVML_SUCCESS) || n == 0 {
                return Vec::new();
            }
            let mut buf = vec![ProcessSample::default(); n as usize + 8];
            let mut n = buf.len() as c_uint;
            if (self.f.process_util)(self.dev, buf.as_mut_ptr(), &mut n, since_us) != NVML_SUCCESS {
                return Vec::new();
            }
            buf[..(n as usize).min(buf.len())].iter().map(|s| (s.pid, s.time_stamp, s.sm_util)).collect()
        }
    }

    /// Each process's current use: its latest sample, if it's under [`FRESH_US`] old.
    ///
    /// The driver keeps **one sample per process**, refreshed irregularly (every 0.2–0.8 s
    /// under contention, measured on driver 620), each covering its own period. Asking only for
    /// samples newer than the last read (`lastSeenTimeStamp`) leaves a process out of every
    /// read where its sample didn't change, so a reader that counted those as 0 saw medians of
    /// 0% for a game using 20%. So every read asks for the last [`FRESH_US`] and takes each
    /// process's latest sample as its use now.
    pub fn read(&mut self) -> Reading {
        let mut out = Reading::default();
        // SAFETY: NVML calls with out-pointers to locals and a buffer of the size it asked for;
        // the device handle is NVML's, valid while initialized.
        unsafe {
            let mut u = Utilization::default();
            if (self.f.util)(self.dev, &mut u) == NVML_SUCCESS {
                out.device = Some(u.gpu as f64);
            }
            let (mut now, mut max): (c_uint, c_uint) = (0, 0);
            if (self.f.clock)(self.dev, CLOCK_SM, &mut now) == NVML_SUCCESS && (self.f.max_clock)(self.dev, CLOCK_SM, &mut max) == NVML_SUCCESS && max > 0 {
                out.clock_ratio = Some((now as f64 / max as f64).clamp(0.0, 1.0));
            }
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
        let mut latest: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
        for (pid, ts, sm) in self.raw(now.saturating_sub(FRESH_US)) {
            let e = latest.entry(pid).or_insert((ts, sm));
            if ts >= e.0 {
                *e = (ts, sm);
            }
        }
        self.last_ts = now;
        out.by_pid = latest.into_iter().map(|(p, (_, sm))| (p, sm as f64)).collect();
        out
    }
}

/// How old a process's sample may be and still count as its use now.
pub const FRESH_US: u64 = 2_000_000;

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: paired with the init in open.
        unsafe {
            (self.f.shutdown)();
            libc::dlclose(self.lib);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shares_sum_over_pids() {
        let r = super::Reading { by_pid: [(10, 12.0), (11, 3.5), (99, 80.0)].into_iter().collect(), device: Some(95.0), clock_ratio: Some(0.5) };
        assert_eq!(r.share_of(&[10, 11, 12]), 15.5);
        assert_eq!(r.at_full_clock(40.0), 20.0, "40% busy at half clock is a fifth of the GPU");
    }

    /// `cargo test -p quiver-core --lib nvml -- --ignored --nocapture`: prints per-process use
    /// over three one-second windows (compare `nvidia-smi pmon -s u`).
    #[test]
    #[ignore = "needs an NVIDIA GPU"]
    fn live_reading() {
        let mut n = super::Nvml::open().unwrap();
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let r = n.read();
            eprintln!("device {:?}%, by pid {:?}", r.device, r.by_pid);
        }
    }

    #[test]
    #[ignore = "needs an NVIDIA GPU"]
    fn nvml_raw() {
        let n = super::Nvml::open().unwrap();
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
            let r = n.raw(now - 2_000_000);
            eprintln!("{} samples in the last 2 s: {:?}", r.len(), r.iter().map(|(p, t, u)| (p, (now - t) / 1000, u)).collect::<Vec<_>>());
        }
    }

    /// `QUIVER_TEST_NVML_SECS=60 cargo test -p quiver-core --lib nvml_watch -- --ignored
    /// --nocapture`: one CSV line a second, the share per container (libpod id prefix, or
    /// `host`), for measuring the lab's GPU limit.
    #[test]
    #[ignore = "needs an NVIDIA GPU"]
    fn nvml_watch() {
        let secs: u64 = std::env::var("QUIVER_TEST_NVML_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
        let mut n = super::Nvml::open().unwrap();
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_secs() < secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let r = n.read();
            let mut by: std::collections::BTreeMap<String, f64> = Default::default();
            for (pid, u) in &r.by_pid {
                let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
                let who = crate::lab::procs::libpod_id(&cg).map(|id| id[..12.min(id.len())].to_owned()).unwrap_or_else(|| "host".into());
                *by.entry(who).or_default() += u;
            }
            let parts: Vec<String> = by.iter().map(|(k, v)| format!("{k}={v:.0}")).collect();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
            println!("nvml,{now:.1},{:?},{},clock={:.2}", r.device, parts.join(","), r.clock_ratio.unwrap_or(1.0));
        }
    }
}
