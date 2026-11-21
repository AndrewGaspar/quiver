//! [`StreamHostAdapter`]: a [`StreamHost`] over `quiver-stream-host`'s [`Host`], which is
//! what quiverd uses to serve Moonlight (feature `stream-host`).
//!
//! A GameStream stream becomes a [`HostOptions`]: the negotiated codec, size, rate, colour,
//! bitrate and audio go into its [`StreamConfig`]; reference frame invalidation when
//! Moonlight decodes with it, keyframes otherwise. Then the channels are pumped both ways:
//! frames, audio and host controls to the client; input (stamped here) and loss reports to
//! the host. When either side goes, the host is stopped.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use quiver_stream_host::host::{Host, HostOptions, Target};
use quiver_stream_host::recovery::Recovery;
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioCodec, ColorInfo, Control, HdrMetadata, StreamConfig, now_micros};

use crate::host::{App, HostEnds, StreamHost, StreamRequest};

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
    current: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
        Self { target: Box::new(target), dir: dir.into(), headless_server: false, preset: 1, exe: None, current: Mutex::new(None) }
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
        let task = tokio::spawn(async move {
            let HostEnds { video, audio, mut input, mut control_in, control_out, trigger_rumble: _, bitrate_kbps } = ends;
            let mut host = match Host::start(opts, bitrate_kbps).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("the stream host didn't start: {e:#}");
                    let _ = control_out.send(Control::Bye { reason: format!("the host didn't start: {e:#}") }).await;
                    return;
                }
            };
            let mut seq = 0u64;
            loop {
                tokio::select! {
                    f = host.frames.recv() => match f {
                        Some(f) => if video.send(f).await.is_err() { break },
                        None => break,
                    },
                    Some(a) = host.audio.recv() => { let _ = audio.try_send(a); }
                    Some(c) = host.control_out.recv() => { let _ = control_out.try_send(c); }
                    e = input.recv() => match e {
                        Some(event) => {
                            seq += 1;
                            let _ = host.input.send(Stamped { seq, at: now_micros(), event }).await;
                        }
                        None => break,
                    },
                    c = control_in.recv() => match c {
                        Some(Control::Bye { .. }) | None => break,
                        Some(c) => { let _ = host.control_in.send(c).await; }
                    },
                }
            }
            match host.stop().await {
                Ok(r) => tracing::info!(?r, "stream host stopped"),
                Err(e) => tracing::warn!("stream host stop: {e:#}"),
            }
        });
        if let Some(old) = self.current.lock().unwrap_or_else(|e| e.into_inner()).replace(task) {
            old.abort();
        }
        Ok(())
    }

    fn quit(&self, _app_key: &str) {
        if let Some(t) = self.current.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
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
