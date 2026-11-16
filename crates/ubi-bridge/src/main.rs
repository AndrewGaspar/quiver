//! ubi-bridge: stands in for Connect on the named pipes the game's `uplay_r164.dll`
//! connects to (it spawns Connect only if they are missing):
//! - `terminal_1_uplay_api_process_ipc_pipe_`: per-process handshake (`InitProcessReq`)
//! - `terminal_1_uplay_ipc_pipe_`: the API session (`InitReq`, then everything else)
//! - `terminal_1_uplay_aux_ipc_pipe_`: `uplay_aux_r164.dll` (`uplayauxdll` protocol):
//!   init, then Denuvo activation (`GetCachedOrFreshTokenReq` → game token)
//!
//! Framing: 4-byte big-endian length + protobuf (`uplaydll.Req` from the SDK,
//! `uplaydll.Rsp` back).
//!
//! The bridge answers the init handshake from a [`LaunchContext`] JSON file prepared
//! by the native client (`ubi launch-context`), given as the first argument or via
//! `UBI_BRIDGE_CONTEXT`. Without one it serves placeholder data (recon mode).
//! Requests it does not understand yet are logged and left unanswered.

#[cfg(not(windows))]
fn main() {
    eprintln!("ubi-bridge runs inside Wine: build with --target x86_64-pc-windows-gnu");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(win::run())
}

#[cfg(windows)]
mod win {
    use std::io::Write;

    use anyhow::Result;
    use prost::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
    use ubi_ipc::LaunchContext;
    use ubi_proto::uplaydll;

    static LOG: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

    /// Log to stdout and to `UBI_BRIDGE_LOG` if set (stdout is unreliable under Proton).
    macro_rules! log {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            if let Some(f) = LOG.lock().unwrap().as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }};
    }

    static CONTEXT: std::sync::OnceLock<Option<LaunchContext>> = std::sync::OnceLock::new();

    fn context() -> Option<&'static LaunchContext> {
        CONTEXT.get().and_then(Option::as_ref)
    }

    fn load_context() -> Result<Option<LaunchContext>> {
        let Some(path) = std::env::args().nth(1).or_else(|| std::env::var("UBI_BRIDGE_CONTEXT").ok()) else {
            log!("no launch context: serving placeholder data");
            return Ok(None);
        };
        let ctx: LaunchContext = serde_json::from_slice(&std::fs::read(&path)?)?;
        log!("loaded launch context for uplay id {} ({}) from {path}", ctx.uplay_id, ctx.username);
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

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn new_server(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(first)
            .pipe_mode(PipeMode::Byte)
            .create(name)
    }

    static CONN_IDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
                    let n = CONN_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    log!("[conn {n}] client connected to {name}");
                    let client = std::mem::replace(&mut server, new_server(name, false)?);
                    match kind {
                        Kind::Api => tokio::spawn(dump(n, client)),
                        Kind::Aux => tokio::spawn(aux(n, client)),
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

    async fn dump(n: u32, mut pipe: NamedPipeServer) {
        let result: Result<()> = async {
            loop {
                let len = pipe.read_u32().await? as usize;
                let mut body = vec![0u8; len];
                pipe.read_exact(&mut body).await?;
                let req = match uplaydll::Req::decode(body.as_slice()) {
                    Ok(r) => r,
                    Err(e) => {
                        log!("[conn {n}] undecodable {len}-byte frame ({e}): {}", hex(&body));
                        continue;
                    }
                };
                log!("[conn {n}] <- {}", describe(&req));
                match respond(&req) {
                    Some(rsp) => {
                        log!("[conn {n}] -> {}", summarize(&rsp));
                        let out = rsp.encode_to_vec();
                        pipe.write_u32(out.len() as u32).await?;
                        pipe.write_all(&out).await?;
                        pipe.flush().await?;
                    }
                    None => log!("[conn {n}]    (unhandled, no reply)"),
                }
                let _ = std::io::stdout().flush();
            }
        }
        .await;
        log!("[conn {n}] closed: {result:?}");
    }

    async fn read_frame(pipe: &mut NamedPipeServer) -> std::io::Result<Vec<u8>> {
        let len = pipe.read_u32().await? as usize;
        let mut body = vec![0u8; len];
        pipe.read_exact(&mut body).await?;
        Ok(body)
    }

    async fn write_frame(pipe: &mut NamedPipeServer, msg: &impl Message) -> std::io::Result<()> {
        let out = msg.encode_to_vec();
        pipe.write_u32(out.len() as u32).await?;
        pipe.write_all(&out).await?;
        pipe.flush().await
    }

    /// One request/response round trip to the native broker named in the launch context.
    async fn broker(req: ubi_ipc::BrokerRequest) -> Result<ubi_ipc::BrokerResponse> {
        use tokio::io::AsyncBufReadExt;
        let addr = context()
            .and_then(|c| c.broker.clone())
            .ok_or_else(|| anyhow::anyhow!("no broker in launch context"))?;
        let stream = tokio::net::TcpStream::connect(&addr).await?;
        let (r, mut w) = stream.into_split();
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        w.write_all(&line).await?;
        let mut rsp = String::new();
        tokio::io::BufReader::new(r).read_line(&mut rsp).await?;
        Ok(serde_json::from_str(&rsp)?)
    }

    /// `uplay_aux_r164.dll`: init + Denuvo activation.
    async fn aux(n: u32, mut pipe: NamedPipeServer) {
        use ubi_proto::uplayauxdll as aux;
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
                if let Some(init) = &req.init_req {
                    log!("[aux {n}] <- InitReq uplay id {}", init.uplay_id);
                    let rsp = aux::Rsp {
                        request_id: req.request_id,
                        init_rsp: Some(aux::InitRsp {
                            result: aux::InitResult::Success as i32,
                            uplay_pid: Some(std::process::id()),
                        }),
                        activate_rsp: None,
                    };
                    write_frame(&mut pipe, &rsp).await?;
                    log!("[aux {n}] -> InitRsp Success");
                } else if let Some(act) = &req.activate_req {
                    if let Some(t) = &act.get_cached_or_fresh_token_req {
                        log!("[aux {n}] <- GetCachedOrFreshToken (request token {} bytes)", t.request_token.len());
                        let uplay_id = context().map(|c| c.uplay_id).unwrap_or_default();
                        let token = broker(ubi_ipc::BrokerRequest::DenuvoGameToken {
                            uplay_id,
                            request_token: t.request_token.clone(),
                        })
                        .await;
                        let token_rsp = match token {
                            Ok(ubi_ipc::BrokerResponse::GameToken { game_token }) => {
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
                        let rsp = aux::Rsp {
                            request_id: req.request_id,
                            init_rsp: None,
                            activate_rsp: Some(aux::ActivateRsp {
                                get_cached_or_fresh_token_rsp: Some(token_rsp),
                                get_burn_ticket_rsp: None,
                            }),
                        };
                        write_frame(&mut pipe, &rsp).await?;
                    } else if let Some(t) = &act.get_burn_ticket_req {
                        log!("[aux {n}] <- GetBurnTicket (request token {} bytes), not implemented", t.request_token.len());
                    } else if act.invalidate_cached_token_req.is_some() {
                        // The broker never caches tokens, so there is nothing to invalidate.
                        log!("[aux {n}] <- InvalidateCachedToken");
                        write_frame(&mut pipe, &aux::Rsp { request_id: req.request_id, ..Default::default() }).await?;
                    }
                } else {
                    log!("[aux {n}] <- unknown {req:?}");
                }
            }
        }
        .await;
        log!("[aux {n}] closed: {result:?}");
    }

    /// Name the populated response fields without printing tickets or keys.
    fn summarize(rsp: &uplaydll::Rsp) -> String {
        let full = format!("{rsp:?}");
        let fields: Vec<&str> = full
            .trim_start_matches("Rsp { ")
            .split(", ")
            .filter_map(|f| f.split_once(": Some(").map(|(k, _)| k))
            .filter(|k| k.ends_with("_rsp") || k.ends_with("_push"))
            .collect();
        format!("Rsp {{ {} }}", fields.join(", "))
    }

    /// Debug-print only the populated fields of a request.
    fn describe(req: &uplaydll::Req) -> String {
        let full = format!("{req:#?}");
        full.lines()
            .filter(|l| !l.trim_end().ends_with(": None,"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn respond(req: &uplaydll::Req) -> Option<uplaydll::Rsp> {
        use uplaydll::{InitResult, OverlayInjectionMethod};
        if let Some(r) = &req.init_process_req {
            return Some(uplaydll::Rsp {
                init_process_rsp: Some(uplaydll::InitProcessRsp {
                    result: InitResult::Success as i32,
                    uplay_pid: Some(std::process::id()),
                    overlay_enabled: Some(false),
                    overlay_injection_method: Some(OverlayInjectionMethod::None as i32),
                    sdk_monitoring_config: None,
                    devmode: Some(false),
                }),
                ..Default::default()
            })
            .inspect(|_| log!("    init_process for uplay id {:?}", r.uplay_id));
        }
        if let Some(r) = &req.init_req {
            let id = r.uplay_id.unwrap_or_default();
            let init = match context() {
                Some(ctx) => {
                    if ctx.uplay_id != id {
                        log!("    warning: game asked for uplay id {id}, context is for {}", ctx.uplay_id);
                    }
                    uplaydll::InitRsp {
                        result: InitResult::Success as i32,
                        account: Some(uplaydll::Account {
                            account_id: Some(ctx.account_id.clone()),
                            username: Some(ctx.username.clone()),
                            name_on_platform: Some(ctx.name_on_platform.clone()),
                            ..Default::default()
                        }),
                        connected: Some(true),
                        game_uplay_id: Some(id),
                        uplay_ids: ctx.owned_uplay_ids.clone(),
                        game_cd_keys: ctx
                            .cd_keys
                            .iter()
                            .map(|k| uplaydll::GameCdKey { uplay_id: k.uplay_id, cd_key: Some(k.key.clone()) })
                            .collect(),
                        is_in_offline_mode: Some(false),
                        upc_ticket: Some(ctx.upc_ticket.clone()),
                        uplay_pid: Some(std::process::id()),
                        storage: ctx
                            .savegame_path
                            .clone()
                            .map(|p| uplaydll::Storage { savegame_storage_path: Some(p) }),
                        ubi_services: ctx
                            .ubiservices_app_id
                            .clone()
                            .map(|a| uplaydll::UbiServices { app_id: Some(a) }),
                        ..Default::default()
                    }
                }
                None => uplaydll::InitRsp {
                    result: InitResult::Success as i32,
                    account: Some(uplaydll::Account {
                        account_id: Some("00000000-0000-0000-0000-000000000000".into()),
                        username: Some("recon".into()),
                        name_on_platform: Some("recon".into()),
                        ..Default::default()
                    }),
                    connected: Some(true),
                    game_uplay_id: Some(id),
                    uplay_ids: vec![id],
                    is_in_offline_mode: Some(false),
                    upc_ticket: Some("recon-placeholder-ticket".into()),
                    uplay_pid: Some(std::process::id()),
                    ..Default::default()
                },
            };
            return Some(uplaydll::Rsp { init_rsp: Some(init), ..Default::default() });
        }
        None
    }
}
