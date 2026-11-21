//! The client's end of the transport, as a state machine with no I/O of its own (see
//! [`crate::host`] for the shape). [`crate::ClientSession`] drives it with a socket.

use std::collections::VecDeque;
use std::net::SocketAddr;

use quiver_stream_proto::{AudioPacket, ClientStats, Control, InputEvent, Micros};
use serde::Serialize;

use crate::audio::{AudioReceiver, AudioSender};
use crate::clock::{ClockSync, PING_INTERVAL};
use crate::conn::{self, Opener, Sealer, SmallChunks, Status, Transmit};
use crate::control::{self, ControlReceiver, ControlSender};
use crate::crypto::{Cipher, Direction, SessionKeys};
use crate::feedback::FeedbackBuilder;
use crate::input::{InputOpts, InputSender};
use crate::io::Class;
use crate::path::BASE_DATAGRAM;
use crate::video::{self, RxOpts, VideoEvent, VideoReceiver};
use crate::wire::{self, CHUNK_HEADER, Chunks, Kind};

#[derive(Debug, Clone)]
pub struct ClientOpts {
    pub video: RxOpts,
    /// NACK a frame's missing shards when the answer can come back within this long
    /// (1.5 × RTT plus the reorder slack). None never NACKs.
    pub nack_max_wait_us: Option<Micros>,
    pub input: InputOpts,
    pub audio_redundancy: usize,
    pub cipher: Cipher,
    pub dscp: bool,
    /// How long `connect` keeps saying hello before giving up.
    pub connect_timeout_us: Micros,
    /// The session ends after this long with nothing from the host.
    pub idle_timeout_us: Micros,
}

impl Default for ClientOpts {
    fn default() -> Self {
        Self {
            video: RxOpts::default(),
            nack_max_wait_us: Some(10_000),
            input: InputOpts::default(),
            audio_redundancy: 2,
            cipher: Cipher::Aes256Gcm,
            dscp: true,
            connect_timeout_us: 5_000_000,
            idle_timeout_us: 30_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Connected,
    Video(VideoEvent),
    Audio(AudioPacket),
    Control(Control),
    Closed { reason: String },
}

/// The client's transport counters, cumulative.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ClientNetStats {
    pub connected: bool,
    pub rtt_us: Option<u64>,
    pub min_rtt_us: Option<u64>,
    /// The host's clock minus ours, µs.
    pub clock_offset_us: Option<i64>,
    pub jitter_us: u32,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub bytes_received: u64,
    pub frames: u64,
    pub frames_lost: u64,
    pub shards: u64,
    pub shards_recovered: u64,
    pub shards_late: u64,
    pub shards_surplus: u64,
    pub shards_duplicate: u64,
    pub nacks: u64,
    pub nack_saves: u64,
    pub audio_received: u64,
    pub audio_recovered: u64,
    pub input_sent: u64,
    pub input_resends: u64,
    pub input_unacked: usize,
    pub control_retransmits: u64,
    pub rejected: u64,
    pub replayed: u64,
    /// The host's view: its send budget and encoder target, kbit/s; FEC share; loss.
    pub host_budget_kbps: u32,
    pub host_encoder_kbps: u32,
    pub host_fec_ratio: f32,
    pub host_loss: f32,
}

/// Counters at the last [`ClientCore::take_stats`], to report the difference.
#[derive(Debug, Clone, Default)]
struct Mark {
    frames: u64,
    frames_lost: u64,
    packets: u64,
    highest: u64,
    recovered: u64,
}

pub struct ClientCore {
    opts: ClientOpts,
    server: SocketAddr,
    sealer: Sealer,
    opener: Opener,
    connected: bool,
    started: Micros,
    next_hello: Micros,
    hello_gap: Micros,
    clock: ClockSync,
    next_ping: Micros,
    ping_id: u32,
    small: SmallChunks,
    control_tx: ControlSender,
    control_rx: ControlReceiver,
    input_tx: InputSender,
    audio_tx: AudioSender,
    audio_rx: AudioReceiver,
    video: VideoReceiver,
    feedback: FeedbackBuilder,
    events: VecDeque<ClientEvent>,
    last_rx: Micros,
    closing: Option<(String, u8)>,
    closed: bool,
    host: Status,
    jitter: f64,
    last_transit: Option<i64>,
    network_samples: Vec<u32>,
    bytes_received: u64,
    audio_received: u64,
    mark: Mark,
}

impl ClientCore {
    pub fn new(keys: &SessionKeys, server: SocketAddr, opts: ClientOpts, now: Micros) -> Self {
        Self {
            server,
            sealer: Sealer::new(keys, Direction::ClientToHost, opts.cipher),
            opener: Opener::new(keys, Direction::HostToClient, opts.cipher),
            connected: false,
            started: now,
            next_hello: now,
            hello_gap: 20_000,
            clock: ClockSync::default(),
            next_ping: now,
            ping_id: 0,
            small: SmallChunks::default(),
            control_tx: ControlSender::default(),
            control_rx: ControlReceiver::default(),
            input_tx: InputSender::new(opts.input.clone()),
            audio_tx: AudioSender::new(opts.audio_redundancy),
            audio_rx: AudioReceiver::default(),
            video: VideoReceiver::new(opts.video.clone()),
            feedback: FeedbackBuilder::default(),
            events: VecDeque::new(),
            last_rx: now,
            closing: None,
            closed: false,
            host: Status::default(),
            jitter: 0.0,
            last_transit: None,
            network_samples: Vec::new(),
            bytes_received: 0,
            audio_received: 0,
            mark: Mark::default(),
            opts,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    /// The host's clock minus ours, µs, once a pong has come back.
    pub fn clock_offset(&self) -> Option<i64> {
        self.clock.offset()
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.clock.rtt()
    }

    // --- commands ---------------------------------------------------------------------------

    /// Queues an input event captured at `at`; returns its sequence number.
    pub fn send_input(&mut self, event: InputEvent, at: Micros, now: Micros) -> u64 {
        self.input_tx.push(event, at, now)
    }

    /// Queues microphone audio.
    pub fn send_audio(&mut self, packet: AudioPacket) {
        let budget = conn::body_budget(conn::plain_budget(BASE_DATAGRAM)) - 64;
        let mut body = Vec::new();
        self.audio_tx.write_chunk(&mut body, packet, budget);
        self.small.push(Kind::Audio, |w| {
            w.bytes(&body);
        });
    }

    pub fn send_control(&mut self, msg: &Control) {
        match serde_json::to_vec(msg) {
            Ok(bytes) if bytes.len() <= control::MAX_MESSAGE => self.control_tx.push(&bytes),
            Ok(_) => tracing::warn!("control message too large; dropped"),
            Err(e) => tracing::warn!("control message didn't serialise: {e}"),
        }
    }

    pub fn close(&mut self, reason: &str) {
        if self.closing.is_none() && !self.closed {
            self.closing = Some((reason.to_owned(), 3));
        }
    }

    // --- input ------------------------------------------------------------------------------

    pub fn handle_datagram(&mut self, now: Micros, _from: SocketAddr, data: &mut [u8]) {
        if self.closed {
            return;
        }
        let size = data.len();
        let Some(opened) = self.opener.open(data) else { return };
        self.last_rx = now;
        self.bytes_received += size as u64;
        self.feedback.on_packet(opened.pn, now);
        for c in Chunks::new(opened.plain) {
            let Ok((Some(kind), body)) = c else { continue };
            if let Err(e) = self.handle_chunk(kind, body, size, now) {
                tracing::debug!(?kind, "bad chunk from the host: {e}");
            }
        }
        self.drain_video(now);
    }

    fn handle_chunk(&mut self, kind: Kind, body: &[u8], size: usize, now: Micros) -> Result<(), wire::Malformed> {
        match kind {
            Kind::HelloAck => {
                if !self.connected {
                    self.connected = true;
                    self.events.push_back(ClientEvent::Connected);
                }
            }
            Kind::PathChallenge => {
                let token = conn::get_u64(body)?;
                self.small.push(Kind::PathResponse, |w| {
                    w.u64(token);
                });
            }
            Kind::Ping => {
                let (id, sent) = conn::get_ping(body)?;
                self.small.push(Kind::Pong, |w| conn::put_pong(w, id, sent, now, now));
            }
            Kind::Pong => {
                let (_, t1, t2, t3) = conn::get_pong(body)?;
                self.clock.on_pong(t1, t2, t3, now);
            }
            Kind::Video => self.video.on_shard(body, now)?,
            Kind::Audio => {
                for p in self.audio_rx.on_chunk(body)? {
                    self.audio_received += 1;
                    self.events.push_back(ClientEvent::Audio(p));
                }
            }
            Kind::InputAck => self.input_tx.on_ack(conn::get_u64(body)?),
            Kind::Control => {
                let (seq, fin, data) = control::get_segment(body)?;
                for msg in self.control_rx.on_segment(seq, fin, data) {
                    match serde_json::from_slice::<Control>(&msg) {
                        Ok(c) => self.events.push_back(ClientEvent::Control(c)),
                        Err(e) => tracing::debug!("unknown control message: {e}"),
                    }
                }
            }
            Kind::ControlAck => {
                let (next, sack) = control::get_ack(body)?;
                self.control_tx.on_ack(next, sack);
            }
            Kind::Status => self.host = conn::get_status(body)?,
            Kind::Padding => {
                if body.first().is_some_and(|f| f & conn::PAD_MTU_PROBE != 0) {
                    self.small.push(Kind::MtuAck, |w| {
                        w.u16(size as u16);
                    });
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

    /// Moves finished frames and gaps to the event queue, noting their timing.
    fn drain_video(&mut self, _now: Micros) {
        for (encoded, done) in std::mem::take(&mut self.video.completions) {
            // RFC 3550 jitter of the frames' transit times; the clock offset cancels out.
            let transit = done as i64 - encoded as i64;
            if let Some(last) = self.last_transit.replace(transit) {
                self.jitter += ((transit - last).abs() as f64 - self.jitter) / 16.0;
            }
            if let Some(off) = self.clock.offset() {
                let network = done as i64 - (encoded as i64 - off);
                self.network_samples.push(network.clamp(0, u32::MAX as i64) as u32);
            }
        }
        while let Some(e) = self.video.poll_event() {
            self.events.push_back(ClientEvent::Video(e));
        }
    }

    fn finish(&mut self, reason: String) {
        if !self.closed {
            self.closed = true;
            self.events.push_back(ClientEvent::Closed { reason });
        }
    }

    // --- output -----------------------------------------------------------------------------

    fn nack_wait(&self) -> Option<Micros> {
        let max = self.opts.nack_max_wait_us?;
        let rtt = self.clock.rtt()?;
        let wait = rtt * 3 / 2 + self.opts.video.reorder_slack_us;
        (wait <= max).then_some(wait)
    }

    fn rto(&self) -> Micros {
        self.clock.rto(5_000)
    }

    fn queue_periodic(&mut self, now: Micros) {
        if !self.connected {
            if now >= self.next_hello {
                self.small.push(Kind::Hello, |w| {
                    w.u8(crate::crypto::WIRE_VERSION).u64(now);
                });
                self.next_hello = now + self.hello_gap;
                self.hello_gap = (self.hello_gap * 3 / 2).min(250_000);
            }
            return;
        }
        if now >= self.next_ping {
            self.ping_id = self.ping_id.wrapping_add(1);
            let id = self.ping_id;
            self.small.push(Kind::Ping, |w| conn::put_ping(w, id, now));
            self.next_ping = now + PING_INTERVAL;
        }
        while self.feedback.due(now) {
            let mut body = Vec::new();
            self.feedback.write_chunk(&mut body, now);
            self.small.push(Kind::Feedback, |w| {
                w.bytes(&body);
            });
        }
        for (seq, indices) in self.video.take_nacks() {
            self.small.push(Kind::Nack, |w| video::put_nack(w, seq, &indices));
        }
        if self.control_rx.ack_due {
            let (next, sack) = self.control_rx.ack();
            self.small.push(Kind::ControlAck, |w| control::put_ack(w, next, sack));
        }
        self.input_tx.refresh_pads(now);
    }

    pub fn poll_transmit(&mut self, now: Micros, out: &mut Vec<u8>) -> Option<Transmit> {
        if self.closed {
            return None;
        }
        let dest = self.server;
        if let Some((reason, left)) = &mut self.closing {
            let reason = reason.clone();
            *left -= 1;
            if *left == 0 {
                self.finish("closed".into());
            }
            let (_, len) = self.sealer.seal(out, |b| {
                wire::chunk(b, Kind::Close, |w| {
                    w.bytes(reason.as_bytes());
                })
            });
            return Some(Transmit { dest, len, class: Class::Interactive });
        }
        self.queue_periodic(now);
        let rto = self.rto();
        let input_due = self.connected && self.input_tx.wants_send(now, rto);
        let control_due = self.connected
            && (self.control_tx.has_unsent() || self.control_tx.next_timeout(self.clock.rto(10_000)).is_some_and(|t| t <= now));
        if self.small.is_empty() && !input_due && !control_due {
            return None;
        }
        let budget = conn::plain_budget(BASE_DATAGRAM);
        let connected = self.connected;
        let control_rto = self.clock.rto(10_000);
        let (small, input_tx, control_tx) = (&mut self.small, &mut self.input_tx, &mut self.control_tx);
        let (_, len) = self.sealer.seal(out, |b| {
            let start = b.len();
            if connected {
                // Input first: it rides in every datagram until acked.
                let head = b.len();
                b.extend_from_slice(&[Kind::Input as u8, 0, 0]);
                if input_tx.write_chunk(b, now, rto, conn::body_budget(budget / 2)) {
                    let len = (b.len() - head - CHUNK_HEADER) as u16;
                    b[head + 1..head + 3].copy_from_slice(&len.to_be_bytes());
                } else {
                    b.truncate(head);
                }
            }
            let used = b.len() - start;
            small.pack(b, budget - used);
            while connected && b.len() - start + CHUNK_HEADER + 5 + control::SEGMENT_BYTES <= budget {
                let Some((seq, fin, data)) = control_tx.poll(now, control_rto) else { break };
                wire::chunk(b, Kind::Control, |w| control::put_segment(w, seq, fin, data));
            }
        });
        Some(Transmit { dest, len, class: Class::Interactive })
    }

    pub fn poll_timeout(&self, now: Micros) -> Option<Micros> {
        if self.closed {
            return None;
        }
        if self.closing.is_some() || !self.small.is_empty() {
            return Some(now);
        }
        if !self.connected {
            return Some(self.next_hello.min(self.started + self.opts.connect_timeout_us));
        }
        let rto = self.rto();
        [
            Some(self.next_ping),
            self.feedback.next_timeout(),
            self.input_tx.next_timeout(rto),
            self.input_tx.wants_send(now, rto).then_some(now),
            self.control_tx.has_unsent().then_some(now),
            self.control_tx.next_timeout(self.clock.rto(10_000)),
            self.control_rx.ack_due.then_some(now),
            self.video.next_timeout(self.nack_wait()),
            Some(self.last_rx + self.opts.idle_timeout_us),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn handle_timeout(&mut self, now: Micros) {
        if self.closed {
            return;
        }
        if !self.connected && now >= self.started + self.opts.connect_timeout_us {
            self.finish("connect timed out".into());
            return;
        }
        if self.connected && now >= self.last_rx + self.opts.idle_timeout_us {
            self.finish("timeout".into());
            return;
        }
        self.video.on_timeout(now, self.nack_wait());
        self.drain_video(now);
    }

    // --- stats --------------------------------------------------------------------------------

    pub fn net_stats(&self) -> ClientNetStats {
        let v = &self.video.stats;
        let received = self.feedback.received();
        ClientNetStats {
            connected: self.connected,
            rtt_us: self.clock.rtt(),
            min_rtt_us: self.clock.min_rtt(),
            clock_offset_us: self.clock.offset(),
            jitter_us: self.jitter as u32,
            packets_received: received,
            packets_lost: self.feedback.highest().saturating_sub(received),
            bytes_received: self.bytes_received,
            frames: v.frames,
            frames_lost: v.frames_lost,
            shards: v.shards,
            shards_recovered: v.recovered,
            shards_late: v.late,
            shards_surplus: v.surplus,
            shards_duplicate: v.duplicates,
            nacks: v.nacks,
            nack_saves: v.nack_saves,
            audio_received: self.audio_received,
            audio_recovered: self.audio_rx.recovered,
            input_sent: self.input_tx.sent_events,
            input_resends: self.input_tx.resends,
            input_unacked: self.input_tx.unacked(),
            control_retransmits: self.control_tx.retransmits,
            rejected: self.opener.rejected,
            replayed: self.opener.replayed,
            host_budget_kbps: self.host.budget_kbps,
            host_encoder_kbps: self.host.encoder_kbps,
            host_fec_ratio: f32::from(self.host.fec_permille) / 1000.0,
            host_loss: f32::from(self.host.loss_permille) / 1000.0,
        }
    }

    /// The transport's part of a [`ClientStats`] for the period since the last call:
    /// frame, packet and FEC counts, RTT, jitter, the median network time, and the host's
    /// estimate. The client fills in what only it knows (`frames_dropped`, `decode_us`,
    /// `present_us`, `capture_to_present_us`).
    pub fn take_stats(&mut self) -> ClientStats {
        let v = self.video.stats.clone();
        let received = self.feedback.received();
        let highest = self.feedback.highest();
        let m = std::mem::replace(
            &mut self.mark,
            Mark { frames: v.frames, frames_lost: v.frames_lost, packets: received, highest, recovered: v.recovered },
        );
        let packets = received - m.packets;
        let expected = highest - m.highest;
        let mut samples = std::mem::take(&mut self.network_samples);
        ClientStats {
            frames_received: (v.frames - m.frames) as u32,
            frames_lost: (v.frames_lost - m.frames_lost) as u32,
            frames_dropped: 0,
            packets_received: packets as u32,
            packets_lost: expected.saturating_sub(packets) as u32,
            packets_recovered: (v.recovered - m.recovered) as u32,
            rtt_us: self.clock.rtt().unwrap_or(0) as u32,
            jitter_us: self.jitter as u32,
            network_us: median(&mut samples),
            decode_us: 0,
            present_us: 0,
            capture_to_present_us: 0,
            estimated_kbps: self.host.encoder_kbps,
        }
    }
}

fn median(v: &mut [u32]) -> u32 {
    if v.is_empty() {
        return 0;
    }
    let mid = v.len() / 2;
    *v.select_nth_unstable(mid).1
}
