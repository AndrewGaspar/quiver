//! The async sessions over real UDP sockets on loopback, with and without the relay's
//! impairments.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use quiver_stream_net::sim::{LinkParams, Loss, Relay, SyntheticEncoder};
use quiver_stream_net::{ClientOpts, ClientSession, HostOpts, HostSession, LinkState, SessionKeys, StatsReport, VideoEvent};
use quiver_stream_proto::{Control, InputEvent, now_micros};
use tokio::time::timeout;

fn localhost() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

async fn pair(server: Option<SocketAddr>, host_opts: HostOpts) -> (HostSession, ClientSession, SessionKeys) {
    let keys = SessionKeys::random();
    let host = HostSession::bind(localhost(), keys.clone(), host_opts).await.unwrap();
    let to = server.unwrap_or(host.local_addr());
    let client = timeout(Duration::from_secs(5), ClientSession::connect(to, keys.clone(), ClientOpts::default())).await.unwrap().unwrap();
    timeout(Duration::from_secs(5), host.wait_connected()).await.unwrap().unwrap();
    (host, client, keys)
}

/// Sends `n` frames at `fps` from a task; returns how many frames and gaps the client saw.
async fn stream(host: &HostSession, client: &ClientSession, first: u64, n: u64, fps: u32) -> (Vec<u64>, u64) {
    let h = host.clone();
    let sender = tokio::spawn(async move {
        let mut enc = SyntheticEncoder::new(2560, 1440, fps, 1).starting_at(first);
        let mut tick = tokio::time::interval(Duration::from_micros(enc.interval_us()));
        let target = h.target_bitrate();
        for _ in 0..n {
            tick.tick().await;
            let kbps = *target.borrow();
            h.send_video(enc.next(now_micros(), kbps)).unwrap();
        }
    });
    let mut frames = Vec::new();
    let mut lost = 0;
    while frames.len() as u64 + lost < n {
        match timeout(Duration::from_secs(3), client.recv_video()).await {
            Ok(Some(VideoEvent::Frame(f))) => frames.push(f.seq),
            Ok(Some(VideoEvent::Gap { since_seq, through_seq })) => lost += through_seq - since_seq + 1,
            Ok(None) => panic!("the session closed"),
            Err(_) => break,
        }
    }
    sender.await.unwrap();
    (frames, lost)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_input_and_control_cross_loopback() {
    let (host, client, _) = pair(None, HostOpts { max_kbps: 60_000, ..HostOpts::default() }).await;
    let (frames, lost) = stream(&host, &client, 0, 240, 120).await;
    // Loopback loses nothing; a starved scheduler (a busy build machine) could still make
    // a frame miss its deadline, so allow one or two.
    assert!(lost <= 2, "{lost} frames lost on loopback");
    assert_eq!(frames.len() as u64 + lost, 240);
    assert!(frames.windows(2).all(|w| w[0] < w[1]));

    for code in 1..=100u16 {
        client.send_input(InputEvent::Key { code, down: code % 2 == 0 }).unwrap();
    }
    for code in 1..=100u16 {
        let s = timeout(Duration::from_secs(2), host.recv_input()).await.unwrap().unwrap();
        assert_eq!(s.event, InputEvent::Key { code, down: code % 2 == 0 });
    }

    client.send_control(Control::Keyframe).unwrap();
    assert_eq!(timeout(Duration::from_secs(2), host.recv_control()).await.unwrap(), Some(Control::Keyframe));
    host.send_control(Control::Notice { text: "hi".into() }).unwrap();
    assert_eq!(timeout(Duration::from_secs(2), client.recv_control()).await.unwrap(), Some(Control::Notice { text: "hi".into() }));

    let net = client.net_stats().await.unwrap();
    assert!(net.rtt_us.is_some() && net.clock_offset_us.is_some(), "{net:?}");
    assert!(net.clock_offset_us.unwrap().abs() < 1_000, "same machine, same clock: {net:?}");
    let hs = host.stats().await.unwrap();
    assert!(hs.datagram_bytes >= 1452, "PMTU probing found loopback's room: {}", hs.datagram_bytes);

    let mut reports = client.stats_stream(Duration::from_millis(100));
    let r = timeout(Duration::from_secs(2), reports.recv()).await.unwrap().unwrap();
    let line = serde_json::to_string(&r).unwrap();
    assert!(line.starts_with("{\"t\":\"client_stats\""), "{line}");
    assert!(matches!(r, StatsReport::Client { .. }));

    client.close("done");
    let mut link = host.link_state();
    timeout(Duration::from_secs(2), link.wait_for(|s| matches!(s, LinkState::Closed(_)))).await.unwrap().unwrap();
    assert_eq!(host.recv_input().await, None, "the input lane ends with the session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lossy_relay_and_a_rebind_dont_stop_the_stream() {
    let keys = SessionKeys::random();
    let host = HostSession::bind(localhost(), keys.clone(), HostOpts { max_kbps: 40_000, ..HostOpts::default() }).await.unwrap();
    let lossy = LinkParams { delay_us: 1_000, loss: Loss::Random(0.02), ..LinkParams::default() };
    let relay = Relay::spawn(host.local_addr(), lossy.clone(), lossy, 7).unwrap();
    let client = timeout(Duration::from_secs(5), ClientSession::connect(relay.addr(), keys, ClientOpts::default())).await.unwrap().unwrap();
    let first = timeout(Duration::from_secs(5), host.wait_connected()).await.unwrap().unwrap();

    let (frames, lost) = stream(&host, &client, 0, 120, 120).await;
    assert!(lost <= 1, "{lost} frames lost at 2%");
    assert!(frames.len() >= 119);

    relay.rebind();
    let mut link = host.link_state();
    let moved = timeout(Duration::from_secs(3), link.wait_for(|s| matches!(s, LinkState::Connected(a) if *a != first))).await;
    assert!(moved.is_ok(), "the host never followed the new address");
    let (frames, lost) = stream(&host, &client, 120, 120, 120).await;
    eprintln!("after the rebind: {} frames, {lost} lost", frames.len());
    assert!(frames.len() as u64 + lost >= 119, "the stream stalled after the rebind");
    let (down, up) = relay.stats();
    assert!(down.lost > 0 && up.lost > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strangers_are_ignored() {
    let (host, client, _) = pair(None, HostOpts::default()).await;
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
    for i in 0..200u32 {
        let mut junk = vec![1u8, 0];
        junk.extend_from_slice(&u64::from(i + 1_000_000).to_be_bytes());
        junk.extend_from_slice(&[0xab; 100]);
        stranger.send_to(&junk, host.local_addr()).unwrap();
    }
    client.send_input(InputEvent::ReleaseAll).unwrap();
    let s = timeout(Duration::from_secs(2), host.recv_input()).await.unwrap().unwrap();
    assert_eq!(s.event, InputEvent::ReleaseAll);
    let stats = host.stats().await.unwrap();
    assert!(stats.rejected >= 200, "{stats:?}");
    assert_eq!(stats.peer, Some(client.local_addr().to_string().replace("0.0.0.0", "127.0.0.1")));
}
