//! The boundary between the client and whatever carries the stream.
//!
//! The client takes a [`StreamLink`]: three receivers, one per kind of thing coming in, so
//! video goes straight to the decode thread and audio straight to the audio thread without a
//! hop through a router, and one sender for everything going out. The real transport
//! (`quiver-stream-net`) fills a link from its sockets; [`loopback`] makes one whose other
//! end ([`HostEnd`]) a test or the file source ([`crate::source`]) drives.
//!
//! Ordering: video and audio arrive in `seq` order. Anything that will never arrive is
//! announced with a `Lost` notice in its place, so a receiver can act on a gap at once; a
//! receiver also treats a jump in `seq` with no notice as a loss.

use crossbeam_channel::{Receiver, Sender, unbounded};
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioPacket, Control, EncodedFrame};

/// Video, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoIn {
    Frame(EncodedFrame),
    /// Frames `first..=last` never arrived whole, even after FEC.
    Lost { first: u64, last: u64 },
}

/// Audio, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioIn {
    Packet(AudioPacket),
    Lost { first: u64, last: u64 },
}

/// The control channel and the transport's own news.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlIn {
    Control(Control),
    /// What the transport measured, about once a second. The client folds it into its
    /// [`quiver_stream_proto::ClientStats`] and the stats chip.
    Transport(TransportStats),
    /// The link is gone (the host ended the stream, or the network did).
    Closed { reason: String },
}

/// The transport's side of the stats, since the last report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransportStats {
    pub packets_received: u32,
    pub packets_lost: u32,
    pub packets_recovered: u32,
    /// The round trip and jitter, if the transport measures them (else the client's
    /// Ping/Pong does).
    pub rtt_us: Option<u32>,
    pub jitter_us: Option<u32>,
    /// The host's clock minus the client's, if the transport keeps the clock sync.
    pub clock_offset_us: Option<i64>,
    pub estimated_kbps: u32,
    /// Bytes of video received, for the bitrate.
    pub video_bytes: u64,
}

/// What the client sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    /// Input, stamped with the client's sequence number and capture time.
    Input(Stamped),
    Control(Control),
}

/// The client's end.
pub struct StreamLink {
    pub video: Receiver<VideoIn>,
    pub audio: Receiver<AudioIn>,
    pub control: Receiver<ControlIn>,
    pub out: Sender<Outgoing>,
}

/// The other end of a [`loopback`] link.
#[derive(Clone)]
pub struct HostEnd {
    pub video: Sender<VideoIn>,
    pub audio: Sender<AudioIn>,
    pub control: Sender<ControlIn>,
    pub from_client: Receiver<Outgoing>,
}

/// A link whose far end is in this process.
pub fn loopback() -> (StreamLink, HostEnd) {
    let (vt, vr) = unbounded();
    let (at, ar) = unbounded();
    let (ct, cr) = unbounded();
    let (ot, or) = unbounded();
    (
        StreamLink { video: vr, audio: ar, control: cr, out: ot },
        HostEnd { video: vt, audio: at, control: ct, from_client: or },
    )
}
