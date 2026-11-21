//! Quiver remote play's media transport (design/REMOTE.md §2): UDP between
//! `quiver-stream-host` and `quiver-stream`, carrying video and audio down and input up.
//!
//! # Using it
//!
//! The control plane makes a [`SessionKeys`] per stream and gives it to both ends. The
//! transport has no handshake of its own: whoever holds the keys is the peer.
//!
//! ```no_run
//! # async fn demo(frame: quiver_stream_proto::EncodedFrame) -> std::io::Result<()> {
//! use quiver_stream_net::*;
//! let keys = SessionKeys::random();
//!
//! // Host
//! let host = HostSession::bind("0.0.0.0:47998".parse().unwrap(), keys.clone(), HostOpts::default()).await?;
//! let port = host.local_addr().port(); // → the control plane → the client
//! let mut bitrate = host.target_bitrate(); // reconfigure the encoder on each change
//! host.wait_connected().await.ok();
//! host.send_video(frame).ok();
//! while let Some(event) = host.recv_input().await { /* inject it */ }
//!
//! // Client
//! let client = ClientSession::connect(format!("192.168.1.10:{port}").parse().unwrap(), keys, ClientOpts::default()).await?;
//! while let Some(v) = client.recv_video().await {
//!     match v {
//!         VideoEvent::Frame(f) => { /* decode */ }
//!         VideoEvent::Gap { since_seq, through_seq } => {
//!             client.send_control(quiver_stream_proto::Control::LostFrames { since_seq, through_seq }).ok();
//!         }
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! | Host | Client |
//! |---|---|
//! | [`HostSession::bind`], [`local_addr`](HostSession::local_addr), [`wait_connected`](HostSession::wait_connected) | [`ClientSession::connect`] |
//! | [`send_video`](HostSession::send_video), [`send_audio`](HostSession::send_audio), [`send_control`](HostSession::send_control) | [`recv_video`](ClientSession::recv_video) → [`VideoEvent`], [`recv_audio`](ClientSession::recv_audio), [`recv_control`](ClientSession::recv_control) |
//! | [`recv_input`](HostSession::recv_input) (each [`Stamped`](quiver_stream_proto::input::Stamped) once, in order), [`recv_control`](HostSession::recv_control), [`recv_audio`](HostSession::recv_audio) (mic) | [`send_input`](ClientSession::send_input), [`send_control`](ClientSession::send_control), [`send_audio`](ClientSession::send_audio) (mic) |
//! | [`target_bitrate`](HostSession::target_bitrate) (kbit/s, a `watch`), [`set_max_bitrate`](HostSession::set_max_bitrate) | [`clock_offset`](ClientSession::clock_offset), [`host_to_local`](ClientSession::host_to_local), [`rtt_us`](ClientSession::rtt_us) |
//! | [`stats`](HostSession::stats) → [`HostNetStats`] | [`take_stats`](ClientSession::take_stats) → [`ClientStats`](quiver_stream_proto::ClientStats), [`net_stats`](ClientSession::net_stats) → [`ClientNetStats`] |
//! | [`stats_stream`](HostSession::stats_stream), [`stats_json_lines`](HostSession::stats_json_lines) | the same |
//! | [`link_state`](HostSession::link_state), [`close`](HostSession::close) | the same |
//!
//! Sending never blocks or awaits; receiving is async. Sessions are `Clone + Send + Sync`.
//!
//! # How it works
//!
//! - **Datagrams** are sealed with AES-256-GCM (ring), under a key per direction derived
//!   from the session key; the nonce is the direction and the packet number. A sliding
//!   window of 4096 packet numbers drops replays ([`crypto`]).
//! - **Addresses.** The host adopts the client's address only from authenticated datagrams
//!   and only after the client answers a challenge from it, so roaming and NAT rebinding
//!   just work and a replayed datagram can't hijack the stream ([`path`]).
//! - **Lanes** share the socket as chunks ([`wire`]): video ([`video`]: shards sized to the
//!   path MTU, one Reed-Solomon block per frame with recovery sized to the measured loss,
//!   reassembly with deadlines, gaps, and NACKs when the RTT is short), audio with the
//!   previous packets piggybacked ([`audio`]), input that rides in every datagram until
//!   acked ([`input`]), and a reliable ordered control lane ([`control`]).
//! - **Pacing** spreads each frame over [`HostOpts::pacing_fraction`] of the frame interval
//!   ([`pacer`]), on an I/O thread with microsecond timers, sending with GSO and
//!   receiving with GRO and `recvmmsg` ([`io`]), marked with DSCP for Wi-Fi's WMM queues.
//! - **Bandwidth** is estimated GCC-style from transport-wide feedback: delay gradient
//!   (trendline), loss above the path's baseline, and probing at the start ([`bwe`]).
//! - **Clock sync** from pings: the RTT, and the host-minus-client clock offset from the
//!   least-queued recent sample ([`clock`]).
//! - **Testing**: [`sim`] has a userspace link model (loss, bursts, delay, jitter,
//!   reordering, duplication, a bottleneck with a queue), a virtual-time harness for the
//!   cores, a real-time loopback relay, and a synthetic encoder.
//!
//! The protocol state machines ([`HostCore`], [`ClientCore`]) do no I/O, so the same code
//! runs under the simulator and on sockets.

pub mod audio;
pub mod bwe;
pub mod client;
pub mod clock;
pub mod codec;
pub mod conn;
pub mod control;
pub mod crypto;
pub mod fec;
pub mod feedback;
pub mod host;
pub mod input;
pub mod io;
pub mod pacer;
pub mod path;
pub mod session;
pub mod sim;
pub mod video;
pub mod wire;

pub use client::{ClientCore, ClientEvent, ClientNetStats, ClientOpts};
pub use crypto::{Cipher, SessionKeys};
pub use host::{HostCore, HostEvent, HostNetStats, HostOpts};
pub use input::InputOpts;
pub use session::{ClientSession, ClockInfo, Closed, HostSession, LinkState, SendError, StatsReport};
pub use video::{RxOpts, VideoEvent};
