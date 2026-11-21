//! Input into a Windows program under Wine, in the stream's gamescope: the keys, clicks, wheel
//! and **raw mouse motion** (`WM_INPUT`, which Wine builds from XI2 raw events) a game would
//! read. The program is `tests/wine/input_probe.c`, built with mingw; it runs under the system
//! `wine` in a scratch prefix (`target/stream-host-test/wineprefix`), never one of the
//! person's.
//!
//! Opt-in: `cargo test -p quiver-stream-host --test live_wine -- --ignored --nocapture`.
//! Skips without wine, mingw, gamescope or the NVIDIA driver. The first run makes the prefix
//! (half a minute).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_stream_host::recovery::Recovery;
use quiver_stream_host::{Host, HostOptions, Target};
use quiver_stream_proto::input::Stamped;
use quiver_stream_proto::{AudioCodec, ColorInfo, InputEvent, MouseButton, StreamConfig, VideoCodec};

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/stream-host-test");
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_wine_program_gets_keys_clicks_wheel_and_raw_motion() {
    for (c, a) in [("wine", "--version"), ("x86_64-w64-mingw32-gcc", "--version"), ("gamescope", "--help")] {
        if Command::new(c).arg(a).output().is_err() {
            eprintln!("skip: no {c}");
            return;
        }
    }
    if quiver_stream_host::cuda::Cuda::open().is_err() {
        eprintln!("skip: no CUDA driver");
        return;
    }
    let dir = scratch().join("wine");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("input_probe.exe");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wine/input_probe.c");
    let st = Command::new("x86_64-w64-mingw32-gcc").args(["-mwindows", "-O2"]).arg(&src).arg("-o").arg(&exe).status().unwrap();
    assert!(st.success(), "build the probe");
    let prefix = scratch().join("wineprefix");
    let wine_env = [("WINEPREFIX", prefix.display().to_string()), ("WINEDLLOVERRIDES", "mscoree,mshtml=".into()), ("WINEDEBUG", "-all".into())];
    if !prefix.join("system.reg").exists() {
        let t = Instant::now();
        let st = Command::new("wineboot").arg("-i").envs(wine_env.iter().cloned()).env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").status().unwrap();
        assert!(st.success(), "make the scratch prefix");
        let _ = Command::new("wineserver").arg("-w").envs(wine_env.iter().cloned()).status();
        eprintln!("prefix made in {:.0?}", t.elapsed());
    }
    let log = dir.join("input.log");
    let _ = std::fs::remove_file(&log);
    let winlog = format!("Z:{}", log.display().to_string().replace('/', "\\"));
    let config = StreamConfig { codec: VideoCodec::H264, width: 1280, height: 720, refresh_mhz: 60_000, color: ColorInfo::sdr(), bitrate_kbps: 10_000, audio: AudioCodec::Opus, audio_channels: 2, audio_frame_ms: 5 };
    let opts = HostOptions {
        target: Target::Command { argv: vec!["wine".into(), exe.display().to_string(), winlog], env: wine_env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() },
        config,
        session: format!("wine-{}", std::process::id()),
        dir: dir.join("run"),
        preset: 1,
        recovery: Recovery::RefInvalidation,
        headless_server: false,
        max_frames: None,
        force_sandbox_gid: None,
        exe: Some(PathBuf::from(env!("CARGO_BIN_EXE_quiver-stream-host"))),
    };
    let (_btx, brx) = tokio::sync::watch::channel(10_000);
    let mut host = Host::start(opts, brx).await.expect("start");
    // Wait for the probe's window.
    let t = Instant::now();
    while !std::fs::read_to_string(&log).unwrap_or_default().contains("ready") {
        assert!(t.elapsed() < Duration::from_secs(60), "the probe never started");
        while host.frames.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut seq = 0;
    let mut send = |ev: InputEvent| {
        seq += 1;
        host.input.try_send(Stamped { seq, at: 0, event: ev }).unwrap();
    };
    send(InputEvent::MousePosition { x: 32768, y: 32768 });
    send(InputEvent::MouseButton { button: MouseButton::Left, down: true });
    send(InputEvent::MouseButton { button: MouseButton::Left, down: false });
    send(InputEvent::Key { code: 30, down: true });
    send(InputEvent::Key { code: 30, down: false });
    for _ in 0..10 {
        send(InputEvent::MouseMotion { dx: 5, dy: -2 });
    }
    send(InputEvent::Wheel { dx: 0, dy: 120 });
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        while host.frames.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    host.stop().await.unwrap();
    let _ = Command::new("wineserver").arg("-k").envs(wine_env.iter().cloned()).status();
    let got = std::fs::read_to_string(&log).unwrap();
    eprintln!("the probe got:\n{got}");
    for line in ["lbutton down", "lbutton up", "keydown 65", "keyup 65", "wheel 120"] {
        assert!(got.lines().any(|l| l == line), "no '{line}'");
    }
    let (dx, dy) = got.lines().filter_map(|l| l.strip_prefix("raw ")).map(|r| {
        let mut it = r.split(' ').map(|v| v.parse::<i64>().unwrap());
        (it.next().unwrap(), it.next().unwrap())
    }).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    assert_eq!((dx, dy), (50, -20), "raw motion adds up to what was sent");
}
