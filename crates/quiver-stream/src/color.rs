//! Y'CbCr to R'G'B': the matrix the conversion shader applies to stored samples.

use crate::av::{AVCOL_RANGE_JPEG, AVCOL_SPC_BT2020_NCL, AVCOL_SPC_BT470BG, AVCOL_SPC_SMPTE170M, AVCOL_TRC_SMPTE2084, FrameColor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl Matrix {
    /// (Kr, Kb).
    fn k(self) -> (f32, f32) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// How to read a picture's samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YuvParams {
    pub matrix: Matrix,
    pub full_range: bool,
    /// Bits per component.
    pub depth: u32,
    /// The code value is the sampled (normalised) value times this: 255 for 8-bit, 65535 >>
    /// shift for 16-bit storage, 1023 for Vulkan's 10-bit X6 formats.
    pub code_scale: f32,
    /// The picture is PQ (HDR10).
    pub pq: bool,
}

impl YuvParams {
    /// From what the stream's Configure says (`hdr`, `full_range`), with the frame's own tags
    /// filling in.
    pub fn pick(hdr: bool, full_range: bool, frame: FrameColor, depth: u32, code_scale: f32) -> Self {
        let matrix = if hdr || frame.space == AVCOL_SPC_BT2020_NCL || frame.space == 10 {
            Matrix::Bt2020
        } else if frame.space == AVCOL_SPC_SMPTE170M || frame.space == AVCOL_SPC_BT470BG {
            Matrix::Bt601
        } else {
            Matrix::Bt709
        };
        Self {
            matrix,
            full_range: full_range || frame.range == AVCOL_RANGE_JPEG,
            depth,
            code_scale,
            pq: hdr || frame.trc == AVCOL_TRC_SMPTE2084,
        }
    }

    /// The 4×4 affine matrix, column-major (as a GLSL `mat4` reads it), taking (Y, Cb, Cr, 1)
    /// as sampled to (R', G', B', 1).
    pub fn matrix(&self) -> [[f32; 4]; 4] {
        let (kr, kb) = self.matrix.k();
        let kg = 1.0 - kr - kb;
        let s = (1u32 << self.depth.saturating_sub(8)) as f32;
        let max = ((1u64 << self.depth) - 1) as f32;
        let (yo, ys, co, cs) = if self.full_range {
            (0.0, max, (1u32 << (self.depth - 1)) as f32, max)
        } else {
            (16.0 * s, 219.0 * s, 128.0 * s, 224.0 * s)
        };
        let k = self.code_scale;
        let a = 2.0 * (1.0 - kr);
        let d = 2.0 * (1.0 - kb);
        let b = 2.0 * kb * (1.0 - kb) / kg;
        let c = 2.0 * kr * (1.0 - kr) / kg;
        let (ky, kc) = (k / ys, k / cs);
        let (oy, oc) = (-yo / ys, co / cs);
        // Rows R, G, B.
        let rows = [
            [ky, 0.0, a * kc, oy - a * oc],
            [ky, -b * kc, -c * kc, oy + (b + c) * oc],
            [ky, d * kc, 0.0, oy - d * oc],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let mut m = [[0.0; 4]; 4];
        for (r, row) in rows.iter().enumerate() {
            for (col, v) in row.iter().enumerate() {
                m[col][r] = *v;
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &[[f32; 4]; 4], v: [f32; 3]) -> [f32; 3] {
        let x = [v[0], v[1], v[2], 1.0];
        let mut o = [0.0; 3];
        for (r, out) in o.iter_mut().enumerate() {
            *out = (0..4).map(|c| m[c][r] * x[c]).sum();
        }
        o
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 2e-3)
    }

    #[test]
    fn limited_709_black_white_and_red() {
        let p = YuvParams { matrix: Matrix::Bt709, full_range: false, depth: 8, code_scale: 255.0, pq: false };
        let m = p.matrix();
        let n = |c: f32| c / 255.0;
        assert!(close(apply(&m, [n(16.0), n(128.0), n(128.0)]), [0.0, 0.0, 0.0]));
        assert!(close(apply(&m, [n(235.0), n(128.0), n(128.0)]), [1.0, 1.0, 1.0]));
        // BT.709 red, limited: Y 63, Cb 102, Cr 240.
        let r = apply(&m, [n(63.0), n(102.0), n(240.0)]);
        assert!(r[0] > 0.99 && r[1].abs() < 0.01 && r[2].abs() < 0.01, "{r:?}");
    }

    #[test]
    fn full_range_and_ten_bit_p010() {
        let full = YuvParams { matrix: Matrix::Bt709, full_range: true, depth: 8, code_scale: 255.0, pq: false };
        assert!(close(apply(&full.matrix(), [1.0, 128.0 / 255.0, 128.0 / 255.0]), [1.0, 1.0, 1.0]));
        // P010: 10-bit codes in the top bits of 16; sampled as UNORM16.
        let p010 = YuvParams { matrix: Matrix::Bt2020, full_range: false, depth: 10, code_scale: 65535.0 / 64.0, pq: true };
        let s = |code: f32| code * 64.0 / 65535.0;
        assert!(close(apply(&p010.matrix(), [s(940.0), s(512.0), s(512.0)]), [1.0, 1.0, 1.0]));
        assert!(close(apply(&p010.matrix(), [s(64.0), s(512.0), s(512.0)]), [0.0, 0.0, 0.0]));
        // Vulkan's X6 formats sample as 10-bit UNORM.
        let x6 = YuvParams { code_scale: 1023.0, ..p010 };
        assert!(close(apply(&x6.matrix(), [940.0 / 1023.0, 512.0 / 1023.0, 512.0 / 1023.0]), [1.0, 1.0, 1.0]));
    }

    #[test]
    fn pick_uses_configure_then_frame_tags() {
        let f = FrameColor::default();
        assert_eq!(YuvParams::pick(true, false, f, 10, 1023.0).matrix, Matrix::Bt2020);
        assert!(YuvParams::pick(true, false, f, 10, 1023.0).pq);
        assert_eq!(YuvParams::pick(false, false, f, 8, 255.0).matrix, Matrix::Bt709);
        let tagged = FrameColor { range: AVCOL_RANGE_JPEG, space: AVCOL_SPC_SMPTE170M, ..f };
        let p = YuvParams::pick(false, false, tagged, 8, 255.0);
        assert_eq!(p.matrix, Matrix::Bt601);
        assert!(p.full_range);
    }
}
