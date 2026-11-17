//! The `uplaydll` SDK API pipe (`uplay_r164.dll`): one connection per SDK instance, each
//! answered by [`crate::respond`], plus the play-session lease the game's SDK init takes.

use anyhow::Result;
use prost::Message;
use tokio::net::windows::named_pipe::NamedPipeServer;
use ubi_proto::uplaydll as u;

use super::{read_frame, write_frame};
use crate::hex;
use crate::respond::{Reply, describe, respond, summarize};

pub async fn serve(n: u32, mut pipe: NamedPipeServer) {
    // This connection's share of the play-session lease, taken at the game's SDK init and
    // released when the connection closes.
    let mut lease: Option<LeaseShare> = None;
    let result: Result<()> = async {
        loop {
            let body = read_frame(&mut pipe).await?;
            let req = match u::Req::decode(body.as_slice()) {
                Ok(r) => r,
                Err(e) => {
                    log!("[conn {n}] undecodable {}-byte frame ({e}): {}", body.len(), hex(&body));
                    continue;
                }
            };
            log!("[conn {n}] <- {}", describe(&req));
            if req.init_req.is_some() && lease.is_none() {
                lease = Some(LeaseShare::take(n));
            }
            match respond(&req) {
                Reply::Rsp(rsp) => {
                    log!("[conn {n}] -> {}", summarize(&rsp));
                    write_frame(&mut pipe, &rsp).await?;
                }
                Reply::None => {}
                Reply::Unhandled => log!(
                    "[conn {n}]    (unhandled, no reply; Req fields {:?}, {} bytes: {})",
                    ubi_ipc::field_numbers(&body).unwrap_or_default(),
                    body.len(),
                    hex(&body[..body.len().min(64)])
                ),
            }
        }
    }
    .await;
    drop(lease);
    log!("[conn {n}] closed: {result:?}");
}

/// How long the lease outlives the last SDK connection, so a game that reconnects (a
/// loading screen, a second DLL generation starting up) keeps one session.
const LEASE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// One play-session lease for the whole process, shared by the game's SDK connections:
/// a game keeps several open at once (Black Flag Resynced: three, from two SDK API
/// versions), and each used to hold a lease of its own.
#[derive(Default)]
struct SharedLease {
    shares: usize,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Bumped on every take, so a grace timer knows if someone came back.
    generation: u64,
}

static LEASE: std::sync::Mutex<SharedLease> = std::sync::Mutex::new(SharedLease { shares: 0, task: None, generation: 0 });

struct LeaseShare;

impl LeaseShare {
    fn take(n: u32) -> Self {
        let mut l = LEASE.lock().unwrap();
        l.shares += 1;
        l.generation += 1;
        if l.task.is_none() {
            l.task = Some(tokio::spawn(keep_lease(n)));
        } else {
            log!("[conn {n}]    sharing the play session lease ({} connections)", l.shares);
        }
        LeaseShare
    }
}

impl Drop for LeaseShare {
    fn drop(&mut self) {
        let generation = {
            let mut l = LEASE.lock().unwrap();
            l.shares = l.shares.saturating_sub(1);
            if l.shares > 0 {
                return;
            }
            l.generation
        };
        tokio::spawn(async move {
            tokio::time::sleep(LEASE_GRACE).await;
            let mut l = LEASE.lock().unwrap();
            if l.shares == 0 && l.generation == generation
                && let Some(t) = l.task.take()
            {
                t.abort();
                log!("play session lease released (no SDK connections left)");
            }
        });
    }
}

/// Hold a play-session lease with the broker for as long as this task runs, reopening it
/// whenever the broker goes away (e.g. quiverd restarting) so the session stays reported.
async fn keep_lease(n: u32) {
    use tokio::io::AsyncReadExt;
    loop {
        match super::play_session_lease().await {
            Ok(mut s) => {
                log!("[conn {n}]    play session lease open");
                let mut buf = [0u8; 64];
                // The broker never writes after its reply; a read returns only on close.
                while matches!(s.read(&mut buf).await, Ok(k) if k > 0) {}
                log!("[conn {n}]    play session lease lost; reconnecting");
            }
            Err(e) => log!("[conn {n}]    play session lease failed: {e:#}; retrying"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
