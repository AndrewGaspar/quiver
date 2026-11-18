//! The native broker: holds the Ubisoft session on the host and answers live requests
//! from `quiver-bridge` instances (one JSON object per line over TCP).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use quiver_ipc::{BrokerRequest, BrokerResponse};
use quiver_proto::denuvo_service;

use crate::demux::{Demux, Denuvo, ServiceConnection};
use crate::library::Library;
use crate::online::{Accounts, Online};
use crate::playsession::PlaySessions;

struct Conns {
    demux: Demux,
    library: Library,
    denuvo: ServiceConnection<Denuvo>,
}

pub struct Broker {
    accounts: Arc<Accounts>,
    /// Live service connections per account (user id).
    conns: Mutex<HashMap<String, Conns>>,
    sessions: Option<Arc<PlaySessions>>,
    /// Where mod SDK channels go (none: refused).
    mods: std::sync::OnceLock<Arc<crate::modsdk::ModRegistry>>,
    /// Achievement lists and earns from games (none: refused).
    achievements: std::sync::OnceLock<Arc<crate::achievements::Engine>>,
}

/// The broker's answer to a lab instance's request that must not reach the person's
/// records, or `None` to handle it as usual:
/// - an earned achievement is answered `Ok` and neither recorded nor reported;
/// - a mod channel is refused: Options, perf history and mod revisions are the person's (a
///   lab mod is configured through its files, `quiver lab set`).
///
/// Play sessions are handled as usual but held as `playsession::LAB_HOLDER` (never
/// reported, never in the play history); Denuvo tokens and the achievement list are reads.
pub fn lab_answer(req: &BrokerRequest) -> Option<BrokerResponse> {
    use quiver_api::lab::is_lab_session;
    match req {
        BrokerRequest::AchievementEarned { session: Some(s), uplay_id, achievement_id, .. } if is_lab_session(s) => {
            tracing::info!(uplay_id, achievement_id, session = %s, "lab session earned an achievement: not recorded or reported");
            Some(BrokerResponse::Ok)
        }
        BrokerRequest::ModChannel { session: Some(s), .. } if is_lab_session(s) => Some(BrokerResponse::Error {
            message: "lab sessions have no mod channel (their mods are configured through files: quiver lab set)".into(),
        }),
        _ => None,
    }
}

impl Broker {
    /// `sessions`: where bridges' play sessions are tracked (none: answered but not tracked).
    pub fn new(accounts: Arc<Accounts>, sessions: Option<Arc<PlaySessions>>) -> Arc<Self> {
        Arc::new(Self { accounts, conns: Mutex::new(HashMap::new()), sessions, mods: std::sync::OnceLock::new(), achievements: std::sync::OnceLock::new() })
    }

    /// Accept mod SDK channels (`BrokerRequest::ModChannel`) into `registry`. Set once.
    pub fn set_mods(&self, registry: Arc<crate::modsdk::ModRegistry>) {
        let _ = self.mods.set(registry);
    }

    /// Answer games' achievement requests (`BrokerRequest::Achievements`,
    /// `AchievementEarned`) with `engine`. Set once.
    pub fn set_achievements(&self, engine: Arc<crate::achievements::Engine>) {
        let _ = self.achievements.set(engine);
    }

    /// Drop an account's service connections (after it signs out or signs in again), so
    /// the broker doesn't keep its old demux login alive.
    pub async fn forget(&self, user_id: &str) {
        self.conns.lock().await.remove(user_id);
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            tokio::spawn(self.clone().serve_one(stream, peer));
        }
    }

    /// Serve one bridge connection until it closes (for callers with their own accept loop).
    pub async fn serve_one(self: Arc<Self>, stream: TcpStream, peer: std::net::SocketAddr) {
        tracing::info!(%peer, "bridge connected");
        if let Err(e) = self.handle_conn(stream).await {
            tracing::warn!(%peer, "bridge connection ended: {e:#}");
        }
    }

    async fn handle_conn(&self, stream: TcpStream) -> Result<()> {
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        // A play-session lease, held until the bridge closes this connection.
        let mut _lease = None;
        while let Some(line) = lines.next_line().await? {
            let parsed = serde_json::from_str::<BrokerRequest>(&line);
            // A lab instance's requests that would reach the person's records (design/LAB.md).
            if let Some(rsp) = parsed.as_ref().ok().and_then(lab_answer) {
                let close = matches!(parsed, Ok(BrokerRequest::ModChannel { .. }));
                let mut out = serde_json::to_vec(&rsp)?;
                out.push(b'\n');
                w.write_all(&out).await?;
                if close {
                    return Ok(());
                }
                continue;
            }
            let rsp = match parsed {
                Ok(BrokerRequest::ModChannel { uplay_id, account_id, session, profile, conn, client_pid }) => {
                    let Some(registry) = self.mods.get().cloned() else {
                        let rsp = BrokerResponse::Error { message: "this quiverd takes no mod channels".into() };
                        let mut out = serde_json::to_vec(&rsp)?;
                        out.push(b'\n');
                        w.write_all(&out).await?;
                        return Ok(());
                    };
                    let mut out = serde_json::to_vec(&BrokerResponse::Ok)?;
                    out.push(b'\n');
                    w.write_all(&out).await?;
                    // From here on the connection is the mod's (one JSON message per line).
                    let origin = crate::modsdk::ChannelOrigin { product_id: uplay_id, account: account_id, session, profile, bridge_conn: conn, client_pid };
                    return registry.serve(origin, lines.into_inner(), w).await;
                }
                Ok(BrokerRequest::PlaySession { uplay_id, account_id, session }) => {
                    // A lab instance's bridge (design/LAB.md): tracked, never reported.
                    let holder = crate::playsession::bridge_holder(session.as_deref());
                    tracing::info!(uplay_id, holder, "bridge reports game running");
                    _lease = self.sessions.as_ref().map(|s| s.begin(account_id.as_deref(), uplay_id, holder));
                    BrokerResponse::Ok
                }
                Ok(BrokerRequest::Achievements { uplay_id, account_id, language }) => match self.achievements.get() {
                    None => BrokerResponse::Error { message: "this quiverd doesn't serve achievements".into() },
                    Some(engine) => {
                        let locale = language.filter(|l| !l.is_empty()).unwrap_or_else(|| "en-US".into());
                        let list = engine.bridge_list(account_id.as_deref(), uplay_id, &locale);
                        match tokio::time::timeout(std::time::Duration::from_secs(40), list).await {
                            Ok(Ok((items, unlocked_known))) => BrokerResponse::Achievements { items, unlocked_known },
                            Ok(Err(e)) => BrokerResponse::Error { message: format!("{e:#}") },
                            Err(_) => BrokerResponse::Error { message: "achievements: no answer from Ubisoft in time".into() },
                        }
                    }
                },
                Ok(BrokerRequest::AchievementEarned { uplay_id, account_id, achievement_id, .. }) => match self.achievements.get() {
                    None => BrokerResponse::Error { message: "this quiverd doesn't take achievement reports".into() },
                    Some(engine) => {
                        tracing::info!(uplay_id, achievement_id, "bridge reports an earned achievement");
                        let engine = engine.clone();
                        tokio::spawn(async move {
                            if let Err(e) = engine.earned(account_id.as_deref(), uplay_id, achievement_id).await {
                                tracing::warn!(uplay_id, achievement_id, "recording an earned achievement: {e:#}");
                            }
                        });
                        BrokerResponse::Ok
                    }
                },
                Ok(req) => self.handle(req).await.unwrap_or_else(|e| {
                    tracing::warn!("request failed: {e:#}");
                    BrokerResponse::Error { message: format!("{e:#}") }
                }),
                Err(e) => BrokerResponse::Error { message: format!("bad request: {e}") },
            };
            let mut out = serde_json::to_vec(&rsp)?;
            out.push(b'\n');
            w.write_all(&out).await?;
        }
        Ok(())
    }

    async fn connect(online: &Online) -> Result<Conns> {
        let demux = online.demux().await?;
        let library = Library::fetch(&demux).await?;
        let denuvo = demux.open::<Denuvo>().await?;
        Ok(Conns { demux, library, denuvo })
    }

    async fn handle(&self, req: BrokerRequest) -> Result<BrokerResponse> {
        // Known-stale connections are replaced before use: the ownership session by
        // `Library` itself (every launch context, install or library refresh replaces it),
        // the denuvo connection here. One retry on fresh connections covers the rest.
        let account = match &req {
            BrokerRequest::DenuvoGameToken { account_id, .. }
            | BrokerRequest::PlaySession { account_id, .. }
            | BrokerRequest::ModChannel { account_id, .. }
            | BrokerRequest::Achievements { account_id, .. }
            | BrokerRequest::AchievementEarned { account_id, .. } => account_id.clone(),
        };
        let online = self.accounts.get(account.as_deref())?;
        let user_id = online.session().await?.user_id;
        for attempt in 0..2 {
            let mut guard = self.conns.lock().await;
            if guard.get(&user_id).is_some_and(|c| !c.demux.is_alive()) {
                guard.remove(&user_id);
            }
            if !guard.contains_key(&user_id) {
                let c = Self::connect(&online).await?;
                guard.insert(user_id.clone(), c);
            }
            let conns = guard.get_mut(&user_id).unwrap();
            let result = async {
                if !conns.denuvo.is_open() {
                    tracing::info!(connection = conns.denuvo.id(), "denuvo_service connection was closed; reopening");
                    conns.denuvo = conns.demux.open::<Denuvo>().await?;
                }
                Self::handle_online(conns, &req).await
            }
            .await;
            match result {
                Ok(rsp) => return Ok(rsp),
                Err(e) if attempt == 0 => {
                    tracing::warn!("retrying on new service connections after: {e:#}");
                    guard.remove(&user_id);
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn handle_online(online: &mut Conns, req: &BrokerRequest) -> Result<BrokerResponse> {
        match req {
            BrokerRequest::PlaySession { .. }
            | BrokerRequest::ModChannel { .. }
            | BrokerRequest::Achievements { .. }
            | BrokerRequest::AchievementEarned { .. } => unreachable!("handled per connection"),
            BrokerRequest::DenuvoGameToken { uplay_id, request_token, .. } => {
                tracing::info!(uplay_id, len = request_token.len(), "denuvo game token requested");
                let ownership_token = online.library.ownership_token(*uplay_id).await?;
                let rsp = online
                    .denuvo
                    .request(denuvo_service::Req {
                        get_game_token_req: Some(denuvo_service::GetGameTokenReq {
                            ownership_token,
                            request_token: request_token.clone(),
                        }),
                        ..Default::default()
                    })
                    .await?;
                let result = denuvo_service::rsp::Result::try_from(rsp.result)
                    .map(|r| r.as_str_name())
                    .unwrap_or("unknown");
                if rsp.result != denuvo_service::rsp::Result::Success as i32 {
                    bail!("denuvo_service refused the token request: {result}");
                }
                let game_token = rsp
                    .get_game_token_rsp
                    .context("denuvo_service returned no game token")?
                    .game_token;
                tracing::info!(uplay_id, len = game_token.len(), "denuvo game token issued");
                Ok(BrokerResponse::GameToken { game_token })
            }
        }
    }
}

#[cfg(test)]
mod lab_tests {
    use super::*;

    #[test]
    fn lab_earns_are_not_recorded_and_lab_mod_channels_refused() {
        let earn = |session: Option<&str>| BrokerRequest::AchievementEarned { uplay_id: 3539, account_id: None, achievement_id: 7, session: session.map(str::to_owned) };
        assert!(matches!(lab_answer(&earn(Some("lab-t1-r1"))), Some(BrokerResponse::Ok)));
        assert!(lab_answer(&earn(Some("s-4kq2"))).is_none() && lab_answer(&earn(None)).is_none());
        let chan = BrokerRequest::ModChannel { uplay_id: 3539, account_id: None, session: Some("lab-t1-r1".into()), profile: None, conn: 1, client_pid: None };
        assert!(matches!(lab_answer(&chan), Some(BrokerResponse::Error { .. })));
        let play = BrokerRequest::PlaySession { uplay_id: 3539, account_id: None, session: Some("lab-t1-r1".into()) };
        assert!(lab_answer(&play).is_none(), "play sessions are held, as lab sessions");
    }
}
