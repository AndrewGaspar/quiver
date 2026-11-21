//! The client half of a synthetic stream (see `netlab_host`).
//!
//! It receives frames, "decodes" them as a real client would (after a gap it waits for a
//! frame that doesn't depend on the lost ones, and asks for one with `LostFrames`), sends
//! mouse motion at `--input-hz`, and prints JSON lines on stdout: `link` changes,
//! `client_stats` every `--stats-ms` (the proto's `ClientStats` with `capture_to_present_us`
//! and `frames_dropped` filled in, plus the transport's counters), and a `client_summary`.
//!
//! The server address comes from `--server` or `QUIVER_NETLAB_SERVER_ADDR`; the local one
//! from `--bind` or `QUIVER_NETLAB_CLIENT_ADDR` (any port). Leave the local address
//! unbound for roaming tests, so the kernel can pick the new source address.

#[path = "common/mod.rs"]
mod common;

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use quiver_stream_net::{ClientOpts, ClientSession, LinkState, StatsReport, VideoEvent};
use quiver_stream_proto::{Control, InputEvent, now_micros};
use serde_json::json;

#[derive(Parser)]
struct Args {
    /// The host (ip, or ip:port; default port 47998). Else QUIVER_NETLAB_SERVER_ADDR.
    #[arg(long)]
    server: Option<String>,
    /// The local address to send from. Else QUIVER_NETLAB_CLIENT_ADDR, else any.
    #[arg(long)]
    bind: Option<String>,
    #[arg(long)]
    keys: Option<String>,
    /// Read the keys the host wrote here (waits up to 10 s for it).
    #[arg(long)]
    keys_file: Option<PathBuf>,
    /// Stop after this long; 0 runs until the host ends the stream.
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    #[arg(long, default_value_t = 1000)]
    stats_ms: u64,
    /// Mouse motion events a second (0: none).
    #[arg(long, default_value_t = 500)]
    input_hz: u32,
    /// Never NACK (FEC only).
    #[arg(long)]
    no_nack: bool,
}

#[derive(Default)]
struct Decode {
    /// The last frame lost; frames that reference anything at or before it can't decode.
    lost_through: Option<u64>,
    decoded: u64,
    dropped: u32,
    lost: u64,
    capture_to_present: Vec<u32>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("netlab_client: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let server = common::addr_from(args.server.as_deref(), "QUIVER_NETLAB_SERVER_ADDR", common::DEFAULT_PORT)?
        .ok_or("give --server or QUIVER_NETLAB_SERVER_ADDR")?;
    let bind = common::addr_from(args.bind.as_deref(), "QUIVER_NETLAB_CLIENT_ADDR", 0)?.unwrap_or_else(|| {
        if server.is_ipv6() { "[::]:0".parse().expect("address") } else { SocketAddr::from(([0, 0, 0, 0], 0)) }
    });
    let keys = common::keys(args.keys.as_deref(), args.keys_file.as_deref(), false)?;
    let sock = UdpSocket::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    let opts = ClientOpts { nack_max_wait_us: if args.no_nack { None } else { ClientOpts::default().nack_max_wait_us }, ..ClientOpts::default() };
    let started = Instant::now();
    let client = ClientSession::connect_socket(sock, server, keys, opts).await.map_err(|e| format!("connect {server}: {e}"))?;
    common::emit(&json!({"t": "link", "side": "client", "state": "connected", "detail": server.to_string(),
        "connect_ms": started.elapsed().as_secs_f64() * 1e3, "at_us": now_micros()}));
    client.send_control(Control::Keyframe).map_err(|e| e.to_string())?;

    let decode = Arc::new(Mutex::new(Decode::default()));
    let video = {
        let (client, decode) = (client.clone(), decode.clone());
        tokio::spawn(async move {
            while let Some(v) = client.recv_video().await {
                let now = now_micros();
                let mut d = decode.lock().expect("decode lock");
                match v {
                    VideoEvent::Frame(f) => {
                        let decodable = f.keyframe || d.lost_through.is_none_or(|l| f.refs_from_seq > l);
                        if !decodable {
                            d.dropped += 1;
                            continue;
                        }
                        d.lost_through = None;
                        d.decoded += 1;
                        if let Some(captured) = client.host_to_local(f.captured) {
                            d.capture_to_present.push(now.saturating_sub(captured) as u32);
                        }
                    }
                    VideoEvent::Gap { since_seq, through_seq } => {
                        d.lost += through_seq - since_seq + 1;
                        d.lost_through = Some(through_seq);
                        let _ = client.send_control(Control::LostFrames { since_seq, through_seq });
                    }
                }
            }
        })
    };

    if args.input_hz > 0 {
        let client = client.clone();
        let period = Duration::from_secs_f64(1.0 / f64::from(args.input_hz));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            let mut n = 0i32;
            loop {
                tick.tick().await;
                n += 1;
                if client.send_input(InputEvent::MouseMotion { dx: n % 7 - 3, dy: 1 }).is_err() {
                    return;
                }
            }
        });
    }

    let mut link = client.link_state();
    let mut tick = tokio::time::interval(Duration::from_millis(args.stats_ms));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let Ok(mut stats) = client.take_stats().await else { break };
                let Ok(net) = client.net_stats().await else { break };
                {
                    let mut d = decode.lock().expect("decode lock");
                    stats.frames_dropped = std::mem::take(&mut d.dropped);
                    stats.capture_to_present_us = common::median(&mut std::mem::take(&mut d.capture_to_present));
                }
                let _ = client.send_control(Control::Stats(stats.clone()));
                let line = serde_json::to_value(StatsReport::Client { at_us: now_micros(), stats, net }).expect("stats serialise");
                common::emit(&line);
            }
            changed = link.changed() => {
                if changed.is_err() || matches!(*link.borrow(), LinkState::Closed(_)) {
                    break;
                }
            }
        }
        if args.seconds > 0 && started.elapsed() >= Duration::from_secs(args.seconds) {
            let _ = client.send_control(Control::Bye { reason: "done".into() });
            tokio::time::sleep(Duration::from_millis(100)).await;
            break;
        }
    }
    let reason = match &*client.link_state().borrow() {
        LinkState::Closed(r) => r.clone(),
        _ => "client stopped".into(),
    };
    let net = client.net_stats().await.ok();
    client.close("done");
    video.abort();
    let d = decode.lock().expect("decode lock");
    common::emit(&json!({
        "t": "client_summary",
        "at_us": now_micros(),
        "reason": reason,
        "seconds": started.elapsed().as_secs_f64(),
        "frames_decoded": d.decoded,
        "frames_lost": d.lost,
        "net": net,
    }));
    Ok(())
}
