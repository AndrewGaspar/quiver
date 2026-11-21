//! The few CUDA driver calls the encoder needs, from `libcuda.so.1` opened at run time.
//!
//! NVENC takes its input from CUDA memory: pitched device buffers the encoder registers once.
//! Frames reach them by a copy from host memory (the test pattern, a shared-memory capture)
//! or from other device memory (a dma-buf capture imported into CUDA). Every call runs with
//! the device's primary context pushed on the calling thread ([`Cuda::push`]).

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use libloading::Library;

pub type CUresult = c_int;
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void;
pub type CUdeviceptr = u64;
pub type CUstream = *mut c_void;

const CU_MEMORYTYPE_HOST: c_uint = 1;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;
const CU_MEMHOSTREGISTER_PORTABLE: c_uint = 1;

#[repr(C)]
struct Memcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

struct Fns {
    init: unsafe extern "C" fn(c_uint) -> CUresult,
    device_get: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
    device_get_name: unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult,
    primary_ctx_retain: unsafe extern "C" fn(*mut CUcontext, CUdevice) -> CUresult,
    primary_ctx_release: unsafe extern "C" fn(CUdevice) -> CUresult,
    ctx_push: unsafe extern "C" fn(CUcontext) -> CUresult,
    ctx_pop: unsafe extern "C" fn(*mut CUcontext) -> CUresult,
    mem_alloc_pitch: unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult,
    mem_free: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
    memcpy_2d: unsafe extern "C" fn(*const Memcpy2D) -> CUresult,
    mem_host_register: unsafe extern "C" fn(*mut c_void, usize, c_uint) -> CUresult,
    mem_host_unregister: unsafe extern "C" fn(*mut c_void) -> CUresult,
    get_error_string: unsafe extern "C" fn(CUresult, *mut *const c_char) -> CUresult,
}

/// The CUDA driver, device 0 and its primary context.
pub struct Cuda {
    fns: Fns,
    device: CUdevice,
    ctx: CUcontext,
    pub name: String,
    _lib: Library,
}

// SAFETY: the context is a driver handle usable from any thread once pushed there; Cuda only
// hands it out through `push`.
unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

impl Cuda {
    /// Open the driver and retain device 0's primary context.
    pub fn open() -> Result<Arc<Cuda>> {
        // SAFETY: loading the system's CUDA driver library, which has no constructors that
        // matter here.
        let lib = unsafe { Library::new("libcuda.so.1") }.context("open libcuda.so.1 (the NVIDIA driver's CUDA library)")?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol's type is the driver API's, as declared in cuda.h.
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.with_context(|| format!("libcuda has no {}", $name))?
            };
        }
        let fns = Fns {
            init: sym!("cuInit"),
            device_get: sym!("cuDeviceGet"),
            device_get_name: sym!("cuDeviceGetName"),
            primary_ctx_retain: sym!("cuDevicePrimaryCtxRetain"),
            primary_ctx_release: sym!("cuDevicePrimaryCtxRelease_v2"),
            ctx_push: sym!("cuCtxPushCurrent_v2"),
            ctx_pop: sym!("cuCtxPopCurrent_v2"),
            mem_alloc_pitch: sym!("cuMemAllocPitch_v2"),
            mem_free: sym!("cuMemFree_v2"),
            memcpy_2d: sym!("cuMemcpy2D_v2"),
            mem_host_register: sym!("cuMemHostRegister_v2"),
            mem_host_unregister: sym!("cuMemHostUnregister"),
            get_error_string: sym!("cuGetErrorString"),
        };
        let mut cuda = Cuda { fns, device: 0, ctx: std::ptr::null_mut(), name: String::new(), _lib: lib };
        cuda.check(unsafe { (cuda.fns.init)(0) }, "cuInit")?;
        let mut dev = 0;
        cuda.check(unsafe { (cuda.fns.device_get)(&mut dev, 0) }, "cuDeviceGet")?;
        let mut name = [0 as c_char; 128];
        cuda.check(unsafe { (cuda.fns.device_get_name)(name.as_mut_ptr(), name.len() as c_int, dev) }, "cuDeviceGetName")?;
        cuda.name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
        let mut ctx = std::ptr::null_mut();
        cuda.check(unsafe { (cuda.fns.primary_ctx_retain)(&mut ctx, dev) }, "cuDevicePrimaryCtxRetain")?;
        cuda.device = dev;
        cuda.ctx = ctx;
        Ok(Arc::new(cuda))
    }

    /// The context, for NVENC's session.
    pub fn context(&self) -> CUcontext {
        self.ctx
    }

    fn check(&self, r: CUresult, what: &str) -> Result<()> {
        if r == 0 {
            return Ok(());
        }
        let mut s: *const c_char = std::ptr::null();
        unsafe { (self.fns.get_error_string)(r, &mut s) };
        let msg = if s.is_null() { "unknown".to_owned() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() };
        bail!("{what}: CUDA error {r} ({msg})")
    }

    /// Make the context current on this thread until the guard drops.
    pub fn push(&self) -> Result<CtxGuard<'_>> {
        self.check(unsafe { (self.fns.ctx_push)(self.ctx) }, "cuCtxPushCurrent")?;
        Ok(CtxGuard { cuda: self })
    }

    /// A pitched device buffer, `width_bytes` × `rows`. The context must be current.
    pub fn alloc_pitch(self: &Arc<Self>, width_bytes: usize, rows: usize) -> Result<DeviceBuf> {
        let mut ptr = 0;
        let mut pitch = 0;
        self.check(unsafe { (self.fns.mem_alloc_pitch)(&mut ptr, &mut pitch, width_bytes, rows, 16) }, "cuMemAllocPitch")?;
        Ok(DeviceBuf { cuda: self.clone(), ptr, pitch, width_bytes, rows })
    }

    /// Copy `rows` rows of `width_bytes` from host memory into device memory. The context
    /// must be current.
    ///
    /// # Safety
    /// `src` must be readable for `src_pitch * (rows - 1) + width_bytes` bytes, and the
    /// destination must hold the rectangle.
    pub unsafe fn copy_htod(&self, src: *const u8, src_pitch: usize, dst: CUdeviceptr, dst_pitch: usize, width_bytes: usize, rows: usize) -> Result<()> {
        let p = Memcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: src as *const c_void,
            src_device: 0,
            src_array: std::ptr::null_mut(),
            src_pitch,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device: dst,
            dst_array: std::ptr::null_mut(),
            dst_pitch,
            width_in_bytes: width_bytes,
            height: rows,
        };
        self.check(unsafe { (self.fns.memcpy_2d)(&p) }, "cuMemcpy2D (host to device)")
    }

    /// Copy a rectangle between device buffers. The context must be current.
    ///
    /// # Safety
    /// Both rectangles must lie inside their allocations.
    pub unsafe fn copy_dtod(&self, src: CUdeviceptr, src_pitch: usize, dst: CUdeviceptr, dst_pitch: usize, width_bytes: usize, rows: usize) -> Result<()> {
        let p = Memcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_host: std::ptr::null(),
            src_device: src,
            src_array: std::ptr::null_mut(),
            src_pitch,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device: dst,
            dst_array: std::ptr::null_mut(),
            dst_pitch,
            width_in_bytes: width_bytes,
            height: rows,
        };
        self.check(unsafe { (self.fns.memcpy_2d)(&p) }, "cuMemcpy2D (device to device)")
    }

    /// Page-lock host memory the process already has mapped (a PipeWire buffer), so copies
    /// from it go at DMA speed. The context must be current.
    ///
    /// # Safety
    /// `ptr..ptr+len` must stay mapped until [`Cuda::host_unregister`].
    pub unsafe fn host_register(&self, ptr: *mut u8, len: usize) -> Result<()> {
        self.check(unsafe { (self.fns.mem_host_register)(ptr as *mut c_void, len, CU_MEMHOSTREGISTER_PORTABLE) }, "cuMemHostRegister")
    }

    /// # Safety
    /// `ptr` must have been registered with [`Cuda::host_register`].
    pub unsafe fn host_unregister(&self, ptr: *mut u8) {
        unsafe { (self.fns.mem_host_unregister)(ptr as *mut c_void) };
    }
}

impl Drop for Cuda {
    fn drop(&mut self) {
        unsafe { (self.fns.primary_ctx_release)(self.device) };
    }
}

/// The context is current on this thread while this lives.
pub struct CtxGuard<'a> {
    cuda: &'a Cuda,
}

impl Drop for CtxGuard<'_> {
    fn drop(&mut self) {
        let mut old = std::ptr::null_mut();
        unsafe { (self.cuda.fns.ctx_pop)(&mut old) };
    }
}

/// Pitched device memory, freed on drop (with the context pushed).
pub struct DeviceBuf {
    cuda: Arc<Cuda>,
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    pub width_bytes: usize,
    pub rows: usize,
}

impl Drop for DeviceBuf {
    fn drop(&mut self) {
        if let Ok(_g) = self.cuda.push() {
            unsafe { (self.cuda.fns.mem_free)(self.ptr) };
        }
    }
}
