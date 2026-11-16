//! Game downloads via `download_service`: signed CDN URLs for manifests and slices.
//!
//! Relative paths understood by the CDN:
//! - `manifests/<HASH>.manifest` (also `.metadata`, `.licenses`)
//! - `slices_v3/<c>/<SHA1>` where `<c>` is derived from the first two hex digits

use anyhow::{Context, Result, bail};
use ubi_proto::{download, download_service as ds};

use crate::demux::{Demux, Download, ServiceConnection};
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

pub struct Downloader {
    conn: ServiceConnection<Download>,
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
            .build()?;
        Ok(Self { conn, product_id, http })
    }

    /// Signed URLs (one or more mirrors each) for relative CDN paths.
    pub async fn urls(&mut self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        let rsp = self
            .conn
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
        let mut last = None;
        for url in mirrors {
            match self.http.get(url).send().await.and_then(|r| r.error_for_status()) {
                Ok(r) => return Ok(r.bytes().await?.to_vec()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("no mirrors")))
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
