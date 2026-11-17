//! Game downloads via `download_service`: signed CDN URLs for manifests and slices.
//!
//! Relative paths understood by the CDN:
//! - `manifests/<HASH>.manifest` (also `.metadata`, `.licenses`)
//! - `slices_v3/<c>/<SHA1>` where `<c>` is derived from the first two hex digits

use anyhow::{Context, Result, bail};
use quiver_proto::{download, download_service as ds};

use crate::demux::{Demux, Download, ServiceConnection};
use crate::install::SliceError;
use crate::library::Library;

const BASE32: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

/// CDN path of a slice. Zstd-era manifests use `slices_v3/<c>/<SHA1>`; older ones
/// (deflate or uncompressed) use `slices/<SHA1>`.
pub fn slice_path_for(compression: Option<i32>, sha1: &[u8]) -> String {
    if compression == Some(download::CompressionMethod::Zstd as i32) {
        slice_path(sha1)
    } else {
        format!("slices/{}", hex::encode_upper(sha1))
    }
}

/// `slices_v3` CDN path of a slice, given its SHA-1 (as in `Slice.downloadSha1`).
pub fn slice_path(sha1: &[u8]) -> String {
    let hash = hex::encode_upper(sha1);
    let digit = |i: usize| u8::from_str_radix(&hash[i..=i], 16).unwrap() as usize;
    // Byte formed by the first two hex digits reversed: high nibble picks the offset,
    // low bit (= first digit's parity) picks the upper or lower half of the alphabet.
    let c = BASE32[digit(1) + if digit(0) % 2 == 1 { 16 } else { 0 }] as char;
    format!("slices_v3/{c}/{hash}")
}

/// CDN connect timeout, and how long a response may go without sending a byte.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct Downloader {
    /// Locked so slices can ask for fresh URLs while others download.
    conn: tokio::sync::Mutex<ServiceConnection<Download>>,
    pub product_id: u32,
    pub http: reqwest::Client,
}

impl Downloader {
    pub async fn open(demux: &Demux, lib: &mut Library, product_id: u32) -> Result<Self> {
        let ownership_token = lib.ownership_token(product_id).await?;
        let mut conn = demux.open::<Download>().await?;
        let rsp = conn
            .request(ds::Req {
                initialize_req: Some(ds::InitializeReq { ownership_token, ..Default::default() }),
                ..Default::default()
            })
            .await?;
        if !rsp.initialize_rsp.is_some_and(|r| r.ok) {
            bail!("download_service refused to initialize for product {product_id}");
        }
        let http = reqwest::Client::builder()
            .user_agent("Massgate")
            .pool_max_idle_per_host(32)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STALL_TIMEOUT)
            .build()?;
        Ok(Self { conn: tokio::sync::Mutex::new(conn), product_id, http })
    }

    /// Signed URLs (one or more mirrors each) for relative CDN paths.
    pub async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        let rsp = self
            .conn
            .lock()
            .await
            .request(ds::Req {
                url_req: Some(ds::UrlReq {
                    url_requests: vec![ds::url_req::Request {
                        product_id: self.product_id,
                        relative_file_path: paths.to_vec(),
                    }],
                }),
                ..Default::default()
            })
            .await?;
        let url_rsp = rsp.url_rsp.context("no url_rsp")?;
        let resp = url_rsp.url_responses.into_iter().next().context("empty url_responses")?;
        if resp.result != ds::url_rsp::Result::Success as i32 {
            bail!("download_service URL request failed: result {}", resp.result);
        }
        if resp.download_urls.len() != paths.len() {
            bail!("asked for {} URLs, got {}", paths.len(), resp.download_urls.len());
        }
        Ok(resp.download_urls.into_iter().map(|d| d.urls).collect())
    }

    /// GET the first working mirror.
    pub async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>> {
        self.fetch(mirrors).await.map_err(SliceError::into_inner)
    }

    /// GET the first working mirror, saying whether a failure is worth retrying.
    pub async fn fetch(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError> {
        let mut worst: Option<SliceError> = None;
        for url in mirrors {
            let e = match self.http.get(url).send().await {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) => return Ok(b.to_vec()),
                    Err(e) => classify(e),
                },
                Ok(r) => {
                    let status = r.status();
                    let e = anyhow::anyhow!("HTTP {status}");
                    match status.as_u16() {
                        // Signed URLs expire (or were refused): ask for fresh ones.
                        400 | 401 | 403 | 404 | 410 => SliceError::Refresh(e),
                        408 | 425 | 429 | 500..=599 => SliceError::Retry(e),
                        _ => SliceError::Fatal(e),
                    }
                }
                Err(e) => classify(e),
            };
            // Keep the most actionable error: a refresh beats a retry beats a fatal one.
            worst = Some(match (worst, e) {
                (Some(SliceError::Refresh(a)), _) => SliceError::Refresh(a),
                (_, e @ SliceError::Refresh(_)) => e,
                (Some(SliceError::Retry(a)), SliceError::Fatal(_)) => SliceError::Retry(a),
                (_, e) => e,
            });
        }
        Err(worst.unwrap_or_else(|| SliceError::Refresh(anyhow::anyhow!("no mirrors"))))
    }

    /// Download and parse a manifest; returns the raw file too (to store as
    /// `uplay_install.manifest`).
    pub async fn manifest(&mut self, hash: &str) -> Result<(download::Manifest, Vec<u8>)> {
        let path = format!("manifests/{}.manifest", hash.to_uppercase());
        let urls = self.urls(&[path]).await?;
        let raw = self.get(&urls[0]).await?;
        Ok((crate::manifest::parse_manifest(&raw)?, raw))
    }
}

/// Connection trouble, timeouts and cut-off bodies are worth retrying.
fn classify(e: reqwest::Error) -> SliceError {
    if e.is_timeout() || e.is_connect() || e.is_body() || e.is_request() || e.is_decode() {
        SliceError::Retry(e.into())
    } else {
        SliceError::Fatal(e.into())
    }
}

impl crate::install::SliceSource for Downloader {
    async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        Downloader::urls(self, paths).await
    }

    async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError> {
        self.fetch(mirrors).await
    }
}
