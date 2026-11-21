//! The video thread: a source writes each frame into the encoder's input, the encoder turns
//! it into an [`EncodedFrame`], and the frame goes out on a channel.
//!
//! It is a plain OS thread (NVENC and CUDA calls block), with the CUDA context pushed for its
//! whole life. Between frames it applies what the rest of the host asked for: loss reports,
//! keyframe requests, a new size or codec, and the bitrate from the transport's controller
//! (a `watch`, read without waiting). Every change of size or codec makes a new encode
//! session, starting on a keyframe, and is announced with a [`Control::Configure`] before
//! the first frame that uses it. `seq` keeps counting across sessions.
//!
//! If the frame channel is full (the transport has fallen behind), the frame is dropped
//! rather than queued, and the encoder is told it was lost, so the frames after it never
//! refer to it: the stream stays decodable and the client sees an ordinary gap.
//!
//! **A still picture.** A capture source sends nothing while the picture doesn't change
//! (gamescope paints only on damage; a paused game). It says so ([`Next::Idle`]) every
//! ~100 ms instead of waiting, and a keyframe asked for meanwhile (a client connecting or
//! resuming, a loss) is made from the last frame, encoded again: the picture comes at once.
//! A new encoder session (another codec or colour) takes the last frame along when its size
//! and layout match.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use quiver_stream_proto::{ColorInfo, Control, EncodedFrame, Micros, StreamConfig, VideoCodec, now_micros};
use tokio::sync::{mpsc, watch};

use crate::cuda::Cuda;
use crate::nvenc::{Encoder, EncoderConfig, InputFormat, Surface};
use crate::pattern::Pattern;
use crate::recovery::Recovery;
use crate::stats::{Samples, Window};

/// What the rest of the host asks of the video thread.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoCmd {
    Lost { since: u64, through: u64 },
    Keyframe,
    /// A new size and rate for the picture (the pattern redraws; a capture source follows
    /// what the display delivers).
    Resize { width: u32, height: u32, refresh_mhz: u32 },
    Codec(VideoCodec),
    Color(ColorInfo),
}

/// What a source produced.
pub enum Next {
    /// A frame is in the surface, captured at this time.
    Frame { captured: Micros },
    /// The source's picture changed size or format: make a new session, then ask again.
    Changed { width: u32, height: u32, format: InputFormat },
    /// No new frame for a while (the picture is still): nothing is in the surface.
    Idle,
    /// Nothing more will come.
    Stopped,
}

/// Something that fills the encoder's input.
pub trait Source: Send {
    /// The size and layout frames come in.
    fn format(&self) -> (u32, u32, InputFormat);
    /// Wait for the next frame and write it into `surface`. The CUDA context is current.
    fn next(&mut self, cuda: &Arc<Cuda>, surface: &mut Surface, seq: u64, stop: &AtomicBool) -> Result<Next>;
    /// A resize the host was asked for. Sources that can't resize themselves ignore it.
    fn resize(&mut self, _width: u32, _height: u32, _refresh_mhz: u32) {}
    /// A new colour setup (bit depth, chroma, HDR) for sources that draw their own frames.
    fn recolor(&mut self, _color: &ColorInfo) {}
    /// The colour the frames come in, when the source decides it (the layer's capture of an
    /// HDR game); `None` is the stream's own. A change comes with [`Next::Changed`].
    fn color(&self) -> Option<ColorInfo> {
        None
    }
}

/// The test pattern, paced at the display's rate.
pub struct PatternSource {
    pattern: Pattern,
    refresh_mhz: u32,
    next_at: Option<libc::timespec>,
}

impl PatternSource {
    pub fn new(width: u32, height: u32, refresh_mhz: u32, color: ColorInfo, motion: bool) -> Self {
        PatternSource { pattern: Pattern::new(width, height, color, motion), refresh_mhz, next_at: None }
    }

    fn period_ns(&self) -> u64 {
        1_000_000_000_000 / self.refresh_mhz.max(1) as u64
    }
}

fn now_ts() -> libc::timespec {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts
}

fn add_ns(t: libc::timespec, ns: u64) -> libc::timespec {
    let total = t.tv_nsec as u64 + ns;
    libc::timespec { tv_sec: t.tv_sec + (total / 1_000_000_000) as libc::time_t, tv_nsec: (total % 1_000_000_000) as _ }
}

fn ts_ns(t: &libc::timespec) -> i128 {
    t.tv_sec as i128 * 1_000_000_000 + t.tv_nsec as i128
}

impl Source for PatternSource {
    fn format(&self) -> (u32, u32, InputFormat) {
        (self.pattern.width, self.pattern.height, self.pattern.format)
    }

    fn next(&mut self, cuda: &Arc<Cuda>, surface: &mut Surface, seq: u64, stop: &AtomicBool) -> Result<Next> {
        if stop.load(Ordering::Relaxed) {
            return Ok(Next::Stopped);
        }
        let period = self.period_ns();
        let now = now_ts();
        let at = match self.next_at {
            // Behind by more than a frame (a stall, a new session): start again from now.
            Some(t) if ts_ns(&now) - ts_ns(&t) < period as i128 => t,
            _ => now,
        };
        unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &at, std::ptr::null_mut()) };
        self.next_at = Some(add_ns(at, period));
        let captured = now_micros();
        self.pattern.render(cuda, surface, seq, captured)?;
        Ok(Next::Frame { captured })
    }

    fn resize(&mut self, width: u32, height: u32, refresh_mhz: u32) {
        let color = self.pattern.color().clone();
        let motion = self.pattern.motion;
        self.pattern = Pattern::new(width & !1, height & !1, color, motion);
        self.refresh_mhz = refresh_mhz;
        self.next_at = None;
    }

    fn recolor(&mut self, color: &ColorInfo) {
        let (w, h, motion) = (self.pattern.width, self.pattern.height, self.pattern.motion);
        self.pattern = Pattern::new(w, h, color.clone(), motion);
    }
}

/// The channels the video thread talks on.
pub struct VideoIo {
    pub frames: mpsc::Sender<EncodedFrame>,
    pub control: mpsc::Sender<Control>,
    pub cmds: std_mpsc::Receiver<VideoCmd>,
    pub bitrate: watch::Receiver<u32>,
    pub stop: Arc<AtomicBool>,
    /// Stop after this many frames (benchmarks, `--frames`).
    pub max_frames: Option<u64>,
}

/// What a finished run measured.
#[derive(Debug, Default)]
pub struct RunReport {
    pub frames: u64,
    pub dropped: u64,
    pub keyframes: u64,
    pub capture: Samples,
    pub encode: Samples,
    pub bytes: u64,
}

/// The session settings for a source's picture (and its colour, when the source says).
fn session(base: &EncoderConfig, width: u32, height: u32, format: InputFormat, color: Option<ColorInfo>) -> EncoderConfig {
    EncoderConfig { width, height, input: format, color: color.unwrap_or_else(|| base.color.clone()), ..base.clone() }
}

/// Run until stopped, the source ends, or the frame channel closes.
pub fn run(cuda: Arc<Cuda>, mut source: Box<dyn Source>, mut base: EncoderConfig, mut announce: StreamConfig, io: VideoIo) -> Result<RunReport> {
    let _ctx = cuda.push()?;
    let (w, h, f) = source.format();
    let mut enc = Encoder::new(cuda.clone(), session(&base, w, h, f, source.color())).context("start the encoder")?;
    let configure = |enc: &Encoder, announce: &mut StreamConfig| {
        let c = enc.config();
        announce.codec = c.codec;
        announce.width = c.width;
        announce.height = c.height;
        announce.color = c.color.clone();
        announce.bitrate_kbps = c.bitrate_kbps;
        let _ = io.control.try_send(Control::Configure(announce.clone()));
    };
    configure(&enc, &mut announce);
    let mut report = RunReport::default();
    let mut window = Window::default();
    let mut seq = 0u64;
    let mut bitrate = io.bitrate.clone();
    // Changes of size, codec or colour wait for the next frame boundary; the rest apply to
    // the very next frame, including commands that arrive while the source waits for it.
    let mut pending: Vec<VideoCmd> = Vec::new();
    let apply = |enc: &mut Encoder, pending: &mut Vec<VideoCmd>| -> Result<()> {
        while let Ok(cmd) = io.cmds.try_recv() {
            match cmd {
                VideoCmd::Lost { since, through } => {
                    enc.lost(since, through)?;
                }
                VideoCmd::Keyframe => enc.request_keyframe(),
                other => pending.push(other),
            }
        }
        Ok(())
    };
    loop {
        if io.stop.load(Ordering::Relaxed) || io.max_frames.is_some_and(|m| report.frames >= m) {
            break;
        }
        apply(&mut enc, &mut pending)?;
        let mut rebuild = false;
        for cmd in pending.drain(..) {
            match cmd {
                VideoCmd::Resize { width, height, refresh_mhz } => {
                    source.resize(width, height, refresh_mhz);
                    base.fps_num = refresh_mhz;
                    base.fps_den = 1000;
                    announce.refresh_mhz = refresh_mhz;
                    rebuild = true;
                }
                VideoCmd::Codec(c) if c != base.codec => {
                    base.codec = c;
                    rebuild = true;
                }
                VideoCmd::Color(c) if c != base.color => {
                    source.recolor(&c);
                    base.color = c;
                    rebuild = true;
                }
                _ => {}
            }
        }
        if rebuild {
            let (w, h, f) = source.format();
            base.bitrate_kbps = enc.config().bitrate_kbps;
            let old = enc;
            enc = Encoder::new(cuda.clone(), session(&base, w, h, f, source.color())).context("restart the encoder")?;
            // The last picture comes along, for a still one's keyframe.
            if let Err(e) = enc.adopt_last(&old) {
                tracing::debug!("video: the last frame didn't carry over: {e:#}");
            }
            drop(old);
            configure(&enc, &mut announce);
        }
        match source.next(&cuda, enc.surface(), seq, &io.stop)? {
            Next::Stopped => break,
            Next::Idle => {
                apply(&mut enc, &mut pending)?;
                if !enc.keyframe_pending() || !pending.is_empty() {
                    continue;
                }
                // A keyframe of the still picture.
                let captured = now_micros();
                let Some((frame, t)) = enc.encode_last(seq, captured)? else { continue };
                report.frames += 1;
                report.keyframes += frame.keyframe as u64;
                report.bytes += frame.data.len() as u64;
                report.encode.push(t.encode_us);
                tracing::debug!(seq, keyframe = frame.keyframe, "video: the still picture again");
                match io.frames.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        report.dropped += 1;
                        enc.lost(seq, seq)?;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
                seq += 1;
            }
            Next::Changed { width, height, format } => {
                base.bitrate_kbps = enc.config().bitrate_kbps;
                drop(enc);
                enc = Encoder::new(cuda.clone(), session(&base, width, height, format, source.color())).context("restart the encoder for a new picture size")?;
                configure(&enc, &mut announce);
            }
            Next::Frame { captured } => {
                apply(&mut enc, &mut pending)?;
                if bitrate.has_changed().unwrap_or(false) {
                    let kbps = *bitrate.borrow_and_update();
                    base.bitrate_kbps = kbps;
                    enc.set_bitrate(kbps)?;
                }
                let capture_us = now_micros().saturating_sub(captured) as u32;
                let (frame, t) = enc.encode(seq, captured)?;
                report.frames += 1;
                report.keyframes += frame.keyframe as u64;
                report.bytes += frame.data.len() as u64;
                report.capture.push(capture_us);
                report.encode.push(t.encode_us);
                window.frame(frame.data.len(), capture_us, t.encode_us);
                match io.frames.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        report.dropped += 1;
                        enc.lost(seq, seq)?;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
                seq += 1;
                if let Some(s) = window.tick() {
                    let _ = io.control.try_send(Control::HostStats(s));
                }
            }
        }
    }
    Ok(report)
}

/// The encoder settings a stream starts with.
pub fn base_config(cfg: &StreamConfig, preset: u8, recovery: Recovery) -> EncoderConfig {
    EncoderConfig {
        codec: cfg.codec,
        width: cfg.width,
        height: cfg.height,
        fps_num: cfg.refresh_mhz,
        fps_den: 1000,
        bitrate_kbps: cfg.bitrate_kbps,
        color: cfg.color.clone(),
        input: InputFormat::yuv_for(&cfg.color),
        preset,
        recovery,
    }
}

/// Sleep that gives up early when `stop` is set.
pub fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let end = std::time::Instant::now() + d;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(20)));
    }
}
