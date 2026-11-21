//! A running host that outlives its Moonlight sessions ([`Kept`]): what a disconnect leaves
//! running, and what `/resume` (or `/launch` of the same app) attaches the next session to.
//!
//! GameStream's own hosts keep the game when the client goes (the app backgrounded, the
//! network dropped, the control stream gone): only Quit app (`/cancel`) or the game exiting
//! ends it. Quiver's native streams keep a game for 10 minutes for a resume
//! (design/REMOTE.md §10); this is the same policy for Moonlight.
//!
//! [`Kept`] owns the host's channels ([`HostChannels`]) in one task:
//! - **attached**, it pumps them to the session's [`HostEnds`] (frames, audio, rumble out;
//!   input, stamped here with one count across sessions, and keyframe and loss requests in;
//!   the session's bitrate into the host's);
//! - **detached** (the session's input or control closed, or its `Bye`: a disconnect, never
//!   a quit), it drops what the host makes, until a new session attaches, `keep` passes, the
//!   host ends, or [`Kept::quit`];
//! - **on attach** it asks the host for a keyframe at once (the host repeats its last frame
//!   when the picture is still, so it comes even then), and passes on what the new session
//!   wants differently: another codec or colour as [`Control::Prefs`], another size or rate
//!   as [`Control::Resize`] (a game's display can't change size, so the stream keeps its size
//!   and Moonlight scales it; that is logged).
//!
//! It knows nothing of what the host is: the embedder hands it the channels and says how to
//! stop the host and what to check each second ([`Hooks`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioPacket, Chroma, Control, DisplayCaps, EncodedFrame, InputEvent, StreamPrefs, now_micros};
use tokio::sync::{mpsc, watch};

use crate::host::{HostEnds, StreamRequest, VideoParams};

/// How long a game is kept after its client goes, unless the embedder says otherwise: the
/// native streams' 10 minutes (`quiver_stream_proto::serve::KEEP_SECS`).
pub const KEEP: Duration = Duration::from_secs(quiver_stream_proto::serve::KEEP_SECS);

/// A running host's channels, as [`Kept`] drives them.
pub struct HostChannels {
    pub frames: mpsc::Receiver<EncodedFrame>,
    pub audio: mpsc::Receiver<AudioPacket>,
    pub control_out: mpsc::Receiver<Control>,
    pub input: mpsc::Sender<Stamped>,
    pub control_in: mpsc::Sender<Control>,
    /// The encoder's target bitrate (the host was started with its receiver).
    pub bitrate: watch::Sender<u32>,
}

/// Where a kept host stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptState {
    /// A session is attached.
    Streaming,
    /// No session: kept until then for a resume.
    Kept { until: Instant },
    Ended { reason: String },
}

/// What happened, for the embedder's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptEvent {
    /// A session attached (`resumed`: not the first).
    Attached { client: String, resumed: bool },
    /// The session went; the host is kept this long.
    Detached { reason: String, keep: Duration },
    /// The host ended. `quit`: the client quit the app (or the embedder ended it), rather
    /// than the host or the keep window ending it.
    Ended { reason: String, quit: bool },
}

type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What the embedder adds: how to stop the host, and what to check every second.
pub struct Hooks {
    /// Every second while the host runs: `Some(reason)` ends it (the game exited).
    pub tick: Box<dyn FnMut() -> Option<String> + Send>,
    /// Stops the host, once, when it ends for any reason (given the reason and whether it
    /// was a quit).
    pub stop: Box<dyn FnOnce(String, bool) -> StopFuture + Send>,
    pub events: Option<mpsc::UnboundedSender<KeptEvent>>,
}

enum Cmd {
    Attach(Box<StreamRequest>, HostEnds),
    Quit(String),
}

/// A host kept across sessions. Cloning gives another handle to the same host.
#[derive(Clone)]
pub struct Kept {
    /// The app it runs (an [`crate::App`]'s key).
    pub key: String,
    pub title: String,
    /// Counts up per host started (tests: a resume is the same host).
    pub serial: u64,
    tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<KeptState>>,
    client: Arc<Mutex<String>>,
}

static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Kept {
    /// Start keeping `host`, attached to the first session (`req`, `ends`).
    pub fn spawn(host: HostChannels, keep: Duration, req: StreamRequest, ends: HostEnds, hooks: Hooks) -> Kept {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(KeptState::Streaming));
        let client = Arc::new(Mutex::new(req.client_name.clone()));
        let k = Kept { key: req.app.key.clone(), title: req.app.title.clone(), serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed), tx, state: state.clone(), client: client.clone() };
        let first_video = req.video.clone();
        let first_audio = req.audio.clone();
        let _ = tx_first(&k.tx, req, ends);
        tokio::spawn(run(host, keep, rx, state, client, hooks, first_video, first_audio));
        k
    }

    /// Attach a new session (a resume, with the new launch's keys). Fails once the host has
    /// ended.
    pub fn attach(&self, req: StreamRequest, ends: HostEnds) -> Result<()> {
        if let KeptState::Ended { reason } = self.state() {
            bail!("{} has ended ({reason})", self.title);
        }
        if self.tx.send(Cmd::Attach(Box::new(req), ends)).is_err() {
            bail!("{} has ended", self.title);
        }
        Ok(())
    }

    /// End it: the client quit the app.
    pub fn quit(&self, reason: &str) {
        let _ = self.tx.send(Cmd::Quit(reason.to_owned()));
    }

    pub fn state(&self) -> KeptState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn alive(&self) -> bool {
        !matches!(self.state(), KeptState::Ended { .. })
    }

    /// The client it streams (or last streamed) to.
    pub fn client(&self) -> String {
        self.client.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Wait (up to `within`) for it to end.
    pub async fn ended(&self, within: Duration) -> bool {
        let end = Instant::now() + within;
        while self.alive() {
            if Instant::now() > end {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }
}

fn tx_first(tx: &mpsc::UnboundedSender<Cmd>, req: StreamRequest, ends: HostEnds) -> Result<(), ()> {
    tx.send(Cmd::Attach(Box::new(req), ends)).map_err(|_| ())
}

/// What changed between the video the host makes and what a new session asked for, as
/// messages for the host, and a line for the log when the size can't follow.
pub fn changes(now: &VideoParams, want: &VideoParams) -> (Vec<Control>, Option<String>) {
    let mut out = Vec::new();
    if (want.codec, want.chroma, want.hdr, want.bit_depth) != (now.codec, now.chroma, now.hdr, now.bit_depth) {
        out.push(Control::Prefs(StreamPrefs {
            codec: Some(want.codec),
            chroma444: want.chroma == Chroma::Yuv444,
            hdr: Some(want.hdr),
            resolution: Some((want.width, want.height)),
            fps: Some(want.fps),
            ..Default::default()
        }));
    }
    let mut note = None;
    if (want.width, want.height, want.fps) != (now.width, now.height, now.fps) {
        out.push(Control::Resize {
            display: DisplayCaps { width: want.width, height: want.height, refresh_mhz: want.fps * 1000, hdr: want.hdr, vrr: false, stereo_sbs: false, scale: 1.0 },
        });
        note = Some(format!(
            "the resumed session asked for {}x{}@{}; a running game's display keeps {}x{}@{} (the test pattern follows), and Moonlight scales the picture",
            want.width, want.height, want.fps, now.width, now.height, now.fps
        ));
    }
    (out, note)
}

/// One session's ends, while attached.
struct Attached {
    ends: HostEnds,
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut host: HostChannels,
    keep: Duration,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    state: Arc<Mutex<KeptState>>,
    client: Arc<Mutex<String>>,
    hooks: Hooks,
    mut video: VideoParams,
    audio: crate::host::AudioParams,
) {
    let Hooks { tick: mut check, stop, events } = hooks;
    let emit = |e: KeptEvent| {
        tracing::info!(event = ?e, "kept host");
        if let Some(tx) = &events {
            let _ = tx.send(e);
        }
    };
    let set = |s: KeptState| *state.lock().unwrap_or_else(|e| e.into_inner()) = s;
    let mut cur: Option<Attached> = None;
    let mut sessions = 0u32;
    let mut seq = 0u64;
    let mut until = Instant::now() + keep;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    enum Got {
        Frame(Option<EncodedFrame>),
        Audio(Option<AudioPacket>),
        HostControl(Option<Control>),
        Input(Option<InputEvent>),
        ClientControl(Option<Control>),
        Bitrate,
        Cmd(Option<Cmd>),
        Tick,
    }
    let (reason, quit) = loop {
        let got = match cur.as_mut() {
            Some(a) => tokio::select! {
                f = host.frames.recv() => Got::Frame(f),
                p = host.audio.recv() => Got::Audio(p),
                c = host.control_out.recv() => Got::HostControl(c),
                e = a.ends.input.recv() => Got::Input(e),
                c = a.ends.control_in.recv() => Got::ClientControl(c),
                r = a.ends.bitrate_kbps.changed() => if r.is_ok() { Got::Bitrate } else { Got::Input(None) },
                c = cmds.recv() => Got::Cmd(c),
                _ = tick.tick() => Got::Tick,
            },
            None => tokio::select! {
                f = host.frames.recv() => Got::Frame(f),
                p = host.audio.recv() => Got::Audio(p),
                c = host.control_out.recv() => Got::HostControl(c),
                c = cmds.recv() => Got::Cmd(c),
                _ = tick.tick() => Got::Tick,
            },
        };
        let mut detach: Option<String> = None;
        match got {
            Got::Frame(None) => break ("the host ended the stream".to_owned(), false),
            Got::Frame(Some(f)) => {
                if let Some(a) = &cur
                    && a.ends.video.send(f).await.is_err()
                {
                    detach = Some("the session stopped taking video".into());
                }
            }
            Got::Audio(Some(p)) => {
                if let Some(a) = &cur {
                    let _ = a.ends.audio.try_send(p);
                }
            }
            Got::Audio(None) | Got::HostControl(None) => break ("the host ended the stream".to_owned(), false),
            Got::HostControl(Some(c)) => {
                if let Control::Notice { text } = &c {
                    tracing::info!("kept host: {text}");
                }
                match (&cur, c) {
                    (_, Control::Bye { reason }) => break (format!("the host ended the stream: {reason}"), false),
                    (Some(a), c) => {
                        let _ = a.ends.control_out.try_send(c);
                    }
                    (None, _) => {}
                }
            }
            Got::Input(None) => detach = Some("the client disconnected".into()),
            Got::Input(Some(event)) => {
                seq += 1;
                let _ = host.input.send(Stamped { seq, at: now_micros(), event }).await;
            }
            Got::ClientControl(None) => detach = Some("the client disconnected".into()),
            Got::ClientControl(Some(Control::Bye { reason })) => detach = Some(reason),
            Got::ClientControl(Some(c)) => {
                let _ = host.control_in.send(c).await;
            }
            Got::Bitrate => {
                if let Some(a) = cur.as_mut() {
                    let kbps = *a.ends.bitrate_kbps.borrow_and_update();
                    host.bitrate.send_replace(kbps);
                }
            }
            Got::Cmd(None) => {}
            Got::Cmd(Some(Cmd::Quit(reason))) => break (reason, true),
            Got::Cmd(Some(Cmd::Attach(req, ends))) => {
                let resumed = sessions > 0;
                sessions += 1;
                if cur.take().is_some() {
                    tracing::info!("kept host: a new session replaces the attached one");
                }
                if resumed {
                    let (msgs, note) = changes(&video, &req.video);
                    for m in msgs {
                        let _ = host.control_in.send(m).await;
                    }
                    if let Some(n) = note {
                        tracing::info!("kept host: {n}");
                    }
                    if req.audio != audio {
                        tracing::warn!(was = ?audio, now = ?req.audio, "kept host: the resumed session asked for other audio; the host keeps its own layout");
                    }
                    video = VideoParams { codec: req.video.codec, chroma: req.video.chroma, hdr: req.video.hdr, bit_depth: req.video.bit_depth, ..video };
                }
                host.bitrate.send_replace(*ends.bitrate_kbps.borrow());
                // What the host made meanwhile is stale: the new session starts from now.
                while host.frames.try_recv().is_ok() {}
                while host.audio.try_recv().is_ok() {}
                // The picture at once: a keyframe, even of a still image.
                let _ = host.control_in.send(Control::Keyframe).await;
                *client.lock().unwrap_or_else(|e| e.into_inner()) = req.client_name.clone();
                set(KeptState::Streaming);
                emit(KeptEvent::Attached { client: req.client_name.clone(), resumed });
                cur = Some(Attached { ends });
            }
            Got::Tick => {
                if let Some(r) = check() {
                    break (r, false);
                }
                if cur.is_none() && Instant::now() >= until {
                    break (format!("nobody resumed it within {} min", keep.as_secs().div_ceil(60)), false);
                }
            }
        }
        if let Some(reason) = detach
            && cur.take().is_some()
        {
            let _ = host.input.send(Stamped { seq: { seq += 1; seq }, at: now_micros(), event: InputEvent::ReleaseAll }).await;
            until = Instant::now() + keep;
            set(KeptState::Kept { until });
            emit(KeptEvent::Detached { reason, keep });
        }
    };
    if let Some(a) = cur.take() {
        let _ = a.ends.control_out.try_send(Control::Bye { reason: reason.clone() });
    }
    stop(reason.clone(), quit).await;
    set(KeptState::Ended { reason: reason.clone() });
    emit(KeptEvent::Ended { reason, quit });
}

/// The one app a [`crate::StreamHost`] runs at a time, kept across sessions: what
/// `start`, `quit` and `running` need. A start for the running app attaches to it (and one
/// that comes while it is still starting replaces the session it will start with); a start
/// for another app is refused (the server asks the client to quit the running one first).
#[derive(Default)]
pub struct Slot {
    inner: Arc<Mutex<SlotState>>,
}

#[derive(Default)]
enum SlotState {
    #[default]
    Empty,
    Starting { key: String, newer: Option<(Box<StreamRequest>, HostEnds)>, quit: bool },
    Running(Kept),
}

/// What starting an app makes: its channels and hooks.
pub type Started = (HostChannels, Hooks);

impl Slot {
    /// `StreamHost::start`: attach to the running app, or start it with `launch` (the host,
    /// and whatever the app needs, up), kept for `keep` after a disconnect.
    pub fn start<F, Fut>(&self, req: StreamRequest, ends: HostEnds, keep: Duration, launch: F) -> Result<()>
    where
        F: FnOnce(StreamRequest) -> Fut,
        Fut: Future<Output = Result<Started>> + Send + 'static,
    {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *st {
            SlotState::Running(k) if k.alive() => {
                if k.key != req.app.key {
                    bail!("{} is running: quit it first", k.title);
                }
                return k.attach(req, ends);
            }
            SlotState::Starting { key, newer, .. } => {
                if *key != req.app.key {
                    bail!("{key} is starting: quit it first");
                }
                *newer = Some((Box::new(req), ends));
                return Ok(());
            }
            _ => {}
        }
        *st = SlotState::Starting { key: req.app.key.clone(), newer: None, quit: false };
        drop(st);
        let fut = launch(req.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let started = fut.await;
            let (newer, quit) = match std::mem::take(&mut *inner.lock().unwrap_or_else(|e| e.into_inner())) {
                SlotState::Starting { newer, quit, .. } => (newer, quit),
                _ => (None, false),
            };
            let (req, ends) = match newer {
                Some((r, e)) => (*r, e),
                None => (req, ends),
            };
            match started {
                Err(e) => {
                    tracing::warn!("{} didn't start: {e:#}", req.app.title);
                    let _ = ends.control_out.send(Control::Bye { reason: format!("{} didn't start: {e:#}", req.app.title) }).await;
                }
                Ok((_, hooks)) if quit => (hooks.stop)("the client quit the app".into(), true).await,
                Ok((chans, hooks)) => {
                    let k = Kept::spawn(chans, keep, req, ends, hooks);
                    *inner.lock().unwrap_or_else(|e| e.into_inner()) = SlotState::Running(k);
                }
            }
        });
        Ok(())
    }

    /// `StreamHost::quit`.
    pub fn quit(&self, key: &str, reason: &str) {
        match &mut *self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            SlotState::Running(k) if k.key == key => k.quit(reason),
            SlotState::Starting { key: k, quit, .. } if k == key => *quit = true,
            _ => {}
        }
    }

    /// `StreamHost::running`.
    pub fn running(&self) -> Option<String> {
        match &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            SlotState::Running(k) if k.alive() => Some(k.key.clone()),
            SlotState::Starting { key, quit: false, .. } => Some(key.clone()),
            _ => None,
        }
    }

    /// The running app's keeper, once it is up.
    pub fn kept(&self) -> Option<Kept> {
        match &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            SlotState::Running(k) if k.alive() => Some(k.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::audio::OpusLayout;
    use crate::host::{App, AudioParams, ColorSpace, TriggerRumble};
    use quiver_stream_proto::{ClientCaps, DisplayCaps, VideoCodec};

    pub fn request(key: &str, w: u32) -> StreamRequest {
        StreamRequest {
            app: App { key: key.into(), title: key.into(), hdr: false },
            client_name: "roth".into(),
            client_fingerprint: "f".into(),
            caps: ClientCaps { decoders: vec![], display: DisplayCaps { width: w, height: 720, refresh_mhz: 60_000, hdr: false, vrr: false, stereo_sbs: false, scale: 1.0 }, audio_channels: 2, pads: vec![], microphone: false },
            prefs: StreamPrefs::default(),
            video: VideoParams {
                codec: VideoCodec::Hevc,
                width: w,
                height: 720,
                fps: 60,
                bitrate_kbps: 10_000,
                bit_depth: 8,
                chroma: Chroma::Yuv420,
                hdr: false,
                color_space: ColorSpace::Rec709,
                full_range: false,
                slices_per_frame: 1,
                reference_frame_invalidation: true,
                intra_refresh: false,
            },
            audio: AudioParams { opus: OpusLayout::for_channels(2, false), packet_ms: 5, constant_bitrate: true },
        }
    }

    /// The client's side of one session's ends.
    pub struct Client {
        pub video: mpsc::Receiver<EncodedFrame>,
        pub input: mpsc::Sender<InputEvent>,
        pub control_in: mpsc::Sender<Control>,
        pub control_out: mpsc::Receiver<Control>,
        _audio: mpsc::Receiver<AudioPacket>,
        _trigger: mpsc::Receiver<TriggerRumble>,
        _bitrate: watch::Sender<u32>,
    }

    pub fn ends() -> (HostEnds, Client) {
        let (video_tx, video) = mpsc::channel(8);
        let (audio_tx, _audio) = mpsc::channel(8);
        let (input, input_rx) = mpsc::channel(8);
        let (control_in, control_in_rx) = mpsc::channel(8);
        let (control_out_tx, control_out) = mpsc::channel(8);
        let (trigger_tx, _trigger) = mpsc::channel(8);
        let (_bitrate, bitrate_rx) = watch::channel(10_000);
        (
            HostEnds { video: video_tx, audio: audio_tx, input: input_rx, control_in: control_in_rx, control_out: control_out_tx, trigger_rumble: trigger_tx, bitrate_kbps: bitrate_rx },
            Client { video, input, control_in, control_out, _audio, _trigger, _bitrate },
        )
    }

    /// The host's side: frames in, what reached it out.
    pub struct FakeHost {
        pub frames: mpsc::Sender<EncodedFrame>,
        pub input: mpsc::Receiver<Stamped>,
        pub control_in: mpsc::Receiver<Control>,
        _audio: mpsc::Sender<AudioPacket>,
        _control_out: mpsc::Sender<Control>,
    }

    pub fn host() -> (HostChannels, FakeHost, Arc<Mutex<Option<(String, bool)>>>) {
        let (frames, frames_rx) = mpsc::channel(8);
        let (audio, audio_rx) = mpsc::channel(8);
        let (control_out, control_out_rx) = mpsc::channel(8);
        let (input_tx, input) = mpsc::channel(64);
        let (control_in_tx, control_in) = mpsc::channel(64);
        let (bitrate, _) = watch::channel(10_000);
        let stopped = Arc::new(Mutex::new(None));
        (
            HostChannels { frames: frames_rx, audio: audio_rx, control_out: control_out_rx, input: input_tx, control_in: control_in_tx, bitrate },
            FakeHost { frames, input, control_in, _audio: audio, _control_out: control_out },
            stopped,
        )
    }

    pub fn hooks(stopped: Arc<Mutex<Option<(String, bool)>>>) -> Hooks {
        Hooks {
            tick: Box::new(|| None),
            stop: Box::new(move |reason, quit| {
                Box::pin(async move {
                    *stopped.lock().unwrap() = Some((reason, quit));
                })
            }),
            events: None,
        }
    }

    pub fn frame(seq: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame { seq, captured: now_micros(), encoded: now_micros(), keyframe, refs_from_seq: 0, width: 1280, height: 720, data: vec![0, 0, 1, seq as u8] }
    }

    async fn controls(h: &mut FakeHost) -> Vec<Control> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut v = Vec::new();
        while let Ok(c) = h.control_in.try_recv() {
            v.push(c);
        }
        v
    }

    #[tokio::test]
    async fn a_disconnect_keeps_the_host_and_a_resume_reattaches() {
        let (chans, mut h, stopped) = host();
        let (e1, mut c1) = ends();
        let k = Kept::spawn(chans, Duration::from_secs(600), request("game", 1280), e1, hooks(stopped.clone()));
        // Attached: a keyframe asked for, frames and input flow.
        assert!(controls(&mut h).await.contains(&Control::Keyframe));
        h.frames.send(frame(1, true)).await.unwrap();
        assert_eq!(c1.video.recv().await.unwrap().seq, 1);
        c1.input.send(InputEvent::Key { code: 30, down: true }).await.unwrap();
        let s = h.input.recv().await.unwrap();
        assert_eq!((s.seq, &s.event), (1, &InputEvent::Key { code: 30, down: true }));
        assert_eq!(k.state(), KeptState::Streaming);

        // The client goes (the app backgrounded): the session's Bye, then its ends drop.
        c1.control_in.send(Control::Bye { reason: "the client disconnected".into() }).await.unwrap();
        drop(c1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(k.state(), KeptState::Kept { .. }), "{:?}", k.state());
        assert!(stopped.lock().unwrap().is_none(), "a disconnect isn't a quit");
        // Held keys are let go; frames made meanwhile are dropped, not queued.
        assert_eq!(h.input.recv().await.unwrap().event, InputEvent::ReleaseAll);
        for i in 2..20 {
            h.frames.send(frame(i, false)).await.unwrap();
        }

        // Resume, at another size: the same host, a keyframe asked for, the size change
        // passed on, input numbered on from before.
        let (e2, mut c2) = ends();
        k.attach(request("game", 1920), e2).unwrap();
        let got = controls(&mut h).await;
        assert!(got.contains(&Control::Keyframe), "{got:?}");
        assert!(got.iter().any(|c| matches!(c, Control::Resize { display } if display.width == 1920)), "{got:?}");
        assert_eq!(k.state(), KeptState::Streaming);
        h.frames.send(frame(20, true)).await.unwrap();
        assert_eq!(c2.video.recv().await.unwrap().seq, 20);
        c2.input.send(InputEvent::Key { code: 31, down: true }).await.unwrap();
        assert!(h.input.recv().await.unwrap().seq > 2);

        // Quit app ends it, and stops the host as a quit.
        k.quit("the client quit the app");
        assert!(k.ended(Duration::from_secs(2)).await);
        assert!(matches!(c2.control_out.recv().await, Some(Control::Bye { .. })), "the session is told");
        assert_eq!(stopped.lock().unwrap().clone(), Some(("the client quit the app".into(), true)));
        assert!(k.attach(request("game", 1280), ends().0).is_err(), "nothing to resume after a quit");
    }

    #[tokio::test]
    async fn nobody_resuming_ends_it_after_the_keep_window_and_the_host_ending_ends_it() {
        let (chans, _h, stopped) = host();
        let (e1, c1) = ends();
        let k = Kept::spawn(chans, Duration::from_millis(1500), request("game", 1280), e1, hooks(stopped.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(c1);
        assert!(k.ended(Duration::from_secs(4)).await, "{:?}", k.state());
        let (reason, quit) = stopped.lock().unwrap().clone().unwrap();
        assert!(reason.contains("nobody resumed") && !quit, "{reason}");

        // The game exiting (the tick hook) ends it while streaming, not as a quit.
        let (chans, _h, stopped) = host();
        let (e1, _c1) = ends();
        let mut hk = hooks(stopped.clone());
        hk.tick = Box::new(|| Some("the game exited".into()));
        let k = Kept::spawn(chans, KEEP, request("game", 1280), e1, hk);
        assert!(k.ended(Duration::from_secs(3)).await);
        assert_eq!(stopped.lock().unwrap().clone(), Some(("the game exited".into(), false)));
    }

    #[test]
    fn what_a_resume_changes() {
        let a = request("g", 1280).video;
        assert_eq!(changes(&a, &a), (vec![], None));
        let b = VideoParams { codec: VideoCodec::Av1, ..a.clone() };
        let (m, note) = changes(&a, &b);
        assert!(matches!(&m[..], [Control::Prefs(p)] if p.codec == Some(VideoCodec::Av1)) && note.is_none(), "{m:?}");
        let c = VideoParams { width: 2560, height: 1440, fps: 120, ..a.clone() };
        let (m, note) = changes(&a, &c);
        assert!(matches!(&m[..], [Control::Resize { display }] if display.refresh_mhz == 120_000), "{m:?}");
        assert!(note.unwrap().contains("Moonlight scales"));
    }
}
