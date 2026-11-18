//! Art for exports, fetched once and cached: Ubisoft's product-config files from its asset
//! CDN (cached under their own names), and full URLs (Ubisoft's games API, Epic's catalog;
//! cached as `<sha1 of the URL>.png|jpg`).
//!
//! Each slot takes the first of: the product config's file, the games API's image, Epic's
//! catalog image ([`pick`]), the order the app's art uses too.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use quiver_api::Art;
use quiver_api::art::{ArtRef, ArtSlot};

use crate::library::ART_BASE_URL;

/// The image an export uses for `slot`: the product config's file, else Ubisoft's games
/// API image, else Epic's catalog image (all an Epic game has).
pub fn pick(art: &Art, slot: ArtSlot) -> Option<ArtRef> {
    let (config, space, epic) = match slot {
        ArtSlot::Tile => (&art.thumb, &art.space.tile, &art.epic.tile),
        ArtSlot::Hero => (&art.background, &art.space.hero, &art.epic.hero),
        ArtSlot::Logo => (&art.logo, &art.space.logo, &art.epic.logo),
        ArtSlot::Icon => (&art.icon, &art.space.icon, &art.epic.icon),
    };
    let url = |u: &Option<String>| u.as_ref().filter(|u| u.starts_with("https://")).map(|u| ArtRef::Url(u.clone()));
    config.as_ref().filter(|f| !f.is_empty()).map(|f| ArtRef::Asset(f.clone())).or_else(|| url(space)).or_else(|| url(epic))
}

/// The cache name of a URL's image, without its extension.
pub fn url_stem(url: &str) -> String {
    super::hash(url.as_bytes())
}

/// `dir/<url_stem>.png|jpg`, downloading it on first use. PNG and JPEG are kept as they
/// come (Steam's grid takes only those); anything else is converted to PNG.
pub async fn fetch_url(dir: &Path, url: &str) -> Result<PathBuf> {
    if !url.starts_with("https://") {
        bail!("unexpected art URL '{url}'");
    }
    let stem = url_stem(url);
    for ext in ["png", "jpg"] {
        let p = dir.join(format!("{stem}.{ext}"));
        if p.is_file() {
            return Ok(p);
        }
    }
    let bytes = reqwest::get(url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("fetch {url}"))?
        .bytes()
        .await?;
    let (ext, bytes) = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => ("png", bytes.to_vec()),
        Ok(image::ImageFormat::Jpeg) => ("jpg", bytes.to_vec()),
        _ => ("png", to_png(&bytes).with_context(|| format!("convert {url} to PNG"))?),
    };
    let path = dir.join(format!("{stem}.{ext}"));
    super::write_atomic(&path, &bytes)?;
    Ok(path)
}

/// The image for `r`, cached under `dir` (a local file is used where it is).
pub async fn fetch_ref(dir: &Path, r: &ArtRef) -> Result<PathBuf> {
    match r {
        ArtRef::Asset(file) => fetch(dir, file).await,
        ArtRef::Url(url) => fetch_url(dir, url).await,
        ArtRef::File(p) if p.is_file() => Ok(p.clone()),
        ArtRef::File(p) => bail!("{} not found", p.display()),
    }
}

/// Icon edge for icons made from other art.
const ICON_SIZE: u32 = 256;

/// The export's icon, as a PNG under `icons`: the config's `icon_image`; else a square image
/// (games API avatar, a catalog icon); else the portrait box art, fitted onto a transparent
/// square (Epic's catalog has no icons). `None` without any art.
pub async fn export_icon(icons: &Path, art_dir: &Path, art: &Art) -> Result<Option<PathBuf>> {
    let (src, stem) = match pick(art, ArtSlot::Icon) {
        Some(ArtRef::Asset(file)) => return icon_png(icons, &file).await.map(Some),
        Some(r) => (fetch_ref(icons, &r).await?, None),
        None => match pick(art, ArtSlot::Tile) {
            Some(r) => (fetch_ref(art_dir, &r).await?, Some("icon")),
            None => return Ok(None),
        },
    };
    let name = src.file_stem().context("icon name")?.to_string_lossy().into_owned();
    let png = icons.join(match stem {
        Some(s) => format!("{name}-{s}.png"),
        None => format!("{name}-square.png"),
    });
    if png.is_file() {
        return Ok(Some(png));
    }
    let bytes = std::fs::read(&src)?;
    let out = square_png(&bytes, ICON_SIZE).with_context(|| format!("make an icon of {}", src.display()))?;
    super::write_atomic(&png, &out)?;
    Ok(Some(png))
}

/// The image scaled to fit a `size`×`size` transparent square, centred, as PNG.
pub fn square_png(bytes: &[u8], size: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(bytes)?;
    let fit = img.resize(size, size, image::imageops::FilterType::Lanczos3).to_rgba8();
    let mut canvas = image::RgbaImage::new(size, size);
    let (x, y) = ((size - fit.width()) / 2, (size - fit.height()) / 2);
    image::imageops::overlay(&mut canvas, &fit, x as i64, y as i64);
    let mut out = std::io::Cursor::new(Vec::new());
    canvas.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

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

    /// A `w`×`h` opaque PNG.
    pub fn fixture_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn picks_config_then_games_api_then_epic() {
        let mut art = Art { epic: quiver_api::art::SpaceArt { tile: Some("https://e/t.jpg".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(pick(&art, ArtSlot::Tile), Some(ArtRef::Url("https://e/t.jpg".into())));
        assert_eq!(pick(&art, ArtSlot::Hero), None);
        art.space.tile = Some("https://u/t.jpg".into());
        assert_eq!(pick(&art, ArtSlot::Tile), Some(ArtRef::Url("https://u/t.jpg".into())));
        art.thumb = Some("c.jpg".into());
        assert_eq!(pick(&art, ArtSlot::Tile), Some(ArtRef::Asset("c.jpg".into())));
        // Only https URLs, and no empty file names.
        let art = Art { thumb: Some(String::new()), epic: quiver_api::art::SpaceArt { tile: Some("file:///etc/x".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(pick(&art, ArtSlot::Tile), None);
    }

    #[tokio::test]
    async fn icon_from_box_art_and_cached_urls() {
        let dir = tempfile::tempdir().unwrap();
        let (icons, art_dir) = (dir.path().join("icons"), dir.path().join("art"));
        std::fs::create_dir_all(&art_dir).unwrap();
        assert_eq!(export_icon(&icons, &art_dir, &Art::default()).await.unwrap(), None);
        let url = "https://cdn1.epicgames.com/x/tall";
        std::fs::write(art_dir.join(format!("{}.png", url_stem(url))), fixture_png(60, 90)).unwrap();
        // A cached URL is used without fetching.
        assert_eq!(fetch_url(&art_dir, url).await.unwrap(), art_dir.join(format!("{}.png", url_stem(url))));
        assert!(fetch_url(&art_dir, "http://x/y.png").await.is_err());
        let art = Art { epic: quiver_api::art::SpaceArt { tile: Some(url.into()), ..Default::default() }, ..Default::default() };
        let icon = export_icon(&icons, &art_dir, &art).await.unwrap().unwrap();
        assert_eq!(icon, icons.join(format!("{}-icon.png", url_stem(url))));
        let img = image::open(&icon).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (256, 256));
        // Fitted, not stretched: transparent at the sides, the art in the middle.
        assert_eq!(img.get_pixel(5, 128).0[3], 0);
        assert_eq!(img.get_pixel(128, 128).0, [10, 200, 30, 255]);
        // A config icon wins (and stays as before: `<stem>.png`).
        let name = "48edec3d91f23947b02be534159b74d1.ico";
        std::fs::write(icons.join(name), fixture_ico()).unwrap();
        let art = Art { icon: Some(name.into()), ..art };
        assert_eq!(export_icon(&icons, &art_dir, &art).await.unwrap(), Some(icons.join("48edec3d91f23947b02be534159b74d1.png")));
    }

    #[test]
    fn ico_to_png_picks_largest() {
        let png = to_png(&fixture_ico()).unwrap();
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), (48, 48));
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    /// Convert a real `icon_image` (downloaded by hand from the CDN):
    /// `QUIVER_ICO_CHECK=<file.ico> cargo test -p quiver-core real_icon -- --ignored --nocapture`
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
