//! Game art: Ubisoft's product-config art from the public asset CDN, art URLs from
//! Ubisoft's games API and Steam's CDN (all cached on disk under `$XDG_CACHE_HOME/quiver/art`),
//! Steam's local library cache, or a local image file the user picked for a game. Which
//! one a game shows is [`candidates`]: the user's file, then `quiver_api::art::candidates`
//! (Ubisoft's config, Ubisoft's games API, Steam's cache, Steam's CDN), the first that
//! loads.
//!
//! Loads run on the tokio runtime; the store notifies GPUI when one lands. Views ask for
//! art every frame with [`ArtStore::get`] and draw a placeholder until it's `Ready`, or
//! for good if it `Failed`. Two derived images are made once per file and cached next to
//! it: logos with their transparent edges trimmed, and a small blurred "ambient" copy of
//! hero art that game pages tint themselves with (GPUI has no backdrop blur).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use gpui::Context;
use serde::Deserialize;

pub const CDN: &str = "https://ubistatic3-a.akamaihd.net/orbit/uplay_launcher_3_0/assets/";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtSource {
    /// A file name from a product config, served by Ubisoft's asset CDN.
    Cdn(String),
    /// A full URL on a known art host ([`ART_HOSTS`]): Ubisoft's games API, Steam's CDN.
    Url(String),
    /// An image file on this machine: one the user chose, or Steam's library cache.
    Local(PathBuf),
}

/// Hosts [`ArtSource::Url`] may download from.
pub const ART_HOSTS: &[&str] = &[
    "ubiservices.cdn.ubi.com",
    "static-live.ubisoft.com",
    "shared.steamstatic.com",
    "cdn.cloudflare.steamstatic.com",
    "cdn.akamai.steamstatic.com",
    "steamcdn-a.akamaihd.net",
    // Epic's catalog keyImages.
    "cdn1.epicgames.com",
    "cdn2.epicgames.com",
    "cdn2.unrealengine.com",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    Original,
    /// Transparent edges trimmed (logos).
    Trimmed,
    /// 96 px wide, blurred (hero ambience).
    Ambient,
    /// Cropped to the 240:280 box-art ratio (GPUI doesn't clip `ObjectFit::Cover`, so tiles
    /// need images of exactly their shape; custom art can be any shape).
    Tile,
    /// `Tile`, desaturated and darkened, for games that aren't installed (GPUI only has
    /// full grayscale).
    TileDimmed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtImage {
    pub path: Arc<Path>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
enum Slot {
    Loading,
    Ready(ArtImage),
    Failed(Instant),
    /// Failed before and being tried again: still reported as failed, so a fallback that
    /// is showing doesn't flicker back to a placeholder while the retry runs.
    Retrying,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArtState {
    Loading,
    Ready(ArtImage),
    Failed,
}

/// Failed loads are retried after this long (the next time something asks).
const RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_CONCURRENT: usize = 6;

pub fn default_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("quiver/art")
}

/// Fetches and caches art. Shared by the loader tasks; cheap to clone.
#[derive(Clone)]
pub struct Fetcher {
    cache_dir: PathBuf,
    base_url: String,
    http: reqwest::Client,
    limit: Arc<tokio::sync::Semaphore>,
    /// Hosts URL art may come from, and whether plain `http` is allowed (tests only).
    hosts: Arc<Vec<String>>,
    allow_http: bool,
    /// One lock per file being downloaded, so concurrent requests for the same file (its
    /// hero and its ambient copy, say) download it once.
    inflight: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temp name next to `path` that no other writer in this or another process uses.
fn tmp_path(path: &Path) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.{n}.part", std::process::id()))
}

impl Fetcher {
    pub fn new(cache_dir: PathBuf, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("Quiver/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .build()
            .expect("http client");
        Self {
            cache_dir,
            base_url: base_url.into(),
            http,
            limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            inflight: Arc::default(),
            hosts: Arc::new(ART_HOSTS.iter().map(|h| h.to_string()).collect()),
            allow_http: false,
        }
    }

    /// Allow URL art from `host` over plain http too (test servers).
    #[cfg(test)]
    pub fn allow_test_host(mut self, host: &str) -> Self {
        let mut hosts = (*self.hosts).clone();
        hosts.push(host.to_owned());
        self.hosts = Arc::new(hosts);
        self.allow_http = true;
        self
    }

    /// Where a URL's image is cached, relative to the cache dir: `url/<host>/<path>` for
    /// plain image paths, else `url/<host>/h/<sha1 of the URL>` with the extension the
    /// downloaded image turns out to have (Epic's CDN names files with spaces, `&` and no
    /// extension). `None` for URLs Quiver won't fetch (unknown host, http, a query).
    fn url_cache(&self, url: &str) -> Option<UrlCache> {
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://").filter(|_| self.allow_http))?;
        let (host, path) = rest.split_once('/')?;
        if !self.hosts.iter().any(|h| h == host) || path.is_empty() || path.contains(['?', '#', '\\']) || path.split('/').any(|s| s == "..") || path.len() > 1024 {
            return None;
        }
        let base = PathBuf::from("url").join(host.replace(':', "_"));
        let segs: Vec<&str> = path.split('/').collect();
        let ok = |s: &&str| {
            !s.is_empty() && s.len() <= 128 && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        let ext = segs.last().and_then(|l| l.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
        if segs.len() <= 8 && segs.iter().all(ok) && ext.as_deref().is_some_and(|e| matches!(e, "jpg" | "jpeg" | "png" | "webp")) {
            let mut rel = base;
            rel.extend(segs);
            return Some(UrlCache::Exact(rel));
        }
        use sha1::Digest as _;
        let hash = hex::encode(sha1::Sha1::digest(url.as_bytes()));
        Some(UrlCache::Hashed(base.join("h").join(hash)))
    }

    #[cfg(test)]
    fn url_cache_rel(&self, url: &str) -> Option<PathBuf> {
        match self.url_cache(url)? {
            UrlCache::Exact(p) | UrlCache::Hashed(p) => Some(p),
        }
    }

    /// Download `url` to `path` (once, however many ask at the same time).
    async fn download(&self, key: &str, url: &str, path: &Path) -> Result<PathBuf> {
        if path.is_file() {
            return Ok(path.to_owned());
        }
        let lock = self.inflight.lock().unwrap().entry(key.to_owned()).or_default().clone();
        let _guard = lock.lock().await;
        if path.is_file() {
            return Ok(path.to_owned()); // another task got it while we waited
        }
        let _permit = self.limit.acquire().await?;
        let resp = self.http.get(url).send().await.with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url}: {}", resp.status());
        }
        let bytes = resp.bytes().await.with_context(|| format!("GET {url}"))?;
        // Only cache things that are actually images.
        image::guess_format(&bytes).with_context(|| format!("{url} is not an image"))?;
        tokio::fs::create_dir_all(path.parent().unwrap_or(&self.cache_dir)).await?;
        let tmp = tmp_path(path);
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, path).await?;
        self.inflight.lock().unwrap().remove(key);
        Ok(path.to_owned())
    }

    /// Download `url` to `<stem>.<ext>`, the extension from the image's own bytes.
    async fn download_sniffed(&self, url: &str, stem: &Path) -> Result<PathBuf> {
        let key = stem.to_string_lossy().into_owned();
        let lock = self.inflight.lock().unwrap().entry(key.clone()).or_default().clone();
        let _guard = lock.lock().await;
        if let Some(p) = hashed_file(stem) {
            return Ok(p);
        }
        let _permit = self.limit.acquire().await?;
        let resp = self.http.get(url).send().await.with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url}: {}", resp.status());
        }
        let bytes = resp.bytes().await.with_context(|| format!("GET {url}"))?;
        let ext = match image::guess_format(&bytes).with_context(|| format!("{url} is not an image"))? {
            image::ImageFormat::Jpeg => "jpg",
            image::ImageFormat::Png => "png",
            image::ImageFormat::WebP => "webp",
            f => bail!("{url}: {f:?} images aren't used"),
        };
        let path = stem.with_extension(ext);
        tokio::fs::create_dir_all(path.parent().unwrap_or(&self.cache_dir)).await?;
        let tmp = tmp_path(&path);
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        self.inflight.lock().unwrap().remove(&key);
        Ok(path)
    }

    /// The original file on disk, downloading it first if it isn't cached.
    async fn original(&self, src: &ArtSource) -> Result<PathBuf> {
        match src {
            ArtSource::Local(p) => {
                if !p.is_file() {
                    bail!("{} does not exist", p.display());
                }
                Ok(p.clone())
            }
            ArtSource::Cdn(file) => {
                if !valid_cdn_name(file) {
                    bail!("refusing art file name {file:?}");
                }
                let url = format!("{}{}", self.base_url, file);
                self.download(file, &url, &self.cache_dir.join(file)).await
            }
            ArtSource::Url(url) => match self.url_cache(url).with_context(|| format!("refusing art URL {url:?}"))? {
                UrlCache::Exact(rel) => self.download(url, url, &self.cache_dir.join(rel)).await,
                UrlCache::Hashed(stem) => {
                    let stem = self.cache_dir.join(stem);
                    if let Some(p) = hashed_file(&stem) {
                        return Ok(p);
                    }
                    self.download_sniffed(url, &stem).await
                }
            },
        }
    }

    fn derived_path(&self, src: &ArtSource, original: &Path, variant: Variant) -> PathBuf {
        let stem = match src {
            ArtSource::Cdn(file) => file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).to_owned(),
            ArtSource::Url(url) => match self.url_cache(url) {
                Some(UrlCache::Hashed(stem)) => format!("url-h-{}", stem.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()),
                other => {
                    let rel = match other {
                        Some(UrlCache::Exact(p)) => p,
                        _ => PathBuf::new(),
                    };
                    let flat: Vec<String> = rel.iter().skip(1).map(|s| s.to_string_lossy().into_owned()).collect();
                    let flat = flat.join("_");
                    format!("url-{}", flat.rsplit_once('.').map(|(s, _)| s).unwrap_or(&flat))
                }
            },
            ArtSource::Local(p) => {
                // Key local files on path + mtime, so replacing the file refreshes it.
                let mut h = std::collections::hash_map::DefaultHasher::new();
                p.hash(&mut h);
                std::fs::metadata(original).and_then(|m| m.modified()).ok().hash(&mut h);
                format!("local-{:016x}", h.finish())
            }
        };
        let name = match variant {
            Variant::Original => unreachable!(),
            Variant::Trimmed => format!("{stem}.trim.png"),
            Variant::Ambient => format!("{stem}.amb.jpg"),
            Variant::Tile => format!("{stem}.tile.jpg"),
            Variant::TileDimmed => format!("{stem}.tile-dim.jpg"),
        };
        self.cache_dir.join("derived").join(name)
    }

    pub async fn load(&self, src: ArtSource, variant: Variant) -> Result<ArtImage> {
        let original = self.original(&src).await?;
        let path = match variant {
            Variant::Original => original,
            v => {
                let out = self.derived_path(&src, &original, v);
                if !out.is_file() {
                    let out2 = out.clone();
                    tokio::task::spawn_blocking(move || derive(&original, &out2, v)).await??;
                }
                out
            }
        };
        let p = path.clone();
        let (width, height) = tokio::task::spawn_blocking(move || image::image_dimensions(&p))
            .await?
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(ArtImage { path: path.into(), width, height })
    }
}

/// Where a URL's image lives in the cache.
enum UrlCache {
    /// At this path (relative to the cache dir), as named by the URL.
    Exact(PathBuf),
    /// At this stem plus the image's own extension.
    Hashed(PathBuf),
}

/// The cached file for a hashed URL, whatever its extension.
fn hashed_file(stem: &Path) -> Option<PathBuf> {
    ["jpg", "png", "webp"].iter().map(|e| stem.with_extension(e)).find(|p| p.is_file())
}

/// CDN art names are content hashes like `79b8e5e2468fb3f434b622ee5c5063fb.jpg`.
pub fn valid_cdn_name(file: &str) -> bool {
    let Some((stem, ext)) = file.rsplit_once('.') else { return false };
    !stem.is_empty()
        && stem.len() <= 128
        && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp")
}

fn derive(original: &Path, out: &Path, variant: Variant) -> Result<()> {
    let img = image::open(original).with_context(|| format!("decoding {}", original.display()))?;
    let result = match variant {
        Variant::Trimmed => image::DynamicImage::ImageRgba8(trim_transparent(&img.to_rgba8())),
        Variant::Ambient => {
            let small = img.thumbnail(96, 96);
            image::DynamicImage::ImageRgb8(image::imageops::blur(&small.to_rgb8(), 5.0))
        }
        Variant::Tile => image::DynamicImage::ImageRgb8(tile(&img).to_rgb8()),
        Variant::TileDimmed => image::DynamicImage::ImageRgb8(dim(&tile(&img).to_rgb8())),
        Variant::Original => return Ok(()),
    };
    std::fs::create_dir_all(out.parent().unwrap())?;
    let tmp = tmp_path(out);
    let format = if variant == Variant::Trimmed { image::ImageFormat::Png } else { image::ImageFormat::Jpeg };
    result.save_with_format(&tmp, format)?;
    std::fs::rename(&tmp, out)?;
    Ok(())
}

/// Center-crop to 240:280 and cap the size at 480×560 (2x the tile at its largest).
pub fn tile(img: &image::DynamicImage) -> image::DynamicImage {
    let (w, h) = (img.width().max(1), img.height().max(1));
    let tw = w.min(h * 240 / 280).clamp(1, 480);
    let th = (tw * 280 / 240).max(1);
    img.resize_to_fill(tw, th, image::imageops::FilterType::Triangle)
}

/// CSS `saturate(0.35) brightness(0.62)`, as the mock-up draws games that aren't installed.
pub fn dim(img: &image::RgbImage) -> image::RgbImage {
    let mut out = img.clone();
    for p in out.pixels_mut() {
        let [r, g, b] = p.0.map(|c| c as f32);
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        p.0 = [r, g, b].map(|c| ((l + (c - l) * 0.35) * 0.62).round().clamp(0.0, 255.0) as u8);
    }
    out
}

/// Crop to the bounding box of pixels that aren't (nearly) transparent.
pub fn trim_transparent(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 8 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 > x1 || y0 > y1 {
        return img.clone(); // fully transparent: leave it alone
    }
    image::imageops::crop_imm(img, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image()
}

// ---------------------------------------------------------------------------------------------
// GPUI side

pub struct ArtStore {
    fetcher: Fetcher,
    slots: HashMap<(ArtSource, Variant), Slot>,
}

impl ArtStore {
    pub fn new(fetcher: Fetcher) -> Self {
        Self { fetcher, slots: HashMap::new() }
    }

    /// The current state of a piece of art, starting a load if it hasn't been asked for.
    pub fn get(&mut self, src: &ArtSource, variant: Variant, cx: &mut Context<Self>) -> ArtState {
        let key = (src.clone(), variant);
        let retry = match self.slots.get(&key) {
            Some(Slot::Ready(img)) => return ArtState::Ready(img.clone()),
            Some(Slot::Loading) => return ArtState::Loading,
            Some(Slot::Retrying) => return ArtState::Failed,
            Some(Slot::Failed(when)) if when.elapsed() < RETRY_AFTER => return ArtState::Failed,
            Some(Slot::Failed(_)) => true,
            None => false,
        };
        self.slots.insert(key.clone(), if retry { Slot::Retrying } else { Slot::Loading });
        let fetcher = self.fetcher.clone();
        let (src, variant) = key.clone();
        let rx = crate::on_tokio(async move { fetcher.load(src, variant).await });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("cancelled")));
            this.update(cx, |this, cx| {
                let slot = match result {
                    Ok(img) => Slot::Ready(img),
                    Err(e) => {
                        tracing::warn!("art {:?}: {e:#}", key.0);
                        Slot::Failed(Instant::now())
                    }
                };
                this.slots.insert(key, slot);
                cx.notify();
            })
            .ok();
        })
        .detach();
        if retry { ArtState::Failed } else { ArtState::Loading }
    }
}

/// Per-game image files chosen by the user, by slot. Read from
/// `$XDG_CONFIG_HOME/quiver/custom-art.json` (`{"3539": {"tile": "/path.png"}}`) until the
/// daemon stores them; the app never writes it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CustomArt {
    #[serde(default)]
    pub tile: Option<PathBuf>,
    #[serde(default)]
    pub hero: Option<PathBuf>,
    #[serde(default)]
    pub logo: Option<PathBuf>,
}

pub fn custom_art_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("quiver/custom-art.json")
}

pub fn load_custom_art(path: &Path) -> HashMap<u32, CustomArt> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
    match serde_json::from_str::<HashMap<String, CustomArt>>(&text) {
        Ok(m) => m.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect(),
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", path.display());
            HashMap::new()
        }
    }
}

pub use quiver_api::art::{ArtOrigin, ArtPreference, ArtSlot};

/// One image that could fill a slot. `origin` is `None` for the user's own file.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub src: ArtSource,
    pub origin: Option<ArtOrigin>,
}

/// The images that could fill a game's slot, best first: the user's file, then
/// `quiver_api::art::candidates` in the order the game's `art` setting asks for.
pub fn candidates(entry: &quiver_api::LibraryEntry, custom: Option<&CustomArt>, slot: ArtSlot, pref: ArtPreference) -> Vec<Candidate> {
    let mine = custom.and_then(|c| match slot {
        ArtSlot::Tile => c.tile.clone(),
        ArtSlot::Hero => c.hero.clone(),
        ArtSlot::Logo => c.logo.clone(),
        ArtSlot::Icon => None,
    });
    let mut out: Vec<Candidate> = mine.into_iter().map(|p| Candidate { src: ArtSource::Local(p), origin: None }).collect();
    for (origin, r) in quiver_api::art::candidates(entry, slot, pref) {
        let src = match r {
            quiver_api::art::ArtRef::Asset(f) if valid_cdn_name(&f) => ArtSource::Cdn(f),
            quiver_api::art::ArtRef::Asset(_) => continue,
            quiver_api::art::ArtRef::Url(u) => ArtSource::Url(u),
            quiver_api::art::ArtRef::File(p) => ArtSource::Local(p),
        };
        out.push(Candidate { src, origin: Some(origin) });
    }
    out
}

/// The first candidate's image that hasn't failed (`Loading` while the best untried one
/// loads), and where it came from. `(Failed, None)` when nothing is left.
pub fn pick(cands: &[Candidate], mut state: impl FnMut(&ArtSource) -> ArtState) -> (ArtState, Option<&Candidate>) {
    for c in cands {
        match state(&c.src) {
            ArtState::Failed => continue,
            s => return (s, Some(c)),
        }
    }
    (ArtState::Failed, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn png(w: u32, h: u32, opaque: (u32, u32, u32, u32)) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let inside = x >= opaque.0 && x < opaque.2 && y >= opaque.1 && y < opaque.3;
            *p = image::Rgba([200, 100, 50, if inside { 255 } else { 0 }]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// A one-file HTTP server; returns its base URL and a hit counter.
    async fn serve(body: Vec<u8>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { break };
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let found = req.starts_with("GET /assets/good");
                    let head = if found {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                    };
                    let _ = s.write_all(head.as_bytes()).await;
                    if found {
                        let _ = s.write_all(&body).await;
                    }
                });
            }
        });
        (format!("http://{addr}/assets/"), hits)
    }

    #[tokio::test]
    async fn downloads_once_then_serves_from_cache() {
        let cache = tempfile::tempdir().unwrap();
        let (base, hits) = serve(png(40, 20, (10, 5, 30, 15))).await;
        let f = Fetcher::new(cache.path().join("art"), base);
        let a = f.load(ArtSource::Cdn("good.png".into()), Variant::Original).await.unwrap();
        assert_eq!((a.width, a.height), (40, 20));
        assert!(a.path.starts_with(cache.path()));
        let b = f.load(ArtSource::Cdn("good.png".into()), Variant::Original).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "second load came from disk");
        // Derived: the logo trimmed to its opaque box, the ambient copy small.
        let t = f.load(ArtSource::Cdn("good.png".into()), Variant::Trimmed).await.unwrap();
        assert_eq!((t.width, t.height), (20, 10));
        let amb = f.load(ArtSource::Cdn("good.png".into()), Variant::Ambient).await.unwrap();
        assert!(amb.width <= 96);
        // Tiles are cropped to 240:280 whatever the source's shape.
        let tile = f.load(ArtSource::Cdn("good.png".into()), Variant::Tile).await.unwrap();
        assert_eq!((tile.width, tile.height), (17, 19));
        let dim = f.load(ArtSource::Cdn("good.png".into()), Variant::TileDimmed).await.unwrap();
        assert!(dim.path.to_string_lossy().ends_with("good.tile-dim.jpg"));
        // Failures aren't cached.
        assert!(f.load(ArtSource::Cdn("missing.png".into()), Variant::Original).await.is_err());
        assert!(!cache.path().join("art/missing.png").exists());
        // Hostile names never reach the network or the file system.
        assert!(f.load(ArtSource::Cdn("../etc/passwd".into()), Variant::Original).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_requests_download_once() {
        let cache = tempfile::tempdir().unwrap();
        let (base, hits) = serve(png(64, 64, (0, 0, 64, 64))).await;
        let f = Fetcher::new(cache.path().join("art"), base);
        let src = ArtSource::Cdn("good.png".into());
        let (a, b, c) = tokio::join!(
            f.load(src.clone(), Variant::Original),
            f.load(src.clone(), Variant::Ambient),
            f.load(src.clone(), Variant::Tile),
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok(), "{a:?} {b:?} {c:?}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn local_files_and_custom_art() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mine.png");
        std::fs::write(&file, png(8, 8, (0, 0, 8, 8))).unwrap();
        let f = Fetcher::new(dir.path().join("cache"), "http://127.0.0.1:9/");
        let a = f.load(ArtSource::Local(file.clone()), Variant::Original).await.unwrap();
        assert_eq!(&*a.path, file.as_path());
        let amb = f.load(ArtSource::Local(file.clone()), Variant::Ambient).await.unwrap();
        assert!(amb.path.starts_with(dir.path().join("cache/derived")));
        assert!(f.load(ArtSource::Local(dir.path().join("nope.png")), Variant::Original).await.is_err());

        let json = dir.path().join("custom-art.json");
        std::fs::write(&json, format!(r#"{{"3539": {{"tile": "{}"}}, "bad": {{}}}}"#, file.display())).unwrap();
        let custom = load_custom_art(&json);
        assert_eq!(custom.len(), 1);
        let art = quiver_api::Art { thumb: Some("abc.jpg".into()), background: Some("def.jpg".into()), ..Default::default() };
        let entry = entry_with(art, None);
        let first = |c: Option<&CustomArt>, slot| candidates(&entry, c, slot, ArtPreference::Ubisoft).first().map(|c| c.src.clone());
        assert_eq!(first(custom.get(&3539), ArtSlot::Tile), Some(ArtSource::Local(file)));
        assert_eq!(first(custom.get(&3539), ArtSlot::Hero), Some(ArtSource::Cdn("def.jpg".into())));
        assert_eq!(first(None, ArtSlot::Logo), None);
    }

    fn entry_with(art: quiver_api::Art, steam: Option<quiver_api::art::SteamArt>) -> quiver_api::LibraryEntry {
        let mut g: quiver_api::exports::SteamGame = serde_json::from_value(serde_json::json!({
            "app_id": 3751950, "name": "BF", "install_dir": "/s/common/BF", "state": "off"
        }))
        .unwrap();
        g.art = steam.clone().unwrap_or_default();
        serde_json::from_value::<quiver_api::LibraryEntry>(serde_json::json!({
            "product_id": 66088, "name": "BF", "installed": null, "update_available": false, "running": false,
        }))
        .map(|mut e| {
            e.art = art;
            e.steam = steam.map(|_| g);
            e
        })
        .unwrap()
    }

    #[test]
    fn steam_copies_fall_back_to_steam_art() {
        let steam = quiver_api::art::SteamArt { hero: Some("/lc/3751950/h/library_hero.jpg".into()), ..Default::default() };
        let e = entry_with(quiver_api::Art::default(), Some(steam));
        let hero = candidates(&e, None, ArtSlot::Hero, ArtPreference::Ubisoft);
        assert_eq!(hero[0], Candidate { src: ArtSource::Local("/lc/3751950/h/library_hero.jpg".into()), origin: Some(ArtOrigin::SteamCache) });
        let tile = candidates(&e, None, ArtSlot::Tile, ArtPreference::Ubisoft);
        assert_eq!(tile[0].src, ArtSource::Url("https://shared.steamstatic.com/store_item_assets/steam/apps/3751950/library_600x900.jpg".into()));
        // The first that hasn't failed wins; loading stops the search.
        let failed = ArtSource::Local("/lc/3751950/h/library_hero.jpg".into());
        let (s, c) = pick(&hero, |s| if *s == failed { ArtState::Failed } else { ArtState::Loading });
        assert_eq!((s, c.unwrap().origin), (ArtState::Loading, Some(ArtOrigin::SteamCdn)));
        assert_eq!(pick(&hero, |_| ArtState::Failed), (ArtState::Failed, None));
    }

    #[tokio::test]
    async fn url_art_is_cached_by_host_and_path() {
        let cache = tempfile::tempdir().unwrap();
        let (base, hits) = serve(png(40, 20, (10, 5, 30, 15))).await;
        let host = base.trim_start_matches("http://").split('/').next().unwrap().to_owned();
        let f = Fetcher::new(cache.path().join("art"), "http://127.0.0.1:9/").allow_test_host(&host);
        let url = format!("{base}good.png");
        let a = f.load(ArtSource::Url(url.clone()), Variant::Original).await.unwrap();
        assert!(a.path.ends_with(format!("url/{}/assets/good.png", host.replace(':', "_"))), "{:?}", a.path);
        let t = f.load(ArtSource::Url(url.clone()), Variant::Trimmed).await.unwrap();
        assert_eq!((t.width, t.height), (20, 10));
        assert!(t.path.to_string_lossy().contains("derived/url-"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Unknown hosts, http in production, odd paths and non-images are refused.
        let prod = Fetcher::new(cache.path().join("art"), "http://127.0.0.1:9/");
        assert!(prod.load(ArtSource::Url(url), Variant::Original).await.is_err());
        for bad in [
            "https://evil.example/x.png",
            "https://shared.steamstatic.com/../x.png",
            "https://shared.steamstatic.com/a/b.png?x=1",
            "https://cdn1.epicgames.com/a/../../x",
        ] {
            assert!(prod.url_cache_rel(bad).is_none(), "{bad}");
        }
        assert!(prod.url_cache_rel("https://shared.steamstatic.com/store_item_assets/steam/apps/3751950/logo.png").is_some());
    }

    /// Epic's catalog URLs: spaces, `&` and no extension. Cached under a hash of the URL,
    /// with the extension the image has.
    #[tokio::test]
    async fn epic_urls_are_cached_by_hash() {
        let cache = tempfile::tempdir().unwrap();
        let (base, hits) = serve(png(40, 20, (10, 5, 30, 15))).await;
        let host = base.trim_start_matches("http://").split('/').next().unwrap().to_owned();
        let f = Fetcher::new(cache.path().join("art"), "http://127.0.0.1:9/").allow_test_host(&host);
        let url = format!("{base}good Portrait_1200x1600&RED-1200x1600-61df06244a496c82f3ac231174c1b1f5");
        let a = f.load(ArtSource::Url(url.clone()), Variant::Original).await.unwrap();
        assert_eq!((a.width, a.height), (40, 20));
        let rel = a.path.strip_prefix(cache.path().join("art")).unwrap().to_string_lossy().into_owned();
        assert!(rel.starts_with(&format!("url/{}/h/", host.replace(':', "_"))) && rel.ends_with(".png"), "{rel}");
        let t = f.load(ArtSource::Url(url.clone()), Variant::Tile).await.unwrap();
        assert!(t.path.to_string_lossy().contains("derived/url-h-"), "{:?}", t.path);
        let again = f.load(ArtSource::Url(url), Variant::Original).await.unwrap();
        assert_eq!(again, a);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The real hosts are allowed; the cache path never leaves the cache dir.
        let prod = Fetcher::new(cache.path().join("art"), "http://127.0.0.1:9/");
        let real = "https://cdn1.epicgames.com/item/7af7476ff9eb4a8d9cd9d6486224de76/ABZU_Portrait_Storefront_1200X1600_1200x1600-61df06244a496c82f3ac231174c1b1f5";
        let rel = prod.url_cache_rel(real).unwrap();
        assert!(rel.starts_with("url/cdn1.epicgames.com/h") && rel.components().count() == 4, "{rel:?}");
        assert!(prod.url_cache_rel("https://cdn1.epicgames.com/0584/item/GTAV Landscaped Store-2560x1440-79155f.jpg").is_some());
        assert!(prod.url_cache_rel("http://cdn1.epicgames.com/x.jpg").is_none());
    }

    #[test]
    fn cdn_names_are_validated() {
        assert!(valid_cdn_name("79b8e5e2468fb3f434b622ee5c5063fb.jpg"));
        assert!(valid_cdn_name("a.webp"));
        assert!(!valid_cdn_name("l2"));
        assert!(!valid_cdn_name("x/y.jpg"));
        assert!(!valid_cdn_name("..jpg"));
        assert!(!valid_cdn_name("48edec3d91f23947b02be534159b74d1.ico"));
    }
}
