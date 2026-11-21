//! Video decoding: picking the backend, and probing what this machine can decode.
//!
//! The order is Vulkan Video (zero-copy into the presenter, on the presenter's own device),
//! then NVDEC, then VA-API (both copy each frame down and up again), then software (slice
//! threads, no frame-threading delay; dav1d with a frame delay of 1 for AV1). Every decoder
//! runs with `AV_CODEC_FLAG_LOW_DELAY`: no reordering, each frame out as soon as it is in.

use std::sync::Arc;

use anyhow::{Result, bail};
use quiver_stream_proto::{Chroma, DecoderCaps, VideoCodec};
use tracing::{info, warn};

use crate::av::{Backend, Decoder, HwDevice, Picture};
use crate::gpu::Gpu;

pub const ALL_CODECS: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1];

pub struct VideoDecoder {
    gpu: Arc<Gpu>,
    /// Backends to try, in order.
    order: Vec<Backend>,
    vulkan: Option<HwDevice>,
    others: Vec<HwDevice>,
    dec: Option<Decoder>,
    /// Backends that failed for the current codec (a frame that wouldn't convert).
    failed: Vec<Backend>,
}

impl VideoDecoder {
    /// `only`: one backend, no fallback (for measuring).
    pub fn new(gpu: Arc<Gpu>, only: Option<Backend>) -> Self {
        let order = match only {
            Some(b) => vec![b],
            None => vec![Backend::Vulkan, Backend::Cuda, Backend::Vaapi, Backend::Software],
        };
        Self { gpu, order, vulkan: None, others: Vec::new(), dec: None, failed: Vec::new() }
    }

    fn device(&mut self, b: Backend) -> Result<Option<&HwDevice>> {
        match b {
            Backend::Software => Ok(None),
            Backend::Vulkan => {
                if self.gpu.decode_families.is_empty() {
                    bail!("no Vulkan Video decode queue on {}", self.gpu.name);
                }
                if self.vulkan.is_none() {
                    let desc = self.gpu.libav_device();
                    // SAFETY: `desc` points into `self.gpu`, which outlives the device (both
                    // live in `self`, and `vulkan` is dropped first).
                    self.vulkan = Some(unsafe { HwDevice::vulkan(&desc) }?);
                }
                Ok(self.vulkan.as_ref())
            }
            other => {
                if !self.others.iter().any(|d| d.backend == other) {
                    self.others.push(HwDevice::create(other)?);
                }
                Ok(self.others.iter().find(|d| d.backend == other))
            }
        }
    }

    /// (Re)open for `codec`, on the first backend that works.
    pub fn open(&mut self, codec: VideoCodec) -> Result<Backend> {
        if self.dec.as_ref().is_some_and(|d| d.codec != codec) {
            self.failed.clear();
        }
        self.dec = None;
        let mut why = Vec::new();
        for b in self.order.clone() {
            if self.failed.contains(&b) {
                continue;
            }
            let dev = match self.device(b) {
                Ok(d) => d,
                Err(e) => {
                    why.push(format!("{b:?}: {e:#}"));
                    continue;
                }
            };
            match Decoder::open(codec, dev, 0) {
                Ok(d) => {
                    info!("decoding {codec:?} with {} ({b:?})", d.name());
                    self.dec = Some(d);
                    return Ok(b);
                }
                Err(e) => why.push(format!("{b:?}: {e:#}")),
            }
        }
        bail!("no decoder for {codec:?}: {}", why.join("; "))
    }

    /// The backend in use failed on a frame; open the next one.
    pub fn fall_back(&mut self) -> Result<Backend> {
        let Some(d) = self.dec.as_ref() else { bail!("no decoder") };
        let (codec, b) = (d.codec, d.backend());
        warn!("{b:?} decoding failed; falling back");
        self.failed.push(b);
        self.open(codec)
    }

    pub fn codec(&self) -> Option<VideoCodec> {
        self.dec.as_ref().map(|d| d.codec)
    }

    pub fn backend(&self) -> Option<Backend> {
        self.dec.as_ref().map(|d| d.backend())
    }

    /// Decode one access unit; returns the pictures it produced (one, with low delay).
    pub fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<Picture>, i32> {
        let Some(d) = self.dec.as_mut() else {
            return Err(-1);
        };
        d.send(data, pts)?;
        let mut out = Vec::new();
        while let Some(p) = d.receive()? {
            out.push(p);
        }
        Ok(out)
    }

    /// What this machine can decode, honestly: Vulkan Video's own capability queries for
    /// the hardware path, else whether NVDEC/VA-API devices open, else software.
    pub fn probe(gpu: &Gpu) -> Vec<DecoderCaps> {
        let mut out = Vec::new();
        let cuda = HwDevice::create(Backend::Cuda).is_ok();
        let vaapi = !cuda && HwDevice::create(Backend::Vaapi).is_ok();
        for codec in ALL_CODECS {
            if let Some(c) = gpu.video_decode_caps(codec) {
                let mut chroma = vec![Chroma::Yuv420];
                if c.yuv444 {
                    chroma.push(Chroma::Yuv444);
                }
                out.push(DecoderCaps {
                    codec,
                    hardware: true,
                    max_width: c.max_width,
                    max_height: c.max_height,
                    ten_bit: c.ten_bit,
                    chroma,
                });
            } else if (cuda || vaapi) && Decoder::open(codec, None, 1).is_ok() {
                // NVDEC/VA-API limits aren't queryable through libav; these are the common
                // floor of current hardware.
                out.push(DecoderCaps {
                    codec,
                    hardware: true,
                    max_width: 4096,
                    max_height: 4096,
                    ten_bit: true,
                    chroma: vec![Chroma::Yuv420],
                });
            } else if Decoder::open(codec, None, 1).is_ok() {
                out.push(DecoderCaps {
                    codec,
                    hardware: false,
                    max_width: 8192,
                    max_height: 8192,
                    ten_bit: true,
                    chroma: vec![Chroma::Yuv420, Chroma::Yuv444],
                });
            }
        }
        out
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.dec = None;
        self.vulkan = None;
        self.others.clear();
    }
}
