//! A streamed game opens its stream's pads, through quiverd's real Play path: a scratch
//! quiverd with a test program (`QUIVER_TEST_GAMES`) launched into a `StartStream` game
//! stream's display exactly as a store game is (`launcher::play_store_game`: the launch plan,
//! profile, mods, `LaunchEnv`), a client on the real transport sending a pad, and the
//! program reading the pad's events:
//!
//! 1. **with a mod profile** (overlays: the launch's own namespace, as the person's Pharos
//!    profile has): the program sees the mod, has the stream pad group, opens the pad and
//!    reads it. This is what failed: the launch's namespace mapped only the person's own uid
//!    and gid, so the game couldn't open the pad the host made;
//! 2. without mods: the same;
//! 3. every process of the launch has the group (quiverd's audit, and a stand-in for Wine's
//!    own processes that the program's "Proton" leaves running).
//!
//! The "Proton" here is a stand-in (`proton run X` runs the program X natively), so the test
//! needs no Wine and no real prefix; everything is under the test's own folder.
//!
//! Opt-in (the GPU, briefly; gamescope; the stream pad rule):
//! `cargo build -p quiver-stream-host && cargo test -p quiverd --test stream_pads -- --ignored --nocapture`.
//! Skips without gamescope, the NVIDIA driver, the stream pad rule or `quiver-stream-host`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quiver_api::remote::{ClientCaps, StartStreamReply, StreamPrefs, StreamSession, StreamTarget};
use quiver_api::{Client, Request};
use quiver_stream_net::{ClientOpts, ClientSession, SessionKeys, VideoEvent};
use quiver_stream_proto::{BYE_QUIT_GAME, Control, DisplayCaps, InputEvent, PROTOCOL_VERSION, PadButtons, PadKind, PadState};
use serde_json::json;

const GAME: &str = "epic:quiverpadprobe";

/// "Proton": `proton run Z:\…\x.exe ARGS` runs x.exe (a shell script) natively, with
/// WINEPREFIX set as Proton sets it, and leaves a stand-in for Wine's own processes running.
const FAKE_PROTON: &str = r#"#!/usr/bin/env python3
import os, subprocess, sys
compat = os.environ["STEAM_COMPAT_DATA_PATH"]
os.environ["WINEPREFIX"] = os.path.join(compat, "pfx")
with open(os.path.join(compat, "fake-proton.log"), "a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")
args = sys.argv[2:]
if not args or args[0].lower() in ("wineboot", "reg"):
    sys.exit(0)
exe = args[0]
if exe.startswith("Z:"):
    exe = exe[2:].replace("\\", "/")
# Wine's services (the wineserver, winedevice) stay up beside the game.
subprocess.Popen(["sleep", "60"], stdout=subprocess.DEVNULL)
os.execvp("sh", ["sh", exe] + args[1:])
"#;

/// "wineserver": `-k` ends the prefix's processes (everything "Proton" started for it), `-w`
/// waits for them, as the real one does.
const FAKE_WINESERVER: &str = r#"#!/usr/bin/env python3
import os, signal, sys, time
pfx = os.path.realpath(os.environ["WINEPREFIX"])
def mine():
    out = []
    for p in os.listdir("/proc"):
        if not p.isdigit() or int(p) == os.getpid():
            continue
        try:
            env = open(f"/proc/{p}/environ", "rb").read().split(b"\0")
        except OSError:
            continue
        if any(e.startswith(b"STEAM_COMPAT_DATA_PATH=") and os.path.realpath(e.split(b"=", 1)[1].decode()) == pfx for e in env):
            out.append(int(p))
    return out
if "-k" in sys.argv:
    for p in mine():
        try:
            os.kill(p, signal.SIGKILL)
        except OSError:
            pass
for _ in range(100):
    if not mine():
        break
    time.sleep(0.1)
"#;

/// The game: wait for the stream pad, open it, log what it read.
const PROBE: &str = r#"
log="$1"; echo "groups $(id -G)" >> "$log"
[ -e "$(dirname "$0")/from-the-mod.txt" ] && echo "mod mounted" >> "$log"
for i in $(seq 400); do
  for n in /sys/class/input/event*; do
    case "$(cat $n/device/name 2>/dev/null)" in
      "Quiver Stream Pad "*canary*) ;;
      "Quiver Stream Pad "*)
        dev=/dev/input/${n##*/}
        if exec 3<"$dev"; then
          echo "opened $dev" >> "$log"
          n=$(timeout 8 dd bs=24 count=8 <&3 2>/dev/null | wc -c); echo "read $n bytes" >> "$log"
        else
          echo "refused $dev" >> "$log"
        fi
        sleep 30; exit 0;;
    esac
  done
  sleep 0.1
done
echo "no pad" >> "$log"
"#;

/// The scratch daemon, and on the way out every stream host it started (they outlive
/// quiverd by design), found by their records under its runtime folder.
struct Daemon(Child, PathBuf);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        for e in std::fs::read_dir(self.1.join("quiver/streams")).into_iter().flatten().flatten() {
            let rec: Option<serde_json::Value> = std::fs::read(e.path().join("stream.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
            if let Some(pid) = rec.and_then(|r| r["session"]["pid"].as_i64()).filter(|&p| p > 1) {
                // SAFETY: a signal to a process this test started.
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn executable(p: &Path, s: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(p, s);
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--help").output().is_ok()
}

/// One stream of the test game (with `profile`), a pad sent, and what the program saw.
async fn stream_and_press(q: &Client, root: &Path, profile: Option<&str>) -> (String, Vec<String>) {
    let log = root.join("game/pad.log");
    let _ = std::fs::remove_file(&log);
    let display = DisplayCaps { width: 1280, height: 720, refresh_mhz: 60_000, hdr: false, vrr: false, stereo_sbs: false, scale: 1.0 };
    let caps = ClientCaps { decoders: vec![], display, audio_channels: 2, pads: vec![PadKind::Xbox], microphone: false };
    let target = StreamTarget::Game { game: GAME.into(), profile: profile.map(str::to_owned) };
    // The last launch may still be winding down (its job ends after the prefix is shut).
    let t = Instant::now();
    let reply: StartStreamReply = loop {
        match q.call(Request::StartStream { target: target.clone(), prefs: StreamPrefs::default(), caps: caps.clone() }).await {
            Ok(r) => break r,
            // (or the GPU is short of memory for a moment: the person's game may hold most of it)
            Err(e) if (format!("{e:#}").contains("already running") || format!("{e:#}").contains("out of memory")) && t.elapsed() < Duration::from_secs(40) => {
                eprintln!("retrying: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await
            }
            Err(e) => panic!("start the stream: {e:#}"),
        }
    };
    let keys = SessionKeys::new(reply.ticket.session_id, reply.ticket.key);
    let client = ClientSession::connect(reply.ticket.addr, keys, ClientOpts::default()).await.expect("connect");
    client.send_control(Control::Hello { version: PROTOCOL_VERSION, caps, prefs: StreamPrefs::default(), target }).unwrap();
    let (mut frames, mut notices, mut pad_sent, mut presses) = (0, Vec::new(), false, 0u32);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        tokio::select! {
            c = client.recv_control() => match c {
                Some(Control::Notice { text }) => { eprintln!("notice: {text}"); notices.push(text) }
                Some(_) => {}
                None => break,
            },
            v = client.recv_video() => if let Some(VideoEvent::Frame(_)) = v { frames += 1 },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        // The pad once the game is up; A pressed and released every so often.
        let game_up = std::fs::read_to_string(&log).is_ok_and(|l| l.contains("groups"));
        if game_up && !pad_sent {
            pad_sent = true;
            client.send_input(InputEvent::PadArrive { slot: 0, kind: PadKind::Xbox }).unwrap();
        }
        if pad_sent && start.elapsed().as_millis() % 400 < 120 {
            presses += 1;
            let b = if presses.is_multiple_of(2) { PadButtons::A } else { 0 };
            let _ = client.send_input(InputEvent::Pad { slot: 0, state: PadState { buttons: PadButtons(b), lx: (presses * 1000) as i16, ..Default::default() } });
        }
        if std::fs::read_to_string(&log).is_ok_and(|l| l.contains(" bytes") || l.contains("refused") || l.contains("no pad")) {
            break;
        }
    }
    let got = std::fs::read_to_string(&log).unwrap_or_default();
    eprintln!("{} frames; the program saw:\n{got}", frames);
    // Let the audit (8 s after the launch) run before the game goes.
    tokio::time::sleep(Duration::from_secs(9)).await;
    let _ = client.send_control(Control::Bye { reason: BYE_QUIT_GAME.into() });
    let t = Instant::now();
    loop {
        let s: Vec<StreamSession> = q.call(Request::Streams).await.unwrap();
        if s.is_empty() {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "the stream didn't end: {s:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    (got, notices)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_streamed_game_opens_its_pads_with_and_without_mods() {
    if !have("gamescope") || quiver_stream_host::cuda::Cuda::open().is_err() {
        eprintln!("skip: no gamescope or CUDA");
        return;
    }
    let gid = match quiver_stream_host::sandbox::check_rule() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skip: the stream pad rule: {e:#}");
            return;
        }
    };
    let host_bin = PathBuf::from(env!("CARGO_BIN_EXE_quiverd")).with_file_name("quiver-stream-host");
    if !host_bin.is_file() {
        eprintln!("skip: build quiver-stream-host first (cargo build -p quiver-stream-host)");
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("stream-pads");
    // (overlayfs leaves its work folder mode 0)
    let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&root).status();
    let _ = std::fs::remove_dir_all(&root);
    let d = |n: &str| root.join(n);
    let proton = d("proton");
    executable(&proton.join("proton"), FAKE_PROTON);
    executable(&proton.join("files/bin/wineserver"), FAKE_WINESERVER);
    executable(&d("game/install/padprobe.exe"), PROBE);
    std::fs::create_dir_all(d("game/prefix")).unwrap();
    let games = json!([{
        "product_id": 0, "name": "Pad Probe", "install_dir": d("game/install"), "prefix": d("game/prefix"),
        "exe": "padprobe.exe", "args": d("game/pad.log"), "game": GAME,
    }]);
    write(&d("games.json"), &games.to_string());
    let data = d("data/quiver");
    write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
    let cfg = json!({
        "library": d("lib"), "prefixes": d("pfx"), "proton": proton, "sdk_source": d("sdk"),
        "save_backups": d("backups"), "report_playtime": false, "steam_offers": false,
        "remote": { "stream_port": 48_700 }
    });
    write(&data.join("config.json"), &cfg.to_string());
    write(&data.join("remote/machine.json"), &json!({ "name": "tower" }).to_string());
    // A short runtime folder (socket paths); PipeWire stays the session's.
    let run = PathBuf::from(format!("/run/user/{}/quiver-stream-pads-{}", unsafe { libc::getuid() }, std::process::id()));
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("quiver/quiverd.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_quiverd"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("USER", std::env::var("USER").unwrap_or_default())
        .env("HOME", d("home"))
        .env("XDG_DATA_HOME", d("data"))
        .env("XDG_CONFIG_HOME", d("config"))
        .env("XDG_STATE_HOME", d("state"))
        .env("XDG_CACHE_HOME", d("cache"))
        .env("XDG_RUNTIME_DIR", &run)
        .env("PIPEWIRE_RUNTIME_DIR", std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default())
        .env("QUIVER_SHM_ROOT", d("shm"))
        .env("QUIVER_BROKER", "127.0.0.1:0")
        .env("QUIVER_AUTO_RESTART", "0")
        .env("QUIVER_REMOTE_MDNS", "0")
        .env("QUIVER_STREAM_SCOPE", "0")
        .env("QUIVER_STREAM_HOST_BIN", &host_bin)
        .env("QUIVER_TEST_GAMES", d("games.json"))
        .env("RUST_LOG", "quiverd=info,quiver_core=info,quiver_stream_host=debug")
        .stdout(std::fs::File::create(d("quiverd.log")).unwrap())
        .stderr(std::fs::OpenOptions::new().append(true).open(d("quiverd.log")).unwrap())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child, run.clone());
    let t = Instant::now();
    while std::os::unix::net::UnixStream::connect(&sock).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let q = Client::connect_running_at(sock.clone(), "cli").await.unwrap();

    // 1. A mod profile: the launch's own namespace (an overlay on the install folder), as
    // the person's Pharos profile has. This is the case that had no pads.
    let layer = d("mod-layer");
    write(&layer.join("from-the-mod.txt"), "1");
    let _: serde_json::Value = q
        .call(Request::Mods(quiver_api::ModsRequest::LayerAdd { game: quiver_api::GameRef::parse(GAME).unwrap().into(), name: "probe-mod".into(), path: layer, deletes: vec![] }))
        .await
        .expect("add a layer");
    let _: serde_json::Value = q
        .call(Request::Mods(quiver_api::ModsRequest::ProfileCreate {
            game: quiver_api::GameRef::parse(GAME).unwrap().into(),
            name: "Modded".into(),
            layers: Some(vec!["probe-mod".into()]),
            message: None,
            saves: Some("default".into()),
            from: None,
            own_settings: Some(false),
        }))
        .await
        .expect("make a profile");
    let (got, notices) = stream_and_press(&q, &root, Some("Modded")).await;
    assert!(got.contains("mod mounted"), "the mod's overlay is what the game sees: {got}");
    assert!(got.split_whitespace().any(|g| g == gid.to_string()), "the modded launch has the pad group: {got}");
    assert!(got.contains("opened /dev/input/event") && got.contains("read 192 bytes"), "the modded launch reads its pad: {got}");
    assert!(!notices.iter().any(|n| n.contains("pad")), "{notices:?}");

    // 2. No mods.
    let (got, notices) = stream_and_press(&q, &root, None).await;
    assert!(!got.contains("mod mounted"), "{got}");
    assert!(got.split_whitespace().any(|g| g == gid.to_string()), "the game has the pad group: {got}");
    assert!(got.contains("opened /dev/input/event") && got.contains("read 192 bytes"), "{got}");
    assert!(!notices.iter().any(|n| n.contains("pad")), "{notices:?}");

    // 3. Both launches' processes all had the group (the audit, 8 s in).
    let log = std::fs::read_to_string(d("quiverd.log")).unwrap_or_default();
    assert_eq!(log.matches("every process of the launch has the pad group").count(), 2, "{log}");
    assert!(!log.contains("stream pads:") || !log.contains("outside the stream's pad group"), "{log}");
    let _ = std::fs::remove_dir_all(&run);
}
