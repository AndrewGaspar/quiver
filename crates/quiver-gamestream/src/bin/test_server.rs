//! A scratch GameStream server for testing with a real Moonlight client, before
//! `quiver-stream-host` exists: it replays an encoded file as the stream.
//!
//! ```text
//! ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=60 -t 10 -c:v hevc_nvenc -tune ull -bf 0 -g 60 -aud 1 test.hevc
//! ffmpeg -f lavfi -i sine=frequency=440 -t 10 -ac 2 -c:a libopus -b:a 128k -vbr off -frame_duration 5 test.opus
//! quiver-gamestream-test --state /tmp/x --bind 127.0.0.1 --video test.hevc --audio test.opus --pin 1234
//! ```
//!
//! The video file is Annex-B with access unit delimiters (`-aud 1`), so frames split on
//! them; it loops, and a keyframe request jumps to the next keyframe. Audio is an Ogg Opus
//! file, looped. Input events are counted and printed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use quiver_gamestream::{App, AppLibrary, Art, FileClientStore, GameStreamServer, HostEnds, Ports, ServerConfig, ServerEvent, StreamHost, StreamRequest};
use quiver_stream_proto::{AudioPacket, Control, EncodedFrame, InputEvent, VideoCodec, now_micros};

#[derive(Parser)]
struct Args {
    /// Where the server's identity and paired clients live.
    #[arg(long)]
    state: PathBuf,
    /// Addresses to listen on (repeatable).
    #[arg(long, default_value = "127.0.0.1")]
    bind: Vec<std::net::IpAddr>,
    /// Listen on every address of this interface instead (e.g. tailscale0).
    #[arg(long)]
    interface: Option<String>,
    #[arg(long, default_value_t = 47989)]
    http_port: u16,
    #[arg(long, default_value = "Quiver Test")]
    name: String,
    /// Annex-B H.264 or HEVC, or an AV1 OBU stream (low-overhead, with temporal delimiters).
    #[arg(long)]
    video: Option<PathBuf>,
    /// h264, hevc or av1: what --video holds, and the only codec offered.
    #[arg(long, default_value = "hevc")]
    codec: String,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// An Ogg Opus file.
    #[arg(long)]
    audio: Option<PathBuf>,
    /// Submit this PIN to the first pairing request.
    #[arg(long)]
    pin: Option<String>,
    /// Stream quiver-stream-host's test pattern (NVENC) instead of replaying a file. Needs
    /// the `stream-host` feature.
    #[arg(long)]
    host_pattern: bool,
    /// Drop this percentage of video packets, to test FEC.
    #[arg(long, default_value_t = 0)]
    loss: u8,
    /// Offer HEVC Main10 (HDR) too; the file should then be Main10.
    #[arg(long)]
    hdr: bool,
    /// Stop after this many seconds.
    #[arg(long)]
    exit_after: Option<u64>,
}

struct Library;

impl AppLibrary for Library {
    fn apps(&self) -> Vec<App> {
        vec![
            App { key: "test:pattern".into(), title: "Quiver Test Pattern".into(), hdr: false },
            App { key: "test:second".into(), title: "Second Title".into(), hdr: false },
        ]
    }

    fn art(&self, key: &str) -> Option<Art> {
        let hue = if key == "test:pattern" { 0u8 } else { 120 };
        let img = image::RgbImage::from_fn(628, 888, |x, y| image::Rgb([hue.wrapping_add((x / 4) as u8), (y / 4) as u8, 160]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
        Some(Art { content_type: "image/png".into(), bytes: png })
    }
}

/// Counts for the test to read back.
#[derive(Default)]
struct Stats {
    frames: AtomicUsize,
    audio: AtomicUsize,
    input: AtomicUsize,
    keyframe_requests: AtomicUsize,
}

struct Replay {
    frames: Arc<Vec<(bool, Vec<u8>)>>,
    audio: Arc<Vec<Vec<u8>>>,
    fps: u32,
    stats: Arc<Stats>,
    current: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StreamHost for Replay {
    fn start(&self, req: StreamRequest, ends: HostEnds) -> Result<()> {
        eprintln!(
            "STREAM {} {}x{}@{} {:?} {} kbps audio {}ch/{}ms hdr={}",
            req.app.title, req.video.width, req.video.height, req.video.fps, req.video.codec, req.video.bitrate_kbps, req.audio.opus.channels, req.audio.packet_ms, req.video.hdr
        );
        let frames = self.frames.clone();
        let audio = self.audio.clone();
        let fps = self.fps;
        let stats = self.stats.clone();
        let task = tokio::spawn(async move {
            let HostEnds { video, audio: audio_tx, mut input, mut control_in, .. } = ends;
            let mut tick = tokio::time::interval(Duration::from_micros(1_000_000 / u64::from(fps)));
            let mut audio_tick = tokio::time::interval(Duration::from_millis(5));
            let mut i = 0usize;
            let mut a = 0usize;
            let mut seq = 0u64;
            let mut last_key = 0u64;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if frames.is_empty() { continue; }
                        let (key, data) = &frames[i % frames.len()];
                        let now = now_micros();
                        if *key { last_key = seq; }
                        let f = EncodedFrame { seq, captured: now, encoded: now, keyframe: *key, refs_from_seq: last_key, width: 0, height: 0, data: data.clone() };
                        if video.send(f).await.is_err() { break; }
                        stats.frames.fetch_add(1, Ordering::Relaxed);
                        seq += 1;
                        i += 1;
                    }
                    _ = audio_tick.tick(), if !audio.is_empty() => {
                        let p = AudioPacket { seq: a as u64, captured: now_micros(), mic: false, data: audio[a % audio.len()].clone() };
                        let _ = audio_tx.try_send(p);
                        stats.audio.fetch_add(1, Ordering::Relaxed);
                        a += 1;
                    }
                    ev = input.recv() => match ev {
                        Some(e) => {
                            let n = stats.input.fetch_add(1, Ordering::Relaxed);
                            if n < 50 || matches!(e, InputEvent::Key { .. }) { eprintln!("INPUT {e:?}"); }
                        }
                        None => break,
                    },
                    c = control_in.recv() => match c {
                        Some(Control::Keyframe) | Some(Control::LostFrames { .. }) => {
                            stats.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                            // Jump to the next keyframe.
                            if let Some(off) = (0..frames.len()).find(|k| frames[(i + k) % frames.len()].0) { i += off; }
                        }
                        Some(Control::Bye { reason }) => { eprintln!("BYE {reason}"); break; }
                        Some(_) => {}
                        None => break,
                    },
                }
            }
        });
        if let Some(old) = self.current.lock().unwrap().replace(task) {
            old.abort();
        }
        Ok(())
    }

    fn quit(&self, app_key: &str) {
        eprintln!("QUIT {app_key}");
        if let Some(t) = self.current.lock().unwrap().take() {
            t.abort();
        }
    }
}

/// Splits an Annex-B stream into access units on AUD NAL units, marking keyframes.
fn split_annexb(data: &[u8], hevc: bool) -> Vec<(bool, Vec<u8>)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && (data[i + 2] == 1 || (data[i + 2] == 0 && data[i + 3] == 1)) {
            let sc = if data[i + 2] == 1 { 3 } else { 4 };
            starts.push((i, i + sc));
            i += sc;
        } else {
            i += 1;
        }
    }
    let nal_type = |at: usize| if hevc { (data[at] >> 1) & 0x3F } else { data[at] & 0x1F };
    let aud = if hevc { 35 } else { 9 };
    let mut aus: Vec<(bool, Vec<u8>)> = Vec::new();
    let mut cur_start = None;
    let mut key = false;
    for &(sc, nal) in &starts {
        if nal >= data.len() {
            continue;
        }
        let t = nal_type(nal);
        if t == aud {
            if let Some(s) = cur_start {
                aus.push((key, data[s..sc].to_vec()));
            }
            cur_start = Some(sc);
            key = false;
        }
        if (hevc && (16..=21).contains(&t)) || (!hevc && t == 5) {
            key = true;
        }
    }
    if let Some(s) = cur_start {
        aus.push((key, data[s..].to_vec()));
    }
    aus
}

/// Splits an AV1 low-overhead OBU stream into temporal units, marking keyframes (a
/// sequence header in the unit).
fn split_av1(data: &[u8]) -> Vec<(bool, Vec<u8>)> {
    let mut out: Vec<(bool, Vec<u8>)> = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let header = data[i];
        let obu_type = (header >> 3) & 0xF;
        let ext = header & 0x4 != 0;
        let mut j = i + 1 + usize::from(ext);
        let mut size = 0usize;
        let mut shift = 0;
        loop {
            let Some(&b) = data.get(j) else { return out };
            size |= usize::from(b & 0x7F) << shift;
            j += 1;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        let end = (j + size).min(data.len());
        if obu_type == 2 || out.is_empty() {
            out.push((false, Vec::new()));
        }
        let tu = out.last_mut().expect("pushed");
        if obu_type == 1 {
            tu.0 = true;
        }
        tu.1.extend_from_slice(&data[i..end]);
        i = end;
    }
    out
}

/// The Opus packets of an Ogg Opus file (skipping the two header packets).
fn read_ogg_opus(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    let mut partial = Vec::new();
    let mut i = 0;
    while i + 27 <= data.len() {
        if &data[i..i + 4] != b"OggS" {
            bail!("not an Ogg page at {i}");
        }
        let segs = usize::from(data[i + 26]);
        let table = &data[i + 27..i + 27 + segs];
        let mut at = i + 27 + segs;
        for &len in table {
            partial.extend_from_slice(&data[at..at + usize::from(len)]);
            at += usize::from(len);
            if len < 255 {
                packets.push(std::mem::take(&mut partial));
            }
        }
        i = at;
    }
    Ok(packets.into_iter().skip(2).collect())
}

#[cfg(feature = "stream-host")]
fn host_pattern(state: &std::path::Path, config: &mut ServerConfig) -> Result<Arc<dyn StreamHost>> {
    use quiver_stream_host::host::Target;
    // NVENC does all three, and reference invalidation.
    config.codecs = quiver_gamestream::Codecs::default();
    config.reference_frame_invalidation = true;
    let adapter = quiver_gamestream::stream_host::StreamHostAdapter::new(state.join("streams"), |_app: &App| Ok(Target::TestPattern { motion: true }));
    Ok(Arc::new(adapter))
}

#[cfg(not(feature = "stream-host"))]
fn host_pattern(_: &std::path::Path, _: &mut ServerConfig) -> Result<Arc<dyn StreamHost>> {
    bail!("--host-pattern needs the stream-host feature")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_ansi(false).with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).with_writer(std::io::stderr).init();
    let args = Args::parse();
    let codec = match args.codec.as_str() {
        "h264" => VideoCodec::H264,
        "hevc" => VideoCodec::Hevc,
        "av1" => VideoCodec::Av1,
        other => bail!("unknown codec {other}"),
    };
    let frames = match &args.video {
        Some(p) => {
            let data = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
            match codec {
                VideoCodec::Av1 => split_av1(&data),
                c => split_annexb(&data, c == VideoCodec::Hevc),
            }
        }
        None => Vec::new(),
    };
    if args.video.is_some() && !frames.iter().any(|f| f.0) {
        bail!("the video file has no keyframes (encode with -aud 1)");
    }
    let audio = match &args.audio {
        Some(p) => read_ogg_opus(&std::fs::read(p)?)?,
        None => Vec::new(),
    };
    eprintln!("loaded {} frames ({} keyframes), {} audio packets", frames.len(), frames.iter().filter(|f| f.0).count(), audio.len());

    let mut config = ServerConfig::new(&args.name, &args.state);
    config.bind = match &args.interface {
        Some(i) => quiver_gamestream::interface_addresses(i)?,
        None => args.bind.clone(),
    };
    let listen_on = format!("{:?}", config.bind);
    config.ports = Ports::from_http(args.http_port);
    config.codecs = quiver_gamestream::Codecs {
        h264: codec == VideoCodec::H264,
        hevc: codec == VideoCodec::Hevc,
        hevc_main10: codec == VideoCodec::Hevc && args.hdr,
        av1: codec == VideoCodec::Av1,
        av1_main10: false,
        yuv444: false,
    };
    // The replay can't invalidate references; it jumps to the next keyframe instead.
    config.reference_frame_invalidation = false;
    config.test_video_loss_percent = args.loss;
    let stats = Arc::new(Stats::default());
    let replay = Arc::new(Replay { frames: Arc::new(frames), audio: Arc::new(audio), fps: args.fps, stats: stats.clone(), current: Mutex::new(None) });
    let clients = Arc::new(FileClientStore::new(args.state.join("moonlight-clients.json")));
    let host: Arc<dyn StreamHost> = if args.host_pattern {
        host_pattern(&args.state, &mut config)?
    } else {
        replay
    };
    let server = Arc::new(GameStreamServer::start(config, Arc::new(Library), clients, host).await?);
    eprintln!("READY {} on {listen_on} port {} (fingerprint {})", args.name, args.http_port, &server.identity().fingerprint()[..16]);

    let mut events = server.events();
    let pin = args.pin.clone();
    let s2 = server.clone();
    tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            eprintln!("EVENT {e:?}");
            if let (ServerEvent::PairingRequested { .. }, Some(pin)) = (&e, &pin) {
                let s3 = s2.clone();
                let pin = pin.clone();
                tokio::spawn(async move {
                    match s3.submit_pin(&pin).await {
                        Ok(c) => eprintln!("PAIRED {}", c.fingerprint),
                        Err(e) => eprintln!("PAIR FAILED {e:#}"),
                    }
                });
            }
        }
    });
    let stats_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            eprintln!(
                "STATS frames={} audio={} input={} keyframe_requests={}",
                stats.frames.load(Ordering::Relaxed),
                stats.audio.load(Ordering::Relaxed),
                stats.input.load(Ordering::Relaxed),
                stats.keyframe_requests.load(Ordering::Relaxed)
            );
        }
    });
    match args.exit_after {
        Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
        None => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    stats_task.abort();
    eprintln!("EXIT");
    if let Ok(s) = Arc::try_unwrap(server) {
        s.shutdown().await;
    }
    Ok(())
}
