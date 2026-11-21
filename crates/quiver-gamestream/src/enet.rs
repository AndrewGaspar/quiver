//! The control stream's transport: an ENet host on UDP 47999, on its own thread.
//!
//! Moonlight connects with 48 channels and the session's connect data (from the SETUP
//! answer's `X-SS-Connect-Data`) in the ENet CONNECT. Only that peer, from the launching
//! client's address, is accepted. ENet itself is [`rusty_enet`] (a port of the MIT-licensed
//! C library Moonlight also uses).

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusty_enet::{EventNoRef, Host, HostSettings, Packet, PeerID};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum EnetEvent {
    Connected(SocketAddr),
    Received { channel: u8, data: Vec<u8> },
    Disconnected,
}

#[derive(Debug)]
pub enum EnetCommand {
    Send { channel: u8, data: Vec<u8> },
    /// Disconnect gracefully after what's queued has gone.
    Disconnect,
}

/// The running ENet thread. Dropping it stops the thread (after a short flush).
pub struct EnetHandle {
    commands: std::sync::mpsc::Sender<EnetCommand>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EnetHandle {
    pub fn send(&self, channel: u8, data: Vec<u8>) {
        let _ = self.commands.send(EnetCommand::Send { channel, data });
    }

    pub fn disconnect(&self) {
        let _ = self.commands.send(EnetCommand::Disconnect);
    }
}

impl Drop for EnetHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts the ENet host on `socket`, accepting one peer from `allowed_ip` whose CONNECT
/// carries `connect_data`.
pub fn spawn(socket: UdpSocket, connect_data: u32, allowed_ip: IpAddr, events: mpsc::UnboundedSender<EnetEvent>) -> Result<EnetHandle> {
    socket.set_nonblocking(true)?;
    let fd = socket.as_raw_fd();
    let host = Host::new(socket, HostSettings { peer_limit: 4, channel_limit: 255, ..Default::default() }).context("ENet host")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("gamestream-enet".into())
        .spawn(move || run(host, fd, connect_data, allowed_ip, events, rx, stop2))?;
    Ok(EnetHandle { commands: tx, stop, thread: Some(thread) })
}

fn run(
    mut host: Host<UdpSocket>,
    fd: i32,
    connect_data: u32,
    allowed_ip: IpAddr,
    events: mpsc::UnboundedSender<EnetEvent>,
    commands: std::sync::mpsc::Receiver<EnetCommand>,
    stop: Arc<AtomicBool>,
) {
    let mut peer: Option<PeerID> = None;
    let mut closing_since: Option<Instant> = None;
    loop {
        if stop.load(Ordering::SeqCst) && closing_since.is_none() {
            closing_since = Some(Instant::now());
            if let Some(p) = peer {
                host.peer_mut(p).disconnect_later(0);
            }
        }
        if let Some(t) = closing_since
            && (peer.is_none() || t.elapsed() > Duration::from_millis(300))
        {
            if let Some(p) = peer {
                host.peer_mut(p).disconnect_now(0);
            }
            host.flush();
            return;
        }

        loop {
            let ev = match host.service() {
                Ok(Some(ev)) => ev.no_ref(),
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("ENet socket: {e}");
                    break;
                }
            };
            match ev {
                EventNoRef::Connect { peer: id, data } => {
                    let p = host.peer_mut(id);
                    let addr = p.address();
                    let ok = peer.is_none() && data == connect_data && addr.is_some_and(|a| a.ip() == allowed_ip || a.ip().to_canonical() == allowed_ip.to_canonical());
                    if ok {
                        p.set_timeout(2, 10_000, 10_000);
                        peer = Some(id);
                        let _ = events.send(EnetEvent::Connected(addr.expect("checked")));
                    } else {
                        tracing::warn!(?addr, data, "refused a control-stream connection");
                        p.disconnect_now(0);
                    }
                }
                EventNoRef::Receive { peer: id, channel_id, packet } => {
                    if Some(id) == peer {
                        let _ = events.send(EnetEvent::Received { channel: channel_id, data: packet.data().to_vec() });
                    }
                }
                EventNoRef::Disconnect { peer: id, .. } => {
                    if Some(id) == peer {
                        peer = None;
                        let _ = events.send(EnetEvent::Disconnected);
                    }
                }
            }
        }

        let mut sent = false;
        while let Ok(cmd) = commands.try_recv() {
            let Some(p) = peer else { continue };
            match cmd {
                EnetCommand::Send { channel, data } => {
                    if let Err(e) = host.peer_mut(p).send(channel, &Packet::reliable(data.as_slice())) {
                        tracing::debug!("ENet send: {e:?}");
                    }
                    sent = true;
                }
                EnetCommand::Disconnect => {
                    host.peer_mut(p).disconnect_later(0);
                    sent = true;
                }
            }
        }
        if sent {
            host.flush();
        }

        // Wait for a datagram, or 2 ms for queued commands and ENet's timers.
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, which outlives the call.
        unsafe { libc::poll(&mut pfd, 1, 2) };
    }
}
