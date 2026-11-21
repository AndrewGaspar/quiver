//! The frame stamp: a frame's `seq` and capture time drawn into the picture itself, so a client
//! can read them back from the decoded image and measure end-to-end latency without a camera.
//!
//! The host's test pattern draws it (`quiver-stream-host --target test-pattern`). It is two
//! rows of [`CELLS`] black or white cells across the top-left of the frame: row 0 carries the
//! frame's `seq`, row 1 its `captured` time. Each row is
//!
//! ```text
//! white black | 64 data bits, most significant first | CRC-8 of the value | black white
//! ```
//!
//! A white cell is a 1. The guards give the reader its black and white levels, and the CRC
//! (polynomial 0x07 over the value's eight little-endian bytes) tells a good read from a frame
//! the decoder concealed. Cells are whole pixels wide and sized from the frame ([`layout`]), so
//! they survive chroma subsampling and a lossy encode at any bitrate a stream would use.

use crate::Micros;

/// Cells per row: 2 guard, 64 data, 8 CRC, 2 guard.
pub const CELLS: u32 = 76;

/// Where the stamp sits in a frame of a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StampLayout {
    /// The width of one cell, in pixels.
    pub cell_w: u32,
    /// The height of one row, in pixels.
    pub row_h: u32,
}

impl StampLayout {
    /// The rectangle the stamp covers, from the top-left corner: (width, height).
    pub fn size(&self) -> (u32, u32) {
        (self.cell_w * CELLS, self.row_h * 2)
    }
}

/// The stamp's layout for a `width` × `height` frame. Frames narrower than [`CELLS`] × 2
/// pixels or shorter than 8 have no room for one.
pub fn layout(width: u32, height: u32) -> Option<StampLayout> {
    if width < CELLS * 2 || height < 8 {
        return None;
    }
    // Even sizes, so a 4:2:0 chroma sample never straddles two cells.
    let cell_w = (width / CELLS) & !1;
    let row_h = ((height / 54).max(4)) & !1;
    Some(StampLayout { cell_w, row_h })
}

/// What a stamp says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub seq: u64,
    pub captured: Micros,
}

/// CRC-8, polynomial 0x07, initial 0, over `value`'s little-endian bytes.
pub fn crc8(value: u64) -> u8 {
    let mut crc = 0u8;
    for b in value.to_le_bytes() {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

/// One row's cells, left to right (true is white).
pub fn row_cells(value: u64) -> [bool; CELLS as usize] {
    let mut c = [false; CELLS as usize];
    c[0] = true;
    for i in 0..64 {
        c[2 + i] = (value >> (63 - i)) & 1 == 1;
    }
    let crc = crc8(value);
    for i in 0..8 {
        c[66 + i] = (crc >> (7 - i)) & 1 == 1;
    }
    c[75] = true;
    c
}

/// Draw a stamp: `fill(x, y, w, h, white)` is called once per cell, covering the whole
/// stamp rectangle. Does nothing when the frame is too small ([`layout`]).
pub fn paint(width: u32, height: u32, stamp: Stamp, mut fill: impl FnMut(u32, u32, u32, u32, bool)) {
    let Some(l) = layout(width, height) else { return };
    for (row, value) in [stamp.seq, stamp.captured].into_iter().enumerate() {
        for (i, white) in row_cells(value).into_iter().enumerate() {
            fill(i as u32 * l.cell_w, row as u32 * l.row_h, l.cell_w, l.row_h, white);
        }
    }
}

/// Read a stamp back. `luma(x, y)` is the decoded luma at a pixel, 0–255 (for a 10-bit
/// picture, the top eight bits). None when there's no stamp, or a row fails its CRC.
pub fn read(width: u32, height: u32, luma: impl Fn(u32, u32) -> u8) -> Option<Stamp> {
    let l = layout(width, height)?;
    // The middle of a cell, averaged over a few pixels to ride over ringing at the edges.
    let sample = |cell: u32, row: u32| -> u32 {
        let cx = cell * l.cell_w + l.cell_w / 2;
        let cy = row * l.row_h + l.row_h / 2;
        let r = (l.cell_w / 4).min(l.row_h / 4).max(1) as i64;
        let mut sum = 0u32;
        let mut n = 0u32;
        for dy in [-r / 2, 0, r / 2] {
            for dx in [-r / 2, 0, r / 2] {
                let x = (cx as i64 + dx).clamp(0, width as i64 - 1) as u32;
                let y = (cy as i64 + dy).clamp(0, height as i64 - 1) as u32;
                sum += luma(x, y) as u32;
                n += 1;
            }
        }
        sum / n
    };
    let mut values = [0u64; 2];
    for (row, out) in values.iter_mut().enumerate() {
        let row = row as u32;
        let white = (sample(0, row) + sample(CELLS - 1, row)) / 2;
        let black = (sample(1, row) + sample(CELLS - 2, row)) / 2;
        if white < black + 64 {
            return None;
        }
        let mid = (white + black) / 2;
        let bit = |cell: u32| sample(cell, row) > mid;
        let mut v = 0u64;
        for i in 0..64 {
            v = (v << 1) | bit(2 + i) as u64;
        }
        let mut crc = 0u8;
        for i in 0..8 {
            crc = (crc << 1) | bit(66 + i) as u8;
        }
        if crc != crc8(v) {
            return None;
        }
        *out = v;
    }
    Some(Stamp { seq: values[0], captured: values[1] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(w: u32, h: u32, s: Stamp) -> Vec<u8> {
        let mut img = vec![90u8; (w * h) as usize];
        paint(w, h, s, |x, y, cw, ch, white| {
            for yy in y..y + ch {
                for xx in x..x + cw {
                    img[(yy * w + xx) as usize] = if white { 235 } else { 16 };
                }
            }
        });
        img
    }

    #[test]
    fn round_trips_at_common_sizes() {
        for (w, h) in [(640, 360), (1280, 720), (2560, 1440), (3840, 2160), (3840, 1080)] {
            let s = Stamp { seq: 0x0123_4567_89ab_cdef, captured: 987_654_321_012 };
            let img = draw(w, h, s);
            assert_eq!(read(w, h, |x, y| img[(y * w + x) as usize]), Some(s), "{w}x{h}");
        }
    }

    #[test]
    fn survives_noise_and_soft_edges() {
        let (w, h) = (1280, 720);
        let s = Stamp { seq: 42, captured: 1_000_000 };
        let img = draw(w, h, s);
        // ±30 of noise and a 3-tap blur along x, roughly what a low-bitrate encode does.
        let noisy: Vec<u8> = img
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let n = ((i as u32).wrapping_mul(2_654_435_761) >> 26) as i32 - 32;
                (v as i32 + n).clamp(0, 255) as u8
            })
            .collect();
        let blurred: Vec<u8> = (0..noisy.len())
            .map(|i| {
                let x = i as u32 % w;
                let a = noisy[if x > 0 { i - 1 } else { i }] as u32;
                let c = noisy[if x + 1 < w { i + 1 } else { i }] as u32;
                ((a + 2 * noisy[i] as u32 + c) / 4) as u8
            })
            .collect();
        assert_eq!(read(w, h, |x, y| blurred[(y * w + x) as usize]), Some(s));
    }

    #[test]
    fn no_stamp_and_bad_crc_read_as_none() {
        let (w, h) = (1280, 720);
        let flat = vec![128u8; (w * h) as usize];
        assert_eq!(read(w, h, |x, y| flat[(y * w + x) as usize]), None);
        let mut img = draw(w, h, Stamp { seq: 7, captured: 8 });
        // Flip one data cell of row 0.
        let l = layout(w, h).unwrap();
        let x0 = 40 * l.cell_w;
        for y in 0..l.row_h {
            for x in x0..x0 + l.cell_w {
                let p = &mut img[(y * w + x) as usize];
                *p = if *p > 128 { 16 } else { 235 };
            }
        }
        assert_eq!(read(w, h, |x, y| img[(y * w + x) as usize]), None);
    }

    #[test]
    fn too_small_has_no_layout() {
        assert_eq!(layout(100, 100), None);
        assert!(layout(CELLS * 2, 8).is_some());
        let l = layout(2560, 1440).unwrap();
        assert_eq!((l.cell_w % 2, l.row_h % 2), (0, 0));
        assert!(l.size().0 <= 2560);
    }
}
