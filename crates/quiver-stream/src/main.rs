//! quiver-stream: the stream window quiver-app launches (design/REMOTE.md).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use quiver_stream::av::{self, Backend};
use quiver_stream::bench;
use quiver_stream::decode::VideoDecoder;
use quiver_stream::gpu::Gpu;

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

#[derive(Subcommand)]
enum Cmd {
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
