//! `quiver-stream-host serve` end to end with a client on the real transport (a
//! `ClientSession` here), the stream's program in its headless gamescope, and **hidden pads**:
//!
//! 1. the spec on stdin, `listening` and `connected` on stdout, Configure and frames after the
//!    Hello;
//! 2. a pad arriving makes a stream pad, after the host proved the stream pad rule hides a
//!    canary from this session;
//! 3. the stream's program, in the pad namespace, opens the pad and reads the button presses
//!    sent;
//! 4. this process (the person's uid and groups, as their desktop, Steam and games have) can't
//!    open it: no uaccess, no ACL, root:quiver-stream-pad 0660, the group not one of ours;
//! 5. a Bye with `quit-game` ends the stream and the program.
//!
//! Opt-in: `cargo test -p quiver-stream-host --test live_serve -- --ignored --nocapture`. Skips
//! without gamescope, the NVIDIA driver, the stream pad rule or the namespace.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use quiver_stream_net::{ClientOpts, ClientSession, SessionKeys, VideoEvent};
use quiver_stream_proto::serve::{ServeIn, ServeSpec, ServeTarget};
use quiver_stream_proto::{BYE_QUIT_GAME, ClientCaps, Control, DisplayCaps, InputEvent, PROTOCOL_VERSION, PadButtons, PadKind, PadState, StreamPrefs, StreamTarget};

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/stream-host-test/serve");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// The program: wait for a stream pad, open it, and log what it reads.
const PROBE: &str = r#"
"$TESTWIN" --test-window --fps 60 &
log="$PAD_LOG"; echo "groups $(id -G)" >> "$log"
for i in $(seq 200); do
  for n in /sys/class/input/event*; do
    case "$(cat $n/device/name 2>/dev/null)" in
      "Quiver Stream Pad "*canary*) ;;
      "Quiver Stream Pad "*)
        dev=/dev/input/${n##*/}
        if exec 3<"$dev"; then
          echo "opened $dev" >> "$log"
          n=$(timeout 5 dd bs=24 count=8 <&3 2>/dev/null | wc -c); echo "read $n bytes" >> "$log"
        else
          echo "refused $dev" >> "$log"
        fi
        sleep 60; exit 0;;
    esac
  done
  sleep 0.1
done
echo "no pad" >> "$log"
sleep 60
"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn serve_with_hidden_pads() {
    if Command::new("gamescope").arg("--help").output().is_err() || quiver_stream_host::cuda::Cuda::open().is_err() {
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
    let dir = scratch();
    let log = dir.join("pad.log");
    let keys = SessionKeys::random();
    let display = DisplayCaps { width: 1280, height: 720, refresh_mhz: 60_000, hdr: false, vrr: false, stereo_sbs: false, scale: 1.0 };
    let caps = ClientCaps { decoders: vec![], display: display.clone(), audio_channels: 2, pads: vec![PadKind::Xbox], microphone: false };
    let spec = ServeSpec {
        target: ServeTarget::Command { argv: vec!["sh".into(), "-c".into(), PROBE.into()], env: vec![("PAD_LOG".into(), log.display().to_string()), ("TESTWIN".into(), env!("CARGO_BIN_EXE_quiver-stream-host").into())] },
        keys: keys.to_hex(),
        caps: caps.clone(),
        prefs: StreamPrefs::default(),
        host_name: "test".into(),
        session: format!("t{}", std::process::id()),
        dir: dir.join("host"),
        listen: Some("127.0.0.1:0".parse().unwrap()),
        keep_secs: 0,
        connect_secs: 20,
        headless_server: false,
        transport: None,
    };
    // Its own runtime folder (gamescope's socket), short enough for a socket path.
    let rt = PathBuf::from(format!("/run/user/{}/quiver-stream-test-{}", unsafe { libc::getuid() }, std::process::id()));
    std::fs::create_dir_all(&rt).unwrap();
    let mut host = Command::new(env!("CARGO_BIN_EXE_quiver-stream-host"))
        .arg("serve")
        .env("XDG_RUNTIME_DIR", &rt)
        .env("PIPEWIRE_RUNTIME_DIR", std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default())
        .env("QUIVER_SHM_ROOT", dir.join("shm"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(dir.join("host.err")).unwrap())
        .spawn()
        .unwrap();
    let mut stdin = host.stdin.take().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&ServeIn::Start(Box::new(spec))).unwrap()).unwrap();
    // 1. Its port, from the first line out.
    let mut out = std::io::BufReader::new(host.stdout.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut out, &mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["t"], "listening", "{line}");
    let port = v["port"].as_u64().unwrap() as u16;
    let lines = std::thread::spawn(move || std::io::BufRead::lines(out).map_while(Result::ok).collect::<Vec<String>>());

    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let client = ClientSession::connect(addr, keys, ClientOpts::default()).await.expect("connect");
    client.send_control(Control::Hello { version: PROTOCOL_VERSION, caps, prefs: StreamPrefs::default(), target: StreamTarget::Desktop }).unwrap();
    let mut configured = false;
    let mut frames = 0;
    let mut notices = Vec::new();
    let start = Instant::now();
    let mut pad_sent = false;
    let mut presses = 0;
    while start.elapsed() < Duration::from_secs(25) {
        tokio::select! {
            c = client.recv_control() => match c {
                Some(Control::Configure(_)) => configured = true,
                Some(Control::Notice { text }) => { eprintln!("notice: {text}"); notices.push(text) }
                Some(_) => {}
                None => break,
            },
            v = client.recv_video() => if let Some(VideoEvent::Frame(_)) = v { frames += 1 },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        // 2. A pad, once frames flow; then A pressed and released every so often.
        if frames > 30 && !pad_sent {
            pad_sent = true;
            client.send_input(InputEvent::PadArrive { slot: 0, kind: PadKind::Xbox }).unwrap();
        }
        if pad_sent && start.elapsed().as_millis() % 500 < 120 {
            presses += 1;
            let b = if presses % 2 == 0 { PadButtons::A } else { 0 };
            client.send_input(InputEvent::Pad { slot: 0, state: PadState { buttons: PadButtons(b), lx: (presses * 1000) as i16, ..Default::default() } }).unwrap();
        }
        if std::fs::read_to_string(&log).is_ok_and(|l| l.contains(" bytes") || l.contains("refused") || l.contains("no pad")) {
            break;
        }
    }
    assert!(configured && frames > 30, "configured {configured}, {frames} frames");
    let got = std::fs::read_to_string(&log).unwrap_or_default();
    eprintln!("the program saw:\n{got}");
    // 3. The program, in the namespace with the pad group, opened it and read from it.
    assert!(got.contains(&format!(" {gid}")), "the program has the pad group: {got}");
    assert!(got.contains("opened /dev/input/event") && got.contains("read 192 bytes"), "{got}");
    assert!(!notices.iter().any(|n| n.contains("pad")), "pad notices: {notices:?}");
    // 4. This session can't open it.
    let dev = got.lines().find_map(|l| l.strip_prefix("opened ")).unwrap().to_owned();
    let st = quiver_stream_host::sandbox::node_state(Path::new(&dev)).unwrap();
    eprintln!("{dev} from this session: {st:?}");
    assert_eq!(quiver_stream_host::sandbox::problem(&st, gid), None);
    assert!(!st.openable && !st.uaccess && !st.acl && st.rule_ran);
    assert!(std::fs::File::open(&dev).is_err(), "this session opened {dev}");
    let mut mine = vec![0 as libc::gid_t; 256];
    let n = unsafe { libc::getgroups(256, mine.as_mut_ptr()) } as usize;
    assert!(!mine[..n].contains(&gid), "the pad group is one of this session's");
    // 5. Quit game ends the stream and the program.
    client.send_control(Control::Bye { reason: BYE_QUIT_GAME.into() }).unwrap();
    let t = Instant::now();
    while host.try_wait().unwrap().is_none() && t.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(host.try_wait().unwrap().is_some(), "the host didn't end");
    let lines = lines.join().unwrap();
    eprintln!("{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("\"quit_game\"")) && lines.iter().any(|l| l.contains("\"ended\"")));
    assert!(!Path::new(&dev).exists(), "the pad outlived the stream");
    let _ = std::fs::remove_dir_all(&rt);
}
