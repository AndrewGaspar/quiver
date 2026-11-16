use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, anyhow};
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
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

fn load_context() -> Result<Option<LaunchContext>> {
    let Some(path) = std::env::args().nth(1).or_else(|| std::env::var("UBI_BRIDGE_CONTEXT").ok()) else {
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

pub async fn run() -> Result<()> {
    if let Ok(path) = std::env::var("UBI_BRIDGE_LOG") {
        *LOG.lock().unwrap() = Some(std::fs::OpenOptions::new().create(true).append(true).open(path)?);
    }
    CONTEXT.set(load_context()?).ok();
    let mut tasks = Vec::new();
    for (name, kind) in PIPES {
        let mut server = new_server(name, true)?;
        log!("listening on {name}");
        tasks.push(tokio::spawn(async move {
            loop {
                server.connect().await?;
                let n = CONN_IDS.fetch_add(1, Ordering::Relaxed) + 1;
                log!("[conn {n}] client connected to {name}");
                let client = std::mem::replace(&mut server, new_server(name, false)?);
                match kind {
                    Kind::Api => tokio::spawn(api::serve(n, client)),
                    Kind::Aux => tokio::spawn(aux::serve(n, client)),
                };
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        }));
    }
    for t in tasks {
        t.await??;
    }
    Ok(())
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

#[allow(dead_code)]
fn flush_stdout() {
    let _ = std::io::stdout().flush();
}
