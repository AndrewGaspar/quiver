//! [`StreamHostAdapter`]: a [`StreamHost`] over `quiver-stream-host`'s [`Host`], which is
//! what quiverd uses to serve Moonlight (feature `stream-host`).
//!
//! A GameStream stream becomes a [`HostOptions`]: the negotiated codec, size, rate, colour,
//! bitrate and audio go into its [`StreamConfig`]; reference frame invalidation when
//! Moonlight decodes with it, keyframes otherwise. The host is kept across sessions
//! ([`crate::keep`]): a client leaving leaves it running for `keep` (10 minutes), a resume
//! attaches the next session to it, and Quit app or the program's end stops it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use quiver_stream_host::host::{Host, HostOptions, Target};
use quiver_stream_host::recovery::Recovery;
use quiver_stream_proto::{AudioCodec, ColorInfo, HdrMetadata, StreamConfig, now_micros};
use tokio::sync::{mpsc, watch};

use crate::host::{App, HostEnds, StreamHost, StreamRequest};
use crate::keep::{HostChannels, Hooks, Slot};

/// Decides what a stream of `app` shows: the game's command, or the test pattern.
pub type TargetFor = dyn Fn(&App) -> Result<Target> + Send + Sync;

pub struct StreamHostAdapter {
    target: Box<TargetFor>,
    /// Each stream's folder is made under this one.
    dir: PathBuf,
    /// The machine is a headless server (pads may be ordinary devices; see the host crate).
    pub headless_server: bool,
    /// NVENC preset, 1 (fastest) to 7.
    pub preset: u8,
    /// `quiver-stream-host`, for the pad namespace (None: this executable).
    pub exe: Option<PathBuf>,
    /// How long a host is kept after its client goes.
    pub keep: Duration,
    slot: Slot,
}

/// The host's channels, for a [`crate::keep::Kept`] (the `Host` keeps closed stand-ins);
/// `bitrate` is the sender of the receiver the host was started with.
pub fn take_channels(host: &mut Host, bitrate: watch::Sender<u32>) -> HostChannels {
    fn closed<T>() -> mpsc::Receiver<T> {
        mpsc::channel(1).1
    }
    HostChannels {
        frames: std::mem::replace(&mut host.frames, closed()),
        audio: std::mem::replace(&mut host.audio, closed()),
        control_out: std::mem::replace(&mut host.control_out, closed()),
        input: host.input.clone(),
        control_in: host.control_in.clone(),
        bitrate,
    }
}

/// Hooks for a plain host: the program's end ends it, and stopping stops the host.
pub fn host_hooks(host: Arc<Mutex<Option<Host>>>) -> Hooks {
    let h = host.clone();
    Hooks {
        tick: Box::new(move || {
            let mut g = h.lock().unwrap_or_else(|e| e.into_inner());
            g.as_mut().is_some_and(|h| !h.alive()).then(|| "the program exited".to_owned())
        }),
        stop: Box::new(move |_, _| {
            Box::pin(async move {
                let h = host.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(h) = h {
                    match h.stop().await {
                        Ok(r) => tracing::info!(?r, "stream host stopped"),
                        Err(e) => tracing::warn!("stream host stop: {e:#}"),
                    }
                }
            })
        }),
        events: None,
    }
}

/// HDR10 mastering metadata for when the game hasn't said: BT.2020 primaries, D65, 1000 nits.
pub fn default_hdr() -> HdrMetadata {
    HdrMetadata {
        primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
        white_point: [15635, 16450],
        max_luminance: 10_000_000,
        min_luminance: 50,
        max_cll: 1000,
        max_fall: 400,
    }
}

impl StreamHostAdapter {
    pub fn new(dir: impl Into<PathBuf>, target: impl Fn(&App) -> Result<Target> + Send + Sync + 'static) -> Self {
        Self { target: Box::new(target), dir: dir.into(), headless_server: false, preset: 1, exe: None, keep: crate::keep::KEEP, slot: Slot::default() }
    }

    /// The host's settings for a GameStream request.
    pub fn config_for(req: &StreamRequest) -> StreamConfig {
        let v = &req.video;
        StreamConfig {
            codec: v.codec,
            width: v.width,
            height: v.height,
            refresh_mhz: v.fps * 1000,
            color: ColorInfo { bit_depth: v.bit_depth, chroma: v.chroma, full_range: v.full_range, hdr: v.hdr.then(default_hdr) },
            bitrate_kbps: v.bitrate_kbps,
            audio: AudioCodec::Opus,
            audio_channels: req.audio.opus.channels,
            audio_frame_ms: req.audio.packet_ms,
        }
    }
}

impl StreamHost for StreamHostAdapter {
    fn start(&self, req: StreamRequest, ends: HostEnds) -> Result<()> {
        let target = (self.target)(&req.app)?;
        let session = format!("moonlight-{}", std::process::id() ^ (now_micros() as u32));
        let opts = HostOptions {
            target,
            config: Self::config_for(&req),
            dir: self.dir.join(&session),
            session,
            preset: self.preset,
            recovery: if req.video.reference_frame_invalidation { Recovery::RefInvalidation } else { Recovery::Keyframe },
            headless_server: self.headless_server,
            max_frames: None,
            force_sandbox_gid: None,
            exe: self.exe.clone(),
            constant_bitrate_audio: req.audio.constant_bitrate,
        };
        self.slot.start(req, ends, self.keep, move |req| async move {
            let (bitrate, rx) = watch::channel(req.video.bitrate_kbps);
            let mut host = Host::start(opts, rx).await?;
            let chans = take_channels(&mut host, bitrate);
            Ok((chans, host_hooks(Arc::new(Mutex::new(Some(host))))))
        })
    }

    fn quit(&self, app_key: &str) {
        self.slot.quit(app_key, "the client quit the app");
    }

    fn running(&self) -> Option<String> {
        self.slot.running()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::OpusLayout;
    use crate::host::{AudioParams, ColorSpace, VideoParams};
    use quiver_stream_proto::{Chroma, VideoCodec};

    #[test]
    fn config_from_request() {
        let sdp = crate::sdp::Announce::parse(crate::sdp::tests::MOONLIGHT_ANNOUNCE).unwrap();
        let (caps, prefs) = sdp.caps_and_prefs(&[], 20);
        let req = StreamRequest {
            app: App { key: "k".into(), title: "t".into(), hdr: true },
            client_name: "c".into(),
            client_fingerprint: "f".into(),
            caps,
            prefs,
            video: VideoParams {
                codec: VideoCodec::Hevc,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 15590,
                bit_depth: 10,
                chroma: Chroma::Yuv420,
                hdr: true,
                color_space: ColorSpace::Rec2020,
                full_range: false,
                slices_per_frame: 1,
                reference_frame_invalidation: true,
                intra_refresh: false,
            },
            audio: AudioParams { opus: OpusLayout::for_channels(6, false), packet_ms: 10, constant_bitrate: true },
        };
        let c = StreamHostAdapter::config_for(&req);
        assert_eq!((c.width, c.height, c.refresh_mhz, c.bitrate_kbps), (1920, 1080, 60_000, 15590));
        assert_eq!((c.color.bit_depth, c.color.hdr.is_some()), (10, true));
        assert_eq!((c.audio_channels, c.audio_frame_ms), (6, 10));
    }

    #[test]
    fn host_encoder_layouts_match_what_moonlight_is_told() {
        // The host's own Opus encoder, made as it makes it, re-indexed into Moonlight's order.
        for ch in [6u8, 8] {
            let e = quiver_stream_host::opus::Encoder::new(ch, 50, 256_000).unwrap();
            let ours = OpusLayout::for_channels(ch, false);
            assert_eq!((e.streams, e.coupled), (ours.streams, ours.coupled_streams), "{ch} channels");
            let remapped: Vec<u8> = OpusLayout::vorbis_index(ch).iter().map(|&v| e.mapping[v]).collect();
            assert_eq!(remapped, ours.mapping, "{ch} channels");
        }
    }
}
