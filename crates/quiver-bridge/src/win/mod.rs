use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use quiver_ipc::run::BridgeArgs;
use quiver_ipc::{BrokerRequest, BrokerResponse, LaunchContext};

mod api;
mod aux;
mod mods;

use crate::{CONTEXT, context, hex};

fn load_context(path: Option<&str>) -> Result<Option<LaunchContext>> {
    let Some(path) = path.map(str::to_owned).or_else(|| crate::env_var("QUIVER_BRIDGE_CONTEXT", "UBI_BRIDGE_CONTEXT")) else {
        log!("no launch context: serving placeholder data");
        return Ok(None);
    };
    let ctx: LaunchContext = serde_json::from_slice(&std::fs::read(&path)?)?;
    log!(
        "loaded launch context for uplay id {} ({}, {} chunks) from {path}",
        ctx.uplay_id,
        ctx.username,
        ctx.chunks.len()
    );
    Ok(Some(ctx))
}

#[derive(Clone, Copy)]
enum Kind {
    Api,
    Aux,
}

const PIPES: [(&str, Kind); 3] = [
    (r"\\.\pipe\terminal_1_uplay_api_process_ipc_pipe_", Kind::Api),
    (r"\\.\pipe\terminal_1_uplay_ipc_pipe_", Kind::Api),
    (r"\\.\pipe\terminal_1_uplay_aux_ipc_pipe_", Kind::Aux),
];

fn new_server(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    ServerOptions::new().first_pipe_instance(first).pipe_mode(PipeMode::Byte).create(name)
}

pub(crate) static CONN_IDS: AtomicU32 = AtomicU32::new(0);

/// SDK connections being served right now (all pipes).
static LIVE: AtomicUsize = AtomicUsize::new(0);

pub async fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = BridgeArgs::parse(&argv).map_err(|e| anyhow!(e))?;
    let log_path = match &args {
        BridgeArgs::Run { log: Some(l), .. } => Some(l.clone()),
        _ => crate::env_var("QUIVER_BRIDGE_LOG", "UBI_BRIDGE_LOG"),
    };
    if let Some(path) = log_path {
        *crate::LOG.lock().unwrap() = Some(std::fs::OpenOptions::new().create(true).append(true).open(path)?);
    }
    // Also what `quiver lab` looks for in this binary before using it in an instance.
    log!("quiver-bridge {} ({})", env!("CARGO_PKG_VERSION"), quiver_ipc::LAB_PROTOCOL_MARKER);
    match &args {
        BridgeArgs::Serve { context } => CONTEXT.set(load_context(context.as_deref())?).ok(),
        BridgeArgs::Run { context, .. } => {
            let ctx = load_context(Some(context))?;
            // It holds a ticket and is written fresh for every launch: don't leave it around.
            if let Err(e) = std::fs::remove_file(context) {
                log!("could not remove {context}: {e}");
            }
            CONTEXT.set(ctx).ok()
        }
    };
    // The mod pipe first: once the Connect pipes are up (what the launcher waits for), the
    // game may start, and its mods connect early.
    let mods_pipe = mods::pipe_name();
    mods::start(&mods_pipe);
    // The mods-only role (another store's game): the SDK pipe and its relay, nothing of
    // Connect. The marker is what launchers check for before giving a bridge this role.
    if context().is_some_and(|c| c.mods_only) {
        log!("mods-only ({}): no Connect pipes, no Ubisoft services", quiver_ipc::MODS_ONLY_MARKER);
        if let BridgeArgs::Run { exe, args, .. } = args {
            let code = match run_game(&exe, &args, &mods_pipe).await {
                Ok(c) => c,
                Err(e) => {
                    log!("{e:#}");
                    1
                }
            };
            std::process::exit(code);
        }
        // Serving for a game quiverd starts itself (a sibling): until it's stopped.
        std::future::pending::<()>().await;
    }
    // Own every pipe before anything can look for Connect.
    let mut tasks = Vec::new();
    for (name, kind) in PIPES {
        let mut server = new_server(name, true)
            .map_err(|e| anyhow!("{name}: {e} (is Ubisoft Connect or another bridge running in this prefix?)"))?;
        log!("listening on {name}");
        tasks.push(tokio::spawn(async move {
            loop {
                server.connect().await?;
                let n = CONN_IDS.fetch_add(1, Ordering::Relaxed) + 1;
                log!("[conn {n}] client connected to {name}");
                let client = std::mem::replace(&mut server, new_server(name, false)?);
                LIVE.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    match kind {
                        Kind::Api => api::serve(n, client).await,
                        Kind::Aux => aux::serve(n, client).await,
                    }
                    LIVE.fetch_sub(1, Ordering::SeqCst);
                });
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        }));
    }
    if let BridgeArgs::Run { exe, args, .. } = args {
        let code = match run_game(&exe, &args, &mods_pipe).await {
            Ok(c) => c,
            Err(e) => {
                log!("{e:#}");
                1
            }
        };
        std::process::exit(code);
    }
    for t in tasks {
        t.await??;
    }
    Ok(())
}

/// Launcher mode: start the game as Connect would, wait for it, then for any process it
/// handed the SDK connection to (a relaunch), and return its exit code.
async fn run_game(exe: &str, args: &[String], mods_pipe: &str) -> Result<i32> {
    let ctx = context().ok_or_else(|| anyhow!("--run needs a launch context"))?;
    let workdir = quiver_ipc::run::win_parent(exe);
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args).current_dir(workdir);
    if ctx.mods_only {
        // Another store's arguments may carry its licence (Epic's exchange code): not logged.
        log!("starting {exe} with {} argument(s) in {workdir} (mods only)", args.len());
    } else {
        let uplay_args = quiver_ipc::run::uplay_arguments(ctx.uplay_id, exe, workdir);
        log!("starting {exe} {args:?} in {workdir} (UPLAY_ARGUMENTS for {})", ctx.uplay_id);
        cmd.env("UPLAY_ARGUMENTS", &uplay_args);
    }
    // Here the bridge is the game's parent: it tells the game (and its mods) where to
    // connect itself.
    cmd.env("QUIVER_MODS_PIPE", mods_pipe);
    if let Some(s) = context().and_then(|c| c.session.as_deref()) {
        cmd.env("QUIVER_SESSION", s);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("starting {exe}: {e}"))?;
    let status = child.wait().await?;
    let code = status.code().unwrap_or(1);
    log!("game exited ({code}); waiting for SDK connections to close");
    // A game that relaunches itself connects again from the new process; give it a moment.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    while LIVE.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    log!("no SDK connections left; exiting");
    Ok(code)
}

pub async fn read_frame(pipe: &mut NamedPipeServer) -> std::io::Result<Vec<u8>> {
    let len = pipe.read_u32().await? as usize;
    let mut body = vec![0u8; len];
    pipe.read_exact(&mut body).await?;
    Ok(body)
}

pub async fn write_frame(pipe: &mut NamedPipeServer, msg: &impl Message) -> std::io::Result<()> {
    let out = msg.encode_to_vec();
    pipe.write_u32(out.len() as u32).await?;
    pipe.write_all(&out).await?;
    pipe.flush().await
}

/// One request/response round trip to the native broker named in the launch context.
pub async fn broker(req: BrokerRequest) -> Result<BrokerResponse> {
    let addr = context()
        .and_then(|c| c.broker.clone())
        .ok_or_else(|| anyhow!("no broker in launch context"))?;
    let stream = tokio::net::TcpStream::connect(&addr).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    let mut rsp = String::new();
    tokio::io::BufReader::new(r).read_line(&mut rsp).await?;
    Ok(serde_json::from_str(&rsp)?)
}

/// The game's achievements with the account's unlock status, from quiverd.
pub async fn load_achievements(language: Option<String>) -> Result<()> {
    let ctx = context().ok_or_else(|| anyhow!("no launch context"))?;
    let req = BrokerRequest::Achievements { uplay_id: ctx.uplay_id, account_id: Some(ctx.account_id.clone()), language };
    match broker(req).await? {
        BrokerResponse::Achievements { items, unlocked_known } => {
            let earned = items.iter().filter(|a| a.earned).count();
            log!(
                "achievements from quiverd: {} defined, {earned} earned{}",
                items.len(),
                if unlocked_known { "" } else { " (Ubisoft's unlock status unavailable)" }
            );
            crate::achievements::set_list(items);
            Ok(())
        }
        BrokerResponse::Error { message } => Err(anyhow!("broker: {message}")),
        other => Err(anyhow!("unexpected broker reply {other:?}")),
    }
}

/// Hand an earned achievement to quiverd (it deduplicates and reports it).
pub async fn achievement_earned(achievement_id: u32) -> Result<()> {
    let ctx = context().ok_or_else(|| anyhow!("no launch context"))?;
    let req = BrokerRequest::AchievementEarned { uplay_id: ctx.uplay_id, account_id: Some(ctx.account_id.clone()), achievement_id, session: ctx.session.clone() };
    match broker(req).await? {
        BrokerResponse::Ok => Ok(()),
        BrokerResponse::Error { message } => Err(anyhow!("broker: {message}")),
        other => Err(anyhow!("unexpected broker reply {other:?}")),
    }
}

/// Tell the broker the game is running. The returned connection is the lease: the broker
/// keeps the play session open until it closes.
pub async fn play_session_lease() -> Result<tokio::net::TcpStream> {
    let ctx = context().ok_or_else(|| anyhow!("no launch context"))?;
    let addr = ctx.broker.clone().ok_or_else(|| anyhow!("no broker in launch context"))?;
    let mut stream = tokio::net::TcpStream::connect(&addr).await?;
    let mut line = serde_json::to_vec(&BrokerRequest::PlaySession { uplay_id: ctx.uplay_id, account_id: Some(ctx.account_id.clone()), session: ctx.session.clone() })?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut rsp = String::new();
    tokio::io::BufReader::new(&mut stream).read_line(&mut rsp).await?;
    match serde_json::from_str::<BrokerResponse>(&rsp)? {
        BrokerResponse::Ok => Ok(stream),
        BrokerResponse::Error { message } => Err(anyhow!("broker: {message}")),
        other => Err(anyhow!("unexpected broker reply {other:?}")),
    }
}

#[allow(dead_code)]
fn flush_stdout() {
    let _ = std::io::stdout().flush();
}
