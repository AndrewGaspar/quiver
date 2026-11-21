//! One launch and its stream: the RTSP handshake that sets it up, and the session that runs
//! it (video and audio out, control and input in).

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use quiver_stream_proto::{Chroma, Control, InputEvent, PadKind, now_micros};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};

use crate::audio::{AudioPacketizer, OpusLayout};
use crate::control::{self, ClientMessage, ControlCipher, to_client};
use crate::crypto::Key;
use crate::enet::{self, EnetEvent};
use crate::host::{App, AudioParams, ColorSpace, HostEnds, PairedClient, StreamRequest, TriggerRumble, VideoParams};
use crate::input::{InputDecoder, MOTION_ACCEL, MOTION_GYRO, Notice};
use crate::rtsp;
use crate::sdp::{Announce, Describe, ENC_AUDIO, ENC_CONTROL_V2, ENC_VIDEO, FF_PEN_TOUCH};
use crate::video::{FrameType, VideoPacketizer};
use crate::{AvEncryption, ServerEvent, Shared, is_lan};

pub(crate) struct LaunchParams {
    pub app: App,
    pub app_number: u32,
    pub client: PairedClient,
    pub client_ip: IpAddr,
    /// The server address the client reached: the stream's UDP ports bind there.
    pub local_ip: IpAddr,
    pub key: Key,
    pub key_id: u32,
    pub mode: (u32, u32, u32),
    pub pad_mask: u32,
    pub encrypted_rtsp: bool,
}

/// A `/launch` (or `/resume`): the app, the client, the keys, and the stream once RTSP has
/// set it up.
pub(crate) struct Launch {
    pub app: App,
    pub app_number: u32,
    pub client: PairedClient,
    pub client_ip: IpAddr,
    local_ip: IpAddr,
    key: Key,
    key_id: u32,
    pad_mask: u32,
    encrypted_rtsp: bool,
    /// Echoed in the client's UDP pings, so the server knows where to send media.
    ping_payload: [u8; 16],
    /// Echoed in the client's ENet CONNECT.
    connect_data: u32,
    session_id: String,
    rtsp_sent: AtomicU32,
    rtsp_received: Mutex<u32>,
    state: Mutex<LaunchState>,
}

#[derive(Default)]
struct LaunchState {
    sockets: Option<Sockets>,
    announce: Option<Announce>,
    stream: Option<watch::Sender<Option<String>>>,
}

struct Sockets {
    video: Arc<UdpSocket>,
    audio: Arc<UdpSocket>,
    control: std::net::UdpSocket,
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).map_err(|_| anyhow!("no randomness"))?;
    Ok(b)
}

impl Launch {
    pub fn new(p: LaunchParams) -> Result<Self> {
        let ping: [u8; 8] = random()?;
        let ping_payload: [u8; 16] = hex::encode_upper(ping).into_bytes().try_into().expect("16 hex digits");
        let connect_data = u32::from_le_bytes(random()?);
        let session_id = hex::encode_upper(random::<6>()?);
        tracing::debug!(mode = ?p.mode, "launch");
        Ok(Self {
            app: p.app,
            app_number: p.app_number,
            client: p.client,
            client_ip: p.client_ip,
            local_ip: p.local_ip,
            key: p.key,
            key_id: p.key_id,
            pad_mask: p.pad_mask,
            encrypted_rtsp: p.encrypted_rtsp,
            ping_payload,
            connect_data,
            session_id,
            rtsp_sent: AtomicU32::new(0),
            rtsp_received: Mutex::new(0),
            state: Mutex::new(LaunchState::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LaunchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_streaming(&self) -> bool {
        self.state().stream.is_some()
    }

    /// Stops the stream, if one runs, and releases its ports.
    pub fn end(&self, reason: &str) {
        let mut s = self.state();
        if let Some(stop) = s.stream.take() {
            let _ = stop.send(Some(reason.to_owned()));
        }
        s.sockets = None;
        s.announce = None;
    }

    fn ensure_sockets(&self, shared: &Shared) -> Result<(u16, u16, u16)> {
        let mut s = self.state();
        if s.sockets.is_none() {
            let bind = |port| -> Result<std::net::UdpSocket> {
                let at = SocketAddr::new(self.local_ip, port);
                let sock = std::net::UdpSocket::bind(at).with_context(|| format!("bind UDP {at}"))?;
                sock.set_nonblocking(true)?;
                Ok(sock)
            };
            let p = shared.config.ports;
            let video = UdpSocket::from_std(bind(p.video)?)?;
            let audio = UdpSocket::from_std(bind(p.audio)?)?;
            let control = bind(p.control)?;
            set_send_buffer(&video, 4 << 20);
            dont_fragment(&video, self.local_ip);
            dont_fragment(&audio, self.local_ip);
            s.sockets = Some(Sockets { video: Arc::new(video), audio: Arc::new(audio), control });
        }
        let p = shared.config.ports;
        Ok((p.video, p.audio, p.control))
    }
}

/// Media datagrams never fragment: a send larger than the path MTU fails instead (and the
/// kernel learns a smaller path MTU from ICMP). Packets are sized to fit
/// ([`fit_packet_size`]).
fn dont_fragment(sock: &UdpSocket, local: IpAddr) {
    use std::os::fd::AsRawFd;
    let (level, name, value) = if local.is_ipv4() {
        (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, libc::IP_PMTUDISC_DO)
    } else {
        (libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER, libc::IPV6_PMTUDISC_DO)
    };
    // SAFETY: a valid socket fd and a pointer to an int that outlives the call.
    unsafe {
        libc::setsockopt(sock.as_raw_fd(), level, name, (&value as *const i32).cast(), std::mem::size_of::<i32>() as u32);
    }
}

fn set_send_buffer(sock: &UdpSocket, bytes: i32) {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid socket fd and a pointer to an int that outlives the call.
    unsafe {
        libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&bytes as *const i32).cast(), std::mem::size_of::<i32>() as u32);
    }
}

// --- RTSP ---------------------------------------------------------------------------------

/// Serves one RTSP connection: one request, one answer.
pub(crate) async fn serve_rtsp(shared: &Arc<Shared>, mut tcp: TcpStream, peer: SocketAddr) -> Result<()> {
    let Some(launch) = shared.current_launch() else {
        tracing::debug!(%peer, "RTSP with no launch");
        return Ok(());
    };
    if peer.ip().to_canonical() != launch.client_ip.to_canonical() {
        bail!("RTSP from {peer}, not from the client that launched");
    }
    let _ = tcp.set_nodelay(true);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let req = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = tcp.read(&mut chunk).await?;
            if n == 0 {
                bail!("connection closed mid-request");
            }
            buf.extend_from_slice(&chunk[..n]);
            if launch.encrypted_rtsp {
                if let Some((plain, seq, _)) = rtsp::open(&launch.key, &buf)? {
                    let mut last = launch.rtsp_received.lock().unwrap_or_else(|e| e.into_inner());
                    if seq <= *last {
                        bail!("replayed RTSP message {seq}");
                    }
                    *last = seq;
                    let (req, _) = rtsp::parse_request(&plain)?.ok_or_else(|| anyhow!("truncated encrypted RTSP request"))?;
                    return Ok(req);
                }
            } else if let Some((req, _)) = rtsp::parse_request(&buf)? {
                return Ok(req);
            }
            if buf.len() > rtsp::MAX_MESSAGE + 64 {
                bail!("RTSP request too long");
            }
        }
    })
    .await
    .map_err(|_| anyhow!("RTSP request timed out"))??;

    tracing::debug!(method = %req.method, target = %req.target, "RTSP");
    let cseq = req.cseq();
    if req.method == "SETUP" {
        // A stream that was just ended may still hold the ports for a moment.
        for _ in 0..30 {
            match launch.ensure_sockets(shared) {
                Err(e) if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => break,
            }
        }
    }
    let (status, reason, headers, body) = match handle(shared, &launch, &req) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("RTSP {}: {e:#}", req.method);
            (400, "BAD REQUEST", Vec::new(), String::new())
        }
    };
    let plain = rtsp::response(status, reason, cseq, &headers, body.as_bytes());
    let out = if launch.encrypted_rtsp { rtsp::seal(&launch.key, launch.rtsp_sent.fetch_add(1, Ordering::SeqCst) + 1, &plain) } else { plain };
    tcp.write_all(&out).await?;
    let _ = tcp.shutdown().await;
    Ok(())
}

type RtspAnswer = (u16, &'static str, Vec<(&'static str, String)>, String);

fn handle(shared: &Arc<Shared>, launch: &Arc<Launch>, req: &rtsp::Request) -> Result<RtspAnswer> {
    let ok = |headers, body| Ok((200, "OK", headers, body));
    match req.method.as_str() {
        "OPTIONS" => ok(Vec::new(), String::new()),
        "DESCRIBE" => {
            let c = &shared.config;
            let want_av = match c.av_encryption {
                AvEncryption::Never => false,
                AvEncryption::Always => true,
                AvEncryption::OutsideLan => !is_lan(launch.client_ip),
            };
            let d = Describe {
                hevc: c.codecs.hevc,
                av1: c.codecs.av1,
                reference_frame_invalidation: c.reference_frame_invalidation,
                feature_flags: if c.native_touch { FF_PEN_TOUCH } else { 0 },
                encryption_supported: ENC_CONTROL_V2 | ENC_VIDEO | ENC_AUDIO,
                encryption_requested: ENC_CONTROL_V2 | if want_av { ENC_VIDEO | ENC_AUDIO } else { 0 },
            };
            ok(Vec::new(), d.render())
        }
        "SETUP" => {
            let (video, audio, control) = launch.ensure_sockets(shared)?;
            let session = ("Session", format!("{};timeout = 90", launch.session_id));
            let ping = String::from_utf8(launch.ping_payload.to_vec()).expect("hex");
            match req.stream_kind() {
                Some("audio") => ok(vec![session, ("Transport", format!("server_port={audio}")), ("X-SS-Ping-Payload", ping)], String::new()),
                Some("video") => ok(vec![session, ("Transport", format!("server_port={video}")), ("X-SS-Ping-Payload", ping)], String::new()),
                Some("control") => ok(
                    vec![session, ("Transport", format!("server_port={control}")), ("X-SS-Connect-Data", launch.connect_data.to_string())],
                    String::new(),
                ),
                other => Ok((404, "NOT FOUND", Vec::new(), format!("no stream {other:?}"))),
            }
        }
        "ANNOUNCE" => {
            let a = Announce::parse(std::str::from_utf8(&req.body).context("ANNOUNCE body")?)?;
            let codecs = shared.config.codecs;
            let supported = match a.codec {
                quiver_stream_proto::VideoCodec::H264 => codecs.h264,
                quiver_stream_proto::VideoCodec::Hevc => if a.hdr { codecs.hevc_main10 } else { codecs.hevc },
                quiver_stream_proto::VideoCodec::Av1 => if a.hdr { codecs.av1_main10 } else { codecs.av1 },
            };
            if !supported || a.encryption & ENC_CONTROL_V2 == 0 {
                bail!("the client asked for {:?} (HDR {}) with encryption {:#x}, which this server can't do", a.codec, a.hdr, a.encryption);
            }
            tracing::info!(?a, "Moonlight ANNOUNCE");
            launch.state().announce = Some(a);
            ok(Vec::new(), String::new())
        }
        "PLAY" => {
            start_stream(shared, launch)?;
            ok(Vec::new(), String::new())
        }
        _ => Ok((404, "NOT FOUND", Vec::new(), String::new())),
    }
}

fn start_stream(shared: &Arc<Shared>, launch: &Arc<Launch>) -> Result<()> {
    let mut st = launch.state();
    if st.stream.is_some() {
        return Ok(());
    }
    let announce = st.announce.clone().ok_or_else(|| anyhow!("PLAY before ANNOUNCE"))?;
    let sockets = st.sockets.take().ok_or_else(|| anyhow!("PLAY before SETUP"))?;
    let c = &shared.config;
    let pads: Vec<PadKind> = (0..16).filter(|i| launch.pad_mask & (1 << i) != 0).map(|_| PadKind::Xbox).collect();
    let (caps, prefs) = announce.caps_and_prefs(&pads, c.fec_percent);
    let video = VideoParams {
        codec: announce.codec,
        width: announce.width,
        height: announce.height,
        fps: announce.fps,
        bitrate_kbps: announce.video_kbps(c.fec_percent),
        bit_depth: if announce.hdr { 10 } else { 8 },
        chroma: if announce.chroma444 { Chroma::Yuv444 } else { Chroma::Yuv420 },
        hdr: announce.hdr,
        color_space: if announce.hdr { ColorSpace::Rec2020 } else { announce.color_space },
        full_range: announce.full_range,
        slices_per_frame: announce.slices_per_frame,
        reference_frame_invalidation: c.reference_frame_invalidation && announce.max_ref_frames == 0,
        intra_refresh: announce.intra_refresh,
    };
    let audio = AudioParams {
        opus: OpusLayout::for_channels(announce.audio_channels, announce.audio_high_quality),
        packet_ms: announce.audio_packet_ms,
        constant_bitrate: true,
    };
    let req = StreamRequest {
        app: launch.app.clone(),
        client_name: launch.client.name.clone(),
        client_fingerprint: launch.client.fingerprint.clone(),
        caps,
        prefs,
        video: video.clone(),
        audio: audio.clone(),
    };

    let (video_tx, video_rx) = mpsc::channel(8);
    let (audio_tx, audio_rx) = mpsc::channel(64);
    let (input_tx, input_rx) = mpsc::channel(512);
    let (control_in_tx, control_in_rx) = mpsc::channel(64);
    let (control_out_tx, control_out_rx) = mpsc::channel(64);
    let (trigger_tx, trigger_rx) = mpsc::channel(16);
    let (bitrate_tx, bitrate_rx) = watch::channel(video.bitrate_kbps);
    let ends = HostEnds {
        video: video_tx,
        audio: audio_tx,
        input: input_rx,
        control_in: control_in_rx,
        control_out: control_out_tx,
        trigger_rumble: trigger_tx,
        bitrate_kbps: bitrate_rx,
    };
    shared.host.start(req, ends).context("the host couldn't start the stream")?;

    let (stop_tx, stop_rx) = watch::channel(None);
    st.stream = Some(stop_tx);
    drop(st);
    shared.emit(ServerEvent::StreamStarted { app: launch.app.title.clone(), client: launch.client.name.clone() });
    let session = Session {
        shared: shared.clone(),
        launch: launch.clone(),
        announce,
        video,
        audio,
        sockets,
        video_rx,
        audio_rx,
        input_tx,
        control_in_tx,
        control_out_rx,
        trigger_rx,
        _bitrate_tx: bitrate_tx,
        stop: stop_rx,
    };
    tokio::spawn(session.run());
    Ok(())
}

// --- the session --------------------------------------------------------------------------

struct Session {
    shared: Arc<Shared>,
    launch: Arc<Launch>,
    announce: Announce,
    video: VideoParams,
    audio: AudioParams,
    sockets: Sockets,
    video_rx: mpsc::Receiver<quiver_stream_proto::EncodedFrame>,
    audio_rx: mpsc::Receiver<quiver_stream_proto::AudioPacket>,
    input_tx: mpsc::Sender<InputEvent>,
    control_in_tx: mpsc::Sender<Control>,
    control_out_rx: mpsc::Receiver<Control>,
    trigger_rx: mpsc::Receiver<TriggerRumble>,
    _bitrate_tx: watch::Sender<u32>,
    stop: watch::Receiver<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Media {
    Video,
    Audio,
}

/// Learns where the client wants media from its pings: 16 bytes of the session's ping
/// payload (then a sequence number), from the launching client's address.
async fn ping_listener(sock: Arc<UdpSocket>, payload: [u8; 16], client_ip: IpAddr, kind: Media, tx: mpsc::UnboundedSender<(Media, SocketAddr)>) {
    let mut buf = [0u8; 256];
    let mut last = None;
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
        if from.ip().to_canonical() != client_ip.to_canonical() {
            continue;
        }
        let ok = (n >= 16 && buf[..16] == payload) || &buf[..n] == b"PING";
        if ok && last != Some(from) {
            last = Some(from);
            if tx.send((kind, from)).is_err() {
                return;
            }
        }
    }
}

impl Session {
    async fn run(mut self) {
        let l = self.launch.clone();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let control = match self.sockets.control.try_clone() {
            Ok(s) => s,
            Err(e) => return self.finish(None, &format!("control socket: {e}")).await,
        };
        let enet = match enet::spawn(control, l.connect_data, l.client_ip, ev_tx) {
            Ok(h) => h,
            Err(e) => return self.finish(None, &format!("control stream: {e:#}")).await,
        };
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let pings = [
            tokio::spawn(ping_listener(self.sockets.video.clone(), l.ping_payload, l.client_ip, Media::Video, peer_tx.clone())),
            tokio::spawn(ping_listener(self.sockets.audio.clone(), l.ping_payload, l.client_ip, Media::Audio, peer_tx)),
        ];

        let c = &self.shared.config;
        let mut cipher = ControlCipher::new(l.key);
        let video_key = (self.announce.encryption & ENC_VIDEO != 0).then_some(l.key);
        let audio_key = (self.announce.encryption & ENC_AUDIO != 0).then_some((l.key, l.key_id));
        // Made when the client's video address is known, to size packets for its path.
        let mut vp: Option<VideoPacketizer> = None;
        let mut ap = AudioPacketizer::new(u32::from(self.audio.packet_ms), audio_key);
        let mut decoder = InputDecoder::new();
        let mut video_peer: Option<SocketAddr> = None;
        let mut audio_peer: Option<SocketAddr> = None;
        // Frame index → the host's sequence number, for translating loss reports.
        let mut sent_frames: VecDeque<(u32, u64)> = VecDeque::with_capacity(1024);
        let mut invalidated_through: Option<u64> = None;
        let mut first_capture: Option<u64> = None;
        let mut sent_keyframe = false;
        let mut connected = false;
        let mut connected_at: Option<std::time::Instant> = None;
        let loss = u32::from(c.test_video_loss_percent);
        let mut rng: u32 = 0x9E37_79B9 ^ l.connect_data;
        let mut dropped = 0u64;
        let (mut sent_packets, mut send_errors, mut largest) = (0u64, 0u64, 0usize);
        let (mut idr_requests, mut rfi_requests) = (0u32, 0u32);
        let connect_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        tracing::info!(
            codec = ?self.video.codec, w = self.video.width, h = self.video.height, fps = self.video.fps, kbps = self.video.bitrate_kbps,
            hdr = self.video.hdr, audio = ?self.audio.opus, video_encrypted = video_key.is_some(), audio_encrypted = audio_key.is_some(),
            "Moonlight stream starting"
        );

        let send = |cipher: &mut ControlCipher, ptype: u16, payload: &[u8]| enet.send(0, cipher.seal(ptype, payload));

        let reason: String = loop {
            tokio::select! {
                _ = self.stop.changed() => break self.stop.borrow().clone().unwrap_or_else(|| "stopped".into()),
                ev = ev_rx.recv() => match ev {
                    Some(EnetEvent::Connected(addr)) => {
                        tracing::info!(%addr, "control stream connected");
                        connected = true;
                        connected_at = Some(std::time::Instant::now());
                        if self.video.hdr {
                            send(&mut cipher, to_client::HDR_MODE, &control::hdr_mode(true, None));
                        }
                    }
                    Some(EnetEvent::Received { channel, data }) => {
                        let (ptype, payload) = match cipher.open(&data) {
                            Ok(m) => m,
                            Err(e) => { tracing::debug!(channel, "control message dropped: {e:#}"); continue; }
                        };
                        match ClientMessage::parse(ptype, &payload) {
                            ClientMessage::RequestIdr => { idr_requests += 1; let _ = self.control_in_tx.try_send(Control::Keyframe); }
                            ClientMessage::InvalidateRefFrames { first, last } => {
                                let seq_of = |f: u32| sent_frames.iter().find(|(i, _)| *i == f).map(|(_, s)| *s);
                                let since = seq_of(first).or_else(|| sent_frames.front().map(|(_, s)| *s));
                                let through = seq_of(last).or_else(|| sent_frames.back().map(|(_, s)| *s));
                                match (since, through) {
                                    (Some(since_seq), Some(through_seq)) if self.video.reference_frame_invalidation => {
                                        invalidated_through = Some(through_seq);
                                        rfi_requests += 1;
                                        let _ = self.control_in_tx.try_send(Control::LostFrames { since_seq, through_seq });
                                    }
                                    _ => { let _ = self.control_in_tx.try_send(Control::Keyframe); }
                                }
                            }
                            ClientMessage::Input(p) => {
                                let mut notices = Vec::new();
                                match decoder.decode(&p, &mut notices) {
                                    Ok(events) => for e in events {
                                        if self.input_tx.try_send(e).is_err() { tracing::debug!("input queue full"); }
                                    },
                                    Err(e) => tracing::debug!("input packet: {e:#}"),
                                }
                                for Notice::EnableMotion { slot, accel, gyro } in notices {
                                    if accel { send(&mut cipher, to_client::SET_MOTION_EVENT, &control::set_motion_event(slot.into(), 100, MOTION_ACCEL)); }
                                    if gyro { send(&mut cipher, to_client::SET_MOTION_EVENT, &control::set_motion_event(slot.into(), 100, MOTION_GYRO)); }
                                }
                            }
                            ClientMessage::Ignored(_) => {}
                            ClientMessage::Unknown(t) => tracing::debug!(ptype = format_args!("{t:#06x}"), "unknown control message"),
                        }
                    }
                    Some(EnetEvent::Disconnected) | None => break "the client disconnected".into(),
                },
                Some((kind, addr)) = peer_rx.recv() => {
                    tracing::info!(?kind, %addr, "client media address");
                    match kind {
                        Media::Video => {
                            if video_peer.is_none() { let _ = self.control_in_tx.try_send(Control::Keyframe); }
                            video_peer = Some(addr);
                            if vp.is_none() {
                                let fit = fit_packet_size(self.announce.packet_size, video_key.is_some(), path_mtu(l.local_ip, addr));
                                vp = Some(match fit {
                                    Some(size) => {
                                        tracing::warn!(
                                            asked = self.announce.packet_size, size,
                                            "the client's video packets don't fit the path MTU: sending smaller ones without FEC (set Moonlight's packet size to {} or less)", size + if video_key.is_some() { 32 } else { 0 }
                                        );
                                        VideoPacketizer::new(size, 0, 0, video_key)
                                    }
                                    None => VideoPacketizer::new(self.announce.packet_size, c.fec_percent, self.announce.min_fec_packets, video_key),
                                });
                            }
                        }
                        Media::Audio => audio_peer = Some(addr),
                    }
                }
                frame = self.video_rx.recv() => {
                    let Some(f) = frame else { break "the host ended the stream".into() };
                    let (Some(to), Some(vp)) = (video_peer, vp.as_mut()) else { continue };
                    if !sent_keyframe && !f.keyframe { continue; }
                    let ftype = if f.keyframe {
                        if !sent_keyframe {
                            let ms = connected_at.map(|t| t.elapsed().as_millis());
                            tracing::info!(after_connect_ms = ms.unwrap_or(0) as u64, seq = f.seq, "first video frame sent");
                        }
                        sent_keyframe = true;
                        invalidated_through = None;
                        FrameType::Idr
                    } else if invalidated_through.is_some_and(|t| f.refs_from_seq > t) {
                        invalidated_through = None;
                        FrameType::AfterInvalidation
                    } else {
                        FrameType::P
                    };
                    let base = *first_capture.get_or_insert(f.captured);
                    let rtp_ts = (f.captured.saturating_sub(base) * 9 / 100) as u32;
                    let latency = (now_micros().saturating_sub(f.captured) / 100).min(u64::from(u16::MAX)) as u16;
                    if sent_frames.len() == sent_frames.capacity() { sent_frames.pop_front(); }
                    sent_frames.push_back((vp.next_frame_index(), f.seq));
                    match vp.packetize(&f.data, ftype, latency, rtp_ts) {
                        Ok(packets) => for p in packets {
                            if loss > 0 {
                                rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
                                if rng % 100 < loss { dropped += 1; continue; }
                            }
                            if let Err(e) = self.sockets.video.send_to(&p, to).await {
                                send_errors += 1;
                                if send_errors <= 5 { tracing::warn!(len = p.len(), "video send: {e}"); }
                                break;
                            }
                            sent_packets += 1;
                            largest = largest.max(p.len());
                        },
                        Err(e) => tracing::warn!("frame {} not sent: {e:#}", f.seq),
                    }
                }
                Some(a) = self.audio_rx.recv() => {
                    if a.mic { continue; }
                    let Some(to) = audio_peer else { continue };
                    for p in ap.push(&a.data) {
                        let _ = self.sockets.audio.send_to(&p, to).await;
                    }
                }
                Some(ctl) = self.control_out_rx.recv() => match ctl {
                    Control::Rumble { slot, low, high, .. } if connected => send(&mut cipher, to_client::RUMBLE, &control::rumble(slot.into(), low, high)),
                    Control::Configure(cfg) if connected && self.video.hdr => {
                        send(&mut cipher, to_client::HDR_MODE, &control::hdr_mode(cfg.color.hdr.is_some(), cfg.color.hdr.as_ref()));
                    }
                    Control::Bye { reason } => break format!("the host ended the stream: {reason}"),
                    _ => {}
                },
                Some(t) = self.trigger_rx.recv() => if connected {
                    send(&mut cipher, to_client::RUMBLE_TRIGGERS, &control::rumble_triggers(t.slot.into(), t.left, t.right));
                },
                _ = tokio::time::sleep_until(connect_deadline), if !connected => break "the client never connected the control stream".into(),
            }
        };

        tracing::info!(sent_packets, largest, send_errors, dropped_for_test = dropped, idr_requests, rfi_requests, "video totals");
        tracing::info!(fec_blocks = ap.fec_blocks, uneven_blocks = ap.uneven_blocks, "audio totals");
        if connected {
            send(&mut cipher, to_client::TERMINATION, &control::termination(control::TERMINATION_GRACEFUL));
            enet.disconnect();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for p in pings {
            p.abort();
        }
        self.finish(Some(enet), &reason).await;
    }

    async fn finish(self, enet: Option<enet::EnetHandle>, reason: &str) {
        let _ = self.input_tx.try_send(InputEvent::ReleaseAll);
        let _ = self.control_in_tx.try_send(Control::Bye { reason: reason.to_owned() });
        if let Some(e) = enet {
            // Joining the thread blocks briefly.
            let _ = tokio::task::spawn_blocking(move || drop(e)).await;
        }
        {
            let mut st = self.launch.state();
            st.stream = None;
            st.announce = None;
        }
        tracing::info!(reason, "Moonlight stream ended");
        self.shared.emit(ServerEvent::StreamEnded { app: self.launch.app.title.clone(), reason: reason.to_owned() });
    }
}

/// The MTU of the route from `local` to `peer`, from a connected UDP socket (Linux's
/// `IP_MTU`/`IPV6_MTU`). None when it can't be found.
pub(crate) fn path_mtu(local: IpAddr, peer: SocketAddr) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let sock = std::net::UdpSocket::bind(SocketAddr::new(local, 0)).ok()?;
    sock.connect(peer).ok()?;
    let (level, name) = if peer.is_ipv4() { (libc::IPPROTO_IP, libc::IP_MTU) } else { (libc::IPPROTO_IPV6, libc::IPV6_MTU) };
    let mut mtu: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: a valid socket fd, and an int and its length that outlive the call.
    let r = unsafe { libc::getsockopt(sock.as_raw_fd(), level, name, (&mut mtu as *mut libc::c_int).cast(), &mut len) };
    (r == 0 && mtu > 0).then_some(mtu as u32)
}

/// A video datagram is the client's packet size plus the 16-byte RTP header, plus 32 bytes
/// when encrypted (the client already took those 32 off its packet size). If that doesn't
/// fit the path MTU less the IP and UDP headers, the largest packet size that does (a
/// multiple of 16). None when the client's size fits, or the MTU is unknown.
///
/// Moonlight decodes smaller packets fine, but its FEC recovery assumes its own size, so a
/// stream sent smaller goes without FEC; fragmenting instead would lose whole frames to the
/// loss of one fragment, and a tunnel like Tailscale (MTU 1280) may not carry fragments well.
pub(crate) fn fit_packet_size(packet_size: usize, encrypted: bool, mtu: Option<u32>) -> Option<usize> {
    let mtu = mtu? as usize;
    // IPv6's header is the larger; assume it so one rule covers both.
    let room = mtu.checked_sub(40 + 8)?;
    let overhead = 16 + if encrypted { 32 } else { 0 };
    if packet_size + overhead <= room {
        return None;
    }
    let size = (room - overhead) / 16 * 16;
    Some(size.max(256))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_size_fits_the_path() {
        // Moonlight on a LAN: 1392, or 1360 with video encryption; Tailscale's MTU is 1280.
        assert_eq!(fit_packet_size(1392, false, Some(1500)), None);
        assert_eq!(fit_packet_size(1392, false, Some(1280)), Some(1216));
        assert_eq!(fit_packet_size(1360, true, Some(1280)), Some(1184));
        // What Moonlight sends for a remote IPv4 address (1024) or IPv6 one (1184) fits.
        assert_eq!(fit_packet_size(1024, false, Some(1280)), None);
        assert_eq!(fit_packet_size(1184 - 32, true, Some(1280)), None);
        assert_eq!(fit_packet_size(1392, false, None), None);
        for (p, enc) in [(1392, false), (1360, true)] {
            let size = fit_packet_size(p, enc, Some(1280)).unwrap();
            assert!(size + 16 + if enc { 32 } else { 0 } + 48 <= 1280);
        }
    }

    #[test]
    fn loopback_path_mtu() {
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(path_mtu("127.0.0.1".parse().unwrap(), peer).is_some_and(|m| m >= 1280));
    }
}
