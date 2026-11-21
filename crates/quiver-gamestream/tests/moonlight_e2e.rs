//! The real Moonlight client against the test server: pair, list, stream twice, quit.
//!
//! Gated: it runs only with `QUIVER_E2E_MOONLIGHT=1`, and skips when gamescope, moonlight or
//! the other tools it needs aren't installed. It uses the GPU briefly (decode, and NVENC for
//! the test media or, with `--features stream-host`, the host's test pattern).
//!
//! ```text
//! QUIVER_E2E_MOONLIGHT=1 cargo test -p quiver-gamestream --test moonlight_e2e -- --nocapture
//! ```
//!
//! Everything runs in a private user, mount and network namespace (`moonlight-e2e.sh`) whose
//! loopback has Tailscale's MTU, 1280: the first stream is at the packet size Moonlight uses
//! for a host it counts as local (1392, too big for that MTU, so the server must cut it down),
//! the second at Moonlight's own choice for 127.0.0.1 (1024, which fits). No datagram may need
//! fragmenting. One of the streams goes over IPv6 (`::1`), the other over IPv4.

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().is_ok_and(|s| s.success())
}

/// Five seconds of test pattern (HEVC with access unit delimiters) and a tone (Opus, 5 ms
/// CBR packets).
fn media(dir: &Path) -> Vec<String> {
    let video = dir.join("test.hevc");
    let audio = dir.join("test.opus");
    let v = video.to_str().unwrap();
    let src = ["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=60", "-t", "5", "-pix_fmt", "yuv420p"];
    let nvenc = [&src[..], &["-c:v", "hevc_nvenc", "-tune", "ull", "-bf", "0", "-g", "60", "-forced-idr", "1", "-aud", "1", "-b:v", "6M", v]].concat();
    let x265 = [&src[..], &["-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "aud=1:bframes=0:keyint=60", "-b:v", "6M", v]].concat();
    assert!(ffmpeg(&nvenc) || ffmpeg(&x265), "couldn't encode the test video");
    assert!(ffmpeg(&[
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "5", "-ac", "2", "-c:a", "libopus", "-b:a", "128k", "-vbr", "off",
        "-frame_duration", "5", audio.to_str().unwrap(),
    ]));
    ["--video", v, "--codec", "hevc", "--audio", audio.to_str().unwrap()].map(String::from).to_vec()
}

/// A screenshot shows a picture, not a blank or single-colour window.
fn has_picture(png: &Path) -> bool {
    let Ok(img) = image::open(png) else { return false };
    let rgb = img.to_rgb8();
    let mut colours = std::collections::HashSet::new();
    for p in rgb.pixels().step_by(97) {
        colours.insert([p[0] / 32, p[1] / 32, p[2] / 32]);
    }
    colours.len() >= 6
}

/// `key=value` from the server's tracing line containing `marker`.
fn fields(log: &str, marker: &str) -> Vec<std::collections::HashMap<String, String>> {
    log.lines()
        .filter(|l| l.contains(marker))
        .map(|l| l.split_whitespace().filter_map(|w| w.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
        .collect()
}

#[test]
fn moonlight_pairs_lists_streams_and_quits() {
    if std::env::var("QUIVER_E2E_MOONLIGHT").as_deref() != Ok("1") {
        eprintln!("skipped: set QUIVER_E2E_MOONLIGHT=1 to run Moonlight end to end");
        return;
    }
    let missing: Vec<&str> = ["gamescope", "moonlight", "unshare", "ffmpeg", "xprop", "prlimit", "ip"].into_iter().filter(|t| !have(t)).collect();
    if !missing.is_empty() {
        eprintln!("skipped: not installed: {missing:?}");
        return;
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("moonlight-e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let server_args = if cfg!(feature = "stream-host") { vec!["--host-pattern".to_owned()] } else { media(&dir) };

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/moonlight-e2e.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env("GS_SERVER", env!("CARGO_BIN_EXE_quiver-gamestream-test"))
        .env("GS_STATE", &dir)
        .env("GS_SERVER_ARGS", server_args.join(" "))
        .env("GS_MTU", "1280")
        .output()
        .unwrap();
    let steps = String::from_utf8_lossy(&out.stdout);
    let server = std::fs::read_to_string(dir.join("server.log")).unwrap_or_default();
    eprintln!("{steps}\n--- server log: {}", dir.join("server.log").display());
    for step in ["STEP namespace lo mtu 1280", "STEP server ready", "STEP paired", "STEP listed", "STEP reached over IPv6", "STEP streamed stream-1392", "STEP streamed stream-auto", "STEP quit", "STEP done"] {
        assert!(steps.contains(step), "missing {step:?}\n{steps}\n{}", String::from_utf8_lossy(&out.stderr));
    }

    // Both streams connected their control stream and got frames on screen.
    assert_eq!(server.matches("control stream connected").count(), 2, "{server}");
    assert!(server.contains("control stream connected addr=[::1]"), "no stream over IPv6");
    assert!(server.contains("control stream connected addr=127.0.0.1"), "no stream over IPv4");
    for stream in ["stream-1392", "stream-auto"] {
        let log = std::fs::read_to_string(dir.join(format!("{stream}.log"))).unwrap();
        assert!(log.contains("Received first video packet"), "{stream}: no video");
        assert!(log.contains("Received first audio packet") || cfg!(feature = "stream-host"), "{stream}: no audio");
        let shots: Vec<PathBuf> = std::fs::read_dir(dir.join("client/shots"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(&format!("{stream}-")))
            .collect();
        assert!(shots.iter().filter(|p| has_picture(p)).count() >= 2, "{stream}: too few screenshots with a picture: {shots:?}");
    }

    // The first stream's packets were cut to fit the 1280 MTU; the second's already fit. No
    // send failed (the sockets don't fragment, so a datagram over the MTU would).
    assert_eq!(server.matches("don't fit the path MTU").count(), 1, "{server}");
    let totals = fields(&server, "video totals");
    assert_eq!(totals.len(), 2, "{server}");
    for t in &totals {
        assert_eq!(t["send_errors"], "0", "{t:?}");
        let largest: usize = t["largest"].parse().unwrap();
        assert!(largest > 0 && largest + 48 <= 1280, "largest datagram {largest}");
    }
    assert!(server.contains("Moonlight quit the app"));
}
