//! `uplay_aux_r164.dll`: init, then Denuvo activation relayed to the native broker.

use anyhow::Result;
use prost::Message;
use tokio::net::windows::named_pipe::NamedPipeServer;
use ubi_ipc::{BrokerRequest, BrokerResponse};
use ubi_proto::uplayauxdll as aux;

use super::{broker, context, hex, read_frame, write_frame};

pub async fn serve(n: u32, mut pipe: NamedPipeServer) {
    let result: Result<()> = async {
        loop {
            let body = read_frame(&mut pipe).await?;
            let req = match aux::Req::decode(body.as_slice()) {
                Ok(r) => r,
                Err(e) => {
                    log!("[aux {n}] undecodable frame ({e}): {}", hex(&body));
                    continue;
                }
            };
            if let Some(rsp) = handle(n, &req).await {
                write_frame(&mut pipe, &rsp).await?;
            }
        }
    }
    .await;
    log!("[aux {n}] closed: {result:?}");
}

async fn handle(n: u32, req: &aux::Req) -> Option<aux::Rsp> {
    let reply = |init_rsp, activate_rsp| aux::Rsp { request_id: req.request_id, init_rsp, activate_rsp };
    if let Some(init) = &req.init_req {
        log!("[aux {n}] <- InitReq uplay id {}", init.uplay_id);
        let init = aux::InitRsp { result: aux::InitResult::Success as i32, uplay_pid: Some(std::process::id()) };
        return Some(reply(Some(init), None));
    }
    let act = req.activate_req.as_ref()?;
    if let Some(t) = &act.get_cached_or_fresh_token_req {
        log!("[aux {n}] <- GetCachedOrFreshToken (request token {} bytes)", t.request_token.len());
        let uplay_id = context().map(|c| c.uplay_id).unwrap_or_default();
        let token = broker(BrokerRequest::DenuvoGameToken { uplay_id, request_token: t.request_token.clone() }).await;
        let rsp = match token {
            Ok(BrokerResponse::GameToken { game_token }) => {
                log!("[aux {n}] -> game token ({} bytes)", game_token.len());
                aux::GetCachedOrFreshTokenRsp {
                    result: aux::OperationResult::Ok as i32,
                    error_code: None,
                    game_token: Some(game_token),
                }
            }
            other => {
                log!("[aux {n}] -> activation failed: {other:?}");
                aux::GetCachedOrFreshTokenRsp {
                    result: aux::OperationResult::ConnectionError as i32,
                    error_code: None,
                    game_token: None,
                }
            }
        };
        let activate = aux::ActivateRsp { get_cached_or_fresh_token_rsp: Some(rsp), get_burn_ticket_rsp: None };
        return Some(reply(None, Some(activate)));
    }
    if act.invalidate_cached_token_req.is_some() {
        // The broker never caches tokens, so there is nothing to invalidate.
        log!("[aux {n}] <- InvalidateCachedToken");
        return Some(reply(None, None));
    }
    if let Some(t) = &act.get_burn_ticket_req {
        log!("[aux {n}] <- GetBurnTicket (request token {} bytes): not implemented", t.request_token.len());
    }
    None
}
