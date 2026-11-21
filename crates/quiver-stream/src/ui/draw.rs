//! Overlay geometry: rounded rectangles, text and images as triangles for the overlay
//! pipeline (shaders/ui.vert, ui.frag).

use super::atlas::{Atlas, Font, Glyph};

/// One overlay vertex (matches the pipeline's vertex input).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 2],
    /// Atlas texel for images, offset from the centre for shapes.
    pub uv: [f32; 2],
    pub color: [u8; 4],
    /// Half size, corner radius, kind (0 image, 1 rounded rect).
    pub shape: [f32; 4],
}

/// An sRGB colour with straight alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const fn hex(rgb: u32, a: u8) -> Self {
        Self([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, a])
    }
    pub fn alpha(self, a: f32) -> Self {
        let mut c = self.0;
        c[3] = (c[3] as f32 * a.clamp(0.0, 1.0)) as u8;
        Self(c)
    }
}

/// Quiver's dark palette (quiver-ui's `palette.rs`) and the brand accents.
pub mod colors {
    use super::Rgba;
    pub const INK0: Rgba = Rgba::hex(0x0a0c10, 255);
    pub const INK2: Rgba = Rgba::hex(0x141821, 255);
    pub const INK4: Rgba = Rgba::hex(0x222836, 255);
    pub const LINE2: Rgba = Rgba::hex(0x2c3343, 255);
    pub const TX1: Rgba = Rgba::hex(0xeceff5, 255);
    pub const TX2: Rgba = Rgba::hex(0xa8afbf, 255);
    pub const TX3: Rgba = Rgba::hex(0x727a8d, 255);
    pub const CYAN: Rgba = Rgba::hex(0x3fc8e4, 255);
    pub const RED: Rgba = Rgba::hex(0xff6a55, 255);
    pub const OK: Rgba = Rgba::hex(0x4fc48e, 255);
    pub const WARN: Rgba = Rgba::hex(0xf0b44c, 255);
}

pub struct Painter<'a> {
    pub atlas: &'a Atlas,
    pub verts: Vec<Vertex>,
    pub scale: f32,
}

impl<'a> Painter<'a> {
    pub fn new(atlas: &'a Atlas, scale: f32) -> Self {
        Self { atlas, verts: Vec::new(), scale }
    }

    /// Scale a length in UI points to pixels.
    pub fn s(&self, v: f32) -> f32 {
        v * self.scale
    }

    #[allow(clippy::too_many_arguments)]
    fn quad(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, uv0: [f32; 2], uv1: [f32; 2], color: Rgba, shape: [f32; 4]) {
        let v = |x, y, u, w| Vertex { pos: [x, y], uv: [u, w], color: color.0, shape };
        let (a, b, c, d) = (v(x0, y0, uv0[0], uv0[1]), v(x1, y0, uv1[0], uv0[1]), v(x1, y1, uv1[0], uv1[1]), v(x0, y1, uv0[0], uv1[1]));
        self.verts.extend_from_slice(&[a, b, c, a, c, d]);
    }

    /// A filled rounded rectangle.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgba) {
        let (hw, hh) = (w / 2.0, h / 2.0);
        // Half a pixel beyond the edge, for the anti-aliased rim.
        let pad = 1.0;
        self.quad(
            x - pad,
            y - pad,
            x + w + pad,
            y + h + pad,
            [-hw - pad, -hh - pad],
            [hw + pad, hh + pad],
            color,
            [hw, hh, radius.min(hw).min(hh), 1.0],
        );
    }

    /// An atlas image (a glyph or the logo) with its top-left at (x, y).
    pub fn image(&mut self, x: f32, y: f32, g: &Glyph, color: Rgba) {
        if g.w == 0 {
            return;
        }
        let (u0, v0) = (g.x as f32, g.y as f32);
        self.quad(x, y, x + g.w as f32, y + g.h as f32, [u0, v0], [u0 + g.w as f32, v0 + g.h as f32], color, [0.0; 4]);
    }

    /// Text with its baseline at `baseline`; returns the width.
    pub fn text(&mut self, x: f32, baseline: f32, font: Font, text: &str, color: Rgba) -> f32 {
        let mut pen = x.round();
        let base = baseline.round();
        for ch in text.chars() {
            let Some(&g) = self.atlas.glyph(font, ch) else {
                continue;
            };
            self.image(pen + g.left, base - g.top, &g, color);
            pen += g.advance;
        }
        pen - x
    }

    pub fn measure(&self, font: Font, text: &str) -> f32 {
        self.atlas.measure(font, text)
    }
}
