//! Epic play time (design/STORE-EPIC.md §3 C10, STORES.md §3.9): Epic's own total is the
//! headline, and Quiver reports each session after it ends, as EGL does.
//!
//! | Call | Endpoint |
//! |---|---|
//! | totals | `GET library-service …/library/api/public/playtime/account/<id>/all` → `[{artifactId, totalTime}]` (seconds) |
//! | a session | `PUT library-service …/library/api/public/playtime/account/<id>` `{machineId, artifactId, startTime, endTime, startSegment, endSegment}` → 204 |
//!
//! (Community-documented, unverified until the first real report; spike S6.)
//!
//! **Never twice.** Every session goes into a queue (`playtime-queue.json`) with an id; an
//! id is only ever queued once. Just before a session is sent it is written down as
//! `unconfirmed`, and only Epic's answer moves it on: accepted → `reported`; an error reply
//! → back to `queued` (5xx, 429, an expired session) or `failed` (anything else); no
//! connection at all → `queued` (it never left). A request that went out and got no answer
//! stays `unconfirmed` and is not sent again on its own, because it may have counted.
//! Queued sessions are retried later (quiverd does every few minutes).
//!
//! **The switch.** `report_playtime` (covers every store) off: sessions are kept here only
//! (`off`). On, before the person's first yes: `held`; the first `PlaytimeSend` from the
//! app or a terminal opens the gate and sends them (steer 11: on by default once verified).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use quiver_api::GameRef;
use quiver_api::epicgame::{EpicPlaytime, QueuedSession, SessionReport};
use serde::{Deserialize, Serialize};

use crate::Epic;
use crate::api::{Api, EpicError, LIBRARY_HOST, urlencode};
use crate::http::{Method, Request};
use crate::progress::{WriteKind, read_json, write_json};

/// Totals are read again after this.
const TOTALS_TTL: chrono::Duration = chrono::Duration::minutes(10);
/// Retries of a session Epic answered with a temporary error before it is `failed`.
const MAX_ATTEMPTS: u32 = 10;
/// Reported and failed sessions are kept this long (their ids keep them from being queued
/// again).
const KEEP_DAYS: i64 = 180;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Total {
    pub artifact_id: String,
    pub total_time: u64,
}

/// How a report went.
#[derive(Debug)]
pub enum Sent {
    Accepted,
    /// Epic answered with an error.
    Refused(EpicError),
    /// The request never left (no connection).
    NotSent(String),
    /// It went out and no answer came back.
    NoAnswer(String),
}

fn playtime_url(account: &str) -> String {
    format!("{LIBRARY_HOST}/library/api/public/playtime/account/{}", urlencode(account))
}

/// Whether a transport error means the request can't have reached Epic.
fn never_left(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("couldn't connect") || s.contains("nothing scripted")
}

impl Api {
    pub async fn playtime_totals(&self, token: &str, account: &str) -> Result<Vec<Total>> {
        self.json(Request::new("playtime totals", Method::Get, format!("{}/all", playtime_url(account))).bearer(token)).await
    }

    /// Report one session.
    pub async fn playtime_report(&self, token: &str, account: &str, machine: &str, app: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> Sent {
        let body = serde_json::json!({
            "machineId": machine,
            "artifactId": app,
            "startTime": start.to_rfc3339_opts(SecondsFormat::Millis, true),
            "endTime": end.to_rfc3339_opts(SecondsFormat::Millis, true),
            "startSegment": true,
            "endSegment": true,
        });
        let req = Request::new("playtime report", Method::Put, playtime_url(account)).bearer(token).json(&body);
        match self.send(req).await {
            Ok(r) if r.ok() => Sent::Accepted,
            Ok(r) => Sent::Refused(EpicError::from_response("playtime report", &r)),
            Err(e) if never_left(&e) => Sent::NotSent(format!("{e:#}")),
            Err(e) => Sent::NoAnswer(format!("{e:#}")),
        }
    }
}

/// `playtime-queue.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Queue {
    pub sessions: Vec<QueuedSession>,
}

impl Queue {
    fn path(epic_dir: &Path) -> std::path::PathBuf {
        epic_dir.join("playtime-queue.json")
    }

    pub fn load(epic_dir: &Path) -> Result<Queue> {
        read_json(&Self::path(epic_dir))
    }

    fn save(&self, epic_dir: &Path) -> Result<()> {
        write_json(&Self::path(epic_dir), self)
    }
}

/// A finished session, as a launch hands it over.
#[derive(Debug, Clone)]
pub struct Played {
    /// The launch's session id (`s-…`): the report's id.
    pub id: String,
    pub app: String,
    pub account_id: String,
    pub started: DateTime<Utc>,
    pub ended: DateTime<Utc>,
}

impl Epic {
    /// Queue a finished session (once per id). `report`: the `report_playtime` setting.
    pub async fn queue_session(&self, p: &Played, report: bool) -> Result<QueuedSession> {
        let game = crate::provider::game_ref(&p.app).context("not an Epic app name")?;
        let _q = self.p3.queue.lock().await;
        let mut q = Queue::load(&self.dir)?;
        if let Some(s) = q.sessions.iter().find(|s| s.id == p.id) {
            return Ok(s.clone());
        }
        let state = match (report, self.writes()?.playtime.open()) {
            (false, _) => SessionReport::Off,
            (true, false) => SessionReport::Held,
            (true, true) => SessionReport::Queued,
        };
        let s = QueuedSession {
            id: p.id.clone(),
            game,
            account_id: p.account_id.clone(),
            started: p.started,
            ended: p.ended,
            seconds: (p.ended - p.started).num_seconds().max(0) as u64,
            state,
            attempts: 0,
            last_error: None,
            reported_at: None,
        };
        let cutoff = Utc::now() - chrono::Duration::days(KEEP_DAYS);
        q.sessions.retain(|s| !matches!(s.state, SessionReport::Reported | SessionReport::Failed | SessionReport::Off) || s.ended > cutoff);
        q.sessions.push(s.clone());
        q.save(&self.dir)?;
        Ok(s)
    }

    /// Send what's queued. `report`: `report_playtime`; `first_ok`: the person said yes to
    /// the first report (opens the gate, and sends `held` sessions too).
    pub async fn send_queue(&self, report: bool, first_ok: bool) -> Result<Vec<QueuedSession>> {
        if !report {
            return Ok(Vec::new());
        }
        if first_ok {
            self.allow_writes(WriteKind::Playtime)?;
        }
        if !self.writes()?.playtime.open() {
            return Ok(Vec::new());
        }
        let machine = self.machine_id()?;
        let _q = self.p3.queue.lock().await;
        let mut q = Queue::load(&self.dir)?;
        let mut touched = Vec::new();
        for i in 0..q.sessions.len() {
            if !matches!(q.sessions[i].state, SessionReport::Queued | SessionReport::Held) {
                continue;
            }
            let s = q.sessions[i].clone();
            let token = match self.access_token(&s.account_id).await {
                Ok(t) => t,
                Err(e) => {
                    q.sessions[i].last_error = Some(format!("{e:#}"));
                    continue;
                }
            };
            // Written down before it goes: a crash mid-send must not send it again.
            q.sessions[i].state = SessionReport::Unconfirmed;
            q.sessions[i].attempts += 1;
            q.save(&self.dir)?;
            let sent = self.api.playtime_report(token.expose(), &s.account_id, &machine, s.game.id.as_str(), s.started, s.ended).await;
            let e = &mut q.sessions[i];
            match sent {
                Sent::Accepted => {
                    e.state = SessionReport::Reported;
                    e.reported_at = Some(Utc::now());
                    e.last_error = None;
                    self.mark_verified(WriteKind::Playtime)?;
                }
                Sent::Refused(err) => {
                    let temporary = err.status >= 500 || err.status == 429 || err.status == 401;
                    e.state = if temporary && e.attempts < MAX_ATTEMPTS { SessionReport::Queued } else { SessionReport::Failed };
                    e.last_error = Some(format!("HTTP {}: {err}", err.status));
                }
                Sent::NotSent(why) => {
                    e.state = SessionReport::Queued;
                    e.attempts = e.attempts.saturating_sub(1);
                    e.last_error = Some(why);
                }
                Sent::NoAnswer(why) => {
                    e.last_error = Some(format!("{why}; not sent again, so it can't count twice"));
                }
            }
            tracing::info!(session = e.id, state = ?e.state, "Epic play-time report");
            q.save(&self.dir)?;
            touched.push(q.sessions[i].clone());
        }
        if touched.iter().any(|s| s.state == SessionReport::Reported) {
            self.p3.totals.lock().await.clear();
        }
        Ok(touched)
    }

    /// Epic's totals for an account (app name → seconds), from the last read when recent.
    pub async fn playtime_totals(&self, account: &str, refresh: bool) -> Result<(DateTime<Utc>, HashMap<String, u64>)> {
        let mut cache = self.p3.totals.lock().await;
        if let Some((at, t)) = cache.get(account)
            && !refresh
            && Utc::now() - *at < TOTALS_TTL
        {
            return Ok((*at, t.clone()));
        }
        let token = self.access_token(account).await?;
        let list = self.api.playtime_totals(token.expose(), account).await?;
        let mut totals: HashMap<String, u64> = HashMap::new();
        for t in list {
            *totals.entry(t.artifact_id).or_default() += t.total_time;
        }
        let now = Utc::now();
        cache.insert(account.to_owned(), (now, totals.clone()));
        Ok((now, totals))
    }

    /// Epic's total for a game, for its account (`None`: Epic has no time for it).
    pub async fn playtime_total(&self, app: &str, refresh: bool) -> Result<Option<u64>> {
        let t = self.title_of(app).await?;
        let (account, _) = self.owner_of(&t)?;
        Ok(self.playtime_totals(&account, refresh).await?.1.get(app).copied())
    }

    /// `EpicGameRequest::Playtime`.
    pub async fn playtime_view(&self, game: Option<&GameRef>, refresh: bool, reporting: bool) -> Result<EpicPlaytime> {
        let mut out = EpicPlaytime { game: game.copied(), reporting, reports: self.writes()?.playtime, ..Default::default() };
        let account = match game {
            Some(g) => {
                let t = self.title_of(g.id.as_str()).await?;
                Some(self.owner_of(&t)?.0)
            }
            None => self.accounts()?.into_iter().find(|a| a.default).map(|a| a.account_id),
        };
        if let Some(a) = account {
            match self.playtime_totals(&a, refresh).await {
                Ok((at, totals)) => {
                    out.fetched = Some(at);
                    match game {
                        Some(g) => out.total_seconds = Some(totals.get(g.id.as_str()).copied().unwrap_or(0)),
                        None => {
                            let mut list: Vec<(String, u64)> = totals.into_iter().collect();
                            list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                            out.games = list;
                        }
                    }
                }
                Err(e) => out.error = Some(format!("{e:#}")),
            }
        }
        let mut q = Queue::load(&self.dir)?.sessions;
        q.retain(|s| game.is_none_or(|g| s.game == *g));
        q.sort_by_key(|s| std::cmp::Reverse(s.ended));
        out.queue = q;
        Ok(out)
    }
}
