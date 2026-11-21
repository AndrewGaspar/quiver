//! NVENC end to end on the real GPU: the test pattern encoded, then decoded by ffmpeg and read
//! back. Checked:
//!
//! 1. every codec decodes, and every frame's stamp reads back as its own seq;
//! 2. reference invalidation: drop two frames, report them lost, and every frame from the
//!    recovery frame on decodes **bit-exactly** as it does from the whole stream; the recovery
//!    frame has `refs_from_seq` past the loss and isn't a keyframe;
//! 3. a bitrate change mid-stream makes no keyframe;
//! 4. 10-bit HDR10 (HEVC, AV1, H.264 where the GPU can) carries BT.2020/PQ and the mastering
//!    display and content light level metadata; 4:4:4 comes out as 4:4:4.
//!
//! Opt-in, brief GPU use (a few seconds at 1080p or less):
//! `cargo test -p quiver-stream-host --test live_encode -- --ignored --nocapture`.
//! Skips when there's no NVIDIA driver or no ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use quiver_stream_host::cuda::Cuda;
use quiver_stream_host::recovery::Recovery;
use quiver_stream_host::video::{self, PatternSource, VideoCmd, VideoIo};
use quiver_stream_proto::stamp;
use quiver_stream_proto::{AudioCodec, Chroma, ColorInfo, EncodedFrame, HdrMetadata, StreamConfig, VideoCodec};

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/stream-host-test");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn available() -> bool {
    if Cuda::open().is_err() {
        eprintln!("skip: no CUDA driver");
        return false;
    }
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skip: no ffmpeg");
        return false;
    }
    true
}

fn cfg(codec: VideoCodec, w: u32, h: u32, color: ColorInfo) -> StreamConfig {
    StreamConfig { codec, width: w, height: h, refresh_mhz: 60_000, color, bitrate_kbps: 20_000, audio: AudioCodec::Opus, audio_channels: 2, audio_frame_ms: 5 }
}

/// Encode `n` frames; `script(seq)` gives commands to send before frame `seq` is made, and
/// a bitrate to switch to.
fn encode(c: &StreamConfig, n: u64, script: impl Fn(u64) -> (Vec<VideoCmd>, Option<u32>)) -> Vec<EncodedFrame> {
    let cuda = Cuda::open().unwrap();
    let (ftx, mut frx) = tokio::sync::mpsc::channel(1024);
    let (ctx, _crx) = tokio::sync::mpsc::channel(1024);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (btx, brx) = tokio::sync::watch::channel(c.bitrate_kbps);
    let stop = Arc::new(AtomicBool::new(false));
    // The script needs to run between frames, so the pattern runs one frame at a time.
    let mut out = Vec::new();
    let source = Box::new(PatternSource::new(c.width, c.height, 1_000_000, c.color.clone(), true));
    let io = VideoIo { frames: ftx, control: ctx, cmds: cmd_rx, bitrate: brx, stop: stop.clone(), max_frames: Some(n) };
    let base = video::base_config(c, 1, Recovery::RefInvalidation);
    let announce = c.clone();
    let (gate_tx, gate_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let th = std::thread::spawn(move || {
        // Hold the video thread to the script's pace: one frame per gate.
        let gated = Gated { inner: source, gate: gate_rx };
        video::run(cuda, Box::new(gated), base, announce, io)
    });
    for seq in 0..n {
        let (cmds, rate) = script(seq);
        for cmd in cmds {
            cmd_tx.send(cmd).unwrap();
        }
        if let Some(r) = rate {
            btx.send(r).unwrap();
        }
        gate_tx.send(()).unwrap();
        out.push(frx.blocking_recv().expect("a frame"));
    }
    drop(gate_tx);
    th.join().unwrap().unwrap();
    out
}

/// A source that makes one frame each time the test opens the gate.
struct Gated {
    inner: Box<PatternSource>,
    gate: std::sync::mpsc::Receiver<()>,
}

impl video::Source for Gated {
    fn format(&self) -> (u32, u32, quiver_stream_host::nvenc::InputFormat) {
        self.inner.format()
    }
    fn next(&mut self, cuda: &Arc<Cuda>, s: &mut quiver_stream_host::nvenc::Surface, seq: u64, stop: &AtomicBool) -> anyhow::Result<video::Next> {
        if self.gate.recv().is_err() {
            return Ok(video::Next::Stopped);
        }
        self.inner.next(cuda, s, seq, stop)
    }
}

fn ext(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "obu",
    }
}

/// Decode an elementary stream to 8-bit luma planes, one per frame.
fn decode_luma(path: &Path, w: u32, h: u32) -> Vec<Vec<u8>> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-f", "rawvideo", "-pix_fmt", "gray", "-"]).output().unwrap();
    assert!(o.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&o.stderr));
    let size = (w * h) as usize;
    assert_eq!(o.stdout.len() % size, 0);
    o.stdout.chunks(size).map(|c| c.to_vec()).collect()
}

fn write(frames: &[&EncodedFrame], name: &str) -> PathBuf {
    let p = scratch().join(name);
    let mut data = Vec::new();
    for f in frames {
        data.extend_from_slice(&f.data);
    }
    std::fs::write(&p, data).unwrap();
    p
}

fn read_stamp(luma: &[u8], w: u32, h: u32) -> Option<u64> {
    stamp::read(w, h, |x, y| luma[(y * w + x) as usize]).map(|s| s.seq)
}

#[test]
#[ignore]
fn every_codec_round_trips_with_stamps() {
    if !available() {
        return;
    }
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        let (w, h) = (1280, 720);
        let frames = encode(&cfg(codec, w, h, ColorInfo::sdr()), 30, |_| (vec![], None));
        assert!(frames[0].keyframe && frames[1..].iter().all(|f| !f.keyframe), "{codec:?}: one keyframe");
        assert!(frames.iter().enumerate().all(|(i, f)| f.seq == i as u64 && f.refs_from_seq == 0));
        let p = write(&frames.iter().collect::<Vec<_>>(), &format!("rt.{}", ext(codec)));
        let lumas = decode_luma(&p, w, h);
        assert_eq!(lumas.len(), 30, "{codec:?}");
        for (i, l) in lumas.iter().enumerate() {
            assert_eq!(read_stamp(l, w, h), Some(i as u64), "{codec:?} frame {i}");
        }
        eprintln!("{codec:?}: 30 frames, stamps all read back");
    }
}

#[test]
#[ignore]
fn reference_invalidation_recovers_bit_exactly_without_a_keyframe() {
    if !available() {
        return;
    }
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        let (w, h) = (640, 360);
        // The client "loses" 20 and 21; its report arrives before frame 23 is made (22 was
        // already sent, and depends on them).
        let frames = encode(&cfg(codec, w, h, ColorInfo::sdr()), 40, |seq| if seq == 23 { (vec![VideoCmd::Lost { since: 20, through: 21 }], None) } else { (vec![], None) });
        let rec = &frames[23];
        assert!(!rec.keyframe, "{codec:?}: recovery without a keyframe");
        assert_eq!(rec.refs_from_seq, 23, "{codec:?}");
        assert_eq!(frames[22].refs_from_seq, 0);
        assert!(frames[24..].iter().all(|f| f.refs_from_seq == 23));
        let all: Vec<&EncodedFrame> = frames.iter().collect();
        // What a client that lost 20 and 21 decodes: it drops 22 too (refs_from_seq 0 ≤ 21).
        let lossy: Vec<&EncodedFrame> = frames.iter().filter(|f| !(20..=22).contains(&f.seq)).collect();
        let full = decode_luma(&write(&all, &format!("rfi-full.{}", ext(codec))), w, h);
        let part = decode_luma(&write(&lossy, &format!("rfi-lossy.{}", ext(codec))), w, h);
        // ffmpeg may emit a concealed picture for a gap; match from the end.
        let tail = 40 - 23;
        assert!(part.len() >= tail, "{codec:?}: {} decoded", part.len());
        for k in 0..tail {
            let a = &full[full.len() - tail + k];
            let b = &part[part.len() - tail + k];
            assert_eq!(read_stamp(b, w, h), Some(23 + k as u64), "{codec:?}");
            assert!(a == b, "{codec:?}: frame {} differs from the full decode", 23 + k);
        }
        eprintln!("{codec:?}: lost 20-22, frame 23 recovered without a keyframe, 17 frames bit-exact");
    }
}

#[test]
#[ignore]
fn bitrate_changes_make_no_keyframe() {
    if !available() {
        return;
    }
    let c = cfg(VideoCodec::Hevc, 1280, 720, ColorInfo::sdr());
    let frames = encode(&c, 60, |seq| match seq {
        20 => (vec![], Some(2_000)),
        40 => (vec![], Some(40_000)),
        _ => (vec![], None),
    });
    assert_eq!(frames.iter().filter(|f| f.keyframe).count(), 1);
    let bytes = |r: std::ops::Range<usize>| frames[r].iter().map(|f| f.data.len()).sum::<usize>();
    eprintln!("bytes: 20 frames at 20 Mbit/s {}, at 2 Mbit/s {}, at 40 Mbit/s {}", bytes(1..20), bytes(22..40), bytes(42..60));
    assert!(bytes(22..40) < bytes(1..20), "the lower rate took effect");
    // Forced keyframe on request.
    let frames = encode(&c, 10, |seq| if seq == 5 { (vec![VideoCmd::Keyframe], None) } else { (vec![], None) });
    assert!(frames[5].keyframe && frames[5].refs_from_seq == 5 && frames[6].refs_from_seq == 5);
}

fn probe(path: &Path) -> String {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-show_frames", "-read_intervals", "%+#1", "-select_streams", "v"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
#[ignore]
fn hdr10_and_444() {
    if !available() {
        return;
    }
    let hdr = HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 1000, max_fall: 400 };
    let color = ColorInfo { bit_depth: 10, chroma: Chroma::Yuv420, full_range: false, hdr: Some(hdr) };
    let cuda = Cuda::open().unwrap();
    drop(cuda);
    for codec in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
        let c = cfg(codec, 1280, 720, color.clone());
        let frames = match std::panic::catch_unwind(|| encode(&c, 5, |_| (vec![], None))) {
            Ok(f) => f,
            Err(_) if codec == VideoCodec::H264 => {
                eprintln!("H264: 10-bit not supported here");
                continue;
            }
            Err(e) => std::panic::resume_unwind(e),
        };
        let p = write(&frames.iter().collect::<Vec<_>>(), &format!("hdr.{}", ext(codec)));
        let s = probe(&p);
        eprintln!("{codec:?} HDR10:\n{}", s.lines().filter(|l| l.contains("color_") || l.contains("pix_fmt") || l.contains("luminance") || l.contains("max_content") || l.contains("side_data_type")).collect::<Vec<_>>().join("\n"));
        assert!(s.contains("color_transfer=smpte2084"), "{codec:?}");
        assert!(s.contains("color_primaries=bt2020"), "{codec:?}");
        assert!(s.contains("pix_fmt=yuv420p10le"), "{codec:?}");
        assert!(s.contains("Mastering display metadata"), "{codec:?}: mastering display");
        assert!(s.contains("max_content=1000"), "{codec:?}: content light level");
        let nits = s.lines().find_map(|l| l.strip_prefix("max_luminance=")).and_then(|f| f.split_once('/')).map(|(a, b)| a.parse::<f64>().unwrap() / b.parse::<f64>().unwrap());
        assert!(nits.is_some_and(|n| (n - 1000.0).abs() < 1.0), "{codec:?}: max luminance {nits:?}");
        let lumas = decode_luma(&p, 1280, 720);
        assert_eq!(read_stamp(&lumas[3], 1280, 720), Some(3));
    }
    for codec in [VideoCodec::Hevc, VideoCodec::H264] {
        let c = cfg(codec, 1280, 720, ColorInfo { bit_depth: 8, chroma: Chroma::Yuv444, full_range: false, hdr: None });
        let frames = encode(&c, 5, |_| (vec![], None));
        let p = write(&frames.iter().collect::<Vec<_>>(), &format!("444.{}", ext(codec)));
        let s = probe(&p);
        assert!(s.contains("pix_fmt=yuv444p"), "{codec:?}: {s}");
        eprintln!("{codec:?}: 4:4:4 ok");
    }
}
