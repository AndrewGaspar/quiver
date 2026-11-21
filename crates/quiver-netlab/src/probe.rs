//! `verify`: measure what the shaping actually delivers and compare it with the profile.
//!
//! Three measurements, all on sockets this process makes inside the lab:
//! - **Idle**: small UDP packets at a fixed rate from the client, echoed by the server. Both
//!   ends share `CLOCK_MONOTONIC`, so each packet gives the one-way delay in each direction as
//!   well as the round trip, and sequence numbers give loss, duplicates and reordering per
//!   direction.
//! - **Rate**: a paced UDP flood at 1.2× the profile's rate, one direction at a time; the
//!   receiver's delivered rate is the link's capacity. A light echo probe runs during the
//!   flood to the client, so the round trip under load shows the bottleneck's queue
//!   (bufferbloat).
//! - **TCP**: one bulk TCP flow to the client, for reference (not checked: TCP's rate depends
//!   on its own congestion control as much as on the link).
//!
//! The expected delays come from a model of netem rather than from the profile's numbers
//! alone: it samples the same distribution tables tc loads (`/usr/share/tc/*.dist`) and
//! schedules each packet no earlier than the one before it, as netem does with a `rate`. So
//! the check is that the kernel does what the model says, which is what the profile means.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use quiver_stream_proto::now_micros;
use serde::{Deserialize, Serialize};

use crate::link::{Shaper, WanderState};
use crate::ns::{Dir, Lab, Ns, SERVER_V4};
use crate::profile::Profile;
use crate::shape::{Distribution, LinkShape};

/// Bytes a flood packet occupies at the shaper: 1400 of payload, UDP, IPv4 and Ethernet.
const FLOOD_PAYLOAD: usize = 1400;
const FLOOD_WIRE: f64 = (FLOOD_PAYLOAD + 8 + 20 + 14) as f64;
const MAGIC: u32 = 0x514e_4c42; // "QNLB"

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ProbeOptions {
    /// How long the idle probe runs, and at how many packets a second.
    pub idle_s: f64,
    pub pps: u32,
    /// How long each direction's flood runs.
    pub load_s: f64,
    pub tcp_s: f64,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        ProbeOptions { idle_s: 3.0, pps: 1000, load_s: 2.5, tcp_s: 2.0 }
    }
}

/// A summary of a sample of values (ms, or whatever was sampled).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Dist {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

impl Dist {
    pub fn of(values: &[f64]) -> Dist {
        if values.is_empty() {
            return Dist::default();
        }
        let mut v = values.to_vec();
        v.sort_by(|a, b| a.total_cmp(b));
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        Dist {
            n: v.len(),
            min: v[0],
            p50: q(0.5),
            p90: q(0.9),
            p99: q(0.99),
            max: v[v.len() - 1],
            mean: v.iter().sum::<f64>() / v.len() as f64,
        }
    }
}

/// One direction of the idle probe.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OneWay {
    pub sent: u64,
    /// Distinct packets that arrived.
    pub received: u64,
    pub duplicates: u64,
    /// Arrivals with a lower sequence number than the arrival before them.
    pub reordered: u64,
    pub owd_ms: Dist,
}

impl OneWay {
    pub fn loss_pct(&self) -> f64 {
        if self.sent == 0 { 0.0 } else { 100.0 * (1.0 - self.received as f64 / self.sent as f64) }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EchoResult {
    pub to_server: OneWay,
    pub to_client: OneWay,
    pub rtt_ms: Dist,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateResult {
    pub offered_mbit: f64,
    /// What the sender managed to send. Below the link's rate, the result is sender-limited.
    pub sent_mbit: f64,
    /// What arrived, counted at the shaper's size (payload plus UDP/IP/Ethernet headers).
    pub delivered_mbit: f64,
    pub loss_pct: f64,
    /// The rate the shaper was set to, averaged over the measurement (it moves with `wander`).
    pub shaped_mbit: f64,
    /// The round trip of a light probe while this direction was flooded.
    pub loaded_rtt_ms: Option<Dist>,
}

impl RateResult {
    pub fn sender_limited(&self) -> bool {
        self.sent_mbit < self.shaped_mbit * 1.05
    }
}

/// What the profile and the model of netem predict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expected {
    pub owd_to_server_ms: Dist,
    pub owd_to_client_ms: Dist,
    pub rtt_ms: Dist,
    pub loss_to_server_pct: f64,
    pub loss_to_client_pct: f64,
    /// The round trip with the queue to the client full.
    pub loaded_rtt_p50_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub what: String,
    pub target: String,
    pub measured: String,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub profile: String,
    pub options: ProbeOptions,
    pub expected: Expected,
    pub idle: EchoResult,
    pub rate_to_client: RateResult,
    pub rate_to_server: RateResult,
    pub tcp_to_client_mbit: f64,
    pub checks: Vec<Check>,
}

impl Verification {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// A plain-text table of the checks.
    pub fn render(&self) -> String {
        let mut s = format!("verify {}:\n", self.profile);
        let w = self.checks.iter().map(|c| c.what.len()).max().unwrap_or(10);
        for c in &self.checks {
            s += &format!(
                "  {:<w$}  target {:<26}  measured {:<26}  {}\n",
                c.what,
                c.target,
                c.measured,
                if c.ok { "ok" } else { "FAIL" }
            );
        }
        s += &format!("  {:<w$}  {:.1} Mbit/s (reference, not checked)\n", "tcp to client", self.tcp_to_client_mbit);
        s
    }
}

/// Reads a tc distribution table (values in 1/8192 of a standard deviation).
pub fn dist_table(d: Distribution) -> Option<Vec<i32>> {
    let name = match d {
        Distribution::Uniform => return None,
        Distribution::Normal => "normal",
        Distribution::Pareto => "pareto",
        Distribution::Paretonormal => "paretonormal",
    };
    for dir in ["/usr/share/tc", "/usr/lib/tc", "/usr/lib64/tc"] {
        if let Ok(text) = std::fs::read_to_string(format!("{dir}/{name}.dist")) {
            let v: Vec<i32> = text
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .flat_map(|l| l.split_whitespace().map(|x| x.parse::<i32>().unwrap_or(0)).collect::<Vec<_>>())
                .collect();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// netem's one-way delays for packets sent at `send_ms`, in order: each packet's delay is
/// drawn as netem draws it, and it can't leave before the packet ahead of it.
pub fn model_owd(shape: &LinkShape, send_ms: &[f64], rng: &mut fastrand::Rng) -> Vec<f64> {
    let table = if shape.jitter_ms > 0.0 { dist_table(shape.distribution) } else { None };
    let mut last = f64::MIN;
    send_ms
        .iter()
        .map(|&t| {
            let d = if shape.jitter_ms <= 0.0 {
                shape.delay_ms
            } else if let Some(tab) = &table {
                shape.delay_ms + shape.jitter_ms * tab[rng.usize(..tab.len())] as f64 / 8192.0
            } else {
                shape.delay_ms + shape.jitter_ms * (rng.f64() * 2.0 - 1.0)
            };
            let leave = (t + d).max(t).max(last);
            last = leave;
            leave - t
        })
        .collect()
}

/// The model's prediction for the idle echo probe.
pub fn expected(p: &Profile, opts: &ProbeOptions) -> Expected {
    let mut rng = fastrand::Rng::with_seed(7);
    let n = (opts.idle_s * opts.pps as f64) as usize;
    let send: Vec<f64> = (0..n).map(|i| i as f64 * 1000.0 / opts.pps as f64).collect();
    let up = model_owd(&p.to_server, &send, &mut rng);
    let arrive: Vec<f64> = send.iter().zip(&up).map(|(t, d)| t + d).collect();
    let down = model_owd(&p.to_client, &arrive, &mut rng);
    let rtt: Vec<f64> = up.iter().zip(&down).map(|(a, b)| a + b).collect();
    let rtt_d = Dist::of(&rtt);
    let loss = |s: &LinkShape| (s.mean_loss_pct() + s.corrupt_pct).min(100.0);
    Expected {
        owd_to_server_ms: Dist::of(&up),
        owd_to_client_ms: Dist::of(&down),
        loaded_rtt_p50_ms: rtt_d.p50 + p.to_client.full_queue_ms(),
        rtt_ms: rtt_d,
        loss_to_server_pct: loss(&p.to_server),
        loss_to_client_pct: loss(&p.to_client),
    }
}

/// A UDP echo server on the server side: it counts what arrives and echoes each packet with
/// its own sequence number and timestamp appended.
struct Echo {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<OneWay>>,
    echoed: Arc<AtomicU64>,
}

impl Echo {
    fn start(lab: &Lab, port: u16) -> Result<Echo> {
        let sock = lab.udp(Ns::Server, SocketAddr::new("0.0.0.0".parse().unwrap(), port))?;
        sock.set_read_timeout(Some(Duration::from_millis(50)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let echoed = Arc::new(AtomicU64::new(0));
        let (s2, e2) = (stop.clone(), echoed.clone());
        let thread = std::thread::spawn(move || {
            let mut rx = Receiver::default();
            let mut buf = [0u8; 2048];
            let mut eseq: u32 = 0;
            while !s2.load(Ordering::Relaxed) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                let now = now_micros();
                let Some((seq, sent)) = parse_probe(&buf[..n]) else { continue };
                rx.packet(seq, now.saturating_sub(sent));
                let mut out = [0u8; 32];
                out[..16].copy_from_slice(&buf[..16]);
                out[16..20].copy_from_slice(&eseq.to_le_bytes());
                out[20..28].copy_from_slice(&now_micros().to_le_bytes());
                eseq += 1;
                if sock.send_to(&out[..28], from).is_ok() {
                    e2.fetch_add(1, Ordering::Relaxed);
                }
            }
            rx.finish(0)
        });
        Ok(Echo { stop, thread: Some(thread), echoed })
    }

    fn finish(mut self, sent: u64) -> OneWay {
        self.stop.store(true, Ordering::Relaxed);
        let mut w = self.thread.take().unwrap().join().unwrap();
        w.sent = sent;
        w
    }
}

/// Counts distinct, duplicate and late packets by sequence number, and their delays.
#[derive(Default)]
struct Receiver {
    seen: std::collections::HashSet<u32>,
    prev_seq: Option<u32>,
    dup: u64,
    reordered: u64,
    owd: Vec<f64>,
}

impl Receiver {
    fn packet(&mut self, seq: u32, owd_us: u64) {
        if !self.seen.insert(seq) {
            self.dup += 1;
            return;
        }
        // A descent (a packet after one with a higher number) counts once per packet that
        // overtook, however far it jumped.
        if self.prev_seq.is_some_and(|m| seq < m) {
            self.reordered += 1;
        }
        self.prev_seq = Some(seq);
        self.owd.push(owd_us as f64 / 1000.0);
    }

    fn finish(self, sent: u64) -> OneWay {
        OneWay {
            sent,
            received: self.seen.len() as u64,
            duplicates: self.dup,
            reordered: self.reordered,
            owd_ms: Dist::of(&self.owd),
        }
    }
}

fn parse_probe(b: &[u8]) -> Option<(u32, u64)> {
    if b.len() < 16 || u32::from_le_bytes(b[0..4].try_into().unwrap()) != MAGIC {
        return None;
    }
    Some((u32::from_le_bytes(b[4..8].try_into().unwrap()), u64::from_le_bytes(b[8..16].try_into().unwrap())))
}

/// The echo probe: `pps` packets a second for `secs` from the client to the server's echo.
pub fn echo_probe(lab: &Lab, port: u16, pps: u32, secs: f64, linger: Duration) -> Result<EchoResult> {
    let echo = Echo::start(lab, port)?;
    let sock = lab.udp(Ns::Client, SocketAddr::new("0.0.0.0".parse().unwrap(), 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(20)))?;
    let to = SocketAddr::new(SERVER_V4.into(), port);
    let n = (secs * pps as f64).round() as u32;
    let interval = Duration::from_secs_f64(1.0 / pps as f64);
    let rx_sock = sock.try_clone()?;
    let done = Arc::new(AtomicBool::new(false));
    let d2 = done.clone();
    let reader = std::thread::spawn(move || {
        let mut down = Receiver::default();
        let mut rtts = Vec::new();
        let mut buf = [0u8; 64];
        while !d2.load(Ordering::Relaxed) {
            let Ok((len, _)) = rx_sock.recv_from(&mut buf) else { continue };
            let now = now_micros();
            if len < 28 {
                continue;
            }
            let Some((_, sent)) = parse_probe(&buf[..len]) else { continue };
            let eseq = u32::from_le_bytes(buf[16..20].try_into().unwrap());
            let etime = u64::from_le_bytes(buf[20..28].try_into().unwrap());
            let fresh = !down.seen.contains(&eseq);
            down.packet(eseq, now.saturating_sub(etime));
            if fresh {
                rtts.push(now.saturating_sub(sent) as f64 / 1000.0);
            }
        }
        (down, rtts)
    });
    let start = Instant::now();
    let mut pkt = [0u8; 64];
    pkt[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    for seq in 0..n {
        let due = start + interval * seq;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        pkt[4..8].copy_from_slice(&seq.to_le_bytes());
        pkt[8..16].copy_from_slice(&now_micros().to_le_bytes());
        let _ = sock.send_to(&pkt, to);
    }
    std::thread::sleep(linger);
    done.store(true, Ordering::Relaxed);
    let (down, rtts) = reader.join().unwrap();
    let echoed = echo.echoed.load(Ordering::Relaxed);
    let to_server = echo.finish(n as u64);
    Ok(EchoResult { to_server, to_client: down.finish(echoed), rtt_ms: Dist::of(&rtts) })
}

/// Sets big socket buffers (forcing past `rmem_max`: this process owns the lab's user
/// namespace, so it has CAP_NET_ADMIN over the socket's network namespace).
pub fn big_buffers(sock: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let size: libc::c_int = 16 << 20;
    for opt in [libc::SO_RCVBUFFORCE, libc::SO_SNDBUFFORCE] {
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&size as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// A paced UDP flood from one side to the other: `mbit` (at the shaper's packet size) for
/// `secs`. Returns the bytes sent at the shaper's size. Stops early if `stop` is set.
pub fn udp_flood(sock: &UdpSocket, to: SocketAddr, mbit: f64, secs: f64, stop: &AtomicBool) -> u64 {
    let pkt = [0x55u8; FLOOD_PAYLOAD];
    let start = Instant::now();
    let bytes_per_s = mbit * 1e6 / 8.0;
    let mut sent = 0u64;
    loop {
        let el = start.elapsed().as_secs_f64();
        if el >= secs || stop.load(Ordering::Relaxed) {
            break;
        }
        let target = (el * bytes_per_s) as u64;
        let mut burst = 0;
        while sent < target && burst < 256 {
            if sock.send_to(&pkt, to).is_ok() {
                sent += FLOOD_WIRE as u64;
            } else {
                // A full socket buffer: count it as sent (it's dropped locally) and move on.
                sent += FLOOD_WIRE as u64;
            }
            burst += 1;
        }
        if sent >= target {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    sent
}

/// Floods `dir` at `offered_mbit` for `secs` and measures what arrives. With
/// `loaded_probe: Some(after_s)`, a light echo probe runs from `after_s` (once the queue has
/// filled) to the end of the flood.
pub fn udp_rate(lab: &Lab, dir: Dir, offered_mbit: f64, secs: f64, port: u16, loaded_probe: Option<f64>) -> Result<RateResult> {
    let any: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let (src_ns, dst_ns, dst_ip) = match dir {
        Dir::ToClient => (Ns::Server, Ns::Client, lab.addr(crate::Side::Client, false)),
        Dir::ToServer => (Ns::Client, Ns::Server, SERVER_V4.into()),
    };
    let rx = lab.udp(dst_ns, SocketAddr::new(any.ip(), port))?;
    big_buffers(&rx);
    rx.set_read_timeout(Some(Duration::from_millis(50)))?;
    let tx = lab.udp(src_ns, any)?;
    big_buffers(&tx);
    let to = SocketAddr::new(dst_ip, port);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut buf = [0u8; 2048];
            let mut count = 0u64;
            let mut first: Option<Instant> = None;
            let mut last = Instant::now();
            // The first 200 ms are left out: tbf's bucket starts full.
            let mut counted_from: Option<(Instant, u64)> = None;
            while !stop.load(Ordering::Relaxed) {
                if rx.recv_from(&mut buf).is_ok() {
                    let now = Instant::now();
                    let f = *first.get_or_insert(now);
                    count += 1;
                    last = now;
                    if counted_from.is_none() && now.duration_since(f) >= Duration::from_millis(200) {
                        counted_from = Some((now, count));
                    }
                }
            }
            (count, counted_from, last)
        });
        let probe = loaded_probe.map(|after| {
            let after = after + 0.2;
            s.spawn(move || {
                std::thread::sleep(Duration::from_secs_f64(after));
                echo_probe(lab, port + 100, 100, (secs - after - 0.2).max(0.5), Duration::from_millis(1500))
            })
        });
        let sent = udp_flood(&tx, to, offered_mbit, secs, &AtomicBool::new(false));
        std::thread::sleep(Duration::from_millis(1000));
        let loaded = match probe {
            Some(h) => Some(h.join().unwrap()?.rtt_ms),
            None => None,
        };
        stop.store(true, Ordering::Relaxed);
        let (count, from, last) = reader.join().unwrap();
        let delivered = match from {
            Some((t0, c0)) if last > t0 => {
                (count - c0) as f64 * FLOOD_WIRE * 8.0 / last.duration_since(t0).as_secs_f64() / 1e6
            }
            _ => 0.0,
        };
        let sent_packets = sent as f64 / FLOOD_WIRE;
        Ok(RateResult {
            offered_mbit,
            sent_mbit: sent as f64 * 8.0 / secs / 1e6,
            delivered_mbit: delivered,
            loss_pct: if sent_packets > 0.0 { 100.0 * (1.0 - count as f64 / sent_packets).max(0.0) } else { 0.0 },
            shaped_mbit: 0.0,
            loaded_rtt_ms: loaded,
        })
    })
}

/// One bulk TCP flow in `dir` for `secs`; the goodput in Mbit/s.
pub fn tcp_rate(lab: &Lab, dir: Dir, secs: f64, port: u16) -> Result<f64> {
    use std::io::{Read, Write};
    let (src_ns, dst_ns, dst_ip) = match dir {
        Dir::ToClient => (Ns::Server, Ns::Client, lab.addr(crate::Side::Client, false)),
        Dir::ToServer => (Ns::Client, Ns::Server, SERVER_V4.into()),
    };
    let listener = lab.tcp_listen(dst_ns, SocketAddr::new("0.0.0.0".parse().unwrap(), port))?;
    let reader = std::thread::spawn(move || -> u64 {
        let Ok((mut s, _)) = listener.accept() else { return 0 };
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = vec![0u8; 1 << 16];
        let mut total = 0u64;
        while let Ok(n) = s.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    });
    let mut s = lab.tcp_connect(src_ns, SocketAddr::new(dst_ip, port), Duration::from_secs(5))?;
    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
    let buf = vec![0xAAu8; 1 << 16];
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        if s.write(&buf).is_err() {
            continue;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    drop(s);
    let total = reader.join().unwrap();
    Ok(total as f64 * 8.0 / elapsed / 1e6)
}

/// Runs `f` while a thread wanders the rates the profile says wander, and returns `f`'s
/// result with the mean rate applied to each direction meanwhile.
fn with_wander<T>(shaper: &mut Shaper, p: &Profile, f: impl FnOnce() -> T) -> Result<(T, [f64; 2])> {
    let stop = AtomicBool::new(false);
    let rates = Mutex::new([vec![p.to_client.rate_mbit], vec![p.to_server.rate_mbit]]);
    let out = std::thread::scope(|s| {
        let wander = s.spawn(|| -> Result<()> {
            let mut st = WanderState::new(p, 11);
            while !stop.load(Ordering::Relaxed) {
                for (dir, rate) in st.tick(shaper)? {
                    rates.lock().unwrap()[if dir == Dir::ToClient { 0 } else { 1 }].push(rate);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // Back to the profile's own rates for the next measurement.
            shaper.apply_profile(p)
        });
        let out = f();
        stop.store(true, Ordering::Relaxed);
        wander.join().unwrap().map(|_| out)
    })?;
    let r = rates.into_inner().unwrap();
    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    Ok((out, [mean(&r[0]), mean(&r[1])]))
}

/// Shapes the lab as `p`, measures it, and checks the measurements against the model.
pub fn verify(lab: &Lab, p: &Profile, opts: &ProbeOptions) -> Result<Verification> {
    let mut shaper = Shaper::new(lab);
    shaper.apply_profile(p)?;
    let exp = expected(p, opts);
    let linger = Duration::from_millis((exp.rtt_ms.max * 2.0) as u64 + 500);
    let idle = echo_probe(lab, 7001, opts.pps, opts.idle_s, linger)?;

    let mut rates = Vec::new();
    for (dir, port) in [(Dir::ToClient, 7002), (Dir::ToServer, 7003)] {
        let shape = p.shape(dir);
        // Offer well above the link so its queue fills quickly (at 1.2× a 600 ms FIFO takes 3 s
        // to fill), and start the loaded probe once it has.
        let top = shape.wander.map_or(shape.rate_mbit, |w| w.max_mbit.max(shape.rate_mbit));
        let offered = (top * 2.0).min(top + 300.0);
        let fill_s = shape.queue_bytes() as f64 * 8.0 / ((offered - top) * 1e6);
        let secs = opts.load_s.max(fill_s + 2.0);
        let (r, mean) =
            with_wander(&mut shaper, p, || udp_rate(lab, dir, offered, secs, port, (dir == Dir::ToClient).then_some(fill_s)))?;
        let mut r = r?;
        r.shaped_mbit = mean[if dir == Dir::ToClient { 0 } else { 1 }];
        rates.push(r);
    }
    let tcp = tcp_rate(lab, Dir::ToClient, opts.tcp_s, 7004).unwrap_or(0.0);
    let rate_to_server = rates.pop().unwrap();
    let rate_to_client = rates.pop().unwrap();
    let mut v = Verification {
        profile: p.name.clone(),
        options: *opts,
        expected: exp,
        idle,
        rate_to_client,
        rate_to_server,
        tcp_to_client_mbit: tcp,
        checks: vec![],
    };
    v.checks = checks(p, &v);
    Ok(v)
}

fn checks(p: &Profile, v: &Verification) -> Vec<Check> {
    let mut out = Vec::new();
    let mut add = |what: &str, target: String, measured: String, ok: bool| {
        out.push(Check { what: what.into(), target, measured, ok });
    };
    // Delays: within 15% (and 0.3 ms) of the model at the median, 25% (and 1 ms) at p99.
    let near = |m: f64, e: f64, rel: f64, abs: f64| (m - e).abs() <= e * rel + abs;
    for (name, m, e) in [
        ("owd to client", &v.idle.to_client.owd_ms, &v.expected.owd_to_client_ms),
        ("owd to server", &v.idle.to_server.owd_ms, &v.expected.owd_to_server_ms),
        ("rtt", &v.idle.rtt_ms, &v.expected.rtt_ms),
    ] {
        add(
            &format!("{name} p50 (ms)"),
            format!("{:.2}", e.p50),
            format!("{:.2}", m.p50),
            near(m.p50, e.p50, 0.15, 0.3),
        );
        add(
            &format!("{name} p99 (ms)"),
            format!("{:.2}", e.p99),
            format!("{:.2}", m.p99),
            near(m.p99, e.p99, 0.25, 1.0),
        );
    }
    // Loss: within four standard deviations of a binomial count, widened for bursts.
    for (name, ow, e, shape) in [
        ("loss to client (%)", &v.idle.to_client, v.expected.loss_to_client_pct, &p.to_client),
        ("loss to server (%)", &v.idle.to_server, v.expected.loss_to_server_pct, &p.to_server),
    ] {
        let n = ow.sent.max(1) as f64;
        let q = e / 100.0;
        let burst = shape.loss.burst.unwrap_or(1.0).max(1.0);
        let sd = (q * (1.0 - q) * (2.0 * burst - 1.0) / n).sqrt() * 100.0;
        let m = ow.loss_pct();
        add(name, format!("{e:.2} ± {:.2}", 4.0 * sd + 0.15), format!("{m:.2}"), (m - e).abs() <= 4.0 * sd + 0.15);
    }
    for (name, r) in [("rate to client (Mbit/s)", &v.rate_to_client), ("rate to server (Mbit/s)", &v.rate_to_server)] {
        if r.sender_limited() {
            // The sender couldn't offer more than the link: what got through must be what was sent.
            add(
                name,
                format!("{:.1} (sender reached {:.1})", r.shaped_mbit, r.sent_mbit),
                format!("{:.1}", r.delivered_mbit),
                r.delivered_mbit >= r.sent_mbit * 0.85,
            );
        } else {
            let wanders = if std::ptr::eq(r, &v.rate_to_client) { p.to_client.wander.is_some() } else { p.to_server.wander.is_some() };
            let tol = if wanders { 0.15 } else { 0.07 };
            add(
                name,
                format!("{:.1} ± {:.0}%", r.shaped_mbit, tol * 100.0),
                format!("{:.1}", r.delivered_mbit),
                near(r.delivered_mbit, r.shaped_mbit, tol, 0.2),
            );
        }
    }
    if let Some(loaded) = &v.rate_to_client.loaded_rtt_ms
        && !v.rate_to_client.sender_limited()
        && p.to_client.wander.is_none()
    {
        let e = v.expected.loaded_rtt_p50_ms;
        add(
            "loaded rtt p50 (ms)",
            format!("{e:.1}"),
            format!("{:.1}", loaded.p50),
            near(loaded.p50, e, 0.2, 3.0),
        );
    }
    let dup = |s: &LinkShape| s.duplicate_pct;
    if dup(&p.to_client) > 0.0 {
        let ow = &v.idle.to_client;
        let m = 100.0 * ow.duplicates as f64 / ow.received.max(1) as f64;
        add("duplicates to client (%)", format!("{:.2}", dup(&p.to_client)), format!("{m:.2}"), m > 0.0 && m < dup(&p.to_client) * 3.0 + 0.5);
    }
    if p.to_client.reorder_pct > 0.0 {
        let ow = &v.idle.to_client;
        let m = 100.0 * ow.reordered as f64 / ow.received.max(1) as f64;
        add(
            "reordered to client (%)",
            format!("{:.2}", p.to_client.reorder_pct),
            format!("{m:.2}"),
            m > 0.0 && m < p.to_client.reorder_pct * 3.0 + 0.5,
        );
    }
    out
}

/// A light echo probe that runs for a whole `run`: the path's own round trip over time,
/// independent of what the workloads report.
pub struct PathProbe {
    stop: Arc<AtomicBool>,
    echo: Option<Echo>,
    sender: Option<std::thread::JoinHandle<u64>>,
    reader: Option<std::thread::JoinHandle<Vec<(u64, f64)>>>,
}

/// What a [`PathProbe`] saw: (CLOCK_MONOTONIC µs at send, round trip ms) per answered packet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathSamples {
    pub sent: u64,
    pub samples: Vec<(u64, f64)>,
}

impl PathProbe {
    pub fn start(lab: &Lab, port: u16, pps: u32) -> Result<PathProbe> {
        let echo = Echo::start(lab, port)?;
        let sock = lab.udp(Ns::Client, SocketAddr::new("0.0.0.0".parse().unwrap(), 0))?;
        sock.set_read_timeout(Some(Duration::from_millis(50)))?;
        let rx = sock.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        let to = SocketAddr::new(SERVER_V4.into(), port);
        let s1 = stop.clone();
        let interval = Duration::from_secs_f64(1.0 / pps.max(1) as f64);
        let sender = std::thread::spawn(move || {
            let mut pkt = [0u8; 64];
            pkt[0..4].copy_from_slice(&MAGIC.to_le_bytes());
            let mut seq: u32 = 0;
            while !s1.load(Ordering::Relaxed) {
                pkt[4..8].copy_from_slice(&seq.to_le_bytes());
                pkt[8..16].copy_from_slice(&now_micros().to_le_bytes());
                let _ = sock.send_to(&pkt, to);
                seq += 1;
                std::thread::sleep(interval);
            }
            seq as u64
        });
        let s2 = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 64];
            // Keep reading a little after the stop, for the last answers in flight.
            let mut until: Option<Instant> = None;
            loop {
                if s2.load(Ordering::Relaxed) && until.is_none() {
                    until = Some(Instant::now() + Duration::from_millis(500));
                }
                if until.is_some_and(|u| Instant::now() >= u) {
                    break;
                }
                let Ok((n, _)) = rx.recv_from(&mut buf) else { continue };
                let now = now_micros();
                if let Some((_, sent)) = parse_probe(&buf[..n]) {
                    out.push((sent, now.saturating_sub(sent) as f64 / 1000.0));
                }
            }
            out
        });
        Ok(PathProbe { stop, echo: Some(echo), sender: Some(sender), reader: Some(reader) })
    }

    pub fn stop(mut self) -> PathSamples {
        self.stop.store(true, Ordering::Relaxed);
        let sent = self.sender.take().unwrap().join().unwrap_or(0);
        let samples = self.reader.take().unwrap().join().unwrap_or_default();
        if let Some(e) = self.echo.take() {
            e.finish(sent);
        }
        PathSamples { sent, samples }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_quantiles() {
        let v: Vec<f64> = (0..=100).map(|i| i as f64).collect();
        let d = Dist::of(&v);
        assert_eq!((d.min, d.p50, d.p90, d.p99, d.max), (0.0, 50.0, 90.0, 99.0, 100.0));
        assert_eq!(Dist::of(&[]).n, 0);
    }

    #[test]
    fn the_model_keeps_order_and_centres_on_the_delay() {
        let s = LinkShape { delay_ms: 20.0, jitter_ms: 5.0, distribution: Distribution::Uniform, ..Default::default() };
        let send: Vec<f64> = (0..2000).map(|i| i as f64 * 10.0).collect();
        let owd = model_owd(&s, &send, &mut fastrand::Rng::with_seed(1));
        let leave: Vec<f64> = send.iter().zip(&owd).map(|(a, b)| a + b).collect();
        assert!(leave.windows(2).all(|w| w[1] >= w[0]));
        // Packets 10 ms apart rarely queue behind each other with ±5 ms: median ≈ delay.
        let d = Dist::of(&owd);
        assert!((d.p50 - 20.0).abs() < 0.5, "{d:?}");
        assert!(d.min >= 15.0 && d.max <= 25.0);
    }
}
