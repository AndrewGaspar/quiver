//! A GameStream-protocol server: what Moonlight clients (TVs, phones, Steam Decks, other
//! PCs) speak, as a second front end over Quiver's remote-play host (design/REMOTE.md,
//! phase 1b).
//!
//! It takes the same host types as Quiver's own transport: it consumes [`EncodedFrame`]s and
//! [`AudioPacket`]s from the host and gives it [`InputEvent`]s and [`Control`] messages. The
//! embedder (quiverd) supplies three things:
//! - an [`AppLibrary`]: the titles Moonlight lists, and their box art;
//! - a [`ClientStore`]: where paired Moonlight clients are pinned ([`FileClientStore`] keeps
//!   them in a JSON file of the caller's choosing, apart from Quiver's own peers);
//! - a [`StreamHost`]: starts a stream for a [`StreamRequest`] and wires its [`HostEnds`].
//!
//! The pieces, by port (the defaults; [`Ports::from_http`] derives the rest):
//!
//! | port | what | module |
//! |---|---|---|
//! | TCP 47989 | HTTP: `/serverinfo`, `/pair` (stages 1–4), `/unpair` | [`nvhttp`] |
//! | TCP 47984 | HTTPS, client certificates: `/serverinfo`, `/pair` (stage 5), `/applist`, `/appasset`, `/launch`, `/resume`, `/cancel` | [`nvhttp`] |
//! | TCP 48010 | RTSP: OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY, encrypted | [`rtsp`], [`sdp`] |
//! | UDP 47998 | video: RTP with Moonlight's NV header and Reed-Solomon FEC | [`video`], [`fec`] |
//! | UDP 47999 | control: ENet, AES-GCM; input, IDR and RFI requests, rumble, HDR, termination | [`control`], [`input`] |
//! | UDP 48000 | audio: Opus RTP with FEC, optionally AES-CBC | [`audio`] |
//!
//! **Over Tailscale.** [`ServerConfig::bind`] can be just the tailnet's addresses (the 100.x
//! one and the `fd7a:115c:a1e0::` one, or [`interface_addresses`]`("tailscale0")`), and the
//! stream's UDP ports bind to whichever address the client used. Nothing depends on mDNS:
//! Moonlight adds the server by IP or MagicDNS name. Tailscale's MTU is 1280, so media
//! datagrams are sent with don't-fragment and sized to the route's MTU: Moonlight sends 1024-byte
//! video packets to a 100.x address (it counts CGNAT space as remote), which fit, but treats
//! an `fd7a:` address as local and asks for 1392, which doesn't; then the server cuts
//! packets to fit and sends them without FEC (Moonlight's FEC recovery assumes its own
//! size). Setting Moonlight's packet size to 1024 keeps FEC on such a path.
//!
//! **Pairing** is GameStream's PIN flow ([`pairing`]): Moonlight shows a PIN, the server's
//! owner enters it on the server side, fed in through [`GameStreamServer::submit_pin`] (from
//! the Quiver app on another machine, or `quiver remote moonlight-pin 1234`).
//!
//! **Security.** GameStream's PIN pairing is weaker than Quiver's SPAKE2 pairing: the 4-digit
//! PIN only keys AES-ECB over values that are sent in the clear, and the server reveals its
//! pairing secret before the client has proven it knows the PIN, so whoever's request the
//! owner approves can recover the PIN offline in 10⁴ tries and complete the pairing. The PIN
//! therefore proves nothing about *which* device asked; the owner's approval of a request
//! (shown with the device's name and address) is what authorises it. Hence the defaults:
//! the whole front end is off unless enabled, one pairing request waits at a time (a second
//! from elsewhere is refused and reported), a request expires after [`ServerConfig::pin_timeout`],
//! and a paired Moonlight client may only stream, never use the remote API. After pairing,
//! everything rests on the pinned certificates (mutual TLS) and the per-launch AES key.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;

pub mod audio;
pub mod control;
pub mod crypto;
mod enet;
pub mod fec;
pub mod host;
mod http;
pub mod identity;
pub mod input;
pub mod nvhttp;
pub mod pairing;
pub mod rtsp;
pub mod sdp;
mod stream;
pub mod store;
#[cfg(feature = "stream-host")]
pub mod stream_host;
mod tls;
pub mod video;

pub use host::{App, AppLibrary, Art, AudioParams, ClientStore, ColorSpace, HostEnds, PairedClient, StreamHost, StreamRequest, TriggerRumble, VideoParams};
pub use identity::Identity;
pub use store::FileClientStore;

#[allow(unused_imports)]
use quiver_stream_proto::{AudioPacket, Control, EncodedFrame, InputEvent};

/// The ports the server listens on. Moonlight only needs to be told the HTTP port; it
/// learns the others from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub http: u16,
    pub https: u16,
    pub rtsp: u16,
    pub video: u16,
    pub control: u16,
    pub audio: u16,
}

impl Ports {
    /// The GameStream layout around an HTTP port: HTTPS 5 below it, video, control and audio
    /// 9, 10 and 11 above, RTSP 21 above. 47989 gives Moonlight's defaults.
    pub fn from_http(http: u16) -> Self {
        Self { http, https: http - 5, video: http + 9, control: http + 10, audio: http + 11, rtsp: http + 21 }
    }
}

impl Default for Ports {
    fn default() -> Self {
        Self::from_http(47989)
    }
}

/// The codecs the host can encode. Moonlight picks the best both sides have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codecs {
    pub h264: bool,
    pub hevc: bool,
    pub hevc_main10: bool,
    pub av1: bool,
    pub av1_main10: bool,
    /// 4:4:4 chroma for each codec offered.
    pub yuv444: bool,
}

impl Default for Codecs {
    /// What NVENC on this machine does.
    fn default() -> Self {
        Self { h264: true, hevc: true, hevc_main10: true, av1: true, av1_main10: true, yuv444: false }
    }
}

impl Codecs {
    /// `ServerCodecModeSupport` in `/serverinfo`.
    pub fn mode_flags(&self) -> u32 {
        let mut f = 0;
        if self.h264 {
            f |= 0x1;
            if self.yuv444 {
                f |= 0x4_0000;
            }
        }
        if self.hevc {
            f |= 0x100;
            if self.yuv444 {
                f |= 0x8_0000;
            }
        }
        if self.hevc_main10 {
            f |= 0x200;
            if self.yuv444 {
                f |= 0x10_0000;
            }
        }
        if self.av1 {
            f |= 0x1_0000;
            if self.yuv444 {
                f |= 0x20_0000;
            }
        }
        if self.av1_main10 {
            f |= 0x2_0000;
            if self.yuv444 {
                f |= 0x40_0000;
            }
        }
        f
    }
}

/// When to ask Moonlight to encrypt audio and video (the control stream always is). A
/// client may also turn it on itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvEncryption {
    Never,
    /// Ask unless the client is on a private, loopback, link-local or CGNAT (Tailscale)
    /// address.
    OutsideLan,
    Always,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The name Moonlight shows for this server.
    pub name: String,
    /// The addresses to listen on. The default is every address (IPv4 and IPv6). To be
    /// reachable only over a tailnet, give its addresses: the 100.x address and the
    /// `fd7a:115c:a1e0::` one, or everything on `tailscale0` ([`interface_addresses`]).
    /// A stream's UDP ports bind to the address the client reached the server on.
    pub bind: Vec<IpAddr>,
    pub ports: Ports,
    /// Where the server's identity (key, certificate, id) lives.
    pub state_dir: PathBuf,
    pub codecs: Codecs,
    /// Parity per video FEC block, percent.
    pub fec_percent: u8,
    /// The host honours [`Control::LostFrames`] by invalidating references (NVENC can).
    /// When false, Moonlight asks for keyframes instead.
    pub reference_frame_invalidation: bool,
    /// Ask Moonlight for touch events ([`InputEvent::Touch`]) rather than emulating a mouse
    /// with touches. Off by default: few PC games take touch, and every host has a mouse.
    pub native_touch: bool,
    pub av_encryption: AvEncryption,
    /// How long a pairing request waits for its PIN.
    pub pin_timeout: Duration,
    /// For tests: drop this share of video packets (percent) at random, to exercise FEC
    /// with a real client. Always 0 in use.
    pub test_video_loss_percent: u8,
}

impl ServerConfig {
    pub fn new(name: impl Into<String>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            bind: vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED), IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)],
            ports: Ports::default(),
            state_dir: state_dir.into(),
            codecs: Codecs::default(),
            fec_percent: 20,
            reference_frame_invalidation: true,
            native_touch: false,
            av_encryption: AvEncryption::OutsideLan,
            pin_timeout: Duration::from_secs(120),
            test_video_loss_percent: 0,
        }
    }
}

/// What happened, for the embedder's UI and logs.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// A Moonlight client asked to pair and is waiting for its PIN.
    PairingRequested { device_name: String, address: IpAddr },
    /// A second request arrived from another address while one was waiting, and was
    /// refused. Possibly someone racing the person's own request.
    PairingContested { address: IpAddr },
    Paired(PairedClient),
    PairingFailed { address: IpAddr, reason: String },
    StreamStarted { app: String, client: String },
    StreamEnded { app: String, reason: String },
}

/// A pairing request waiting for (or past) its PIN, for [`GameStreamServer::pending_pairing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    pub device_name: String,
    pub address: IpAddr,
}

pub(crate) struct PendingPairing {
    pub id: u64,
    pub device_name: String,
    pub address: IpAddr,
    pub salt: [u8; 16],
    pub client: pairing::ClientCert,
    pub pin_tx: Option<oneshot::Sender<String>>,
    pub state: Option<pairing::Pairing>,
    pub waiters: Vec<oneshot::Sender<Result<PairedClient, String>>>,
    pub since: std::time::Instant,
}

pub(crate) struct Shared {
    pub config: ServerConfig,
    pub identity: Identity,
    pub library: Arc<dyn AppLibrary>,
    pub clients: Arc<dyn ClientStore>,
    pub host: Arc<dyn StreamHost>,
    pub pairing: Mutex<Option<PendingPairing>>,
    pub pairing_ids: std::sync::atomic::AtomicU64,
    pub launch: Mutex<Option<Arc<stream::Launch>>>,
    pub events: broadcast::Sender<ServerEvent>,
}

impl Shared {
    pub fn emit(&self, e: ServerEvent) {
        tracing::info!(event = ?e, "gamestream");
        let _ = self.events.send(e);
    }

    pub fn current_launch(&self) -> Option<Arc<stream::Launch>> {
        self.launch.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A running GameStream server. Dropping it (or [`Self::shutdown`]) stops every listener
/// and the stream.
pub struct GameStreamServer {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
    stop: watch::Sender<bool>,
}

impl GameStreamServer {
    /// Loads or makes the identity and starts listening on HTTP, HTTPS and RTSP. The UDP
    /// ports are bound per stream.
    pub async fn start(config: ServerConfig, library: Arc<dyn AppLibrary>, clients: Arc<dyn ClientStore>, host: Arc<dyn StreamHost>) -> Result<Self> {
        let dir = config.state_dir.clone();
        let identity = tokio::task::spawn_blocking(move || Identity::load_or_create(&dir)).await??;
        Self::start_with_identity(config, identity, library, clients, host).await
    }

    pub async fn start_with_identity(
        config: ServerConfig,
        identity: Identity,
        library: Arc<dyn AppLibrary>,
        clients: Arc<dyn ClientStore>,
        host: Arc<dyn StreamHost>,
    ) -> Result<Self> {
        let tls = tokio_rustls::TlsAcceptor::from(tls::server_config(&identity)?);
        if config.bind.is_empty() {
            bail!("no address to listen on");
        }
        let mut http = Vec::new();
        let mut https = Vec::new();
        let mut rtsp = Vec::new();
        for &ip in &config.bind {
            http.push(listen(ip, config.ports.http).with_context(|| format!("listen on HTTP {ip} {}", config.ports.http))?);
            https.push(listen(ip, config.ports.https).with_context(|| format!("listen on HTTPS {ip} {}", config.ports.https))?);
            rtsp.push(listen(ip, config.ports.rtsp).with_context(|| format!("listen on RTSP {ip} {}", config.ports.rtsp))?);
        }
        let (events, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            config,
            identity,
            library,
            clients,
            host,
            pairing: Mutex::new(None),
            pairing_ids: Default::default(),
            launch: Mutex::new(None),
            events,
        });
        let (stop, _) = watch::channel(false);
        let mut tasks = Vec::new();

        for http in http {
            let s = shared.clone();
            tasks.push(tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = http.accept().await else { continue };
                let s = s.clone();
                tokio::spawn(async move {
                    let Ok(local) = tcp.local_addr().map(|a| a.ip()) else { return };
                    let _ = tcp.set_nodelay(true);
                    let mut tcp = tcp;
                    if let Err(e) = nvhttp::serve(&s, &mut tcp, peer, local, None).await {
                        tracing::debug!(%peer, "HTTP: {e:#}");
                    }
                });
            }
            }));
        }

        for https in https {
            let s = shared.clone();
            let tls = tls.clone();
            tasks.push(tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = https.accept().await else { continue };
                let s = s.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    let Ok(local) = tcp.local_addr().map(|a| a.ip()) else { return };
                    let _ = tcp.set_nodelay(true);
                    let mut io = match tokio::time::timeout(Duration::from_secs(10), tls.accept(tcp)).await {
                        Ok(Ok(io)) => io,
                        Ok(Err(e)) => return tracing::debug!(%peer, "TLS handshake: {e}"),
                        Err(_) => return tracing::debug!(%peer, "TLS handshake timed out"),
                    };
                    let cert = io.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| c.as_ref().to_vec());
                    if let Err(e) = nvhttp::serve(&s, &mut io, peer, local, Some(cert)).await {
                        tracing::debug!(%peer, "HTTPS: {e:#}");
                    }
                });
            }
            }));
        }

        for rtsp in rtsp {
            let s = shared.clone();
            tasks.push(tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = rtsp.accept().await else { continue };
                let s = s.clone();
                tokio::spawn(async move {
                    if let Err(e) = stream::serve_rtsp(&s, tcp, peer).await {
                        tracing::warn!(%peer, "RTSP: {e:#}");
                    }
                });
            }
            }));
        }

        Ok(Self { shared, tasks, stop })
    }

    pub fn identity(&self) -> &Identity {
        &self.shared.identity
    }

    pub fn config(&self) -> &ServerConfig {
        &self.shared.config
    }

    pub fn events(&self) -> broadcast::Receiver<ServerEvent> {
        self.shared.events.subscribe()
    }

    /// The pairing request waiting for a PIN, if any.
    pub fn pending_pairing(&self) -> Option<PairingRequest> {
        let p = self.shared.pairing.lock().unwrap_or_else(|e| e.into_inner());
        p.as_ref().filter(|p| p.pin_tx.is_some()).map(|p| PairingRequest { device_name: p.device_name.clone(), address: p.address })
    }

    /// Feeds the PIN Moonlight shows to the waiting pairing request, and waits for that
    /// pairing to finish: `Ok` with the client once its certificate is pinned.
    pub async fn submit_pin(&self, pin: &str) -> Result<PairedClient> {
        let pin = pin.trim();
        if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
            bail!("a Moonlight PIN is 4 digits");
        }
        let rx = {
            let mut guard = self.shared.pairing.lock().unwrap_or_else(|e| e.into_inner());
            let p = guard.as_mut().ok_or_else(|| anyhow!("no Moonlight client is waiting to pair"))?;
            let tx = p.pin_tx.take().ok_or_else(|| anyhow!("that pairing already has its PIN"))?;
            tx.send(pin.to_owned()).map_err(|_| anyhow!("the pairing request went away"))?;
            let (otx, orx) = oneshot::channel();
            p.waiters.push(otx);
            orx
        };
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(c))) => Ok(c),
            Ok(Ok(Err(reason))) => bail!("pairing failed: {reason}"),
            Ok(Err(_)) => bail!("the pairing was abandoned"),
            Err(_) => bail!("the client didn't finish pairing"),
        }
    }

    /// Ends the stream (if any) and stops listening.
    pub async fn shutdown(mut self) {
        self.stop_all();
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
    }

    fn stop_all(&mut self) {
        let _ = self.stop.send(true);
        for t in &self.tasks {
            t.abort();
        }
        if let Some(l) = self.shared.launch.lock().unwrap_or_else(|e| e.into_inner()).take() {
            l.end("server stopped");
        }
        self.shared.pairing.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl Drop for GameStreamServer {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// A TCP listener on `ip:port`. An IPv6 socket is IPv6-only, so `0.0.0.0` and `::` can both
/// be bound.
fn listen(ip: IpAddr, port: u16) -> Result<TcpListener> {
    let sock = match ip {
        IpAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        IpAddr::V6(_) => {
            let s = tokio::net::TcpSocket::new_v6()?;
            set_v6_only(&s)?;
            s
        }
    };
    sock.set_reuseaddr(true)?;
    sock.bind(SocketAddr::new(ip, port))?;
    Ok(sock.listen(64)?)
}

pub(crate) fn set_v6_only(fd: &impl std::os::fd::AsRawFd) -> Result<()> {
    let one: libc::c_int = 1;
    // SAFETY: a valid socket fd and a pointer to an int that outlives the call.
    let r = unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, (&one as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as u32)
    };
    if r != 0 {
        bail!("IPV6_V6ONLY: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// The addresses on a network interface (for example `tailscale0`), to listen only there.
/// Link-local IPv6 addresses are left out (a tailnet doesn't use them).
pub fn interface_addresses(name: &str) -> Result<Vec<IpAddr>> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list that is only read here and freed below.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            bail!("getifaddrs: {}", std::io::Error::last_os_error());
        }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || std::ffi::CStr::from_ptr(ifa.ifa_name).to_bytes() != name.as_bytes() {
                continue;
            }
            match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr))));
                }
                libc::AF_INET6 => {
                    let a = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let ip = std::net::Ipv6Addr::from(a.sin6_addr.s6_addr);
                    if (ip.segments()[0] & 0xffc0) != 0xfe80 {
                        out.push(IpAddr::V6(ip));
                    }
                }
                _ => {}
            }
        }
        libc::freeifaddrs(ifap);
    }
    if out.is_empty() {
        bail!("interface {name} has no usable address");
    }
    Ok(out)
}

/// Whether an address is on this side of the internet: loopback, private, link-local, or
/// CGNAT (Tailscale's 100.64/10).
pub fn is_lan(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_and_codecs() {
        let p = Ports::default();
        assert_eq!((p.http, p.https, p.rtsp, p.video, p.control, p.audio), (47989, 47984, 48010, 47998, 47999, 48000));
        assert_eq!(Codecs::default().mode_flags(), 0x1 | 0x100 | 0x200 | 0x1_0000 | 0x2_0000);
        let all = Codecs { yuv444: true, ..Codecs::default() };
        assert_eq!(all.mode_flags() & 0x7C_0000, 0x7C_0000);
    }

    #[test]
    fn loopback_interface_addresses() {
        let a = interface_addresses("lo").unwrap();
        assert!(a.contains(&"127.0.0.1".parse().unwrap()), "{a:?}");
        assert!(interface_addresses("no-such-if0").is_err());
    }

    #[tokio::test]
    async fn listens_on_v4_and_v6_together() {
        let a = listen("0.0.0.0".parse().unwrap(), 0).unwrap();
        let port = a.local_addr().unwrap().port();
        // The same port on :: works only because the IPv6 socket is IPv6-only.
        let b = listen("::".parse().unwrap(), port);
        assert!(b.is_ok(), "{b:?}");
    }

    #[test]
    fn lan_addresses() {
        for a in ["127.0.0.1", "10.1.2.3", "192.168.0.9", "172.16.0.1", "169.254.1.1", "100.100.1.1", "::1", "fd7a:115c:a1e0::1", "fe80::1", "::ffff:192.168.1.1"] {
            assert!(is_lan(a.parse().unwrap()), "{a}");
        }
        for a in ["8.8.8.8", "100.128.0.1", "2001:db8::1"] {
            assert!(!is_lan(a.parse().unwrap()), "{a}");
        }
    }
}
