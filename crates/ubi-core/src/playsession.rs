//! Play-session reporting to UbiServices, as Ubisoft Connect does it (captured from
//! Connect 13416 with tools/recon/tls-tap):
//!
//! ```text
//! POST   /v1/profiles/{userId}/global/ubiconnect/playsession/api/sessions   {"productId":N}  -> 201 {expiresAt, createdAt}
//! PUT    (same URL, same body), every 330 s                                                  -> 200 {expiresAt, updatedAt}
//! DELETE …/sessions?productId=N                                                              -> 204
//! ```
//!
//! A session that stops receiving heartbeats expires on its own (`expiresAt` is six
//! minutes out), so a crash costs at most a few minutes of reported time.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use reqwest::Method;

use crate::online::Online;

/// Connect's heartbeat interval.
pub const HEARTBEAT: Duration = Duration::from_secs(330);

fn url(user_id: &str) -> String {
    format!("https://public-ubiservices.ubi.com/v1/profiles/{user_id}/global/ubiconnect/playsession/api/sessions")
}

async fn call(online: &Online, method: Method, product_id: u32) -> Result<()> {
    let session = online.session().await?;
    let auth = &online.auth;
    let mut req = auth
        .http
        .request(method.clone(), url(&session.user_id))
        .header("Ubi-AppId", &auth.app_id)
        .header("Ubi-SessionId", &session.session_id)
        .header("Authorization", format!("Ubi_v1 t={}", session.ticket));
    req = if method == Method::DELETE {
        req.query(&[("productId", product_id)])
    } else {
        req.json(&serde_json::json!({ "productId": product_id }))
    };
    let rsp = req.send().await?;
    let status = rsp.status();
    if !status.is_success() {
        bail!("{method} playsession returned {status}: {}", rsp.text().await.unwrap_or_default());
    }
    Ok(())
}

/// Start a play session for `product_id`.
pub async fn start(online: &Online, product_id: u32) -> Result<()> {
    call(online, Method::POST, product_id).await
}

/// Keep the session alive (call every [`HEARTBEAT`]).
pub async fn heartbeat(online: &Online, product_id: u32) -> Result<()> {
    call(online, Method::PUT, product_id).await
}

/// End the session.
pub async fn end(online: &Online, product_id: u32) -> Result<()> {
    call(online, Method::DELETE, product_id).await
}

/// Drives one session from the launcher's wait loop. Failures are logged, never fatal:
/// reporting play time must not get in the way of playing.
pub struct PlaySessionReporter<'a> {
    online: &'a Online,
    product_id: u32,
    last: Option<Instant>,
}

impl<'a> PlaySessionReporter<'a> {
    pub fn new(online: &'a Online, product_id: u32) -> Self {
        Self { online, product_id, last: None }
    }

    pub async fn start(&mut self) {
        match start(self.online, self.product_id).await {
            Ok(()) => tracing::info!(self.product_id, "play session started"),
            Err(e) => tracing::warn!(self.product_id, "play session start failed: {e:#}"),
        }
        // Even after a failed start, heartbeats (PUT) retry on schedule; Connect does the same.
        self.last = Some(Instant::now());
    }

    /// Send a heartbeat if one is due.
    pub async fn tick(&mut self) {
        if self.last.is_some_and(|t| t.elapsed() < HEARTBEAT) {
            return;
        }
        match heartbeat(self.online, self.product_id).await {
            Ok(()) => tracing::info!(self.product_id, "play session updated"),
            Err(e) => {
                tracing::warn!(self.product_id, "play session update failed: {e:#}");
                // The session may have expired (e.g. after a suspend): open a new one.
                if start(self.online, self.product_id).await.is_ok() {
                    tracing::info!(self.product_id, "play session restarted");
                }
            }
        }
        self.last = Some(Instant::now());
    }

    pub async fn end(&mut self) {
        if self.last.take().is_none() {
            return;
        }
        match end(self.online, self.product_id).await {
            Ok(()) => tracing::info!(self.product_id, "play session ended"),
            Err(e) => tracing::warn!(self.product_id, "play session end failed: {e:#}"),
        }
    }
}
