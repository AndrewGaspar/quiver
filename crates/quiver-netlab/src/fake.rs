//! A stand-in stream: a UDP "video" sender on the server side and a receiver on the client
//! side, so `run` and `matrix` work end to end before `quiver-stream-host` and
//! `quiver-stream` exist.
//!
//! The sender makes `fps` frames a second of `bitrate / fps` bytes each, split into packets
//! and sent as a burst, stamped with the capture time. It sends to whatever address the
//! receiver's feedback last came from, so a roam is followed as soon as feedback arrives from
//! the new address. The bitrate is fixed, or AIMD: on more than 5% loss or a frame latency
//! 30 ms above the recent minimum, down by a fifth (or to 90% of what the receiver got, if
//! that is lower), at most every 300 ms; up by 1.5% plus 0.2 Mbit/s per feedback otherwise.
//!
//! The receiver reassembles frames and writes [`ClientStats`] lines (capture → frame
//! complete as `capture_to_present_us`, packet and frame loss, RTT from ping/pong, jitter)
//! plus the `frame_gaps`, `max_gap_ms`, `recv_kbps` and `latency_p99_us` extras. The sender
//! writes [`HostStats`] lines with its real send rate.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use quiver_stream_proto::{ClientStats, HostStats, now_micros};

use crate::probe::Dist;
use crate::stats::StatsWriter;

const MEDIA: u8 = b'M';
const FEEDBACK: u8 = b'F';
const PONG: u8 = b'P';
const HEADER: usize = 21;

#[derive(Debug, Clone, clap::Args)]
pub struct ServeOpts {
    /// The UDP port to listen on.
    #[arg(long, default_value_t = 47998)]
    pub port: u16,
    /// The bitrate, or the starting bitrate with --aimd, in Mbit/s.
    #[arg(long, default_value_t = 20.0)]
    pub mbit: f64,
    /// Adapt the bitrate to loss and delay (additive increase, multiplicative decrease).
    #[arg(long)]
    pub aimd: bool,
    #[arg(long, default_value_t = 2.0)]
    pub min_mbit: f64,
    #[arg(long, default_value_t = 150.0)]
    pub max_mbit: f64,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// Bytes of payload per packet.
    #[arg(long, default_value_t = 1200)]
    pub payload: usize,
    /// Stop after this many seconds (default: until SIGTERM).
    #[arg(long)]
    pub duration: Option<f64>,
    /// The stats file (default: $QUIVER_NETLAB_STATS).
    #[arg(long)]
    pub stats: Option<PathBuf>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct PlayOpts {
    /// The sender's address (default: $QUIVER_NETLAB_SERVER_ADDR, port 47998).
    #[arg(long)]
    pub server: Option<SocketAddr>,
    /// The frame rate the sender runs at, for gap detection.
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// How often to write a stats line, in ms.
    #[arg(long, default_value_t = 500)]
    pub period_ms: u64,
    #[arg(long)]
    pub duration: Option<f64>,
    #[arg(long)]
    pub stats: Option<PathBuf>,
}

/// The sender. Runs until `stop` is set or its duration passes.
pub fn serve(o: &ServeOpts, stop: &AtomicBool) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", o.port)).with_context(|| format!("binding UDP {}", o.port))?;
    sock.set_nonblocking(true)?;
    let mut stats = StatsWriter::from_env(o.stats.as_deref())?;
    let interval = Duration::from_secs_f64(1.0 / o.fps as f64);
    let payload = o.payload.max(HEADER + 1);
    let mut peer: Option<SocketAddr> = None;
    let mut migrations = 0u32;
    let mut rate = o.mbit;
    let mut cooldown = Instant::now();
    let mut lat_window: VecDeque<(Instant, u32)> = VecDeque::new();
    let started = Instant::now();
    let mut next_frame = Instant::now();
    let mut frame: u32 = 0;
    let mut seq: u32 = 0;
    let mut period = (Instant::now(), 0u64, 0u32);
    let mut buf = vec![0u8; payload.max(64)];
    let mut rx = [0u8; 64];
    while !stop.load(Ordering::Relaxed) && o.duration.is_none_or(|d| started.elapsed().as_secs_f64() < d) {
        while let Ok((n, from)) = sock.recv_from(&mut rx) {
            if n < 21 || rx[0] != FEEDBACK {
                continue;
            }
            if peer != Some(from) {
                if peer.is_some() {
                    migrations += 1;
                }
                peer = Some(from);
            }
            let ping = u64::from_le_bytes(rx[1..9].try_into().unwrap());
            if ping != 0 {
                let mut pong = [0u8; 9];
                pong[0] = PONG;
                pong[1..9].copy_from_slice(&ping.to_le_bytes());
                let _ = sock.send_to(&pong, from);
            }
            let loss_ppm = u32::from_le_bytes(rx[9..13].try_into().unwrap());
            let latency = u32::from_le_bytes(rx[13..17].try_into().unwrap());
            let recv_kbps = u32::from_le_bytes(rx[17..21].try_into().unwrap());
            if o.aimd {
                let now = Instant::now();
                if latency > 0 {
                    lat_window.push_back((now, latency));
                }
                while lat_window.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(10)) {
                    lat_window.pop_front();
                }
                let base = lat_window.iter().map(|(_, l)| *l).min().unwrap_or(latency);
                let congested = loss_ppm > 50_000 || (latency > 0 && latency > base + 30_000);
                if congested {
                    if now >= cooldown {
                        // Down to what got through when that is lower: a step-down is
                        // followed in one feedback rather than many.
                        let got = recv_kbps as f64 / 1000.0;
                        let down = if got > 0.0 { (rate * 0.8).min(got * 0.9) } else { rate * 0.8 };
                        rate = down.max(o.min_mbit);
                        cooldown = now + Duration::from_millis(300);
                    }
                } else if latency == 0 || latency < base + 10_000 {
                    rate = (rate * 1.015 + 0.2).min(o.max_mbit);
                }
            }
        }
        let now = Instant::now();
        if now >= next_frame {
            next_frame += interval;
            if now > next_frame + interval * 4 {
                next_frame = now + interval; // Fell behind (a stall): don't burst to catch up.
            }
            if let Some(to) = peer {
                let bytes = (rate * 1e6 / 8.0 / o.fps as f64) as usize;
                let count = bytes.div_ceil(payload).clamp(1, u16::MAX as usize) as u16;
                let captured = now_micros();
                for idx in 0..count {
                    buf[0] = MEDIA;
                    buf[1..5].copy_from_slice(&seq.to_le_bytes());
                    buf[5..9].copy_from_slice(&frame.to_le_bytes());
                    buf[9..11].copy_from_slice(&idx.to_le_bytes());
                    buf[11..13].copy_from_slice(&count.to_le_bytes());
                    buf[13..21].copy_from_slice(&captured.to_le_bytes());
                    seq = seq.wrapping_add(1);
                    if sock.send_to(&buf[..payload], to).is_ok() {
                        period.1 += payload as u64;
                    }
                }
                period.2 += 1;
                frame = frame.wrapping_add(1);
            }
        }
        if period.0.elapsed() >= Duration::from_millis(500) {
            let secs = period.0.elapsed().as_secs_f64();
            let mut extra = BTreeMap::new();
            extra.insert("target_kbps".into(), rate * 1000.0);
            extra.insert("migrations".into(), migrations as f64);
            stats.host(
                HostStats {
                    fps: (period.2 as f64 / secs) as f32,
                    bitrate_kbps: (period.1 as f64 * 8.0 / secs / 1000.0) as u32,
                    ..Default::default()
                },
                extra,
            );
            period = (Instant::now(), 0, 0);
        }
        let wait = next_frame.saturating_duration_since(Instant::now()).min(Duration::from_millis(1));
        std::thread::sleep(wait.max(Duration::from_micros(100)));
    }
    Ok(())
}

/// Packets received against the span of sequence numbers seen, over a window.
#[derive(Default)]
struct SeqWindow {
    start: Option<u32>,
    received: u32,
}

impl SeqWindow {
    fn packet(&mut self, seq: u32) {
        self.start.get_or_insert(seq);
        self.received += 1;
    }

    /// (received, lost) since the window began, given the highest sequence number so far;
    /// starts the next window.
    fn take(&mut self, highest: Option<u32>) -> (u32, u32) {
        let span = match (self.start, highest) {
            (Some(a), Some(b)) if b >= a => b - a + 1,
            _ => 0,
        };
        let out = (self.received, span.saturating_sub(self.received));
        self.start = highest.map(|h| h.wrapping_add(1));
        self.received = 0;
        out
    }
}

struct FrameAcc {
    got: u16,
    count: u16,
    captured: u64,
    first_rx: u64,
}

/// The receiver. Runs until `stop` is set or its duration passes.
pub fn play(o: &PlayOpts, stop: &AtomicBool) -> Result<()> {
    let server = match o.server {
        Some(s) => s,
        None => {
            let ip = std::env::var("QUIVER_NETLAB_SERVER_ADDR").context("--server, or QUIVER_NETLAB_SERVER_ADDR")?;
            SocketAddr::new(ip.parse()?, 47998)
        }
    };
    let bind: SocketAddr = if server.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let sock = UdpSocket::bind(bind)?;
    sock.set_read_timeout(Some(Duration::from_millis(1)))?;
    let mut stats = StatsWriter::from_env(o.stats.as_deref())?;
    let interval_us = 1_000_000 / o.fps.max(1) as u64;
    let started = Instant::now();
    let mut frames: HashMap<u32, FrameAcc> = HashMap::new();
    let mut newest_done: Option<u32> = None;
    let mut last_done_us: Option<u64> = None;
    let mut highest_seq: Option<u32> = None;
    let mut period_seq = SeqWindow::default();
    let mut fb_seq = SeqWindow::default();
    let mut buf = vec![0u8; 65536];
    let mut last_feedback = Instant::now() - Duration::from_secs(1);
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut rtt_us = 0u32;
    let mut jitter_us = 0f64;
    let mut prev_transit: Option<i64> = None;
    let mut period_start = Instant::now();
    // Per period.
    let mut lat: Vec<f64> = Vec::new();
    let mut transit: Vec<f64> = Vec::new();
    let (mut bytes, mut frames_lost, mut gaps, mut max_gap_us) = (0u64, 0u32, 0u32, 0u64);
    // Frame latencies since the last feedback, for the sender's controller.
    let mut fb_lat = Vec::<u32>::new();
    let mut fb_bytes = 0u64;
    let mut fb_window = Instant::now();
    while !stop.load(Ordering::Relaxed) && o.duration.is_none_or(|d| started.elapsed().as_secs_f64() < d) {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let now = now_micros();
            if n >= HEADER && buf[0] == MEDIA {
                let seq = u32::from_le_bytes(buf[1..5].try_into().unwrap());
                let fid = u32::from_le_bytes(buf[5..9].try_into().unwrap());
                let count = u16::from_le_bytes(buf[11..13].try_into().unwrap());
                let captured = u64::from_le_bytes(buf[13..21].try_into().unwrap());
                period_seq.packet(seq);
                fb_seq.packet(seq);
                bytes += n as u64;
                fb_bytes += n as u64;
                if highest_seq.is_none_or(|h| seq > h) {
                    highest_seq = Some(seq);
                }
                if newest_done.is_some_and(|d| fid <= d) && !frames.contains_key(&fid) {
                    continue; // A late or duplicate packet of a finished or abandoned frame.
                }
                let f = frames.entry(fid).or_insert(FrameAcc { got: 0, count, captured, first_rx: now });
                f.got += 1;
                if f.got >= f.count {
                    let f = frames.remove(&fid).unwrap();
                    let l = now.saturating_sub(f.captured);
                    lat.push(l as f64);
                    fb_lat.push(l as u32);
                    let t = f.first_rx as i64 - f.captured as i64;
                    transit.push(t.max(0) as f64);
                    if let Some(p) = prev_transit {
                        jitter_us += ((t - p).abs() as f64 - jitter_us) / 16.0;
                    }
                    prev_transit = Some(t);
                    if let Some(last) = last_done_us {
                        let gap = now.saturating_sub(last);
                        if gap * 2 > interval_us * 5 {
                            gaps += 1;
                            max_gap_us = max_gap_us.max(gap);
                        }
                    }
                    last_done_us = Some(now);
                    newest_done = Some(newest_done.map_or(fid, |d| d.max(fid)));
                    // Frames older than this one that are still incomplete never will be.
                    let stale: Vec<u32> = frames.keys().copied().filter(|&k| k < fid).collect();
                    for k in stale {
                        frames.remove(&k);
                        frames_lost += 1;
                    }
                }
            } else if n >= 9 && buf[0] == PONG {
                let sent = u64::from_le_bytes(buf[1..9].try_into().unwrap());
                rtt_us = now.saturating_sub(sent) as u32;
            }
        }
        // Frames that stopped arriving altogether (an outage) are lost after 500 ms.
        let now = now_micros();
        let before = frames.len();
        frames.retain(|_, f| now.saturating_sub(f.first_rx) < 500_000);
        frames_lost += (before - frames.len()) as u32;

        if last_feedback.elapsed() >= Duration::from_millis(50) {
            last_feedback = Instant::now();
            let mut fb = [0u8; 21];
            fb[0] = FEEDBACK;
            if last_ping.elapsed() >= Duration::from_millis(200) {
                last_ping = Instant::now();
                fb[1..9].copy_from_slice(&now_micros().to_le_bytes());
            }
            let (fb_pkts, fb_lost) = fb_seq.take(highest_seq);
            let expected = fb_pkts + fb_lost;
            let loss_ppm = if expected > 0 { (fb_lost as u64 * 1_000_000 / expected as u64) as u32 } else { 0 };
            fb_lat.sort_unstable();
            let med = fb_lat.get(fb_lat.len() / 2).copied().unwrap_or(0);
            fb[9..13].copy_from_slice(&loss_ppm.to_le_bytes());
            fb[13..17].copy_from_slice(&med.to_le_bytes());
            let fb_secs = fb_window.elapsed().as_secs_f64().max(1e-3);
            let fb_kbps = (fb_bytes as f64 * 8.0 / fb_secs / 1000.0) as u32;
            fb[17..21].copy_from_slice(&fb_kbps.to_le_bytes());
            fb_bytes = 0;
            fb_window = Instant::now();
            let _ = sock.send_to(&fb, server);
            fb_lat.clear();
        }

        if period_start.elapsed() >= Duration::from_millis(o.period_ms) {
            let secs = period_start.elapsed().as_secs_f64();
            let (pkts, lost) = period_seq.take(highest_seq);
            let ld = Dist::of(&lat);
            let td = Dist::of(&transit);
            let recv_kbps = bytes as f64 * 8.0 / secs / 1000.0;
            let mut extra = BTreeMap::new();
            extra.insert("frame_gaps".into(), gaps as f64);
            extra.insert("max_gap_ms".into(), max_gap_us as f64 / 1000.0);
            extra.insert("recv_kbps".into(), recv_kbps);
            extra.insert("latency_p99_us".into(), ld.p99);
            stats.client(
                ClientStats {
                    frames_received: lat.len() as u32,
                    frames_lost,
                    frames_dropped: 0,
                    packets_received: pkts,
                    packets_lost: lost,
                    packets_recovered: 0,
                    rtt_us,
                    jitter_us: jitter_us as u32,
                    network_us: td.p50 as u32,
                    decode_us: 0,
                    present_us: 0,
                    capture_to_present_us: ld.p50 as u32,
                    estimated_kbps: recv_kbps as u32,
                },
                extra,
            );
            period_start = Instant::now();
            lat.clear();
            transit.clear();
            (bytes, frames_lost, gaps, max_gap_us) = (0, 0, 0, 0);
        }
    }
    Ok(())
}
