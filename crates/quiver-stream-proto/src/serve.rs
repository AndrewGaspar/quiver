//! quiverd ↔ `quiver-stream-host serve`: how quiverd starts a stream's host process, gives
//! it each session's keys, and hears what happens (design/REMOTE.md §2.1).
//!
//! The host reads newline-delimited JSON [`ServeIn`] on its stdin (a pipe or socket quiverd
//! holds) and writes [`ServeOut`] lines on its stdout. The first line in is always
//! [`ServeIn::Start`]. Keys never go in argv or the environment, where other processes and
//! logs could read them.
//!
//! The same lines also go over `<dir>/control.sock`, a Unix socket the host listens on (in a
//! folder only the person can open), so a quiverd that restarted onto a new build can attach
//! to a stream that kept running: it connects, sends [`ServeIn::Status`], and from then on
//! hears the events too. EOF on stdin isn't a stop: the stream goes on until it ends, is
//! stopped, or its kept session expires.
//!
//! One host process serves one stream target. The client may leave and a new client (with
//! new keys: [`ServeIn::Session`]) resume within [`ServeSpec::keep_secs`], without the
//! program restarting.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ClientCaps, StreamPrefs};

/// What the stream shows, as the host runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServeTarget {
    /// The test pattern with its stamp.
    TestPattern {
        #[serde(default = "yes")]
        motion: bool,
    },
    /// A program the host runs in its headless gamescope.
    Command { argv: Vec<String>, env: Vec<(String, String)> },
    /// A headless gamescope that someone else (quiverd's game launch) starts the program
    /// into, from [`ServeOut::Ready`]'s [`LaunchEnv`].
    Display,
}

fn yes() -> bool {
    true
}

/// Transport settings that override the defaults (measurements and tuning).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TransportTuning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pacing_fraction: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fec_loss_margin: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_kbps: Option<u32>,
}

/// Everything a stream's host starts with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServeSpec {
    pub target: ServeTarget,
    /// The first session's keys: `SessionKeys::to_hex` (80 hex digits).
    pub keys: String,
    /// The client's caps and prefs from `StartStream`; the client's Hello may refine them.
    pub caps: ClientCaps,
    #[serde(default)]
    pub prefs: StreamPrefs,
    /// This machine's name, for the client's Welcome.
    pub host_name: String,
    /// The stream's id: its sink, pads and folder are named after it.
    pub session: String,
    /// The stream's folder (private): gamescope's log, `control.sock`.
    pub dir: PathBuf,
    /// Where to bind the media socket when none is passed in (`--socket-fd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<SocketAddr>,
    /// How long a session the client left is kept for a resume; 0 ends the stream when the
    /// client leaves.
    #[serde(default = "keep_default")]
    pub keep_secs: u64,
    /// How long a new session waits for its client before it counts as left.
    #[serde(default = "connect_default")]
    pub connect_secs: u64,
    /// This machine is an opted-in headless server (`seat` decides pads with it).
    #[serde(default)]
    pub headless_server: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportTuning>,
}

/// The default [`ServeSpec::keep_secs`]: ten minutes (design/REMOTE.md §5).
pub const KEEP_SECS: u64 = 600;

fn keep_default() -> u64 {
    KEEP_SECS
}

fn connect_default() -> u64 {
    60
}

/// How to start a program into a [`ServeTarget::Display`] stream: the environment to add
/// (the X display, the stream's sink), what to remove, and the command every process of it
/// must run under (`wrapper… program…`) so it can open the stream's hidden pads (the pad
/// namespace; empty when pads aren't hidden).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchEnv {
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub wrapper: Vec<String>,
}

/// quiverd → host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServeIn {
    /// First, and only once.
    Start(Box<ServeSpec>),
    /// A new client session with fresh keys: the current client (if any) is told and
    /// dropped, and the program keeps running.
    Session { keys: String },
    /// End the stream (and the program).
    Stop { reason: String },
    /// Answer with [`ServeOut::Status`].
    Status,
}

/// Where a stream stands, for [`ServeOut::Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeState {
    /// The target is up; no client has connected to this session yet.
    Waiting,
    Streaming,
    /// The client left; the session is kept for a resume until `kept_until_ms`.
    Kept,
}

/// host → quiverd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServeOut {
    /// The media socket's port.
    Listening { port: u16 },
    /// A [`ServeTarget::Display`] is up: start the program into it like this.
    Ready { launch: LaunchEnv },
    /// The client of the current session reached the host from `peer`.
    Connected { peer: String },
    /// The client left (`reason`); the session is kept for `keep_secs`.
    Disconnected { reason: String, keep_secs: u64 },
    /// The client asked to end the game too (the stream menu's Quit game). The host ends
    /// the stream right after; whoever started the program should stop it.
    QuitGame,
    /// Something for the person (pads unavailable, a size the program can't follow).
    Notice { text: String },
    Status { state: ServeState, peer: Option<String>, pid: u32, port: u16, kept_until_ms: Option<u64> },
    /// The stream is over; the process exits next.
    Ended { reason: String },
}

/// A path to `dir/name` short enough for a Unix socket address (108 bytes): the path
/// itself, or, when it is too long, `/proc/self/fd/<fd>/name` through `dir` opened here
/// (keep the returned file open while binding or connecting).
pub fn socket_path(dir: &std::path::Path, name: &str) -> std::io::Result<(PathBuf, Option<std::fs::File>)> {
    let full = dir.join(name);
    if full.as_os_str().len() < 100 {
        return Ok((full, None));
    }
    let d = std::fs::File::open(dir)?;
    use std::os::fd::AsRawFd;
    Ok((PathBuf::from(format!("/proc/self/fd/{}/{name}", d.as_raw_fd())), Some(d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_socket_paths_go_through_the_folder() {
        let (p, f) = socket_path(std::path::Path::new("/run/user/1000/quiver/streams/s1"), "control.sock").unwrap();
        assert!(f.is_none() && p.ends_with("s1/control.sock"));
        let long = std::env::temp_dir().join("x".repeat(110));
        std::fs::create_dir_all(&long).unwrap();
        let (p, f) = socket_path(&long, "control.sock").unwrap();
        assert!(f.is_some() && p.starts_with("/proc/self/fd/") && p.as_os_str().len() < 100);
        std::fs::remove_dir(&long).unwrap();
    }

    #[test]
    fn lines_round_trip() {
        let s = ServeIn::Session { keys: "ab".into() };
        let line = serde_json::to_string(&s).unwrap();
        assert_eq!(line, r#"{"t":"session","keys":"ab"}"#);
        assert_eq!(serde_json::from_str::<ServeIn>(&line).unwrap(), s);
        let t: ServeTarget = serde_json::from_str(r#"{"kind":"test_pattern"}"#).unwrap();
        assert_eq!(t, ServeTarget::TestPattern { motion: true });
        let o = ServeOut::Disconnected { reason: "bye".into(), keep_secs: 600 };
        assert_eq!(serde_json::from_str::<ServeOut>(&serde_json::to_string(&o).unwrap()).unwrap(), o);
        let spec: ServeSpec = serde_json::from_value(serde_json::json!({
            "target": {"kind": "display"}, "keys": "00", "host_name": "Tower", "session": "s1", "dir": "/run/x",
            "caps": {"decoders": [], "display": {"width": 1280, "height": 720, "refresh_mhz": 60000, "hdr": false, "vrr": false, "stereo_sbs": false, "scale": 1.0}, "audio_channels": 2, "pads": [], "microphone": false}
        }))
        .unwrap();
        assert_eq!((spec.keep_secs, spec.connect_secs, spec.listen), (KEEP_SECS, 60, None));
    }
}
