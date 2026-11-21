//! End-to-end behaviour of the host and client cores over simulated links, in virtual time.

use quiver_stream_net::sim::{LinkParams, Loss, SimPair, SyntheticEncoder};
use quiver_stream_net::{ClientEvent, ClientOpts, HostEvent, HostOpts, SessionKeys, VideoEvent};
use quiver_stream_proto::{Control, CursorImage, InputEvent, Micros};

const SEC: Micros = 1_000_000;

fn keys() -> SessionKeys {
    SessionKeys::new(42, [7; 32])
}

fn host_opts(max_kbps: u32) -> HostOpts {
    HostOpts { max_kbps, start_kbps: max_kbps.min(20_000), ..HostOpts::default() }
}

fn no_nacks() -> ClientOpts {
    ClientOpts { nack_max_wait_us: None, ..ClientOpts::default() }
}

/// What the client saw.
#[derive(Debug, Default)]
struct Seen {
    frames: Vec<u64>,
    gaps: Vec<(u64, u64)>,
    /// Capture (on the client's clock) to whole frame, µs.
    latency: Vec<Micros>,
}

impl Seen {
    fn of(pair: &SimPair) -> Self {
        let mut s = Seen::default();
        for (at, e) in &pair.client_events {
            match e {
                ClientEvent::Video(VideoEvent::Frame(f)) => {
                    s.frames.push(f.seq);
                    let captured_local = (f.captured as i64 - pair.host_skew) as Micros;
                    s.latency.push(at.saturating_sub(captured_local));
                }
                ClientEvent::Video(VideoEvent::Gap { since_seq, through_seq }) => s.gaps.push((*since_seq, *through_seq)),
                _ => {}
            }
        }
        s
    }

    fn lost(&self) -> u64 {
        self.gaps.iter().map(|(a, b)| b - a + 1).sum()
    }

    fn percentile(&self, p: f64) -> Micros {
        let mut v = self.latency.clone();
        v.sort_unstable();
        v.get(((v.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0)
    }

    /// Every frame number appears once, as a frame or inside a gap, in order.
    fn assert_complete_and_ordered(&self) {
        let mut all: Vec<(u64, u64)> = self.frames.iter().map(|s| (*s, *s)).chain(self.gaps.iter().copied()).collect();
        all.sort_unstable();
        for w in all.windows(2) {
            assert_eq!(w[0].1 + 1, w[1].0, "frame numbers skipped or repeated around {w:?}");
        }
        assert!(self.frames.windows(2).all(|w| w[0] < w[1]), "frames out of order");
    }
}

/// Streams 1440p frames at `fps`, following the host's bitrate target, until `until`.
/// `each` runs every frame, before it is sent.
fn stream(pair: &mut SimPair, fps: u32, until: Micros, each: impl FnMut(&mut SimPair)) -> Vec<(Micros, u32)> {
    stream_from(pair, &mut SyntheticEncoder::new(2560, 1440, fps, 9), until, each)
}

fn stream_from(pair: &mut SimPair, enc: &mut SyntheticEncoder, until: Micros, mut each: impl FnMut(&mut SimPair)) -> Vec<(Micros, u32)> {
    let mut targets = Vec::new();
    let interval = enc.interval_us();
    pair.run(until, interval, |p| {
        each(p);
        let now = p.host_now();
        let kbps = p.host.encoder_target_kbps(now);
        targets.push((p.now, kbps));
        let frame = enc.next(now, kbps);
        p.host.send_video(frame, now).unwrap();
    });
    targets
}

fn client_nacks(name: &str) -> bool {
    name.contains("NACK")
}

fn lossy(loss: Loss) -> LinkParams {
    LinkParams { delay_us: 1_000, loss, ..LinkParams::lan() }
}

#[test]
fn two_percent_random_loss_loses_no_frames() {
    let fec_only = HostOpts { answer_nacks: false, ..host_opts(50_000) };
    for (name, host, client) in [("FEC + NACK", host_opts(50_000), ClientOpts::default()), ("FEC only", fec_only, no_nacks())] {
        let mut pair = SimPair::new(&keys(), host, client, lossy(Loss::Random(0.02)), lossy(Loss::Random(0.02)), 1);
        pair.connect();
        let end = pair.now + 10 * SEC;
        stream(&mut pair, 120, end, |_| {});
        let seen = Seen::of(&pair);
        seen.assert_complete_and_ordered();
        let stats = pair.client.net_stats();
        eprintln!(
            "{name}: {} frames, {} lost, {} shards rebuilt, {} NACKs ({} saved), FEC {:.1}%, p50 {} µs, p99 {} µs",
            seen.frames.len(),
            seen.lost(),
            stats.shards_recovered,
            stats.nacks,
            stats.nack_saves,
            pair.host.stats(pair.host_now()).fec_ratio * 100.0,
            seen.percentile(0.5),
            seen.percentile(0.99),
        );
        assert!(seen.frames.len() > 1150, "{name}: only {} frames", seen.frames.len());
        // FEC alone is planned for one frame in 10^4 lost; NACKs pick up the rest.
        let allowed = if client_nacks(name) { 0 } else { 1 };
        assert!(seen.lost() <= allowed, "{name}: frames lost at 2% loss: {:?}", seen.gaps);
        assert!(stats.shards_recovered > 100, "FEC did the work");
    }
}

#[test]
fn ten_percent_loss_loses_some_frames_and_recovers() {
    let mut pair = SimPair::new(&keys(), host_opts(30_000), no_nacks(), lossy(Loss::Random(0.10)), lossy(Loss::Random(0.10)), 2);
    pair.connect();
    let lossy_until = pair.now + 6 * SEC;
    let end = lossy_until + 4 * SEC;
    let mut healed = false;
    stream(&mut pair, 120, end, |p| {
        if !healed && p.now >= lossy_until {
            healed = true;
            p.down.set_params(lossy(Loss::None));
            p.up.set_params(lossy(Loss::None));
        }
    });
    let seen = Seen::of(&pair);
    seen.assert_complete_and_ordered();
    let total = seen.frames.len() as u64 + seen.lost();
    eprintln!("10% loss: {} of {total} frames lost, in {} gaps", seen.lost(), seen.gaps.len());
    assert!(seen.lost() * 10 < total, "more than a tenth of the frames lost");
    // Once the loss stops, frames flow with no further gaps (allow a second to settle).
    let last_gap_at = pair.client_events.iter().filter(|(_, e)| matches!(e, ClientEvent::Video(VideoEvent::Gap { .. }))).map(|(t, _)| *t).max();
    if let Some(t) = last_gap_at {
        assert!(t < lossy_until + SEC, "a gap {} ms after the loss stopped", (t - lossy_until) / 1000);
    }
    let after = pair.client_events.iter().filter(|(t, e)| *t > lossy_until + SEC && matches!(e, ClientEvent::Video(VideoEvent::Frame(_)))).count();
    assert!(after > 300, "only {after} frames after recovery");
}

#[test]
fn nacks_rescue_frames_fec_cannot_at_low_rtt() {
    let run = |client: ClientOpts| {
        let mut pair = SimPair::new(&keys(), host_opts(30_000), client, lossy(Loss::Random(0.10)), lossy(Loss::Random(0.10)), 3);
        pair.connect();
        let end = pair.now + 6 * SEC;
        stream(&mut pair, 120, end, |_| {});
        let seen = Seen::of(&pair);
        seen.assert_complete_and_ordered();
        (seen.lost(), seen.percentile(0.5), seen.percentile(0.99), pair.client.net_stats().nack_saves)
    };
    let (lost_fec, p50_fec, p99_fec, _) = run(no_nacks());
    let (lost_nack, p50_nack, p99_nack, saves) = run(ClientOpts::default());
    eprintln!(
        "10% loss, 2 ms RTT: FEC only {lost_fec} lost (p50 {p50_fec} µs, p99 {p99_fec} µs); with NACK {lost_nack} lost, {saves} saved (p50 {p50_nack} µs, p99 {p99_nack} µs)"
    );
    assert!(lost_nack <= lost_fec);
}

#[test]
fn bursty_loss_is_mostly_absorbed() {
    // Gilbert–Elliott: 2% average loss in bursts of about 4 packets.
    let loss = Loss::Bursty { good_to_bad: 0.005, bad_to_good: 0.25, loss_good: 0.0, loss_bad: 1.0 };
    let mut pair = SimPair::new(&keys(), host_opts(50_000), ClientOpts::default(), lossy(loss), lossy(loss), 4);
    pair.connect();
    let end = pair.now + 10 * SEC;
    stream(&mut pair, 120, end, |_| {});
    let seen = Seen::of(&pair);
    seen.assert_complete_and_ordered();
    eprintln!("bursty {:.1}% loss: {} of {} frames lost", loss.mean() * 100.0, seen.lost(), seen.frames.len());
    assert!(seen.lost() * 200 < seen.frames.len() as u64, "more than 0.5% of frames lost");
}

#[test]
fn bitrate_converges_under_a_cap_and_backs_off_when_it_drops() {
    let capped = |mbit: u64| LinkParams {
        delay_us: 2_000,
        rate_bps: Some(mbit * 1_000_000),
        // About 50 ms of queue at the starting cap: room for bufferbloat if we let it.
        queue_bytes: (40 * 1_000_000 / 8) / 20,
        ..LinkParams::default()
    };
    let mut pair = SimPair::new(&keys(), host_opts(100_000), ClientOpts::default(), capped(40), LinkParams::default(), 5);
    pair.connect();
    let t0 = pair.now;
    let drop_at = t0 + 15 * SEC;
    let end = drop_at + 5 * SEC;
    let mut dropped = false;
    let targets = stream(&mut pair, 120, end, |p| {
        if !dropped && p.now >= drop_at {
            dropped = true;
            p.down.set_params(capped(15));
        }
    });
    let mean = |from: Micros, to: Micros| {
        let v: Vec<f64> = targets.iter().filter(|(t, _)| *t >= from && *t < to).map(|(_, k)| f64::from(*k)).collect();
        v.iter().sum::<f64>() / v.len() as f64
    };
    let settled = mean(t0 + 8 * SEC, drop_at);
    let first_under = targets.iter().find(|(t, k)| *t >= drop_at && *k < 15_000).map(|(t, _)| t - drop_at);
    let after = mean(drop_at + 2 * SEC, end);
    let (q50, q95) = pair.down.queue_percentiles();
    let seen = Seen::of(&pair);
    eprintln!(
        "40 Mbit/s cap: encoder target {settled:.0} kbps; after the drop to 15: under it in {:?} ms, then {after:.0} kbps; queue p50 {q50} µs p95 {q95} µs; frame p50 {} µs p99 {} µs; {} frames lost",
        first_under.map(|t| t / 1000),
        seen.percentile(0.5),
        seen.percentile(0.99),
        seen.lost()
    );
    assert!(settled > 0.6 * 40_000.0 && settled < 40_000.0, "settled at {settled} kbps under a 40 Mbit/s cap");
    assert!(first_under.is_some_and(|t| t < 1_200_000), "took {first_under:?} µs to get under the new cap");
    assert!(after > 0.5 * 15_000.0 && after < 15_000.0, "{after} kbps under a 15 Mbit/s cap");
    assert!(q50 < 5_000, "the bottleneck queue sat at {q50} µs (bufferbloat)");
}

#[test]
fn probing_reaches_a_fast_link_quickly() {
    let mut pair = SimPair::new(&keys(), host_opts(150_000), ClientOpts::default(), LinkParams::lan(), LinkParams::lan(), 6);
    pair.connect();
    let t0 = pair.now;
    let targets = stream(&mut pair, 120, t0 + 3 * SEC, |_| {});
    let reached = targets.iter().find(|(_, k)| *k > 100_000).map(|(t, _)| t - t0);
    eprintln!("gigabit LAN, 150 Mbit/s max: encoder target over 100 Mbit/s after {:?} ms; probes {:?}", reached.map(|t| t / 1000), pair.host.stats(pair.host_now()).probes);
    assert!(reached.is_some_and(|t| t < SEC), "took {reached:?} µs");
}

#[test]
fn the_client_can_roam_to_a_new_address() {
    let mut pair = SimPair::new(&keys(), host_opts(30_000), ClientOpts::default(), lossy(Loss::None), lossy(Loss::None), 7);
    pair.connect();
    let roam_at = pair.now + 2 * SEC;
    let end = roam_at + 3 * SEC;
    let mut roamed = false;
    stream(&mut pair, 120, end, |p| {
        if !roamed && p.now >= roam_at {
            roamed = true;
            p.client_addr = "192.168.7.9:41000".parse().unwrap();
        }
    });
    let migrated = pair.host_events.iter().find_map(|(t, e)| matches!(e, HostEvent::Migrated { .. }).then_some(*t));
    let seen = Seen::of(&pair);
    seen.assert_complete_and_ordered();
    eprintln!(
        "roam: migrated {:?} ms after the address changed, {} datagrams went to the old address, {} frames lost",
        migrated.map(|t| (t - roam_at) / 1000),
        pair.misaddressed,
        seen.lost()
    );
    assert!(migrated.is_some_and(|t| t - roam_at < 100_000), "migration took too long: {migrated:?}");
    assert_eq!(pair.host.peer(), Some(pair.client_addr));
    let late = pair.client_events.iter().filter(|(t, e)| *t > roam_at + SEC && matches!(e, ClientEvent::Video(VideoEvent::Frame(_)))).count();
    assert!(late > 200, "{late} frames after the roam");
    assert!(seen.lost() < 10, "{} frames lost around the roam", seen.lost());
}

#[test]
fn replayed_and_forged_datagrams_are_dropped() {
    let mut pair = SimPair::new(&keys(), host_opts(20_000), ClientOpts::default(), LinkParams::default(), LinkParams::default(), 8);
    pair.connect();
    let now = pair.now;
    pair.client.send_input(InputEvent::Key { code: 30, down: true }, now, now);
    let mut buf = Vec::new();
    let t = pair.client.poll_transmit(now, &mut buf).expect("an input datagram");
    let datagram = buf[..t.len].to_vec();
    let addr = pair.client_addr;
    let mut first = datagram.clone();
    pair.host.handle_datagram(now, addr, &mut first);
    for _ in 0..3 {
        let mut again = datagram.clone();
        pair.host.handle_datagram(now, addr, &mut again);
    }
    // A second, fresh datagram, tampered with in flight from another address: the AEAD
    // refuses it, and it neither moves the path nor burns its packet number.
    pair.client.send_input(InputEvent::Key { code: 30, down: false }, now, now);
    buf.clear();
    let t = pair.client.poll_transmit(now, &mut buf).expect("a second input datagram");
    let second = buf[..t.len].to_vec();
    let mut forged = second.clone();
    let mid = forged.len() / 2;
    forged[mid] ^= 0x40;
    pair.host.handle_datagram(now, "10.6.6.6:6".parse().unwrap(), &mut forged);
    // A datagram under another session's keys, with a packet number not yet used here.
    let mut other = quiver_stream_net::ClientCore::new(&SessionKeys::new(42, [8; 32]), pair.host_addr, ClientOpts::default(), now);
    let mut wrong_key = Vec::new();
    for i in 0..60 {
        let mut b = Vec::new();
        if let Some(t) = other.poll_transmit(now + i * 300_000, &mut b) {
            wrong_key = b[..t.len].to_vec();
        }
    }
    assert!(pair.host.stats(now).packets_received < 50, "the other session's packet number is ahead");
    pair.host.handle_datagram(now, "10.9.9.9:1".parse().unwrap(), &mut wrong_key);
    let stats = pair.host.stats(now);
    assert_eq!(stats.replayed, 3, "the three copies");
    assert_eq!(stats.rejected, 2, "the forgery and the other session's datagram");
    // The untouched second datagram still gets in.
    let mut real = second;
    pair.host.handle_datagram(now, addr, &mut real);
    let inputs: Vec<_> = std::iter::from_fn(|| pair.host.poll_event()).filter(|e| matches!(e, HostEvent::Input(_))).collect();
    assert_eq!(inputs.len(), 2, "each key event applied once");
    pair.run(now + 200_000, 10_000, |_| {});
    assert_eq!(pair.host.peer(), Some(addr), "nothing moved the path");
    assert!(!pair.host_events.iter().any(|(_, e)| matches!(e, HostEvent::Migrated { .. })));
}

#[test]
fn input_is_applied_exactly_once_in_order_under_loss() {
    let rough = LinkParams { delay_us: 3_000, jitter_us: 2_000, loss: Loss::Random(0.2), reorder: 0.05, duplicate: 0.05, ..LinkParams::default() };
    let mut pair = SimPair::new(&keys(), host_opts(10_000), ClientOpts::default(), rough.clone(), rough, 9);
    pair.connect();
    let mut n = 0u16;
    let end = pair.now + 5 * SEC;
    // A 1 kHz mouse plus key presses, with no video, so input rides alone.
    pair.run(end, 1_000, |p| {
        n += 1;
        let e = if n.is_multiple_of(10) { InputEvent::Key { code: n, down: true } } else { InputEvent::MouseMotion { dx: i32::from(n), dy: 1 } };
        let now = p.now;
        p.client.send_input(e, now, now);
    });
    let drain_until = pair.now + 2 * SEC;
    pair.run(drain_until, 10_000, |_| {});
    let seqs: Vec<u64> = pair.host_events.iter().filter_map(|(_, e)| if let HostEvent::Input(s) = e { Some(s.seq) } else { None }).collect();
    let expected: Vec<u64> = (1..=u64::from(n)).collect();
    let stats = pair.client.net_stats();
    eprintln!("input at 20% loss: {} events, {} resends, {} duplicates dropped at the host", seqs.len(), stats.input_resends, pair.host.stats(pair.host_now()).input_duplicates);
    assert_eq!(seqs, expected);
}

#[test]
fn control_messages_arrive_in_order_under_loss() {
    let rough = LinkParams { delay_us: 2_000, loss: Loss::Random(0.1), reorder: 0.05, ..LinkParams::default() };
    let mut pair = SimPair::new(&keys(), host_opts(10_000), ClientOpts::default(), rough.clone(), rough, 10);
    pair.connect();
    let cursor = Control::Cursor {
        shape: Some(CursorImage { width: 256, height: 256, hot_x: 1, hot_y: 2, rgba: vec![200; 256 * 256 * 4], relative: false }),
    };
    pair.host.send_control(&Control::Notice { text: "one".into() });
    pair.host.send_control(&cursor);
    pair.host.send_control(&Control::Notice { text: "three".into() });
    pair.client.send_control(&Control::Keyframe);
    let end = pair.now + 5 * SEC;
    pair.run(end, 10_000, |_| {});
    let got: Vec<_> = pair.client_events.iter().filter_map(|(_, e)| if let ClientEvent::Control(c) = e { Some(c.clone()) } else { None }).collect();
    assert_eq!(got, vec![Control::Notice { text: "one".into() }, cursor, Control::Notice { text: "three".into() }]);
    assert!(pair.host_events.iter().any(|(_, e)| *e == HostEvent::Control(Control::Keyframe)));
}

#[test]
fn audio_survives_loss_with_redundancy() {
    let mut pair = SimPair::new(&keys(), host_opts(10_000), ClientOpts::default(), lossy(Loss::Random(0.05)), lossy(Loss::None), 11);
    pair.connect();
    let mut seq = 0;
    let end = pair.now + 4 * SEC;
    pair.run(end, 5_000, |p| {
        seq += 1;
        let now = p.host_now();
        p.host.send_audio(quiver_stream_proto::AudioPacket { seq, captured: now, mic: false, data: vec![1; 160] }, now);
    });
    let mut got: Vec<u64> = pair.client_events.iter().filter_map(|(_, e)| if let ClientEvent::Audio(a) = e { Some(a.seq) } else { None }).collect();
    let n = got.len();
    got.sort_unstable();
    got.dedup();
    assert_eq!(got.len(), n, "a packet came up twice");
    let missing = seq as usize - n;
    eprintln!("audio at 5% loss: {missing} of {seq} packets missing after redundancy");
    assert!(missing * 500 < seq as usize, "{missing} of {seq} missing");
}

#[test]
fn the_clock_offset_is_learned() {
    let mut pair = SimPair::new(&keys(), host_opts(10_000), ClientOpts::default(), LinkParams { delay_us: 3_000, jitter_us: 1_000, ..LinkParams::default() }, LinkParams { delay_us: 3_000, jitter_us: 1_000, ..LinkParams::default() }, 12);
    pair.host_skew = 123_456_789;
    pair.connect();
    let end = pair.now + 3 * SEC;
    pair.run(end, 10_000, |_| {});
    let off = pair.client.clock_offset().expect("an offset");
    let rtt = pair.client.rtt_us().unwrap();
    eprintln!("clock: offset error {} µs, rtt {rtt} µs", off - pair.host_skew);
    assert!((off - pair.host_skew).abs() < 600, "offset {off} vs {}", pair.host_skew);
    assert!((6_000..8_500).contains(&rtt), "rtt {rtt}");
}

#[test]
fn stats_cover_the_period() {
    let mut pair = SimPair::new(&keys(), host_opts(20_000), ClientOpts::default(), lossy(Loss::Random(0.01)), lossy(Loss::None), 13);
    pair.connect();
    let mut enc = SyntheticEncoder::new(1920, 1080, 60, 1);
    let end = pair.now + 2 * SEC;
    stream_from(&mut pair, &mut enc, end, |_| {});
    let first = pair.client.take_stats();
    let end = pair.now + SEC;
    stream_from(&mut pair, &mut enc, end, |_| {});
    let second = pair.client.take_stats();
    assert!(first.frames_received > 100);
    assert!((50..=70).contains(&second.frames_received), "{second:?}");
    assert!(second.packets_received > 100);
    assert!(second.rtt_us > 0 && second.network_us > 0 && second.estimated_kbps > 0, "{second:?}");
}
