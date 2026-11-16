//! The native broker: holds the Ubisoft session on the host and answers live requests
//! from `ubi-bridge` instances (one JSON object per line over TCP).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use ubi_ipc::{BrokerRequest, BrokerResponse};
use ubi_proto::denuvo_service;

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
}

impl Broker {
    /// `sessions`: where bridges' play sessions are tracked (none: answered but not tracked).
    pub fn new(accounts: Arc<Accounts>, sessions: Option<Arc<PlaySessions>>) -> Arc<Self> {
        Arc::new(Self { accounts, conns: Mutex::new(HashMap::new()), sessions })
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            tracing::info!(%peer, "bridge connected");
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle_conn(stream).await {
                    tracing::warn!(%peer, "bridge connection ended: {e:#}");
                }
            });
        }
    }

    async fn handle_conn(&self, stream: TcpStream) -> Result<()> {
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        // A play-session lease, held until the bridge closes this connection.
        let mut _lease = None;
        while let Some(line) = lines.next_line().await? {
            let rsp = match serde_json::from_str::<BrokerRequest>(&line) {
                Ok(BrokerRequest::PlaySession { uplay_id, account_id }) => {
                    tracing::info!(uplay_id, "bridge reports game running");
                    _lease = self.sessions.as_ref().map(|s| s.begin(account_id.as_deref(), uplay_id, "bridge"));
                    BrokerResponse::Ok
                }
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
        // One retry on a fresh connection: the demux socket may have gone stale.
        let account = match &req {
            BrokerRequest::DenuvoGameToken { account_id, .. } | BrokerRequest::PlaySession { account_id, .. } => account_id.clone(),
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
            match Self::handle_online(conns, &req).await {
                Ok(rsp) => return Ok(rsp),
                Err(e) if attempt == 0 => {
                    tracing::warn!("retrying on a new demux connection after: {e:#}");
                    guard.remove(&user_id);
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn handle_online(online: &mut Conns, req: &BrokerRequest) -> Result<BrokerResponse> {
        match req {
            BrokerRequest::PlaySession { .. } => unreachable!("handled per connection"),
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
