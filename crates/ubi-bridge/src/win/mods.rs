//! The mod SDK pipe (`QUIVER_MODS_PIPE`): mods inside the game (`sdk/quiver_mod.h`) connect
//! here, and each connection is relayed to quiverd as a broker mod channel
//! (`ubi_ipc::modrelay`, which also does the checking: the game process is untrusted).
//!
//! The name comes from the launch context, so when quiverd starts the game itself (the bridge
//! is the game's sibling) it can put the same name in the game's environment; in launcher
//! mode (`--run`, the bridge is the parent) the bridge sets it on the game.

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use ubi_ipc::{BrokerRequest, modrelay};
use ubi_proto::modsdk;

use super::{CONN_IDS, context};

/// Mod connections served at once; more are closed right away (the SDK retries later).
const MAX_LIVE: usize = 8;

static LIVE: AtomicUsize = AtomicUsize::new(0);

fn valid(name: &str) -> bool {
    name.len() <= 200
        && name
            .strip_prefix(r"\\.\pipe\")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_graphic() && b != b'\\'))
}

/// The pipe this bridge serves: the launch context's, else `QUIVER_MODS_PIPE` from its own
/// environment, else the session's (or the prefix-wide) default name.
pub fn pipe_name() -> String {
    context()
        .and_then(|c| c.mods_pipe.clone())
        .filter(|n| valid(n))
        .or_else(|| std::env::var("QUIVER_MODS_PIPE").ok().filter(|n| valid(n)))
        .unwrap_or_else(|| modsdk::pipe_name(context().and_then(|c| c.session.as_deref())))
}

fn server(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        .pipe_mode(PipeMode::Byte)
        .in_buffer_size(64 * 1024)
        .out_buffer_size(64 * 1024)
        .create(name)
}

/// Create the pipe and accept mods on it. A failure is logged, never fatal: the game runs
/// without mod channels.
pub fn start(name: &str) {
    let mut listener = match server(name, true) {
        Ok(s) => s,
        Err(e) => {
            log!("mod pipe {name}: {e}; mods can't reach Quiver this session");
            return;
        }
    };
    log!("listening on {name} (mods)");
    let name = name.to_owned();
    tokio::spawn(async move {
        loop {
            if let Err(e) = listener.connect().await {
                log!("mod pipe: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            let next = match server(&name, false) {
                Ok(s) => s,
                Err(e) => {
                    log!("mod pipe {name}: {e}; no more mod connections");
                    return;
                }
            };
            let client = std::mem::replace(&mut listener, next);
            let n = CONN_IDS.fetch_add(1, Ordering::Relaxed) + 1;
            if LIVE.load(Ordering::SeqCst) >= MAX_LIVE {
                log!("[mod {n}] refused: {MAX_LIVE} mods are connected already");
                continue;
            }
            LIVE.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                serve(n, client).await;
                LIVE.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}

async fn serve(n: u32, pipe: NamedPipeServer) {
    let client_pid = client_pid(&pipe);
    log!("[mod {n}] connected (pid {})", client_pid.map_or("?".into(), |p| p.to_string()));
    let s = modrelay::relay(pipe, || open(n, client_pid), &modrelay::Limits::default()).await;
    log!(
        "[mod {n}] {} closed: {} ({} sent, {} received, {} malformed, {} over the rate)",
        s.surface.as_deref().unwrap_or("(no hello)"),
        s.end,
        s.up,
        s.down,
        s.dropped_invalid,
        s.dropped_rate
    );
}

/// A broker connection turned into this mod's channel.
async fn open(n: u32, client_pid: Option<u32>) -> std::io::Result<tokio::io::BufReader<tokio::net::TcpStream>> {
    let ctx = context().ok_or_else(|| std::io::Error::other("no launch context"))?;
    let addr = ctx.broker.clone().ok_or_else(|| std::io::Error::other("no broker in the launch context"))?;
    let stream = tokio::net::TcpStream::connect(&addr).await?;
    let req = BrokerRequest::ModChannel {
        uplay_id: ctx.uplay_id,
        account_id: Some(ctx.account_id.clone()),
        session: ctx.session.clone(),
        profile: ctx.profile.clone(),
        conn: n,
        client_pid,
    };
    modrelay::open_channel(stream, &req).await
}

/// The Windows pid of the process at the other end, as Wine knows it.
fn client_pid(pipe: &NamedPipeServer) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetNamedPipeClientProcessId(pipe: *mut std::ffi::c_void, pid: *mut u32) -> i32;
    }
    let mut pid = 0u32;
    // SAFETY: a valid pipe handle for the duration of the call and a valid out pointer.
    (unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } != 0).then_some(pid)
}
