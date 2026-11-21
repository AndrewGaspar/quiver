//! `cargo bench -p quiver-stream-net`: the transport's costs on this machine.
//!
//! 1. AEAD: AES-256-GCM against ChaCha20-Poly1305, sealing and opening full datagrams.
//! 2. FEC: encoding and decoding a 1440p120 frame at 150 Mbit/s with 2 %-loss recovery.
//! 3. Loopback throughput: a host and a client session streaming 150 Mbit/s of 1440p120
//!    frames; packets a second, and CPU per thread and per Mbit/s.
//! 4. Added latency per frame: send to whole-frame-at-the-client on loopback, for several
//!    pacing fractions, P-frames and keyframes apart.
//!
//! `QUIVER_BENCH_SECONDS` sets the throughput run's length (default 10).

use std::time::{Duration, Instant};

use quiver_stream_net::crypto::{Cipher, Direction, PacketKey};
use quiver_stream_net::fec::{self, FecPlanner};
use quiver_stream_net::sim::SyntheticEncoder;
use quiver_stream_net::{ClientOpts, ClientSession, HostOpts, HostSession, SessionKeys, VideoEvent};
use quiver_stream_proto::now_micros;

fn main() {
    // `cargo bench` passes --bench; ignore arguments.
    aead();
    fec_cost();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let secs = std::env::var("QUIVER_BENCH_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    rt.block_on(throughput(150_000, secs));
    rt.block_on(throughput(600_000, secs.min(5)));
    for fraction in [0.1, 0.25, 0.5, 1.0] {
        rt.block_on(frame_latency(fraction));
    }
}

fn aead() {
    println!("== AEAD, 1472-byte datagrams");
    let keys = SessionKeys::random();
    for cipher in [Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305] {
        let tx = PacketKey::new(&keys, Direction::HostToClient, cipher);
        let rx = PacketKey::new(&keys, Direction::HostToClient, cipher);
        let body = vec![7u8; 1472 - 26];
        let n = 200_000u64;
        let mut buf = Vec::with_capacity(1500);
        let start = Instant::now();
        for pn in 1..=n {
            buf.clear();
            tx.write_header(&mut buf, pn);
            buf.extend_from_slice(&body);
            tx.seal(&mut buf, 0);
        }
        let seal = start.elapsed();
        let mut sealed = buf.clone();
        let start = Instant::now();
        for _ in 0..n {
            sealed.copy_from_slice(&buf);
            std::hint::black_box(rx.open(&mut sealed));
        }
        let open = start.elapsed();
        let gbps = |d: Duration| n as f64 * 1472.0 * 8.0 / d.as_secs_f64() / 1e9;
        println!(
            "  {cipher:?}: seal {:.0} ns ({:.1} Gbit/s), open {:.0} ns ({:.1} Gbit/s)",
            seal.as_nanos() as f64 / n as f64,
            gbps(seal),
            open.as_nanos() as f64 / n as f64,
            gbps(open)
        );
    }
}

fn fec_cost() {
    println!("== FEC, 1440p120 at 150 Mbit/s (156 kB frames)");
    let len = 150_000_000 / 8 / 120;
    let (k, shard) = fec::layout(len, 1424);
    let mut planner = FecPlanner::new(2e-3, 0.5, 1);
    planner.set_loss(0.02 * 1.25 + 0.002);
    let r = planner.parity_for(k);
    let data = vec![0x5au8; k * shard];
    let mut enc = None;
    let n = 500;
    let start = Instant::now();
    let mut parity = Vec::new();
    for _ in 0..n {
        parity = fec::encode(&mut enc, &data, k, r, shard);
    }
    let e = start.elapsed() / n;
    let mut dec = None;
    let mut have = vec![true; k + r];
    for i in 0..r {
        have[i * 3] = false;
    }
    let full = [data.clone(), parity].concat();
    let start = Instant::now();
    for _ in 0..n {
        let mut buf = full.clone();
        fec::recover(&mut dec, &mut buf, &have, k, r, shard).expect("recoverable");
    }
    let d = start.elapsed() / n;
    println!(
        "  k = {k}, r = {r} ({:.1} % overhead): encode {} µs a frame ({:.1} % of a core at 120 fps), rebuild {r} lost shards {} µs",
        r as f64 / (k + r) as f64 * 100.0,
        e.as_micros(),
        e.as_secs_f64() * 120.0 * 100.0,
        d.as_micros()
    );
}

/// CPU seconds used so far by this process's threads whose name starts with `prefix`.
fn thread_cpu(prefix: &str) -> f64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let mut total = 0.0;
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else { return 0.0 };
    for entry in dir.flatten() {
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
        if !stat[open + 1..close].starts_with(prefix) {
            continue;
        }
        let fields: Vec<&str> = stat[close + 2..].split(' ').collect();
        let utime: f64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let stime: f64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        total += (utime + stime) / ticks;
    }
    total
}

async fn connect(opts: HostOpts) -> (HostSession, ClientSession) {
    let keys = SessionKeys::random();
    let host = HostSession::bind("127.0.0.1:0".parse().unwrap(), keys.clone(), opts).await.expect("bind");
    let client = ClientSession::connect(host.local_addr(), keys, ClientOpts::default()).await.expect("connect");
    host.wait_connected().await.expect("connected");
    // Let PMTU probing and the start probes finish.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    (host, client)
}

async fn throughput(kbps: u32, secs: u64) {
    println!("== loopback, 1440p120 at {} Mbit/s for {secs} s", kbps / 1000);
    let (host, client) = connect(HostOpts { max_kbps: kbps * 2, start_kbps: kbps, ..HostOpts::default() }).await;
    let drain = {
        let client = client.clone();
        tokio::spawn(async move {
            let (mut frames, mut gaps) = (0u64, 0u64);
            while let Some(v) = client.recv_video().await {
                match v {
                    VideoEvent::Frame(_) => frames += 1,
                    VideoEvent::Gap { .. } => gaps += 1,
                }
            }
            (frames, gaps)
        })
    };
    let before = host.stats().await.expect("stats");
    let (h0, c0) = (thread_cpu("stream-net-host"), thread_cpu("stream-net-clie"));
    let t0 = Instant::now();
    let mut enc = SyntheticEncoder::new(2560, 1440, 120, 3);
    let mut tick = tokio::time::interval(Duration::from_micros(enc.interval_us()));
    while t0.elapsed() < Duration::from_secs(secs) {
        tick.tick().await;
        host.send_video(enc.next(now_micros(), kbps)).expect("send");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let elapsed = t0.elapsed().as_secs_f64();
    let after = host.stats().await.expect("stats");
    let (h1, c1) = (thread_cpu("stream-net-host"), thread_cpu("stream-net-clie"));
    let net = client.net_stats().await.expect("stats");
    client.close("bench done");
    host.close("bench done");
    let (frames, gaps) = drain.await.unwrap_or_default();
    let packets = (after.packets_sent - before.packets_sent) as f64 / elapsed;
    let mbit = (after.bytes_sent - before.bytes_sent) as f64 * 8.0 / elapsed / 1e6;
    let (hc, cc) = ((h1 - h0) / elapsed * 100.0, (c1 - c0) / elapsed * 100.0);
    println!(
        "  {packets:.0} datagrams/s, {mbit:.0} Mbit/s on the wire ({} B datagrams, FEC {:.1} %); {frames} frames, {gaps} gaps, {} shards lost",
        after.datagram_bytes,
        after.fec_ratio * 100.0,
        net.packets_lost
    );
    println!(
        "  CPU: host thread {hc:.1} %, client thread {cc:.1} % of a core; {:.3} % and {:.3} % of a core per Mbit/s",
        hc / mbit,
        cc / mbit
    );
}

async fn frame_latency(fraction: f32) {
    let kbps = 150_000;
    let opts = HostOpts { max_kbps: kbps * 2, start_kbps: kbps, pacing_fraction: fraction, probe: false, ..HostOpts::default() };
    let (host, client) = connect(opts).await;
    let recv = {
        let client = client.clone();
        tokio::spawn(async move {
            let mut p = Vec::new();
            let mut key = Vec::new();
            while let Some(v) = client.recv_video().await {
                if let VideoEvent::Frame(f) = v {
                    let us = now_micros().saturating_sub(f.captured);
                    if f.keyframe { key.push(us) } else { p.push(us) }
                }
            }
            (p, key)
        })
    };
    let mut enc = SyntheticEncoder::new(2560, 1440, 120, 4);
    enc.keyframe_every = Some(60);
    let mut tick = tokio::time::interval(Duration::from_micros(enc.interval_us()));
    for _ in 0..600 {
        tick.tick().await;
        host.send_video(enc.next(now_micros(), kbps)).expect("send");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.close("bench done");
    host.close("bench done");
    let (mut p, mut key) = recv.await.unwrap_or_default();
    let pct = |v: &mut Vec<u64>, q: f64| {
        v.sort_unstable();
        v.get(((v.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0)
    };
    println!(
        "== frame latency, 1440p120 at 150 Mbit/s, pacing {fraction}: P-frames p50 {} µs p99 {} µs; keyframes (5×) p50 {} µs p99 {} µs",
        pct(&mut p, 0.5),
        pct(&mut p, 0.99),
        pct(&mut key, 0.5),
        pct(&mut key, 0.99)
    );
}
