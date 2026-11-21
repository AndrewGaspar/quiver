//! The latency test pattern: a frame's sequence number and capture time, drawn into the
//! frame as black and white blocks, so the client can read them back from what it decoded
//! (or presented) and time each stage without trusting any side channel.
//!
//! The host's test-pattern target draws it with [`draw_luma8`] (on a Y plane) or
//! [`draw_rgba`] (on a 4-byte-per-pixel capture, before encoding); the client reads it with
//! [`read_luma8`] or [`read_luma16`].
//!
//! Layout. The block size `b` is [`block_size`]: 1/64 of the frame's width (or of the width
//! a 16:9 frame of that height would have, if narrower: an SBS frame's left eye), rounded
//! down to a multiple of 8 so blocks line up with macroblocks, and at least 8. The pattern
//! sits in the top-left corner: a black quiet zone `(COLUMNS + 2) × (ROWS + 2)` blocks in
//! size, with the [`BITS`] data blocks inside it, one block in from the edge, row by row, left
//! to right. A white block is a 1, a black block a 0. The bits are, most significant first:
//! [`SYNC`] (8 bits), `seq` (32), `captured` (64) and a CRC-16/CCITT-FALSE of the 12 bytes of
//! `seq` and `captured`, big-endian (16).

use crate::Micros;

/// Data blocks per row.
pub const COLUMNS: u32 = 40;
/// Rows of data blocks.
pub const ROWS: u32 = 3;
/// Bits in the pattern: `COLUMNS × ROWS`.
pub const BITS: usize = (COLUMNS * ROWS) as usize;
/// The first byte of every pattern, so a frame without one isn't misread.
pub const SYNC: u8 = 0b1011_0010;

/// What a pattern carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// The frame's `seq`, truncated to 32 bits (the reader knows roughly where it is).
    pub seq: u32,
    /// When the host captured the frame, on the host's clock.
    pub captured: Micros,
}

/// The block size, in pixels, for a frame of this size.
pub fn block_size(width: u32, height: u32) -> u32 {
    let w = width.min(height.saturating_mul(16) / 9);
    ((w / 64) & !7).max(8)
}

/// The size of the area the pattern covers (quiet zone included), from the top-left corner.
pub fn region(width: u32, height: u32) -> (u32, u32) {
    let b = block_size(width, height);
    ((COLUMNS + 2) * b, (ROWS + 2) * b)
}

/// The top-left corner of data block `i`.
pub fn block_origin(width: u32, height: u32, i: usize) -> (u32, u32) {
    let b = block_size(width, height);
    let i = i as u32;
    ((1 + i % COLUMNS) * b, (1 + i / COLUMNS) * b)
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF).
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// The pattern's bits for a stamp.
pub fn encode_bits(stamp: Stamp) -> [bool; BITS] {
    let mut bytes = [0u8; 15];
    bytes[0] = SYNC;
    bytes[1..5].copy_from_slice(&stamp.seq.to_be_bytes());
    bytes[5..13].copy_from_slice(&stamp.captured.to_be_bytes());
    let crc = crc16(&bytes[1..13]);
    bytes[13..15].copy_from_slice(&crc.to_be_bytes());
    let mut bits = [false; BITS];
    for (i, bit) in bits.iter_mut().enumerate() {
        *bit = bytes[i / 8] & (0x80 >> (i % 8)) != 0;
    }
    bits
}

/// The stamp in a pattern's bits, if the sync byte and the CRC check out.
pub fn decode_bits(bits: &[bool; BITS]) -> Option<Stamp> {
    let mut bytes = [0u8; 15];
    for (i, &bit) in bits.iter().enumerate() {
        if bit {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    if bytes[0] != SYNC {
        return None;
    }
    let crc = u16::from_be_bytes([bytes[13], bytes[14]]);
    if crc != crc16(&bytes[1..13]) {
        return None;
    }
    Some(Stamp {
        seq: u32::from_be_bytes(bytes[1..5].try_into().unwrap()),
        captured: u64::from_be_bytes(bytes[5..13].try_into().unwrap()),
    })
}

/// Draw the pattern with `put(x, y, white)` for every pixel of its region.
pub fn draw_with(width: u32, height: u32, stamp: Stamp, mut put: impl FnMut(u32, u32, bool)) {
    let b = block_size(width, height);
    let (rw, rh) = region(width, height);
    let bits = encode_bits(stamp);
    for y in 0..rh.min(height) {
        for x in 0..rw.min(width) {
            let (bx, by) = (x / b, y / b);
            let white = (1..=COLUMNS).contains(&bx)
                && (1..=ROWS).contains(&by)
                && bits[((by - 1) * COLUMNS + (bx - 1)) as usize];
            put(x, y, white);
        }
    }
}

/// Draw the pattern on an 8-bit luma plane. Limited range uses 16 and 235, full range 0
/// and 255.
pub fn draw_luma8(plane: &mut [u8], stride: usize, width: u32, height: u32, full_range: bool, stamp: Stamp) {
    let (lo, hi) = if full_range { (0, 255) } else { (16, 235) };
    draw_with(width, height, stamp, |x, y, white| {
        if let Some(p) = plane.get_mut(y as usize * stride + x as usize) {
            *p = if white { hi } else { lo };
        }
    });
}

/// Draw the pattern on 4-byte pixels (RGBA, BGRA or BGRX alike): black or white, alpha
/// opaque. `stride` is in bytes.
pub fn draw_rgba(buf: &mut [u8], stride: usize, width: u32, height: u32, stamp: Stamp) {
    draw_with(width, height, stamp, |x, y, white| {
        let o = y as usize * stride + x as usize * 4;
        if let Some(p) = buf.get_mut(o..o + 4) {
            let v = if white { 255 } else { 0 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
    });
}

/// Read the pattern with `luma(x, y)`, a pixel's luma scaled to 0–255. Each block is judged
/// by the mean of its inner half, against the midpoint, so compression noise and soft edges
/// don't flip a bit.
pub fn read_with(width: u32, height: u32, luma: impl Fn(u32, u32) -> u8) -> Option<Stamp> {
    let b = block_size(width, height);
    let (rw, rh) = region(width, height);
    if rw > width || rh > height {
        return None;
    }
    let mut bits = [false; BITS];
    for (i, bit) in bits.iter_mut().enumerate() {
        let (x0, y0) = block_origin(width, height, i);
        let (mut sum, mut n) = (0u32, 0u32);
        for y in y0 + b / 4..y0 + b - b / 4 {
            for x in x0 + b / 4..x0 + b - b / 4 {
                sum += luma(x, y) as u32;
                n += 1;
            }
        }
        *bit = sum / n.max(1) >= 126;
    }
    decode_bits(&bits)
}

/// Read the pattern from an 8-bit luma plane (limited or full range).
pub fn read_luma8(plane: &[u8], stride: usize, width: u32, height: u32) -> Option<Stamp> {
    read_with(width, height, |x, y| plane.get(y as usize * stride + x as usize).copied().unwrap_or(0))
}

/// Read the pattern from a 16-bit luma plane holding `depth`-bit values shifted left by
/// `shift` (P010: depth 10, shift 6; yuv420p10: depth 10, shift 0). `stride` is in samples.
pub fn read_luma16(plane: &[u16], stride: usize, width: u32, height: u32, depth: u32, shift: u32) -> Option<Stamp> {
    let down = (depth + shift).saturating_sub(8);
    read_with(width, height, |x, y| {
        let v = plane.get(y as usize * stride + x as usize).copied().unwrap_or(0) as u32;
        (v >> down).min(255) as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: Stamp = Stamp { seq: 0xDEAD_BEEF, captured: 0x0123_4567_89AB_CDEF };

    #[test]
    fn bits_round_trip_and_reject_damage() {
        let bits = encode_bits(STAMP);
        assert_eq!(decode_bits(&bits), Some(STAMP));
        for i in [0, 9, 50, 110, 119] {
            let mut b = bits;
            b[i] = !b[i];
            assert_eq!(decode_bits(&b), None, "bit {i}");
        }
        assert_eq!(decode_bits(&[false; BITS]), None);
    }

    #[test]
    fn block_sizes() {
        assert_eq!(block_size(640, 360), 8);
        assert_eq!(block_size(1280, 720), 16);
        assert_eq!(block_size(1920, 1080), 24);
        assert_eq!(block_size(2560, 1440), 40);
        assert_eq!(block_size(3840, 2160), 56);
        // SBS: the left eye's 16:9 width.
        assert_eq!(block_size(3840, 1080), 24);
        for (w, h) in [(640, 360), (1920, 1080), (3840, 1080), (3840, 2160), (800, 1280)] {
            let (rw, rh) = region(w, h);
            assert!(rw <= w && rh <= h, "{w}x{h}");
        }
    }

    #[test]
    fn luma8_round_trip_with_noise() {
        for (w, h, full) in [(640, 360, false), (1920, 1080, true), (3840, 2160, false), (3840, 1080, false)] {
            let stride = w as usize + 32;
            let mut plane = vec![128u8; stride * h as usize];
            draw_luma8(&mut plane, stride, w, h, full, STAMP);
            // Compression-like noise: ±40 everywhere, and soft block edges.
            let mut seed = 12345u32;
            for p in plane.iter_mut() {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let n = ((seed >> 16) % 81) as i32 - 40;
                *p = (*p as i32 + n).clamp(0, 255) as u8;
            }
            assert_eq!(read_luma8(&plane, stride, w, h), Some(STAMP), "{w}x{h}");
        }
    }

    #[test]
    fn luma16_p010_round_trip() {
        let (w, h) = (2560u32, 1440u32);
        let mut y8 = vec![0u8; (w * h) as usize];
        draw_luma8(&mut y8, w as usize, w, h, false, STAMP);
        // 8-bit to 10-bit, MSB-aligned like P010.
        let p010: Vec<u16> = y8.iter().map(|&v| ((v as u16) << 2) << 6).collect();
        assert_eq!(read_luma16(&p010, w as usize, w, h, 10, 6), Some(STAMP));
        let lsb: Vec<u16> = y8.iter().map(|&v| (v as u16) << 2).collect();
        assert_eq!(read_luma16(&lsb, w as usize, w, h, 10, 0), Some(STAMP));
    }

    #[test]
    fn rgba_matches_luma() {
        let (w, h) = (1280u32, 720u32);
        let mut rgba = vec![90u8; (w * h * 4) as usize];
        draw_rgba(&mut rgba, w as usize * 4, w, h, STAMP);
        let got = read_with(w, h, |x, y| rgba[((y * w + x) * 4) as usize]);
        assert_eq!(got, Some(STAMP));
    }

    #[test]
    fn plain_frame_reads_nothing() {
        let (w, h) = (1920u32, 1080u32);
        assert_eq!(read_luma8(&vec![16u8; (w * h) as usize], w as usize, w, h), None);
        assert_eq!(read_luma8(&vec![235u8; (w * h) as usize], w as usize, w, h), None);
        assert_eq!(read_luma8(&[0u8; 16], 4, 4, 4), None);
    }
}
