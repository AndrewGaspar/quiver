//! The namespaces themselves: topology, isolation, credentials, shaping and roaming. Each
//! test skips when this machine can't make unprivileged user namespaces.

use std::net::{IpAddr, SocketAddr};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_netlab::ns::{CLIENT_V4, SERVER_V4, SERVER_V6};
use quiver_netlab::shape::{LinkPatch, LinkShape};
use quiver_netlab::{Dir, Ns, Shaper, Side, test_lab};

fn echo_once(lab: &quiver_netlab::Lab, server: IpAddr, any: IpAddr, port: u16) -> (Duration, SocketAddr) {
    let srv = lab.udp(Ns::Server, SocketAddr::new(any, port)).unwrap();
    let cli = lab.udp(Ns::Client, SocketAddr::new(any, 0)).unwrap();
    cli.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    srv.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let t0 = Instant::now();
    cli.send_to(b"ping", SocketAddr::new(server, port)).unwrap();
    let mut buf = [0u8; 16];
    let (n, from) = srv.recv_from(&mut buf).unwrap();
    srv.send_to(&buf[..n], from).unwrap();
    let (n, _) = cli.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ping");
    (t0.elapsed(), from)
}

#[test]
fn sides_reach_each_other_over_v4_and_v6_and_nothing_else() {
    let Some(lab) = test_lab() else { return };
    let (_, from) = echo_once(&lab, SERVER_V4.into(), "0.0.0.0".parse().unwrap(), 9000);
    assert_eq!(from.ip(), IpAddr::from(CLIENT_V4));
    let (_, from6) = echo_once(&lab, SERVER_V6.into(), "::".parse().unwrap(), 9001);
    assert_eq!(from6.ip(), lab.addr(Side::Client, true));

    // Only the lab's own interfaces exist in each namespace.
    let mut r = lab.links(Ns::Router).unwrap();
    r.sort();
    assert_eq!(r, ["cli", "lo", "srv"]);
    assert_eq!(lab.links(Ns::Server).unwrap(), ["lo", "eth0"]);
    assert_eq!(lab.links(Ns::Client).unwrap(), ["lo", "eth0"]);

    // The router has no route out, so the endpoints have none either.
    assert!(lab.run(Ns::Router, &["ip", "route", "get", "1.1.1.1"]).is_err());
    let err = lab.tcp_connect(Ns::Client, "1.1.1.1:80".parse().unwrap(), Duration::from_secs(2)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("unreachable") || msg.contains("refused"), "{msg}");
}

#[test]
fn workloads_run_as_the_person_and_cannot_reshape() {
    let Some(lab) = test_lab() else { return };
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "id -u; id -g; readlink /proc/self/ns/net; ip -o link show | wc -l"]);
    cmd.stdout(std::process::Stdio::piped());
    let out = lab.spawn(Side::Client, cmd).unwrap().wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], uid.to_string());
    assert_eq!(lines[1], gid.to_string());
    let ours = std::fs::read_link("/proc/self/ns/net").unwrap();
    assert_ne!(lines[2], ours.to_str().unwrap(), "the workload is in our network namespace");
    assert_eq!(lines[3].trim(), "2");

    // No capability over the lab's namespaces: the shaping is the lab's alone.
    let mut tc = Command::new("tc");
    tc.args(["qdisc", "add", "dev", "eth0", "root", "netem", "delay", "1ms"]);
    tc.stderr(std::process::Stdio::null());
    assert!(!lab.spawn(Side::Client, tc).unwrap().wait().unwrap().success());
}

#[test]
fn netem_delay_and_rate_take_effect_and_change_in_place() {
    let Some(lab) = test_lab() else { return };
    let mut shaper = Shaper::new(&lab);
    let s = LinkShape { delay_ms: 20.0, rate_mbit: 50.0, ..Default::default() };
    shaper.apply(Dir::ToClient, &s).unwrap();
    shaper.apply(Dir::ToServer, &s).unwrap();
    let (rtt, _) = echo_once(&lab, SERVER_V4.into(), "0.0.0.0".parse().unwrap(), 9002);
    assert!(rtt >= Duration::from_millis(40) && rtt < Duration::from_millis(80), "{rtt:?}");

    let s2 = s.patched(&LinkPatch { delay_ms: Some(5.0), rate_mbit: Some(10.0), ..Default::default() });
    shaper.apply(Dir::ToClient, &s2).unwrap();
    shaper.apply(Dir::ToServer, &s2).unwrap();
    let (rtt, _) = echo_once(&lab, SERVER_V4.into(), "0.0.0.0".parse().unwrap(), 9003);
    assert!(rtt >= Duration::from_millis(10) && rtt < Duration::from_millis(40), "{rtt:?}");
    let c = shaper.counters(Dir::ToClient).unwrap();
    assert!(c.sent_packets >= 2, "{c:?}");
}

#[test]
fn a_roam_changes_the_clients_source_address() {
    let Some(lab) = test_lab() else { return };
    let any: IpAddr = "0.0.0.0".parse().unwrap();
    let srv = lab.udp(Ns::Server, SocketAddr::new(any, 9010)).unwrap();
    srv.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let cli = lab.udp(Ns::Client, SocketAddr::new(any, 0)).unwrap();
    let to = SocketAddr::new(SERVER_V4.into(), 9010);
    let mut buf = [0u8; 8];
    cli.send_to(b"a", to).unwrap();
    let (_, before) = srv.recv_from(&mut buf).unwrap();
    let new = lab.roam().unwrap();
    assert_ne!(new.v4, CLIENT_V4);
    cli.send_to(b"b", to).unwrap();
    let (_, after) = srv.recv_from(&mut buf).unwrap();
    assert_eq!(before.port(), after.port(), "same socket");
    assert_eq!(after.ip(), IpAddr::from(new.v4));
}
