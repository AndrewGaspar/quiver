//! The test-pattern target: synthetic frames at any size and rate, for measuring the whole
//! pipeline without a game.
//!
//! Each frame has, in a band across the top:
//! - the **stamp** ([`quiver_stream_proto::stamp`]): the frame's seq and capture time as black
//!   and white cells a client reads back from its decoded picture;
//! - the same seq, and the capture time in milliseconds, as large seven-segment digits;
//! - a block that moves a fixed step each frame, for judging pacing by eye.
//!
//! Below the band are colour bars and fine texture. They scroll sideways a few pixels a frame
//! (`motion`), so the encoder works on a whole changing picture, as it would with a game.
//! The bars and texture are drawn once, kept in CUDA memory, and copied into the encoder's
//! input each frame; only the band is drawn on the CPU and uploaded. In HDR the bars are at SDR
//! levels and one patch is at 1000 nits.

use anyhow::Result;
use quiver_stream_proto::stamp::{self, Stamp};
use quiver_stream_proto::{ColorInfo, Micros};

use crate::cuda::{Cuda, DeviceBuf};
use crate::nvenc::{InputFormat, Surface};
use std::sync::Arc;

/// A colour in the picture's YUV, at the picture's bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Yuv {
    pub y: u16,
    pub u: u16,
    pub v: u16,
}

/// The SMPTE ST 2084 (PQ) encoding of an absolute luminance, 0..1.
pub fn pq(nits: f64) -> f64 {
    let y = (nits / 10_000.0).clamp(0.0, 1.0);
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let p = y.powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2)
}

/// Non-linear R'G'B' (0..1) to the picture's Y'CbCr: BT.709 for SDR, BT.2020 (non-constant
/// luminance) for HDR, at its bit depth and range.
pub fn rgb_to_yuv(rgb: [f64; 3], color: &ColorInfo) -> Yuv {
    let (kr, kb) = if color.hdr.is_some() { (0.2627, 0.0593) } else { (0.2126, 0.0722) };
    let [r, g, b] = rgb.map(|c| c.clamp(0.0, 1.0));
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    let bits = color.bit_depth.max(8);
    let scale = (1u32 << (bits - 8)) as f64;
    let max = ((1u32 << bits) - 1) as f64;
    let full = color.full_range;
    let luma = if full { y * max } else { 16.0 * scale + 219.0 * scale * y };
    let chroma = |v: f64| if full { 128.0 * scale + v * max } else { 128.0 * scale + 224.0 * scale * v };
    let q = |v: f64| v.round().clamp(0.0, max) as u16;
    Yuv { y: q(luma), u: q(chroma(cb)), v: q(chroma(cr)) }
}

/// A colour for the pattern: SDR R'G'B', or in HDR the same colour at `sdr_nits` (PQ).
fn color_of(rgb: [f64; 3], color: &ColorInfo, nits: f64) -> Yuv {
    if color.hdr.is_some() {
        // Linearise the sRGB-ish value, scale to nits, and PQ-encode each channel.
        let lin = rgb.map(|c| c.clamp(0.0, 1.0).powf(2.4));
        rgb_to_yuv(lin.map(|l| pq(l * nits)), color)
    } else {
        rgb_to_yuv(rgb, color)
    }
}

/// A picture in host memory, laid out as the encoder's input (planes stacked at one pitch).
pub struct Canvas {
    pub format: InputFormat,
    pub width: u32,
    pub height: u32,
    pub pitch: usize,
    pub data: Vec<u8>,
}

impl Canvas {
    pub fn new(format: InputFormat, width: u32, height: u32) -> Canvas {
        assert!(!format.is_rgb(), "the pattern draws YUV");
        let pitch = width as usize * format.bytes_per_pixel();
        Canvas { format, width, height, pitch, data: vec![0; pitch * format.rows(height)] }
    }

    /// Set `n` samples of row `row` from column `col`, every `step` samples, to `v`.
    fn run(&mut self, row: usize, col: usize, n: usize, step: usize, v: u16) {
        let bpp = self.format.bytes_per_pixel();
        let start = row * self.pitch + col * bpp;
        let line = &mut self.data[start..start + (n.saturating_sub(1) * step + 1) * bpp];
        if bpp == 1 {
            if step == 1 {
                line.fill(v as u8);
            } else {
                line.iter_mut().step_by(step).for_each(|b| *b = v as u8);
            }
        } else {
            let le = (v << 6).to_le_bytes();
            for px in line.chunks_exact_mut(2).step_by(step) {
                px.copy_from_slice(&le);
            }
        }
    }

    /// Fill a rectangle (clipped to the picture). For 4:2:0, give even coordinates and sizes.
    pub fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, c: Yuv) {
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        if x >= x1 || y >= y1 {
            return;
        }
        let (x0, n) = (x as usize, (x1 - x) as usize);
        let hh = self.height as usize;
        for row in y as usize..y1 as usize {
            self.run(row, x0, n, 1, c.y);
        }
        match self.format {
            InputFormat::Nv12 | InputFormat::P010 => {
                let (cx0, cx1) = (x0 / 2, (x1 as usize).div_ceil(2));
                for row in (y as usize / 2)..(y1 as usize).div_ceil(2) {
                    self.run(hh + row, cx0 * 2, cx1 - cx0, 2, c.u);
                    self.run(hh + row, cx0 * 2 + 1, cx1 - cx0, 2, c.v);
                }
            }
            InputFormat::Yuv444 | InputFormat::Yuv444P16 => {
                for row in y as usize..y1 as usize {
                    self.run(hh + row, x0, n, 1, c.u);
                    self.run(2 * hh + row, x0, n, 1, c.v);
                }
            }
            InputFormat::Bgrx | InputFormat::Xrgb2101010 => unreachable!(),
        }
    }

    /// The luma at a pixel, in 8 bits (for tests and the stamp's read-back).
    pub fn luma8(&self, x: u32, y: u32) -> u8 {
        let i = y as usize * self.pitch + x as usize * self.format.bytes_per_pixel();
        match self.format.bytes_per_pixel() {
            1 => self.data[i],
            _ => (u16::from_le_bytes([self.data[i], self.data[i + 1]]) >> 8) as u8,
        }
    }

    /// Byte ranges of the rows `y0..y1` in every plane: (first row, rows), for uploads.
    pub fn plane_rows(&self, y0: u32, y1: u32) -> Vec<(usize, usize)> {
        let h = self.height as usize;
        let (y0, y1) = (y0 as usize, y1 as usize);
        match self.format {
            InputFormat::Nv12 | InputFormat::P010 => vec![(y0, y1 - y0), (h + y0 / 2, y1.div_ceil(2) - y0 / 2)],
            _ => vec![(y0, y1 - y0), (h + y0, y1 - y0), (2 * h + y0, y1 - y0)],
        }
    }
}

/// Seven-segment digits: segments a b c d e f g as bits 0..6.
const DIGITS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];

/// Draw `text` (digits only; anything else is a gap) at (x, y), each digit `dw` × `dh`.
fn draw_digits(cv: &mut Canvas, x: u32, y: u32, dw: u32, dh: u32, text: &str, fg: Yuv) {
    let t = (dw / 5).max(2) & !1;
    let half = (dh / 2) & !1;
    let mut cx = x;
    for ch in text.chars() {
        if let Some(d) = ch.to_digit(10) {
            let s = DIGITS[d as usize];
            let seg = |cv: &mut Canvas, on: bool, sx: u32, sy: u32, sw: u32, sh: u32| {
                if on {
                    cv.fill(sx, sy, sw, sh, fg);
                }
            };
            seg(cv, s & 0x01 != 0, cx, y, dw, t);
            seg(cv, s & 0x02 != 0, cx + dw - t, y, t, half);
            seg(cv, s & 0x04 != 0, cx + dw - t, y + half, t, dh - half);
            seg(cv, s & 0x08 != 0, cx, y + dh - t, dw, t);
            seg(cv, s & 0x10 != 0, cx, y + half, t, dh - half);
            seg(cv, s & 0x20 != 0, cx, y, t, half);
            seg(cv, s & 0x40 != 0, cx, y + half - t / 2, dw, t);
        }
        cx += dw + dw / 3 + (dw / 3) % 2;
    }
}

/// The band at the top that changes every frame, and the static picture below it.
pub struct Pattern {
    pub width: u32,
    pub height: u32,
    pub format: InputFormat,
    color: ColorInfo,
    /// The band's height (even).
    pub band: u32,
    canvas: Canvas,
    background: Option<DeviceBuf>,
    pub motion: bool,
    step: u32,
    gray: Yuv,
    white: Yuv,
    black: Yuv,
}

impl Pattern {
    pub fn new(width: u32, height: u32, color: ColorInfo, motion: bool) -> Pattern {
        let format = InputFormat::yuv_for(&color);
        let l = stamp::layout(width, height);
        let stamp_h = l.map(|l| l.size().1).unwrap_or(0);
        let dh = (height / 8).max(8) & !1;
        let band = ((stamp_h + dh * 2 + dh / 2 + height / 20 + 16).min(height)) & !1;
        let mut canvas = Canvas::new(format, width, height);
        let gray = color_of([0.25, 0.25, 0.25], &color, 203.0);
        let white = color_of([1.0, 1.0, 1.0], &color, 203.0);
        let black = color_of([0.0, 0.0, 0.0], &color, 203.0);
        draw_background(&mut canvas, band, &color);
        Pattern { width, height, format, color, band, canvas, background: None, motion, step: 4, gray, white, black }
    }

    pub fn color(&self) -> &ColorInfo {
        &self.color
    }

    /// Draw frame `seq` (captured at `captured`) into the host canvas's band.
    pub fn draw_band(&mut self, seq: u64, captured: Micros) {
        let (w, band) = (self.width, self.band);
        let cv = &mut self.canvas;
        cv.fill(0, 0, w, band, self.gray);
        let (white, black) = (self.white, self.black);
        stamp::paint(w, self.height, Stamp { seq, captured }, |x, y, cw, ch, on| cv.fill(x, y, cw, ch, if on { white } else { black }));
        let stamp_h = stamp::layout(w, self.height).map(|l| l.size().1).unwrap_or(0);
        let dh = (self.height / 8).max(8) & !1;
        let dw = (dh / 2) & !1;
        let y0 = stamp_h + 8;
        draw_digits(cv, 8, y0, dw, dh, &format!("{seq:08}"), white);
        draw_digits(cv, 8, y0 + dh + dh / 4, dw / 2 & !1, dh / 2 & !1, &format!("{:010}", captured / 1000), white);
        // The moving block.
        let by = (y0 + dh + dh / 4 + dh / 2 + 8) & !1;
        let bh = band.saturating_sub(by + 4) & !1;
        let bw = (w / 32).max(8) & !1;
        let travel = w.saturating_sub(bw).max(2);
        let bx = ((seq as u32).wrapping_mul(self.step) % travel) & !1;
        cv.fill(bx, by, bw, bh, white);
    }

    /// Write frame `seq` into the encoder's surface: the background (scrolled when `motion`)
    /// and then the band. The CUDA context must be current.
    pub fn render(&mut self, cuda: &Arc<Cuda>, surface: &mut Surface, seq: u64, captured: Micros) -> Result<()> {
        assert_eq!((surface.format, surface.width, surface.height), (self.format, self.width, self.height));
        if self.background.is_none() {
            let rows = self.format.rows(self.height);
            let buf = cuda.alloc_pitch(self.canvas.pitch, rows)?;
            // SAFETY: the canvas holds `rows` rows of `pitch` bytes, and so does the buffer.
            unsafe { cuda.copy_htod(self.canvas.data.as_ptr(), self.canvas.pitch, buf.ptr, buf.pitch, self.canvas.pitch, rows)? };
            self.background = Some(buf);
        }
        let bg = self.background.as_ref().unwrap();
        let rows = self.format.rows(self.height);
        let bpp = self.format.bytes_per_pixel();
        let row_bytes = self.width as usize * bpp;
        if self.motion {
            // Scroll left by `off` pixels: bg[off..] to [0..], then bg[..off] to the end.
            let off = ((seq as usize * self.step as usize * 2) % self.width as usize) & !1;
            let ob = off * bpp;
            unsafe {
                if ob < row_bytes {
                    cuda.copy_dtod(bg.ptr + ob as u64, bg.pitch, surface.row(0), surface.pitch(), row_bytes - ob, rows)?;
                }
                if ob > 0 {
                    cuda.copy_dtod(bg.ptr, bg.pitch, surface.row(0) + (row_bytes - ob) as u64, surface.pitch(), ob, rows)?;
                }
            }
            surface.tag = 0;
        } else if surface.tag != 1 {
            unsafe { cuda.copy_dtod(bg.ptr, bg.pitch, surface.row(0), surface.pitch(), row_bytes, rows)? };
            surface.tag = 1;
        }
        self.draw_band(seq, captured);
        for (first, n) in self.canvas.plane_rows(0, self.band) {
            let src = unsafe { self.canvas.data.as_ptr().add(first * self.canvas.pitch) };
            unsafe { cuda.copy_htod(src, self.canvas.pitch, surface.row(first), surface.pitch(), row_bytes, n)? };
        }
        Ok(())
    }

    /// The host canvas (for tests).
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }
}

/// Colour bars, a grey ramp and fine texture below the band, periodic across the width so
/// scrolling wraps seamlessly.
fn draw_background(cv: &mut Canvas, band: u32, color: &ColorInfo) {
    let (w, h) = (cv.width, cv.height);
    let bars: [[f64; 3]; 8] = [[0.75, 0.75, 0.75], [0.75, 0.75, 0.0], [0.0, 0.75, 0.75], [0.0, 0.75, 0.0], [0.75, 0.0, 0.75], [0.75, 0.0, 0.0], [0.0, 0.0, 0.75], [0.1, 0.1, 0.1]];
    let rest = h - band;
    let bars_h = (rest / 2) & !1;
    let bw = (w / 8) & !1;
    for (i, c) in bars.iter().enumerate() {
        let x = i as u32 * bw;
        let wide = if i == 7 { w - x } else { bw };
        cv.fill(x, band, wide, bars_h, color_of(*c, color, 100.0));
    }
    // A ramp of 16 greys.
    let ramp_y = band + bars_h;
    let ramp_h = (rest / 6) & !1;
    let sw = (w / 16) & !1;
    for i in 0..16u32 {
        let v = i as f64 / 15.0;
        let x = i * sw;
        cv.fill(x, ramp_y, if i == 15 { w - x } else { sw }, ramp_h, color_of([v, v, v], color, 100.0));
    }
    // Texture: 8×8 blocks of pseudo-random greys, then a highlight patch.
    let tex_y = ramp_y + ramp_h;
    let mut seed = 0x9e37_79b9u32;
    for by in (tex_y..h).step_by(8) {
        for bx in (0..w).step_by(8) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let v = 0.2 + 0.6 * ((seed >> 8) & 0xff) as f64 / 255.0;
            let tint = [v, v * (0.8 + 0.2 * ((seed & 3) as f64 / 3.0)), v * 0.9];
            cv.fill(bx, by, 8, 8, color_of(tint, color, 100.0));
        }
    }
    if color.hdr.is_some() {
        // 1000 nits white, a quarter of the width.
        let p = color_of([1.0, 1.0, 1.0], color, 1000.0);
        cv.fill(w / 2, tex_y, (w / 4) & !1, ((h - tex_y) / 2) & !1, p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_stream_proto::{Chroma, HdrMetadata};

    #[test]
    fn bt709_limited_white_black_and_red() {
        let c = ColorInfo::sdr();
        assert_eq!(rgb_to_yuv([1.0, 1.0, 1.0], &c), Yuv { y: 235, u: 128, v: 128 });
        assert_eq!(rgb_to_yuv([0.0, 0.0, 0.0], &c), Yuv { y: 16, u: 128, v: 128 });
        // BT.709 red: Y 63, Cb 102, Cr 240.
        assert_eq!(rgb_to_yuv([1.0, 0.0, 0.0], &c), Yuv { y: 63, u: 102, v: 240 });
        let ten = ColorInfo { bit_depth: 10, ..ColorInfo::sdr() };
        assert_eq!(rgb_to_yuv([1.0, 1.0, 1.0], &ten), Yuv { y: 940, u: 512, v: 512 });
        let full = ColorInfo { full_range: true, ..ColorInfo::sdr() };
        assert_eq!(rgb_to_yuv([1.0, 1.0, 1.0], &full).y, 255);
    }

    #[test]
    fn pq_reference_points() {
        // ST 2084: 100 nits ≈ 0.508, 1000 nits ≈ 0.752, 10000 = 1.
        assert!((pq(100.0) - 0.5081).abs() < 0.001);
        assert!((pq(1000.0) - 0.7518).abs() < 0.001);
        assert!((pq(10_000.0) - 1.0).abs() < 1e-9);
        assert_eq!(pq(0.0), 0.0f64.max(pq(0.0)));
    }

    fn read_back(p: &Pattern) -> Option<Stamp> {
        let cv = p.canvas();
        stamp::read(p.width, p.height, |x, y| cv.luma8(x, y))
    }

    #[test]
    fn the_band_carries_a_readable_stamp_in_every_format() {
        let hdr = HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 1000, max_fall: 400 };
        for color in [
            ColorInfo::sdr(),
            ColorInfo { bit_depth: 10, chroma: Chroma::Yuv420, full_range: false, hdr: Some(hdr.clone()) },
            ColorInfo { bit_depth: 8, chroma: Chroma::Yuv444, full_range: false, hdr: None },
            ColorInfo { bit_depth: 10, chroma: Chroma::Yuv444, full_range: true, hdr: None },
        ] {
            let mut p = Pattern::new(1280, 720, color.clone(), true);
            p.draw_band(123_456, 9_876_543_210);
            assert_eq!(read_back(&p), Some(Stamp { seq: 123_456, captured: 9_876_543_210 }), "{color:?}");
        }
    }

    #[test]
    fn the_band_fits_small_and_odd_aspect_frames() {
        for (w, h) in [(320, 180), (3840, 1080), (1920, 1200)] {
            let mut p = Pattern::new(w, h, ColorInfo::sdr(), false);
            assert!(p.band <= h && p.band % 2 == 0);
            p.draw_band(7, 8);
            assert_eq!(read_back(&p), Some(Stamp { seq: 7, captured: 8 }), "{w}x{h}");
        }
    }

    #[test]
    fn plane_rows_cover_the_band_in_each_plane() {
        let cv = Canvas::new(InputFormat::Nv12, 64, 32);
        assert_eq!(cv.plane_rows(0, 10), vec![(0, 10), (32, 5)]);
        let cv = Canvas::new(InputFormat::Yuv444, 64, 32);
        assert_eq!(cv.plane_rows(0, 10), vec![(0, 10), (32, 10), (64, 10)]);
    }
}
