//! `quiver-stream-host serve`: one stream's [`Host`] behind the transport's [`HostSession`],
//! as quiverd runs it (design/REMOTE.md §2.1; the lines in and out are
//! [`quiver_stream_proto::serve`]).
//!
//! - **Keys and settings** come as the first line on stdin ([`ServeIn::Start`]), never in
//!   argv or the environment. The media socket is passed in (`--socket-fd`, bound by quiverd)
//!   or bound from the spec.
//! - **The target starts at once**, from the caps and prefs `StartStream` carried, so the
//!   game loads while the client connects. The client's Hello refines them: a different
//!   display size becomes a Resize, its prefs go to the encoder.
//! - **The wiring**: frames → `send_video`, audio → `send_audio`, the host's control out →
//!   `send_control` (each new session gets the last Configure again), the client's input →
//!   the host's injectors (renumbered, so a resumed session's numbers carry on), its control
//!   → the host, and the transport's target bitrate → the encoder. Media waits for the
//!   Hello, so nothing piles up in the transport while the client's window opens.
//! - **Sessions.** A client that leaves (Bye, or silence) leaves its session kept for
//!   `keep_secs`: the program keeps running (the test pattern stops, having nothing to
//!   keep), and a [`ServeIn::Session`] with fresh keys starts a new [`HostSession`] on the
//!   same port. `Bye{reason: "quit-game"}` ends the stream and the program.
//! - **Stats lines** for the netlab when `QUIVER_NETLAB_STATS` is set (host stats with the
//!   controller's target).

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_stream_net::netlab::StatsFile;
use quiver_stream_net::{HostOpts, HostSession, LinkState, SessionKeys};
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::serve::{ServeIn, ServeOut, ServeSpec, ServeState, ServeTarget};
use quiver_stream_proto::{
    AudioCodec, BYE_QUIT_GAME, Chroma, ClientCaps, ColorInfo, Control, DisplayCaps, PROTOCOL_VERSION, StreamConfig, StreamPrefs, VideoCodec, now_micros,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, watch};

use crate::recovery::Recovery;
use crate::{Host, HostOptions, Target};

/// `serve`'s command line: nothing secret.
#[derive(Debug, Clone, Default)]
pub struct ServeArgs {
    /// The media socket, already bound (quiverd's).
    pub socket_fd: Option<i32>,
    /// Bind here instead (overrides the spec's `listen`).
    pub listen: Option<SocketAddr>,
    /// The NVENC preset.
    pub preset: u8,
}

/// The first settings for the caps and prefs `StartStream` brought.
pub fn initial_config(caps: &ClientCaps, prefs: &StreamPrefs) -> StreamConfig {
    let codec = pick_codec(caps, prefs.codec);
    let dec = caps.decoders.iter().find(|d| d.codec == codec);
    let (mut w, mut h) = prefs.resolution.unwrap_or((caps.display.width, caps.display.height));
    if let Some(d) = dec {
        w = w.min(d.max_width.max(1));
        h = h.min(d.max_height.max(1));
    }
    let (w, h) = ((w.max(320)) & !1, (h.max(180)) & !1);
    let refresh_mhz = prefs.fps.map(|f| f * 1000).unwrap_or(caps.display.refresh_mhz).max(1000);
    let ten_bit = dec.is_some_and(|d| d.ten_bit);
    let hdr = caps.display.hdr && prefs.hdr != Some(false) && ten_bit;
    let yuv444 = prefs.chroma444 && dec.is_some_and(|d| d.chroma.contains(&Chroma::Yuv444));
    let color = ColorInfo { bit_depth: if hdr { 10 } else { 8 }, chroma: if yuv444 { Chroma::Yuv444 } else { Chroma::Yuv420 }, full_range: false, hdr: hdr.then(default_hdr) };
    StreamConfig {
        codec,
        width: w,
        height: h,
        refresh_mhz,
        color,
        bitrate_kbps: 20_000,
        audio: AudioCodec::Opus,
        audio_channels: if caps.audio_channels >= 2 { 2 } else { caps.audio_channels.max(1) },
        audio_frame_ms: 5,
    }
}

/// The codec to stream: the one asked for, if the client decodes it; else hardware HEVC,
/// AV1, H.264, in that order; else what the client decodes at all; else HEVC.
pub fn pick_codec(caps: &ClientCaps, asked: Option<VideoCodec>) -> VideoCodec {
    let has = |c: VideoCodec, hw: bool| caps.decoders.iter().any(|d| d.codec == c && (d.hardware || !hw));
    if let Some(c) = asked
        && (caps.decoders.is_empty() || has(c, false))
    {
        return c;
    }
    for hw in [true, false] {
        for c in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
            if has(c, hw) {
                return c;
            }
        }
    }
    VideoCodec::Hevc
}

/// HDR10 metadata for a stream whose source gives none: BT.2020, D65, 1000 nits.
fn default_hdr() -> quiver_stream_proto::HdrMetadata {
    quiver_stream_proto::HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 1000, max_fall: 400 }
}

fn host_target(t: &ServeTarget) -> Target {
    match t {
        ServeTarget::TestPattern { motion } => Target::TestPattern { motion: *motion },
        ServeTarget::Command { argv, env } => Target::Command { argv: argv.clone(), env: env.clone() },
        ServeTarget::Display => Target::Display,
    }
}

fn host_opts(spec: &ServeSpec) -> HostOpts {
    let mut o = HostOpts::default();
    if let Some(max) = spec.prefs.max_bitrate_kbps {
        o.max_kbps = max.max(o.min_kbps);
    }
    if let Some(t) = &spec.transport {
        if let Some(v) = t.pacing_fraction {
            o.pacing_fraction = v.clamp(0.05, 1.0);
        }
        if let Some(v) = t.fec_loss_margin {
            o.fec_loss_margin = v;
        }
        if let Some(v) = t.min_kbps {
            o.min_kbps = v;
        }
        if let Some(v) = t.max_kbps {
            o.max_kbps = v;
        }
        if let Some(v) = t.start_kbps {
            o.start_kbps = v;
        }
    }
    o.start_kbps = o.start_kbps.clamp(o.min_kbps, o.max_kbps);
    o.fps_hint = (initial_config(&spec.caps, &spec.prefs).refresh_mhz / 1000).max(1);
    o
}

/// Lines out: stdout and every attached control connection.
#[derive(Clone)]
struct Events {
    tx: broadcast::Sender<String>,
    stdout: mpsc::UnboundedSender<String>,
}

impl Events {
    fn new() -> Events {
        let (tx, _) = broadcast::channel(64);
        let (stdout, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            while let Some(line) = rx.recv().await {
                // quiverd may have gone (a restart): keep going without it.
                if out.write_all(line.as_bytes()).await.is_err() || out.flush().await.is_err() {
                    while rx.recv().await.is_some() {}
                }
            }
        });
        Events { tx, stdout }
    }

    fn send(&self, e: &ServeOut) {
        let mut line = serde_json::to_string(e).unwrap_or_default();
        line.push('\n');
        let _ = self.stdout.send(line.clone());
        let _ = self.tx.send(line);
    }
}

/// Accept attachments on `<dir>/control.sock` from this user's processes only.
fn listen_control(dir: &std::path::Path, events: Events, cmds: mpsc::UnboundedSender<ServeIn>) -> Result<PathBuf> {
    let path = dir.join("control.sock");
    let _ = std::fs::remove_file(&path);
    let (addr, _dir) = quiver_stream_proto::serve::socket_path(dir, "control.sock")?;
    let listener = tokio::net::UnixListener::bind(&addr).with_context(|| format!("bind {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let me = unsafe { libc::getuid() };
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else { continue };
            if conn.peer_cred().ok().map(|c| c.uid()) != Some(me) {
                continue;
            }
            let (r, mut w) = conn.into_split();
            let mut sub = events.tx.subscribe();
            tokio::spawn(async move {
                while let Ok(line) = sub.recv().await {
                    if w.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
            let cmds = cmds.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    match serde_json::from_str::<ServeIn>(&l) {
                        Ok(ServeIn::Start(_)) => tracing::warn!("serve: a second start, ignored"),
                        Ok(c) => {
                            let _ = cmds.send(c);
                        }
                        Err(e) => tracing::warn!("serve: bad control line: {e}"),
                    }
                }
            });
        }
    });
    Ok(path)
}

/// How a session ended.
enum SessionEnd {
    /// The client left, or never came.
    Left(String),
    NewKeys(SessionKeys),
    QuitGame,
    Stop(String),
    /// The program (or the target) ended.
    TargetEnded(String),
}

/// What one turn of a session's loop got.
enum Got {
    Link(Option<LinkState>),
    Frame(Option<quiver_stream_proto::EncodedFrame>),
    Audio(Option<quiver_stream_proto::AudioPacket>),
    HostControl(Option<Control>),
    Input(Option<Stamped>),
    ClientControl(Option<Control>),
    Target,
    Cmd(Option<ServeIn>),
    Tick,
}

struct Serve {
    spec: ServeSpec,
    args: ServeArgs,
    events: Events,
    socket: UdpSocket,
    port: u16,
    host: Option<Host>,
    /// The bitrate the encoder follows; each session's controller feeds it.
    bitrate: watch::Sender<u32>,
    /// The latest Configure, for each new session.
    configure: Option<StreamConfig>,
    config: StreamConfig,
    input_seq: u64,
    stats: StatsFile,
    target_kbps: u32,
    state: ServeState,
    peer: Option<String>,
    kept_until: Option<Instant>,
}

impl Serve {
    fn is_pattern(&self) -> bool {
        matches!(self.spec.target, ServeTarget::TestPattern { .. })
    }

    async fn start_host(&mut self) -> Result<()> {
        let opts = HostOptions {
            target: host_target(&self.spec.target),
            config: self.config.clone(),
            session: self.spec.session.clone(),
            dir: self.spec.dir.clone(),
            preset: self.args.preset.clamp(1, 7),
            recovery: Recovery::RefInvalidation,
            headless_server: self.spec.headless_server,
            max_frames: None,
            force_sandbox_gid: None,
            exe: None,
            constant_bitrate_audio: false,
        };
        let host = Host::start(opts, self.bitrate.subscribe()).await?;
        tracing::info!("serve: target up (pads {:?}{})", host.pad_mode, host.x_display.as_deref().map(|d| format!(", display {d}")).unwrap_or_default());
        if let Some(l) = &host.launch {
            self.events.send(&ServeOut::Ready { launch: l.clone() });
        }
        if let crate::seat::PadMode::Off(why) = &host.pad_mode
            && !self.is_pattern()
        {
            tracing::info!("serve: no pads: {why}");
        }
        self.host = Some(host);
        Ok(())
    }

    async fn stop_host(&mut self) {
        if let Some(h) = self.host.take() {
            match h.stop().await {
                Ok(r) => tracing::info!("serve: target stopped: {} frames, {} dropped; capture {}; encode {}", r.frames, r.dropped, r.capture.summary(), r.encode.summary()),
                Err(e) => tracing::warn!("serve: stopping the target: {e:#}"),
            }
        }
    }

    fn status(&self) -> ServeOut {
        let kept_until_ms = self.kept_until.map(|t| t.saturating_duration_since(Instant::now()).as_millis() as u64);
        ServeOut::Status { state: self.state, peer: self.peer.clone(), pid: std::process::id(), port: self.port, kept_until_ms }
    }

    /// The host's control message: remembered, written as stats, and passed on.
    fn host_control(&mut self, c: &Control) {
        match c {
            Control::Configure(cfg) => {
                self.configure = Some(cfg.clone());
                self.config = cfg.clone();
            }
            Control::HostStats(s) => {
                let extra: BTreeMap<String, f64> = [("target_kbps".to_string(), self.target_kbps as f64)].into();
                self.stats.host(s, &extra);
            }
            Control::Notice { text } => self.events.send(&ServeOut::Notice { text: text.clone() }),
            _ => {}
        }
    }

    /// One client session, from binding to its end.
    async fn session(&mut self, keys: SessionKeys, cmds: &mut mpsc::UnboundedReceiver<ServeIn>) -> Result<SessionEnd> {
        let sock = self.socket.try_clone().context("share the media socket")?;
        let sess = HostSession::bind_socket(sock, keys, host_opts(&self.spec)).context("start the session")?;
        // The transport answers the client's hello on its own thread meanwhile, so the client
        // connects while the target (gamescope, the game's display) comes up.
        if self.host.is_none() {
            self.start_host().await?;
        }
        let mut link = sess.link_state();
        let mut target = sess.target_bitrate();
        self.target_kbps = *target.borrow();
        self.bitrate.send_replace(self.target_kbps);
        self.state = ServeState::Waiting;
        self.peer = None;
        let deadline = Instant::now() + Duration::from_secs(self.spec.connect_secs.max(1));
        let mut hello = false;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let end = loop {
            let Some(host) = self.host.as_mut() else { bail!("no target") };
            let got = tokio::select! {
                r = link.changed() => Got::Link(r.ok().map(|_| link.borrow_and_update().clone())),
                f = host.frames.recv() => Got::Frame(f),
                a = host.audio.recv() => Got::Audio(a),
                c = host.control_out.recv() => Got::HostControl(c),
                i = sess.recv_input() => Got::Input(i),
                c = sess.recv_control() => Got::ClientControl(c),
                r = target.changed() => if r.is_ok() { Got::Target } else { Got::Link(None) },
                c = cmds.recv() => Got::Cmd(c),
                _ = tick.tick() => Got::Tick,
            };
            match got {
                Got::Link(None) => break SessionEnd::Left("the session ended".into()),
                Got::Link(Some(LinkState::Connected(a))) => {
                    if self.state == ServeState::Waiting {
                        tracing::info!("serve: client connected from {a}");
                        self.state = ServeState::Streaming;
                        self.peer = Some(a.to_string());
                        self.kept_until = None;
                        self.events.send(&ServeOut::Connected { peer: a.to_string() });
                    } else {
                        tracing::info!("serve: client moved to {a}");
                        self.peer = Some(a.to_string());
                    }
                }
                Got::Link(Some(LinkState::Closed(r))) => break SessionEnd::Left(r),
                Got::Link(Some(LinkState::Waiting)) => {}
                Got::Frame(None) | Got::Audio(None) | Got::HostControl(None) => break SessionEnd::TargetEnded("the video pipeline stopped".into()),
                Got::Frame(Some(f)) => {
                    if hello && let Err(e) = sess.send_video(f) {
                        tracing::warn!("serve: frame not sent: {e}");
                        if let Some(h) = self.host.as_ref() {
                            let _ = h.control_in.try_send(Control::Keyframe);
                        }
                    }
                }
                Got::Audio(Some(p)) => {
                    if hello {
                        let _ = sess.send_audio(p);
                    }
                }
                Got::HostControl(Some(c)) => {
                    if let (Control::HostStats(s), true) = (&c, self.stats.enabled()) {
                        // Under the netlab: the transport's own view alongside.
                        if let Ok(n) = sess.stats().await {
                            let mut extra: BTreeMap<String, f64> = [("target_kbps".to_string(), self.target_kbps as f64)].into();
                            extra.insert("budget_kbps".into(), n.budget_kbps as f64);
                            extra.insert("acked_kbps".into(), n.acked_kbps.unwrap_or(0) as f64);
                            extra.insert("loss".into(), n.loss as f64);
                            extra.insert("loss_baseline".into(), n.loss_baseline as f64);
                            extra.insert("fec_ratio".into(), n.fec_ratio as f64);
                            extra.insert("pacer_delay_us".into(), n.pacer_delay_us as f64);
                            extra.insert("overuses".into(), n.overuses as f64);
                            extra.insert("resends".into(), n.resends as f64);
                            extra.insert("rtt_us".into(), n.rtt_us.unwrap_or(0) as f64);
                            self.stats.host(s, &extra);
                        }
                    } else {
                        self.host_control(&c);
                    }
                    if hello {
                        let _ = sess.send_control(c);
                    }
                }
                Got::Input(None) | Got::ClientControl(None) => break SessionEnd::Left("the session ended".into()),
                Got::Input(Some(mut s)) => {
                    self.input_seq += 1;
                    s.seq = self.input_seq;
                    let _ = host.input.try_send(s);
                }
                Got::ClientControl(Some(c)) => match c {
                    Control::Hello { version, caps, prefs, .. } => {
                        if version != PROTOCOL_VERSION {
                            let _ = sess.send_control(Control::Bye { reason: format!("the host speaks stream protocol {PROTOCOL_VERSION}, the client {version}") });
                            break SessionEnd::Left(format!("protocol {version}"));
                        }
                        let _ = sess.send_control(Control::Welcome { version: PROTOCOL_VERSION, host_name: self.spec.host_name.clone(), session: self.spec.session.clone() });
                        if let Some(c) = self.configure.clone() {
                            let _ = sess.send_control(Control::Configure(c));
                        }
                        let max = prefs.max_bitrate_kbps.unwrap_or(host_opts(&self.spec).max_kbps);
                        let _ = sess.set_max_bitrate(max);
                        let want = initial_config(&caps, &prefs);
                        let cur = &self.config;
                        // A rate within 2 % (59.94 against 60) isn't a change.
                        let rate_differs = want.refresh_mhz.abs_diff(cur.refresh_mhz) * 50 > cur.refresh_mhz;
                        if (want.width, want.height) != (cur.width, cur.height) || rate_differs {
                            let display = DisplayCaps { width: want.width, height: want.height, refresh_mhz: want.refresh_mhz, ..caps.display.clone() };
                            let _ = host.control_in.try_send(Control::Resize { display });
                        }
                        if prefs != self.spec.prefs {
                            let _ = host.control_in.try_send(Control::Prefs(prefs));
                        }
                        let _ = host.control_in.try_send(Control::Keyframe);
                        hello = true;
                    }
                    Control::Ping { id, sent } => {
                        let _ = sess.send_control(Control::Pong { id, sent, peer_now: now_micros() });
                    }
                    Control::Bye { reason } if reason == BYE_QUIT_GAME => break SessionEnd::QuitGame,
                    Control::Bye { reason } => break SessionEnd::Left(format!("the client left: {reason}")),
                    Control::Prefs(p) => {
                        let max = p.max_bitrate_kbps.unwrap_or(host_opts(&self.spec).max_kbps);
                        let _ = sess.set_max_bitrate(max);
                        let _ = host.control_in.try_send(Control::Prefs(p));
                    }
                    // The window settling at the size the stream already has isn't a resize.
                    Control::Resize { display }
                        if (display.width & !1, display.height & !1) == (self.config.width, self.config.height)
                            && display.refresh_mhz.abs_diff(self.config.refresh_mhz) * 50 <= self.config.refresh_mhz => {}
                    c @ (Control::Resize { .. } | Control::LostFrames { .. } | Control::Keyframe) => {
                        let _ = host.control_in.try_send(c);
                    }
                    Control::Stats(_) | Control::Pong { .. } => {}
                    other => tracing::debug!("serve: client sent {other:?}"),
                },
                Got::Target => {
                    self.target_kbps = *target.borrow_and_update();
                    self.bitrate.send_replace(self.target_kbps);
                }
                Got::Cmd(None) => {}
                Got::Cmd(Some(ServeIn::Session { keys })) => match SessionKeys::from_hex(&keys) {
                    Some(k) => {
                        let _ = sess.send_control(Control::Bye { reason: "the stream was resumed from another client".into() });
                        break SessionEnd::NewKeys(k);
                    }
                    None => tracing::warn!("serve: a session with bad keys, ignored"),
                },
                Got::Cmd(Some(ServeIn::Stop { reason })) => {
                    let _ = sess.send_control(Control::Bye { reason: reason.clone() });
                    break SessionEnd::Stop(reason);
                }
                Got::Cmd(Some(ServeIn::Status)) => self.events.send(&self.status()),
                Got::Cmd(Some(ServeIn::Start(_))) => {}
                Got::Tick => {
                    if !host.alive() {
                        let _ = sess.send_control(Control::Bye { reason: "the game exited".into() });
                        break SessionEnd::TargetEnded("the program exited".into());
                    }
                    if self.state == ServeState::Waiting && Instant::now() > deadline {
                        break SessionEnd::Left("the client never connected".into());
                    }
                }
            }
        };
        // Let the goodbye go out, then end the session and wait for its thread to let go of
        // the socket.
        tokio::time::sleep(Duration::from_millis(50)).await;
        sess.close("ended");
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while !matches!(*link.borrow_and_update(), LinkState::Closed(_)) {
                if link.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(end)
    }

    /// Between sessions: the program runs on (its frames are dropped) until a new session,
    /// a stop, the program's end, or the keep window's end.
    async fn kept(&mut self, cmds: &mut mpsc::UnboundedReceiver<ServeIn>) -> Result<SessionEnd> {
        let until = self.kept_until.unwrap_or_else(Instant::now);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            let got = match self.host.as_mut() {
                Some(host) => tokio::select! {
                    f = host.frames.recv() => Got::Frame(f),
                    a = host.audio.recv() => Got::Audio(a),
                    c = host.control_out.recv() => Got::HostControl(c),
                    c = cmds.recv() => Got::Cmd(c),
                    _ = tick.tick() => Got::Tick,
                },
                None => tokio::select! {
                    c = cmds.recv() => Got::Cmd(c),
                    _ = tick.tick() => Got::Tick,
                },
            };
            match got {
                Got::Frame(None) | Got::Audio(None) | Got::HostControl(None) => return Ok(SessionEnd::TargetEnded("the video pipeline stopped".into())),
                Got::HostControl(Some(c)) => self.host_control(&c),
                Got::Cmd(Some(ServeIn::Session { keys })) => match SessionKeys::from_hex(&keys) {
                    Some(k) => return Ok(SessionEnd::NewKeys(k)),
                    None => tracing::warn!("serve: a session with bad keys, ignored"),
                },
                Got::Cmd(Some(ServeIn::Stop { reason })) => return Ok(SessionEnd::Stop(reason)),
                Got::Cmd(Some(ServeIn::Status)) => self.events.send(&self.status()),
                Got::Tick => {
                    if self.host.as_mut().is_some_and(|h| !h.alive()) {
                        return Ok(SessionEnd::TargetEnded("the program exited".into()));
                    }
                    if Instant::now() >= until {
                        return Ok(SessionEnd::Stop("nobody resumed the stream".into()));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Run `serve` to the stream's end.
pub async fn serve(args: ServeArgs) -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let first = stdin.next_line().await.context("read the stream's spec on stdin")?.context("no spec on stdin")?;
    let spec = match serde_json::from_str::<ServeIn>(&first).context("the first line on stdin isn't a start")? {
        ServeIn::Start(s) => *s,
        _ => bail!("the first line on stdin must be a start"),
    };
    let keys = SessionKeys::from_hex(&spec.keys).context("the spec's keys aren't 80 hex digits")?;
    let (cmd_tx, mut cmds) = mpsc::unbounded_channel::<ServeIn>();
    {
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            while let Ok(Some(l)) = stdin.next_line().await {
                match serde_json::from_str::<ServeIn>(&l) {
                    Ok(c) => {
                        let _ = tx.send(c);
                    }
                    Err(e) => tracing::warn!("serve: bad line on stdin: {e}"),
                }
            }
            tracing::info!("serve: stdin closed; carrying on (control.sock still answers)");
        });
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&spec.dir).with_context(|| format!("create {}", spec.dir.display()))?;
    }
    let events = Events::new();
    let control = listen_control(&spec.dir, events.clone(), cmd_tx.clone())?;
    {
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) else { return };
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
            tokio::select! {
                _ = term.recv() => {}
                _ = async { match int.as_mut() { Some(i) => { i.recv().await; } None => std::future::pending().await } } => {}
            }
            let _ = tx.send(ServeIn::Stop { reason: "the stream host was stopped".into() });
        });
    }
    let socket = match (args.socket_fd, args.listen.or(spec.listen)) {
        (Some(fd), _) => {
            // SAFETY: quiverd passed this fd for exactly this; nothing else in the process owns it.
            let s = unsafe { UdpSocket::from_raw_fd(fd) };
            s.local_addr().context("--socket-fd isn't a bound UDP socket")?;
            s
        }
        (None, Some(a)) => UdpSocket::bind(a).with_context(|| format!("bind {a}"))?,
        (None, None) => UdpSocket::bind("0.0.0.0:47998").context("bind 0.0.0.0:47998")?,
    };
    let port = socket.local_addr()?.port();
    let config = initial_config(&spec.caps, &spec.prefs);
    let start_kbps = host_opts(&spec).start_kbps;
    let (bitrate, _) = watch::channel(start_kbps);
    let keep = spec.keep_secs;
    let mut s = Serve {
        spec,
        args,
        events: events.clone(),
        socket,
        port,
        host: None,
        bitrate,
        configure: None,
        config,
        input_seq: 0,
        stats: StatsFile::from_env(),
        target_kbps: start_kbps,
        state: ServeState::Waiting,
        peer: None,
        kept_until: None,
    };
    events.send(&ServeOut::Listening { port });
    let result = async {
        let mut keys = Some(keys);
        loop {
            let end = match keys.take() {
                Some(k) => s.session(k, &mut cmds).await?,
                None => s.kept(&mut cmds).await?,
            };
            match end {
                SessionEnd::NewKeys(k) => keys = Some(k),
                SessionEnd::Left(reason) => {
                    tracing::info!("serve: {reason}");
                    if keep == 0 {
                        return Ok(reason);
                    }
                    s.state = ServeState::Kept;
                    s.peer = None;
                    s.kept_until = Some(Instant::now() + Duration::from_secs(keep));
                    s.events.send(&ServeOut::Disconnected { reason, keep_secs: keep });
                    // The pattern has nothing to keep: free the GPU until a resume.
                    if s.is_pattern() {
                        s.stop_host().await;
                    }
                }
                SessionEnd::QuitGame => {
                    s.events.send(&ServeOut::QuitGame);
                    return Ok("the client quit the game".to_owned());
                }
                SessionEnd::Stop(r) | SessionEnd::TargetEnded(r) => return Ok(r),
            }
        }
    }
    .await;
    s.stop_host().await;
    let _ = std::fs::remove_file(&control);
    let reason = match &result {
        Ok(r) => r.clone(),
        Err(e) => format!("{e:#}"),
    };
    events.send(&ServeOut::Ended { reason });
    // Let the last line reach stdout and the attached sockets.
    tokio::time::sleep(Duration::from_millis(100)).await;
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_stream_proto::DecoderCaps;

    fn caps(decoders: Vec<DecoderCaps>) -> ClientCaps {
        ClientCaps {
            decoders,
            display: DisplayCaps { width: 2561, height: 1440, refresh_mhz: 119_880, hdr: true, vrr: false, stereo_sbs: false, scale: 1.0 },
            audio_channels: 2,
            pads: vec![],
            microphone: false,
        }
    }

    fn dec(codec: VideoCodec, hardware: bool) -> DecoderCaps {
        DecoderCaps { codec, hardware, max_width: 4096, max_height: 2304, ten_bit: true, chroma: vec![Chroma::Yuv420] }
    }

    #[test]
    fn codec_follows_the_ask_then_hardware() {
        let c = caps(vec![dec(VideoCodec::H264, true), dec(VideoCodec::Av1, true), dec(VideoCodec::Hevc, false)]);
        assert_eq!(pick_codec(&c, None), VideoCodec::Av1, "hardware first, HEVC only in software");
        assert_eq!(pick_codec(&c, Some(VideoCodec::Hevc)), VideoCodec::Hevc, "asked, and decodable");
        assert_eq!(pick_codec(&caps(vec![dec(VideoCodec::H264, true)]), Some(VideoCodec::Av1)), VideoCodec::H264, "asked but not decodable");
        assert_eq!(pick_codec(&caps(vec![]), None), VideoCodec::Hevc);
    }

    #[test]
    fn config_from_caps_and_prefs() {
        let c = caps(vec![dec(VideoCodec::Hevc, true)]);
        let cfg = initial_config(&c, &StreamPrefs::default());
        assert_eq!((cfg.width, cfg.height, cfg.refresh_mhz), (2560, 1440, 119_880));
        assert_eq!(cfg.color.bit_depth, 10);
        assert!(cfg.color.hdr.is_some());
        let p = StreamPrefs { hdr: Some(false), resolution: Some((1920, 1080)), fps: Some(60), ..Default::default() };
        let cfg = initial_config(&c, &p);
        assert_eq!((cfg.width, cfg.height, cfg.refresh_mhz, cfg.color.hdr.is_some()), (1920, 1080, 60_000, false));
        let spec = ServeSpec {
            target: ServeTarget::TestPattern { motion: true },
            keys: String::new(),
            caps: c,
            prefs: StreamPrefs { max_bitrate_kbps: Some(30_000), ..Default::default() },
            host_name: "t".into(),
            session: "s".into(),
            dir: "/tmp".into(),
            listen: None,
            keep_secs: 0,
            connect_secs: 1,
            headless_server: false,
            transport: Some(quiver_stream_proto::serve::TransportTuning { pacing_fraction: Some(2.0), ..Default::default() }),
        };
        let o = host_opts(&spec);
        assert_eq!((o.max_kbps, o.start_kbps, o.pacing_fraction, o.fps_hint), (30_000, 20_000, 1.0, 119));
    }
}
