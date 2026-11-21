//! `quiver-stream-host`: run one stream's host side on its own, for testing.
//!
//! ```text
//! quiver-stream-host --target test-pattern --size 2560x1440 --fps 120 --codec hevc --out f.h265
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use quiver_stream_proto::{AudioCodec, Chroma, ColorInfo, Control, HdrMetadata, StreamConfig, VideoCodec};
use quiver_stream_host::cuda::Cuda;
use quiver_stream_host::recovery::Recovery;
use quiver_stream_host::video::{self, PatternSource, VideoIo};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Target {
    TestPattern,
    Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RecoveryArg {
    Rfi,
    IntraRefresh,
    Keyframe,
}

#[derive(Parser, Debug)]
#[command(name = "quiver-stream-host", about = "One Quiver stream's host side, standalone: virtual display, capture, NVENC, audio, input")]
struct Args {
    #[arg(long, value_enum, default_value = "test-pattern")]
    target: Target,
    /// WIDTHxHEIGHT.
    #[arg(long, default_value = "1920x1080")]
    size: String,
    #[arg(long, default_value_t = 60.0)]
    fps: f64,
    #[arg(long, value_enum, default_value = "hevc")]
    codec: Codec,
    /// kbit/s.
    #[arg(long, default_value_t = 30_000)]
    bitrate: u32,
    #[arg(long, default_value_t = 8)]
    bit_depth: u8,
    #[arg(long)]
    yuv444: bool,
    /// HDR10 (BT.2020 PQ, 10-bit), with example mastering metadata.
    #[arg(long)]
    hdr: bool,
    #[arg(long)]
    full_range: bool,
    /// NVENC preset, 1 (fastest) to 7.
    #[arg(long, default_value_t = 1)]
    preset: u8,
    #[arg(long, value_enum, default_value = "rfi")]
    recovery: RecoveryArg,
    /// Keep the pattern's background still (only the band changes).
    #[arg(long)]
    still: bool,
    /// Stop after this many frames.
    #[arg(long)]
    frames: Option<u64>,
    /// Stop after this many seconds.
    #[arg(long)]
    seconds: Option<f64>,
    /// Write the elementary stream here (Annex-B for H.264/HEVC, OBUs for AV1; ffplay opens
    /// either).
    #[arg(long)]
    out: Option<PathBuf>,
    /// The command to run in the virtual display (--target command).
    #[arg(last = true)]
    command: Vec<String>,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let Some((w, h)) = s.split_once('x') else { bail!("--size wants WIDTHxHEIGHT, not {s}") };
    Ok((w.parse()?, h.parse()?))
}

/// Example HDR10 metadata: BT.2020 primaries, D65, 1000 / 0.005 nits, MaxCLL 1000, MaxFALL 400.
fn example_hdr() -> HdrMetadata {
    HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 1000, max_fall: 400 }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let a = Args::parse();
    let (width, height) = parse_size(&a.size)?;
    let codec = match a.codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
        Codec::Av1 => VideoCodec::Av1,
    };
    let color = ColorInfo {
        bit_depth: if a.hdr { 10 } else { a.bit_depth },
        chroma: if a.yuv444 { Chroma::Yuv444 } else { Chroma::Yuv420 },
        full_range: a.full_range,
        hdr: a.hdr.then(example_hdr),
    };
    let refresh_mhz = (a.fps * 1000.0).round() as u32;
    let cfg = StreamConfig { codec, width, height, refresh_mhz, color: color.clone(), bitrate_kbps: a.bitrate, audio: AudioCodec::Opus, audio_channels: 2, audio_frame_ms: 5 };
    let recovery = match a.recovery {
        RecoveryArg::Rfi => Recovery::RefInvalidation,
        RecoveryArg::IntraRefresh => Recovery::IntraRefresh,
        RecoveryArg::Keyframe => Recovery::Keyframe,
    };
    let max_frames = a.frames.or(a.seconds.map(|s| (s * a.fps).round() as u64));
    match a.target {
        Target::TestPattern => run_pattern(cfg, a.preset, recovery, !a.still, max_frames, a.out),
        Target::Command => bail!("--target command is not wired into the CLI yet"),
    }
}

fn run_pattern(cfg: StreamConfig, preset: u8, recovery: Recovery, motion: bool, max_frames: Option<u64>, out: Option<PathBuf>) -> Result<()> {
    let cuda = Cuda::open()?;
    eprintln!("gpu: {}", cuda.name);
    let (ftx, mut frx) = tokio::sync::mpsc::channel(64);
    let (ctx, mut crx) = tokio::sync::mpsc::channel(64);
    let (_cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (_btx, brx) = tokio::sync::watch::channel(cfg.bitrate_kbps);
    let stop = Arc::new(AtomicBool::new(false));
    let source = Box::new(PatternSource::new(cfg.width, cfg.height, cfg.refresh_mhz, cfg.color.clone(), motion));
    let base = video::base_config(&cfg, preset, recovery);
    let io = VideoIo { frames: ftx, control: ctx, cmds: cmd_rx, bitrate: brx, stop: stop.clone(), max_frames };
    let announce = cfg.clone();
    let th = std::thread::Builder::new().name("video".into()).spawn(move || video::run(cuda, source, base, announce, io))?;
    let mut file = out.map(std::fs::File::create).transpose()?.map(std::io::BufWriter::new);
    loop {
        while let Ok(c) = crx.try_recv() {
            print_control(&c);
        }
        match frx.blocking_recv() {
            Some(f) => {
                if let Some(w) = file.as_mut() {
                    w.write_all(&f.data)?;
                }
            }
            None => break,
        }
    }
    while let Ok(c) = crx.try_recv() {
        print_control(&c);
    }
    let report = th.join().map_err(|_| anyhow::anyhow!("the video thread panicked"))??;
    if let Some(mut w) = file {
        w.flush()?;
    }
    eprintln!(
        "done: {} frames ({} keyframes, {} dropped), {:.1} MB\n  capture→encode start: {}\n  encode: {}",
        report.frames,
        report.keyframes,
        report.dropped,
        report.bytes as f64 / 1e6,
        report.capture.summary(),
        report.encode.summary()
    );
    Ok(())
}

fn print_control(c: &Control) {
    match c {
        Control::HostStats(s) => eprintln!("stats: {:.1} fps, capture {} µs, encode {} µs, {} kbit/s", s.fps, s.capture_us, s.encode_us, s.bitrate_kbps),
        Control::Configure(c) => eprintln!("configure: {:?} {}x{} @ {:.3} Hz, {} kbit/s, {}-bit {:?}{}", c.codec, c.width, c.height, c.refresh_mhz as f64 / 1000.0, c.bitrate_kbps, c.color.bit_depth, c.color.chroma, if c.color.hdr.is_some() { " HDR10" } else { "" }),
        other => eprintln!("control: {other:?}"),
    }
}
