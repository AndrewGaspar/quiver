//! The contract between Quiver's remote-play pieces (design/REMOTE.md).
//!
//! These pieces build against these types and not against each other:
//! - the **host** (`quiver-stream-host`): capture, encode, audio and input injection on the
//!   server;
//! - the **transport** (`quiver-stream-net`): packetising, FEC, encryption, pacing and
//!   congestion control;
//! - the **client** (`quiver-stream`): decode, present, input capture and audio playback;
//! - the **control plane** (`quiver-remote`, in quiverd and quiver-app): pairing, the remote
//!   API, and starting a stream.
//!
//! Timestamps are microseconds on the sender's `CLOCK_MONOTONIC`. The client learns the
//! offset between the two clocks from [`Control::Ping`] and [`Control::Pong`], so it can show
//! the latency of each stage.
//!
//! Change this crate only on purpose. Bump [`PROTOCOL_VERSION`] whenever a change would break
//! the other side.

use serde::{Deserialize, Serialize};

pub mod input;
pub mod media;
pub mod stamp;

pub use input::{InputEvent, MouseButton, PadButtons, PadKind, PadState};
pub use media::{AudioCodec, AudioPacket, Chroma, ColorInfo, EncodedFrame, HdrMetadata, VideoCodec};

/// Both sides refuse a peer whose major version differs from theirs.
pub const PROTOCOL_VERSION: u32 = 1;

/// A monotonic timestamp in microseconds, on the clock of the side that took it.
pub type Micros = u64;

/// Now, on this side's `CLOCK_MONOTONIC`, in microseconds.
pub fn now_micros() -> Micros {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

// --- what the client can do ---------------------------------------------------------------

/// One decoder the client has, and the most it can do with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecoderCaps {
    pub codec: VideoCodec,
    /// Hardware decode (NVDEC, VAAPI or Vulkan Video). The host prefers these.
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// 10-bit decode, which HDR needs.
    pub ten_bit: bool,
    pub chroma: Vec<Chroma>,
}

/// The display the stream goes to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayCaps {
    /// The size, in pixels, of what the stream fills. That is the window or the output.
    pub width: u32,
    pub height: u32,
    /// The refresh rate, in mHz (59940 for 59.94 Hz).
    pub refresh_mhz: u32,
    /// The client's display can show HDR: PQ, BT.2020.
    pub hdr: bool,
    /// The display is VRR-capable, so the client can present frames as they arrive.
    pub vrr: bool,
    /// The output is side-by-side 3D (design/STEREO-UI.md). The host runs the game at full
    /// width, so the game's own SBS mode or the Sight layer fills both halves.
    pub stereo_sbs: bool,
    /// The scale factor, so Sight and the client's menus match the client's UI size.
    pub scale: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCaps {
    pub decoders: Vec<DecoderCaps>,
    pub display: DisplayCaps,
    /// 2, 6 (5.1) or 8 (7.1).
    pub audio_channels: u8,
    /// The pads plugged in at the start. More can arrive later ([`InputEvent::PadArrive`]).
    pub pads: Vec<PadKind>,
    /// The client sends microphone audio ([`AudioPacket`] with `mic: true`).
    pub microphone: bool,
}

// --- the stream's settings -----------------------------------------------------------------

/// What the person picked for this stream. `Auto` lets the host and transport decide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamPrefs {
    pub codec: Option<VideoCodec>,
    /// Bitrate cap in kbit/s. None means adaptive, up to what the link carries.
    pub max_bitrate_kbps: Option<u32>,
    /// A frame rate other than the display's.
    pub fps: Option<u32>,
    /// A resolution other than the display's (e.g. 1080p on a 4K TV).
    pub resolution: Option<(u32, u32)>,
    /// Off: SDR even when both ends could do HDR.
    pub hdr: Option<bool>,
    pub pacing: Pacing,
    /// Chroma 4:4:4 for text-heavy desktops (needs a decoder that does it).
    pub chroma444: bool,
}

/// How the client presents frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Pacing {
    /// Show each frame as soon as it is decoded (tearing or VRR). This has the lowest latency.
    #[default]
    LowestLatency,
    /// Show one frame per refresh, with a small buffer. This has the least judder.
    Smooth,
}

/// The settings the host chose, sent in [`Control::Configure`]. A new one can arrive
/// mid-stream: the client resizes, or the bitrate controller changes the rate. A change of
/// codec or size always starts on a keyframe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// The rate the host's virtual display refreshes at, in mHz.
    pub refresh_mhz: u32,
    pub color: ColorInfo,
    pub bitrate_kbps: u32,
    pub audio: AudioCodec,
    pub audio_channels: u8,
    pub audio_frame_ms: u8,
}

// --- control messages ------------------------------------------------------------------------

/// What the stream is showing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamTarget {
    /// A game in the host's library, with an optional profile (design/PROFILES.md).
    Game { game: String, profile: Option<String> },
    /// A lab instance's display (design/LAB.md), for watching or helping an agent. Read-only
    /// unless `input` is true.
    LabInstance { instance: String, input: bool },
    /// The host's own desktop (a host that isn't headless).
    Desktop,
    /// The host's synthetic test pattern, with the frame stamp ([`stamp`]) drawn into every
    /// frame and a tone each second: for diagnostics and latency checks, no program runs.
    TestPattern,
}

/// Reliable messages on the stream's control channel, in both directions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    /// Client → host, first.
    Hello { version: u32, caps: ClientCaps, prefs: StreamPrefs, target: StreamTarget },
    /// Host → client, after Hello. It is followed by a Configure before the first frame.
    Welcome { version: u32, host_name: String, session: String },
    /// Host → client. These are the settings the frames and audio that follow use.
    Configure(StreamConfig),
    /// Client → host. The client's window or output changed.
    Resize { display: DisplayCaps },
    /// Client → host. The person changed a setting mid-stream.
    Prefs(StreamPrefs),
    /// Client → host. A frame couldn't be decoded, and frames from `since_seq` on depend on
    /// it. The host invalidates the reference, or sends a keyframe if it can't.
    LostFrames { since_seq: u64, through_seq: u64 },
    /// Client → host. Send a keyframe now (at the start, or after a long stall).
    Keyframe,
    /// Either way. The other side answers at once with Pong carrying its own clock.
    Ping { id: u32, sent: Micros },
    Pong { id: u32, sent: Micros, peer_now: Micros },
    /// Client → host, about once a second. The bitrate controller and the stats overlay
    /// use it.
    Stats(ClientStats),
    /// Host → client: the host's side, for the stats overlay.
    HostStats(HostStats),
    /// Host → client. The game wants a cursor shown, for menus and desktop targets.
    /// `None` hides it. Pixels are RGBA, at most 256×256.
    Cursor { shape: Option<CursorImage> },
    /// Host → client: rumble or trigger haptics for one pad, until `duration_ms` passes or a
    /// newer one arrives.
    Rumble { slot: u8, low: u16, high: u16, duration_ms: u16 },
    /// Host → client: something worth a toast on the client.
    Notice { text: String },
    /// Either way: the stream is ending. From the client, `reason` [`BYE_QUIT_GAME`] also
    /// ends the game; any other reason leaves it running, so the stream can be resumed.
    Bye { reason: String },
}

/// The [`Control::Bye`] reason with which the client asks the host to end the game too
/// (the stream menu's "Quit game"). Other reasons only disconnect.
pub const BYE_QUIT_GAME: &str = "quit-game";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
    /// The game wants relative motion: capture the mouse and hide the cursor.
    pub relative: bool,
}

/// What the client measured since its last Stats.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientStats {
    pub frames_received: u32,
    /// Frames that never arrived whole, even after FEC.
    pub frames_lost: u32,
    /// Frames the client dropped so it could stay current.
    pub frames_dropped: u32,
    pub packets_received: u32,
    pub packets_lost: u32,
    /// Packets that FEC put back.
    pub packets_recovered: u32,
    pub rtt_us: u32,
    pub jitter_us: u32,
    /// Medians over the period.
    pub network_us: u32,
    pub decode_us: u32,
    pub present_us: u32,
    /// From the host capturing a frame to the client presenting it, by the synced clocks.
    pub capture_to_present_us: u32,
    /// The bandwidth the transport estimates, in kbit/s.
    pub estimated_kbps: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostStats {
    pub fps: f32,
    /// Medians: capture to encode start, and encode itself.
    pub capture_us: u32,
    pub encode_us: u32,
    pub bitrate_kbps: u32,
    pub gpu_percent: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trips_as_json() {
        let m = Control::Rumble { slot: 0, low: 1, high: 2, duration_ms: 50 };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"t\":\"rumble\""));
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), m);
    }

    #[test]
    fn test_pattern_target_is_additive() {
        let s = serde_json::to_string(&StreamTarget::TestPattern).unwrap();
        assert_eq!(s, "\"TestPattern\"");
        assert_eq!(serde_json::from_str::<StreamTarget>("\"Desktop\"").unwrap(), StreamTarget::Desktop);
    }

    #[test]
    fn clock_moves_forward() {
        let a = now_micros();
        let b = now_micros();
        assert!(b >= a && a > 0);
    }
}
