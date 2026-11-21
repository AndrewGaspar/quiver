//! One stream's host side, put together: the async API the transport and quiverd use.
//!
//! [`Host::start`] brings up, in order: the audio sink (so the program finds it), the pad
//! policy (and the user namespace the program runs in when pads are hidden), the virtual
//! display running the program, capture, the video thread, and the input thread. What comes
//! out is on [`Host`]'s receivers; what goes in is on its senders:
//!
//! | | |
//! |---|---|
//! | `frames` | [`EncodedFrame`]s, in order, `seq` counting up |
//! | `audio` | [`AudioPacket`]s, Opus, 5 ms each |
//! | `control_out` | [`Control::Configure`] (before the first frame and after every change), `HostStats` (each second), `Rumble`, `Cursor`, `Notice` |
//! | `input` | the client's [`Stamped`] input events (duplicates are dropped here) |
//! | `control_in` | `Resize`, `Prefs`, `LostFrames`, `Keyframe` from the client |
//! | `bitrate` (given to `start`) | the transport's target bitrate in kbit/s, applied without a keyframe |
//!
//! [`Host::stop`] (or dropping the `Host`) ends the program, removes the sink and the pads, and
//! returns what the video thread measured.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioPacket, Chroma, ColorInfo, Control, EncodedFrame, InputEvent, StreamConfig, StreamPrefs};
use tokio::sync::{mpsc, watch};

use crate::audio::{Audio, AudioSpec};
use crate::capture::{PwCapture, Want};
use crate::cuda::Cuda;
use crate::display::{Display, DisplaySpec};
use crate::input::Dedup;
use crate::input::pads::PadHost;
use crate::input::x11::XInjector;
use crate::recovery::Recovery;
use crate::seat::{self, PadMode};
use crate::video::{self, PatternSource, RunReport, Source, VideoCmd, VideoIo};

/// What the stream shows.
#[derive(Debug, Clone)]
pub enum Target {
    /// The synthetic pattern ([`crate::pattern`]), with a tone burst each second.
    TestPattern { motion: bool },
    /// A program, run in a headless gamescope.
    Command { argv: Vec<String>, env: Vec<(String, String)> },
}

#[derive(Debug, Clone)]
pub struct HostOptions {
    pub target: Target,
    /// The first settings: codec, size, rate, colour, bitrate, audio channels.
    pub config: StreamConfig,
    /// A name for this stream's sink, pads and folder.
    pub session: String,
    /// The stream's folder (gamescope's log, its display file).
    pub dir: PathBuf,
    /// NVENC preset, 1–7.
    pub preset: u8,
    pub recovery: Recovery,
    /// The machine is a headless server: pads may be ordinary devices if nobody is seated
    /// ([`crate::seat`]).
    pub headless_server: bool,
    /// Stop after this many frames (tests and benchmarks).
    pub max_frames: Option<u64>,
    /// Run the program in the pad namespace with this group even when no hidden pads are made
    /// (tests of the namespace itself).
    pub force_sandbox_gid: Option<u32>,
    /// The `quiver-stream-host` binary, which runs the inside of the pad namespace
    /// ([`crate::sandbox`]). None is this process's own executable.
    pub exe: Option<PathBuf>,
}

pub struct Host {
    pub frames: mpsc::Receiver<EncodedFrame>,
    pub audio: mpsc::Receiver<AudioPacket>,
    pub control_out: mpsc::Receiver<Control>,
    pub input: mpsc::Sender<Stamped>,
    pub control_in: mpsc::Sender<Control>,
    /// The pads' mode for this stream.
    pub pad_mode: PadMode,
    /// The program's X display, for a command target.
    pub x_display: Option<String>,
    stop: Arc<AtomicBool>,
    video: Option<std::thread::JoinHandle<Result<RunReport>>>,
    input_thread: Option<std::thread::JoinHandle<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    parts: Option<Parts>,
}

/// What lives as long as the stream.
struct Parts {
    display: Option<Display>,
    _audio: Option<Audio>,
    _tone: Option<std::thread::JoinHandle<()>>,
}

impl Host {
    pub async fn start(opts: HostOptions, bitrate: watch::Receiver<u32>) -> Result<Host> {
        tokio::task::spawn_blocking(move || Host::start_blocking(opts, bitrate)).await.context("the host's start task")?
    }

    fn start_blocking(opts: HostOptions, bitrate: watch::Receiver<u32>) -> Result<Host> {
        let cfg = opts.config.clone();
        let cuda = Cuda::open()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (ftx, frx) = mpsc::channel(32);
        let (atx, arx) = mpsc::channel(256);
        let (ctx, crx) = mpsc::channel(256);
        let (in_tx, mut in_rx) = mpsc::channel::<Stamped>(1024);
        let (cin_tx, mut cin_rx) = mpsc::channel::<Control>(256);
        let (vcmd_tx, vcmd_rx) = std_mpsc::channel::<VideoCmd>();

        let mut parts = Parts { display: None, _audio: None, _tone: None };
        let mut pad_mode = PadMode::Off("the test pattern has no program to give pads to".into());
        let mut x = None;
        let source: Box<dyn Source> = match &opts.target {
            Target::TestPattern { motion } => {
                parts._tone = Some(crate::tone::start(cfg.audio_channels, atx.clone(), stop.clone(), cfg.refresh_mhz)?);
                Box::new(PatternSource::new(cfg.width, cfg.height, cfg.refresh_mhz, cfg.color.clone(), *motion))
            }
            Target::Command { argv, env } => {
                let audio = Audio::start(AudioSpec { id: opts.session.clone(), channels: cfg.audio_channels, frame_ms_x10: cfg.audio_frame_ms as u32 * 10, bitrate: if cfg.audio_channels > 2 { 256_000 } else { 128_000 } }, atx.clone())?;
                let mut env = env.clone();
                env.extend(audio.child_env());
                if cfg.color.hdr.is_some() {
                    env.push(("ENABLE_GAMESCOPE_WSI".into(), "1".into()));
                    env.push(("PROTON_ENABLE_HDR".into(), "1".into()));
                }
                let seated = seat::seated_sessions(std::path::Path::new("/run/systemd/sessions"));
                pad_mode = seat::decide(opts.headless_server, &seated, crate::sandbox::check_rule().map_err(|e| format!("{e:#}")));
                let wrapper = match (pad_mode.clone(), opts.force_sandbox_gid) {
                    (PadMode::Hidden { gid }, _) | (_, Some(gid)) => {
                        let exe = match &opts.exe {
                            Some(e) => e.clone(),
                            None => std::env::current_exe().context("find quiver-stream-host for the pad namespace")?,
                        };
                        crate::sandbox::wrap(&exe, gid, &[])
                    }
                    _ => Vec::new(),
                };
                let display = Display::start(
                    DisplaySpec { width: cfg.width, height: cfg.height, refresh_mhz: cfg.refresh_mhz, hdr: cfg.color.hdr.is_some(), command: argv.clone(), env, dir: opts.dir.clone(), wrapper },
                    Duration::from_secs(30),
                )?;
                x = Some((display.x_display.clone(), display.pid()));
                let want = Want::Bgrx;
                let capture = PwCapture::start(cuda.clone(), display.node_id, want, Duration::from_secs(15))?;
                parts.display = Some(display);
                parts._audio = Some(audio);
                Box::new(capture)
            }
        };
        let mut cfg_v = cfg.clone();
        if matches!(opts.target, Target::Command { .. }) {
            // gamescope's screencast is 8-bit SDR.
            cfg_v.color.bit_depth = 8;
            cfg_v.color.hdr = None;
        }
        let base = video::base_config(&cfg_v, opts.preset, opts.recovery);
        let io = VideoIo { frames: ftx, control: ctx.clone(), cmds: vcmd_rx, bitrate, stop: stop.clone(), max_frames: opts.max_frames };
        let cu = cuda.clone();
        let video = std::thread::Builder::new().name("video".into()).spawn(move || {
            let r = video::run(cu, source, base, cfg_v, io);
            if let Err(e) = &r {
                tracing::error!("video: {e:#}");
            }
            r
        })?;

        // Input: a thread owns the X connection and the pads; a task forwards to it.
        let (itx, irx) = std_mpsc::channel::<InputEvent>();
        let pads = match &pad_mode {
            PadMode::Off(_) => None,
            m => match PadHost::start(m.clone(), &opts.session, ctx.clone()) {
                Ok(p) => Some(p),
                Err(e) => {
                    pad_mode = PadMode::Off(format!("{e:#}"));
                    None
                }
            },
        };
        let injector = match &x {
            Some((d, pid)) => Some(XInjector::connect(d, *pid)?),
            None => None,
        };
        let (pm, ctx_i, stop_i) = (pad_mode.clone(), ctx.clone(), stop.clone());
        let input_thread = std::thread::Builder::new().name("input".into()).spawn(move || input_thread(irx, injector, pads, pm, ctx_i, stop_i))?;
        let rt = tokio::runtime::Handle::current();
        let mut tasks = Vec::new();
        tasks.push(rt.spawn(async move {
            let mut dedup = Dedup::default();
            while let Some(s) = in_rx.recv().await {
                if dedup.filter(&s) && itx.send(s.event).is_err() {
                    break;
                }
            }
        }));
        let is_pattern = matches!(opts.target, Target::TestPattern { .. });
        let ctx_c = ctx.clone();
        let mut color = cfg.color.clone();
        let size = (cfg.width, cfg.height);
        tasks.push(rt.spawn(async move {
            while let Some(c) = cin_rx.recv().await {
                for cmd in control_to_video(&c, is_pattern, &mut color) {
                    if vcmd_tx.send(cmd).is_err() {
                        return;
                    }
                }
                if let Control::Resize { display } = &c
                    && !is_pattern
                {
                    let _ = ctx_c.try_send(Control::Notice { text: format!("the stream stays {}x{}: resizing a running program's display isn't supported yet (asked for {}x{})", size.0, size.1, display.width, display.height) });
                }
            }
        }));
        Ok(Host {
            frames: frx,
            audio: arx,
            control_out: crx,
            input: in_tx,
            control_in: cin_tx,
            pad_mode,
            x_display: x.map(|(d, _)| d),
            stop,
            video: Some(video),
            input_thread: Some(input_thread),
            tasks,
            parts: Some(parts),
        })
    }

    /// Stop everything and return what the video thread measured.
    pub async fn stop(mut self) -> Result<RunReport> {
        self.stop.store(true, Ordering::Relaxed);
        let video = self.video.take();
        let input = self.input_thread.take();
        let parts = self.parts.take();
        for t in self.tasks.drain(..) {
            t.abort();
        }
        tokio::task::spawn_blocking(move || {
            let r = match video {
                Some(v) => v.join().map_err(|_| anyhow::anyhow!("the video thread panicked")).and_then(|r| r),
                None => Ok(RunReport::default()),
            };
            if let Some(i) = input {
                let _ = i.join();
            }
            drop(parts);
            r
        })
        .await?
    }

    /// The program's display is still up (always true for the test pattern).
    pub fn alive(&mut self) -> bool {
        self.parts.as_mut().and_then(|p| p.display.as_mut()).is_none_or(|d| d.alive())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

/// What a control message from the client means for the video thread.
pub fn control_to_video(c: &Control, is_pattern: bool, color: &mut ColorInfo) -> Vec<VideoCmd> {
    match c {
        Control::LostFrames { since_seq, through_seq } => vec![VideoCmd::Lost { since: *since_seq, through: *through_seq }],
        Control::Keyframe => vec![VideoCmd::Keyframe],
        Control::Resize { display } if is_pattern => vec![VideoCmd::Resize { width: display.width & !1, height: display.height & !1, refresh_mhz: display.refresh_mhz }],
        Control::Prefs(p) => prefs_to_video(p, is_pattern, color),
        _ => Vec::new(),
    }
}

fn prefs_to_video(p: &StreamPrefs, is_pattern: bool, color: &mut ColorInfo) -> Vec<VideoCmd> {
    let mut out = Vec::new();
    if let Some(c) = p.codec {
        out.push(VideoCmd::Codec(c));
    }
    let mut next = color.clone();
    next.chroma = if p.chroma444 { Chroma::Yuv444 } else { Chroma::Yuv420 };
    if p.hdr == Some(false) {
        next.hdr = None;
    }
    if next != *color {
        *color = next.clone();
        out.push(VideoCmd::Color(next));
    }
    if is_pattern && let (Some((w, h)), Some(fps)) = (p.resolution, p.fps) {
        out.push(VideoCmd::Resize { width: w & !1, height: h & !1, refresh_mhz: fps * 1000 });
    }
    out
}

fn input_thread(rx: std_mpsc::Receiver<InputEvent>, mut x: Option<XInjector>, pads: Option<PadHost>, pad_mode: PadMode, control: mpsc::Sender<Control>, stop: Arc<AtomicBool>) {
    let mut told_no_pads = false;
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(ev) => match &ev {
                InputEvent::PadArrive { .. } | InputEvent::PadLeave { .. } | InputEvent::Pad { .. } | InputEvent::PadMotion { .. } => match &pads {
                    Some(p) => p.send(ev),
                    None => {
                        if !told_no_pads && let (InputEvent::PadArrive { .. }, PadMode::Off(why)) = (&ev, &pad_mode) {
                            told_no_pads = true;
                            let _ = control.try_send(Control::Notice { text: format!("pads aren't available on this stream: {why}") });
                        }
                    }
                },
                InputEvent::ReleaseAll => {
                    if let Some(p) = &pads {
                        p.send(ev.clone());
                    }
                    if let Some(x) = x.as_mut() {
                        let _ = x.send(&ev);
                    }
                }
                _ => {
                    if let Some(x) = x.as_mut()
                        && let Err(e) = x.send(&ev)
                    {
                        tracing::warn!("input: {e:#}");
                    }
                }
            },
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(x) = x.as_mut() {
            match x.poll_cursor() {
                Ok(Some(c)) => {
                    let _ = control.try_send(c);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!("cursor: {e:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_stream_proto::{DisplayCaps, VideoCodec};

    #[test]
    fn control_messages_become_video_commands() {
        let mut color = ColorInfo::sdr();
        assert_eq!(control_to_video(&Control::LostFrames { since_seq: 4, through_seq: 6 }, false, &mut color), vec![VideoCmd::Lost { since: 4, through: 6 }]);
        assert_eq!(control_to_video(&Control::Keyframe, false, &mut color), vec![VideoCmd::Keyframe]);
        let d = DisplayCaps { width: 1281, height: 721, refresh_mhz: 59_940, hdr: false, vrr: false, stereo_sbs: false, scale: 1.0 };
        assert_eq!(control_to_video(&Control::Resize { display: d.clone() }, true, &mut color), vec![VideoCmd::Resize { width: 1280, height: 720, refresh_mhz: 59_940 }]);
        assert!(control_to_video(&Control::Resize { display: d }, false, &mut color).is_empty());
        let p = StreamPrefs { codec: Some(VideoCodec::Av1), chroma444: true, ..Default::default() };
        let cmds = control_to_video(&Control::Prefs(p.clone()), false, &mut color);
        assert_eq!(cmds[0], VideoCmd::Codec(VideoCodec::Av1));
        assert!(matches!(&cmds[1], VideoCmd::Color(c) if c.chroma == Chroma::Yuv444));
        // The same prefs again change nothing about colour.
        assert_eq!(control_to_video(&Control::Prefs(p), false, &mut color).len(), 1);
    }
}
