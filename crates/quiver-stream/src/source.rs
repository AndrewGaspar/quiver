//! A stand-in host for development: plays a video file (Annex-B H.264/HEVC, AV1 IVF or OBU,
//! or anything libavformat reads) into a [`loopback`](crate::link::loopback) link at a given
//! rate, stamping each frame as captured when it is sent. It answers Ping, honours a
//! Keyframe request by skipping to the file's next keyframe, can drop frames at random to
//! exercise gap handling, and can send a test tone as Opus audio.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use quiver_stream_proto::{
    AudioCodec, AudioPacket, ColorInfo, Control, EncodedFrame, HdrMetadata, HostStats, PROTOCOL_VERSION, StreamConfig, now_micros,
};
use tracing::{debug, info, warn};

use crate::av::Demuxer;
use crate::link::{AudioIn, ControlIn, HostEnd, Outgoing, TransportStats, VideoIn};

#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    /// Frames per second (the file's own rate, else 60).
    pub fps: Option<f64>,
    /// Start over at the end.
    pub looping: bool,
    /// The fraction of frames to drop, with a Lost notice each.
    pub loss: f64,
    /// Send a quiet 440 Hz tone as audio.
    pub tone: bool,
    /// Say the stream is HDR10 (for a PQ file).
    pub hdr: bool,
    /// Stop after this many frames.
    pub frames: Option<u64>,
}

/// A small xorshift, so loss is reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn hdr10_default() -> HdrMetadata {
    // BT.2020 primaries, D65, a 1000-nit mastering display.
    HdrMetadata {
        primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
        white_point: [15635, 16450],
        max_luminance: 10_000_000,
        min_luminance: 50,
        max_cll: 1000,
        max_fall: 400,
    }
}

impl FileSource {
    /// Run on its own threads, driving `host`. Ends when the file does (unless looping) or
    /// the client goes away.
    pub fn spawn(self, host: HostEnd) -> Result<JoinHandle<()>> {
        let probe = Demuxer::open(&self.path)?;
        let fps = self.fps.or(probe.fps.filter(|f| *f > 1.0 && *f < 1000.0)).unwrap_or(60.0);
        let config = StreamConfig {
            codec: probe.codec,
            width: probe.width,
            height: probe.height,
            refresh_mhz: (fps * 1000.0).round() as u32,
            color: ColorInfo { bit_depth: if self.hdr { 10 } else { 8 }, hdr: self.hdr.then(hdr10_default), ..ColorInfo::sdr() },
            bitrate_kbps: 0,
            audio: AudioCodec::Opus,
            audio_channels: 2,
            audio_frame_ms: 5,
        };
        drop(probe);
        let config_size = (config.width, config.height);
        info!("file source: {} {:?} {}×{} at {fps} fps", self.path.display(), config.codec, config.width, config.height);
        let size = config_size;
        Ok(std::thread::Builder::new().name("source-video".into()).spawn(move || {
            // Like a host: nothing until the client's Hello.
            loop {
                match host.from_client.recv() {
                    Ok(Outgoing::Control(Control::Hello { .. })) => break,
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            let _ = host.control.send(ControlIn::Control(Control::Welcome {
                version: PROTOCOL_VERSION,
                host_name: "file".into(),
                session: format!("file-{}", std::process::id()),
            }));
            let _ = host.control.send(ControlIn::Control(Control::Configure(config)));
            if self.tone {
                let audio = host.audio.clone();
                let _ = std::thread::Builder::new().name("source-audio".into()).spawn(move || {
                    if let Err(e) = tone(audio) {
                        warn!("test tone: {e:#}");
                    }
                });
            }
            if let Err(e) = self.run(host, fps, size) {
                warn!("file source: {e:#}");
            }
        })?)
    }

    fn run(self, host: HostEnd, fps: f64, size: (u32, u32)) -> Result<()> {
        let mut dmx = Demuxer::open(&self.path)?;
        let period = Duration::from_secs_f64(1.0 / fps);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let start = Instant::now();
        let mut seq = 0u64;
        let mut last_key = 0u64;
        let mut want_key = false;
        let mut sent_bytes = 0u64;
        let mut last_stats = Instant::now();
        let mut ticks = 0u64;
        loop {
            // The client's messages, answered as they come until the next frame is due.
            let due = start + period * ticks as u32;
            loop {
                let m = match due.checked_duration_since(Instant::now()) {
                    Some(left) => match host.from_client.recv_timeout(left) {
                        Ok(m) => m,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                        Err(_) => return Ok(()),
                    },
                    None => match host.from_client.try_recv() {
                        Ok(m) => m,
                        Err(_) => break,
                    },
                };
                match m {
                    Outgoing::Control(Control::Ping { id, sent }) => {
                        let _ = host.control.send(ControlIn::Control(Control::Pong { id, sent, peer_now: now_micros() }));
                    }
                    Outgoing::Control(Control::Keyframe) => {
                        debug!("keyframe asked for: skipping to the next one");
                        want_key = true;
                    }
                    Outgoing::Control(Control::Bye { reason }) => {
                        info!("client said bye: {reason}");
                        return Ok(());
                    }
                    Outgoing::Control(c) => debug!("client: {c:?}"),
                    Outgoing::Input(i) => debug!("input: {:?}", i.event),
                }
            }
            if self.frames.is_some_and(|n| seq >= n) {
                let _ = host.control.send(ControlIn::Closed { reason: "the file source sent all its frames".into() });
                return Ok(());
            }
            let Some((data, key)) = dmx.read()? else {
                if !self.looping {
                    let _ = host.control.send(ControlIn::Closed { reason: "end of file".into() });
                    return Ok(());
                }
                dmx = Demuxer::open(&self.path).context("reopening the file")?;
                continue;
            };
            if want_key && !key {
                continue;
            }
            want_key = false;
            ticks += 1;
            if key {
                last_key = seq;
            }
            let now = now_micros();
            let frame = EncodedFrame {
                seq,
                captured: now,
                encoded: now,
                keyframe: key,
                refs_from_seq: if key { seq } else { last_key },
                width: size.0,
                height: size.1,
                data,
            };
            sent_bytes += frame.data.len() as u64;
            let msg = if !key && self.loss > 0.0 && rng.next_f64() < self.loss {
                VideoIn::Lost { first: seq, last: seq }
            } else {
                VideoIn::Frame(frame)
            };
            if host.video.send(msg).is_err() {
                return Ok(());
            }
            seq += 1;
            if last_stats.elapsed() >= Duration::from_secs(1) {
                let secs = last_stats.elapsed().as_secs_f32();
                last_stats = Instant::now();
                let kbps = (sent_bytes as f32 * 8.0 / 1000.0 / secs) as u32;
                let _ = host.control.send(ControlIn::Transport(TransportStats {
                    video_bytes: sent_bytes,
                    estimated_kbps: kbps,
                    ..Default::default()
                }));
                let _ = host.control.send(ControlIn::Control(Control::HostStats(HostStats {
                    fps: fps as f32,
                    bitrate_kbps: kbps,
                    ..Default::default()
                })));
                sent_bytes = 0;
            }
        }
    }
}

/// A quiet 440 Hz tone, 5 ms Opus frames, sent in real time.
fn tone(audio: crossbeam_channel::Sender<AudioIn>) -> Result<()> {
    let mut enc = crate::opus::Encoder::new(2, 96_000)?;
    let frame = 240usize;
    let start = Instant::now();
    let mut pcm = vec![0f32; frame * 2];
    for n in 0u64.. {
        for i in 0..frame {
            let t = (n as usize * frame + i) as f32 / 48_000.0;
            let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.05;
            pcm[i * 2] = v;
            pcm[i * 2 + 1] = v;
        }
        let data = enc.encode(&pcm)?;
        let due = start + Duration::from_micros(n * 5_000);
        if let Some(w) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(w);
        }
        if audio.send(AudioIn::Packet(AudioPacket { seq: n, captured: now_micros(), mic: false, data })).is_err() {
            return Ok(());
        }
    }
    Ok(())
}
