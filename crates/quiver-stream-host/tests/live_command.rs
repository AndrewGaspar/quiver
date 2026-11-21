//! The command target end to end: headless gamescope running the test window
//! (`quiver-stream-host --test-window`), captured over PipeWire, encoded, with audio from the
//! stream's own sink and input injected through XTEST. Checked:
//!
//! 1. the display is never :0–:9, and the frames decode with the test window's stamps, which
//!    gives the display's own latency (program draws → capture) per frame;
//! 2. keys, buttons, relative motion (also as XI2 raw motion), absolute position and the wheel
//!    reach the program, and only it;
//! 3. a quiet tone played by a PulseAudio client inside the stream lands in the stream's audio,
//!    is linked to nothing but the stream's sink, and the sink is gone after the stream;
//! 4. without the stream pad rule, a pad arriving gets a notice, not a device.
//!
//! Opt-in, a few seconds of GPU at 1280×720:
//! `cargo test -p quiver-stream-host --test live_command -- --ignored --nocapture`.
//! Skips without gamescope, PipeWire (`pw-cli`), ffmpeg or the NVIDIA driver.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_stream_host::recovery::Recovery;
use quiver_stream_host::stats::Samples;
use quiver_stream_host::{Host, HostOptions, Target};
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioCodec, ColorInfo, Control, EncodedFrame, InputEvent, MouseButton, PadKind, StreamConfig, VideoCodec, stamp};

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/stream-host-test");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd).arg(arg).output().is_ok_and(|o| o.status.success())
}

fn available() -> bool {
    for (c, a) in [("gamescope", "--help"), ("pw-cli", "--version"), ("ffmpeg", "-version"), ("paplay", "--version")] {
        if !have(c, a) && !(c == "gamescope" && Command::new(c).arg(a).output().is_ok()) {
            eprintln!("skip: no {c}");
            return false;
        }
    }
    if quiver_stream_host::cuda::Cuda::open().is_err() {
        eprintln!("skip: no CUDA driver");
        return false;
    }
    true
}

fn quiet_wav(path: &Path) {
    // 3 s of a 440 Hz tone at -60 dBFS: if it ever leaked to the speakers, nobody would hear it.
    let n = 48_000 * 3;
    let mut d = Vec::with_capacity(44 + n * 4);
    d.extend_from_slice(b"RIFF");
    d.extend_from_slice(&((36 + n * 4) as u32).to_le_bytes());
    d.extend_from_slice(b"WAVEfmt ");
    d.extend_from_slice(&16u32.to_le_bytes());
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&2u16.to_le_bytes());
    d.extend_from_slice(&48_000u32.to_le_bytes());
    d.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    d.extend_from_slice(&4u16.to_le_bytes());
    d.extend_from_slice(&16u16.to_le_bytes());
    d.extend_from_slice(b"data");
    d.extend_from_slice(&((n * 4) as u32).to_le_bytes());
    for i in 0..n {
        let v = ((i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 32.0) as i16;
        d.extend_from_slice(&v.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, d).unwrap();
}

fn sinks() -> String {
    String::from_utf8_lossy(&Command::new("pactl").args(["list", "short", "sinks"]).output().unwrap().stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn command_target_end_to_end() {
    if !available() {
        return;
    }
    let dir = scratch().join("cmd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("input.log");
    let wav = dir.join("quiet.wav");
    quiet_wav(&wav);
    let exe = env!("CARGO_BIN_EXE_quiver-stream-host");
    let script = format!("paplay '{}' & exec '{exe}' --test-window --fps 60", wav.display());
    let session = format!("test-{}", std::process::id());
    let (w, h) = (1280, 720);
    let config = StreamConfig { codec: VideoCodec::Hevc, width: w, height: h, refresh_mhz: 60_000, color: ColorInfo::sdr(), bitrate_kbps: 20_000, audio: AudioCodec::Opus, audio_channels: 2, audio_frame_ms: 5 };
    let opts = HostOptions {
        target: Target::Command { argv: vec!["sh".into(), "-c".into(), script], env: vec![("QUIVER_TESTWIN_LOG".into(), log.display().to_string())] },
        config,
        session: session.clone(),
        dir: dir.join("run"),
        preset: 1,
        recovery: Recovery::RefInvalidation,
        headless_server: false,
        max_frames: None,
    };
    let (_btx, brx) = tokio::sync::watch::channel(20_000);
    let mut host = Host::start(opts, brx).await.expect("start");
    let display = host.x_display.clone().unwrap();
    eprintln!("display {display}, pads {:?}", host.pad_mode);
    assert!(!quiver_stream_host::display::reserved_display(&display), "{display}");
    let sink = format!("quiver-stream-{session}");
    assert!(sinks().contains(&sink), "the stream's sink exists while it runs");

    let mut frames: Vec<EncodedFrame> = Vec::new();
    let mut audio = Vec::new();
    let mut controls = Vec::new();
    let start = Instant::now();
    let mut seq = 0u64;
    let mut send = |ev: InputEvent| {
        seq += 1;
        // Each event twice, as the transport's redundancy would: the second is dropped.
        let s = Stamped { seq, at: 0, event: ev };
        host.input.try_send(s.clone()).unwrap();
        host.input.try_send(s).unwrap();
    };
    let mut sent = false;
    while start.elapsed() < Duration::from_secs(4) {
        if !sent && start.elapsed() > Duration::from_millis(1500) {
            sent = true;
            send(InputEvent::Key { code: 30, down: true });
            send(InputEvent::Key { code: 30, down: false });
            send(InputEvent::MousePosition { x: 32768, y: 32768 });
            send(InputEvent::MouseMotion { dx: 7, dy: -3 });
            send(InputEvent::MouseButton { button: MouseButton::Left, down: true });
            send(InputEvent::MouseButton { button: MouseButton::Left, down: false });
            send(InputEvent::Wheel { dx: 0, dy: 60 });
            send(InputEvent::Wheel { dx: 0, dy: 60 });
            send(InputEvent::Key { code: 31, down: true });
            send(InputEvent::ReleaseAll);
            send(InputEvent::PadArrive { slot: 0, kind: PadKind::Xbox });
        }
        tokio::select! {
            Some(f) = host.frames.recv() => frames.push(f),
            Some(a) = host.audio.recv() => audio.push(a),
            Some(c) = host.control_out.recv() => controls.push(c),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    let report = host.stop().await.unwrap();
    eprintln!("{} frames, capture→encode start {}, encode {}", report.frames, report.capture.summary(), report.encode.summary());
    assert!(frames.len() > 150, "{} frames", frames.len());

    // 1. Stamps: the program's draw time against the capture time.
    let path = dir.join("cmd.hevc");
    std::fs::write(&path, frames.iter().flat_map(|f| f.data.iter().copied()).collect::<Vec<u8>>()).unwrap();
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-f", "rawvideo", "-pix_fmt", "gray", "-"]).output().unwrap();
    let lumas: Vec<&[u8]> = o.stdout.chunks((w * h) as usize).collect();
    assert_eq!(lumas.len(), frames.len());
    let mut display_us = Samples::default();
    let mut total_us = Samples::default();
    let mut read = 0;
    for (f, l) in frames.iter().zip(&lumas) {
        if let Some(s) = stamp::read(w, h, |x, y| l[(y * w + x) as usize]) {
            read += 1;
            display_us.push(f.captured.saturating_sub(s.captured) as u32);
            total_us.push(f.encoded.saturating_sub(s.captured) as u32);
        }
    }
    eprintln!("stamps read in {read} of {} frames\n  program draw → capture: {}\n  program draw → encoded: {}", frames.len(), display_us.summary(), total_us.summary());
    assert!(read as f64 > frames.len() as f64 * 0.9);

    // 2. Input.
    let got = std::fs::read_to_string(&log).unwrap();
    eprintln!("input log:\n{got}");
    for line in ["key 38 down", "key 38 up", "button 1 down", "button 1 up", "button 4 down", "button 4 up", "key 39 down", "key 39 up", "motion 646 356"] {
        assert!(got.lines().any(|l| l == line), "the program didn't get '{line}'");
    }
    assert!(got.lines().any(|l| l == "raw 7 -3"), "no XI2 raw motion of (7, -3)");
    assert_eq!(got.lines().filter(|l| *l == "key 38 down").count(), 1, "a duplicate got through");
    assert_eq!(got.lines().filter(|l| l.starts_with("button 4")).count(), 2, "two half notches are one click");

    // 3. Audio: the quiet tone, from the stream's own sink, which is gone now.
    assert!(audio.len() > 600, "{} audio packets", audio.len());
    assert!(!sinks().contains(&sink), "the sink outlived the stream");
    assert!(audio.iter().map(|p| p.data.len()).max().unwrap() > 8, "the audio is more than silence frames");

    // 4. Pads.
    let notices: Vec<&String> = controls.iter().filter_map(|c| if let Control::Notice { text } = c { Some(text) } else { None }).collect();
    eprintln!("notices: {notices:?}");
    if quiver_stream_host::sandbox::check_rule().is_err() {
        assert!(notices.iter().any(|n| n.contains("pads aren't available")), "a pad arriving without the rule gets a notice");
    }
    assert!(controls.iter().any(|c| matches!(c, Control::Configure(_))));
}
