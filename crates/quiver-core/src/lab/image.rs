//! Image metrics for screenshots (generalised from the Pharos harness's `abtest`): whether a
//! frame shows anything, sharpness (standard deviation of the Laplacian, higher is
//! sharper), temporal noise (mean absolute difference between consecutive shots, lower
//! is more stable) and how much changed between two shots. All on grey levels 0–255, over
//! a crop given in pixels (`WxH+X+Y`) or as fractions of the frame (`0.25,0.25,0.5,0.5`),
//! so a crop survives a change of resolution.

use anyhow::{Context, Result, bail};
use image::GrayImage;
use serde::{Deserialize, Serialize};

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// A crop as given: pixels, or fractions of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Crop {
    Pixels(Rect),
    Fraction { x: f64, y: f64, w: f64, h: f64 },
}

impl Crop {
    /// `WxH+X+Y` (pixels) or `x,y,w,h` (fractions, 0–1).
    pub fn parse(s: &str) -> Result<Crop> {
        if s.contains(',') {
            let v: Vec<f64> = s.split(',').map(|p| p.trim().parse::<f64>()).collect::<std::result::Result<_, _>>().with_context(|| format!("crop '{s}'"))?;
            let [x, y, w, h] = v[..] else { bail!("crop '{s}': expected x,y,w,h") };
            if [x, y, w, h].iter().any(|v| !(0.0..=1.0).contains(v)) || w == 0.0 || h == 0.0 || x + w > 1.0 + 1e-9 || y + h > 1.0 + 1e-9 {
                bail!("crop '{s}': fractions must be within the frame");
            }
            return Ok(Crop::Fraction { x, y, w, h });
        }
        let (size, off) = s.split_once('+').with_context(|| format!("crop '{s}': expected WxH+X+Y"))?;
        let (w, h) = size.split_once('x').with_context(|| format!("crop '{s}': expected WxH+X+Y"))?;
        let (x, y) = off.split_once('+').with_context(|| format!("crop '{s}': expected WxH+X+Y"))?;
        let n = |v: &str| v.trim().parse::<u32>().with_context(|| format!("crop '{s}'"));
        Ok(Crop::Pixels(Rect { x: n(x)?, y: n(y)?, w: n(w)?, h: n(h)? }))
    }

    /// The crop on a `width`×`height` frame, clamped to it.
    pub fn rect(&self, width: u32, height: u32) -> Rect {
        let r = match *self {
            Crop::Pixels(r) => r,
            Crop::Fraction { x, y, w, h } => {
                let (fw, fh) = (f64::from(width), f64::from(height));
                Rect { x: (x * fw) as u32, y: (y * fh) as u32, w: (w * fw).round() as u32, h: (h * fh).round() as u32 }
            }
        };
        let x = r.x.min(width.saturating_sub(1));
        let y = r.y.min(height.saturating_sub(1));
        Rect { x, y, w: r.w.min(width - x).max(1), h: r.h.min(height - y).max(1) }
    }
}

/// Load an image file as grey levels.
pub fn load_gray(path: &std::path::Path) -> Result<GrayImage> {
    Ok(image::open(path).with_context(|| format!("read {}", path.display()))?.to_luma8())
}

pub fn crop(img: &GrayImage, c: Option<&Crop>) -> GrayImage {
    match c {
        None => img.clone(),
        Some(c) => {
            let r = c.rect(img.width(), img.height());
            image::imageops::crop_imm(img, r.x, r.y, r.w, r.h).to_image()
        }
    }
}

/// Mean and standard deviation of the grey levels.
pub fn mean_std(img: &GrayImage) -> (f64, f64) {
    let n = f64::from(img.width()) * f64::from(img.height());
    if n == 0.0 {
        return (0.0, 0.0);
    }
    let (mut s, mut s2) = (0.0f64, 0.0f64);
    for p in img.pixels() {
        let v = f64::from(p.0[0]);
        s += v;
        s2 += v * v;
    }
    let mean = s / n;
    (mean, (s2 / n - mean * mean).max(0.0).sqrt())
}

/// A frame that shows nothing yet: (almost) one flat colour. Loading screens with a logo
/// on black are *not* blank.
pub fn is_blank(img: &GrayImage) -> bool {
    mean_std(img).1 < 2.0
}

/// Sharpness: the standard deviation of the 4-neighbour Laplacian (higher is sharper).
pub fn sharpness(img: &GrayImage) -> f64 {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let px = |x: u32, y: u32| f64::from(img.get_pixel(x, y).0[0]);
    let (mut s, mut s2, mut n) = (0.0, 0.0, 0.0);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let l = px(x - 1, y) + px(x + 1, y) + px(x, y - 1) + px(x, y + 1) - 4.0 * px(x, y);
            s += l;
            s2 += l * l;
            n += 1.0;
        }
    }
    let mean = s / n;
    (s2 / n - mean * mean).max(0.0).sqrt()
}

/// Mean absolute difference between two images of the same size (0–255).
pub fn mean_abs_diff(a: &GrayImage, b: &GrayImage) -> Result<f64> {
    if a.dimensions() != b.dimensions() {
        bail!("images differ in size: {:?} vs {:?}", a.dimensions(), b.dimensions());
    }
    let n = f64::from(a.width()) * f64::from(a.height());
    let s: f64 = a.pixels().zip(b.pixels()).map(|(p, q)| (f64::from(p.0[0]) - f64::from(q.0[0])).abs()).sum();
    Ok(if n == 0.0 { 0.0 } else { s / n })
}

/// The share of pixels (0–1) that changed by more than `threshold` levels: did a click or a
/// key do anything?
pub fn changed_fraction(a: &GrayImage, b: &GrayImage, threshold: u8) -> Result<f64> {
    if a.dimensions() != b.dimensions() {
        bail!("images differ in size: {:?} vs {:?}", a.dimensions(), b.dimensions());
    }
    let n = f64::from(a.width()) * f64::from(a.height());
    let c = a.pixels().zip(b.pixels()).filter(|(p, q)| p.0[0].abs_diff(q.0[0]) > threshold).count();
    Ok(if n == 0.0 { 0.0 } else { c as f64 / n })
}

/// Metrics over a series of shots of the same view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeriesMetrics {
    pub shots: usize,
    pub mean_level: f64,
    pub sharpness: f64,
    /// Mean of consecutive shots' mean absolute differences (`None` for a single shot).
    pub temporal_noise: Option<f64>,
}

pub fn series(imgs: &[GrayImage]) -> Result<SeriesMetrics> {
    if imgs.is_empty() {
        bail!("no shots");
    }
    let n = imgs.len() as f64;
    let mean_level = imgs.iter().map(|i| mean_std(i).0).sum::<f64>() / n;
    let sharp = imgs.iter().map(sharpness).sum::<f64>() / n;
    let noise = if imgs.len() > 1 {
        let d: Vec<f64> = imgs.windows(2).map(|w| mean_abs_diff(&w[0], &w[1])).collect::<Result<_>>()?;
        Some(d.iter().sum::<f64>() / d.len() as f64)
    } else {
        None
    };
    Ok(SeriesMetrics { shots: imgs.len(), mean_level, sharpness: sharp, temporal_noise: noise })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    fn flat(w: u32, h: u32, v: u8) -> GrayImage {
        GrayImage::from_pixel(w, h, Luma([v]))
    }

    fn checker(w: u32, h: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]))
    }

    #[test]
    fn crops() {
        assert_eq!(Crop::parse("200x100+10+20").unwrap().rect(1920, 1080), Rect { x: 10, y: 20, w: 200, h: 100 });
        let f = Crop::parse("0.25,0.25,0.5,0.5").unwrap();
        assert_eq!(f.rect(2560, 1600), Rect { x: 640, y: 400, w: 1280, h: 800 });
        assert_eq!(f.rect(1280, 800), Rect { x: 320, y: 200, w: 640, h: 400 }, "same view at another resolution");
        assert_eq!(Crop::parse("5000x5000+100+100").unwrap().rect(640, 480), Rect { x: 100, y: 100, w: 540, h: 380 });
        assert!(Crop::parse("0.8,0,0.5,1").is_err() && Crop::parse("x").is_err());
    }

    #[test]
    fn metrics() {
        assert!(is_blank(&flat(64, 64, 0)) && !is_blank(&checker(64, 64)));
        assert!(sharpness(&checker(32, 32)) > 100.0);
        assert_eq!(sharpness(&flat(32, 32, 9)), 0.0);
        assert_eq!(mean_abs_diff(&flat(4, 4, 10), &flat(4, 4, 30)).unwrap(), 20.0);
        assert_eq!(changed_fraction(&flat(4, 4, 10), &flat(4, 4, 10), 8).unwrap(), 0.0);
        assert_eq!(changed_fraction(&flat(4, 4, 10), &flat(4, 4, 40), 8).unwrap(), 1.0);
        assert!(mean_abs_diff(&flat(4, 4, 0), &flat(5, 4, 0)).is_err());
        let s = series(&[flat(8, 8, 0), flat(8, 8, 10), flat(8, 8, 20)]).unwrap();
        assert_eq!((s.shots, s.temporal_noise), (3, Some(10.0)));
        assert_eq!(crop(&checker(10, 10), Some(&Crop::parse("4x4+2+2").unwrap())).dimensions(), (4, 4));
    }
}
