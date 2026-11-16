use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use ubi_ipc::run::BridgeArgs;
use ubi_ipc::{BrokerRequest, BrokerResponse, LaunchContext};

static LOG: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// Log to stdout and to `UBI_BRIDGE_LOG` if set (stdout is unreliable under Proton).
macro_rules! log {
    ($($t:tt)*) => {{
        let line = format!("{} {}", crate::win::timestamp(), format!($($t)*));
        println!("{line}");
        if let Some(f) = crate::win::LOG.lock().unwrap().as_mut() {
            use std::io::Write as _;
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }};
}

/// Local wall-clock time as HH:MM:SS.mmm, matching the SDK proxy's log.
pub fn timestamp() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime { y: u16, mo: u16, dow: u16, d: u16, h: u16, mi: u16, s: u16, ms: u16 }
    unsafe extern "system" {
        fn GetLocalTime(t: *mut SystemTime);
    }
    let mut t = SystemTime::default();
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}.{:03}", t.h, t.mi, t.s, t.ms)
}

mod api;
mod aux;

static CONTEXT: OnceLock<Option<LaunchContext>> = OnceLock::new();

pub fn context() -> Option<&'static LaunchContext> {
    CONTEXT.get().and_then(Option::as_ref)
}

fn load_context(path: Option<&str>) -> Result<Option<LaunchContext>> {
    let Some(path) = path.map(str::to_owned).or_else(|| std::env::var("UBI_BRIDGE_CONTEXT").ok()) else {
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

static CONN_IDS: AtomicU32 = AtomicU32::new(0);

/// SDK connections being served right now (all pipes).
static LIVE: AtomicUsize = AtomicUsize::new(0);

pub async fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = BridgeArgs::parse(&argv).map_err(|e| anyhow!(e))?;
    let log_path = match &args {
        BridgeArgs::Run { log: Some(l), .. } => Some(l.clone()),
        _ => std::env::var("UBI_BRIDGE_LOG").ok(),
    };
    if let Some(path) = log_path {
        *LOG.lock().unwrap() = Some(std::fs::OpenOptions::new().create(true).append(true).open(path)?);
    }
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
        let code = match run_game(&exe, &args).await {
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
async fn run_game(exe: &str, args: &[String]) -> Result<i32> {
    let uplay_id = context().map(|c| c.uplay_id).ok_or_else(|| anyhow!("--run needs a launch context"))?;
    let workdir = ubi_ipc::run::win_parent(exe);
    let uplay_args = ubi_ipc::run::uplay_arguments(uplay_id, exe, workdir);
    log!("starting {exe} {args:?} in {workdir} (UPLAY_ARGUMENTS for {uplay_id})");
    let mut child = tokio::process::Command::new(exe)
        .args(args)
        .current_dir(workdir)
        .env("UPLAY_ARGUMENTS", &uplay_args)
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

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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

/// Tell the broker the game is running. The returned connection is the lease: the broker
/// keeps the play session open until it closes.
pub async fn play_session_lease() -> Result<tokio::net::TcpStream> {
    let ctx = context().ok_or_else(|| anyhow!("no launch context"))?;
    let addr = ctx.broker.clone().ok_or_else(|| anyhow!("no broker in launch context"))?;
    let mut stream = tokio::net::TcpStream::connect(&addr).await?;
    let mut line = serde_json::to_vec(&BrokerRequest::PlaySession { uplay_id: ctx.uplay_id, account_id: Some(ctx.account_id.clone()) })?;
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
