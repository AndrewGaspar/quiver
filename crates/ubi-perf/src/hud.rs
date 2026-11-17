//! `hud.image` (SIGHT-HUD §3.8): the HUD image quiver-sight renders and the Sight layer
//! composites. Double-buffered, a seqlock per buffer, a publish counter.
//!
//! **Writer** (quiver-sight): `w = (published + 1) % 2`; `buffer[w].seq` to odd; release
//! fence; descriptor and pixels; `seq` to even (release); `published += 1` (release).
//!
//! **Reader** (the layer, once per present): load `published` (acquire); equal to the last
//! one uploaded: nothing to do. Else `buffer[published % 2].seq` must be even; copy the
//! descriptor, validate it, copy the pixels, acquire fence, re-read `seq`; changed: discard
//! and retry on the next present. Every byte is untrusted (§3.16).

use core::mem::offset_of;
use core::sync::atomic::{Ordering, fence};

use crate::error::Error;
use crate::layout::*;
use crate::shm::{Atomics, ShmRegion};

const OFF_MAGIC: usize = offset_of!(HudHeader, magic);
const OFF_VERSION: usize = offset_of!(HudHeader, version);
const OFF_HEADER_BYTES: usize = offset_of!(HudHeader, header_bytes);
const OFF_BUFFER_BYTES: usize = offset_of!(HudHeader, buffer_bytes);
const OFF_PUBLISHED: usize = offset_of!(HudHeader, published);
const OFF_HEARTBEAT: usize = offset_of!(HudHeader, heartbeat);
const OFF_PID: usize = offset_of!(HudHeader, writer_pid);
const DESC_WORDS: usize = HUD_PIXELS_OFFSET / 8;

/// The layer stops drawing an image whose writer heartbeat is older than this (§2.8).
pub const HUD_STALE_NS: i64 = 3_000_000_000;

/// Bytes of a `hud.image` file with buffers of `buffer_bytes`.
pub const fn hud_file_bytes(buffer_bytes: u32) -> usize {
    HUD_HEADER_BYTES + 2 * buffer_bytes as usize
}

/// The smallest valid buffer size (a multiple of 4096) for a `width × height` image with
/// tightly packed rows; `None` above the 4 MiB cap.
pub fn buffer_bytes_for(width: u32, height: u32) -> Option<u32> {
    let need = HUD_PIXELS_OFFSET as u64 + width as u64 * 4 * height as u64;
    let rounded = need.div_ceil(4096) * 4096;
    (rounded <= HUD_MAX_BUFFER_BYTES as u64).then_some(rounded as u32)
}

/// Packs a premultiplied colour for `qv_hud_graph` (bytes R, G, B, A in memory).
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from_le_bytes([r, g, b, a])
}

/// Premultiplies a straight-alpha colour and packs it.
pub fn rgba_premul(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    rgba(m(r), m(g), m(b), a)
}

/// Premultiplies straight-alpha RGBA8 pixels in place (for a renderer that reads back
/// straight alpha; GPUI's window output is already premultiplied).
pub fn premultiply_in_place(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
}

impl HudBufferHeader {
    pub const ZERO: HudBufferHeader = HudBufferHeader {
        seq: 0,
        width: 0,
        height: 0,
        stride: 0,
        format: HUD_FORMAT_RGBA8_PREMUL_SRGB,
        scale: 1.0,
        extent_w: 0,
        extent_h: 0,
        x: 0,
        y: 0,
        opacity: 1.0,
        ngraphs: 0,
        graph: [HudGraph::ZERO; HUD_MAX_GRAPHS],
        _pad0: 0,
        model_rev: 0,
        _reserved: [0; HUD_PIXELS_OFFSET - 184],
    };

    /// The graphs in use (`ngraphs` clamped to 2).
    pub fn graphs(&self) -> &[HudGraph] {
        &self.graph[..(self.ngraphs as usize).min(HUD_MAX_GRAPHS)]
    }

    /// Bytes of pixel data: `stride × height`.
    pub fn pixel_bytes(&self) -> usize {
        self.stride as usize * self.height as usize
    }

    fn to_words(self) -> [u64; DESC_WORDS] {
        // SAFETY: repr(C), 256 bytes, integers and f32s, explicit padding fields.
        unsafe { core::mem::transmute::<HudBufferHeader, [u64; DESC_WORDS]>(self) }
    }

    fn from_words(w: [u64; DESC_WORDS]) -> HudBufferHeader {
        // SAFETY: every bit pattern is a valid HudBufferHeader (integers and f32s).
        unsafe { core::mem::transmute::<[u64; DESC_WORDS], HudBufferHeader>(w) }
    }

    /// The §3.16 checks, against the buffer capacity and (for the layer) the swapchain extent
    /// it is about to draw on. A descriptor that fails isn't drawn.
    pub fn validate(&self, buffer_bytes: u32, swapchain_extent: Option<(u32, u32)>) -> Result<(), Error> {
        let inv = Error::Invalid;
        if self.format != HUD_FORMAT_RGBA8_PREMUL_SRGB {
            return Err(inv("format"));
        }
        if self.width == 0 || self.height == 0 || self.width > HUD_MAX_DIM || self.height > HUD_MAX_DIM {
            return Err(inv("width/height"));
        }
        if !self.stride.is_multiple_of(4) || (self.stride as u64) < self.width as u64 * 4 {
            return Err(inv("stride"));
        }
        if self.stride as u64 * self.height as u64 + HUD_PIXELS_OFFSET as u64 > buffer_bytes as u64 {
            return Err(inv("image larger than buffer"));
        }
        if !self.scale.is_finite() || self.scale <= 0.0 || self.scale > 16.0 {
            return Err(inv("scale"));
        }
        if !self.opacity.is_finite() || !(0.0..=1.0).contains(&self.opacity) {
            return Err(inv("opacity"));
        }
        if let Some((w, h)) = swapchain_extent
            && (self.extent_w != w || self.extent_h != h)
        {
            return Err(inv("laid out for another extent"));
        }
        let inside = |x: i32, y: i32, w: i64, h: i64, cw: u32, ch: u32| {
            x >= 0 && y >= 0 && x as i64 + w <= cw as i64 && y as i64 + h <= ch as i64
        };
        if !inside(self.x, self.y, self.width as i64, self.height as i64, self.extent_w, self.extent_h) {
            return Err(inv("image outside the swapchain"));
        }
        if self.ngraphs as usize > HUD_MAX_GRAPHS {
            return Err(inv("ngraphs"));
        }
        for g in self.graphs() {
            if g.w <= 0 || g.h <= 0 || !inside(g.x, g.y, g.w as i64, g.h as i64, self.width, self.height) {
                return Err(inv("graph outside the image"));
            }
            if !(1000..=10000).contains(&g.span_ms) {
                return Err(inv("graph span"));
            }
            if !g.min_ms.is_finite() || !g.max_ms.is_finite() || g.min_ms < 0.0 || (g.max_ms != 0.0 && g.max_ms <= g.min_ms) {
                return Err(inv("graph range"));
            }
            if g.series == 0 || g.series & !(GRAPH_SERIES_PRESENTED | GRAPH_SERIES_BASE | GRAPH_SERIES_GENERATED_TINT) != 0 {
                return Err(inv("graph series"));
            }
        }
        Ok(())
    }
}

impl HudGraph {
    pub const ZERO: HudGraph = HudGraph {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        span_ms: 0,
        min_ms: 0.0,
        max_ms: 0.0,
        line: 0,
        fill: 0,
        muted: 0,
        warn_above_ms: 0,
        bad_above_ms: 0,
        warn: 0,
        bad: 0,
        series: 0,
    };
}

/// A validated `hud.image` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudInfo {
    pub buffer_bytes: u32,
}

/// Initializes `region` as an empty `hud.image` (quiverd, at session start); magic last.
pub fn init_hud<R: ShmRegion>(region: &R, buffer_bytes: u32) -> Result<HudInfo, Error> {
    if !region.writable() {
        return Err(Error::ReadOnly);
    }
    if buffer_bytes < 4096 || !buffer_bytes.is_multiple_of(4096) || buffer_bytes > HUD_MAX_BUFFER_BYTES {
        return Err(Error::Layout("buffer_bytes"));
    }
    let need = hud_file_bytes(buffer_bytes);
    if region.len() < need {
        return Err(Error::TooSmall { need, have: region.len() });
    }
    region.u64_at(OFF_MAGIC).store(0, Ordering::Relaxed);
    fence(Ordering::Release);
    // Zero the header and both descriptors; pixels are only read where a descriptor says.
    for off in (8..HUD_HEADER_BYTES).step_by(8) {
        region.u64_at(off).store(0, Ordering::Relaxed);
    }
    for b in 0..2 {
        let base = HUD_HEADER_BYTES + b * buffer_bytes as usize;
        for k in 0..DESC_WORDS {
            region.u64_at(base + 8 * k).store(0, Ordering::Relaxed);
        }
    }
    region.u16_at(OFF_VERSION).store(HUD_VERSION, Ordering::Relaxed);
    region.u16_at(OFF_HEADER_BYTES).store(HUD_HEADER_BYTES as u16, Ordering::Relaxed);
    region.u32_at(OFF_BUFFER_BYTES).store(buffer_bytes, Ordering::Relaxed);
    region.u64_at(OFF_MAGIC).store(HUD_MAGIC, Ordering::Release);
    Ok(HudInfo { buffer_bytes })
}

/// Validates a `hud.image` header in (untrusted) memory.
pub fn read_hud_header<R: ShmRegion>(region: &R) -> Result<HudInfo, Error> {
    if region.len() < HUD_HEADER_BYTES {
        return Err(Error::TooSmall { need: HUD_HEADER_BYTES, have: region.len() });
    }
    let magic = region.u64_at(OFF_MAGIC).load(Ordering::Acquire);
    if magic == 0 {
        return Err(Error::NotReady);
    }
    if magic != HUD_MAGIC {
        return Err(Error::BadMagic(magic));
    }
    let version = region.u16_at(OFF_VERSION).load(Ordering::Relaxed);
    if version != HUD_VERSION {
        return Err(Error::Version(version));
    }
    if region.u16_at(OFF_HEADER_BYTES).load(Ordering::Relaxed) as usize != HUD_HEADER_BYTES {
        return Err(Error::Layout("header_bytes"));
    }
    let buffer_bytes = region.u32_at(OFF_BUFFER_BYTES).load(Ordering::Relaxed);
    if buffer_bytes < 4096 || !buffer_bytes.is_multiple_of(4096) || buffer_bytes > HUD_MAX_BUFFER_BYTES {
        return Err(Error::Layout("buffer_bytes"));
    }
    let need = hud_file_bytes(buffer_bytes);
    if region.len() < need {
        return Err(Error::TooSmall { need, have: region.len() });
    }
    Ok(HudInfo { buffer_bytes })
}

fn buffer_off(info: &HudInfo, b: u64) -> usize {
    HUD_HEADER_BYTES + (b % 2) as usize * info.buffer_bytes as usize
}

/// quiver-sight's side. One writer per file (`&mut self`).
pub struct HudWriter<R: ShmRegion> {
    region: R,
    info: HudInfo,
}

impl<R: ShmRegion> HudWriter<R> {
    /// Attaches to a `hud.image` quiverd created; continues from its publish counter.
    pub fn new(region: R) -> Result<Self, Error> {
        if !region.writable() {
            return Err(Error::ReadOnly);
        }
        let info = read_hud_header(&region)?;
        Ok(HudWriter { region, info })
    }

    pub fn info(&self) -> &HudInfo {
        &self.info
    }

    pub fn region(&self) -> &R {
        &self.region
    }

    pub fn published(&self) -> u64 {
        self.region.u64_at(OFF_PUBLISHED).load(Ordering::Relaxed)
    }

    pub fn set_writer_pid(&self, pid: u32) {
        self.region.u32_at(OFF_PID).store(pid, Ordering::Relaxed);
    }

    /// CLOCK_MONOTONIC_RAW ns, at least once a second.
    pub fn heartbeat(&self, now_ns: i64) {
        self.region.u64_at(OFF_HEARTBEAT).store(now_ns as u64, Ordering::Release);
    }

    /// Writes `desc` (its `seq` is ignored) and `pixels` (premultiplied RGBA8, `stride ×
    /// height` bytes) into the back buffer and publishes it. Returns the new publish count.
    pub fn publish(&mut self, desc: &HudBufferHeader, pixels: &[u8]) -> Result<u64, Error> {
        desc.validate(self.info.buffer_bytes, None)?;
        let len = desc.pixel_bytes();
        if pixels.len() < len {
            return Err(Error::Invalid("fewer pixels than stride × height"));
        }
        let published = self.region.u64_at(OFF_PUBLISHED);
        let p = published.load(Ordering::Relaxed);
        let base = buffer_off(&self.info, p.wrapping_add(1));
        let seq = self.region.u64_at(base);
        let s = seq.load(Ordering::Relaxed);
        let s = s + (s & 1); // a crashed writer may have left it odd
        seq.store(s + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let words = desc.to_words();
        for (k, w) in words.iter().enumerate().skip(1) {
            self.region.u64_at(base + 8 * k).store(*w, Ordering::Relaxed);
        }
        let px = base + HUD_PIXELS_OFFSET;
        let mut chunks = pixels[..len].chunks_exact(8);
        for (k, c) in chunks.by_ref().enumerate() {
            self.region.u64_at(px + 8 * k).store(u64::from_le_bytes(c.try_into().unwrap()), Ordering::Relaxed);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            // In bounds: validate() put stride × height + 256 within a buffer that is a
            // multiple of 4096 bytes, so the rounded-up word fits too.
            self.region.u64_at(px + (len / 8) * 8).store(u64::from_le_bytes(w), Ordering::Relaxed);
        }
        seq.store(s + 2, Ordering::Release);
        published.store(p.wrapping_add(1), Ordering::Release);
        Ok(p.wrapping_add(1))
    }

    /// Leaves a buffer half-written (seq odd), as a writer crashing mid-publish would.
    #[doc(hidden)]
    pub fn begin_without_finish_for_test(&mut self) {
        let p = self.published();
        let seq = self.region.u64_at(buffer_off(&self.info, p.wrapping_add(1)));
        let s = seq.load(Ordering::Relaxed);
        seq.store((s + (s & 1)) + 1, Ordering::Relaxed);
    }
}

/// What [`HudReader::poll`] found.
// The image variant carries the 256-byte descriptor by value: the layer polls once per present
// and copies it anyway; no allocation on that path.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HudPoll {
    /// The publish counter hasn't moved since the last image taken (or rejected).
    Unchanged,
    /// The current buffer is being written; retry next present.
    Busy,
    /// The buffer changed during the copy; retry next present.
    Torn,
    /// The descriptor failed validation, or `dst` is too small; not drawn. Not retried until
    /// the next publish.
    Invalid(Error),
    /// A new image is in `dst[..desc.pixel_bytes()]`. `published` is the counter this poll
    /// saw; the buffer may already hold a later publish of the same buffer (still one
    /// publish's bytes, and the next poll takes the newest anyway).
    Image { published: u64, desc: HudBufferHeader },
}

/// The layer's side. Never writes.
pub struct HudReader<R: ShmRegion> {
    region: R,
    info: HudInfo,
    last: u64,
}

impl<R: ShmRegion> HudReader<R> {
    pub fn new(region: R) -> Result<Self, Error> {
        let info = read_hud_header(&region)?;
        Ok(HudReader { region, info, last: 0 })
    }

    pub fn info(&self) -> &HudInfo {
        &self.info
    }

    /// The publish counter (acquire): one load, the per-present "anything new?" check.
    #[inline]
    pub fn published(&self) -> u64 {
        self.region.u64_at(OFF_PUBLISHED).load(Ordering::Acquire)
    }

    pub fn heartbeat(&self) -> i64 {
        self.region.u64_at(OFF_HEARTBEAT).load(Ordering::Acquire) as i64
    }

    pub fn writer_pid(&self) -> u32 {
        self.region.u32_at(OFF_PID).load(Ordering::Relaxed)
    }

    /// No heartbeat within [`HUD_STALE_NS`] of `now_ns` (CLOCK_MONOTONIC_RAW), or none yet.
    pub fn is_stale(&self, now_ns: i64) -> bool {
        let hb = self.heartbeat();
        hb == 0 || now_ns.saturating_sub(hb) > HUD_STALE_NS
    }

    /// Takes the current image if it is new: copies it into `dst` (at least
    /// `stride × height` bytes; size it to the buffer capacity to be safe) and validates it
    /// against the swapchain extent the layer will draw on.
    pub fn poll(&mut self, swapchain_extent: (u32, u32), dst: &mut [u8]) -> HudPoll {
        let p = self.published();
        if p == self.last {
            return HudPoll::Unchanged;
        }
        let base = buffer_off(&self.info, p);
        let seq = self.region.u64_at(base);
        let s1 = seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return HudPoll::Busy;
        }
        let mut w = [0u64; DESC_WORDS];
        for (k, word) in w.iter_mut().enumerate().skip(1) {
            *word = self.region.u64_at(base + 8 * k).load(Ordering::Relaxed);
        }
        let desc = HudBufferHeader::from_words(w);
        // Check that the descriptor is one writer's before trusting its sizes.
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) != s1 {
            return HudPoll::Torn;
        }
        if let Err(e) = desc.validate(self.info.buffer_bytes, Some(swapchain_extent)) {
            self.last = p;
            return HudPoll::Invalid(e);
        }
        let len = desc.pixel_bytes();
        if dst.len() < len {
            self.last = p;
            return HudPoll::Invalid(Error::TooSmall { need: len, have: dst.len() });
        }
        let px = base + HUD_PIXELS_OFFSET;
        let (whole, rest) = dst[..len].split_at_mut(len / 8 * 8);
        for (k, c) in whole.chunks_exact_mut(8).enumerate() {
            c.copy_from_slice(&self.region.u64_at(px + 8 * k).load(Ordering::Relaxed).to_le_bytes());
        }
        if !rest.is_empty() {
            let wd = self.region.u64_at(px + len / 8 * 8).load(Ordering::Relaxed).to_le_bytes();
            rest.copy_from_slice(&wd[..rest.len()]);
        }
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) != s1 {
            return HudPoll::Torn;
        }
        self.last = p;
        HudPoll::Image { published: p, desc }
    }

    /// Forget the last image taken, so the next poll takes the current one again (after a
    /// swapchain recreate threw the texture away).
    pub fn reset(&mut self) {
        self.last = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::HeapRegion;
    use std::sync::Arc;

    pub(crate) fn desc(w: u32, h: u32, rev: u64) -> HudBufferHeader {
        let mut d = HudBufferHeader::ZERO;
        d.width = w;
        d.height = h;
        d.stride = w * 4;
        d.scale = 2.0;
        d.extent_w = 2560;
        d.extent_h = 1600;
        d.x = 24;
        d.y = 24;
        d.opacity = 0.9;
        d.model_rev = rev;
        d.ngraphs = 1;
        d.graph[0] = HudGraph { x: 8, y: h as i32 / 2, w: w as i32 - 16, h: h as i32 / 4, span_ms: 3000, max_ms: 0.0, series: 1, line: rgba_premul(255, 255, 255, 200), ..HudGraph::ZERO };
        d
    }

    fn pixels(d: &HudBufferHeader) -> Vec<u8> {
        (0..d.pixel_bytes()).map(|i| (i as u64 ^ d.model_rev) as u8).collect()
    }

    #[test]
    fn publish_and_read() {
        let bb = buffer_bytes_for(568, 280).unwrap();
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        assert_eq!(read_hud_header(&mem), Err(Error::NotReady));
        init_hud(&mem, bb).unwrap();
        let mut w = HudWriter::new(mem.clone()).unwrap();
        let mut r = HudReader::new(mem.clone()).unwrap();
        let mut dst = vec![0u8; bb as usize];
        assert_eq!(r.poll((2560, 1600), &mut dst), HudPoll::Unchanged);
        assert!(r.is_stale(10));
        w.heartbeat(5);
        assert!(!r.is_stale(10));
        for rev in 1..=3 {
            let d = desc(568, 280, rev);
            assert_eq!(w.publish(&d, &pixels(&d)), Ok(rev));
            match r.poll((2560, 1600), &mut dst) {
                HudPoll::Image { published, desc: got } => {
                    assert_eq!(published, rev);
                    assert_eq!(got.model_rev, rev);
                    assert_eq!(&dst[..d.pixel_bytes()], &pixels(&d)[..]);
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(r.poll((2560, 1600), &mut dst), HudPoll::Unchanged);
        }
        // Odd pixel byte counts (stride padding is the writer's business, any multiple of 4).
        let mut d = desc(5, 3, 9);
        d.stride = 20;
        d.ngraphs = 0;
        w.publish(&d, &pixels(&d)).unwrap();
        assert!(matches!(r.poll((2560, 1600), &mut dst), HudPoll::Image { .. }));
        assert_eq!(&dst[..60], &pixels(&d)[..]);
        // Another extent: not drawn, not retried until the next publish.
        w.publish(&desc(100, 100, 10), &pixels(&desc(100, 100, 10))).unwrap();
        assert!(matches!(r.poll((1920, 1080), &mut dst), HudPoll::Invalid(_)));
        assert_eq!(r.poll((1920, 1080), &mut dst), HudPoll::Unchanged);
        r.reset();
        assert!(matches!(r.poll((2560, 1600), &mut dst), HudPoll::Image { .. }));
        // dst too small.
        w.publish(&desc(100, 100, 11), &pixels(&desc(100, 100, 11))).unwrap();
        assert!(matches!(r.poll((2560, 1600), &mut [0u8; 16]), HudPoll::Invalid(Error::TooSmall { .. })));
    }

    #[test]
    fn busy_while_writer_is_mid_publish() {
        let bb = buffer_bytes_for(64, 64).unwrap();
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_hud(&mem, bb).unwrap();
        let mut w = HudWriter::new(mem.clone()).unwrap();
        let mut r = HudReader::new(mem.clone()).unwrap();
        let mut dst = vec![0u8; bb as usize];
        w.publish(&desc(64, 64, 1), &pixels(&desc(64, 64, 1))).unwrap();
        // The writer starts on buffer 0 (the back buffer); the front (1) stays readable.
        w.begin_without_finish_for_test();
        assert!(matches!(r.poll((2560, 1600), &mut dst), HudPoll::Image { published: 1, .. }));
        // A restarted writer finishes over a buffer left odd.
        let mut w = HudWriter::new(mem.clone()).unwrap();
        w.publish(&desc(64, 64, 2), &pixels(&desc(64, 64, 2))).unwrap();
        assert!(matches!(r.poll((2560, 1600), &mut dst), HudPoll::Image { published: 2, .. }));
        // A front buffer left odd reads as busy.
        let seq = mem.u64_at(buffer_off(&HudInfo { buffer_bytes: bb }, 3));
        w.publish(&desc(64, 64, 3), &pixels(&desc(64, 64, 3))).unwrap();
        seq.fetch_add(1, Ordering::Relaxed);
        assert_eq!(r.poll((2560, 1600), &mut dst), HudPoll::Busy);
    }

    #[test]
    fn validation() {
        let bb = buffer_bytes_for(284 * 2, 140 * 2).unwrap();
        let ok = desc(568, 280, 1);
        assert_eq!(ok.validate(bb, Some((2560, 1600))), Ok(()));
        type Case = (&'static str, Box<dyn Fn(&mut HudBufferHeader)>);
        let cases: Vec<Case> = vec![
            ("format", Box::new(|d| d.format = 2)),
            ("zero width", Box::new(|d| d.width = 0)),
            ("too wide", Box::new(|d| d.width = 4096)),
            ("stride < width*4", Box::new(|d| d.stride = d.width * 4 - 4)),
            ("stride % 4", Box::new(|d| d.stride += 2)),
            ("too big for buffer", Box::new(|d| d.height = 2048)),
            ("nan scale", Box::new(|d| d.scale = f32::NAN)),
            ("opacity", Box::new(|d| d.opacity = 1.5)),
            ("inf opacity", Box::new(|d| d.opacity = f32::INFINITY)),
            ("off right", Box::new(|d| d.x = 2560 - 100)),
            ("negative", Box::new(|d| d.y = -1)),
            ("extent", Box::new(|d| d.extent_w = 1920)),
            ("ngraphs", Box::new(|d| d.ngraphs = 3)),
            ("graph outside", Box::new(|d| d.graph[0].x = 560)),
            ("graph zero", Box::new(|d| d.graph[0].h = 0)),
            ("span", Box::new(|d| d.graph[0].span_ms = 999)),
            ("range", Box::new(|d| d.graph[0].max_ms = f32::NAN)),
            ("range order", Box::new(|d| {
                d.graph[0].min_ms = 10.0;
                d.graph[0].max_ms = 5.0;
            })),
            ("series", Box::new(|d| d.graph[0].series = 0)),
            ("huge stride", Box::new(|d| d.stride = !3)),
        ];
        for (name, f) in cases {
            let mut d = ok;
            f(&mut d);
            assert!(d.validate(bb, Some((2560, 1600))).is_err(), "{name}");
        }
        // The second graph is only checked when ngraphs says it is in use.
        let mut d = ok;
        d.graph[1].x = -5;
        assert!(d.validate(bb, None).is_ok());
        d.ngraphs = 2;
        assert!(d.validate(bb, None).is_err());
    }

    #[test]
    fn header_checks() {
        let mem = HeapRegion::new(hud_file_bytes(8192));
        assert!(init_hud(&mem, 5000).is_err());
        assert!(init_hud(&mem, 8 << 20).is_err());
        assert!(matches!(init_hud(&mem, 16384), Err(Error::TooSmall { .. })));
        init_hud(&mem, 8192).unwrap();
        mem.u32_at(OFF_BUFFER_BYTES).store(1 << 30, Ordering::Relaxed);
        assert!(read_hud_header(&mem).is_err());
        mem.u32_at(OFF_BUFFER_BYTES).store(8192, Ordering::Relaxed);
        mem.u16_at(OFF_VERSION).store(2, Ordering::Relaxed);
        assert_eq!(read_hud_header(&mem), Err(Error::Version(2)));
    }

    #[test]
    fn colours() {
        assert_eq!(rgba(1, 2, 3, 4).to_le_bytes(), [1, 2, 3, 4]);
        assert_eq!(rgba_premul(255, 128, 0, 128), rgba(128, 64, 0, 128));
        let mut px = [255, 255, 255, 0, 200, 100, 50, 255];
        premultiply_in_place(&mut px);
        assert_eq!(px, [0, 0, 0, 0, 200, 100, 50, 255]);
        assert_eq!(buffer_bytes_for(568, 280), Some(638976));
        assert_eq!(buffer_bytes_for(2048, 2048), None);
    }

    /// A writer publishing as fast as it can, a reader polling: every image taken is one
    /// publish's pixels and descriptor, never a mix.
    #[test]
    fn concurrent_never_tears() {
        let bb = buffer_bytes_for(128, 64).unwrap();
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_hud(&mem, bb).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (mem, stop) = (mem.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut w = HudWriter::new(mem).unwrap();
                let mut rev = 0;
                while !stop.load(Ordering::Relaxed) {
                    rev += 1;
                    let d = desc(128, 64, rev);
                    w.publish(&d, &pixels(&d)).unwrap();
                }
                rev
            })
        };
        let mut r = HudReader::new(mem.clone()).unwrap();
        let mut dst = vec![0u8; bb as usize];
        let (mut images, mut torn, mut busy) = (0, 0, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            match r.poll((2560, 1600), &mut dst) {
                HudPoll::Image { desc: d, published } => {
                    // The buffer may already hold a later publish of the same parity: still whole.
                    assert!(d.model_rev >= published);
                    assert!(dst[..d.pixel_bytes()].iter().enumerate().all(|(i, &b)| b == (i as u64 ^ d.model_rev) as u8));
                    images += 1;
                }
                HudPoll::Torn => torn += 1,
                HudPoll::Busy => busy += 1,
                HudPoll::Unchanged => {}
                HudPoll::Invalid(e) => panic!("{e}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        let published = writer.join().unwrap();
        assert!(images > 0, "images {images} torn {torn} busy {busy} of {published}");
    }
}
