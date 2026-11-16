//! Ubisoft art for exports, fetched from the asset CDN once and cached.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::library::ART_BASE_URL;

/// Art file names come from product configs; accept only plain names.
fn check_name(file: &str) -> Result<()> {
    if file.is_empty()
        || file.starts_with('.')
        || !file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("unexpected art file name '{file}'");
    }
    Ok(())
}

/// `dir/<file>`, downloading it from the CDN if it isn't cached yet.
pub async fn fetch(dir: &Path, file: &str) -> Result<PathBuf> {
    check_name(file)?;
    let path = dir.join(file);
    if path.is_file() {
        return Ok(path);
    }
    let url = format!("{ART_BASE_URL}{file}");
    let bytes = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("fetch {url}"))?
        .bytes()
        .await?;
    super::write_atomic(&path, &bytes)?;
    Ok(path)
}

/// The game's `icon_image` as a PNG (desktop environments and Steam don't reliably read
/// .ico): `icons/<file>` is the cached original, `icons/<stem>.png` the largest image in
/// it. Returns the PNG path.
pub async fn icon_png(icons: &Path, file: &str) -> Result<PathBuf> {
    let src = fetch(icons, file).await?;
    let stem = Path::new(file).file_stem().context("icon name")?.to_string_lossy().into_owned();
    let png = icons.join(format!("{stem}.png"));
    if png != src && png.is_file() {
        return Ok(png);
    }
    if file.to_ascii_lowercase().ends_with(".png") {
        return Ok(src);
    }
    let bytes = std::fs::read(&src)?;
    let out = to_png(&bytes).with_context(|| format!("convert {} to PNG", src.display()))?;
    super::write_atomic(&png, &out)?;
    Ok(png)
}

/// Decode an .ico (largest entry) or other image and encode it as PNG.
pub fn to_png(bytes: &[u8]) -> Result<Vec<u8>> {
    let img = if bytes.starts_with(&[0, 0, 1, 0]) { largest_ico_entry(bytes)? } else { image::load_from_memory(bytes)? };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// The image crate's ICO decoder picks the entry it thinks is best; pick the largest
/// ourselves by rewriting the directory to hold just that entry.
fn largest_ico_entry(bytes: &[u8]) -> Result<image::DynamicImage> {
    if bytes.len() < 6 {
        bail!("truncated icon");
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let entry = |i: usize| -> Option<&[u8]> { bytes.get(6 + i * 16..6 + i * 16 + 16) };
    let size = |e: &[u8]| {
        let d = |b: u8| if b == 0 { 256u32 } else { b as u32 };
        (d(e[0]) * d(e[1]), e[6] as u32 | (e[7] as u32) << 8)
    };
    let best = (0..count).filter_map(|i| entry(i).map(|e| (i, size(e)))).max_by_key(|&(_, s)| s).map(|(i, _)| i);
    let Some(best) = best else { bail!("empty icon") };
    let e = entry(best).unwrap();
    let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
    let off = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
    let data = bytes.get(off..off + len).context("truncated icon")?;
    let mut one = vec![0, 0, 1, 0, 1, 0];
    one.extend_from_slice(&e[..12]);
    one.extend_from_slice(&22u32.to_le_bytes());
    one.extend_from_slice(data);
    Ok(image::load_from_memory_with_format(&one, image::ImageFormat::Ico)?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A two-entry .ico (16x16 red, 48x48 blue), built with the image crate.
    pub fn fixture_ico() -> Vec<u8> {
        use image::codecs::ico::{IcoEncoder, IcoFrame};
        let frame = |n: u32, rgba: [u8; 4]| {
            let img = image::RgbaImage::from_pixel(n, n, image::Rgba(rgba));
            IcoFrame::as_png(img.as_raw(), n, n, image::ExtendedColorType::Rgba8).unwrap()
        };
        let mut out = Vec::new();
        IcoEncoder::new(&mut out).encode_images(&[frame(16, [255, 0, 0, 255]), frame(48, [0, 0, 255, 255])]).unwrap();
        out
    }

    #[test]
    fn ico_to_png_picks_largest() {
        let png = to_png(&fixture_ico()).unwrap();
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), (48, 48));
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    /// Convert a real `icon_image` (downloaded by hand from the CDN):
    /// `QUIVER_ICO_CHECK=<file.ico> cargo test -p ubi-core real_icon -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_icon() {
        let path = std::env::var("QUIVER_ICO_CHECK").expect("set QUIVER_ICO_CHECK");
        let png = to_png(&std::fs::read(&path).unwrap()).unwrap();
        let img = image::load_from_memory(&png).unwrap();
        eprintln!("{path}: {}x{}", img.width(), img.height());
    }

    #[test]
    fn names() {
        assert!(check_name("48edec3d91f23947b02be534159b74d1.ico").is_ok());
        for bad in ["", "../x.png", ".hidden", "a/b.png", "x y.png"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn cached_icon_needs_no_network() {
        let dir = tempfile::tempdir().unwrap();
        let name = "48edec3d91f23947b02be534159b74d1.ico";
        std::fs::write(dir.path().join(name), fixture_ico()).unwrap();
        let png = icon_png(dir.path(), name).await.unwrap();
        assert_eq!(png, dir.path().join("48edec3d91f23947b02be534159b74d1.png"));
        assert!(png.is_file());
    }
}
