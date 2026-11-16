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

use crate::online::{Accounts, Online};

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

/// Sessions are per account and game (the URL carries the account's user id).
type Key = (String, u32);

enum Cmd {
    Begin { account: Option<String>, product_id: u32, holder: String, lease: u64 },
    End { lease: u64 },
}

struct Active {
    online: Arc<Online>,
    started: chrono::DateTime<Utc>,
    /// (lease id, holder name)
    holders: Vec<(u64, String)>,
    reported: bool,
    last_beat: Instant,
}

/// The daemon's play-session tracker.
pub struct PlaySessions {
    tx: mpsc::UnboundedSender<Cmd>,
    snapshot: Arc<Mutex<Vec<ActiveSession>>>,
    next_lease: std::sync::atomic::AtomicU64,
}

/// Holds a play session open; dropping it releases this holder's share.
pub struct Lease {
    tx: mpsc::UnboundedSender<Cmd>,
    id: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::End { lease: self.id });
    }
}

impl PlaySessions {
    /// Start the tracker. With `report` false, sessions are tracked (for `sessions` and the
    /// UI) but nothing is sent to Ubisoft. `on_change` runs after every change.
    pub fn new(accounts: Arc<Accounts>, report: bool, on_change: impl Fn() + Send + 'static) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run(accounts, report, rx, snapshot.clone(), on_change));
        Arc::new(Self { tx, snapshot, next_lease: Default::default() })
    }

    /// Hold a session open for `product_id` on `account` (`None`: the default account)
    /// until the lease is dropped.
    pub fn begin(&self, account: Option<&str>, product_id: u32, holder: &str) -> Lease {
        let id = self.next_lease.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Begin { account: account.map(str::to_owned), product_id, holder: holder.to_owned(), lease: id });
        Lease { tx: self.tx.clone(), id }
    }

    pub fn list(&self) -> Vec<ActiveSession> {
        self.snapshot.lock().unwrap().clone()
    }
}

async fn run(
    accounts: Arc<Accounts>,
    report: bool,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    snapshot: Arc<Mutex<Vec<ActiveSession>>>,
    on_change: impl Fn(),
) {
    let mut active: HashMap<Key, Active> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        let mut changed = false;
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => return,
                Some(Cmd::Begin { account, product_id, holder, lease }) => {
                    let online = match accounts.get(account.as_deref()) {
                        Ok(o) => o,
                        Err(e) => { tracing::warn!(product_id, "play session for unknown account: {e:#}"); continue }
                    };
                    let user_id = match online.session().await {
                        Ok(s) => s.user_id,
                        Err(e) => { tracing::warn!(product_id, "play session without a session: {e:#}"); continue }
                    };
                    changed = true;
                    let key = (user_id, product_id);
                    if let Some(a) = active.get_mut(&key) {
                        a.holders.push((lease, holder));
                    } else {
                        tracing::info!(product_id, %holder, "game started");
                        let reported = report && match start(&online, product_id).await {
                            Ok(()) => { tracing::info!(product_id, "play session started"); true }
                            Err(e) => { tracing::warn!(product_id, "play session start failed: {e:#}"); false }
                        };
                        active.insert(key, Active { online, started: Utc::now(), holders: vec![(lease, holder)], reported, last_beat: Instant::now() });
                    }
                }
                Some(Cmd::End { lease }) => {
                    let Some(key) = active.iter().find(|(_, a)| a.holders.iter().any(|h| h.0 == lease)).map(|(k, _)| k.clone()) else { continue };
                    let a = active.get_mut(&key).unwrap();
                    a.holders.retain(|h| h.0 != lease);
                    changed = true;
                    if a.holders.is_empty() {
                        let product_id = key.1;
                        let a = active.remove(&key).unwrap();
                        tracing::info!(product_id, minutes = (Utc::now() - a.started).num_minutes(), "game stopped");
                        if report {
                            match end(&a.online, product_id).await {
                                Ok(()) => tracing::info!(product_id, "play session ended"),
                                Err(e) => tracing::warn!(product_id, "play session end failed: {e:#}"),
                            }
                        }
                    }
                }
            },
            _ = tick.tick() => {
                if !report { continue }
                for (&(_, product_id), a) in active.iter_mut() {
                    let online = a.online.clone();
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
                .map(|((user_id, product_id), a)| ActiveSession {
                    product_id: *product_id,
                    account: user_id.clone(),
                    started: a.started,
                    holders: a.holders.iter().map(|h| h.1.clone()).collect(),
                    reported: a.reported,
                })
                .collect();
            list.sort_by_key(|s| s.started);
            *snapshot.lock().unwrap() = list;
            on_change();
        }
    }
}
