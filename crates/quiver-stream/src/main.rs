//! quiver-stream: the stream window quiver-app launches (design/REMOTE.md).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ash::vk;
use clap::{Parser, Subcommand};
use quiver_stream::app::{self, Options};
use quiver_stream::av::{self, Backend};
use quiver_stream::bench;
use quiver_stream::decode::VideoDecoder;
use quiver_stream::gpu::Gpu;
use quiver_stream::link;
use quiver_stream::{caps, net};
use quiver_stream::source::FileSource;
use quiver_stream_proto::{Pacing, StreamPrefs, StreamTarget};

#[derive(Parser)]
#[command(name = "quiver-stream", about = "Quiver's remote-play client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("WIDTHxHEIGHT")?;
    Ok((w.parse().map_err(|_| "width")?, h.parse().map_err(|_| "height")?))
}

fn parse_backend(s: &str) -> Result<Backend, String> {
    Backend::parse(s).ok_or_else(|| "vulkan, cuda, vaapi or software".into())
}

fn parse_pacing(s: &str) -> Result<Pacing, String> {
    match s {
        "lowest" | "lowest-latency" => Ok(Pacing::LowestLatency),
        "smooth" => Ok(Pacing::Smooth),
        _ => Err("lowest or smooth".into()),
    }
}

fn parse_mode(s: &str) -> Result<vk::PresentModeKHR, String> {
    Ok(match s {
        "immediate" => vk::PresentModeKHR::IMMEDIATE,
        "mailbox" => vk::PresentModeKHR::MAILBOX,
        "fifo" => vk::PresentModeKHR::FIFO,
        "fifo-relaxed" => vk::PresentModeKHR::FIFO_RELAXED,
        _ => return Err("immediate, mailbox, fifo or fifo-relaxed".into()),
    })
}

#[derive(Subcommand)]
enum Cmd {
    /// Play a video file in the stream window, as if a host were streaming it (the file
    /// source: frames stamped as captured when sent).
    Play {
        file: PathBuf,
        /// Frames per second (default: the file's, else 60).
        #[arg(long)]
        fps: Option<f64>,
        /// Start over at the end.
        #[arg(long = "loop")]
        looping: bool,
        /// A window rather than full screen.
        #[arg(long)]
        windowed: bool,
        /// The window's size.
        #[arg(long, value_parser = parse_size)]
        size: Option<(u32, u32)>,
        #[arg(long, value_parser = parse_pacing, default_value = "lowest")]
        pacing: Pacing,
        /// Force a present mode.
        #[arg(long, value_parser = parse_mode)]
        present_mode: Option<vk::PresentModeKHR>,
        /// One decoder backend, no fallback.
        #[arg(long, value_parser = parse_backend)]
        decoder: Option<Backend>,
        /// Show the stats chip from the start.
        #[arg(long)]
        stats: bool,
        /// Leave gamepads alone.
        #[arg(long)]
        no_pads: bool,
        /// No audio output.
        #[arg(long)]
        no_audio: bool,
        /// Send a quiet test tone as the stream's audio.
        #[arg(long)]
        tone: bool,
        /// Drop this fraction of frames (0..1), with Lost notices, to exercise recovery.
        #[arg(long, default_value_t = 0.0)]
        loss: f64,
        /// Treat the file as HDR10.
        #[arg(long)]
        hdr: bool,
        /// The display is side-by-side 3D.
        #[arg(long)]
        sbs: bool,
        /// Read the test pattern from each frame and report latencies after N frames (JSON).
        #[arg(long)]
        measure: Option<u64>,
        /// Quit after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
        /// Show the first-run hint.
        #[arg(long)]
        intro: bool,
    },
    /// Stream from a host: the ticket (`StartStream`'s reply, `{"ticket": {"addr",
    /// "session_id", "key"}, "host_name"}`) is the first line on stdin, never an argument.
    Connect {
        /// A window rather than full screen.
        #[arg(long)]
        windowed: bool,
        #[arg(long, value_parser = parse_size)]
        size: Option<(u32, u32)>,
        #[arg(long, value_parser = parse_pacing, default_value = "lowest")]
        pacing: Pacing,
        #[arg(long, value_parser = parse_mode)]
        present_mode: Option<vk::PresentModeKHR>,
        #[arg(long, value_parser = parse_backend)]
        decoder: Option<Backend>,
        /// Show the stats chip from the start.
        #[arg(long)]
        stats: bool,
        #[arg(long)]
        no_pads: bool,
        #[arg(long)]
        no_audio: bool,
        /// Read the stamp from each frame and report latencies after N frames (JSON).
        #[arg(long)]
        measure: Option<u64>,
        /// Quit after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
        #[arg(long)]
        intro: bool,
        /// Never NACK missing shards (FEC only).
        #[arg(long)]
        no_nack: bool,
        /// After a later frame starts arriving, how long to wait for an incomplete one (µs).
        #[arg(long)]
        reorder_slack_us: Option<u64>,
    },
    /// What this machine can decode and show: the `ClientCaps` a Hello or `StartStream`
    /// carries (decoders, the display, pads), as JSON.
    Caps {
        /// JSON (the only form; for symmetry with other tools).
        #[arg(long)]
        json: bool,
    },
    /// What this machine can decode and show (the Hello's caps), as JSON.
    Probe,
    /// Decode a file without a window, timing each frame.
    Bench {
        file: PathBuf,
        #[arg(long, default_value_t = 120.0)]
        fps: f64,
        /// One decoder backend, no fallback.
        #[arg(long, value_parser = parse_backend)]
        decoder: Option<Backend>,
        #[arg(long, default_value_t = 600)]
        frames: usize,
        /// Check each frame's test pattern (made with `quiver-stream pattern`).
        #[arg(long)]
        pattern: bool,
    },
    /// Raw NV12 frames carrying the test pattern, to stdout, for an encoder.
    Pattern {
        #[arg(value_parser = parse_size)]
        size: (u32, u32),
        frames: u32,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("QUIVER_STREAM_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    av::quiet(true);
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Play {
            file,
            fps,
            looping,
            windowed,
            size,
            pacing,
            present_mode,
            decoder,
            stats,
            no_pads,
            no_audio,
            tone,
            loss,
            hdr,
            sbs,
            measure,
            seconds,
            intro,
        } => {
            let (client, host) = link::loopback();
            let source = FileSource { path: file, fps, looping, loss, tone, hdr, frames: None };
            let _feeder = source.spawn(host)?;
            let opts = Options {
                windowed,
                size,
                prefs: StreamPrefs { pacing, ..Default::default() },
                present_mode,
                decoder,
                stats,
                pads: !no_pads,
                audio: !no_audio,
                sbs,
                target: StreamTarget::Desktop,
                intro,
                measure,
                duration: seconds.map(Duration::from_secs_f64),
                periods: None,
            };
            if let Some(report) = app::run(client, opts)? {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        }
        Cmd::Connect { windowed, size, pacing, present_mode, decoder, stats, no_pads, no_audio, measure, seconds, intro, no_nack, reorder_slack_us } => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let spec = net::ConnectSpec::parse(&line)?;
            let mut opts = quiver_stream_net::ClientOpts::default();
            if let Some(s) = reorder_slack_us {
                opts.video.reorder_slack_us = s;
            }
            if no_nack {
                opts.nack_max_wait_us = None;
            }
            let (link, net) = net::connect(&spec.ticket, opts)?;
            // Under the netlab: each second's numbers as its stats lines.
            let mut file = quiver_stream_net::netlab::StatsFile::from_env();
            let periods = file.enabled().then(|| {
                let (tx, rx) = crossbeam_channel::unbounded::<quiver_stream::stats::Period>();
                std::thread::spawn(move || {
                    while let Ok(p) = rx.recv() {
                        file.client(&p.client, &p.netlab_extra());
                    }
                });
                tx
            });
            let mut prefs = spec.prefs.clone().unwrap_or_default();
            prefs.pacing = pacing;
            let opts = Options {
                windowed,
                size,
                prefs,
                present_mode,
                decoder,
                stats,
                pads: !no_pads,
                audio: !no_audio,
                sbs: false,
                target: spec.target.clone().unwrap_or(StreamTarget::Desktop),
                intro,
                measure,
                duration: seconds.map(Duration::from_secs_f64),
                periods,
            };
            let result = app::run(link, opts);
            net.close();
            match result {
                Ok(Some(report)) => println!("{}", serde_json::to_string_pretty(&report)?),
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
        Cmd::Caps { json: _ } => println!("{}", serde_json::to_string_pretty(&caps::probe()?)?),
        Cmd::Probe => {
            let (entry, instance, exts) = Gpu::instance(&[])?;
            let gpu = Arc::new(Gpu::new(entry, instance, exts, None)?);
            let caps = VideoDecoder::probe(&gpu);
            let ext: Vec<String> = gpu.device_extensions.iter().map(|e| e.to_string_lossy().into_owned()).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "gpu": gpu.name,
                    "decoders": caps,
                    "device_extensions": ext,
                    "decode_queue_families": gpu.decode_families.iter().map(|(f, ops)| format!("{f}: {ops:?}")).collect::<Vec<_>>(),
                }))?
            );
        }
        Cmd::Bench { file, fps, decoder, frames, pattern } => {
            let r = bench::decode(&file, fps, decoder, frames, pattern)?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Pattern { size, frames } => bench::pattern_frames(size.0, size.1, frames)?,
    }
    Ok(())
}
