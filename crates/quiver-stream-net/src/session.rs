//! The async API: [`HostSession`] and [`ClientSession`].
//!
//! Each session runs its core on a dedicated I/O thread with its own socket. The thread
//! sleeps in `ppoll` with microsecond timeouts and 1 µs timer slack, so the pacer's
//! schedule holds to tens of microseconds, which tokio's 1 ms timer wheel couldn't. The
//! async side talks to the thread through channels; nothing here blocks a tokio worker.
//! Handles are cheap to clone; the session ends when the last one is dropped (or on
//! [`close`](HostSession::close)).

use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self as std_mpsc, TryRecvError};
use std::sync::{Arc, Weak};
use std::time::Duration;

use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioPacket, ClientStats, Control, EncodedFrame, InputEvent, Micros, now_micros};
use serde::Serialize;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::client::{ClientCore, ClientEvent, ClientNetStats, ClientOpts};
use crate::conn::Transmit;
use crate::crypto::SessionKeys;
use crate::host::{HostCore, HostEvent, HostNetStats, HostOpts};
use crate::io::{self, Io, RecvBatch, Waker};
use crate::video::{FrameTooLarge, VideoEvent};

/// Where a session stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Host: no client yet. Client: saying hello.
    Waiting,
    /// Host: the client's current address. Client: the host's.
    Connected(SocketAddr),
    Closed(String),
}

/// The session has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stream session closed")
    }
}

impl std::error::Error for Closed {}

// --- the I/O thread ---------------------------------------------------------------------------

/// What the I/O thread drives: a core plus the channels its events go to.
trait Runtime: Send + 'static {
    type Cmd: Send + 'static;
    fn command(&mut self, cmd: Self::Cmd, now: Micros);
    fn datagram(&mut self, now: Micros, from: SocketAddr, data: &mut [u8]);
    fn timeout(&mut self, now: Micros);
    fn transmit(&mut self, now: Micros, out: &mut Vec<u8>) -> Option<Transmit>;
    fn next_timeout(&self, now: Micros) -> Option<Micros>;
    fn flush(&mut self, now: Micros);
    fn close(&mut self);
    fn done(&self) -> bool;
}

struct Thread<C> {
    cmds: std_mpsc::Sender<C>,
    waker: Arc<Waker>,
}

impl<C> Thread<C> {
    fn send(&self, cmd: C) -> Result<(), Closed> {
        self.cmds.send(cmd).map_err(|_| Closed)?;
        self.waker.wake();
        Ok(())
    }
}

fn spawn<R: Runtime>(name: &str, rt: R, sock: UdpSocket, dscp: bool) -> std::io::Result<Thread<R::Cmd>> {
    let io = Io::new(&sock, dscp)?;
    let waker = Arc::new(Waker::new()?);
    let (tx, rx) = std_mpsc::channel();
    let w = waker.clone();
    std::thread::Builder::new().name(name.into()).spawn(move || drive(rt, sock, io, rx, &w))?;
    Ok(Thread { cmds: tx, waker })
}

fn drive<R: Runtime>(mut rt: R, sock: UdpSocket, mut io: Io, cmds: std_mpsc::Receiver<R::Cmd>, waker: &Waker) {
    io::tighten_timer_slack();
    let mut batch = RecvBatch::new(32, io.gro());
    let mut out = Vec::with_capacity(64 * 1500);
    let mut sends = Vec::with_capacity(64);
    let mut detached = false;
    loop {
        let now = now_micros();
        loop {
            match cmds.try_recv() {
                Ok(c) => rt.command(c, now),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !detached {
                        detached = true;
                        rt.close();
                    }
                    break;
                }
            }
        }
        for _ in 0..16 {
            let Ok(n) = batch.recv(&sock) else { break };
            if n == 0 {
                break;
            }
            let now = now_micros();
            for i in 0..n {
                let (from, _, seg) = batch.meta[i];
                for d in batch.buf_mut(i).chunks_mut(seg) {
                    rt.datagram(now, from, d);
                }
            }
        }
        let now = now_micros();
        if rt.next_timeout(now).is_some_and(|t| t <= now) {
            rt.timeout(now);
        }
        transmit_all(&mut rt, now, &mut out, &mut sends, &mut io, &sock);
        rt.flush(now);
        if rt.done() {
            break;
        }
        let now = now_micros();
        let wait = rt.next_timeout(now).map(|t| t.saturating_sub(now));
        if wait != Some(0) {
            io::wait(&sock, waker, wait.or(if detached { Some(100_000) } else { None }));
        }
    }
}

/// Sends everything the core has due, batching runs of same-size datagrams to the same
/// place into one GSO send.
fn transmit_all<R: Runtime>(rt: &mut R, now: Micros, out: &mut Vec<u8>, sends: &mut Vec<Transmit>, io: &mut Io, sock: &UdpSocket) {
    loop {
        out.clear();
        sends.clear();
        while sends.len() < 64 && out.len() < 60_000 {
            match rt.transmit(now, out) {
                Some(t) => sends.push(t),
                None => break,
            }
        }
        let full = sends.len() == 64 || out.len() >= 60_000;
        let mut i = 0;
        let mut offset = 0;
        while i < sends.len() {
            let first = sends[i];
            let mut j = i + 1;
            let mut len = first.len;
            while j < sends.len()
                && j - i < io.max_segments()
                && sends[j].dest == first.dest
                && sends[j].class == first.class
                && sends[j].len <= first.len
                && sends[j - 1].len == first.len
            {
                len += sends[j].len;
                j += 1;
            }
            let seg = (j - i > 1).then_some(first.len);
            if let Err(e) = io.send(sock, first.dest, &out[offset..offset + len], seg, first.class) {
                tracing::trace!("send to {}: {e}", first.dest);
            }
            offset += len;
            i = j;
        }
        if !full {
            return;
        }
    }
}

fn bind_any_for(server: SocketAddr) -> std::io::Result<UdpSocket> {
    if server.is_ipv6() { UdpSocket::bind("[::]:0") } else { UdpSocket::bind("0.0.0.0:0") }
}

// --- the host -------------------------------------------------------------------------------------

enum HostCmd {
    Video(EncodedFrame),
    Audio(AudioPacket),
    Control(Control),
    MaxBitrate(u32),
    Stats(oneshot::Sender<HostNetStats>),
    Close(String),
}

struct HostRuntime {
    core: HostCore,
    input: mpsc::UnboundedSender<Stamped>,
    control: mpsc::UnboundedSender<Control>,
    audio: mpsc::UnboundedSender<AudioPacket>,
    target: watch::Sender<u32>,
    link: watch::Sender<LinkState>,
    last_target: u32,
}

impl Runtime for HostRuntime {
    type Cmd = HostCmd;

    fn command(&mut self, cmd: HostCmd, now: Micros) {
        match cmd {
            HostCmd::Video(f) => {
                if let Err(e) = self.core.send_video(f, now) {
                    tracing::warn!("{e}");
                }
            }
            HostCmd::Audio(p) => self.core.send_audio(p, now),
            HostCmd::Control(c) => self.core.send_control(&c),
            HostCmd::MaxBitrate(k) => self.core.set_max_bitrate(k),
            HostCmd::Stats(reply) => {
                let _ = reply.send(self.core.stats(now));
            }
            HostCmd::Close(reason) => self.core.close(&reason),
        }
    }

    fn datagram(&mut self, now: Micros, from: SocketAddr, data: &mut [u8]) {
        self.core.handle_datagram(now, from, data);
    }

    fn timeout(&mut self, now: Micros) {
        self.core.handle_timeout(now);
    }

    fn transmit(&mut self, now: Micros, out: &mut Vec<u8>) -> Option<Transmit> {
        self.core.poll_transmit(now, out)
    }

    fn next_timeout(&self, now: Micros) -> Option<Micros> {
        self.core.poll_timeout(now).map(|t| t.min(now + 50_000))
    }

    fn flush(&mut self, now: Micros) {
        while let Some(e) = self.core.poll_event() {
            match e {
                HostEvent::Connected(a) | HostEvent::Migrated { to: a, .. } => {
                    self.link.send_replace(LinkState::Connected(a));
                }
                HostEvent::Input(s) => {
                    let _ = self.input.send(s);
                }
                HostEvent::Control(c) => {
                    let _ = self.control.send(c);
                }
                HostEvent::Audio(p) => {
                    let _ = self.audio.send(p);
                }
                HostEvent::Closed { reason } => {
                    self.link.send_replace(LinkState::Closed(reason));
                }
            }
        }
        let t = self.core.encoder_target_kbps(now);
        if t.abs_diff(self.last_target) * 100 > self.last_target.max(1) {
            self.last_target = t;
            self.target.send_replace(t);
        }
    }

    fn close(&mut self) {
        self.core.close("dropped");
    }

    fn done(&self) -> bool {
        self.core.is_closed()
    }
}

struct HostInner {
    thread: Thread<HostCmd>,
    local: SocketAddr,
    input: Mutex<mpsc::UnboundedReceiver<Stamped>>,
    control: Mutex<mpsc::UnboundedReceiver<Control>>,
    audio: Mutex<mpsc::UnboundedReceiver<AudioPacket>>,
    target: watch::Receiver<u32>,
    link: watch::Receiver<LinkState>,
}

/// The host's end of a stream. Clone it freely; every clone is the same session.
#[derive(Clone)]
pub struct HostSession {
    inner: Arc<HostInner>,
}

impl HostSession {
    /// Binds the media socket at `addr` (port 0 picks one; see
    /// [`local_addr`](Self::local_addr)) and starts listening for the client's hello.
    pub async fn bind(addr: SocketAddr, keys: SessionKeys, opts: HostOpts) -> std::io::Result<Self> {
        Self::bind_socket(UdpSocket::bind(addr)?, keys, opts)
    }

    /// As [`bind`](Self::bind), with a socket the caller made (one passed down by quiverd,
    /// say).
    pub fn bind_socket(sock: UdpSocket, keys: SessionKeys, opts: HostOpts) -> std::io::Result<Self> {
        let local = sock.local_addr()?;
        let (input_tx, input) = mpsc::unbounded_channel();
        let (control_tx, control) = mpsc::unbounded_channel();
        let (audio_tx, audio) = mpsc::unbounded_channel();
        let (target_tx, target) = watch::channel(opts.start_kbps);
        let (link_tx, link) = watch::channel(LinkState::Waiting);
        let dscp = opts.dscp;
        let rt = HostRuntime {
            last_target: opts.start_kbps,
            core: HostCore::new(&keys, opts, now_micros()),
            input: input_tx,
            control: control_tx,
            audio: audio_tx,
            target: target_tx,
            link: link_tx,
        };
        let thread = spawn("stream-net-host", rt, sock, dscp)?;
        let inner = HostInner {
            thread,
            local,
            input: Mutex::new(input),
            control: Mutex::new(control),
            audio: Mutex::new(audio),
            target,
            link,
        };
        Ok(Self { inner: Arc::new(inner) })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local
    }

    /// Waits for the client's first validated hello; returns its address.
    pub async fn wait_connected(&self) -> Result<SocketAddr, Closed> {
        wait_connected(self.inner.link.clone()).await
    }

    /// Where the session stands; changes when the client connects, moves, or leaves.
    pub fn link_state(&self) -> watch::Receiver<LinkState> {
        self.inner.link.clone()
    }

    /// Queues a frame. It never blocks: the frame is cut, protected and paced on the I/O
    /// thread. Frames before the client connects are dropped.
    pub fn send_video(&self, frame: EncodedFrame) -> Result<(), SendError> {
        if frame.data.len() + crate::codec::FRAME_META_LEN > crate::fec::MAX_FRAME_BYTES {
            return Err(SendError::TooLarge(FrameTooLarge));
        }
        self.inner.thread.send(HostCmd::Video(frame)).map_err(SendError::Closed)
    }

    pub fn send_audio(&self, packet: AudioPacket) -> Result<(), Closed> {
        self.inner.thread.send(HostCmd::Audio(packet))
    }

    pub fn send_control(&self, msg: Control) -> Result<(), Closed> {
        self.inner.thread.send(HostCmd::Control(msg))
    }

    /// The next input event, applied once and in order; None once the session ends.
    pub async fn recv_input(&self) -> Option<Stamped> {
        self.inner.input.lock().await.recv().await
    }

    pub async fn recv_control(&self) -> Option<Control> {
        self.inner.control.lock().await.recv().await
    }

    /// The client's microphone.
    pub async fn recv_audio(&self) -> Option<AudioPacket> {
        self.inner.audio.lock().await.recv().await
    }

    /// The bitrate the encoder should aim for, kbit/s. It changes as the bandwidth
    /// estimate, FEC level and pacer queue change; reconfigure the encoder on each change.
    pub fn target_bitrate(&self) -> watch::Receiver<u32> {
        self.inner.target.clone()
    }

    /// Caps the bitrate (the client's `StreamPrefs::max_bitrate_kbps`).
    pub fn set_max_bitrate(&self, kbps: u32) -> Result<(), Closed> {
        self.inner.thread.send(HostCmd::MaxBitrate(kbps))
    }

    pub async fn stats(&self) -> Result<HostNetStats, Closed> {
        let (tx, rx) = oneshot::channel();
        self.inner.thread.send(HostCmd::Stats(tx))?;
        rx.await.map_err(|_| Closed)
    }

    /// Ends the session: the client is told, and the I/O thread stops.
    pub fn close(&self, reason: &str) {
        let _ = self.inner.thread.send(HostCmd::Close(reason.to_owned()));
    }

    /// Stats every `period`, until the session ends.
    pub fn stats_stream(&self, period: Duration) -> mpsc::Receiver<StatsReport> {
        let weak = Arc::downgrade(&self.inner);
        stats_task(period, move || {
            let weak = weak.clone();
            async move {
                let s = HostSession { inner: Weak::upgrade(&weak)? };
                let net = s.stats().await.ok()?;
                Some(StatsReport::Host { at_us: now_micros(), net })
            }
        })
    }

    /// Writes [`stats_stream`](Self::stats_stream) to `out` as JSON lines.
    pub fn stats_json_lines(&self, period: Duration, out: impl Write + Send + 'static) -> tokio::task::JoinHandle<()> {
        write_json_lines(self.stats_stream(period), out)
    }
}

/// Why [`HostSession::send_video`] refused a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    TooLarge(FrameTooLarge),
    Closed(Closed),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::TooLarge(e) => e.fmt(f),
            SendError::Closed(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SendError {}

// --- the client -------------------------------------------------------------------------------------

enum ClientCmd {
    Input(InputEvent, Micros),
    Audio(AudioPacket),
    Control(Control),
    Stats(oneshot::Sender<ClientStats>),
    NetStats(oneshot::Sender<ClientNetStats>),
    Close(String),
}

/// The clock estimate, as the client's I/O thread last saw it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockInfo {
    /// The host's clock minus ours, µs.
    pub offset_us: Option<i64>,
    pub rtt_us: Option<u64>,
}

struct ClientRuntime {
    core: ClientCore,
    server: SocketAddr,
    video: mpsc::Sender<VideoEvent>,
    /// Frames the app didn't take in time, to report as a gap once there is room.
    overflow: Option<(u64, u64)>,
    audio: mpsc::UnboundedSender<AudioPacket>,
    control: mpsc::UnboundedSender<Control>,
    link: watch::Sender<LinkState>,
    clock: watch::Sender<ClockInfo>,
}

impl ClientRuntime {
    fn push_video(&mut self, e: VideoEvent) {
        if let Some((a, b)) = self.overflow {
            if self.video.try_send(VideoEvent::Gap { since_seq: a, through_seq: b }).is_err() {
                self.overflow = Some(extend_gap((a, b), &e));
                return;
            }
            self.overflow = None;
        }
        if let Err(mpsc::error::TrySendError::Full(e)) = self.video.try_send(e) {
            tracing::debug!("the app isn't taking frames; dropping");
            self.overflow = Some(extend_gap(self.overflow.unwrap_or((u64::MAX, 0)), &e));
        }
    }
}

fn extend_gap((a, b): (u64, u64), e: &VideoEvent) -> (u64, u64) {
    match e {
        VideoEvent::Frame(f) => (a.min(f.seq), b.max(f.seq)),
        VideoEvent::Gap { since_seq, through_seq } => (a.min(*since_seq), b.max(*through_seq)),
    }
}

impl Runtime for ClientRuntime {
    type Cmd = ClientCmd;

    fn command(&mut self, cmd: ClientCmd, now: Micros) {
        match cmd {
            ClientCmd::Input(e, at) => {
                self.core.send_input(e, at, now);
            }
            ClientCmd::Audio(p) => self.core.send_audio(p),
            ClientCmd::Control(c) => self.core.send_control(&c),
            ClientCmd::Stats(reply) => {
                let _ = reply.send(self.core.take_stats());
            }
            ClientCmd::NetStats(reply) => {
                let _ = reply.send(self.core.net_stats());
            }
            ClientCmd::Close(reason) => self.core.close(&reason),
        }
    }

    fn datagram(&mut self, now: Micros, from: SocketAddr, data: &mut [u8]) {
        self.core.handle_datagram(now, from, data);
    }

    fn timeout(&mut self, now: Micros) {
        self.core.handle_timeout(now);
    }

    fn transmit(&mut self, now: Micros, out: &mut Vec<u8>) -> Option<Transmit> {
        self.core.poll_transmit(now, out)
    }

    fn next_timeout(&self, now: Micros) -> Option<Micros> {
        self.core.poll_timeout(now).map(|t| t.min(now + 50_000))
    }

    fn flush(&mut self, _now: Micros) {
        while let Some(e) = self.core.poll_event() {
            match e {
                ClientEvent::Connected => {
                    self.link.send_replace(LinkState::Connected(self.server));
                }
                ClientEvent::Video(v) => self.push_video(v),
                ClientEvent::Audio(p) => {
                    let _ = self.audio.send(p);
                }
                ClientEvent::Control(c) => {
                    let _ = self.control.send(c);
                }
                ClientEvent::Closed { reason } => {
                    self.link.send_replace(LinkState::Closed(reason));
                }
            }
        }
        let info = ClockInfo { offset_us: self.core.clock_offset(), rtt_us: self.core.rtt_us() };
        self.clock.send_if_modified(|c| std::mem::replace(c, info) != info);
    }

    fn close(&mut self) {
        self.core.close("dropped");
    }

    fn done(&self) -> bool {
        self.core.is_closed()
    }
}

struct ClientInner {
    thread: Thread<ClientCmd>,
    local: SocketAddr,
    video: Mutex<mpsc::Receiver<VideoEvent>>,
    audio: Mutex<mpsc::UnboundedReceiver<AudioPacket>>,
    control: Mutex<mpsc::UnboundedReceiver<Control>>,
    link: watch::Receiver<LinkState>,
    clock: watch::Receiver<ClockInfo>,
}

/// The client's end of a stream. Clone it freely; every clone is the same session.
#[derive(Clone)]
pub struct ClientSession {
    inner: Arc<ClientInner>,
}

/// How many frames may wait for the app before the oldest are reported as a gap.
const VIDEO_QUEUE: usize = 64;

impl ClientSession {
    /// Says hello to the host at `server` until it answers (validating our address), or
    /// fails with `TimedOut` after `opts.connect_timeout_us`.
    pub async fn connect(server: SocketAddr, keys: SessionKeys, opts: ClientOpts) -> std::io::Result<Self> {
        Self::connect_socket(bind_any_for(server)?, server, keys, opts).await
    }

    /// As [`connect`](Self::connect), from a socket the caller made.
    pub async fn connect_socket(sock: UdpSocket, server: SocketAddr, keys: SessionKeys, opts: ClientOpts) -> std::io::Result<Self> {
        let local = sock.local_addr()?;
        let (video_tx, video) = mpsc::channel(VIDEO_QUEUE);
        let (audio_tx, audio) = mpsc::unbounded_channel();
        let (control_tx, control) = mpsc::unbounded_channel();
        let (link_tx, link) = watch::channel(LinkState::Waiting);
        let (clock_tx, clock) = watch::channel(ClockInfo::default());
        let dscp = opts.dscp;
        let rt = ClientRuntime {
            core: ClientCore::new(&keys, server, opts, now_micros()),
            server,
            video: video_tx,
            overflow: None,
            audio: audio_tx,
            control: control_tx,
            link: link_tx,
            clock: clock_tx,
        };
        let thread = spawn("stream-net-client", rt, sock, dscp)?;
        let inner = ClientInner {
            thread,
            local,
            video: Mutex::new(video),
            audio: Mutex::new(audio),
            control: Mutex::new(control),
            link: link.clone(),
            clock,
        };
        let session = Self { inner: Arc::new(inner) };
        match wait_connected(link).await {
            Ok(_) => Ok(session),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the stream host didn't answer")),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local
    }

    pub fn link_state(&self) -> watch::Receiver<LinkState> {
        self.inner.link.clone()
    }

    /// The next frame or gap, in frame order; None once the session ends.
    pub async fn recv_video(&self) -> Option<VideoEvent> {
        self.inner.video.lock().await.recv().await
    }

    /// The next game audio packet, in arrival order (the jitter buffer orders them).
    pub async fn recv_audio(&self) -> Option<AudioPacket> {
        self.inner.audio.lock().await.recv().await
    }

    pub async fn recv_control(&self) -> Option<Control> {
        self.inner.control.lock().await.recv().await
    }

    /// Sends an input event, stamped now.
    pub fn send_input(&self, event: InputEvent) -> Result<(), Closed> {
        self.send_input_at(event, now_micros())
    }

    /// Sends an input event captured at `at` (our clock).
    pub fn send_input_at(&self, event: InputEvent, at: Micros) -> Result<(), Closed> {
        self.inner.thread.send(ClientCmd::Input(event, at))
    }

    /// Sends microphone audio.
    pub fn send_audio(&self, packet: AudioPacket) -> Result<(), Closed> {
        self.inner.thread.send(ClientCmd::Audio(packet))
    }

    pub fn send_control(&self, msg: Control) -> Result<(), Closed> {
        self.inner.thread.send(ClientCmd::Control(msg))
    }

    /// The host's clock minus ours, µs. A host timestamp `t` is `t - offset` on our clock,
    /// so `capture_to_present = presented - (frame.captured - offset)`.
    pub fn clock_offset(&self) -> Option<i64> {
        self.inner.clock.borrow().offset_us
    }

    /// Converts a host timestamp to our clock, once the offset is known.
    pub fn host_to_local(&self, host: Micros) -> Option<Micros> {
        self.clock_offset().map(|o| (host as i64 - o).max(0) as Micros)
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.inner.clock.borrow().rtt_us
    }

    pub fn clock(&self) -> watch::Receiver<ClockInfo> {
        self.inner.clock.clone()
    }

    /// The transport's part of [`ClientStats`] since the last call (see
    /// [`ClientCore::take_stats`]); the client adds its own fields and sends
    /// `Control::Stats`.
    pub async fn take_stats(&self) -> Result<ClientStats, Closed> {
        let (tx, rx) = oneshot::channel();
        self.inner.thread.send(ClientCmd::Stats(tx))?;
        rx.await.map_err(|_| Closed)
    }

    /// Cumulative transport counters.
    pub async fn net_stats(&self) -> Result<ClientNetStats, Closed> {
        let (tx, rx) = oneshot::channel();
        self.inner.thread.send(ClientCmd::NetStats(tx))?;
        rx.await.map_err(|_| Closed)
    }

    pub fn close(&self, reason: &str) {
        let _ = self.inner.thread.send(ClientCmd::Close(reason.to_owned()));
    }

    /// Stats every `period`, until the session ends. Each report takes the period's
    /// [`ClientStats`], so don't also call [`take_stats`](Self::take_stats) while one runs.
    pub fn stats_stream(&self, period: Duration) -> mpsc::Receiver<StatsReport> {
        let weak = Arc::downgrade(&self.inner);
        stats_task(period, move || {
            let weak = weak.clone();
            async move {
                let s = ClientSession { inner: Weak::upgrade(&weak)? };
                let stats = s.take_stats().await.ok()?;
                let net = s.net_stats().await.ok()?;
                Some(StatsReport::Client { at_us: now_micros(), stats, net })
            }
        })
    }

    /// Writes [`stats_stream`](Self::stats_stream) to `out` as JSON lines.
    pub fn stats_json_lines(&self, period: Duration, out: impl Write + Send + 'static) -> tokio::task::JoinHandle<()> {
        write_json_lines(self.stats_stream(period), out)
    }
}

async fn wait_connected(mut link: watch::Receiver<LinkState>) -> Result<SocketAddr, Closed> {
    loop {
        match &*link.borrow_and_update() {
            LinkState::Connected(a) => return Ok(*a),
            LinkState::Closed(_) => return Err(Closed),
            LinkState::Waiting => {}
        }
        link.changed().await.map_err(|_| Closed)?;
    }
}

// --- stats reports --------------------------------------------------------------------------------

/// One periodic stats report, as [`stats_json_lines`](HostSession::stats_json_lines)
/// writes it: `{"t":"host_stats",...}` or `{"t":"client_stats",...}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum StatsReport {
    #[serde(rename = "host_stats")]
    Host { at_us: Micros, net: HostNetStats },
    #[serde(rename = "client_stats")]
    Client { at_us: Micros, stats: ClientStats, net: ClientNetStats },
}

fn stats_task<F, Fut>(period: Duration, mut take: F) -> mpsc::Receiver<StatsReport>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<StatsReport>> + Send,
{
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(report) = take().await else { return };
            if tx.send(report).await.is_err() {
                return;
            }
        }
    });
    rx
}

fn write_json_lines(mut reports: mpsc::Receiver<StatsReport>, mut out: impl Write + Send + 'static) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(r) = reports.recv().await {
            let Ok(line) = serde_json::to_string(&r) else { continue };
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return;
            }
        }
    })
}
