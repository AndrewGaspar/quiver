//! What the server needs from its embedder (quiverd): the app list and art, a store for
//! paired clients, and a host that streams. Plus the request a stream starts with.

use anyhow::Result;
use quiver_stream_proto::{AudioPacket, Chroma, ClientCaps, Control, EncodedFrame, InputEvent, StreamPrefs, VideoCodec};
use tokio::sync::{mpsc, watch};

use crate::audio::OpusLayout;

/// One title Moonlight can start. Moonlight calls these apps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The embedder's id for the title (a Quiver game id). Moonlight sees a number derived
    /// from it ([`crate::nvhttp::app_number`]), stable across restarts.
    pub key: String,
    pub title: String,
    /// The title can render HDR. Moonlight shows an HDR badge and asks for 10-bit only then.
    pub hdr: bool,
}

/// Box art for an app: Moonlight shows it portrait, about 2:3 (it asks for 628×888).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    /// `image/png` or `image/jpeg`.
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// The library Moonlight browses. quiverd implements this over its installed, playable
/// titles (only real installable owned games with titles and art).
pub trait AppLibrary: Send + Sync + 'static {
    fn apps(&self) -> Vec<App>;
    /// The art for `key`, or `None` (Moonlight then draws the title on a plain tile).
    fn art(&self, key: &str) -> Option<Art>;
}

/// A Moonlight client whose certificate is pinned.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairedClient {
    /// The device name Moonlight sent while pairing (often just "roth", Moonlight's
    /// placeholder) and where it paired from, for the server's UI.
    pub name: String,
    pub address: String,
    /// SHA-256 of the certificate's DER, hex. This is the pin.
    pub fingerprint: String,
    pub cert_pem: String,
    /// Seconds since the Unix epoch.
    pub paired_at: u64,
}

/// Where paired Moonlight clients are kept. It is separate from Quiver's own peers
/// (`peers.json`): a GameStream pairing is weaker (see the crate docs), so it grants
/// streaming, never the remote API.
pub trait ClientStore: Send + Sync + 'static {
    fn list(&self) -> Result<Vec<PairedClient>>;
    /// Adds or replaces (by fingerprint).
    fn add(&self, client: PairedClient) -> Result<()>;
    /// Removes by fingerprint. `Ok(false)` when it wasn't there.
    fn remove(&self, fingerprint: &str) -> Result<bool>;

    fn find(&self, fingerprint: &str) -> Result<Option<PairedClient>> {
        Ok(self.list()?.into_iter().find(|c| c.fingerprint == fingerprint))
    }
}

/// The streaming host behind the server: quiverd runs the app in a stream host and wires its
/// channels to [`HostEnds`].
///
/// As GameStream hosts do, a running app outlives its sessions: the client going (its input
/// closing, a `Bye` on `control_in`) is a disconnect, and the app is kept for a resume
/// ([`crate::keep::Kept`] does this for an embedder). Only [`StreamHost::quit`] (Moonlight's
/// Quit app) or the app ending ends it.
pub trait StreamHost: Send + Sync + 'static {
    /// Starts streaming `req.app`: launching it, or, when it is running (kept after a
    /// disconnect, or streaming to a session this replaces), attaching this session to it
    /// with the new launch's keys. Called when the client's RTSP handshake finishes, so every
    /// setting is known. The session ends when `ends.input` closes (the client left) or the
    /// host drops `ends.video` (the app or the host stopped).
    fn start(&self, req: StreamRequest, ends: HostEnds) -> Result<()>;
    /// The client asked to quit the app (`/cancel`, Moonlight's "Quit app").
    fn quit(&self, app_key: &str);
    /// The app running now (streaming, or kept for a resume), by key. `/serverinfo` reports
    /// it as the current game, so Moonlight offers Resume and Quit for it, and asks to quit
    /// it before launching another.
    fn running(&self) -> Option<String>;
}

/// The host's ends of a stream's channels.
pub struct HostEnds {
    /// Encoded video, in order. Annex-B for H.264/HEVC, OBUs for AV1 (as
    /// [`EncodedFrame`] says), with parameter sets on every keyframe.
    pub video: mpsc::Sender<EncodedFrame>,
    /// Opus packets laid out as [`StreamRequest::audio`] says.
    pub audio: mpsc::Sender<AudioPacket>,
    /// Input from the client, already translated (evdev keys, XInput pads).
    pub input: mpsc::Receiver<InputEvent>,
    /// Client → host: [`Control::Keyframe`], [`Control::LostFrames`] and a final
    /// [`Control::Bye`].
    pub control_in: mpsc::Receiver<Control>,
    /// Host → client: [`Control::Rumble`] and [`Control::Bye`] are forwarded; others are
    /// accepted and ignored (GameStream has no place for them).
    pub control_out: mpsc::Sender<Control>,
    /// Host → client trigger rumble (DualSense and Xbox One impulse triggers), which
    /// [`Control`] has no variant for yet.
    pub trigger_rumble: mpsc::Sender<TriggerRumble>,
    /// The target video bitrate. GameStream fixes it at the start (Moonlight doesn't
    /// adapt), so it holds the negotiated rate.
    pub bitrate_kbps: watch::Receiver<u32>,
}

/// Rumble on a pad's triggers, 0..=65535 each, until a newer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerRumble {
    pub slot: u8,
    pub left: u16,
    pub right: u16,
}

/// Everything negotiated for one stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRequest {
    pub app: App,
    pub client_name: String,
    pub client_fingerprint: String,
    /// What the client said it can do, in Quiver's terms.
    pub caps: ClientCaps,
    /// What the client asked for, in Quiver's terms.
    pub prefs: StreamPrefs,
    /// The encoder settings the stream must use. These are stricter than `prefs`: Moonlight
    /// can't renegotiate, so the host must produce exactly this.
    pub video: VideoParams,
    pub audio: AudioParams,
}

/// The video Moonlight negotiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoParams {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// 8, or 10 for HDR (HEVC Main10, AV1 Main 10).
    pub bit_depth: u8,
    pub chroma: Chroma,
    pub hdr: bool,
    pub color_space: ColorSpace,
    pub full_range: bool,
    /// Slices per frame the client's decoder wants (1 unless it said otherwise).
    pub slices_per_frame: u32,
    /// The client decodes with reference frame invalidation: on loss it sends
    /// [`Control::LostFrames`], and the host should invalidate references rather than send a
    /// keyframe.
    pub reference_frame_invalidation: bool,
    /// The client asked for intra refresh instead of periodic keyframes.
    pub intra_refresh: bool,
}

/// The colour matrix the client expects (BT.2020 always for HDR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Rec601,
    Rec709,
    Rec2020,
}

/// The audio Moonlight negotiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioParams {
    /// The Opus multistream layout, which the host's encoder must use exactly.
    pub opus: OpusLayout,
    /// Each packet holds this many milliseconds (5, or 10 for slow clients).
    pub packet_ms: u8,
    /// Encode at a constant bitrate. Moonlight's audio FEC needs every packet of a 4-packet
    /// block to be the same size; variable packets still play, but lose FEC.
    pub constant_bitrate: bool,
}
