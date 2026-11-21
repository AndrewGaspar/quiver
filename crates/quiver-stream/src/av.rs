//! libav through the C shim (csrc/qs_av.c): hardware devices, decoders, decoded pictures and
//! a file demuxer. The structs mirror csrc/qs_av.h field for field.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;

use anyhow::{Result, anyhow, bail};
use quiver_stream_proto::VideoCodec;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct QsVkQueueFamily {
    pub idx: c_int,
    pub num: c_int,
    pub flags: u32,
    pub video_caps: u32,
}

#[repr(C)]
pub struct QsVkDevice {
    pub get_instance_proc_addr: *const c_void,
    pub instance: *const c_void,
    pub physical_device: *const c_void,
    pub device: *const c_void,
    pub instance_extensions: *const *const c_char,
    pub nb_instance_extensions: c_int,
    pub device_extensions: *const *const c_char,
    pub nb_device_extensions: c_int,
    pub features: *const c_void,
    pub qf: [QsVkQueueFamily; 16],
    pub nb_qf: c_int,
}

#[repr(C)]
struct QsPicture {
    frame: *mut c_void,
    kind: c_int,
    width: c_int,
    height: c_int,
    pts: i64,
    nb_planes: c_int,
    log2_chroma_w: c_int,
    log2_chroma_h: c_int,
    depth: c_int,
    bytes: c_int,
    shift: c_int,
    data: [*mut u8; 3],
    linesize: [c_int; 3],
    color_range: c_int,
    colorspace: c_int,
    color_trc: c_int,
    color_primaries: c_int,
    transfer_us: i64,
}

/// One locked Vulkan image of a decoded frame ([`Picture::vk_lock`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VkImageInfo {
    pub image: u64,
    pub format: i32,
    pub layout: i32,
    pub access: u64,
    pub semaphore: u64,
    pub sem_value: u64,
    pub queue_family: u32,
    pub img_flags: u32,
    pub usage: u32,
    pub nb_images: c_int,
    pub width: c_int,
    pub height: c_int,
}

#[repr(C)]
#[derive(Default)]
struct QsStreamInfo {
    codec: c_int,
    width: c_int,
    height: c_int,
    fps_num: c_int,
    fps_den: c_int,
}

#[repr(C)]
struct QsPacket {
    data: *const u8,
    size: c_int,
    key: c_int,
}

#[allow(non_camel_case_types)]
enum QsHwDevice {}
#[allow(non_camel_case_types)]
enum QsDec {}
#[allow(non_camel_case_types)]
enum QsDemux {}

unsafe extern "C" {
    fn qs_hw_device_vulkan(vk: *const QsVkDevice, err: *mut c_char, errlen: c_int) -> *mut QsHwDevice;
    fn qs_hw_device_create(hw: c_int, err: *mut c_char, errlen: c_int) -> *mut QsHwDevice;
    fn qs_hw_device_free(dev: *mut QsHwDevice);
    fn qs_dec_open(codec: c_int, dev: *mut QsHwDevice, threads: c_int, err: *mut c_char, errlen: c_int) -> *mut QsDec;
    fn qs_dec_backend(dec: *const QsDec) -> c_int;
    fn qs_dec_name(dec: *const QsDec) -> *const c_char;
    fn qs_dec_send(dec: *mut QsDec, data: *const u8, size: c_int, pts: i64) -> c_int;
    fn qs_dec_receive(dec: *mut QsDec, pic: *mut QsPicture) -> c_int;
    fn qs_dec_flush(dec: *mut QsDec);
    fn qs_dec_close(dec: *mut QsDec);
    fn qs_pic_free(pic: *mut QsPicture);
    fn qs_pic_vk_lock(pic: *mut QsPicture, out: *mut VkImageInfo) -> c_int;
    fn qs_pic_vk_unlock(pic: *mut QsPicture, inp: *const VkImageInfo);
    fn qs_demux_open(path: *const c_char, info: *mut QsStreamInfo, err: *mut c_char, errlen: c_int) -> *mut QsDemux;
    fn qs_demux_read(dmx: *mut QsDemux, pkt: *mut QsPacket) -> c_int;
    fn qs_demux_close(dmx: *mut QsDemux);
    fn qs_av_quiet(quiet: c_int);
    fn qs_av_strerror(err: c_int, buf: *mut c_char, len: c_int);
}

/// Keep libav's log to errors (it prints per-frame chatter otherwise).
pub fn quiet(on: bool) {
    unsafe { qs_av_quiet(on as c_int) }
}

pub fn strerror(code: i32) -> String {
    let mut buf = [0 as c_char; 128];
    unsafe { qs_av_strerror(code, buf.as_mut_ptr(), buf.len() as c_int) };
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn err_string(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn codec_id(c: VideoCodec) -> c_int {
    match c {
        VideoCodec::H264 => 0,
        VideoCodec::Hevc => 1,
        VideoCodec::Av1 => 2,
    }
}

/// Where decoding happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Software,
    /// Vulkan Video: the frames stay on the GPU and are sampled in place.
    Vulkan,
    /// NVDEC: frames are copied to memory, then uploaded.
    Cuda,
    /// VA-API: likewise.
    Vaapi,
}

impl Backend {
    fn from_raw(n: c_int) -> Self {
        match n {
            1 => Self::Vulkan,
            2 => Self::Cuda,
            3 => Self::Vaapi,
            _ => Self::Software,
        }
    }
    fn raw(self) -> c_int {
        match self {
            Self::Software => 0,
            Self::Vulkan => 1,
            Self::Cuda => 2,
            Self::Vaapi => 3,
        }
    }
    pub fn is_hardware(self) -> bool {
        self != Self::Software
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "software" | "sw" => Self::Software,
            "vulkan" => Self::Vulkan,
            "cuda" | "nvdec" => Self::Cuda,
            "vaapi" => Self::Vaapi,
            _ => return None,
        })
    }
}

/// A libav hardware device context.
pub struct HwDevice {
    raw: NonNull<QsHwDevice>,
    pub backend: Backend,
}

// SAFETY: an AVHWDeviceContext is shared between decoders and threads by design.
unsafe impl Send for HwDevice {}
unsafe impl Sync for HwDevice {}

impl HwDevice {
    /// libav on a Vulkan device the caller made. Everything `vk` points at must outlive the
    /// returned device.
    ///
    /// # Safety
    /// The handles must be valid and the extension and feature lists must be what the
    /// instance and device were created with.
    pub unsafe fn vulkan(vk: &QsVkDevice) -> Result<Self> {
        let mut err = [0 as c_char; 256];
        let raw = unsafe { qs_hw_device_vulkan(vk, err.as_mut_ptr(), err.len() as c_int) };
        let raw = NonNull::new(raw).ok_or_else(|| anyhow!("{}", err_string(&err)))?;
        Ok(Self { raw, backend: Backend::Vulkan })
    }

    /// libav's own device of this kind (CUDA or VA-API; Vulkan makes its own device too, but
    /// then the presenter can't share it).
    pub fn create(backend: Backend) -> Result<Self> {
        if backend == Backend::Software {
            bail!("software decoding needs no device");
        }
        let mut err = [0 as c_char; 256];
        let raw = unsafe { qs_hw_device_create(backend.raw(), err.as_mut_ptr(), err.len() as c_int) };
        let raw = NonNull::new(raw).ok_or_else(|| anyhow!("{}", err_string(&err)))?;
        Ok(Self { raw, backend })
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        unsafe { qs_hw_device_free(self.raw.as_ptr()) }
    }
}

pub struct Decoder {
    raw: NonNull<QsDec>,
    pub codec: VideoCodec,
}

unsafe impl Send for Decoder {}

impl Decoder {
    /// A decoder on `device`, or in software with `threads` slice threads (0: one per core).
    pub fn open(codec: VideoCodec, device: Option<&HwDevice>, threads: u32) -> Result<Self> {
        let mut err = [0 as c_char; 256];
        let dev = device.map_or(std::ptr::null_mut(), |d| d.raw.as_ptr());
        let raw = unsafe { qs_dec_open(codec_id(codec), dev, threads as c_int, err.as_mut_ptr(), err.len() as c_int) };
        let raw = NonNull::new(raw).ok_or_else(|| anyhow!("{}", err_string(&err)))?;
        Ok(Self { raw, codec })
    }

    /// What decodes now. A hardware decoder falls back to software (libav's choice) when the
    /// stream's profile isn't supported.
    pub fn backend(&self) -> Backend {
        Backend::from_raw(unsafe { qs_dec_backend(self.raw.as_ptr()) })
    }

    pub fn name(&self) -> String {
        unsafe { CStr::from_ptr(qs_dec_name(self.raw.as_ptr())) }.to_string_lossy().into_owned()
    }

    /// Feed one access unit (copied). `pts` comes back on its picture.
    pub fn send(&mut self, data: &[u8], pts: i64) -> Result<(), i32> {
        let r = unsafe { qs_dec_send(self.raw.as_ptr(), data.as_ptr(), data.len() as c_int, pts) };
        if r < 0 { Err(r) } else { Ok(()) }
    }

    /// The next decoded picture, if there is one.
    pub fn receive(&mut self) -> Result<Option<Picture>, i32> {
        let mut pic = std::mem::MaybeUninit::<QsPicture>::zeroed();
        let r = unsafe { qs_dec_receive(self.raw.as_ptr(), pic.as_mut_ptr()) };
        match r {
            1 => Ok(Some(Picture { raw: unsafe { pic.assume_init() } })),
            0 => Ok(None),
            e => Err(e),
        }
    }

    pub fn flush(&mut self) {
        unsafe { qs_dec_flush(self.raw.as_ptr()) }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { qs_dec_close(self.raw.as_ptr()) }
    }
}

/// How a picture's planes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// 2: Y and interleaved CbCr (NV12, P010); 3: Y, Cb, Cr.
    pub planes: u32,
    pub log2_chroma_w: u32,
    pub log2_chroma_h: u32,
    pub depth: u32,
    /// Bytes per component, 1 or 2.
    pub bytes: u32,
    /// Where the bits sit in a 2-byte component (6 for P010, 0 for yuv420p10).
    pub shift: u32,
}

/// What a picture says about its colours, in libav's enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameColor {
    /// 2 is full (JPEG) range.
    pub range: i32,
    pub space: i32,
    pub trc: i32,
    pub primaries: i32,
}

pub const AVCOL_RANGE_JPEG: i32 = 2;
pub const AVCOL_SPC_BT2020_NCL: i32 = 9;
pub const AVCOL_SPC_BT470BG: i32 = 5;
pub const AVCOL_SPC_SMPTE170M: i32 = 6;
pub const AVCOL_TRC_SMPTE2084: i32 = 16;

/// A decoded frame.
pub struct Picture {
    raw: QsPicture,
}

unsafe impl Send for Picture {}

impl Picture {
    pub fn is_vulkan(&self) -> bool {
        self.raw.kind == 1
    }
    pub fn width(&self) -> u32 {
        self.raw.width as u32
    }
    pub fn height(&self) -> u32 {
        self.raw.height as u32
    }
    pub fn pts(&self) -> i64 {
        self.raw.pts
    }
    /// Microseconds spent copying a hardware frame to memory.
    pub fn transfer_us(&self) -> u64 {
        self.raw.transfer_us.max(0) as u64
    }
    pub fn layout(&self) -> PlaneLayout {
        PlaneLayout {
            planes: self.raw.nb_planes as u32,
            log2_chroma_w: self.raw.log2_chroma_w as u32,
            log2_chroma_h: self.raw.log2_chroma_h as u32,
            depth: self.raw.depth as u32,
            bytes: self.raw.bytes as u32,
            shift: self.raw.shift as u32,
        }
    }
    pub fn color(&self) -> FrameColor {
        FrameColor {
            range: self.raw.color_range,
            space: self.raw.colorspace,
            trc: self.raw.color_trc,
            primaries: self.raw.color_primaries,
        }
    }

    /// A CPU plane's rows: (bytes, stride, rows).
    pub fn plane(&self, i: usize) -> Option<(&[u8], usize, usize)> {
        if self.is_vulkan() || i >= self.raw.nb_planes as usize {
            return None;
        }
        let l = self.layout();
        let rows = if i == 0 { self.height() } else { self.height().div_ceil(1 << l.log2_chroma_h) } as usize;
        let stride = self.raw.linesize[i];
        if stride <= 0 || self.raw.data[i].is_null() {
            return None;
        }
        let len = stride as usize * rows;
        Some((unsafe { std::slice::from_raw_parts(self.raw.data[i], len) }, stride as usize, rows))
    }

    /// Lock the frame's Vulkan image to use it on the GPU. Wait on `semaphore` at
    /// `sem_value`, signal `sem_value + 1`, and pass the new state to [`Picture::vk_unlock`].
    pub fn vk_lock(&mut self) -> Option<VkImageInfo> {
        let mut info = VkImageInfo::default();
        (unsafe { qs_pic_vk_lock(&mut self.raw, &mut info) } == 0).then_some(info)
    }

    pub fn vk_unlock(&mut self, info: &VkImageInfo) {
        unsafe { qs_pic_vk_unlock(&mut self.raw, info) }
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        unsafe { qs_pic_free(&mut self.raw) }
    }
}

/// A video file, read one access unit at a time.
pub struct Demuxer {
    raw: NonNull<QsDemux>,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// The file's frame rate, if it says (raw streams often don't).
    pub fps: Option<f64>,
}

unsafe impl Send for Demuxer {}

impl Demuxer {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let c = CString::new(path.as_os_str().as_encoded_bytes())?;
        let mut info = QsStreamInfo::default();
        let mut err = [0 as c_char; 256];
        let raw = unsafe { qs_demux_open(c.as_ptr(), &mut info, err.as_mut_ptr(), err.len() as c_int) };
        let raw = NonNull::new(raw).ok_or_else(|| anyhow!("{}: {}", path.display(), err_string(&err)))?;
        let codec = match info.codec {
            0 => VideoCodec::H264,
            1 => VideoCodec::Hevc,
            _ => VideoCodec::Av1,
        };
        let fps = (info.fps_num > 0 && info.fps_den > 0).then(|| info.fps_num as f64 / info.fps_den as f64);
        Ok(Self { raw, codec, width: info.width as u32, height: info.height as u32, fps })
    }

    /// The next access unit: (data, keyframe), or None at the end.
    pub fn read(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        let mut p = QsPacket { data: std::ptr::null(), size: 0, key: 0 };
        let r = unsafe { qs_demux_read(self.raw.as_ptr(), &mut p) };
        match r {
            1 => Ok(Some((unsafe { std::slice::from_raw_parts(p.data, p.size as usize) }.to_vec(), p.key != 0))),
            0 => Ok(None),
            e => bail!("reading the file: {}", strerror(e)),
        }
    }
}

impl Drop for Demuxer {
    fn drop(&mut self) {
        unsafe { qs_demux_close(self.raw.as_ptr()) }
    }
}
