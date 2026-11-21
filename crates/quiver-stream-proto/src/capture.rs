//! The capture link between the Sight layer, inside the game, and `quiver-stream-host`
//! (design/REMOTE.md §2.1): the game's presented frames, already converted to the encoder's
//! input format, in GPU memory both sides share.
//!
//! The host listens on a Unix `SOCK_SEQPACKET` socket and names it to the game in
//! [`ENV_SOCKET`]. The layer connects from a thread of its own. Then:
//!
//! 1. the layer says [`ToHost::Hello`], and the host answers with [`ToLayer::Want`]: the chroma,
//!    bit depth, range and whether HDR may be sent;
//! 2. the layer makes a ring of [`SLOTS`] frames in one exportable Vulkan allocation (opaque
//!    fd), and one exportable timeline semaphore, and sends [`ToHost::Ring`] with both fds
//!    attached (`SCM_RIGHTS`, memory first). It sends a new ring (a new `generation`) whenever
//!    the swapchain, the format or the connection changes;
//! 3. per captured present, [`ToHost::Frame`]: the slot, the timeline value its conversion
//!    signals, the present time, and the HDR metadata;
//! 4. the host waits on that value on the GPU, reads the slot, and gives it back with
//!    [`ToLayer::Release`]. A slot the host holds is never written; a present that finds no
//!    free slot isn't captured.
//!
//! Each slot holds one picture with every plane stacked at one pitch (`pitch` bytes a row,
//! [`PixelFormat::rows`] rows): what NVENC reads from a CUDA pointer. Messages are little
//! endian, at most [`MAX_MSG`] bytes, and encode into a caller's buffer without allocating
//! (the layer sends them from the game's present).

use crate::{HdrMetadata, Micros};

/// The game's environment: the path of the host's capture socket. Without it the layer never
/// captures.
pub const ENV_SOCKET: &str = "QUIVER_STREAM_CAPTURE_SOCKET";

/// Bumped when a message changes shape; both sides refuse another version.
pub const VERSION: u16 = 1;

/// Frames in a ring.
pub const SLOTS: usize = 3;

/// The longest message.
pub const MAX_MSG: usize = 128;

const MAGIC: [u8; 2] = *b"QC";

/// The layout of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4:2:0, 8-bit: Y, then interleaved CbCr at half height.
    Nv12,
    /// 4:2:0, 10-bit in the top bits of 16.
    P010,
    /// 4:4:4, 8-bit: Y, Cb, Cr planes.
    Yuv444,
    /// 4:4:4, 10-bit in the top bits of 16.
    Yuv444P16,
}

impl PixelFormat {
    fn code(self) -> u8 {
        match self {
            PixelFormat::Nv12 => 1,
            PixelFormat::P010 => 2,
            PixelFormat::Yuv444 => 3,
            PixelFormat::Yuv444P16 => 4,
        }
    }

    fn from_code(c: u8) -> Option<PixelFormat> {
        Some(match c {
            1 => PixelFormat::Nv12,
            2 => PixelFormat::P010,
            3 => PixelFormat::Yuv444,
            4 => PixelFormat::Yuv444P16,
            _ => return None,
        })
    }

    /// The format for a chroma and bit depth.
    pub fn of(chroma444: bool, ten_bit: bool) -> PixelFormat {
        match (chroma444, ten_bit) {
            (false, false) => PixelFormat::Nv12,
            (false, true) => PixelFormat::P010,
            (true, false) => PixelFormat::Yuv444,
            (true, true) => PixelFormat::Yuv444P16,
        }
    }

    pub fn ten_bit(self) -> bool {
        matches!(self, PixelFormat::P010 | PixelFormat::Yuv444P16)
    }

    pub fn chroma444(self) -> bool {
        matches!(self, PixelFormat::Yuv444 | PixelFormat::Yuv444P16)
    }

    /// Bytes per sample.
    pub fn sample_bytes(self) -> u32 {
        if self.ten_bit() { 2 } else { 1 }
    }

    /// Rows of the stacked planes, for a picture `height` tall (even).
    pub fn rows(self, height: u32) -> u32 {
        if self.chroma444() { height * 3 } else { height + height.div_ceil(2) }
    }
}

/// What the host asks the layer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Want {
    pub chroma444: bool,
    /// 10-bit even for SDR.
    pub ten_bit: bool,
    /// HDR10 (PQ, BT.2020) may be sent when the game presents HDR10 or scRGB; otherwise the
    /// layer tone-maps to SDR.
    pub hdr: bool,
    pub full_range: bool,
}

/// A new ring. The message carries two fds: the memory (opaque fd) and the timeline semaphore
/// (opaque fd).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ring {
    /// Unique in the layer's process; frames and releases name it.
    pub generation: u32,
    pub slots: u8,
    pub format: PixelFormat,
    /// The picture's size (even).
    pub width: u32,
    pub height: u32,
    /// Bytes per row of every plane.
    pub pitch: u32,
    /// Slot `i` starts at `i × slot_bytes` in the allocation.
    pub slot_bytes: u64,
    /// The allocation's size, as `cuImportExternalMemory` needs it.
    pub alloc_bytes: u64,
    /// A dedicated allocation (`CUDA_EXTERNAL_MEMORY_DEDICATED`).
    pub dedicated: bool,
    /// HDR10: PQ and BT.2020. Otherwise SDR, BT.709.
    pub hdr: bool,
    pub full_range: bool,
}

/// A captured present.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub generation: u32,
    pub slot: u8,
    /// The timeline value the slot's conversion signals.
    pub value: u64,
    /// The swapchain's presents, counting from 1.
    pub seq: u64,
    /// When the game called `vkQueuePresentKHR`, on `CLOCK_MONOTONIC`.
    pub presented: Micros,
    /// The last `vkSetHdrMetadataEXT` on the swapchain, on an HDR ring.
    pub hdr: Option<HdrMetadata>,
}

/// From the layer.
#[derive(Debug, Clone, PartialEq)]
pub enum ToHost {
    Hello { pid: u32 },
    Ring(Ring),
    Frame(Frame),
}

/// From the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToLayer {
    Want(Want),
    Release { generation: u32, slot: u8 },
}

struct W<'a> {
    buf: &'a mut [u8; MAX_MSG],
    at: usize,
}

impl W<'_> {
    fn new(buf: &mut [u8; MAX_MSG], kind: u8) -> W<'_> {
        let mut w = W { buf, at: 0 };
        w.bytes(&MAGIC);
        w.bytes(&VERSION.to_le_bytes());
        w.u8(kind);
        w
    }
    fn bytes(&mut self, b: &[u8]) {
        self.buf[self.at..self.at + b.len()].copy_from_slice(b);
        self.at += b.len();
    }
    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
}

struct R<'a> {
    buf: &'a [u8],
    at: usize,
}

impl R<'_> {
    fn bytes(&mut self, n: usize) -> Option<&[u8]> {
        let b = self.buf.get(self.at..self.at + n)?;
        self.at += n;
        Some(b)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
}

/// The header, and the message's kind.
fn open(buf: &[u8]) -> Option<(u8, R<'_>)> {
    let mut r = R { buf, at: 0 };
    if r.bytes(2)? != MAGIC || r.u16()? != VERSION {
        return None;
    }
    let kind = r.u8()?;
    Some((kind, r))
}

fn flags(bits: &[bool]) -> u8 {
    bits.iter().enumerate().fold(0, |f, (i, &b)| f | ((b as u8) << i))
}

fn bit(f: u8, i: u8) -> bool {
    f & (1 << i) != 0
}

impl ToHost {
    /// Writes the message into `buf`; returns its length.
    pub fn encode(&self, buf: &mut [u8; MAX_MSG]) -> usize {
        match self {
            ToHost::Hello { pid } => {
                let mut w = W::new(buf, 1);
                w.u32(*pid);
                w.at
            }
            ToHost::Ring(r) => {
                let mut w = W::new(buf, 2);
                w.u32(r.generation);
                w.u8(r.slots);
                w.u8(r.format.code());
                w.u8(flags(&[r.dedicated, r.hdr, r.full_range]));
                w.u32(r.width);
                w.u32(r.height);
                w.u32(r.pitch);
                w.u64(r.slot_bytes);
                w.u64(r.alloc_bytes);
                w.at
            }
            ToHost::Frame(f) => {
                let mut w = W::new(buf, 3);
                w.u32(f.generation);
                w.u8(f.slot);
                w.u64(f.value);
                w.u64(f.seq);
                w.u64(f.presented);
                w.u8(f.hdr.is_some() as u8);
                if let Some(h) = &f.hdr {
                    for p in h.primaries.iter().chain([&h.white_point]) {
                        w.u16(p[0]);
                        w.u16(p[1]);
                    }
                    w.u32(h.max_luminance);
                    w.u32(h.min_luminance);
                    w.u16(h.max_cll);
                    w.u16(h.max_fall);
                }
                w.at
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Option<ToHost> {
        let (kind, mut r) = open(buf)?;
        Some(match kind {
            1 => ToHost::Hello { pid: r.u32()? },
            2 => {
                let generation = r.u32()?;
                let slots = r.u8()?;
                let format = PixelFormat::from_code(r.u8()?)?;
                let f = r.u8()?;
                let ring = Ring {
                    generation,
                    slots,
                    format,
                    dedicated: bit(f, 0),
                    hdr: bit(f, 1),
                    full_range: bit(f, 2),
                    width: r.u32()?,
                    height: r.u32()?,
                    pitch: r.u32()?,
                    slot_bytes: r.u64()?,
                    alloc_bytes: r.u64()?,
                };
                if ring.slots == 0 || ring.slots as usize > SLOTS || ring.width == 0 || ring.height == 0 || ring.pitch < ring.width * ring.format.sample_bytes() {
                    return None;
                }
                if (ring.pitch as u64) * ring.format.rows(ring.height) as u64 > ring.slot_bytes || ring.slot_bytes * ring.slots as u64 > ring.alloc_bytes {
                    return None;
                }
                ToHost::Ring(ring)
            }
            3 => {
                let (generation, slot, value, seq, presented) = (r.u32()?, r.u8()?, r.u64()?, r.u64()?, r.u64()?);
                let hdr = if r.u8()? != 0 {
                    let mut p = [[0u16; 2]; 4];
                    for q in p.iter_mut() {
                        *q = [r.u16()?, r.u16()?];
                    }
                    Some(HdrMetadata {
                        primaries: [p[0], p[1], p[2]],
                        white_point: p[3],
                        max_luminance: r.u32()?,
                        min_luminance: r.u32()?,
                        max_cll: r.u16()?,
                        max_fall: r.u16()?,
                    })
                } else {
                    None
                };
                if slot as usize >= SLOTS {
                    return None;
                }
                ToHost::Frame(Frame { generation, slot, value, seq, presented, hdr })
            }
            _ => return None,
        })
    }
}

impl ToLayer {
    pub fn encode(&self, buf: &mut [u8; MAX_MSG]) -> usize {
        match self {
            ToLayer::Want(w) => {
                let mut o = W::new(buf, 11);
                o.u8(flags(&[w.chroma444, w.ten_bit, w.hdr, w.full_range]));
                o.at
            }
            ToLayer::Release { generation, slot } => {
                let mut o = W::new(buf, 12);
                o.u32(*generation);
                o.u8(*slot);
                o.at
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Option<ToLayer> {
        let (kind, mut r) = open(buf)?;
        Some(match kind {
            11 => {
                let f = r.u8()?;
                ToLayer::Want(Want { chroma444: bit(f, 0), ten_bit: bit(f, 1), hdr: bit(f, 2), full_range: bit(f, 3) })
            }
            12 => {
                let (generation, slot) = (r.u32()?, r.u8()?);
                if slot as usize >= SLOTS {
                    return None;
                }
                ToLayer::Release { generation, slot }
            }
            _ => return None,
        })
    }
}

/// HDR metadata as `VkHdrMetadataEXT` gives it (xy chromaticities, nits) in the units of
/// [`HdrMetadata`]. Values out of range are clamped; a non-finite one reads as 0.
pub fn hdr_metadata(primaries_xy: [[f32; 2]; 4], max_luminance: f32, min_luminance: f32, max_cll: f32, max_fall: f32) -> HdrMetadata {
    let unit = |v: f32, scale: f32, max: f32| if v.is_finite() && v > 0.0 { (v * scale).round().min(max) } else { 0.0 };
    let xy = |p: [f32; 2]| [unit(p[0], 50_000.0, 50_000.0) as u16, unit(p[1], 50_000.0, 50_000.0) as u16];
    HdrMetadata {
        primaries: [xy(primaries_xy[0]), xy(primaries_xy[1]), xy(primaries_xy[2])],
        white_point: xy(primaries_xy[3]),
        max_luminance: unit(max_luminance, 10_000.0, u32::MAX as f32) as u32,
        min_luminance: unit(min_luminance, 10_000.0, u32::MAX as f32) as u32,
        max_cll: unit(max_cll, 1.0, u16::MAX as f32) as u16,
        max_fall: unit(max_fall, 1.0, u16::MAX as f32) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> Ring {
        Ring { generation: 7, slots: 3, format: PixelFormat::P010, width: 3840, height: 2160, pitch: 7680, slot_bytes: 7680 * 3240, alloc_bytes: 7680 * 3240 * 3, dedicated: true, hdr: true, full_range: false }
    }

    #[test]
    fn round_trips() {
        let mut buf = [0u8; MAX_MSG];
        let hdr = HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 4000, max_fall: 600 };
        for m in [
            ToHost::Hello { pid: 4242 },
            ToHost::Ring(ring()),
            ToHost::Frame(Frame { generation: 7, slot: 2, value: 99, seq: 1234, presented: 5_000_000, hdr: Some(hdr) }),
            ToHost::Frame(Frame { generation: 7, slot: 0, value: 100, seq: 1235, presented: 5_008_333, hdr: None }),
        ] {
            let n = m.encode(&mut buf);
            assert!(n <= MAX_MSG);
            assert_eq!(ToHost::decode(&buf[..n]), Some(m.clone()), "{m:?}");
            // Truncated: refused, never a panic.
            assert_eq!(ToHost::decode(&buf[..n - 1]), None);
        }
        for m in [ToLayer::Want(Want { chroma444: true, ten_bit: false, hdr: true, full_range: false }), ToLayer::Release { generation: 3, slot: 1 }] {
            let n = m.encode(&mut buf);
            assert_eq!(ToLayer::decode(&buf[..n]), Some(m));
        }
    }

    #[test]
    fn refuses_bad_messages() {
        let mut buf = [0u8; MAX_MSG];
        let n = ToHost::Hello { pid: 1 }.encode(&mut buf);
        let mut other = buf;
        other[2] = 9; // another version
        assert_eq!(ToHost::decode(&other[..n]), None);
        other = buf;
        other[0] = b'X';
        assert_eq!(ToHost::decode(&other[..n]), None);
        // A ring whose slots don't fit its allocation, or whose pitch is short.
        let mut r = ring();
        r.alloc_bytes -= 1;
        let n = ToHost::Ring(r).encode(&mut buf);
        assert_eq!(ToHost::decode(&buf[..n]), None);
        let mut r = ring();
        r.pitch = 3840;
        let n = ToHost::Ring(r).encode(&mut buf);
        assert_eq!(ToHost::decode(&buf[..n]), None);
        // A slot out of range.
        let n = ToLayer::Release { generation: 1, slot: 3 }.encode(&mut buf);
        assert_eq!(ToLayer::decode(&buf[..n]), None);
    }

    #[test]
    fn formats_and_metadata() {
        assert_eq!(PixelFormat::Nv12.rows(1440), 2160);
        assert_eq!(PixelFormat::Yuv444P16.rows(1080), 3240);
        assert_eq!(PixelFormat::of(false, true), PixelFormat::P010);
        assert_eq!(PixelFormat::of(true, false).sample_bytes(), 1);
        let m = hdr_metadata([[0.708, 0.292], [0.17, 0.797], [0.131, 0.046], [0.3127, 0.329]], 1000.0, 0.005, 4000.0, f32::NAN);
        assert_eq!(m.primaries[0], [35400, 14600]);
        assert_eq!(m.white_point, [15635, 16450]);
        assert_eq!((m.max_luminance, m.min_luminance, m.max_cll, m.max_fall), (10_000_000, 50, 4000, 0));
    }
}
