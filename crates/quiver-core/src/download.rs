//! Game downloads via `download_service`: signed CDN URLs for manifests and slices.
//!
//! Relative paths understood by the CDN:
//! - `manifests/<HASH>.manifest` (also `.metadata`, `.licenses`)
//! - `slices_v3/<c>/<SHA1>` where `<c>` is derived from the first two hex digits

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use quiver_proto::{download, download_service as ds};

use crate::demux::{Demux, Download, ServiceConnection};
use crate::install::SliceError;
use crate::library::Library;
use crate::online::Online;
use crate::retry::{Transient, permanent, transient};
use crate::stores::BoxFuture;

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
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// The longest gap between reads before a slice is tried again on a fresh connection (the
// bandwidth limit waits before a request, never between reads).
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How a [`Downloader`] gets a live demux socket again once its own is gone: the
/// account's shared socket, reconnected by [`Online::demux`].
pub type Reconnect = Arc<dyn Fn() -> BoxFuture<'static, Result<Demux>> + Send + Sync>;

/// The download_service session: the socket it runs on, its connection (`None` once it
/// failed: the next request opens a new one) and the ownership token it was opened with.
struct Session {
    demux: Demux,
    conn: Option<ServiceConnection<Download>>,
    token: String,
}

pub struct Downloader {
    /// Locked so slices can ask for fresh URLs while others download.
    session: tokio::sync::Mutex<Session>,
    reconnect: Option<Reconnect>,
    pub product_id: u32,
    pub http: reqwest::Client,
}

impl Downloader {
    /// A downloader on `demux` alone: once that socket is gone, URL requests fail
    /// (transiently). See [`Downloader::open_online`].
    pub async fn open(demux: &Demux, lib: &mut Library, product_id: u32) -> Result<Self> {
        Self::open_with(demux, lib, product_id, None).await
    }

    /// A downloader that follows the account's socket: when the server closes the
    /// download_service connection, or the whole socket drops, the next URL request opens
    /// a new connection (on the reconnected socket if need be) and initializes it again.
    pub async fn open_online(online: &Arc<Online>, demux: &Demux, lib: &mut Library, product_id: u32) -> Result<Self> {
        let o = online.clone();
        let reconnect: Reconnect = Arc::new(move || {
            let o = o.clone();
            Box::pin(async move { o.demux().await })
        });
        Self::open_with(demux, lib, product_id, Some(reconnect)).await
    }

    /// [`Downloader::open`], reconnecting through `reconnect` when the socket is gone.
    pub async fn open_with(demux: &Demux, lib: &mut Library, product_id: u32, reconnect: Option<Reconnect>) -> Result<Self> {
        let token = lib.ownership_token(product_id).await?;
        let Some(conn) = initialize(demux, &token).await? else {
            return Err(permanent(format!("download_service refused to initialize for product {product_id}")));
        };
        let http = reqwest::Client::builder()
            .user_agent("Massgate")
            .pool_max_idle_per_host(32)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STALL_TIMEOUT)
            .build()?;
        let session = Session { demux: demux.clone(), conn: Some(conn), token };
        Ok(Self { session: tokio::sync::Mutex::new(session), reconnect, product_id, http })
    }

    /// Signed URLs (one or more mirrors each) for relative CDN paths.
    ///
    /// A connection the server closed (or that stopped answering) is replaced on the next
    /// request: a new `download_service` connection, initialized with the ownership token,
    /// or a fresh token should the server refuse the old one. Failures that may pass are
    /// [`Transient`] (the caller waits and asks again); a product the account doesn't own,
    /// even with a fresh token, is permanent.
    pub async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        let mut s = self.session.lock().await;
        let mut renewed = false;
        loop {
            self.ensure_open(&mut s, renewed).await?;
            let conn = s.conn.as_mut().expect("opened");
            let rsp = match conn.request(url_req(self.product_id, paths)).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::info!(connection = conn.id(), "download_service request failed ({e:#}); a new connection is opened for the next one");
                    s.conn = None;
                    return Err(e);
                }
            };
            let url_rsp = rsp.url_rsp.context("no url_rsp")?;
            let resp = url_rsp.url_responses.into_iter().next().context("empty url_responses")?;
            if resp.result == ds::url_rsp::Result::NotOwned as i32 && !renewed {
                // The session's ownership may have lapsed: once more with a fresh token.
                tracing::info!(product = self.product_id, "download_service says not owned; renewing the ownership token");
                renewed = true;
                continue;
            }
            if resp.result != ds::url_rsp::Result::Success as i32 {
                return Err(permanent(format!("download_service URL request failed: result {} (not owned)", resp.result)));
            }
            if resp.download_urls.len() != paths.len() {
                bail!("asked for {} URLs, got {}", paths.len(), resp.download_urls.len());
            }
            return Ok(resp.download_urls.into_iter().map(|d| d.urls).collect());
        }
    }

    /// Give `s` an open, initialized connection, reconnecting the socket if it is gone.
    /// `fresh_token`: ask ownership_service for a new token first.
    async fn ensure_open(&self, s: &mut Session, fresh_token: bool) -> Result<()> {
        if !fresh_token && s.conn.as_ref().is_some_and(|c| c.is_open()) {
            return Ok(());
        }
        s.conn = None;
        if !s.demux.is_alive() {
            let reconnect = self.reconnect.as_ref().ok_or_else(|| transient("the demux connection is gone"))?;
            tracing::info!(product = self.product_id, "demux connection gone; reconnecting");
            s.demux = reconnect().await.map_err(|e| e.context(Transient("reconnecting to Ubisoft".into())))?;
        }
        let mut fresh = fresh_token;
        loop {
            if fresh {
                s.token = self.fresh_token(&s.demux).await?;
            }
            if let Some(conn) = initialize(&s.demux, &s.token).await? {
                tracing::info!(product = self.product_id, connection = conn.id(), "download_service connection opened");
                s.conn = Some(conn);
                return Ok(());
            }
            if fresh {
                return Err(permanent(format!("download_service refused to initialize for product {}", self.product_id)));
            }
            // The token may have expired: once more with a new one.
            tracing::info!(product = self.product_id, "download_service refused the ownership token; asking for a new one");
            fresh = true;
        }
    }

    async fn fresh_token(&self, demux: &Demux) -> Result<String> {
        Library::fetch(demux).await?.ownership_token(self.product_id).await
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

/// Open a download_service connection and initialize it with an ownership token.
/// `None`: the server refused the token.
async fn initialize(demux: &Demux, token: &str) -> Result<Option<ServiceConnection<Download>>> {
    let mut conn = demux.open::<Download>().await?;
    let rsp = conn
        .request(ds::Req {
            initialize_req: Some(ds::InitializeReq { ownership_token: token.to_owned(), ..Default::default() }),
            ..Default::default()
        })
        .await?;
    Ok(rsp.initialize_rsp.is_some_and(|r| r.ok).then_some(conn))
}

fn url_req(product_id: u32, paths: &[String]) -> ds::Req {
    ds::Req {
        url_req: Some(ds::UrlReq { url_requests: vec![ds::url_req::Request { product_id, relative_file_path: paths.to_vec() }] }),
        ..Default::default()
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

#[cfg(test)]
mod tests;
