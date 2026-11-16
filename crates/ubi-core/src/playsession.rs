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
//!
//! [`PlaySessions`] is the daemon's single owner of these calls. Every launch path takes a
//! [`Lease`] while its game runs (the launcher for games quiverd starts, the broker for any
//! bridge, including containers); leases for the same game share one upstream session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use reqwest::Method;
use tokio::sync::mpsc;
pub use ubi_api::ActiveSession;

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

enum Cmd {
    Begin { product_id: u32, holder: String },
    End { product_id: u32, holder: String },
}

struct Active {
    started: chrono::DateTime<Utc>,
    holders: Vec<String>,
    reported: bool,
    last_beat: Instant,
}

/// The daemon's play-session tracker.
pub struct PlaySessions {
    tx: mpsc::UnboundedSender<Cmd>,
    snapshot: Arc<Mutex<Vec<ActiveSession>>>,
}

/// Holds a play session open; dropping it releases this holder's share.
pub struct Lease {
    tx: mpsc::UnboundedSender<Cmd>,
    product_id: u32,
    holder: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::End { product_id: self.product_id, holder: std::mem::take(&mut self.holder) });
    }
}

impl PlaySessions {
    /// Start the tracker. With `report` false, sessions are tracked (for `sessions` and the
    /// UI) but nothing is sent to Ubisoft. `on_change` runs after every change.
    pub fn new(online: Arc<Online>, report: bool, on_change: impl Fn() + Send + 'static) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run(online, report, rx, snapshot.clone(), on_change));
        Arc::new(Self { tx, snapshot })
    }

    /// Hold a session open for `product_id` until the lease is dropped.
    pub fn begin(&self, product_id: u32, holder: &str) -> Lease {
        let _ = self.tx.send(Cmd::Begin { product_id, holder: holder.to_owned() });
        Lease { tx: self.tx.clone(), product_id, holder: holder.to_owned() }
    }

    pub fn list(&self) -> Vec<ActiveSession> {
        self.snapshot.lock().unwrap().clone()
    }
}

async fn run(
    online: Arc<Online>,
    report: bool,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    snapshot: Arc<Mutex<Vec<ActiveSession>>>,
    on_change: impl Fn(),
) {
    let mut active: HashMap<u32, Active> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        let mut changed = false;
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => return,
                Some(Cmd::Begin { product_id, holder }) => {
                    changed = true;
                    if let Some(a) = active.get_mut(&product_id) {
                        a.holders.push(holder);
                    } else {
                        tracing::info!(product_id, %holder, "game started");
                        let reported = report && match start(&online, product_id).await {
                            Ok(()) => { tracing::info!(product_id, "play session started"); true }
                            Err(e) => { tracing::warn!(product_id, "play session start failed: {e:#}"); false }
                        };
                        active.insert(product_id, Active { started: Utc::now(), holders: vec![holder], reported, last_beat: Instant::now() });
                    }
                }
                Some(Cmd::End { product_id, holder }) => {
                    let Some(a) = active.get_mut(&product_id) else { continue };
                    if let Some(i) = a.holders.iter().position(|h| *h == holder) {
                        a.holders.remove(i);
                    }
                    changed = true;
                    if a.holders.is_empty() {
                        let a = active.remove(&product_id).unwrap();
                        tracing::info!(product_id, minutes = (Utc::now() - a.started).num_minutes(), "game stopped");
                        if report {
                            match end(&online, product_id).await {
                                Ok(()) => tracing::info!(product_id, "play session ended"),
                                Err(e) => tracing::warn!(product_id, "play session end failed: {e:#}"),
                            }
                        }
                    }
                }
            },
            _ = tick.tick() => {
                if !report { continue }
                for (&product_id, a) in active.iter_mut() {
                    // Heartbeat on schedule; a failed start is retried on the same schedule.
                    if a.last_beat.elapsed() < HEARTBEAT {
                        continue;
                    }
                    a.last_beat = Instant::now();
                    let was = a.reported;
                    a.reported = match if was { heartbeat(&online, product_id).await } else { start(&online, product_id).await } {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(product_id, "play session update failed: {e:#}");
                            // Expired (e.g. after a suspend)? Open a new one.
                            start(&online, product_id).await.is_ok()
                        }
                    };
                    changed |= a.reported != was;
                }
            }
        }
        if changed {
            let mut list: Vec<ActiveSession> = active
                .iter()
                .map(|(&product_id, a)| ActiveSession { product_id, started: a.started, holders: a.holders.clone(), reported: a.reported })
                .collect();
            list.sort_by_key(|s| s.started);
            *snapshot.lock().unwrap() = list;
            on_change();
        }
    }
}
