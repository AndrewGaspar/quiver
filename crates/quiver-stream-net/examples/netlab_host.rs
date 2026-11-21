//! The host half of a synthetic stream, for quiver-netlab and by hand.
//!
//! It streams frames of realistic size (following the transport's bitrate target) at the
//! given resolution and rate to one client, and prints JSON lines on stdout: `listening`,
//! `link` changes, `host_stats` every `--stats-ms`, and a `host_summary` at the end.
//!
//! ```text
//! netlab_host --listen 10.0.0.1 --keys-file /tmp/keys --fps 120 --width 2560 --height 1440
//! netlab_client --server 10.0.0.1 --keys-file /tmp/keys
//! ```
//!
//! The listen address can also come from `QUIVER_NETLAB_SERVER_ADDR`, and the keys from
//! `QUIVER_STREAM_KEYS`.

#[path = "common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use quiver_stream_net::sim::SyntheticEncoder;
use quiver_stream_net::{HostOpts, HostSession, LinkState};
use quiver_stream_proto::{Control, now_micros};
use serde_json::json;

#[derive(Parser)]
struct Args {
    /// Where to listen (ip, or ip:port; default port 47998). Else QUIVER_NETLAB_SERVER_ADDR,
    /// else 0.0.0.0:47998.
    #[arg(long)]
    listen: Option<String>,
    /// The session keys as 80 hex digits (else QUIVER_STREAM_KEYS, else --keys-file).
    #[arg(long)]
    keys: Option<String>,
    /// A file to write fresh keys to (if it doesn't exist) for the client to read.
    #[arg(long)]
    keys_file: Option<PathBuf>,
    #[arg(long, default_value_t = 2560)]
    width: u32,
    #[arg(long, default_value_t = 1440)]
    height: u32,
    #[arg(long, default_value_t = 120)]
    fps: u32,
    /// How long to stream once the client is connected; 0 streams until it leaves.
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    /// The bitrate ceiling, kbit/s.
    #[arg(long, default_value_t = 150_000)]
    max_kbps: u32,
    #[arg(long, default_value_t = 20_000)]
    start_kbps: u32,
    /// The share of a frame interval each frame is paced over.
    #[arg(long, default_value_t = 0.25)]
    pacing: f32,
    /// A keyframe every this many frames (0: only when the client asks).
    #[arg(long, default_value_t = 0)]
    keyframe_every: u64,
    #[arg(long, default_value_t = 1000)]
    stats_ms: u64,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("netlab_host: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let listen = common::addr_from(args.listen.as_deref(), "QUIVER_NETLAB_SERVER_ADDR", common::DEFAULT_PORT)?
        .unwrap_or_else(|| ([0, 0, 0, 0], common::DEFAULT_PORT).into());
    let keys = common::keys(args.keys.as_deref(), args.keys_file.as_deref(), true)?;
    let opts = HostOpts {
        max_kbps: args.max_kbps,
        start_kbps: args.start_kbps.min(args.max_kbps),
        pacing_fraction: args.pacing,
        fps_hint: args.fps,
        ..HostOpts::default()
    };
    let host = HostSession::bind(listen, keys, opts).await.map_err(|e| format!("bind {listen}: {e}"))?;
    common::emit(&json!({"t": "listening", "addr": host.local_addr().to_string(), "at_us": now_micros()}));

    let mut link = host.link_state();
    tokio::spawn(async move {
        while link.changed().await.is_ok() {
            let state = link.borrow_and_update().clone();
            let (state, detail) = match state {
                LinkState::Waiting => ("waiting", String::new()),
                LinkState::Connected(a) => ("connected", a.to_string()),
                LinkState::Closed(r) => ("closed", r),
            };
            common::emit(&json!({"t": "link", "side": "host", "state": state, "detail": detail, "at_us": now_micros()}));
        }
    });

    host.wait_connected().await.map_err(|e| e.to_string())?;
    let stats = host.stats_json_lines(Duration::from_millis(args.stats_ms), std::io::stdout());

    let keyframe = Arc::new(AtomicBool::new(false));
    let keyframes_asked = Arc::new(AtomicU64::new(0));
    let leaving = Arc::new(AtomicBool::new(false));
    {
        let (host, keyframe, asked, leaving) = (host.clone(), keyframe.clone(), keyframes_asked.clone(), leaving.clone());
        tokio::spawn(async move {
            while let Some(c) = host.recv_control().await {
                match c {
                    // The synthetic encoder can't invalidate references, so a loss costs a
                    // keyframe, as with an encoder that lacks reference invalidation.
                    Control::Keyframe | Control::LostFrames { .. } => {
                        keyframe.store(true, Ordering::Relaxed);
                        asked.fetch_add(1, Ordering::Relaxed);
                    }
                    Control::Bye { .. } => leaving.store(true, Ordering::Relaxed),
                    _ => {}
                }
            }
        });
    }
    let inputs = Arc::new(AtomicU64::new(0));
    {
        let (host, inputs) = (host.clone(), inputs.clone());
        tokio::spawn(async move {
            while host.recv_input().await.is_some() {
                inputs.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    let mut enc = SyntheticEncoder::new(args.width, args.height, args.fps, now_micros());
    enc.keyframe_every = (args.keyframe_every > 0).then_some(args.keyframe_every);
    let target = host.target_bitrate();
    let link = host.link_state();
    let mut tick = tokio::time::interval(Duration::from_micros(enc.interval_us()));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = Instant::now();
    let mut frames = 0u64;
    loop {
        tick.tick().await;
        if matches!(*link.borrow(), LinkState::Closed(_)) || leaving.load(Ordering::Relaxed) {
            break;
        }
        if args.seconds > 0 && start.elapsed() >= Duration::from_secs(args.seconds) {
            break;
        }
        if keyframe.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
        }
        let kbps = *target.borrow();
        if host.send_video(enc.next(now_micros(), kbps)).is_err() {
            break;
        }
        frames += 1;
    }
    let net = host.stats().await.ok();
    host.close("done");
    common::emit(&json!({
        "t": "host_summary",
        "at_us": now_micros(),
        "frames_sent": frames,
        "seconds": start.elapsed().as_secs_f64(),
        "input_events": inputs.load(Ordering::Relaxed),
        "keyframes_asked": keyframes_asked.load(Ordering::Relaxed),
        "net": net,
    }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    stats.abort();
    Ok(())
}
