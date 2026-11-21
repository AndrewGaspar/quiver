//! The overlay's texture atlas: Quiver's fonts (Geist, from quiver-ui's assets) rasterised
//! at the sizes the overlay uses, and the logo, packed once at start.
//!
//! Glyphs are white with coverage in alpha, so a vertex colour tints them; the logo keeps
//! its colours. Rasterising goes through ttf-parser outlines and tiny-skia (resvg's), so no
//! system font library is involved.

use std::collections::HashMap;

use resvg::tiny_skia::{self, FillRule, Paint, PathBuilder, Pixmap, Transform};

const GEIST_MEDIUM: &[u8] = include_bytes!("../../../quiver-ui/assets/fonts/Geist-Medium.ttf");
const GEIST_SEMIBOLD: &[u8] = include_bytes!("../../../quiver-ui/assets/fonts/Geist-SemiBold.ttf");
const GEIST_MONO: &[u8] = include_bytes!("../../../quiver-ui/assets/fonts/GeistMono-Medium.ttf");
const LOGO: &[u8] = include_bytes!("../../../quiver-ui/assets/brand/mark-dark.svg");

/// The overlay's text styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    /// Body text and menu rows.
    Body,
    /// Headings.
    Title,
    /// Numbers in the stats chip.
    Mono,
    /// Small labels.
    Small,
}

impl Font {
    pub const ALL: [Font; 4] = [Font::Body, Font::Title, Font::Mono, Font::Small];

    /// Size in pixels at scale 1.
    fn px(self) -> f32 {
        match self {
            Font::Body => 16.0,
            Font::Title => 21.0,
            Font::Mono => 14.0,
            Font::Small => 13.0,
        }
    }

    fn data(self) -> &'static [u8] {
        match self {
            Font::Body | Font::Small => GEIST_MEDIUM,
            Font::Title => GEIST_SEMIBOLD,
            Font::Mono => GEIST_MONO,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Glyph {
    /// Where it is in the atlas, in texels.
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Offset from the pen position (on the baseline) to the bitmap's top-left.
    pub left: f32,
    pub top: f32,
    pub advance: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    pub ascent: f32,
    pub descent: f32,
    pub line: f32,
}

pub struct Atlas {
    pub width: u32,
    pub height: u32,
    /// RGBA8, straight alpha.
    pub pixels: Vec<u8>,
    glyphs: HashMap<(Font, char), Glyph>,
    metrics: HashMap<Font, Metrics>,
    /// The logo's rectangle.
    pub logo: Glyph,
    /// A white texel, for solid shapes drawn as images.
    pub white: (u32, u32),
}

/// The characters the overlay draws (anything else falls back to '?').
fn charset() -> impl Iterator<Item = char> {
    (' '..='~').chain("·•…→←↑↓‹›–—×°✓ ".chars())
}

struct Packer {
    w: u32,
    x: u32,
    y: u32,
    row: u32,
}

impl Packer {
    fn place(&mut self, w: u32, h: u32) -> (u32, u32) {
        if self.x + w + 1 > self.w {
            self.x = 1;
            self.y += self.row + 1;
            self.row = 0;
        }
        let at = (self.x, self.y);
        self.x += w + 1;
        self.row = self.row.max(h);
        at
    }
}

struct Outline(PathBuilder, f32, f32, f32);

impl ttf_parser::OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x * self.1 + self.2, -y * self.1 + self.3);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x * self.1 + self.2, -y * self.1 + self.3);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1 * self.1 + self.2, -y1 * self.1 + self.3, x * self.1 + self.2, -y * self.1 + self.3);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(
            x1 * self.1 + self.2,
            -y1 * self.1 + self.3,
            x2 * self.1 + self.2,
            -y2 * self.1 + self.3,
            x * self.1 + self.2,
            -y * self.1 + self.3,
        );
    }
    fn close(&mut self) {
        self.0.close();
    }
}

impl Atlas {
    /// Rasterise everything at `scale` (the display's UI scale).
    pub fn new(scale: f32) -> Self {
        let scale = scale.clamp(0.5, 4.0);
        let width = 1024;
        let mut height = 1024;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let mut pk = Packer { w: width, x: 1, y: 1, row: 0 };
        // The white texel block.
        let (wx, wy) = pk.place(2, 2);
        for y in wy..wy + 2 {
            for x in wx..wx + 2 {
                let o = ((y * width + x) * 4) as usize;
                pixels[o..o + 4].copy_from_slice(&[255; 4]);
            }
        }
        let mut glyphs = HashMap::new();
        let mut metrics = HashMap::new();
        for font in Font::ALL {
            let face = ttf_parser::Face::parse(font.data(), 0).expect("bundled font parses");
            let px = font.px() * scale;
            let s = px / face.units_per_em() as f32;
            metrics.insert(
                font,
                Metrics {
                    ascent: face.ascender() as f32 * s,
                    descent: -face.descender() as f32 * s,
                    line: (face.ascender() - face.descender() + face.line_gap()) as f32 * s,
                },
            );
            for ch in charset() {
                let Some(id) = face.glyph_index(ch) else {
                    continue;
                };
                let advance = face.glyph_hor_advance(id).unwrap_or(0) as f32 * s;
                let Some(bb) = face.glyph_bounding_box(id) else {
                    glyphs.insert((font, ch), Glyph { advance, ..Default::default() });
                    continue;
                };
                // One pixel of padding around the bitmap.
                let left = (bb.x_min as f32 * s).floor() - 1.0;
                let top = (bb.y_max as f32 * s).ceil() + 1.0;
                let w = ((bb.x_max as f32 * s).ceil() - left + 1.0).max(1.0) as u32;
                let h = (top - (bb.y_min as f32 * s).floor() + 1.0).max(1.0) as u32;
                let mut ob = Outline(PathBuilder::new(), s, -left, top);
                if face.outline_glyph(id, &mut ob).is_none() {
                    continue;
                }
                let Some(path) = ob.0.finish() else { continue };
                let Some(mut pm) = Pixmap::new(w, h) else {
                    continue;
                };
                let mut paint = Paint::default();
                paint.set_color(tiny_skia::Color::WHITE);
                paint.anti_alias = true;
                pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
                let (x, y) = pk.place(w, h);
                if y + h >= height {
                    height *= 2;
                    pixels.resize((width * height * 4) as usize, 0);
                }
                blit(&mut pixels, width, x, y, &pm, true);
                glyphs.insert((font, ch), Glyph { x, y, w, h, left, top, advance });
            }
        }
        // The logo.
        let size = (34.0 * scale).round() as u32;
        let mut logo = Glyph::default();
        if let Ok(tree) = resvg::usvg::Tree::from_data(LOGO, &resvg::usvg::Options::default())
            && let Some(mut pm) = Pixmap::new(size, size)
        {
            let k = size as f32 / tree.size().width();
            resvg::render(&tree, Transform::from_scale(k, k), &mut pm.as_mut());
            let (x, y) = pk.place(size, size);
            if y + size >= height {
                height *= 2;
                pixels.resize((width * height * 4) as usize, 0);
            }
            blit(&mut pixels, width, x, y, &pm, false);
            logo = Glyph { x, y, w: size, h: size, left: 0.0, top: 0.0, advance: size as f32 };
        }
        // Trim to what's used.
        let used = (pk.y + pk.row + 2).min(height);
        pixels.truncate((width * used * 4) as usize);
        Self { width, height: used, pixels, glyphs, metrics, logo, white: (wx + 1, wy + 1) }
    }

    pub fn glyph(&self, font: Font, ch: char) -> Option<&Glyph> {
        self.glyphs.get(&(font, ch)).or_else(|| self.glyphs.get(&(font, '?')))
    }

    pub fn metrics(&self, font: Font) -> Metrics {
        self.metrics.get(&font).copied().unwrap_or_default()
    }

    /// The width of a line of text.
    pub fn measure(&self, font: Font, text: &str) -> f32 {
        text.chars().filter_map(|c| self.glyph(font, c)).map(|g| g.advance).sum()
    }
}

/// Copy a pixmap (premultiplied) into the atlas, un-premultiplied; `mask` keeps only the
/// coverage, white.
fn blit(dst: &mut [u8], width: u32, x: u32, y: u32, pm: &Pixmap, mask: bool) {
    for (i, px) in pm.pixels().iter().enumerate() {
        let (px_x, px_y) = (i as u32 % pm.width(), i as u32 / pm.width());
        let o = (((y + px_y) * width + x + px_x) * 4) as usize;
        let c = px.demultiply();
        let rgba = if mask { [255, 255, 255, c.alpha()] } else { [c.red(), c.green(), c.blue(), c.alpha()] };
        dst[o..o + 4].copy_from_slice(&rgba);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_has_glyphs_metrics_and_the_logo() {
        let a = Atlas::new(1.0);
        assert!(a.height <= 1024 && a.height > 16);
        let g = a.glyph(Font::Body, 'Q').unwrap();
        assert!(g.w > 4 && g.h > 8 && g.advance > 5.0, "{g:?}");
        // Coverage was drawn.
        let mut lit = 0;
        for y in g.y..g.y + g.h {
            for x in g.x..g.x + g.w {
                lit += (a.pixels[((y * a.width + x) * 4 + 3) as usize] > 128) as u32;
            }
        }
        assert!(lit > 10);
        assert!(a.measure(Font::Mono, "000") > a.measure(Font::Mono, "0") * 2.5);
        assert!(a.glyph(Font::Body, ' ').unwrap().advance > 0.0);
        assert!(a.logo.w == 34 && a.logo.h == 34);
        assert!(a.metrics(Font::Title).line > a.metrics(Font::Small).line);
        // An unknown character falls back.
        assert!(a.glyph(Font::Body, '\u{10FFFF}').is_some());
    }

    #[test]
    fn scale_grows_glyphs() {
        let a = Atlas::new(1.0);
        let b = Atlas::new(2.0);
        let (ga, gb) = (a.glyph(Font::Body, 'M').unwrap(), b.glyph(Font::Body, 'M').unwrap());
        assert!(gb.advance > ga.advance * 1.9);
    }
}
