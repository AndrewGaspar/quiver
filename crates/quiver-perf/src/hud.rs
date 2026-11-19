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
const OFF_BACKDROP: usize = offset_of!(HudHeader, backdrop);
const OFF_BACKDROP_UNTIL: usize = offset_of!(HudHeader, backdrop_until);
const OFF_STEREO: usize = offset_of!(HudHeader, stereo);
const DESC_WORDS: usize = HUD_PIXELS_OFFSET / 8;

/// The layer stops drawing an image whose writer heartbeat is older than this (§2.8).
pub const HUD_STALE_NS: i64 = 3_000_000_000;

/// Size limits of an image file: `hud.image` ([`HUD_LIMITS`]) or the Sight panel's
/// `panel.image` ([`PANEL_LIMITS`]), the same format with room for a full-screen image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLimits {
    /// Most pixels a side.
    pub max_dim: u32,
    /// Most bytes per buffer (descriptor included).
    pub max_buffer_bytes: u32,
}

pub const HUD_LIMITS: ImageLimits = ImageLimits { max_dim: HUD_MAX_DIM, max_buffer_bytes: HUD_MAX_BUFFER_BYTES };
pub const PANEL_LIMITS: ImageLimits = ImageLimits { max_dim: PANEL_MAX_DIM, max_buffer_bytes: PANEL_MAX_BUFFER_BYTES };

/// Bytes of a `hud.image` file with buffers of `buffer_bytes`.
pub const fn hud_file_bytes(buffer_bytes: u32) -> usize {
    HUD_HEADER_BYTES + 2 * buffer_bytes as usize
}

/// The smallest valid buffer size (a multiple of 4096) for a `width × height` image with
/// tightly packed rows; `None` above the 4 MiB cap.
pub fn buffer_bytes_for(width: u32, height: u32) -> Option<u32> {
    buffer_bytes_within(width, height, &HUD_LIMITS)
}

/// [`buffer_bytes_for`] under other limits (`panel.image`).
pub fn buffer_bytes_within(width: u32, height: u32, limits: &ImageLimits) -> Option<u32> {
    let need = HUD_PIXELS_OFFSET as u64 + width as u64 * 4 * height as u64;
    let rounded = need.div_ceil(4096) * 4096;
    (rounded <= limits.max_buffer_bytes as u64).then_some(rounded as u32)
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
        stereo: 0,
        model_rev: 0,
        dirty_y0: 0,
        dirty_y1: 0,
        dirty_base: 0,
        depth: 0,
        nlifts: 0,
        lifts: [HudLift::ZERO; HUD_MAX_LIFTS],
        _reserved: [0; HUD_PIXELS_OFFSET - 252],
    };

    /// The graphs in use (`ngraphs` clamped to 2).
    pub fn graphs(&self) -> &[HudGraph] {
        &self.graph[..(self.ngraphs as usize).min(HUD_MAX_GRAPHS)]
    }

    /// The lifts in use (`nlifts` clamped to [`HUD_MAX_LIFTS`]).
    pub fn lifts(&self) -> &[HudLift] {
        &self.lifts[..(self.nlifts as usize).min(HUD_MAX_LIFTS)]
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
        self.validate_within(&HUD_LIMITS, buffer_bytes, swapchain_extent)
    }

    /// [`HudBufferHeader::validate`] under other limits (`panel.image`).
    pub fn validate_within(&self, limits: &ImageLimits, buffer_bytes: u32, swapchain_extent: Option<(u32, u32)>) -> Result<(), Error> {
        let inv = Error::Invalid;
        if self.format != HUD_FORMAT_RGBA8_PREMUL_SRGB {
            return Err(inv("format"));
        }
        if self.width == 0 || self.height == 0 || self.width > limits.max_dim || self.height > limits.max_dim {
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
        // A stereo image is laid out in one eye's logical space (crate::stereo).
        let Some(stereo) = crate::stereo::Stereo::from_code(self.stereo) else {
            return Err(inv("stereo layout"));
        };
        let extent = (self.extent_w, self.extent_h);
        if stereo.is_stereo() && !stereo.fits(extent) {
            return Err(inv("stereo layout on a frame too small to split"));
        }
        let (cw, ch) = stereo.logical_extent(extent);
        if !inside(self.x, self.y, self.width as i64, self.height as i64, cw, ch) {
            return Err(inv("image outside the swapchain"));
        }
        if self.ngraphs as usize > HUD_MAX_GRAPHS {
            return Err(inv("ngraphs"));
        }
        if self.depth > crate::stereo::DEPTH_ONE {
            return Err(inv("depth"));
        }
        if self.nlifts as usize > HUD_MAX_LIFTS {
            return Err(inv("nlifts"));
        }
        for l in self.lifts() {
            if l.w == 0 || l.h == 0 || !inside(l.x as i32, l.y as i32, l.w as i64, l.h as i64, self.width, self.height) {
                return Err(inv("lift outside the image"));
            }
            if l.depth > crate::stereo::DEPTH_ONE {
                return Err(inv("lift depth"));
            }
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

    /// The rows that changed since publish `have` (the image a reader's texture holds), when
    /// the descriptor promises it ([`HudBufferHeader::dirty_base`]) and the rows are in the
    /// image; `None`: take the whole image.
    pub fn dirty_rows(&self, have: u64) -> Option<(u32, u32)> {
        (have != 0 && self.dirty_base == have && self.dirty_y0 < self.dirty_y1 && self.dirty_y1 <= self.height).then_some((self.dirty_y0, self.dirty_y1))
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

/// The Sight panel's backdrop request in the `hud.image` header (`qv_hud_header.backdrop`,
/// `backdrop_dim`, `backdrop_radius`, `backdrop_until`; SIGHT §14): quiver-sight asks the layer
/// in this session's game to blur its frames while the panel is open over it.
///
/// The first three fields share one 64-bit word (offset 128), stored and loaded as one, and
/// the deadline is the next word. Writer: deadline, then the word (release). Reader: the word
/// (acquire), then the deadline. A request is live while its mode is
/// [`HUD_BACKDROP_BLUR`] and the deadline is ahead (at most [`HUD_BACKDROP_MAX_HOLD_NS`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackdropRequest {
    /// `HUD_BACKDROP_*`.
    pub mode: u32,
    /// Darkening, 1/1000.
    pub dim: u16,
    /// Blur radius, 1/1000 of the swapchain height; 0: [`HUD_BACKDROP_DEFAULT_RADIUS`].
    pub radius: u16,
    /// CLOCK_MONOTONIC_RAW ns.
    pub until: u64,
    /// How the game's frames pack their eyes (`qv_hud_header.stereo`, `crate::stereo`): the
    /// blur stays inside each eye. Written before the word, read after it.
    pub stereo: u32,
}

impl BackdropRequest {
    pub const NONE: BackdropRequest = BackdropRequest { mode: HUD_BACKDROP_NONE, dim: 0, radius: 0, until: 0, stereo: 0 };

    /// A blur until `until_ns`, dim and radius in 1/1000 (clamped to 1000).
    pub fn blur(dim: u16, radius: u16, until_ns: i64) -> BackdropRequest {
        BackdropRequest { mode: HUD_BACKDROP_BLUR, dim: dim.min(1000), radius: radius.min(1000), until: until_ns.max(0) as u64, stereo: 0 }
    }

    /// The same request for frames packed as `s`.
    pub fn with_stereo(self, s: crate::stereo::Stereo) -> BackdropRequest {
        BackdropRequest { stereo: s.code(), ..self }
    }

    /// The layout the blur keeps its samples inside (mono for an unknown code).
    pub fn stereo(&self) -> crate::stereo::Stereo {
        crate::stereo::Stereo::from_code(self.stereo).unwrap_or_default()
    }

    /// The request asks for a blur at `now_ns`.
    pub fn is_live(&self, now_ns: i64) -> bool {
        let now = now_ns.max(0) as u64;
        self.mode == HUD_BACKDROP_BLUR && self.until > now
    }

    /// Darkening as a fraction, 0..=1.
    pub fn dim_fraction(&self) -> f32 {
        self.dim.min(1000) as f32 / 1000.0
    }

    /// Blur radius as a fraction of the swapchain height.
    pub fn radius_fraction(&self) -> f32 {
        let r = if self.radius == 0 { HUD_BACKDROP_DEFAULT_RADIUS } else { self.radius.min(1000) as u32 };
        r as f32 / 1000.0
    }

    fn word(&self) -> u64 {
        self.mode as u64 | (self.dim as u64) << 32 | (self.radius as u64) << 48
    }

    fn from_words(word: u64, until: u64, stereo: u32) -> BackdropRequest {
        BackdropRequest { mode: word as u32, dim: (word >> 32) as u16, radius: (word >> 48) as u16, until, stereo }
    }
}

/// Writes a backdrop request into a `hud.image` header (quiver-sight).
pub fn set_backdrop<R: ShmRegion>(region: &R, b: &BackdropRequest) {
    if b.mode == HUD_BACKDROP_NONE {
        region.u64_at(OFF_BACKDROP).store(0, Ordering::Release);
        region.u64_at(OFF_BACKDROP_UNTIL).store(0, Ordering::Relaxed);
    } else {
        region.u32_at(OFF_STEREO).store(b.stereo, Ordering::Relaxed);
        region.u64_at(OFF_BACKDROP_UNTIL).store(b.until, Ordering::Relaxed);
        region.u64_at(OFF_BACKDROP).store(b.word(), Ordering::Release);
    }
}

/// Reads the backdrop request (untrusted: check [`BackdropRequest::is_live`]; the deadline
/// is clamped to [`HUD_BACKDROP_MAX_HOLD_NS`] from `now_ns`).
pub fn read_backdrop<R: ShmRegion>(region: &R, now_ns: i64) -> BackdropRequest {
    let word = region.u64_at(OFF_BACKDROP).load(Ordering::Acquire);
    if word as u32 == HUD_BACKDROP_NONE {
        return BackdropRequest::NONE;
    }
    let until = region.u64_at(OFF_BACKDROP_UNTIL).load(Ordering::Acquire);
    let stereo = region.u32_at(OFF_STEREO).load(Ordering::Relaxed);
    let cap = (now_ns.max(0) as u64).saturating_add(HUD_BACKDROP_MAX_HOLD_NS);
    BackdropRequest::from_words(word, until.min(cap), stereo)
}

/// A validated `hud.image` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudInfo {
    pub buffer_bytes: u32,
    pub limits: ImageLimits,
}

/// Initializes `region` as an empty `hud.image` (quiverd, at session start); magic last.
pub fn init_hud<R: ShmRegion>(region: &R, buffer_bytes: u32) -> Result<HudInfo, Error> {
    init_image(region, buffer_bytes, &HUD_LIMITS)
}

/// [`init_hud`] under other limits (`panel.image`).
pub fn init_image<R: ShmRegion>(region: &R, buffer_bytes: u32, limits: &ImageLimits) -> Result<HudInfo, Error> {
    if !region.writable() {
        return Err(Error::ReadOnly);
    }
    if buffer_bytes < 4096 || !buffer_bytes.is_multiple_of(4096) || buffer_bytes > limits.max_buffer_bytes {
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
    Ok(HudInfo { buffer_bytes, limits: *limits })
}

/// Validates a `hud.image` header in (untrusted) memory.
pub fn read_hud_header<R: ShmRegion>(region: &R) -> Result<HudInfo, Error> {
    read_image_header(region, &HUD_LIMITS)
}

/// [`read_hud_header`] under other limits (`panel.image`).
pub fn read_image_header<R: ShmRegion>(region: &R, limits: &ImageLimits) -> Result<HudInfo, Error> {
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
    if buffer_bytes < 4096 || !buffer_bytes.is_multiple_of(4096) || buffer_bytes > limits.max_buffer_bytes {
        return Err(Error::Layout("buffer_bytes"));
    }
    let need = hud_file_bytes(buffer_bytes);
    if region.len() < need {
        return Err(Error::TooSmall { need, have: region.len() });
    }
    Ok(HudInfo { buffer_bytes, limits: *limits })
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
        Self::with_limits(region, &HUD_LIMITS)
    }

    /// Attaches to an image file of other limits (`panel.image`).
    pub fn with_limits(region: R, limits: &ImageLimits) -> Result<Self, Error> {
        if !region.writable() {
            return Err(Error::ReadOnly);
        }
        let info = read_image_header(&region, limits)?;
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

    /// The Sight panel's backdrop request ([`BackdropRequest`]); independent of publishing.
    pub fn set_backdrop(&self, b: &BackdropRequest) {
        set_backdrop(&self.region, b);
    }

    /// Writes `desc` (its `seq` is ignored) and `pixels` (premultiplied RGBA8, `stride ×
    /// height` bytes) into the back buffer and publishes it. Returns the new publish count.
    pub fn publish(&mut self, desc: &HudBufferHeader, pixels: &[u8]) -> Result<u64, Error> {
        desc.validate_within(&self.info.limits, self.info.buffer_bytes, None)?;
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
        Self::with_limits(region, &HUD_LIMITS)
    }

    /// Reads an image file of other limits (`panel.image`).
    pub fn with_limits(region: R, limits: &ImageLimits) -> Result<Self, Error> {
        let info = read_image_header(&region, limits)?;
        Ok(HudReader { region, info, last: 0 })
    }

    pub fn info(&self) -> &HudInfo {
        &self.info
    }

    /// The mapping, for checks like [`crate::shm::Mapping::is_file_at`] (the layer re-opens
    /// `hud.image` when quiverd replaces it).
    pub fn region(&self) -> &R {
        &self.region
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

    /// The Sight panel's backdrop request (two loads; the layer's per-present check).
    #[inline]
    pub fn backdrop(&self, now_ns: i64) -> BackdropRequest {
        read_backdrop(&self.region, now_ns)
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
        self.poll_since(swapchain_extent, dst, 0).0
    }

    /// [`HudReader::poll`] for a reader whose texture holds publish `have` (0: nothing): when
    /// the new image says only some rows changed since then ([`HudBufferHeader::dirty_rows`]),
    /// only those rows are copied into `dst` (at their place in the image's layout; the rest
    /// of `dst` is left alone) and returned.
    pub fn poll_since(&mut self, swapchain_extent: (u32, u32), dst: &mut [u8], have: u64) -> (HudPoll, Option<(u32, u32)>) {
        let p = self.published();
        if p == self.last {
            return (HudPoll::Unchanged, None);
        }
        let base = buffer_off(&self.info, p);
        let seq = self.region.u64_at(base);
        let s1 = seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return (HudPoll::Busy, None);
        }
        let mut w = [0u64; DESC_WORDS];
        for (k, word) in w.iter_mut().enumerate().skip(1) {
            *word = self.region.u64_at(base + 8 * k).load(Ordering::Relaxed);
        }
        let desc = HudBufferHeader::from_words(w);
        // Check that the descriptor is one writer's before trusting its sizes.
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) != s1 {
            return (HudPoll::Torn, None);
        }
        if let Err(e) = desc.validate_within(&self.info.limits, self.info.buffer_bytes, Some(swapchain_extent)) {
            self.last = p;
            return (HudPoll::Invalid(e), None);
        }
        let len = desc.pixel_bytes();
        if dst.len() < len {
            self.last = p;
            return (HudPoll::Invalid(Error::TooSmall { need: len, have: dst.len() }), None);
        }
        let px = base + HUD_PIXELS_OFFSET;
        let rows = desc.dirty_rows(have);
        // Byte ranges of whole rows; the first and last word may hold a neighbouring row's
        // bytes too (copied as well: the same image).
        let (from, to) = match rows {
            Some((y0, y1)) => (y0 as usize * desc.stride as usize, y1 as usize * desc.stride as usize),
            None => (0, len),
        };
        for k in from / 8..to.div_ceil(8) {
            let b = self.region.u64_at(px + 8 * k).load(Ordering::Relaxed).to_le_bytes();
            let at = 8 * k;
            let n = (len - at).min(8);
            dst[at..at + n].copy_from_slice(&b[..n]);
        }
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) != s1 {
            return (HudPoll::Torn, None);
        }
        self.last = p;
        (HudPoll::Image { published: p, desc }, rows)
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
    fn backdrop_request() {
        let bb = buffer_bytes_for(64, 64).unwrap();
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_hud(&mem, bb).unwrap();
        let w = HudWriter::new(mem.clone()).unwrap();
        let r = HudReader::new(mem.clone()).unwrap();
        // A fresh file (or a writer from before): no request.
        assert_eq!(r.backdrop(100), BackdropRequest::NONE);
        assert!(!r.backdrop(100).is_live(100));
        let req = BackdropRequest::blur(120, 0, 2_000);
        w.set_backdrop(&req);
        let got = r.backdrop(1_000);
        assert_eq!(got, req);
        assert!(got.is_live(1_000) && !got.is_live(2_000));
        assert!((got.dim_fraction() - 0.12).abs() < 1e-6);
        assert!((got.radius_fraction() - HUD_BACKDROP_DEFAULT_RADIUS as f32 / 1000.0).abs() < 1e-6);
        assert_eq!(BackdropRequest::blur(5000, 40, 1).dim, 1000);
        // The header words, as C sees them.
        assert_eq!(mem.u32_at(OFF_BACKDROP).load(Ordering::Relaxed), HUD_BACKDROP_BLUR);
        assert_eq!(mem.u16_at(offset_of!(HudHeader, backdrop_dim)).load(Ordering::Relaxed), 120);
        // A deadline too far ahead is cut to the maximum hold.
        w.set_backdrop(&BackdropRequest::blur(0, 30, i64::MAX));
        let got = r.backdrop(5);
        assert_eq!(got.until, 5 + HUD_BACKDROP_MAX_HOLD_NS);
        assert!((got.radius_fraction() - 0.03).abs() < 1e-6);
        // An unknown mode reads back but isn't live.
        mem.u32_at(OFF_BACKDROP).store(7, Ordering::Relaxed);
        assert!(!r.backdrop(5).is_live(5));
        w.set_backdrop(&BackdropRequest::NONE);
        assert_eq!(r.backdrop(5), BackdropRequest::NONE);
        // Publishing doesn't touch it, and it doesn't touch the publish counter.
        w.set_backdrop(&req);
        let mut w = w;
        let d = desc(64, 64, 1);
        w.publish(&d, &pixels(&d)).unwrap();
        assert_eq!((r.backdrop(1_000), r.published()), (req, 1));
    }

    #[test]
    fn stereo_images_and_requests() {
        use crate::stereo::Stereo;
        // The blur's layout travels with the request.
        let bb = buffer_bytes_for(64, 64).unwrap();
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_hud(&mem, bb).unwrap();
        let w = HudWriter::new(mem.clone()).unwrap();
        let r = HudReader::new(mem.clone()).unwrap();
        w.set_backdrop(&BackdropRequest::blur(0, 0, 2_000).with_stereo(Stereo::Hsbs));
        assert_eq!(r.backdrop(1_000).stereo(), Stereo::Hsbs);
        assert_eq!(mem.u32_at(OFF_STEREO).load(Ordering::Relaxed), crate::stereo::STEREO_HSBS);
        mem.u32_at(OFF_STEREO).store(77, Ordering::Relaxed);
        assert_eq!(r.backdrop(1_000).stereo(), Stereo::Mono, "an unknown code blurs as mono");
        // A stereo image is placed in one eye's logical space: sbs on 3840x1080 is 1920 wide.
        let bb = buffer_bytes_for(200, 100).unwrap();
        let mut d = desc(200, 100, 1);
        (d.extent_w, d.extent_h, d.stereo) = (3840, 1080, Stereo::Sbs.code());
        d.x = 1700;
        assert!(d.validate(bb, Some((3840, 1080))).is_ok());
        d.x = 1800;
        assert_eq!(d.validate(bb, Some((3840, 1080))), Err(Error::Invalid("image outside the swapchain")), "past the eye");
        // hsbs keeps the window's width as the logical width.
        (d.extent_w, d.stereo, d.x) = (1920, Stereo::Hsbs.code(), 1700);
        assert!(d.validate(bb, Some((1920, 1080))).is_ok());
        d.stereo = 9;
        assert_eq!(d.validate(bb, Some((1920, 1080))), Err(Error::Invalid("stereo layout")));
        (d.stereo, d.extent_w, d.x) = (Stereo::Sbs.code(), 3, 0);
        assert!(d.validate(bb, Some((3, 1080))).is_err());
    }

    #[test]
    fn depth_and_lifts() {
        let bb = buffer_bytes_for(200, 100).unwrap();
        let mut d = desc(200, 100, 1);
        (d.extent_w, d.extent_h, d.stereo) = (3840, 1080, crate::stereo::STEREO_SBS);
        assert_eq!((d.depth, d.lifts()), (0, &[][..]), "zero from writers before depth");
        d.depth = 450;
        d.nlifts = 2;
        d.lifts[0] = HudLift { x: 10, y: 10, w: 180, h: 80, depth: 450, _pad: 0 };
        d.lifts[1] = HudLift { x: 20, y: 20, w: 100, h: 30, depth: 750, _pad: 0 };
        assert!(d.validate(bb, Some((3840, 1080))).is_ok());
        assert_eq!(d.lifts().len(), 2);
        assert!(d.lifts[1].contains(20.0, 49.5) && !d.lifts[1].contains(120.0, 30.0));
        // It travels through the buffer.
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_hud(&mem, bb).unwrap();
        let mut w = HudWriter::new(mem.clone()).unwrap();
        let mut r = HudReader::new(mem.clone()).unwrap();
        w.publish(&d, &pixels(&d)).unwrap();
        let mut dst = vec![0u8; d.pixel_bytes()];
        let HudPoll::Image { desc: got, .. } = r.poll((3840, 1080), &mut dst) else { panic!("no image") };
        assert_eq!((got.depth, got.lifts()), (450, d.lifts()));
        // Bounds.
        let bad = |f: &dyn Fn(&mut HudBufferHeader)| {
            let mut e = d;
            f(&mut e);
            e.validate(bb, Some((3840, 1080)))
        };
        assert_eq!(bad(&|e| e.depth = 1001), Err(Error::Invalid("depth")));
        assert_eq!(bad(&|e| e.nlifts = 5), Err(Error::Invalid("nlifts")));
        assert_eq!(bad(&|e| e.lifts[1].w = 190), Err(Error::Invalid("lift outside the image")));
        assert_eq!(bad(&|e| e.lifts[0].h = 0), Err(Error::Invalid("lift outside the image")));
        assert_eq!(bad(&|e| e.lifts[1].depth = 2000), Err(Error::Invalid("lift depth")));
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
        let seq = mem.u64_at(buffer_off(&HudInfo { buffer_bytes: bb, limits: HUD_LIMITS }, 3));
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

    /// Hostile descriptors (random fields, plausible-ish ranges so some pass): the reader never
    /// panics or reads out of bounds, and whatever validates fits the buffer and the
    /// swapchain.
    #[test]
    fn random_descriptors() {
        let bb = buffer_bytes_for(256, 256).unwrap();
        let mem = Arc::new(HeapRegion::new(hud_file_bytes(bb)));
        init_hud(&mem, bb).unwrap();
        let mut r = HudReader::new(mem.clone()).unwrap();
        let mut dst = vec![0u8; bb as usize];
        // Near the boundaries half the time, anything the other half.
        let any = |n: u32| if fastrand::bool() { fastrand::u32(..) } else { fastrand::u32(0..n) };
        let float = || [1.0, 0.0, -1.0, f32::NAN, f32::INFINITY, 0.5, 2.5, 100.0, -0.0][fastrand::usize(..9)];
        let (mut ok, mut images) = (0, 0);
        for i in 1..10_000u64 {
            // A valid descriptor with 0–3 fields replaced by hostile values.
            let mut d = desc(fastrand::u32(17..=256), fastrand::u32(8..=240), i);
            for _ in 0..fastrand::usize(0..4) {
                let g = fastrand::usize(..2);
                match fastrand::u8(0..19) {
                    0 => d.width = any(300),
                    1 => d.height = any(300),
                    2 => d.stride = any(1300),
                    3 => d.format = any(3),
                    4 => d.scale = float(),
                    5 => d.opacity = float(),
                    6 => d.extent_w = any(4000),
                    7 => d.extent_h = any(4000),
                    8 => d.x = any(4000) as i32,
                    9 => d.y = any(4000) as i32,
                    10 => d.ngraphs = any(4),
                    11 => d.graph[g].x = any(300) as i32,
                    12 => d.graph[g].y = any(300) as i32,
                    13 => d.graph[g].w = any(300) as i32,
                    14 => d.graph[g].h = any(300) as i32,
                    15 => d.graph[g].span_ms = any(12000),
                    16 => d.graph[g].min_ms = float(),
                    17 => d.graph[g].max_ms = float(),
                    _ => d.graph[g].series = any(9),
                }
            }
            let valid = d.validate(bb, Some((d.extent_w, d.extent_h))).is_ok();
            if valid {
                ok += 1;
                assert!(d.pixel_bytes() + HUD_PIXELS_OFFSET <= bb as usize);
                assert!(d.x as u64 + d.width as u64 <= d.extent_w as u64);
                for g in d.graphs() {
                    assert!(g.x >= 0 && g.y >= 0 && (g.x + g.w) as u32 <= d.width && (g.y + g.h) as u32 <= d.height);
                }
            }
            // Write the raw descriptor as a hostile writer would (bypassing publish's checks).
            let base = buffer_off(&r.info, i);
            for (k, w) in d.to_words().iter().enumerate() {
                mem.u64_at(base + 8 * k).store(*w, Ordering::Relaxed);
            }
            mem.u64_at(OFF_PUBLISHED).store(i, Ordering::Release);
            match r.poll((d.extent_w, d.extent_h), &mut dst) {
                HudPoll::Image { .. } => {
                    assert!(valid);
                    images += 1;
                }
                HudPoll::Invalid(_) => assert!(!valid),
                other => panic!("{other:?}"),
            }
        }
        assert!(ok > 100 && images == ok, "{ok} valid, {images} taken");
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
