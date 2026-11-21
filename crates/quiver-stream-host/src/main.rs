//! `quiver-stream-host`: one stream's host side, standalone, for testing.
//!
//! ```text
//! quiver-stream-host --target test-pattern --size 2560x1440 --fps 120 --codec hevc --out f.h265
//! quiver-stream-host --target command --size 1920x1080 --fps 60 --out g.h265 --audio-out g.opus -- vkcube
//! quiver-stream-host serve [--socket-fd N | --listen ADDR] [--preset P] < spec.json
//! ```
//!
//! `serve` is how quiverd runs a stream (`quiver_stream_host::serve`): the spec, keys
//! included, is the first line on stdin.
//!
//! Stats go to stderr once a second, and a summary at the end. `--out` is a raw elementary
//! stream (Annex-B H.264/HEVC, or AV1 OBUs) that ffplay opens; `--audio-out` is Ogg Opus.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use quiver_stream_host::recovery::Recovery;
use quiver_stream_host::{Host, HostOptions, Target};
use quiver_stream_proto::{AudioCodec, Chroma, ColorInfo, Control, HdrMetadata, StreamConfig, VideoCodec};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TargetArg {
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
    target: TargetArg,
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
    /// HDR10 (BT.2020 PQ, 10-bit), with example mastering metadata (test pattern).
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
    /// 2, 6 (5.1) or 8 (7.1).
    #[arg(long, default_value_t = 2)]
    audio_channels: u8,
    /// This machine is a headless server: pads may be ordinary uinput devices when nobody is
    /// seated at it.
    #[arg(long)]
    headless_server: bool,
    /// Stop after this many frames.
    #[arg(long)]
    frames: Option<u64>,
    /// Stop after this many seconds.
    #[arg(long)]
    seconds: Option<f64>,
    /// Write the video elementary stream here.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Write the audio here, as Ogg Opus.
    #[arg(long)]
    audio_out: Option<PathBuf>,
    /// Run the test window (an X11 program for the command target) instead of a host.
    #[arg(long, hide = true)]
    test_window: bool,
    /// The program to run in the virtual display (--target command).
    #[arg(last = true)]
    command: Vec<String>,
}

/// `quiver-stream-host serve`: the spec (keys included) is the first line on stdin.
#[derive(Parser, Debug)]
#[command(name = "quiver-stream-host serve", about = "Serve one stream behind the transport, as quiverd runs it; the spec is the first line on stdin")]
struct ServeCli {
    /// The media socket, already bound, inherited on this fd.
    #[arg(long)]
    socket_fd: Option<i32>,
    /// Bind the media socket here instead (else the spec's `listen`, else 0.0.0.0:47998).
    #[arg(long)]
    listen: Option<std::net::SocketAddr>,
    /// NVENC preset, 1 (fastest) to 7.
    #[arg(long, default_value_t = 1)]
    preset: u8,
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
    // The pad namespace's inside part (quiver_stream_host::sandbox) runs before anything else:
    // `--sandbox-exec GID -- PROGRAM…`.
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("--sandbox-exec") {
        let gid: u32 = raw.get(2).and_then(|g| g.parse().ok()).ok_or_else(|| anyhow::anyhow!("--sandbox-exec GID -- PROGRAM"))?;
        let program: Vec<String> = raw.iter().skip(3).skip_while(|a| *a == "--").cloned().collect();
        return Err(quiver_stream_host::sandbox::sandbox_exec(gid, &program));
    }
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    if raw.get(1).map(String::as_str) == Some("serve") {
        let s = ServeCli::parse_from(raw.iter().skip(1));
        let args = quiver_stream_host::serve::ServeArgs { socket_fd: s.socket_fd, listen: s.listen, preset: s.preset };
        return tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(quiver_stream_host::serve::serve(args));
    }
    let a = Args::parse();
    if a.test_window {
        return quiver_stream_host::testwin::run(a.fps.round() as u32);
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(a))
}

async fn run(a: Args) -> Result<()> {
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
    let config = StreamConfig { codec, width, height, refresh_mhz, color, bitrate_kbps: a.bitrate, audio: AudioCodec::Opus, audio_channels: a.audio_channels, audio_frame_ms: 5 };
    let recovery = match a.recovery {
        RecoveryArg::Rfi => Recovery::RefInvalidation,
        RecoveryArg::IntraRefresh => Recovery::IntraRefresh,
        RecoveryArg::Keyframe => Recovery::Keyframe,
    };
    let target = match a.target {
        TargetArg::TestPattern => Target::TestPattern { motion: !a.still },
        TargetArg::Command if a.command.is_empty() => bail!("--target command needs the program after --"),
        TargetArg::Command => Target::Command { argv: a.command.clone(), env: vec![] },
    };
    let session = format!("cli-{}", std::process::id());
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("quiver-stream").join(&session);
    let max_frames = a.frames.or(a.seconds.map(|s| (s * a.fps).round() as u64));
    let opts = HostOptions { target, config: config.clone(), session, dir: dir.clone(), preset: a.preset, recovery, headless_server: a.headless_server, max_frames, force_sandbox_gid: None, exe: None };
    let (_bitrate_tx, bitrate_rx) = tokio::sync::watch::channel(a.bitrate);
    let mut host = Host::start(opts, bitrate_rx).await?;
    eprintln!("pads: {:?}{}", host.pad_mode, host.x_display.as_deref().map(|d| format!(", display {d}")).unwrap_or_default());
    let mut video = a.out.map(std::fs::File::create).transpose()?.map(std::io::BufWriter::new);
    let enc = quiver_stream_host::opus::Encoder::new(config.audio_channels, 50, 128_000)?;
    let mut audio = match a.audio_out {
        Some(p) => Some(quiver_stream_host::ogg::OggOpus::new(std::io::BufWriter::new(std::fs::File::create(p)?), enc.channels, enc.streams, enc.coupled, &enc.mapping, enc.frame as u64)?),
        None => None,
    };
    drop(enc);
    let mut audio_packets = 0u64;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            f = host.frames.recv() => match f {
                Some(f) => if let Some(w) = video.as_mut() { w.write_all(&f.data)?; },
                None => break,
            },
            p = host.audio.recv() => if let Some(p) = p {
                audio_packets += 1;
                if let Some(o) = audio.as_mut() { o.packet(&p.data)?; }
            },
            c = host.control_out.recv() => if let Some(c) = c { print_control(&c) },
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => if !host.alive() { eprintln!("the program exited"); break; },
        }
    }
    let report = host.stop().await?;
    if let Some(mut w) = video {
        w.flush()?;
    }
    if let Some(o) = audio {
        o.finish()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!(
        "done: {} frames ({} keyframes, {} dropped), {:.1} MB, {} audio packets\n  capture→encode start: {}\n  encode: {}",
        report.frames,
        report.keyframes,
        report.dropped,
        report.bytes as f64 / 1e6,
        audio_packets,
        report.capture.summary(),
        report.encode.summary()
    );
    Ok(())
}

fn print_control(c: &Control) {
    match c {
        Control::HostStats(s) => eprintln!("stats: {:.1} fps, capture {} µs, encode {} µs, {} kbit/s", s.fps, s.capture_us, s.encode_us, s.bitrate_kbps),
        Control::Configure(c) => eprintln!(
            "configure: {:?} {}x{} @ {:.3} Hz, {} kbit/s, {}-bit {:?}{}",
            c.codec,
            c.width,
            c.height,
            c.refresh_mhz as f64 / 1000.0,
            c.bitrate_kbps,
            c.color.bit_depth,
            c.color.chroma,
            if c.color.hdr.is_some() { " HDR10" } else { "" }
        ),
        Control::Cursor { shape } => eprintln!("cursor: {}", shape.as_ref().map(|s| format!("{}x{}", s.width, s.height)).unwrap_or("hidden".into())),
        other => eprintln!("control: {other:?}"),
    }
}
