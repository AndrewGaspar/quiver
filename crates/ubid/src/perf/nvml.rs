//! NVIDIA's management library, loaded at run time (`libnvidia-ml.so.1` comes with the
//! driver; quiverd doesn't link it). Only the handful of calls the HUD shows.
//!
//! NVML opens the GPU's device nodes, which wakes a runtime-suspended dGPU and keeps it
//! awake while initialized (SIGHT S9). [`super::system`] decides when that's acceptable.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};

type Ret = c_int;
type Device = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

struct Fns {
    init: unsafe extern "C" fn() -> Ret,
    shutdown: unsafe extern "C" fn() -> Ret,
    count: unsafe extern "C" fn(*mut c_uint) -> Ret,
    handle: unsafe extern "C" fn(c_uint, *mut Device) -> Ret,
    util: unsafe extern "C" fn(Device, *mut Utilization) -> Ret,
    temp: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
    power: unsafe extern "C" fn(Device, *mut c_uint) -> Ret,
    clock: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
    memory: unsafe extern "C" fn(Device, *mut Memory) -> Ret,
    name: unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Ret,
}

/// One device's readings; a field is `None` when NVML doesn't support it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NvSample {
    pub name: Option<String>,
    pub load: Option<f64>,
    pub temp: Option<f64>,
    pub power_w: Option<f64>,
    pub clock_mhz: Option<f64>,
    pub vram_used: Option<f64>,
    pub vram_total: Option<f64>,
}

pub struct Nvml {
    lib: *mut c_void,
    f: Fns,
    initialized: bool,
}

// SAFETY: NVML is thread-safe; the handle is only used by the sensors thread anyway.
unsafe impl Send for Nvml {}

impl Nvml {
    /// Loads the library (no device access yet). `None` without an NVIDIA driver.
    pub fn load() -> Option<Nvml> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; symbols are cast to their
        // documented NVML signatures.
        unsafe {
            let lib = libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return None;
            }
            macro_rules! sym {
                ($name:literal, $t:ty) => {{
                    let p = libc::dlsym(lib, $name.as_ptr());
                    if p.is_null() {
                        libc::dlclose(lib);
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, $t>(p)
                }};
            }
            let f = Fns {
                init: sym!(c"nvmlInit_v2", unsafe extern "C" fn() -> Ret),
                shutdown: sym!(c"nvmlShutdown", unsafe extern "C" fn() -> Ret),
                count: sym!(c"nvmlDeviceGetCount_v2", unsafe extern "C" fn(*mut c_uint) -> Ret),
                handle: sym!(c"nvmlDeviceGetHandleByIndex_v2", unsafe extern "C" fn(c_uint, *mut Device) -> Ret),
                util: sym!(c"nvmlDeviceGetUtilizationRates", unsafe extern "C" fn(Device, *mut Utilization) -> Ret),
                temp: sym!(c"nvmlDeviceGetTemperature", unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret),
                power: sym!(c"nvmlDeviceGetPowerUsage", unsafe extern "C" fn(Device, *mut c_uint) -> Ret),
                clock: sym!(c"nvmlDeviceGetClockInfo", unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret),
                memory: sym!(c"nvmlDeviceGetMemoryInfo", unsafe extern "C" fn(Device, *mut Memory) -> Ret),
                name: sym!(c"nvmlDeviceGetName", unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Ret),
            };
            Some(Nvml { lib, f, initialized: false })
        }
    }

    /// Opens the devices (wakes the GPU).
    pub fn init(&mut self) -> bool {
        if !self.initialized {
            // SAFETY: documented NVML call.
            self.initialized = unsafe { (self.f.init)() } == 0;
        }
        self.initialized
    }

    /// Closes the devices so the GPU can suspend again.
    pub fn shutdown(&mut self) {
        if self.initialized {
            // SAFETY: documented NVML call, paired with init.
            unsafe { (self.f.shutdown)() };
            self.initialized = false;
        }
    }

    /// Readings from device `index` (initialize first).
    pub fn sample(&self, index: u32) -> Option<NvSample> {
        if !self.initialized {
            return None;
        }
        // SAFETY: NVML calls with out-pointers to locals; the device handle comes from
        // NVML itself and is valid while initialized.
        unsafe {
            let mut n: c_uint = 0;
            if (self.f.count)(&mut n) != 0 || index >= n {
                return None;
            }
            let mut dev: Device = std::ptr::null_mut();
            if (self.f.handle)(index, &mut dev) != 0 {
                return None;
            }
            let mut s = NvSample::default();
            let mut u = Utilization::default();
            if (self.f.util)(dev, &mut u) == 0 {
                s.load = Some(u.gpu as f64);
            }
            let mut v: c_uint = 0;
            if (self.f.temp)(dev, 0, &mut v) == 0 {
                s.temp = Some(v as f64);
            }
            if (self.f.power)(dev, &mut v) == 0 {
                s.power_w = Some(v as f64 / 1000.0);
            }
            if (self.f.clock)(dev, 0, &mut v) == 0 {
                s.clock_mhz = Some(v as f64);
            }
            let mut m = Memory::default();
            if (self.f.memory)(dev, &mut m) == 0 {
                s.vram_used = Some(m.used as f64);
                s.vram_total = Some(m.total as f64);
            }
            let mut buf = [0 as c_char; 96];
            if (self.f.name)(dev, buf.as_mut_ptr(), buf.len() as c_uint) == 0 {
                s.name = Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned());
            }
            Some(s)
        }
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        self.shutdown();
        // SAFETY: our handle from dlopen.
        unsafe { libc::dlclose(self.lib) };
    }
}
