//! HDR in a lab instance (design/LAB.md §13): `display.hdr = true` (`quiver lab create …
//! --hdr`, `quiver lab edit N display.hdr=on`).
//!
//! - **The display.** gamescope's headless backend has no real output, so HDR is forced:
//!   [`GAMESCOPE_ARGS`] make its one output HDR10 PQ. Its screenshots stay 8-bit PNGs
//!   (tonemapped: 1000 nits and up clip to white), and a `.avif` path writes nothing
//!   headless (gamescope 3.17), so the faithful capture is the Sight layer's.
//! - **The game.** HDR surface formats only exist through gamescope's WSI layer
//!   (`ENABLE_GAMESCOPE_WSI=1` plus `GAMESCOPE_WAYLAND_DISPLAY`, which gamescope gives its own
//!   children but not a `podman exec`): without it DXVK and vkd3d-proton see only sRGB and
//!   make an SDR swapchain. `PROTON_ENABLE_HDR=1` (`DXVK_HDR=1` in Proton) for builds that
//!   still gate HDR on it. Measured with the D3D12 test program (HDR10 and scRGB): X11 with
//!   the WSI layer works; winewayland made no swapchain in the container.
//! - **Measuring.** [`stats`] decodes a capture in the swapchain's own format (the Sight
//!   layer's `capture-<n>.raw` + `.json`: A2B10G10R10 / A2R10G10B10 PQ, R16G16B16A16 scRGB,
//!   8-bit sRGB) to luminance in nits; [`png16`] writes it as a lossless 16-bit PNG.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// gamescope's flags for an HDR instance: HDR on, the headless output forced to HDR10 PQ.
pub const GAMESCOPE_ARGS: &str = "--hdr-enabled --hdr-debug-force-support --hdr-debug-force-output";

/// The game's environment in an HDR instance (before the manifest's, which can override).
pub fn launch_env() -> Vec<(String, String)> {
    vec![("ENABLE_GAMESCOPE_WSI".into(), "1".into()), ("PROTON_ENABLE_HDR".into(), "1".into())]
}

/// The root-window properties gamescope sets about HDR, for `xprop -root …`.
pub const GAMESCOPE_ATOMS: [&str; 4] = ["GAMESCOPE_HDR_OUTPUT_FEEDBACK", "GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK", "GAMESCOPE_COLOR_APP_HDR_METADATA_FEEDBACK", "GAMESCOPE_DISPLAY_SUPPORTS_HDR"];

/// What gamescope says (from `xprop -root` output).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GamescopeHdr {
    /// Its output is HDR (`GAMESCOPE_HDR_OUTPUT_FEEDBACK`).
    pub output_hdr: Option<bool>,
    /// The focused client presents HDR (`GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK`).
    pub app_hdr: Option<bool>,
    /// The EOTF of the client's HDR metadata infoframe (2: PQ), from
    /// `GAMESCOPE_COLOR_APP_HDR_METADATA_FEEDBACK`'s first byte. The rest of that property is
    /// not reliable (gamescope writes a byte array as 32-bit items through Xlib's `long`s).
    pub app_eotf: Option<u8>,
    /// `GAMESCOPE_DISPLAY_SUPPORTS_HDR` (0 headless, even forced).
    pub display_supports_hdr: Option<bool>,
    #[serde(default)]
    pub raw: BTreeMap<String, String>,
}

impl GamescopeHdr {
    pub fn parse(xprop: &str) -> GamescopeHdr {
        let mut g = GamescopeHdr::default();
        for line in xprop.lines() {
            let Some((name, value)) = line.split_once('=') else { continue };
            let name = name.split('(').next().unwrap_or("").trim();
            let value = value.trim();
            let nums: Vec<u64> = value.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            let flag = nums.first().map(|&v| v != 0);
            match name {
                "GAMESCOPE_HDR_OUTPUT_FEEDBACK" => g.output_hdr = flag,
                "GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK" => g.app_hdr = flag,
                "GAMESCOPE_COLOR_APP_HDR_METADATA_FEEDBACK" => g.app_eotf = nums.first().map(|&v| (v & 0xff) as u8),
                "GAMESCOPE_DISPLAY_SUPPORTS_HDR" => g.display_supports_hdr = flag,
                _ => continue,
            }
            g.raw.insert(name.to_owned(), value.to_owned());
        }
        g
    }
}

/// `VkFormat`s a capture can be in.
pub mod vk_format {
    pub const R8G8B8A8_UNORM: u32 = 37;
    pub const R8G8B8A8_SRGB: u32 = 43;
    pub const B8G8R8A8_UNORM: u32 = 44;
    pub const B8G8R8A8_SRGB: u32 = 50;
    pub const A8B8G8R8_UNORM_PACK32: u32 = 51;
    pub const A8B8G8R8_SRGB_PACK32: u32 = 57;
    pub const A2R10G10B10_UNORM_PACK32: u32 = 58;
    pub const A2B10G10R10_UNORM_PACK32: u32 = 64;
    pub const R16G16B16A16_SFLOAT: u32 = 97;
}

/// `VkColorSpaceKHR`s.
pub mod vk_color_space {
    pub const SRGB_NONLINEAR: u32 = 0;
    pub const EXTENDED_SRGB_LINEAR: u32 = 1_000_104_002;
    pub const HDR10_ST2084: u32 = 1_000_104_008;
    pub const HDR10_HLG: u32 = 1_000_104_012;
}

/// A short name for a colour space.
pub fn color_space_name(cs: u32) -> String {
    use vk_color_space::*;
    match cs {
        SRGB_NONLINEAR => "sRGB (SDR)".into(),
        EXTENDED_SRGB_LINEAR => "scRGB (extended sRGB linear, FP16 HDR)".into(),
        HDR10_ST2084 => "HDR10 (BT.2020 PQ)".into(),
        HDR10_HLG => "HLG (BT.2020)".into(),
        1_000_104_001 => "Display P3 nonlinear".into(),
        1_000_104_003 => "Display P3 linear".into(),
        1_000_104_007 => "BT.2020 linear".into(),
        1_000_104_014 => "extended sRGB nonlinear".into(),
        n => format!("colour space {n}"),
    }
}

pub fn is_hdr_color_space(cs: u32) -> bool {
    cs != vk_color_space::SRGB_NONLINEAR && cs != 1_000_104_001 && cs != 1_000_104_006
}

pub fn format_name(f: u32) -> String {
    use vk_format::*;
    match f {
        R8G8B8A8_UNORM => "R8G8B8A8_UNORM".into(),
        R8G8B8A8_SRGB => "R8G8B8A8_SRGB".into(),
        B8G8R8A8_UNORM => "B8G8R8A8_UNORM".into(),
        B8G8R8A8_SRGB => "B8G8R8A8_SRGB".into(),
        A8B8G8R8_UNORM_PACK32 => "A8B8G8R8_UNORM_PACK32".into(),
        A8B8G8R8_SRGB_PACK32 => "A8B8G8R8_SRGB_PACK32".into(),
        A2R10G10B10_UNORM_PACK32 => "A2R10G10B10_UNORM_PACK32".into(),
        A2B10G10R10_UNORM_PACK32 => "A2B10G10R10_UNORM_PACK32".into(),
        R16G16B16A16_SFLOAT => "R16G16B16A16_SFLOAT".into(),
        n => format!("format {n}"),
    }
}

/// SMPTE ST 2084 (PQ): code value (0..1) to nits.
pub fn pq_to_nits(e: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    10000.0 * ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

/// Nits to a PQ code value (0..1).
pub fn nits_to_pq(nits: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// IEEE half to f32.
pub fn f16_to_f32(h: u16) -> f32 {
    let s = u32::from(h >> 15) << 31;
    let e = (h >> 10) & 0x1f;
    let m = u32::from(h & 0x3ff);
    let bits = match e {
        0 if m == 0 => s,
        0 => {
            // Subnormal: normalise.
            let mut e = 127 - 15 + 1;
            let mut m = m;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            s | ((e as u32) << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => s | 0x7f80_0000 | (m << 13),
        _ => s | ((u32::from(e) + 127 - 15) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// SDR reference white (BT.2408), nits: how sRGB 1.0 is counted.
pub const SDR_WHITE_NITS: f64 = 203.0;

/// How a capture's pixels are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decode {
    /// 10-bit PQ BT.2020 (or HLG, approximated as PQ is wrong; treated as SDR-relative).
    Pq10 { r_shift: u32, b_shift: u32 },
    /// FP16 linear, 1.0 = 80 nits (scRGB), BT.709 primaries.
    Scrgb,
    /// 8-bit sRGB, 1.0 = [`SDR_WHITE_NITS`].
    Srgb8 { bgr: bool },
}

fn decoder(format: u32, cs: u32) -> Result<(Decode, usize)> {
    use vk_format::*;
    Ok(match format {
        A2B10G10R10_UNORM_PACK32 if cs == vk_color_space::HDR10_ST2084 => (Decode::Pq10 { r_shift: 0, b_shift: 20 }, 4),
        A2R10G10B10_UNORM_PACK32 if cs == vk_color_space::HDR10_ST2084 => (Decode::Pq10 { r_shift: 20, b_shift: 0 }, 4),
        R16G16B16A16_SFLOAT if cs == vk_color_space::EXTENDED_SRGB_LINEAR => (Decode::Scrgb, 8),
        R8G8B8A8_UNORM | R8G8B8A8_SRGB | A8B8G8R8_UNORM_PACK32 | A8B8G8R8_SRGB_PACK32 if cs == vk_color_space::SRGB_NONLINEAR => (Decode::Srgb8 { bgr: false }, 4),
        B8G8R8A8_UNORM | B8G8R8A8_SRGB if cs == vk_color_space::SRGB_NONLINEAR => (Decode::Srgb8 { bgr: true }, 4),
        // 10-bit SDR (sRGB colour space): as 8-bit sRGB, scaled.
        _ => bail!("can't measure {} in {}", format_name(format), color_space_name(cs)),
    })
}

/// A pixel's linear RGB in nits, in its own primaries (BT.2020 for PQ, BT.709 otherwise), and
/// its luminance.
fn pixel(d: Decode, px: &[u8]) -> ([f64; 3], f64) {
    match d {
        Decode::Pq10 { r_shift, b_shift } => {
            let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            let c = |s: u32| pq_to_nits(f64::from((v >> s) & 1023) / 1023.0);
            let (r, g, b) = (c(r_shift), c(10), c(b_shift));
            ([r, g, b], 0.2627 * r + 0.6780 * g + 0.0593 * b)
        }
        Decode::Scrgb => {
            let c = |i: usize| f64::from(f16_to_f32(u16::from_le_bytes([px[2 * i], px[2 * i + 1]]))).max(0.0) * 80.0;
            let (r, g, b) = (c(0), c(1), c(2));
            let (r, g, b) = (if r.is_finite() { r } else { 0.0 }, if g.is_finite() { g } else { 0.0 }, if b.is_finite() { b } else { 0.0 });
            ([r, g, b], 0.2126 * r + 0.7152 * g + 0.0722 * b)
        }
        Decode::Srgb8 { bgr } => {
            let (r, b) = if bgr { (px[2], px[0]) } else { (px[0], px[2]) };
            let c = |v: u8| srgb_to_linear(f64::from(v) / 255.0) * SDR_WHITE_NITS;
            let (r, g, b) = (c(r), c(px[1]), c(b));
            ([r, g, b], 0.2126 * r + 0.7152 * g + 0.0722 * b)
        }
    }
}

/// Per-image numbers, in nits (luminance of each pixel).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HdrStats {
    pub pixels: u64,
    pub max_nits: f64,
    pub avg_nits: f64,
    pub p99_nits: f64,
    /// Fractions of the pixels above 203 nits (SDR reference white) and above 1000.
    pub above_203: f64,
    pub above_1000: f64,
    /// The brightest channel anywhere, nits (saturated highlights exceed the luminance).
    pub max_channel_nits: f64,
    /// How the values were read.
    pub decode: String,
}

/// The capture's stats (`raw`: tightly packed rows of `width` pixels in `format`).
pub fn stats(raw: &[u8], format: u32, color_space: u32, width: u32, height: u32) -> Result<HdrStats> {
    let (d, bpp) = decoder(format, color_space)?;
    let n = width as usize * height as usize;
    if raw.len() < n * bpp {
        bail!("capture is {} bytes, expected {} for {width}x{height}", raw.len(), n * bpp);
    }
    // A histogram for the percentile: 0.5-nit bins to 10 000 nits, and above.
    let mut hist = vec![0u64; 20_001];
    let (mut sum, mut max, mut maxc, mut a203, mut a1000) = (0f64, 0f64, 0f64, 0u64, 0u64);
    for px in raw[..n * bpp].chunks_exact(bpp) {
        let (rgb, y) = pixel(d, px);
        sum += y;
        max = max.max(y);
        maxc = maxc.max(rgb[0].max(rgb[1]).max(rgb[2]));
        a203 += u64::from(y > 203.0);
        a1000 += u64::from(y > 1000.0);
        hist[((y * 2.0) as usize).min(20_000)] += 1;
    }
    let target = (n as f64 * 0.99).ceil() as u64;
    let (mut acc, mut p99) = (0u64, 0f64);
    for (i, c) in hist.iter().enumerate() {
        acc += c;
        if acc >= target {
            p99 = i as f64 / 2.0;
            break;
        }
    }
    let nf = n.max(1) as f64;
    Ok(HdrStats {
        pixels: n as u64,
        max_nits: max,
        avg_nits: sum / nf,
        p99_nits: p99,
        above_203: a203 as f64 / nf,
        above_1000: a1000 as f64 / nf,
        max_channel_nits: maxc,
        decode: match d {
            Decode::Pq10 { .. } => "PQ (ST 2084), BT.2020 luminance".into(),
            Decode::Scrgb => "scRGB linear (1.0 = 80 nits), BT.709 luminance".into(),
            Decode::Srgb8 { .. } => format!("sRGB, 1.0 = {SDR_WHITE_NITS} nits (SDR reference white)"),
        },
    })
}

/// The capture as a 16-bit RGB PNG. HDR10: the PQ code values themselves (lossless, BT.2020
/// PQ). scRGB: converted to the same BT.2020 PQ encoding (a derived image: the `.raw` is the
/// exact one). SDR: the sRGB values.
pub fn png16(raw: &[u8], format: u32, color_space: u32, width: u32, height: u32) -> Result<image::ImageBuffer<image::Rgb<u16>, Vec<u16>>> {
    let (d, bpp) = decoder(format, color_space)?;
    let n = width as usize * height as usize;
    if raw.len() < n * bpp {
        bail!("capture is {} bytes, expected {}", raw.len(), n * bpp);
    }
    // BT.709 to BT.2020 (linear).
    const M: [[f64; 3]; 3] = [[0.6274, 0.3293, 0.0433], [0.0691, 0.9195, 0.0114], [0.0164, 0.0880, 0.8956]];
    let mut out = Vec::with_capacity(n * 3);
    for px in raw[..n * bpp].chunks_exact(bpp) {
        match d {
            Decode::Pq10 { r_shift, b_shift } => {
                let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                // 10 to 16 bits, exactly invertible (bit replication).
                let c = |s: u32| {
                    let x = ((v >> s) & 1023) as u16;
                    (x << 6) | (x >> 4)
                };
                out.extend([c(r_shift), c(10), c(b_shift)]);
            }
            Decode::Scrgb => {
                let (rgb, _) = pixel(d, px);
                for row in M {
                    let l = row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2];
                    out.push((nits_to_pq(l) * 65535.0).round() as u16);
                }
            }
            Decode::Srgb8 { bgr } => {
                let (r, b) = if bgr { (px[2], px[0]) } else { (px[0], px[2]) };
                out.extend([r, px[1], b].map(|v| u16::from(v) * 257));
            }
        }
    }
    image::ImageBuffer::from_raw(width, height, out).ok_or_else(|| anyhow::anyhow!("image size"))
}

/// The Sight layer's capture sidecar (`capture-<n>.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureMeta {
    pub id: u64,
    pub format: u32,
    #[serde(default)]
    pub format_name: String,
    pub color_space: u32,
    #[serde(default)]
    pub color_space_name: String,
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    #[serde(default)]
    pub row_bytes: u32,
    #[serde(default)]
    pub swapchain: u32,
    #[serde(default)]
    pub hdr_metadata: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_round_trips() {
        for nits in [0.0, 0.005, 1.0, 100.0, 203.0, 1000.0, 4000.0, 10000.0] {
            let e = nits_to_pq(nits);
            assert!((pq_to_nits(e) - nits).abs() < 1e-6 * nits.max(1.0), "{nits}");
        }
        // The test program's code values.
        assert!((pq_to_nits(0.58069) - 203.0).abs() < 0.5);
        assert!((pq_to_nits(0.75183) - 1000.0).abs() < 2.0);
        assert!((pq_to_nits(0.90256) - 4000.0).abs() < 10.0);
    }

    #[test]
    fn half_floats() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x4a40), 12.5);
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        assert!(f16_to_f32(0x7c00).is_infinite());
    }

    fn pq10(r: f64, g: f64, b: f64) -> [u8; 4] {
        let c = |n: f64| (nits_to_pq(n) * 1023.0).round() as u32;
        (c(r) | (c(g) << 10) | (c(b) << 20) | (3 << 30)).to_le_bytes()
    }

    #[test]
    fn hdr10_stats() {
        // 4 pixels: two at 203 nits grey, one at 1000, one at 4000.
        let raw: Vec<u8> = [pq10(203.0, 203.0, 203.0), pq10(203.0, 203.0, 203.0), pq10(1000.0, 1000.0, 1000.0), pq10(4000.0, 4000.0, 4000.0)].concat();
        let s = stats(&raw, vk_format::A2B10G10R10_UNORM_PACK32, vk_color_space::HDR10_ST2084, 2, 2).unwrap();
        assert!((s.max_nits - 4000.0).abs() < 20.0, "{s:?}");
        assert!((s.avg_nits - (203.0 * 2.0 + 1000.0 + 4000.0) / 4.0).abs() < 10.0, "{s:?}");
        // 203 itself rounds to a code value just either side: only the 1000 and 4000 count.
        assert!(s.above_1000 > 0.24 && s.above_1000 < 0.26, "{s:?}");
        assert!(s.above_203 >= 0.5 && s.above_203 <= 1.0, "{s:?}");
        // Lossless 16-bit: the top 10 bits are the code values.
        let png = png16(&raw, vk_format::A2B10G10R10_UNORM_PACK32, vk_color_space::HDR10_ST2084, 2, 2).unwrap();
        let v = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        assert_eq!(u32::from(png.get_pixel(1, 1)[0] >> 6), v & 1023);
    }

    #[test]
    fn scrgb_stats() {
        let px = |v: f32| {
            // f32 to f16 for values that are exact in half.
            let h: u16 = [(0.0, 0), (2.5, 0x4100), (12.5, 0x4a40), (50.0, 0x5240)].iter().find(|(f, _)| *f == v).map(|(_, h)| *h).unwrap();
            [h, h, h, 0x3c00].iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()
        };
        let raw = [px(2.5), px(12.5), px(50.0), px(0.0)].concat();
        let s = stats(&raw, vk_format::R16G16B16A16_SFLOAT, vk_color_space::EXTENDED_SRGB_LINEAR, 4, 1).unwrap();
        assert!((s.max_nits - 4000.0).abs() < 1.0, "{s:?}");
        assert_eq!(s.above_1000, 0.25);
        assert_eq!(s.above_203, 0.5);
        assert!(png16(&raw, vk_format::R16G16B16A16_SFLOAT, vk_color_space::EXTENDED_SRGB_LINEAR, 4, 1).is_ok());
    }

    #[test]
    fn sdr_is_relative_to_reference_white() {
        let raw = [[255u8, 255, 255, 255], [0, 0, 0, 255]].concat();
        let s = stats(&raw, vk_format::B8G8R8A8_UNORM, vk_color_space::SRGB_NONLINEAR, 2, 1).unwrap();
        assert!((s.max_nits - 203.0).abs() < 0.01 && s.above_203 == 0.0, "{s:?}");
        assert!(stats(&raw, vk_format::B8G8R8A8_UNORM, vk_color_space::HDR10_ST2084, 2, 1).is_err());
    }

    #[test]
    fn gamescope_atoms() {
        let x = "GAMESCOPE_COLOR_APP_HDR_METADATA_FEEDBACK(CARDINAL) = 2319974402, 429300650, 65552450, 600, 0, 145, 1\n\
                 GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK(CARDINAL) = 1\nGAMESCOPE_HDR_OUTPUT_FEEDBACK(CARDINAL) = 1\n\
                 GAMESCOPE_DISPLAY_SUPPORTS_HDR(CARDINAL) = 0\nGAMESCOPE_FOCUSED_WINDOW(CARDINAL) = 4\n";
        let g = GamescopeHdr::parse(x);
        assert_eq!((g.output_hdr, g.app_hdr, g.app_eotf, g.display_supports_hdr), (Some(true), Some(true), Some(2), Some(false)));
        assert_eq!(g.raw.len(), 4);
        let none = GamescopeHdr::parse("GAMESCOPE_HDR_OUTPUT_FEEDBACK:  not found.\n");
        assert_eq!(none.output_hdr, None);
    }
}
