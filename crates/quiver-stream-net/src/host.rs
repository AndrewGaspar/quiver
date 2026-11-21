//! The host's end of the transport, as a state machine with no I/O of its own.
//!
//! [`HostCore`] takes datagrams, commands and the time, and gives back datagrams to send,
//! events, and when it next needs the time. [`crate::HostSession`] drives it with a socket
//! on a thread; the simulator ([`crate::sim`]) drives it in virtual time.

use std::collections::VecDeque;
use std::net::SocketAddr;

use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioPacket, Control, EncodedFrame, Micros};
use serde::Serialize;

use crate::audio::{AudioReceiver, AudioSender};
use crate::bwe::{Bwe, BweOpts};
use crate::clock::{ClockSync, PING_INTERVAL};
use crate::conn::{self, Opener, STATUS_INTERVAL, Sealer, SmallChunks, Status, Transmit};
use crate::control::{self, ControlReceiver, ControlSender};
use crate::crypto::{Cipher, Direction, OVERHEAD, SessionKeys};
use crate::fec::FecPlanner;
use crate::feedback;
use crate::input::InputReceiver;
use crate::io::Class;
use crate::pacer::Pacer;
use crate::path::{MtuProber, PathChange, PathValidator};
use crate::video::{self, FrameTooLarge, VIDEO_HEADER, VideoSender};
use crate::wire::{self, CHUNK_HEADER, Chunks, Kind};

/// The host's settings. The defaults suit a LAN; every field has a sensible range noted.
#[derive(Debug, Clone)]
pub struct HostOpts {
    /// The bandwidth estimator's floor, ceiling and starting point, in kbit/s. The ceiling
    /// is also what [`StreamPrefs::max_bitrate_kbps`](quiver_stream_proto::StreamPrefs)
    /// should set (see [`crate::HostSession::set_max_bitrate`]).
    pub min_kbps: u32,
    pub max_kbps: u32,
    pub start_kbps: u32,
    /// Probe the path at the start (and after a migration) to reach the rate quickly.
    pub probe: bool,
    /// The share of the frame interval a frame's datagrams are spread over (0.05..=1.0).
    pub pacing_fraction: f32,
    /// Never pace faster than this, kbit/s, whatever the frame size.
    pub max_pacing_kbps: u32,
    /// The chance of losing a frame despite FEC that recovery is planned for.
    pub fec_frame_loss: f64,
    /// The same, while the RTT is short enough for the client's NACKs to rescue a frame
    /// (at most `nack_rtt_us`): FEC then only has to make NACKs rare.
    pub fec_frame_loss_nack: f64,
    pub nack_rtt_us: Micros,
    /// At most this share of recovery to data shards (tiny frames excepted).
    pub fec_max_ratio: f64,
    /// At least this many recovery shards a frame, even on a clean path.
    pub fec_min_parity: usize,
    /// Plan FEC for the measured loss times this, plus 0.2 %.
    pub fec_loss_margin: f64,
    /// Answer the client's NACKs from the last 200 ms of frames.
    pub answer_nacks: bool,
    /// How many earlier audio packets ride along with each one.
    pub audio_redundancy: usize,
    /// Probe for a path MTU above 1232 bytes, up to this datagram size.
    pub mtu_probe: bool,
    pub max_datagram: usize,
    /// The frame rate to assume until frames show the real one.
    pub fps_hint: u32,
    pub cipher: Cipher,
    /// Mark datagrams with DSCP (AF41 video, EF the rest).
    pub dscp: bool,
    /// The session ends after this long with nothing from the client.
    pub idle_timeout_us: Micros,
}

impl Default for HostOpts {
    fn default() -> Self {
        Self {
            min_kbps: 2_000,
            max_kbps: 150_000,
            start_kbps: 20_000,
            probe: true,
            pacing_fraction: 0.3,
            max_pacing_kbps: 2_000_000,
            fec_frame_loss: 1e-4,
            fec_frame_loss_nack: 2e-3,
            nack_rtt_us: 5_000,
            fec_max_ratio: 0.5,
            fec_min_parity: 1,
            fec_loss_margin: 1.25,
            answer_nacks: true,
            audio_redundancy: 2,
            mtu_probe: true,
            max_datagram: 1472,
            fps_hint: 120,
            cipher: Cipher::Aes256Gcm,
            dscp: true,
            idle_timeout_us: 30_000_000,
        }
    }
}

/// What the host learns from the client.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// The client's address is validated; the stream can start.
    Connected(SocketAddr),
    /// The client moved (Wi-Fi roaming, NAT rebinding). The stream goes on.
    Migrated { from: SocketAddr, to: SocketAddr },
    Input(Stamped),
    Control(Control),
    /// Microphone audio from the client.
    Audio(AudioPacket),
    Closed { reason: String },
}

/// The host's transport counters and estimates, for logs and the stats overlay.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HostNetStats {
    pub peer: Option<String>,
    pub rtt_us: Option<u64>,
    pub min_rtt_us: Option<u64>,
    /// What the encoder should produce, kbit/s.
    pub encoder_kbps: u32,
    /// What may be sent in all (media, FEC, headers), kbit/s.
    pub budget_kbps: u32,
    /// The rate the client received at, kbit/s.
    pub acked_kbps: Option<u32>,
    pub loss: f32,
    pub loss_baseline: f32,
    pub fec_ratio: f32,
    pub datagram_bytes: usize,
    pub pacer_delay_us: u64,
    pub pacer_queued_bytes: usize,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub shards_sent: u64,
    pub resends: u64,
    pub nacks_received: u64,
    pub audio_sent: u64,
    pub input_events: u64,
    pub input_duplicates: u64,
    pub control_retransmits: u64,
    pub rejected: u64,
    pub replayed: u64,
    pub migrations: u64,
    pub overuses: u64,
    /// `(rate tried, rate measured)` for each probe, bit/s.
    pub probes: Vec<(u64, u64)>,
}

/// Bytes a video datagram adds around its shard.
const VIDEO_OVERHEAD: usize = OVERHEAD + CHUNK_HEADER + VIDEO_HEADER;

pub struct HostCore {
    opts: HostOpts,
    sealer: Sealer,
    opener: Opener,
    path: PathValidator,
    mtu: MtuProber,
    connected: bool,
    clock: ClockSync,
    next_ping: Micros,
    ping_id: u32,
    next_status: Micros,
    small: SmallChunks,
    control_tx: ControlSender,
    control_rx: ControlReceiver,
    input_rx: InputReceiver,
    audio_tx: AudioSender,
    audio_rx: AudioReceiver,
    video: VideoSender,
    pacer: Pacer,
    bwe: Bwe,
    hello_ack_due: bool,
    events: VecDeque<HostEvent>,
    last_rx: Micros,
    closing: Option<(String, u8)>,
    closed: bool,
    frame_interval: f64,
    last_captured: Option<Micros>,
    audio_bps: f64,
    last_audio: Option<Micros>,
    stats: HostNetStats,
}

impl HostCore {
    pub fn new(keys: &SessionKeys, opts: HostOpts, now: Micros) -> Self {
        let bwe = Bwe::new(BweOpts {
            min_bps: u64::from(opts.min_kbps) * 1000,
            max_bps: u64::from(opts.max_kbps) * 1000,
            start_bps: u64::from(opts.start_kbps) * 1000,
            probe: opts.probe,
        });
        Self {
            sealer: Sealer::new(keys, Direction::HostToClient, opts.cipher),
            opener: Opener::new(keys, Direction::ClientToHost, opts.cipher),
            path: PathValidator::default(),
            mtu: MtuProber::new(opts.max_datagram, opts.mtu_probe),
            connected: false,
            clock: ClockSync::default(),
            next_ping: now,
            ping_id: 0,
            next_status: now,
            small: SmallChunks::default(),
            control_tx: ControlSender::default(),
            control_rx: ControlReceiver::default(),
            input_rx: InputReceiver::default(),
            audio_tx: AudioSender::new(opts.audio_redundancy),
            audio_rx: AudioReceiver::default(),
            video: VideoSender::new(FecPlanner::new(opts.fec_frame_loss, opts.fec_max_ratio, opts.fec_min_parity)),
            pacer: Pacer::default(),
            bwe,
            hello_ack_due: false,
            events: VecDeque::new(),
            last_rx: now,
            closing: None,
            closed: false,
            frame_interval: 1e6 / f64::from(opts.fps_hint.max(1)),
            last_captured: None,
            audio_bps: 0.0,
            last_audio: None,
            stats: HostNetStats::default(),
            opts,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.path.current
    }

    pub fn poll_event(&mut self) -> Option<HostEvent> {
        self.events.pop_front()
    }

    // --- commands ---------------------------------------------------------------------------

    /// Queues a frame. Before the client is connected, frames are dropped (and counted).
    pub fn send_video(&mut self, frame: EncodedFrame, now: Micros) -> Result<(), FrameTooLarge> {
        if !self.connected || self.closing.is_some() {
            self.stats.frames_dropped += 1;
            return Ok(());
        }
        self.observe_interval(frame.captured);
        let nackable = self.opts.answer_nacks && self.clock.rtt().is_some_and(|r| r <= self.opts.nack_rtt_us);
        self.video.planner.set_target(if nackable { self.opts.fec_frame_loss_nack } else { self.opts.fec_frame_loss });
        let loss = self.bwe.planning_loss();
        let planned = if loss > 0.0 { loss * self.opts.fec_loss_margin + 0.002 } else { 0.0 };
        self.video.planner.set_loss(planned);
        let max_shard = self.mtu.current - VIDEO_OVERHEAD;
        let sharded = self.video.shard(frame, max_shard, now)?;
        let datagram = sharded.shard + VIDEO_OVERHEAD;
        let rate = self.pacing_rate(sharded.total() * datagram);
        self.pacer.push_frame(&sharded, datagram, rate, now);
        self.stats.frames_sent += 1;
        Ok(())
    }

    fn observe_interval(&mut self, captured: Micros) {
        if let Some(last) = self.last_captured.replace(captured) {
            let d = captured.saturating_sub(last) as f64;
            if (1_000.0..=100_000.0).contains(&d) {
                self.frame_interval = 0.9 * self.frame_interval + 0.1 * d;
            }
        }
    }

    /// The rate, bps, to spread a frame of `bytes` at: over `pacing_fraction` of the frame
    /// interval, but never slower than an average frame would go (so small frames don't
    /// dawdle) nor more than twice that (so a keyframe doesn't burst at line rate).
    fn pacing_rate(&self, bytes: usize) -> f64 {
        let fraction = f64::from(self.opts.pacing_fraction.clamp(0.05, 1.0));
        let span_s = self.frame_interval * fraction / 1e6;
        let floor = self.bwe.target_bps() as f64 / fraction;
        let want = bytes as f64 * 8.0 / span_s;
        want.clamp(floor, 2.0 * floor).min(f64::from(self.opts.max_pacing_kbps) * 1000.0)
    }

    pub fn send_audio(&mut self, packet: AudioPacket, now: Micros) {
        if !self.connected {
            return;
        }
        if let Some(last) = self.last_audio.replace(now) {
            let dt = (now.saturating_sub(last)).max(1_000) as f64 / 1e6;
            let bps = (packet.data.len() + OVERHEAD + 40) as f64 * 8.0 / dt;
            self.audio_bps = 0.95 * self.audio_bps + 0.05 * bps;
        }
        let budget = conn::body_budget(conn::plain_budget(crate::path::BASE_DATAGRAM));
        let mut body = Vec::new();
        self.audio_tx.write_chunk(&mut body, packet, budget);
        self.small.push(Kind::Audio, |w| {
            w.bytes(&body);
        });
        self.stats.audio_sent += 1;
    }

    pub fn send_control(&mut self, msg: &Control) {
        match serde_json::to_vec(msg) {
            Ok(bytes) if bytes.len() <= control::MAX_MESSAGE => self.control_tx.push(&bytes),
            Ok(_) => tracing::warn!("control message too large; dropped"),
            Err(e) => tracing::warn!("control message didn't serialise: {e}"),
        }
    }

    /// Lowers or raises the bitrate ceiling (the client's `max_bitrate_kbps`).
    pub fn set_max_bitrate(&mut self, kbps: u32) {
        self.bwe.set_max(u64::from(kbps) * 1000);
    }

    /// Says goodbye: a Close chunk goes out a few times, then the core is done.
    pub fn close(&mut self, reason: &str) {
        if self.closing.is_none() && !self.closed {
            self.closing = Some((reason.to_owned(), 3));
            self.pacer.clear();
        }
    }

    // --- input ------------------------------------------------------------------------------

    pub fn handle_datagram(&mut self, now: Micros, from: SocketAddr, data: &mut [u8]) {
        if self.closed {
            return;
        }
        let Some(opened) = self.opener.open(data) else { return };
        self.last_rx = now;
        self.stats.packets_received += 1;
        self.path.on_packet(from, opened.newest);
        for c in Chunks::new(opened.plain) {
            let Ok((Some(kind), body)) = c else { continue };
            if let Err(e) = self.handle_chunk(kind, body, from, now) {
                tracing::debug!(?kind, "bad chunk from the client: {e}");
            }
        }
    }

    fn handle_chunk(&mut self, kind: Kind, body: &[u8], from: SocketAddr, now: Micros) -> Result<(), wire::Malformed> {
        match kind {
            Kind::Hello => {
                if self.path.current == Some(from) {
                    self.hello_ack_due = true;
                }
            }
            Kind::PathResponse => {
                let token = conn::get_u64(body)?;
                match self.path.on_response(from, token) {
                    Some(PathChange::First(addr)) => {
                        self.connected = true;
                        self.hello_ack_due = true;
                        self.mtu.restart(now, 200_000);
                        self.events.push_back(HostEvent::Connected(addr));
                    }
                    Some(PathChange::Migrated { from, to }) => {
                        self.stats.migrations += 1;
                        self.bwe.reset_path();
                        self.mtu.restart(now, 200_000);
                        self.events.push_back(HostEvent::Migrated { from, to });
                    }
                    None => {}
                }
            }
            Kind::Ping => {
                let (id, sent) = conn::get_ping(body)?;
                self.small.push(Kind::Pong, |w| conn::put_pong(w, id, sent, now, now));
            }
            Kind::Pong => {
                let (_, t1, t2, t3) = conn::get_pong(body)?;
                self.clock.on_pong(t1, t2, t3, now);
            }
            Kind::Input => {
                for s in self.input_rx.on_chunk(body)? {
                    self.stats.input_events += 1;
                    self.events.push_back(HostEvent::Input(s));
                }
            }
            Kind::Control => {
                let (seq, fin, data) = control::get_segment(body)?;
                for msg in self.control_rx.on_segment(seq, fin, data) {
                    match serde_json::from_slice::<Control>(&msg) {
                        Ok(c) => self.events.push_back(HostEvent::Control(c)),
                        Err(e) => tracing::debug!("unknown control message: {e}"),
                    }
                }
            }
            Kind::ControlAck => {
                let (next, sack) = control::get_ack(body)?;
                self.control_tx.on_ack(next, sack);
            }
            Kind::Feedback => {
                let fb = feedback::parse(body)?;
                self.bwe.on_feedback(&fb, now);
            }
            Kind::Nack => {
                let (seq, indices) = video::get_nack(body)?;
                self.stats.nacks_received += 1;
                self.answer_nack(seq, &indices, now);
            }
            Kind::MtuAck => {
                let size = wire::Reader::new(body).u16()?;
                self.mtu.on_ack(usize::from(size));
            }
            Kind::Audio => {
                for p in self.audio_rx.on_chunk(body)? {
                    self.events.push_back(HostEvent::Audio(p));
                }
            }
            Kind::Close => {
                let reason = String::from_utf8_lossy(body).into_owned();
                self.finish(reason);
            }
            _ => {}
        }
        Ok(())
    }

    fn answer_nack(&mut self, seq: u64, indices: &[u16], now: Micros) {
        if !self.opts.answer_nacks || self.closing.is_some() {
            return;
        }
        // A resend into a queue that is already long would arrive too late and add to the
        // congestion that probably caused the loss.
        if self.pacer.delay(now) as f64 > self.frame_interval {
            return;
        }
        let Some(frame) = self.video.find(seq) else { return };
        let wanted: Vec<u16> = if indices.is_empty() {
            (0..frame.k as u16).collect()
        } else {
            indices.iter().copied().filter(|i| usize::from(*i) < frame.total()).take(256).collect()
        };
        let datagram = frame.shard + VIDEO_OVERHEAD;
        let rate = self.pacing_rate(0);
        self.pacer.push_resends(&frame, &wanted, datagram, rate, now);
        self.stats.resends += wanted.len() as u64;
    }

    fn finish(&mut self, reason: String) {
        if !self.closed {
            self.closed = true;
            self.pacer.clear();
            self.events.push_back(HostEvent::Closed { reason });
        }
    }

    // --- output -----------------------------------------------------------------------------

    fn queue_periodic(&mut self, now: Micros) {
        if !self.connected {
            return;
        }
        if std::mem::take(&mut self.hello_ack_due) {
            self.small.push(Kind::HelloAck, |_| {});
        }
        if now >= self.next_ping {
            self.ping_id = self.ping_id.wrapping_add(1);
            let id = self.ping_id;
            self.small.push(Kind::Ping, |w| conn::put_ping(w, id, now));
            self.next_ping = now + PING_INTERVAL;
        }
        if now >= self.next_status {
            let s = self.status(now);
            self.small.push(Kind::Status, |w| conn::put_status(w, &s));
            self.next_status = now + STATUS_INTERVAL;
        }
        if self.input_rx.ack_due {
            let through = self.input_rx.acked_through();
            self.small.push(Kind::InputAck, |w| {
                w.u64(through);
            });
        }
        if self.control_rx.ack_due {
            let (next, sack) = self.control_rx.ack();
            self.small.push(Kind::ControlAck, |w| control::put_ack(w, next, sack));
        }
    }

    fn status(&self, now: Micros) -> Status {
        Status {
            budget_kbps: (self.bwe.target_bps() / 1000) as u32,
            encoder_kbps: self.encoder_target_kbps(now),
            fec_permille: (self.video.fec_ratio * 1000.0) as u16,
            loss_permille: (self.bwe.state().loss * 1000.0) as u16,
        }
    }

    fn control_due(&self, now: Micros) -> bool {
        self.control_tx.has_unsent() || self.control_tx.next_timeout(self.control_rto()).is_some_and(|t| t <= now)
    }

    fn control_rto(&self) -> Micros {
        self.clock.rto(10_000)
    }

    /// The next datagram to send, appended to `out`.
    pub fn poll_transmit(&mut self, now: Micros, out: &mut Vec<u8>) -> Option<Transmit> {
        if self.closed {
            return None;
        }
        if let Some((to, token)) = self.path.poll_challenge(now) {
            return Some(self.seal(out, now, to, Class::Interactive, None, |b| {
                wire::chunk(b, Kind::PathChallenge, |w| {
                    w.u64(token);
                })
            }));
        }
        let dest = self.path.current?;
        if let Some((reason, left)) = &mut self.closing {
            let reason = reason.clone();
            *left -= 1;
            if *left == 0 {
                self.finish("closed".into());
            }
            return Some(self.seal(out, now, dest, Class::Interactive, None, |b| {
                wire::chunk(b, Kind::Close, |w| {
                    w.bytes(reason.as_bytes());
                })
            }));
        }
        self.queue_periodic(now);
        if !self.small.is_empty() || self.control_due(now) {
            return Some(self.small_datagram(out, now, dest));
        }
        if let Some(size) = self.mtu.poll(now) {
            return Some(self.seal(out, now, dest, Class::Media, None, |b| {
                let target = b.len() + size - OVERHEAD;
                conn::put_padding(b, target, conn::PAD_MTU_PROBE)
            }));
        }
        if let Some(item) = self.pacer.poll(now) {
            self.stats.shards_sent += 1;
            return Some(self.seal(out, now, dest, Class::Media, None, |b| {
                let start = b.len();
                b.extend_from_slice(&[Kind::Video as u8, 0, 0]);
                item.frame.write_body(usize::from(item.index), b);
                let len = (b.len() - start - CHUNK_HEADER) as u16;
                b[start + 1..start + 3].copy_from_slice(&len.to_be_bytes());
            }));
        }
        let size = self.mtu.current;
        if let Some(id) = self.bwe.poll_probe(now, size) {
            return Some(self.seal(out, now, dest, Class::Media, Some(id), |b| {
                let target = b.len() + size - OVERHEAD;
                conn::put_padding(b, target, 0)
            }));
        }
        None
    }

    fn small_datagram(&mut self, out: &mut Vec<u8>, now: Micros, dest: SocketAddr) -> Transmit {
        let budget = conn::plain_budget(self.mtu.current);
        let rto = self.control_rto();
        let (small, control_tx) = (&mut self.small, &mut self.control_tx);
        let mut fill = |b: &mut Vec<u8>| {
            let start = b.len();
            small.pack(b, budget);
            while b.len() - start + CHUNK_HEADER + 5 + control::SEGMENT_BYTES <= budget {
                let Some((seq, fin, data)) = control_tx.poll(now, rto) else { break };
                wire::chunk(b, Kind::Control, |w| control::put_segment(w, seq, fin, data));
            }
        };
        let (pn, len) = self.sealer.seal(out, &mut fill);
        self.record_sent(pn, now, len, None);
        Transmit { dest, len, class: Class::Interactive }
    }

    fn seal(
        &mut self,
        out: &mut Vec<u8>,
        now: Micros,
        dest: SocketAddr,
        class: Class,
        probe: Option<u32>,
        body: impl FnOnce(&mut Vec<u8>),
    ) -> Transmit {
        let (pn, len) = self.sealer.seal(out, body);
        self.record_sent(pn, now, len, probe);
        Transmit { dest, len, class }
    }

    fn record_sent(&mut self, pn: u64, now: Micros, len: usize, probe: Option<u32>) {
        self.bwe.on_sent(pn, now, len + 28, probe);
        self.stats.packets_sent += 1;
        self.stats.bytes_sent += len as u64;
    }

    /// When the core next needs [`poll_transmit`](Self::poll_transmit) or
    /// [`handle_timeout`](Self::handle_timeout).
    pub fn poll_timeout(&self, now: Micros) -> Option<Micros> {
        if self.closed {
            return None;
        }
        let mut t = vec![self.path.next_timeout()];
        if self.path.current.is_some() && self.closing.is_some() {
            t.push(Some(now));
        }
        if self.connected {
            let soon = !self.small.is_empty() || self.hello_ack_due || self.input_rx.ack_due || self.control_rx.ack_due;
            t.extend([
                soon.then_some(now),
                self.control_tx.has_unsent().then_some(now),
                Some(self.next_ping),
                Some(self.next_status),
                self.control_tx.next_timeout(self.control_rto()),
                self.mtu.next_timeout(),
                self.pacer.next_at(),
                self.bwe.next_probe_at(),
                Some(self.last_rx + self.opts.idle_timeout_us),
            ]);
        }
        t.into_iter().flatten().min()
    }

    pub fn handle_timeout(&mut self, now: Micros) {
        if self.connected && !self.closed && now >= self.last_rx + self.opts.idle_timeout_us {
            self.finish("timeout".into());
        }
    }

    // --- outputs for the encoder and stats ------------------------------------------------------

    /// The bitrate the encoder should aim for, kbit/s: the estimator's budget less FEC,
    /// headers and audio, and less again while the pacer's queue is longer than two
    /// frames (the encoder outruns the link).
    pub fn encoder_target_kbps(&self, now: Micros) -> u32 {
        let total = self.bwe.target_bps() as f64;
        let header = VIDEO_OVERHEAD as f64 / self.mtu.current as f64;
        let mut enc = total * (1.0 - self.video.fec_ratio) * (1.0 - header) - self.audio_bps;
        let queue = self.pacer.delay(now) as f64;
        let allowed = 2.0 * self.frame_interval;
        if queue > allowed {
            enc *= (1.0 - (queue - allowed) / 100_000.0).max(0.5);
        }
        (enc.max(f64::from(self.opts.min_kbps) * 500.0) / 1000.0) as u32
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.clock.rtt()
    }

    pub fn stats(&self, now: Micros) -> HostNetStats {
        let b = self.bwe.state();
        HostNetStats {
            peer: self.path.current.map(|a| a.to_string()),
            rtt_us: self.clock.rtt(),
            min_rtt_us: self.clock.min_rtt(),
            encoder_kbps: self.encoder_target_kbps(now),
            budget_kbps: (b.target_bps / 1000) as u32,
            acked_kbps: b.acked_bps.map(|r| (r / 1000) as u32),
            loss: b.loss as f32,
            loss_baseline: b.loss_baseline as f32,
            fec_ratio: self.video.fec_ratio as f32,
            datagram_bytes: self.mtu.current,
            pacer_delay_us: self.pacer.delay(now),
            pacer_queued_bytes: self.pacer.queued_bytes(),
            input_duplicates: self.input_rx.duplicates,
            control_retransmits: self.control_tx.retransmits,
            rejected: self.opener.rejected,
            replayed: self.opener.replayed,
            overuses: b.overuses,
            probes: b.probes,
            ..self.stats.clone()
        }
    }
}
