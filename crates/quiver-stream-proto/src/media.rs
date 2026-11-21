//! Encoded media: what the host's encoder makes and the client's decoder takes.

use serde::{Deserialize, Serialize};

use crate::Micros;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

/// How to read the decoded pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorInfo {
    pub bit_depth: u8,
    pub chroma: Chroma,
    /// Full range (0–255) rather than limited (16–235).
    pub full_range: bool,
    /// None means SDR: BT.709 primaries and transfer. Some means HDR10: BT.2020 and PQ.
    pub hdr: Option<HdrMetadata>,
}

impl ColorInfo {
    pub fn sdr() -> Self {
        Self { bit_depth: 8, chroma: Chroma::Yuv420, full_range: false, hdr: None }
    }
}

/// The game's HDR metadata (SMPTE ST 2086 plus content light levels), passed on so the
/// client's display tone-maps as the game asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HdrMetadata {
    /// Display primaries and white point, as CIE 1931 xy in units of 0.00002.
    pub primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    /// In units of 0.0001 cd/m².
    pub max_luminance: u32,
    pub min_luminance: u32,
    /// MaxCLL and MaxFALL, in cd/m².
    pub max_cll: u16,
    pub max_fall: u16,
}

/// One encoded video frame (one access unit).
///
/// `data` is Annex-B for H.264 and HEVC, with parameter sets in front of every keyframe. For
/// AV1 it is a low-overhead OBU stream (a temporal unit), with a sequence header in front of
/// every keyframe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedFrame {
    /// Goes up by one each frame and never restarts within a stream, even across a
    /// Configure.
    pub seq: u64,
    /// When the host captured the frame: the game presented it, and the layer or compositor
    /// handed it over.
    pub captured: Micros,
    /// When the encoder finished it.
    pub encoded: Micros,
    /// An IDR or key frame: it decodes on its own.
    pub keyframe: bool,
    /// The frame doesn't depend on any frame before `refs_from_seq`. With reference
    /// invalidation or intra refresh, a client that lost frames before this one can go on
    /// decoding from here.
    pub refs_from_seq: u64,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    Opus,
}

/// One encoded audio frame. Host to client, it is the game's sound. Client to host (`mic`),
/// it is the person's microphone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPacket {
    pub seq: u64,
    /// When the first sample was captured.
    pub captured: Micros,
    pub mic: bool,
    pub data: Vec<u8>,
}
