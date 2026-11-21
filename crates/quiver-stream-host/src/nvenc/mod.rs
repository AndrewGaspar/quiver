//! Low-latency NVENC, through the NVENC API directly (`libnvidia-encode.so.1`, opened at run
//! time) with CUDA input.
//!
//! Chosen over libav (ffmpeg's `*_nvenc`) because a stream needs what libav doesn't expose:
//! **reference-frame invalidation** (`nvEncInvalidateRefFrames`) answering a client's
//! [`Control::LostFrames`](quiver_stream_proto::Control::LostFrames) without a keyframe, and
//! **reconfiguration without an IDR** for every bitrate step the transport asks for. It also
//! lets capture hand the encoder CUDA memory it imported, with no copy through libav's frames.
//!
//! Every session is configured the same way: ultra-low-latency tuning, IPPP with no B-frames
//! and an infinite GOP, one slice, CBR with a VBV of one frame, parameter sets (or AV1's
//! sequence header) in front of every keyframe, a DPB of [`DPB`] frames so invalidated
//! references have something older to fall back on, and one L0 reference per frame.
//!
//! Input is a CUDA buffer in one of the [`InputFormat`]s, written by the source between
//! [`Encoder::surface`] and [`Encoder::encode`]. The encoder is synchronous: `encode` returns
//! the finished access unit.

pub mod sys;

use std::ffi::{CStr, c_void};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use libloading::Library;
use quiver_stream_proto::{Chroma, ColorInfo, EncodedFrame, HdrMetadata, Micros, VideoCodec};

use crate::cuda::{Cuda, DeviceBuf};
use crate::recovery::{Recovery, RefTracker};
use sys::*;

/// The DPB size every session asks for. Reference invalidation can only fall back on a frame
/// still in it, so this is how many frames of loss can be repaired without a keyframe.
pub const DPB: u32 = 5;

/// How many input buffers the encoder rotates through.
const INPUTS: usize = 3;

/// The NVENC API this crate is built against (nv-codec-headers 13.1).
const API_MAJOR: u32 = NVENCAPI_MAJOR_VERSION;
const API_MINOR: u32 = NVENCAPI_MINOR_VERSION;

mod guid {
    use super::GUID;
    const fn g(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
        GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
    }
    pub const CODEC_H264: GUID = g(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
    pub const CODEC_HEVC: GUID = g(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
    pub const CODEC_AV1: GUID = g(0x0a352289, 0x0aa7, 0x4759, [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54]);
    pub const H264_HIGH: GUID = g(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
    pub const H264_HIGH_10: GUID = g(0x8f0c337e, 0x186c, 0x48e9, [0xa6, 0x9d, 0x7a, 0x83, 0x34, 0x08, 0x97, 0x58]);
    pub const H264_HIGH_444: GUID = g(0x7ac663cb, 0xa598, 0x4960, [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52]);
    pub const HEVC_MAIN: GUID = g(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
    pub const HEVC_MAIN10: GUID = g(0xfa4d2b6c, 0x3a5b, 0x411a, [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5]);
    pub const HEVC_FREXT: GUID = g(0x51ec32b5, 0x1b4c, 0x453c, [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41]);
    pub const AV1_MAIN: GUID = g(0x5f2a39f5, 0xf14e, 0x4f95, [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97]);
    pub const PRESETS: [GUID; 7] = [
        g(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]),
        g(0xf581cfb8, 0x88d6, 0x4381, [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab]),
        g(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]),
        g(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]),
        g(0x21c6e6b4, 0x297a, 0x4cba, [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3]),
        g(0x8e75c279, 0x6299, 0x4ab6, [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5]),
        g(0x84848c12, 0x6f71, 0x4c13, [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74]),
    ];
}

/// The `version` fields: `NVENCAPI_STRUCT_VERSION(n)`, with bit 31 for the structs the header
/// marks so.
mod ver {
    use super::NVENCAPI_VERSION;
    pub const fn v(n: u32) -> u32 {
        NVENCAPI_VERSION | (n << 16) | (0x7 << 28)
    }
    pub const fn vx(n: u32) -> u32 {
        v(n) | (1 << 31)
    }
    pub const FUNCTION_LIST: u32 = v(2);
    pub const OPEN_SESSION_EX: u32 = v(1);
    pub const CAPS_PARAM: u32 = v(1);
    pub const PRESET_CONFIG: u32 = vx(5);
    pub const CONFIG: u32 = vx(9);
    pub const RC_PARAMS: u32 = v(1);
    pub const INITIALIZE_PARAMS: u32 = vx(7);
    pub const RECONFIGURE_PARAMS: u32 = vx(2);
    pub const PIC_PARAMS: u32 = vx(7);
    pub const LOCK_BITSTREAM: u32 = vx(2);
    pub const CREATE_BITSTREAM_BUFFER: u32 = v(1);
    pub const REGISTER_RESOURCE: u32 = v(5);
    pub const MAP_INPUT_RESOURCE: u32 = v(4);
}

/// The layout of the encoder's input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// 4:2:0, 8-bit: a Y plane, then interleaved UV at half height.
    Nv12,
    /// 4:2:0, 10-bit in the top bits of 16 (P010).
    P010,
    /// 4:4:4, 8-bit: three full planes.
    Yuv444,
    /// 4:4:4, 10-bit in the top bits of 16: three full planes.
    Yuv444P16,
    /// 8-bit B, G, R, X bytes (PipeWire's BGRx, DRM's XRGB8888). NVENC converts to YUV.
    Bgrx,
    /// 10-bit RGB packed in 32 bits, B in the low bits (DRM's XRGB2101010).
    Xrgb2101010,
}

impl InputFormat {
    fn nv(self) -> NV_ENC_BUFFER_FORMAT {
        match self {
            InputFormat::Nv12 => NV_ENC_BUFFER_FORMAT_NV12,
            InputFormat::P010 => NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
            InputFormat::Yuv444 => NV_ENC_BUFFER_FORMAT_YUV444,
            InputFormat::Yuv444P16 => NV_ENC_BUFFER_FORMAT_YUV444_10BIT,
            InputFormat::Bgrx => NV_ENC_BUFFER_FORMAT_ARGB,
            InputFormat::Xrgb2101010 => NV_ENC_BUFFER_FORMAT_ARGB10,
        }
    }

    /// Bytes per sample of the first plane, per pixel.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            InputFormat::Nv12 | InputFormat::Yuv444 => 1,
            InputFormat::P010 | InputFormat::Yuv444P16 => 2,
            InputFormat::Bgrx | InputFormat::Xrgb2101010 => 4,
        }
    }

    /// The buffer's rows, for a picture `height` tall: every plane is stacked at the same
    /// pitch.
    pub fn rows(self, height: u32) -> usize {
        let h = height as usize;
        match self {
            InputFormat::Nv12 | InputFormat::P010 => h + h.div_ceil(2),
            InputFormat::Yuv444 | InputFormat::Yuv444P16 => h * 3,
            InputFormat::Bgrx | InputFormat::Xrgb2101010 => h,
        }
    }

    pub fn ten_bit(self) -> bool {
        matches!(self, InputFormat::P010 | InputFormat::Yuv444P16 | InputFormat::Xrgb2101010)
    }

    pub fn is_rgb(self) -> bool {
        matches!(self, InputFormat::Bgrx | InputFormat::Xrgb2101010)
    }

    /// The YUV format a test pattern draws for this colour setup.
    pub fn yuv_for(color: &ColorInfo) -> InputFormat {
        match (color.chroma, color.bit_depth > 8) {
            (Chroma::Yuv420, false) => InputFormat::Nv12,
            (Chroma::Yuv420, true) => InputFormat::P010,
            (Chroma::Yuv444, false) => InputFormat::Yuv444,
            (Chroma::Yuv444, true) => InputFormat::Yuv444P16,
        }
    }
}

/// One encode session's settings.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// The frame rate, as a fraction (the virtual display's refresh: mHz / 1000).
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_kbps: u32,
    /// What to encode as: bit depth, chroma, range and HDR.
    pub color: ColorInfo,
    /// What the source writes.
    pub input: InputFormat,
    /// NVENC preset 1 (fastest) to 7 (best).
    pub preset: u8,
    pub recovery: Recovery,
}

impl EncoderConfig {
    /// VBV of one frame, in bits.
    pub fn vbv_bits(&self) -> u32 {
        ((self.bitrate_kbps as u64 * 1000 * self.fps_den as u64) / self.fps_num.max(1) as u64) as u32
    }
}

/// What this GPU's NVENC can do for one codec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    pub max_width: u32,
    pub max_height: u32,
    pub ten_bit: bool,
    pub yuv444: bool,
    pub ref_invalidation: bool,
    pub intra_refresh: bool,
    pub dynamic_bitrate: bool,
}

struct Api {
    fns: NV_ENCODE_API_FUNCTION_LIST,
    _lib: Library,
}

impl Api {
    fn open() -> Result<Api> {
        // SAFETY: the NVIDIA driver's encode library.
        let lib = unsafe { Library::new("libnvidia-encode.so.1") }.context("open libnvidia-encode.so.1 (the NVIDIA driver's NVENC library)")?;
        type MaxVer = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;
        type Create = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
        let max_ver: MaxVer = *unsafe { lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0") }?;
        let create: Create = *unsafe { lib.get(b"NvEncodeAPICreateInstance\0") }?;
        let mut v = 0u32;
        let r = unsafe { max_ver(&mut v) };
        if r != NV_ENC_SUCCESS {
            bail!("NvEncodeAPIGetMaxSupportedVersion: {}", status_name(r));
        }
        let (maj, min) = (v >> 4, v & 0xf);
        if (maj, min) < (API_MAJOR, API_MINOR) {
            bail!("the NVIDIA driver supports NVENC API {maj}.{min}; this build needs {API_MAJOR}.{API_MINOR} (a newer driver)");
        }
        let mut fns: NV_ENCODE_API_FUNCTION_LIST = unsafe { std::mem::zeroed() };
        fns.version = ver::FUNCTION_LIST;
        let r = unsafe { create(&mut fns) };
        if r != NV_ENC_SUCCESS {
            bail!("NvEncodeAPICreateInstance: {}", status_name(r));
        }
        Ok(Api { fns, _lib: lib })
    }
}

fn status_name(s: NVENCSTATUS) -> String {
    let n = match s {
        0 => "success",
        1 => "no encode device",
        2 => "unsupported device",
        3 => "invalid encoder device",
        4 => "invalid device",
        5 => "device not exist",
        6 => "invalid ptr",
        7 => "invalid event",
        8 => "invalid param",
        9 => "invalid call",
        10 => "out of memory",
        11 => "encoder not initialized",
        12 => "unsupported param",
        13 => "lock busy",
        14 => "not enough buffer",
        15 => "invalid version",
        16 => "map failed",
        17 => "need more input",
        18 => "encoder busy",
        19 => "event not registered",
        20 => "generic",
        21 => "incompatible client key",
        22 => "unimplemented",
        23 => "resource register failed",
        24 => "resource not registered",
        25 => "resource not mapped",
        26 => "need more output",
        _ => "unknown",
    };
    format!("NVENC error {s} ({n})")
}

/// One of the encoder's input buffers: CUDA memory, every plane stacked at one pitch.
pub struct Surface {
    pub buf: DeviceBuf,
    pub format: InputFormat,
    pub width: u32,
    pub height: u32,
    registered: NV_ENC_REGISTERED_PTR,
    /// What the source last wrote into this buffer, so it can skip what hasn't changed
    /// ([`Surface::tag`]).
    pub tag: u64,
}

impl Surface {
    pub fn pitch(&self) -> usize {
        self.buf.pitch
    }
    /// The device address of row `row` of the stacked planes.
    pub fn row(&self, row: usize) -> u64 {
        self.buf.ptr + (row * self.buf.pitch) as u64
    }
}

/// What happened to one encode.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeTiming {
    /// From `encode` being called to the bitstream being in host memory.
    pub encode_us: u32,
}

pub struct Encoder {
    api: Api,
    cuda: Arc<Cuda>,
    enc: *mut c_void,
    cfg: EncoderConfig,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    config: Box<NV_ENC_CONFIG>,
    surfaces: Vec<Surface>,
    next: usize,
    bitstream: NV_ENC_OUTPUT_PTR,
    pub caps: Caps,
    refs: RefTracker,
    force_key: bool,
    frames: u64,
}

// SAFETY: the session is used from one thread at a time (the encoder thread owns it).
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(cuda: Arc<Cuda>, cfg: EncoderConfig) -> Result<Encoder> {
        let api = Api::open()?;
        let _g = cuda.push()?;
        let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { std::mem::zeroed() };
        open.version = ver::OPEN_SESSION_EX;
        open.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
        open.device = cuda.context();
        open.apiVersion = NVENCAPI_VERSION;
        let mut enc = std::ptr::null_mut();
        let r = unsafe { api.fns.nvEncOpenEncodeSessionEx.unwrap()(&mut open, &mut enc) };
        if r != NV_ENC_SUCCESS {
            bail!("nvEncOpenEncodeSessionEx: {} (are all NVENC sessions in use?)", status_name(r));
        }
        let mut e = Encoder {
            api,
            cuda: cuda.clone(),
            enc,
            init: Box::new(unsafe { std::mem::zeroed() }),
            config: Box::new(unsafe { std::mem::zeroed() }),
            surfaces: Vec::new(),
            next: 0,
            bitstream: std::ptr::null_mut(),
            caps: Caps::default(),
            refs: RefTracker::new(DPB as usize),
            force_key: true,
            frames: 0,
            cfg,
        };
        e.caps = e.query_caps(codec_guid(e.cfg.codec))?;
        e.check_supported()?;
        e.initialize()?;
        e.allocate()?;
        Ok(e)
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    fn fail(&self, what: &str, r: NVENCSTATUS) -> anyhow::Error {
        let last = unsafe { self.api.fns.nvEncGetLastErrorString.unwrap()(self.enc) };
        let detail = if last.is_null() { String::new() } else { unsafe { CStr::from_ptr(last) }.to_string_lossy().into_owned() };
        if detail.is_empty() { anyhow::anyhow!("{what}: {}", status_name(r)) } else { anyhow::anyhow!("{what}: {}: {detail}", status_name(r)) }
    }

    fn cap(&self, codec: GUID, which: NV_ENC_CAPS) -> Result<i32> {
        let mut p: NV_ENC_CAPS_PARAM = unsafe { std::mem::zeroed() };
        p.version = ver::CAPS_PARAM;
        p.capsToQuery = which;
        let mut v = 0;
        let r = unsafe { self.api.fns.nvEncGetEncodeCaps.unwrap()(self.enc, codec, &mut p, &mut v) };
        if r != NV_ENC_SUCCESS {
            return Err(self.fail("nvEncGetEncodeCaps", r));
        }
        Ok(v)
    }

    fn query_caps(&self, codec: GUID) -> Result<Caps> {
        Ok(Caps {
            max_width: self.cap(codec, NV_ENC_CAPS_WIDTH_MAX)? as u32,
            max_height: self.cap(codec, NV_ENC_CAPS_HEIGHT_MAX)? as u32,
            ten_bit: self.cap(codec, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE)? != 0,
            yuv444: self.cap(codec, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE)? != 0,
            ref_invalidation: self.cap(codec, NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION)? != 0,
            intra_refresh: self.cap(codec, NV_ENC_CAPS_SUPPORT_INTRA_REFRESH)? != 0,
            dynamic_bitrate: self.cap(codec, NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE)? != 0,
        })
    }

    fn check_supported(&self) -> Result<()> {
        let c = &self.cfg;
        if c.width > self.caps.max_width || c.height > self.caps.max_height {
            bail!("{}x{} is larger than NVENC's {:?} limit ({}x{})", c.width, c.height, c.codec, self.caps.max_width, self.caps.max_height);
        }
        if c.color.bit_depth > 8 && !self.caps.ten_bit {
            bail!("this GPU's NVENC can't encode 10-bit {:?}", c.codec);
        }
        if c.color.chroma == Chroma::Yuv444 && !self.caps.yuv444 {
            bail!("this GPU's NVENC can't encode 4:4:4 {:?}", c.codec);
        }
        if c.color.chroma == Chroma::Yuv444 && c.codec == VideoCodec::Av1 {
            bail!("NVENC doesn't encode 4:4:4 AV1");
        }
        if c.width % 2 != 0 || c.height % 2 != 0 {
            bail!("the size must be even ({}x{})", c.width, c.height);
        }
        Ok(())
    }

    fn initialize(&mut self) -> Result<()> {
        let c = self.cfg.clone();
        let codec = codec_guid(c.codec);
        let preset = guid::PRESETS[(c.preset.clamp(1, 7) - 1) as usize];
        let mut pc: NV_ENC_PRESET_CONFIG = unsafe { std::mem::zeroed() };
        pc.version = ver::PRESET_CONFIG;
        pc.presetCfg.version = ver::CONFIG;
        let r = unsafe { self.api.fns.nvEncGetEncodePresetConfigEx.unwrap()(self.enc, codec, preset, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &mut pc) };
        if r != NV_ENC_SUCCESS {
            return Err(self.fail("nvEncGetEncodePresetConfigEx", r));
        }
        let mut cfg = pc.presetCfg;
        cfg.version = ver::CONFIG;
        apply_config(&mut cfg, &c, &self.caps);

        *self.config = cfg;
        let init = &mut *self.init;
        init.version = ver::INITIALIZE_PARAMS;
        init.encodeGUID = codec;
        init.presetGUID = preset;
        init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.encodeWidth = c.width;
        init.encodeHeight = c.height;
        init.darWidth = c.width;
        init.darHeight = c.height;
        init.maxEncodeWidth = c.width;
        init.maxEncodeHeight = c.height;
        init.frameRateNum = c.fps_num;
        init.frameRateDen = c.fps_den;
        init.enablePTD = 1;
        init.encodeConfig = &mut *self.config;
        let r = unsafe { self.api.fns.nvEncInitializeEncoder.unwrap()(self.enc, init) };
        if r != NV_ENC_SUCCESS {
            return Err(self.fail("nvEncInitializeEncoder", r));
        }
        Ok(())
    }

    fn allocate(&mut self) -> Result<()> {
        let c = &self.cfg;
        for _ in 0..INPUTS {
            let buf = self.cuda.alloc_pitch(c.width as usize * c.input.bytes_per_pixel(), c.input.rows(c.height))?;
            let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
            reg.version = ver::REGISTER_RESOURCE;
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
            reg.width = c.width;
            reg.height = c.height;
            reg.pitch = buf.pitch as u32;
            reg.resourceToRegister = buf.ptr as *mut c_void;
            reg.bufferFormat = c.input.nv();
            reg.bufferUsage = NV_ENC_INPUT_IMAGE;
            let r = unsafe { self.api.fns.nvEncRegisterResource.unwrap()(self.enc, &mut reg) };
            if r != NV_ENC_SUCCESS {
                return Err(self.fail("nvEncRegisterResource", r));
            }
            self.surfaces.push(Surface { buf, format: c.input, width: c.width, height: c.height, registered: reg.registeredResource, tag: 0 });
        }
        let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
        bs.version = ver::CREATE_BITSTREAM_BUFFER;
        let r = unsafe { self.api.fns.nvEncCreateBitstreamBuffer.unwrap()(self.enc, &mut bs) };
        if r != NV_ENC_SUCCESS {
            return Err(self.fail("nvEncCreateBitstreamBuffer", r));
        }
        self.bitstream = bs.bitstreamBuffer;
        Ok(())
    }

    /// The buffer the next [`Encoder::encode`] reads. The CUDA context must be current while
    /// the source writes it ([`Cuda::push`]).
    pub fn surface(&mut self) -> &mut Surface {
        &mut self.surfaces[self.next]
    }

    pub fn cuda(&self) -> &Arc<Cuda> {
        &self.cuda
    }

    /// Ask for the next frame to be a keyframe.
    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    /// The client lost frames `since..=through`. Invalidate them as references, or, when
    /// nothing older is left in the DPB to fall back on, make the next frame a keyframe.
    /// Returns false when it will be a keyframe.
    pub fn lost(&mut self, since: u64, through: u64) -> Result<bool> {
        if self.cfg.recovery != Recovery::RefInvalidation || !self.caps.ref_invalidation {
            self.force_key = true;
            return Ok(false);
        }
        match self.refs.lost(since, through) {
            crate::recovery::Action::Nothing => Ok(true),
            crate::recovery::Action::Keyframe => {
                self.force_key = true;
                Ok(false)
            }
            crate::recovery::Action::Invalidate(seqs) => {
                for s in seqs {
                    let r = unsafe { self.api.fns.nvEncInvalidateRefFrames.unwrap()(self.enc, s) };
                    if r != NV_ENC_SUCCESS {
                        tracing::warn!("nvEncInvalidateRefFrames({s}): {}; sending a keyframe", status_name(r));
                        self.force_key = true;
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// Change the bitrate without a keyframe (and the VBV with it, to stay one frame).
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        if kbps == self.cfg.bitrate_kbps || kbps == 0 {
            return Ok(());
        }
        self.cfg.bitrate_kbps = kbps;
        let bps = kbps.saturating_mul(1000);
        self.config.rcParams.averageBitRate = bps;
        self.config.rcParams.maxBitRate = bps;
        self.config.rcParams.vbvBufferSize = self.cfg.vbv_bits();
        self.config.rcParams.vbvInitialDelay = self.cfg.vbv_bits();
        let mut re: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        re.version = ver::RECONFIGURE_PARAMS;
        // SAFETY: a plain C struct; the copy points at the same config.
        re.reInitEncodeParams = unsafe { std::ptr::read(&*self.init) };
        re.reInitEncodeParams.encodeConfig = &mut *self.config;
        re.set_resetEncoder(0);
        re.set_forceIDR(0);
        let r = unsafe { self.api.fns.nvEncReconfigureEncoder.unwrap()(self.enc, &mut re) };
        if r != NV_ENC_SUCCESS {
            return Err(self.fail("nvEncReconfigureEncoder (bitrate)", r));
        }
        Ok(())
    }

    /// Encode the current surface as frame `seq`, captured at `captured`. The CUDA context
    /// must be current.
    pub fn encode(&mut self, seq: u64, captured: Micros) -> Result<(EncodedFrame, EncodeTiming)> {
        let t0 = Instant::now();
        let surface = &self.surfaces[self.next];
        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { std::mem::zeroed() };
        map.version = ver::MAP_INPUT_RESOURCE;
        map.registeredResource = surface.registered;
        let r = unsafe { self.api.fns.nvEncMapInputResource.unwrap()(self.enc, &mut map) };
        if r != NV_ENC_SUCCESS {
            return Err(self.fail("nvEncMapInputResource", r));
        }
        let key = self.force_key;
        let mut pic: NV_ENC_PIC_PARAMS = unsafe { std::mem::zeroed() };
        pic.version = ver::PIC_PARAMS;
        pic.inputWidth = self.cfg.width;
        pic.inputHeight = self.cfg.height;
        pic.inputPitch = surface.buf.pitch as u32;
        pic.inputBuffer = map.mappedResource;
        pic.bufferFmt = map.mappedBufferFmt;
        pic.outputBitstream = self.bitstream;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        pic.inputTimeStamp = seq;
        pic.frameIdx = self.frames as u32;
        if key {
            pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        // HDR metadata rides on keyframes.
        let hdr = if key { self.cfg.color.hdr.clone() } else { None };
        let (mut mdi, mut cll) = match (&hdr, self.cfg.codec) {
            (Some(h), VideoCodec::Hevc) => mastering_hevc(h),
            (Some(h), VideoCodec::Av1) => mastering_av1(h),
            _ => Default::default(),
        };
        let mut sei_payloads: Vec<Vec<u8>> = Vec::new();
        let mut sei: Vec<NV_ENC_SEI_PAYLOAD> = Vec::new();
        if let Some(h) = &hdr {
            match self.cfg.codec {
                VideoCodec::Hevc => {
                    pic.codecPicParams.hevcPicParams.pMaxCll = &mut cll;
                    pic.codecPicParams.hevcPicParams.pMasteringDisplay = &mut mdi;
                }
                VideoCodec::Av1 => {
                    pic.codecPicParams.av1PicParams.pMaxCll = &mut cll;
                    pic.codecPicParams.av1PicParams.pMasteringDisplay = &mut mdi;
                }
                VideoCodec::H264 => {
                    sei_payloads = h264_hdr_sei(h);
                    for (ty, p) in [137u32, 144].into_iter().zip(sei_payloads.iter_mut()) {
                        sei.push(NV_ENC_SEI_PAYLOAD { payloadSize: p.len() as u32, payloadType: ty, payload: p.as_mut_ptr() });
                    }
                    pic.codecPicParams.h264PicParams.seiPayloadArray = sei.as_mut_ptr();
                    pic.codecPicParams.h264PicParams.seiPayloadArrayCnt = sei.len() as u32;
                }
            }
        }
        let r = unsafe { self.api.fns.nvEncEncodePicture.unwrap()(self.enc, &mut pic) };
        if r != NV_ENC_SUCCESS {
            unsafe { self.api.fns.nvEncUnmapInputResource.unwrap()(self.enc, map.mappedResource) };
            return Err(self.fail("nvEncEncodePicture", r));
        }
        let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { std::mem::zeroed() };
        lock.version = ver::LOCK_BITSTREAM;
        lock.outputBitstream = self.bitstream;
        let r = unsafe { self.api.fns.nvEncLockBitstream.unwrap()(self.enc, &mut lock) };
        if r != NV_ENC_SUCCESS {
            unsafe { self.api.fns.nvEncUnmapInputResource.unwrap()(self.enc, map.mappedResource) };
            return Err(self.fail("nvEncLockBitstream", r));
        }
        let data = unsafe { std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize) }.to_vec();
        let pic_type = lock.pictureType;
        unsafe {
            self.api.fns.nvEncUnlockBitstream.unwrap()(self.enc, self.bitstream);
            self.api.fns.nvEncUnmapInputResource.unwrap()(self.enc, map.mappedResource);
        }
        drop((sei, sei_payloads));
        let encoded = quiver_stream_proto::now_micros();
        let keyframe = pic_type == NV_ENC_PIC_TYPE_IDR || pic_type == NV_ENC_PIC_TYPE_I;
        let refs_from_seq = self.refs.encoded(seq, keyframe);
        self.force_key = false;
        self.frames += 1;
        self.next = (self.next + 1) % self.surfaces.len();
        let frame = EncodedFrame { seq, captured, encoded, keyframe, refs_from_seq, width: self.cfg.width, height: self.cfg.height, data };
        Ok((frame, EncodeTiming { encode_us: t0.elapsed().as_micros() as u32 }))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let Ok(_g) = self.cuda.push() else { return };
        unsafe {
            for s in &self.surfaces {
                self.api.fns.nvEncUnregisterResource.unwrap()(self.enc, s.registered);
            }
            if !self.bitstream.is_null() {
                self.api.fns.nvEncDestroyBitstreamBuffer.unwrap()(self.enc, self.bitstream);
            }
            self.api.fns.nvEncDestroyEncoder.unwrap()(self.enc);
        }
        // The surfaces' CUDA memory goes after the session that registered it.
        self.surfaces.clear();
    }
}

fn codec_guid(c: VideoCodec) -> GUID {
    match c {
        VideoCodec::H264 => guid::CODEC_H264,
        VideoCodec::Hevc => guid::CODEC_HEVC,
        VideoCodec::Av1 => guid::CODEC_AV1,
    }
}

/// The colour description for VUI / AV1's sequence header: (primaries, transfer, matrix).
fn color_codes(color: &ColorInfo) -> (u32, u32, u32) {
    if color.hdr.is_some() {
        (NV_ENC_VUI_COLOR_PRIMARIES_BT2020, NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084, NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL)
    } else {
        (NV_ENC_VUI_COLOR_PRIMARIES_BT709, NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709, NV_ENC_VUI_MATRIX_COEFFS_BT709)
    }
}

/// The session's configuration on top of the preset's (module docs).
fn apply_config(cfg: &mut NV_ENC_CONFIG, c: &EncoderConfig, caps: &Caps) {
    let bps = c.bitrate_kbps.saturating_mul(1000);
    cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
    cfg.frameIntervalP = 1;
    let rc = &mut cfg.rcParams;
    rc.version = ver::RC_PARAMS;
    rc.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    rc.averageBitRate = bps;
    rc.maxBitRate = bps;
    rc.vbvBufferSize = c.vbv_bits();
    rc.vbvInitialDelay = c.vbv_bits();
    rc.set_zeroReorderDelay(1);
    rc.set_enableLookahead(0);
    rc.multiPass = NV_ENC_MULTI_PASS_DISABLED;

    let ten = c.color.bit_depth > 8;
    let depth = if ten { NV_ENC_BIT_DEPTH_10 } else { NV_ENC_BIT_DEPTH_8 };
    let in_depth = if c.input.ten_bit() { NV_ENC_BIT_DEPTH_10 } else { NV_ENC_BIT_DEPTH_8 };
    let chroma_idc = if c.color.chroma == Chroma::Yuv444 { 3 } else { 1 };
    let (prim, transfer, matrix) = color_codes(&c.color);
    let full = c.color.full_range as u32;
    let intra = c.recovery == Recovery::IntraRefresh && caps.intra_refresh;
    // An intra refresh wave over a quarter second, every second.
    let ir_cnt = (c.fps_num / c.fps_den.max(1) / 4).max(4);
    let ir_period = ir_cnt * 4;
    match c.codec {
        VideoCodec::H264 => {
            cfg.profileGUID = if c.color.chroma == Chroma::Yuv444 {
                guid::H264_HIGH_444
            } else if ten {
                guid::H264_HIGH_10
            } else {
                guid::H264_HIGH
            };
            let h = unsafe { &mut cfg.encodeCodecConfig.h264Config };
            h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            h.set_repeatSPSPPS(1);
            h.maxNumRefFrames = DPB;
            h.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
            h.chromaFormatIDC = chroma_idc;
            h.outputBitDepth = depth;
            h.inputBitDepth = in_depth;
            h.sliceMode = 0;
            h.sliceModeData = 0;
            h.set_enableIntraRefresh(intra as u32);
            if intra {
                h.intraRefreshPeriod = ir_period;
                h.intraRefreshCnt = ir_cnt;
            }
            let v = &mut h.h264VUIParameters;
            v.videoSignalTypePresentFlag = 1;
            v.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            v.videoFullRangeFlag = full;
            v.colourDescriptionPresentFlag = 1;
            v.colourPrimaries = prim;
            v.transferCharacteristics = transfer;
            v.colourMatrix = matrix;
        }
        VideoCodec::Hevc => {
            cfg.profileGUID = if c.color.chroma == Chroma::Yuv444 {
                guid::HEVC_FREXT
            } else if ten {
                guid::HEVC_MAIN10
            } else {
                guid::HEVC_MAIN
            };
            let h = unsafe { &mut cfg.encodeCodecConfig.hevcConfig };
            h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            h.set_repeatSPSPPS(1);
            h.maxNumRefFramesInDPB = DPB;
            h.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
            h.set_chromaFormatIDC(chroma_idc);
            h.outputBitDepth = depth;
            h.inputBitDepth = in_depth;
            h.sliceMode = 0;
            h.sliceModeData = 0;
            h.set_enableIntraRefresh(intra as u32);
            if intra {
                h.intraRefreshPeriod = ir_period;
                h.intraRefreshCnt = ir_cnt;
            }
            if c.color.hdr.is_some() {
                h.set_outputMaxCll(1);
                h.set_outputMasteringDisplay(1);
            }
            let v = &mut h.hevcVUIParameters;
            v.videoSignalTypePresentFlag = 1;
            v.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            v.videoFullRangeFlag = full;
            v.colourDescriptionPresentFlag = 1;
            v.colourPrimaries = prim;
            v.transferCharacteristics = transfer;
            v.colourMatrix = matrix;
        }
        VideoCodec::Av1 => {
            cfg.profileGUID = guid::AV1_MAIN;
            let a = unsafe { &mut cfg.encodeCodecConfig.av1Config };
            a.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            a.set_repeatSeqHdr(1);
            a.set_outputAnnexBFormat(0);
            a.maxNumRefFramesInDPB = DPB;
            a.numFwdRefs = NV_ENC_NUM_REF_FRAMES_1;
            a.set_chromaFormatIDC(1);
            a.outputBitDepth = depth;
            a.inputBitDepth = in_depth;
            a.set_enableIntraRefresh(intra as u32);
            if intra {
                a.intraRefreshPeriod = ir_period;
                a.intraRefreshCnt = ir_cnt;
            }
            if c.color.hdr.is_some() {
                a.set_outputMaxCll(1);
                a.set_outputMasteringDisplay(1);
            }
            a.colorPrimaries = prim;
            a.transferCharacteristics = transfer;
            a.matrixCoefficients = matrix;
            a.colorRange = full;
        }
    }
}

/// HEVC's SEI units: primaries in 0.00002, luminance in 0.0001 cd/m², the same as
/// [`HdrMetadata`]. HEVC orders the primaries G, B, R; [`HdrMetadata::primaries`] is R, G, B.
fn mastering_hevc(h: &HdrMetadata) -> (MASTERING_DISPLAY_INFO, CONTENT_LIGHT_LEVEL) {
    let p = |i: usize| CHROMA_POINTS { x: h.primaries[i][0], y: h.primaries[i][1] };
    (
        MASTERING_DISPLAY_INFO { r: p(0), g: p(1), b: p(2), whitePoint: CHROMA_POINTS { x: h.white_point[0], y: h.white_point[1] }, maxLuma: h.max_luminance, minLuma: h.min_luminance },
        CONTENT_LIGHT_LEVEL { maxContentLightLevel: h.max_cll, maxPicAverageLightLevel: h.max_fall },
    )
}

/// AV1's metadata OBU units (AV1 spec 6.7.4): primaries in 0.16 fixed point, maximum
/// luminance in 24.8, minimum in 18.14.
fn mastering_av1(h: &HdrMetadata) -> (MASTERING_DISPLAY_INFO, CONTENT_LIGHT_LEVEL) {
    let xy = |v: u16| ((v as u64 * 65536 + 25_000) / 50_000).min(65535) as u16;
    let p = |i: usize| CHROMA_POINTS { x: xy(h.primaries[i][0]), y: xy(h.primaries[i][1]) };
    (
        MASTERING_DISPLAY_INFO {
            r: p(0),
            g: p(1),
            b: p(2),
            whitePoint: CHROMA_POINTS { x: xy(h.white_point[0]), y: xy(h.white_point[1]) },
            maxLuma: ((h.max_luminance as u64 * 256 + 5_000) / 10_000) as u32,
            minLuma: ((h.min_luminance as u64 * 16_384 + 5_000) / 10_000) as u32,
        },
        CONTENT_LIGHT_LEVEL { maxContentLightLevel: h.max_cll, maxPicAverageLightLevel: h.max_fall },
    )
}

/// H.264 has no NVENC switch for HDR metadata, so the SEI payloads are written here:
/// mastering display colour volume (payload type 137) and content light level (144), the
/// same syntax as HEVC's (H.264 D.1.29 and D.1.31).
pub fn h264_hdr_sei(h: &HdrMetadata) -> Vec<Vec<u8>> {
    let mut m = Vec::with_capacity(24);
    for i in [1usize, 2, 0] {
        m.extend_from_slice(&h.primaries[i][0].to_be_bytes());
        m.extend_from_slice(&h.primaries[i][1].to_be_bytes());
    }
    m.extend_from_slice(&h.white_point[0].to_be_bytes());
    m.extend_from_slice(&h.white_point[1].to_be_bytes());
    m.extend_from_slice(&h.max_luminance.to_be_bytes());
    m.extend_from_slice(&h.min_luminance.to_be_bytes());
    let mut l = Vec::with_capacity(4);
    l.extend_from_slice(&h.max_cll.to_be_bytes());
    l.extend_from_slice(&h.max_fall.to_be_bytes());
    vec![m, l]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr() -> HdrMetadata {
        // BT.2020 primaries (R, G, B), D65, 1000 nits / 0.005 nits, MaxCLL 4000, MaxFALL 600.
        HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 4000, max_fall: 600 }
    }

    #[test]
    fn struct_versions_match_the_header() {
        // NVENCAPI_VERSION for 13.1 is 13 | 1 << 24.
        assert_eq!(NVENCAPI_VERSION, 13 | (1 << 24));
        assert_eq!(ver::CONFIG, NVENCAPI_VERSION | (9 << 16) | (7 << 28) | (1 << 31));
        assert_eq!(ver::FUNCTION_LIST, NVENCAPI_VERSION | (2 << 16) | (7 << 28));
    }

    #[test]
    fn h264_sei_is_g_b_r_big_endian() {
        let s = h264_hdr_sei(&hdr());
        assert_eq!(s[0].len(), 24);
        assert_eq!(&s[0][..4], &[8500u16.to_be_bytes(), 39850u16.to_be_bytes()].concat()[..]);
        assert_eq!(&s[0][16..20], &10_000_000u32.to_be_bytes());
        assert_eq!(s[1], vec![0x0f, 0xa0, 0x02, 0x58]);
    }

    #[test]
    fn av1_units() {
        let (m, c) = mastering_av1(&hdr());
        // 0.708 → 0.708 * 65536 = 46399.
        assert_eq!(m.r.x, 46399);
        // 1000 nits in 24.8 is 256000; 0.005 nits in 18.14 is 82.
        assert_eq!(m.maxLuma, 256_000);
        assert_eq!(m.minLuma, 82);
        assert_eq!((c.maxContentLightLevel, c.maxPicAverageLightLevel), (4000, 600));
        let (h, _) = mastering_hevc(&hdr());
        assert_eq!((h.g.x, h.r.x, h.maxLuma), (8500, 35400, 10_000_000));
    }

    #[test]
    fn rows_and_vbv() {
        assert_eq!(InputFormat::Nv12.rows(1440), 2160);
        assert_eq!(InputFormat::Yuv444P16.rows(1080), 3240);
        assert_eq!(InputFormat::Bgrx.rows(720), 720);
        let c = EncoderConfig {
            codec: VideoCodec::Hevc,
            width: 2560,
            height: 1440,
            fps_num: 120,
            fps_den: 1,
            bitrate_kbps: 60_000,
            color: ColorInfo::sdr(),
            input: InputFormat::Nv12,
            preset: 1,
            recovery: Recovery::RefInvalidation,
        };
        assert_eq!(c.vbv_bits(), 500_000);
    }
}
