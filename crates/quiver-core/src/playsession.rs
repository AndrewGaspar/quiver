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
//!
//! When a session no launcher lease held ends (a Steam copy started by `quiver steam-run`,
//! a bridge in a container or another launcher), the tracker hands it to the daemon as a
//! [`Finished`] session so it lands in the game's local play history, as quiverd's own
//! launches do (those are recorded by the launch job, with their settings and profile).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use reqwest::Method;
use tokio::sync::mpsc;
pub use quiver_api::ActiveSession;

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
    /// The next bridge sessions for `product_id` are a Steam launch of `app_id`.
    FromSteam { product_id: u32, app_id: u32 },
}

/// How long a [`PlaySessions::from_steam`] note waits for the game's bridge to report in.
const STEAM_HINT_TTL: Duration = Duration::from_secs(10 * 60);

/// A bridge holder for a game `quiver steam-run` started shows as this.
pub const STEAM_HOLDER: &str = "steam";

/// A play session that ended without a launcher lease (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub product_id: u32,
    pub account: String,
    pub started: chrono::DateTime<Utc>,
    pub ended: chrono::DateTime<Utc>,
    /// Holder names, each once.
    pub holders: Vec<String>,
    pub steam_app_id: Option<u32>,
}

/// The holder name of quiverd's own launches.
pub const LAUNCHER_HOLDER: &str = "launcher";

struct Active {
    online: Arc<Online>,
    started: chrono::DateTime<Utc>,
    /// Everyone who held it, each once (holders come and go).
    ever: Vec<String>,
    /// (lease id, holder name)
    holders: Vec<(u64, String)>,
    reported: bool,
    last_beat: Instant,
    steam_app_id: Option<u32>,
}

/// Holder names, each once, in the order they joined (a game's SDK opens several
/// connections, each holding its own lease).
fn unique_holders(holders: &[(u64, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, h) in holders {
        if !out.contains(h) {
            out.push(h.clone());
        }
    }
    out
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
    /// `on_finished` gets every session that ended without a launcher lease.
    pub fn new(
        accounts: Arc<Accounts>,
        report: bool,
        on_change: impl Fn() + Send + 'static,
        on_finished: impl Fn(Finished) + Send + 'static,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run(accounts, report, rx, snapshot.clone(), on_change, on_finished));
        Arc::new(Self { tx, snapshot, next_lease: Default::default() })
    }

    /// Hold a session open for `product_id` on `account` (`None`: the default account)
    /// until the lease is dropped.
    pub fn begin(&self, account: Option<&str>, product_id: u32, holder: &str) -> Lease {
        let id = self.next_lease.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Begin { account: account.map(str::to_owned), product_id, holder: holder.to_owned(), lease: id });
        Lease { tx: self.tx.clone(), id }
    }

    /// `quiver steam-run` is starting `product_id` from Steam app `app_id`: bridge leases
    /// for it in the next few minutes (and while that session lasts) show as `steam`.
    pub fn from_steam(&self, product_id: u32, app_id: u32) {
        let _ = self.tx.send(Cmd::FromSteam { product_id, app_id });
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
    on_finished: impl Fn(Finished),
) {
    let mut active: HashMap<Key, Active> = HashMap::new();
    let mut steam_hints: HashMap<u32, (u32, Instant)> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        let mut changed = false;
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => return,
                Some(Cmd::FromSteam { product_id, app_id }) => {
                    steam_hints.retain(|_, (_, at)| at.elapsed() < STEAM_HINT_TTL);
                    steam_hints.insert(product_id, (app_id, Instant::now()));
                }
                Some(Cmd::Begin { account, product_id, mut holder, lease }) => {
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
                    let hint = steam_hints.get(&product_id).filter(|(_, at)| at.elapsed() < STEAM_HINT_TTL).map(|h| h.0);
                    let steam_app_id = active.get(&key).and_then(|a| a.steam_app_id).or(hint);
                    if holder == "bridge" && steam_app_id.is_some() {
                        holder = STEAM_HOLDER.to_owned();
                    }
                    if let Some(a) = active.get_mut(&key) {
                        if !a.ever.contains(&holder) {
                            a.ever.push(holder.clone());
                        }
                        a.holders.push((lease, holder));
                        a.steam_app_id = steam_app_id;
                    } else {
                        tracing::info!(product_id, %holder, "game started");
                        let reported = report && match start(&online, product_id).await {
                            Ok(()) => { tracing::info!(product_id, "play session started"); true }
                            Err(e) => { tracing::warn!(product_id, "play session start failed: {e:#}"); false }
                        };
                        active.insert(key, Active {
                            online,
                            started: Utc::now(),
                            ever: vec![holder.clone()],
                            holders: vec![(lease, holder)],
                            reported,
                            last_beat: Instant::now(),
                            steam_app_id,
                        });
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
                        if a.steam_app_id.is_some() {
                            steam_hints.remove(&product_id);
                        }
                        tracing::info!(product_id, minutes = (Utc::now() - a.started).num_minutes(), "game stopped");
                        if let Some(f) = finished(&key, a.started, &a.ever, a.steam_app_id, Utc::now()) {
                            on_finished(f);
                        }
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
                    holders: unique_holders(&a.holders),
                    reported: a.reported,
                    steam_app_id: a.steam_app_id,
                })
                .collect();
            list.sort_by_key(|s| s.started);
            *snapshot.lock().unwrap() = list;
            on_change();
        }
    }
}

/// The session to record for `a` ending now, unless a launcher held it (the launch job
/// records those).
fn finished(
    key: &Key,
    started: chrono::DateTime<Utc>,
    ever: &[String],
    steam_app_id: Option<u32>,
    ended: chrono::DateTime<Utc>,
) -> Option<Finished> {
    (!ever.iter().any(|h| h == LAUNCHER_HOLDER)).then(|| Finished {
        product_id: key.1,
        account: key.0.clone(),
        started,
        ended,
        holders: ever.to_vec(),
        steam_app_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holders_once_each() {
        let h = |v: &[(u64, &str)]| v.iter().map(|(i, s)| (*i, s.to_string())).collect::<Vec<_>>();
        assert_eq!(unique_holders(&h(&[(1, "bridge"), (2, "bridge"), (3, "bridge")])), ["bridge"]);
        assert_eq!(unique_holders(&h(&[(1, "launcher"), (2, "bridge"), (3, "launcher")])), ["launcher", "bridge"]);
        assert!(unique_holders(&[]).is_empty());
    }

    #[test]
    fn sessions_without_a_launcher_are_handed_over() {
        let key: Key = ("uid".into(), 66088);
        let t0 = Utc::now();
        let t1 = t0 + chrono::TimeDelta::minutes(5);
        let f = finished(&key, t0, &[STEAM_HOLDER.into()], Some(3751950), t1).unwrap();
        assert_eq!((f.product_id, f.account.as_str(), f.steam_app_id), (66088, "uid", Some(3751950)));
        assert_eq!((f.started, f.ended), (t0, t1));
        assert!(finished(&key, t0, &["bridge".into()], None, t1).is_some());
        // quiverd's own launch (its bridge joins the launcher's session): the job records it.
        assert!(finished(&key, t0, &[LAUNCHER_HOLDER.into(), "bridge".into()], None, t1).is_none());
    }
}
