//! A network simulator, entirely in userspace, for tests and benchmarks.
//!
//! - [`Link`] is one direction of a path: random or bursty (Gilbert–Elliott) loss, delay,
//!   jitter, reordering, duplication, and a bottleneck with a drop-tail queue. It works on
//!   a virtual clock, so a ten-second stream runs in well under a second and every run with
//!   the same seed is the same.
//! - [`SimPair`] wires a [`HostCore`] and a [`ClientCore`] through two links and runs them
//!   in virtual time, with no sockets and no threads.
//! - [`Relay`] is the same link model in real time on loopback UDP, between real sockets,
//!   for testing [`crate::HostSession`] and [`crate::ClientSession`] (and the host and client
//!   programs) under impairment. It can also rebind its client-facing side, which looks to
//!   the host like a NAT rebinding or a Wi-Fi roam.
//! - [`SyntheticEncoder`] makes frames of realistic sizes at a given rate, following the
//!   transport's bitrate target.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use quiver_stream_proto::{EncodedFrame, Micros, now_micros};

use crate::client::{ClientCore, ClientEvent, ClientOpts};
use crate::crypto::SessionKeys;
use crate::host::{HostCore, HostEvent, HostOpts};

/// A small, fast, seedable random source (SplitMix64).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && self.f64() < p
    }
}

/// How a link loses packets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Loss {
    None,
    /// Each packet independently.
    Random(f64),
    /// Gilbert–Elliott: a good and a bad state, each with its own loss rate, and the
    /// chances per packet of moving between them. The mean burst is `1 / bad_to_good`
    /// packets long.
    Bursty { good_to_bad: f64, bad_to_good: f64, loss_good: f64, loss_bad: f64 },
}

impl Loss {
    /// The long-run loss rate.
    pub fn mean(&self) -> f64 {
        match *self {
            Loss::None => 0.0,
            Loss::Random(p) => p,
            Loss::Bursty { good_to_bad, bad_to_good, loss_good, loss_bad } => {
                let bad = good_to_bad / (good_to_bad + bad_to_good);
                bad * loss_bad + (1.0 - bad) * loss_good
            }
        }
    }
}

/// One direction of a simulated path.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkParams {
    /// One-way propagation delay.
    pub delay_us: Micros,
    /// Extra delay, uniform in `0..jitter_us`, per packet. Packets stay in order unless
    /// `reorder` says otherwise.
    pub jitter_us: Micros,
    pub loss: Loss,
    /// The bottleneck's rate; None is unlimited.
    pub rate_bps: Option<u64>,
    /// The bottleneck's queue, in bytes, before it drops (drop-tail).
    pub queue_bytes: usize,
    /// The chance a packet is held back `reorder_us` and so arrives after later ones.
    pub reorder: f64,
    pub reorder_us: Micros,
    /// The chance a packet arrives twice.
    pub duplicate: f64,
}

impl Default for LinkParams {
    fn default() -> Self {
        Self {
            delay_us: 500,
            jitter_us: 0,
            loss: Loss::None,
            rate_bps: None,
            queue_bytes: 1 << 20,
            reorder: 0.0,
            reorder_us: 2_000,
            duplicate: 0.0,
        }
    }
}

impl LinkParams {
    /// A wired LAN: 0.5 ms each way, gigabit, a switch-sized queue.
    pub fn lan() -> Self {
        Self { rate_bps: Some(1_000_000_000), queue_bytes: 512 << 10, ..Self::default() }
    }

    /// Wi-Fi-ish: 2 ms each way, ±1.5 ms jitter, 1 % loss, 300 Mbit/s, a deep queue.
    pub fn wifi() -> Self {
        Self {
            delay_us: 2_000,
            jitter_us: 1_500,
            loss: Loss::Random(0.01),
            rate_bps: Some(300_000_000),
            queue_bytes: 2 << 20,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub sent: u64,
    pub delivered: u64,
    pub lost: u64,
    pub queue_drops: u64,
    pub duplicated: u64,
    pub reordered: u64,
    /// The longest any packet waited in the bottleneck queue.
    pub max_queue_us: Micros,
}

#[derive(Debug)]
pub struct Link {
    params: LinkParams,
    rng: Rng,
    bad: bool,
    /// When the bottleneck finishes what is queued.
    busy_until: Micros,
    /// The latest delivery time handed out, so jitter doesn't reorder.
    last_delivery: Micros,
    flight: BinaryHeap<Reverse<(Micros, u64, Vec<u8>)>>,
    order: u64,
    pub stats: LinkStats,
    queue_samples: Vec<Micros>,
}

impl Link {
    pub fn new(params: LinkParams, seed: u64) -> Self {
        Self {
            params,
            rng: Rng::new(seed),
            bad: false,
            busy_until: 0,
            last_delivery: 0,
            flight: BinaryHeap::new(),
            order: 0,
            stats: LinkStats::default(),
            queue_samples: Vec::new(),
        }
    }

    pub fn params(&self) -> &LinkParams {
        &self.params
    }

    /// Changes the link mid-run (a bottleneck that drops, a burst of loss).
    pub fn set_params(&mut self, params: LinkParams) {
        self.params = params;
    }

    fn lose(&mut self) -> bool {
        match self.params.loss {
            Loss::None => false,
            Loss::Random(p) => self.rng.chance(p),
            Loss::Bursty { good_to_bad, bad_to_good, loss_good, loss_bad } => {
                let flip = if self.bad { bad_to_good } else { good_to_bad };
                if self.rng.chance(flip) {
                    self.bad = !self.bad;
                }
                self.rng.chance(if self.bad { loss_bad } else { loss_good })
            }
        }
    }

    pub fn send(&mut self, now: Micros, data: Vec<u8>) {
        self.stats.sent += 1;
        let size = data.len() + 28;
        let mut at = now;
        if let Some(rate) = self.params.rate_bps {
            let start = self.busy_until.max(now);
            let backlog = ((start - now) as f64 * rate as f64 / 8e6) as usize;
            if backlog + size > self.params.queue_bytes {
                self.stats.queue_drops += 1;
                return;
            }
            self.busy_until = start + (size as f64 * 8e6 / rate as f64).ceil() as Micros;
            at = self.busy_until;
            let waited = start - now;
            self.stats.max_queue_us = self.stats.max_queue_us.max(waited);
            self.queue_samples.push(waited);
        }
        if self.lose() {
            self.stats.lost += 1;
            return;
        }
        let jitter = if self.params.jitter_us > 0 { self.rng.next_u64() % self.params.jitter_us } else { 0 };
        let mut deliver = (at + self.params.delay_us + jitter).max(self.last_delivery);
        self.last_delivery = deliver;
        if self.rng.chance(self.params.reorder) {
            deliver += self.params.reorder_us;
            self.stats.reordered += 1;
        }
        if self.rng.chance(self.params.duplicate) {
            self.stats.duplicated += 1;
            self.push(deliver + 1, data.clone());
        }
        self.push(deliver, data);
    }

    fn push(&mut self, at: Micros, data: Vec<u8>) {
        self.order += 1;
        self.flight.push(Reverse((at, self.order, data)));
    }

    /// The next packet due by `now`.
    pub fn poll(&mut self, now: Micros) -> Option<Vec<u8>> {
        if self.flight.peek().is_some_and(|Reverse((t, _, _))| *t <= now) {
            self.stats.delivered += 1;
            return self.flight.pop().map(|Reverse((_, _, d))| d);
        }
        None
    }

    pub fn next_delivery(&self) -> Option<Micros> {
        self.flight.peek().map(|Reverse((t, _, _))| *t)
    }

    /// Queueing delay percentiles so far (p50, p95), µs.
    pub fn queue_percentiles(&self) -> (Micros, Micros) {
        let mut v = self.queue_samples.clone();
        if v.is_empty() {
            return (0, 0);
        }
        v.sort_unstable();
        (v[v.len() / 2], v[v.len() * 95 / 100])
    }
}

// --- frames --------------------------------------------------------------------------------------

/// Makes frames the way an encoder would: about `bitrate / fps` bytes each, varying ±25 %,
/// with a keyframe of five times that every `keyframe_every` frames.
#[derive(Debug, Clone)]
pub struct SyntheticEncoder {
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub keyframe_every: Option<u64>,
    seq: u64,
    last_key: u64,
    rng: Rng,
    force_key: bool,
}

impl SyntheticEncoder {
    pub fn new(width: u32, height: u32, fps: u32, seed: u64) -> Self {
        Self { fps, width, height, keyframe_every: None, seq: 0, last_key: 0, rng: Rng::new(seed), force_key: true }
    }

    /// Starts numbering frames at `seq` (frame numbers never restart within a stream).
    pub fn starting_at(mut self, seq: u64) -> Self {
        self.seq = seq;
        self.last_key = seq;
        self
    }

    pub fn interval_us(&self) -> Micros {
        1_000_000 / Micros::from(self.fps.max(1))
    }

    /// The next frame will be a keyframe (as after `Control::Keyframe`).
    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    /// The next frame, sized for `kbps`, captured at `now`.
    pub fn next(&mut self, now: Micros, kbps: u32) -> EncodedFrame {
        let seq = self.seq;
        self.seq += 1;
        let key = self.force_key || self.keyframe_every.is_some_and(|n| seq.is_multiple_of(n.max(1)));
        self.force_key = false;
        if key {
            self.last_key = seq;
        }
        let mean = f64::from(kbps) * 1000.0 / 8.0 / f64::from(self.fps.max(1));
        let size = mean * (0.75 + 0.5 * self.rng.f64()) * if key { 5.0 } else { 1.0 };
        let size = (size as usize).max(64);
        let mut data = vec![0u8; size];
        // Annex-B-looking bytes: a start code and a counter, so content checks are easy.
        data[..4].copy_from_slice(&[0, 0, 0, 1]);
        data[4..12].copy_from_slice(&seq.to_be_bytes());
        EncodedFrame {
            seq,
            captured: now,
            encoded: now + 2_000,
            keyframe: key,
            refs_from_seq: if key { seq } else { self.last_key },
            width: self.width,
            height: self.height,
            data,
        }
    }
}

// --- the virtual-time harness ---------------------------------------------------------------------

/// A host and a client joined by two simulated links, in virtual time.
pub struct SimPair {
    pub host: HostCore,
    pub client: ClientCore,
    /// Host → client.
    pub down: Link,
    /// Client → host.
    pub up: Link,
    pub now: Micros,
    pub host_addr: SocketAddr,
    /// Where the client's datagrams appear to come from. Change it to simulate a roam;
    /// datagrams the host still sends to the old address are lost.
    pub client_addr: SocketAddr,
    pub host_events: Vec<(Micros, HostEvent)>,
    pub client_events: Vec<(Micros, ClientEvent)>,
    /// Datagrams the host sent to an address the client no longer has.
    pub misaddressed: u64,
    /// The host's clock runs this far ahead of the client's, µs (it may be negative), as
    /// two machines' monotonic clocks would.
    pub host_skew: i64,
    buf: Vec<u8>,
}

impl SimPair {
    pub fn new(keys: &SessionKeys, host: HostOpts, client: ClientOpts, down: LinkParams, up: LinkParams, seed: u64) -> Self {
        let host_addr: SocketAddr = "10.0.0.1:47998".parse().expect("address");
        let now = 1_000_000;
        Self {
            host: HostCore::new(keys, host, now),
            client: ClientCore::new(keys, host_addr, client, now),
            down: Link::new(down, seed),
            up: Link::new(up, seed.wrapping_add(1)),
            now,
            host_addr,
            client_addr: "10.0.0.2:50000".parse().expect("address"),
            host_events: Vec::new(),
            client_events: Vec::new(),
            misaddressed: 0,
            host_skew: 0,
            buf: Vec::with_capacity(2048),
        }
    }

    /// Runs until virtual time `until`, calling `tick` at each `every` µs boundary (to feed
    /// frames, input and so on).
    pub fn run(&mut self, until: Micros, every: Micros, mut tick: impl FnMut(&mut Self)) {
        let mut next_tick = self.now;
        while self.now < until {
            if self.now >= next_tick {
                tick(self);
                next_tick += every.max(1);
            }
            self.settle();
            let next = [
                Some(next_tick),
                Some(until),
                self.host.poll_timeout(self.host_now()).map(|t| self.from_host(t)),
                self.client.poll_timeout(self.now),
                self.down.next_delivery(),
                self.up.next_delivery(),
            ]
            .into_iter()
            .flatten()
            .min()
            .expect("until is always there");
            self.now = next.max(self.now + 1).min(until.max(self.now + 1));
        }
    }

    /// Now, on the host's clock.
    pub fn host_now(&self) -> Micros {
        (self.now as i64 + self.host_skew) as Micros
    }

    fn from_host(&self, t: Micros) -> Micros {
        (t as i64 - self.host_skew).max(0) as Micros
    }

    /// Does everything due at the current time.
    pub fn settle(&mut self) {
        let now = self.now;
        for _ in 0..100_000 {
            let mut busy = false;
            while let Some(mut d) = self.down.poll(now) {
                self.client.handle_datagram(now, self.host_addr, &mut d);
                busy = true;
            }
            while let Some(mut d) = self.up.poll(now) {
                self.host.handle_datagram(self.host_now(), self.client_addr, &mut d);
                busy = true;
            }
            let hnow = self.host_now();
            if self.host.poll_timeout(hnow).is_some_and(|t| t <= hnow) {
                self.host.handle_timeout(hnow);
            }
            if self.client.poll_timeout(now).is_some_and(|t| t <= now) {
                self.client.handle_timeout(now);
            }
            while let Some(t) = {
                self.buf.clear();
                self.host.poll_transmit(hnow, &mut self.buf)
            } {
                busy = true;
                if t.dest == self.client_addr {
                    self.down.send(now, self.buf[..t.len].to_vec());
                } else {
                    self.misaddressed += 1;
                }
            }
            while let Some(t) = {
                self.buf.clear();
                self.client.poll_transmit(now, &mut self.buf)
            } {
                busy = true;
                self.up.send(now, self.buf[..t.len].to_vec());
            }
            while let Some(e) = self.host.poll_event() {
                self.host_events.push((now, e));
            }
            while let Some(e) = self.client.poll_event() {
                self.client_events.push((now, e));
            }
            if !busy {
                return;
            }
        }
        panic!("the simulation didn't settle at t={now}");
    }

    /// Runs until the client is connected, or panics after 2 virtual seconds.
    pub fn connect(&mut self) {
        let deadline = self.now + 2_000_000;
        while !self.client.is_connected() {
            assert!(self.now < deadline, "the client didn't connect");
            let t = self.now + 1_000;
            self.run(t, 1_000, |_| {});
        }
    }
}

// --- the real-time relay ------------------------------------------------------------------------------

/// A UDP relay on loopback that applies a [`Link`] each way in real time.
///
/// The client sends to [`Relay::addr`]; the relay forwards to the server from its own
/// upstream socket and sends the server's replies back to wherever the client last sent
/// from. [`Relay::rebind`] swaps the upstream socket for a new one on a new port.
pub struct Relay {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<RelayShared>>,
    thread: Option<JoinHandle<()>>,
}

struct RelayShared {
    down: Link,
    up: Link,
    rebind: bool,
}

impl Relay {
    /// Starts relaying to `server`, with `down` (server → client) and `up` impairments.
    pub fn spawn(server: SocketAddr, down: LinkParams, up: LinkParams, seed: u64) -> io::Result<Self> {
        let front = UdpSocket::bind("127.0.0.1:0")?;
        let addr = front.local_addr()?;
        let back = UdpSocket::bind("127.0.0.1:0")?;
        front.set_nonblocking(true)?;
        back.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(RelayShared { down: Link::new(down, seed), up: Link::new(up, seed + 1), rebind: false }));
        let thread = {
            let (stop, shared) = (stop.clone(), shared.clone());
            std::thread::Builder::new().name("relay".into()).spawn(move || relay_loop(front, back, server, &stop, &shared))?
        };
        Ok(Self { addr, stop, shared, thread: Some(thread) })
    }

    /// Where clients should send.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// From now on, forward to the server from a new port, as a NAT that lost its mapping
    /// or a client that changed networks would.
    pub fn rebind(&self) {
        self.shared.lock().expect("relay lock").rebind = true;
    }

    pub fn set_down(&self, p: LinkParams) {
        self.shared.lock().expect("relay lock").down.set_params(p);
    }

    pub fn set_up(&self, p: LinkParams) {
        self.shared.lock().expect("relay lock").up.set_params(p);
    }

    /// `(down, up)` statistics.
    pub fn stats(&self) -> (LinkStats, LinkStats) {
        let s = self.shared.lock().expect("relay lock");
        (s.down.stats.clone(), s.up.stats.clone())
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn relay_loop(front: UdpSocket, mut back: UdpSocket, server: SocketAddr, stop: &AtomicBool, shared: &Mutex<RelayShared>) {
    let mut client: Option<SocketAddr> = None;
    let mut buf = vec![0u8; 65_536];
    while !stop.load(Ordering::Relaxed) {
        let now = now_micros();
        let mut s = shared.lock().expect("relay lock");
        if std::mem::take(&mut s.rebind) {
            if let Ok(b) = UdpSocket::bind("127.0.0.1:0") {
                let _ = b.set_nonblocking(true);
                back = b;
            }
        }
        while let Ok((n, from)) = front.recv_from(&mut buf) {
            client = Some(from);
            s.up.send(now, buf[..n].to_vec());
        }
        while let Ok((n, _)) = back.recv_from(&mut buf) {
            s.down.send(now, buf[..n].to_vec());
        }
        while let Some(d) = s.up.poll(now) {
            let _ = back.send_to(&d, server);
        }
        if let Some(c) = client {
            while let Some(d) = s.down.poll(now) {
                let _ = front.send_to(&d, c);
            }
        }
        let next = [s.up.next_delivery(), s.down.next_delivery()].into_iter().flatten().min();
        drop(s);
        let wait = next.map_or(200, |t| t.saturating_sub(now_micros()).min(200));
        if wait > 0 {
            std::thread::sleep(Duration::from_micros(wait));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_loss_matches_its_rate() {
        let mut l = Link::new(LinkParams { loss: Loss::Random(0.1), ..Default::default() }, 1);
        for i in 0..20_000 {
            l.send(i, vec![0; 100]);
        }
        let rate = l.stats.lost as f64 / 20_000.0;
        assert!((rate - 0.1).abs() < 0.01, "{rate}");
    }

    #[test]
    fn bursty_loss_comes_in_runs() {
        let loss = Loss::Bursty { good_to_bad: 0.01, bad_to_good: 0.25, loss_good: 0.0, loss_bad: 1.0 };
        let mut l = Link::new(LinkParams { loss, ..Default::default() }, 2);
        let mut runs = Vec::new();
        let mut run = 0;
        for i in 0..100_000 {
            let lost_before = l.stats.lost;
            l.send(i, vec![0; 10]);
            if l.stats.lost > lost_before {
                run += 1;
            } else if run > 0 {
                runs.push(run);
                run = 0;
            }
        }
        let mean_run = runs.iter().sum::<u64>() as f64 / runs.len() as f64;
        assert!((mean_run - 4.0).abs() < 0.5, "mean burst {mean_run}");
        let rate = l.stats.lost as f64 / 100_000.0;
        assert!((rate - loss.mean()).abs() < 0.01, "{rate} vs {}", loss.mean());
    }

    #[test]
    fn the_bottleneck_queues_then_drops() {
        // 8 Mbit/s: a 1000-byte packet takes 1 ms; the queue holds about 5.
        let p = LinkParams { delay_us: 0, rate_bps: Some(8_000_000), queue_bytes: 6_200, ..Default::default() };
        let mut l = Link::new(p, 3);
        for _ in 0..10 {
            l.send(0, vec![0; 972]);
        }
        assert_eq!(l.stats.queue_drops, 4);
        let times: Vec<_> = std::iter::from_fn(|| {
            let t = l.next_delivery()?;
            l.poll(t).map(|_| t)
        })
        .collect();
        assert_eq!(times, vec![1000, 2000, 3000, 4000, 5000, 6000]);
    }

    #[test]
    fn reordering_and_duplication_happen() {
        let p = LinkParams { reorder: 0.2, duplicate: 0.1, ..Default::default() };
        let mut l = Link::new(p, 4);
        for i in 0..1000u64 {
            l.send(i * 10, i.to_be_bytes().to_vec());
        }
        let mut got = Vec::new();
        while let Some(t) = l.next_delivery() {
            got.push(u64::from_be_bytes(l.poll(t).unwrap().try_into().unwrap()));
        }
        assert!(got.len() > 1050);
        assert!(got.windows(2).any(|w| w[1] < w[0]));
    }
}
