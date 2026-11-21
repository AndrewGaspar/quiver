//! The two SDP bodies of the RTSP handshake.
//!
//! **DESCRIBE** (server → client) says what the server can do: `sprop-parameter-sets=AAAAAU`
//! means HEVC, `a=rtpmap:98 AV1/90000` means AV1, `x-nv-video[0].refPicInvalidation`
//! means reference frame invalidation, `x-ss-general.featureFlags` the Sunshine input
//! extensions (pen/touch, controller touchpads), the encryption it supports and requests,
//! and the Opus layouts for 5.1 and 7.1 (`a=fmtp:97 surround-params=`).
//!
//! **ANNOUNCE** (client → server) is a list of `a=name:value ` lines: the client's choices.
//! [`Announce::parse`] reads them and [`Announce::caps_and_prefs`] maps them onto Quiver's
//! [`ClientCaps`] and [`StreamPrefs`].

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use quiver_stream_proto::{Chroma, ClientCaps, DecoderCaps, DisplayCaps, PadKind, Pacing, StreamPrefs, VideoCodec};

use crate::audio::OpusLayout;
use crate::host::ColorSpace;

/// Encryption feature bits (`x-ss-general.encryption*`).
pub const ENC_CONTROL_V2: u32 = 0x01;
pub const ENC_VIDEO: u32 = 0x02;
pub const ENC_AUDIO: u32 = 0x04;

/// `x-ss-general.featureFlags`: the client may send pen and touch events, controller
/// touchpad events.
pub const FF_PEN_TOUCH: u32 = 0x01;
pub const FF_CONTROLLER_TOUCH: u32 = 0x02;

/// Old-style feature flag: audio encryption (`x-nv-general.featureFlags`).
const NVFF_AUDIO_ENCRYPTION: u32 = 0x20;

/// What the DESCRIBE answer advertises.
#[derive(Debug, Clone)]
pub struct Describe {
    pub hevc: bool,
    pub av1: bool,
    pub reference_frame_invalidation: bool,
    pub feature_flags: u32,
    pub encryption_supported: u32,
    pub encryption_requested: u32,
}

impl Describe {
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("a=x-ss-general.featureFlags:{}\n", self.feature_flags));
        s.push_str(&format!("a=x-ss-general.encryptionSupported:{}\n", self.encryption_supported));
        s.push_str(&format!("a=x-ss-general.encryptionRequested:{}\n", self.encryption_requested));
        if self.reference_frame_invalidation {
            s.push_str("a=x-nv-video[0].refPicInvalidation:1\n");
        }
        if self.hevc {
            s.push_str("sprop-parameter-sets=AAAAAU\n");
        }
        if self.av1 {
            s.push_str("a=rtpmap:98 AV1/90000\n");
        }
        // Normal quality, then high quality, for each channel count; Moonlight takes the
        // first entry for its count as normal and the next one as high quality.
        for ch in [2u8, 6, 8] {
            for hq in [false, true] {
                s.push_str(&format!("a=fmtp:97 surround-params={}\n", OpusLayout::for_channels(ch, hq).sdp_params()));
            }
        }
        s
    }
}

/// The client's ANNOUNCE, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The client's display refresh × 100 (5994 for 59.94 Hz), when it said.
    pub refresh_x100: Option<u32>,
    /// Bytes of NV header and payload per video packet (the shard minus the RTP header).
    pub packet_size: usize,
    /// The bitrate the person configured, kbit/s (`x-ml-video.configuredBitrateKbps`), or
    /// the older peak bitrate.
    pub configured_kbps: u32,
    pub codec: VideoCodec,
    pub hdr: bool,
    pub chroma444: bool,
    pub slices_per_frame: u32,
    /// 0 means the client handles reference frame invalidation.
    pub max_ref_frames: u32,
    pub color_space: ColorSpace,
    pub full_range: bool,
    pub intra_refresh: bool,
    pub audio_channels: u8,
    pub audio_high_quality: bool,
    pub audio_packet_ms: u8,
    pub encryption: u32,
    pub min_fec_packets: usize,
}

impl Announce {
    pub fn parse(sdp: &str) -> Result<Self> {
        let mut a: HashMap<&str, &str> = HashMap::new();
        for line in sdp.lines() {
            let Some(rest) = line.strip_prefix("a=") else { continue };
            let Some((k, v)) = rest.split_once(':') else { continue };
            a.entry(k).or_insert(v.trim());
        }
        let num = |k: &str| -> Result<u32> {
            a.get(k).ok_or_else(|| anyhow!("ANNOUNCE has no {k}"))?.parse::<u32>().with_context(|| format!("ANNOUNCE {k}"))
        };
        let opt = |k: &str| -> Option<u32> { a.get(k).and_then(|v| v.parse().ok()) };

        let codec = match opt("x-nv-vqos[0].bitStreamFormat").unwrap_or(0) {
            1 => VideoCodec::Hevc,
            2 => VideoCodec::Av1,
            _ => VideoCodec::H264,
        };
        let csc = opt("x-nv-video[0].encoderCscMode").unwrap_or(0);
        let color_space = match csc >> 1 {
            1 => ColorSpace::Rec709,
            2 => ColorSpace::Rec2020,
            _ => ColorSpace::Rec601,
        };
        let mut encryption = opt("x-ss-general.encryptionEnabled").unwrap_or(0);
        if opt("x-nv-general.featureFlags").unwrap_or(0) & NVFF_AUDIO_ENCRYPTION != 0 {
            encryption |= ENC_AUDIO;
        }
        let configured = opt("x-ml-video.configuredBitrateKbps").filter(|&b| b > 0);
        let configured_kbps = match configured {
            Some(b) => b,
            None => num("x-nv-vqos[0].bw.maximumBitrateKbps")?,
        };
        let refresh_x100 = opt("x-nv-video[0].clientRefreshRateX100").filter(|&r| r > 0);
        Ok(Self {
            width: num("x-nv-video[0].clientViewportWd")?,
            height: num("x-nv-video[0].clientViewportHt")?,
            fps: num("x-nv-video[0].maxFPS")?,
            refresh_x100,
            packet_size: num("x-nv-video[0].packetSize")? as usize,
            configured_kbps,
            codec,
            hdr: opt("x-nv-video[0].dynamicRangeMode").unwrap_or(0) == 1,
            chroma444: opt("x-ss-video[0].chromaSamplingType").unwrap_or(0) == 1,
            slices_per_frame: opt("x-nv-video[0].videoEncoderSlicesPerFrame").unwrap_or(1).max(1),
            max_ref_frames: opt("x-nv-video[0].maxNumReferenceFrames").unwrap_or(1),
            color_space,
            full_range: csc & 1 == 1,
            intra_refresh: opt("x-ss-video[0].intraRefresh").unwrap_or(0) == 1,
            audio_channels: opt("x-nv-audio.surround.numChannels").unwrap_or(2).clamp(2, 8) as u8,
            audio_high_quality: opt("x-nv-audio.surround.AudioQuality").unwrap_or(0) == 1,
            audio_packet_ms: opt("x-nv-aqos.packetDuration").unwrap_or(5).clamp(5, 20) as u8,
            encryption,
            min_fec_packets: opt("x-nv-vqos[0].fec.minRequiredFecPackets").unwrap_or(0) as usize,
        })
    }

    /// The video bitrate to encode at: what the person configured, less room for FEC, audio
    /// and headers (the same allowances other GameStream hosts make, so a given Moonlight
    /// setting fills the link about the same).
    pub fn video_kbps(&self, fec_percent: u8) -> u32 {
        let mut kbps = u64::from(self.configured_kbps) * 100 / (100 + u64::from(fec_percent));
        let audio = u64::from(if self.audio_high_quality { 256u32 } else { 96 }) * u64::from(self.audio_channels);
        kbps -= audio.min(kbps / 5);
        kbps -= 500.min(kbps / 10);
        kbps.max(500) as u32
    }

    /// The display refresh in mHz: the exact rate when the client gave it, else the fps.
    pub fn refresh_mhz(&self) -> u32 {
        match self.refresh_x100 {
            // Only when it agrees with the fps asked for (within 1%).
            Some(r) if (r as f64 / 100.0 / self.fps.max(1) as f64 - 1.0).abs() <= 0.01 => r * 10,
            _ => self.fps * 1000,
        }
    }

    /// The stream in Quiver's terms. `pads` are the pads plugged in at launch.
    pub fn caps_and_prefs(&self, pads: &[PadKind], fec_percent: u8) -> (ClientCaps, StreamPrefs) {
        let chroma = if self.chroma444 { Chroma::Yuv444 } else { Chroma::Yuv420 };
        let caps = ClientCaps {
            decoders: vec![DecoderCaps {
                codec: self.codec,
                // Moonlight doesn't say; it falls back to software itself.
                hardware: true,
                max_width: self.width,
                max_height: self.height,
                ten_bit: self.hdr,
                chroma: vec![chroma],
            }],
            display: DisplayCaps {
                width: self.width,
                height: self.height,
                refresh_mhz: self.refresh_mhz(),
                hdr: self.hdr,
                vrr: false,
                stereo_sbs: false,
                scale: 1.0,
            },
            audio_channels: self.audio_channels,
            pads: pads.to_vec(),
            microphone: false,
        };
        let prefs = StreamPrefs {
            codec: Some(self.codec),
            max_bitrate_kbps: Some(self.video_kbps(fec_percent)),
            fps: Some(self.fps),
            resolution: Some((self.width, self.height)),
            hdr: Some(self.hdr),
            pacing: Pacing::LowestLatency,
            chroma444: self.chroma444,
        };
        (caps, prefs)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An ANNOUNCE as Moonlight 6.x sends it for 1080p60 HEVC Main10, 5.1, 20 Mbit/s.
    pub const MOONLIGHT_ANNOUNCE: &str = "v=0\r\n\
o=android 0 14 IN IPv4 127.0.0.1\r\n\
s=NVIDIA Streaming Client\r\n\
a=x-ml-general.featureFlags:3 \r\n\
a=x-ss-general.encryptionEnabled:5 \r\n\
a=x-ss-video[0].chromaSamplingType:0 \r\n\
a=x-ss-video[0].intraRefresh:0 \r\n\
a=x-nv-video[0].clientViewportWd:1920 \r\n\
a=x-nv-video[0].clientViewportHt:1080 \r\n\
a=x-nv-video[0].maxFPS:60 \r\n\
a=x-nv-video[0].packetSize:1392 \r\n\
a=x-nv-video[0].rateControlMode:4 \r\n\
a=x-nv-video[0].timeoutLengthMs:7000 \r\n\
a=x-nv-video[0].framesWithInvalidRefThreshold:0 \r\n\
a=x-nv-video[0].initialBitrateKbps:16000 \r\n\
a=x-nv-video[0].initialPeakBitrateKbps:16000 \r\n\
a=x-nv-vqos[0].bw.minimumBitrateKbps:16000 \r\n\
a=x-nv-vqos[0].bw.maximumBitrateKbps:16000 \r\n\
a=x-ml-video.configuredBitrateKbps:20000 \r\n\
a=x-nv-vqos[0].fec.enable:1 \r\n\
a=x-nv-vqos[0].videoQualityScoreUpdateTime:5000 \r\n\
a=x-nv-vqos[0].qosTrafficType:5 \r\n\
a=x-nv-aqos.qosTrafficType:4 \r\n\
a=x-nv-general.featureFlags:167 \r\n\
a=x-nv-general.useReliableUdp:13 \r\n\
a=x-nv-vqos[0].fec.minRequiredFecPackets:2 \r\n\
a=x-nv-vqos[0].bllFec.enable:0 \r\n\
a=x-nv-vqos[0].drc.enable:0 \r\n\
a=x-nv-general.enableRecoveryMode:0 \r\n\
a=x-nv-video[0].videoEncoderSlicesPerFrame:1 \r\n\
a=x-nv-clientSupportHevc:1 \r\n\
a=x-nv-vqos[0].bitStreamFormat:1 \r\n\
a=x-nv-video[0].dynamicRangeMode:1 \r\n\
a=x-nv-video[0].maxNumReferenceFrames:0 \r\n\
a=x-nv-video[0].clientRefreshRateX100:5994 \r\n\
a=x-nv-audio.surround.numChannels:6 \r\n\
a=x-nv-audio.surround.channelMask:63 \r\n\
a=x-nv-audio.surround.enable:1 \r\n\
a=x-nv-audio.surround.AudioQuality:0 \r\n\
a=x-nv-aqos.packetDuration:5 \r\n\
a=x-nv-video[0].encoderCscMode:5 \r\n\
t=0 0\r\n\
m=video 47998  \r\n";

    #[test]
    fn parse_moonlight_announce() {
        let a = Announce::parse(MOONLIGHT_ANNOUNCE).unwrap();
        assert_eq!((a.width, a.height, a.fps, a.packet_size), (1920, 1080, 60, 1392));
        assert_eq!(a.codec, VideoCodec::Hevc);
        assert!(a.hdr && !a.chroma444 && !a.intra_refresh);
        assert_eq!(a.max_ref_frames, 0);
        assert_eq!(a.configured_kbps, 20000);
        assert_eq!((a.color_space, a.full_range), (ColorSpace::Rec2020, true));
        assert_eq!((a.audio_channels, a.audio_high_quality, a.audio_packet_ms), (6, false, 5));
        assert_eq!(a.encryption, ENC_CONTROL_V2 | ENC_AUDIO);
        assert_eq!(a.min_fec_packets, 2);
        assert_eq!(a.refresh_mhz(), 59940);
        // 20000 / 1.2 = 16666; minus 6 × 96 audio = 16090; minus 500 = 15590.
        assert_eq!(a.video_kbps(20), 15590);

        let (caps, prefs) = a.caps_and_prefs(&[PadKind::Xbox], 20);
        assert_eq!(caps.decoders[0].codec, VideoCodec::Hevc);
        assert!(caps.decoders[0].ten_bit && caps.display.hdr);
        assert_eq!((caps.display.width, caps.display.height, caps.display.refresh_mhz), (1920, 1080, 59940));
        assert_eq!(caps.audio_channels, 6);
        assert_eq!(caps.pads, [PadKind::Xbox]);
        assert_eq!(prefs.max_bitrate_kbps, Some(15590));
        assert_eq!(prefs.resolution, Some((1920, 1080)));
        assert_eq!(prefs.hdr, Some(true));
    }

    #[test]
    fn defaults_and_errors() {
        let sdp = "a=x-nv-video[0].clientViewportWd:1280 \r\na=x-nv-video[0].clientViewportHt:720 \r\na=x-nv-video[0].maxFPS:120 \r\na=x-nv-video[0].packetSize:1024 \r\na=x-nv-vqos[0].bw.maximumBitrateKbps:5000 \r\na=x-nv-general.featureFlags:39 \r\n";
        let a = Announce::parse(sdp).unwrap();
        assert_eq!(a.codec, VideoCodec::H264);
        assert_eq!(a.configured_kbps, 5000);
        assert_eq!(a.encryption, ENC_AUDIO, "old-style audio encryption flag");
        assert_eq!((a.audio_channels, a.audio_packet_ms), (2, 5));
        assert_eq!(a.refresh_mhz(), 120_000);
        assert_eq!(a.color_space, ColorSpace::Rec601);
        assert!(Announce::parse("a=x-nv-video[0].clientViewportWd:1280 \r\n").is_err());
        // A refresh that disagrees with the fps is ignored.
        let b = Announce::parse(&format!("{sdp}a=x-nv-video[0].clientRefreshRateX100:6000 \r\n")).unwrap();
        assert_eq!(b.refresh_mhz(), 120_000);
    }

    #[test]
    fn describe_lines() {
        let d = Describe {
            hevc: true,
            av1: true,
            reference_frame_invalidation: true,
            feature_flags: FF_PEN_TOUCH,
            encryption_supported: ENC_CONTROL_V2 | ENC_VIDEO | ENC_AUDIO,
            encryption_requested: ENC_CONTROL_V2,
        };
        let s = d.render();
        // Moonlight finds these with strstr.
        for needle in [
            "sprop-parameter-sets=AAAAAU",
            "AV1/90000",
            "x-nv-video[0].refPicInvalidation",
            "a=x-ss-general.featureFlags:1\n",
            "a=x-ss-general.encryptionSupported:7\n",
            "a=x-ss-general.encryptionRequested:1\n",
            "a=fmtp:97 surround-params=642014235\na=fmtp:97 surround-params=660012345\n",
        ] {
            assert!(s.contains(needle), "{needle} in\n{s}");
        }
        let plain = Describe { hevc: false, av1: false, reference_frame_invalidation: false, ..d }.render();
        assert!(!plain.contains("AAAAAU") && !plain.contains("AV1") && !plain.contains("refPic"));
    }
}
