//! Remote play's control plane in the protocol (design/REMOTE.md §3–§5): the listener,
//! pairing, paired machines, and starting a stream.
//!
//! A paired client reaches quiverd over TLS (`quiver-remote`), and the protocol is the
//! same as on the socket. Its requests carry `source: remote:<peer name>` when its client
//! is the person, and `remote:<peer name>/<what it said>` otherwise ([`remote_source_as`]);
//! a few are refused over the tunnel with [`LOCAL_ONLY`] ([`local_only`]).
//! [`crate::Client::machine`] connects to a paired machine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use quiver_remote::discovery::Discovered;
pub use quiver_remote::{Machine, Role, StreamTicket};
pub use quiver_stream_proto::serve::LaunchEnv;
pub use quiver_stream_proto::{ClientCaps, StreamPrefs, StreamTarget};

use crate::Request;

/// `Reply::Err::code` of a request refused because it came over the remote tunnel and only
/// makes sense on the machine itself ([`local_only`]).
pub const LOCAL_ONLY: &str = "local_only";
/// `Reply::Err::code` of a request quiverd knows but can't do yet (`StartStream`, until the
/// stream host is integrated).
pub const NOT_IMPLEMENTED: &str = "not_implemented";

/// The `source` of every request that came over the tunnel from `peer`.
pub fn remote_source(peer: &str) -> String {
    format!("remote:{peer}")
}

/// The `source` quiverd records for a request from `peer` whose client said it was
/// `declared`: `remote:<peer>` for the person (the app, the terminal, Sight), and
/// `remote:<peer>/<declared>` for anything else (`remote:laptop/agent`), so an agent's
/// requests stay an agent's on the other machine too. Like a local `source`, the label is
/// the client's word: it keeps honest automation out of the person's gates.
pub fn remote_source_as(peer: &str, declared: &str) -> String {
    if crate::mods::is_human_source(declared) || declared.is_empty() {
        remote_source(peer)
    } else {
        format!("remote:{peer}/{declared}")
    }
}

/// The person at a front end: here (`cli`, `gui`, `sight`), or on a paired machine
/// (`remote:<peer>`, see [`remote_source_as`]). The gates that protect store accounts
/// (sign-in, sign-out, handing a session to another machine) use this; agents, here or
/// remote, are refused.
pub fn is_person(source: &str) -> bool {
    crate::mods::is_human_source(source) || source.strip_prefix("remote:").is_some_and(|peer| !peer.is_empty() && !peer.contains('/'))
}

/// A request that came over the remote tunnel (from the person or not).
pub fn is_remote(source: &str) -> bool {
    source.starts_with("remote:")
}

/// `remote` in config.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSettings {
    /// The listener is on. Off by default: nothing listens until the person turns it on
    /// (`quiver remote enable`).
    #[serde(default)]
    pub enabled: bool,
    /// What it binds (`host:port`); `None`: [`quiver_remote::DEFAULT_LISTEN`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// The first UDP port streams' media use (one per stream, counting up); `None`:
    /// [`STREAM_PORT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_port: Option<u16>,
    /// The Moonlight (GameStream) server, which has its own switch.
    #[serde(default, skip_serializing_if = "crate::moonlight::MoonlightSettings::is_default")]
    pub moonlight: crate::moonlight::MoonlightSettings,
}

/// The first media port (design/REMOTE.md §3).
pub const STREAM_PORT: u16 = 47998;
/// How many media ports up from the first a stream may take (one per stream at a time), so
/// firewalls allow `STREAM_PORT ..= STREAM_PORT + STREAM_PORTS - 1` (design/INSTALL.md).
pub const STREAM_PORTS: u16 = 64;

impl RemoteSettings {
    pub fn is_default(&self) -> bool {
        *self == RemoteSettings::default()
    }

    pub fn listen(&self) -> String {
        self.listen.clone().unwrap_or_else(|| quiver_remote::DEFAULT_LISTEN.to_owned())
    }
}

/// `Request::RemoteStatus`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteStatus {
    /// This machine's name and id.
    pub name: String,
    pub id: String,
    pub enabled: bool,
    /// What the listener binds (configured, or the default).
    pub listen: String,
    /// Where it is listening now (`None` while off, or when binding failed: `error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listening: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Advertised on the LAN (mDNS).
    #[serde(default)]
    pub discoverable: bool,
    /// A pairing window is open (its code went only to whoever opened it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PairingState>,
    /// Paired machines.
    #[serde(default)]
    pub peers: Vec<PeerInfo>,
    /// What a remote client can't ask for ([`local_only`]), in words.
    #[serde(default)]
    pub local_only: Vec<String>,
    /// Whether quiverd starts at boot here (`Request::RemoteBoot`); `None` from daemons
    /// before it, or without systemd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<crate::service::BootStatus>,
}

/// An open pairing window, without its code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingState {
    pub expires_at: DateTime<Utc>,
    pub tries_left: u32,
}

/// `Request::RemotePairBegin`: the code to type on the other machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairCode {
    pub code: String,
    /// This machine's name, which the other side will see.
    pub name: String,
    pub expires_at: DateTime<Utc>,
    /// Where to reach this machine: the listener's address, and the LAN addresses it has.
    #[serde(default)]
    pub addrs: Vec<String>,
}

/// Whether a paired machine can be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    /// Reached: a server answered the last check, or a client is connected now.
    Online,
    /// Not reached, but its MAC is known: Wake-on-LAN can try to wake it.
    Asleep,
    /// Not reached.
    #[default]
    Offline,
}

/// A paired machine (`Request::RemotePeers`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub paired_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub state: PeerState,
    /// A client's open connections to this machine.
    #[serde(default)]
    pub connections: usize,
}

/// Where a stream stands (`Request::Streams`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    /// The host is up and waiting for the client (or the game is starting).
    Starting,
    /// A client is connected.
    Streaming,
    /// The client left; the game keeps running until `kept_until`, and a `StartStream` for
    /// the same target resumes it.
    Kept,
}

/// One stream this machine serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamSession {
    pub id: String,
    pub target: StreamTarget,
    /// What it shows, for people ("Alan Wake Remastered", "Test pattern").
    pub title: String,
    /// The game ref, for a game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    /// The machine it streams (or last streamed) to: the paired peer's name, or `This PC`.
    pub peer: String,
    pub state: StreamState,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_until: Option<DateTime<Utc>>,
    /// The media's UDP port.
    pub port: u16,
    /// The host process.
    pub pid: u32,
}

impl StreamSession {
    /// "streaming to Laptop", "kept for Laptop until 14:05", "starting for Laptop".
    pub fn describe(&self) -> String {
        match self.state {
            StreamState::Starting => format!("starting for {}", self.peer),
            StreamState::Streaming => format!("streaming to {}", self.peer),
            StreamState::Kept => match self.kept_until {
                Some(t) => format!("kept for {} until {}", self.peer, t.with_timezone(&chrono::Local).format("%H:%M")),
                None => format!("kept for {}", self.peer),
            },
        }
    }
}

/// `Request::StartStream`: where to send the media, and who is serving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartStreamReply {
    pub ticket: StreamTicket,
    pub host_name: String,
}

/// Why a request can't come over the remote tunnel, if it can't: it names a path on the
/// machine to read (a file picker on the other end can't have picked it), hands out a
/// store's launch secret for a local process, or ties itself to a local process. quiverd
/// refuses these from remote clients with [`LOCAL_ONLY`].
pub fn local_only(req: &Request) -> Option<&'static str> {
    use crate::mods::ModsRequest as M;
    use crate::options::OptionsRequest as O;
    use crate::perf::PerfRequest as P;
    use crate::sight::SightRequest as S;
    Some(match req {
        Request::Import { .. } => "importing reads a folder on the machine",
        Request::Setup { library, prefixes, .. } if library.is_some() || prefixes.is_some() => "storage folders are chosen on the machine",
        Request::LaunchContext { .. } | Request::LaunchMaterial { .. } => "launch secrets are for processes on the machine",
        Request::SteamSession { .. } | Request::SteamLaunchPlan { .. } => "Steam launches run on the machine",
        Request::Shutdown => "quiverd is stopped on the machine",
        Request::RemotePairWith { .. } => "pairing to another machine is done from the machine itself",
        Request::Mods(M::LayerAdd { .. }) => "a layer's files are read from a folder on the machine",
        Request::Mods(M::Install { path: Some(_), .. }) => "a mod package is read from a folder on the machine",
        Request::Mods(M::VersionAdd { path, options, .. }) if !path.as_os_str().is_empty() || options.is_some() => {
            "a mod version is read from a folder on the machine"
        }
        Request::Options(O::Lint { .. }) => "linting reads a folder on the machine",
        Request::Perf(P::HudAttach { .. }) => "the HUD renderer runs on the machine",
        Request::Sight(S::Report { .. } | S::Icons { .. }) => "the Sight panel runs on the machine",
        _ => return None,
    })
}

/// [`local_only`]'s list in words, for `RemoteStatus::local_only`.
pub fn local_only_list() -> Vec<String> {
    [
        "import (reads a folder on the machine)",
        "setup with storage folders",
        "launch context and launch material (store launch secrets)",
        "steam-run sessions and launch plans",
        "stopping quiverd",
        "pairing with another machine (remote add)",
        "mods from a folder: layer add, install --path, version add --path",
        "options lint",
        "HUD attach",
        "Sight panel reports and icons",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_forms() {
        let r: Request = serde_json::from_str(r#"{"method":"remote_enable","params":{}}"#).unwrap();
        assert!(matches!(r, Request::RemoteEnable { listen: None }));
        let r: Request = serde_json::from_str(r#"{"method":"remote_status"}"#).unwrap();
        assert!(matches!(r, Request::RemoteStatus));
        let v = serde_json::to_value(Request::RemotePairWith { addr: "tower".into(), code: "123456".into() }).unwrap();
        assert_eq!(v, serde_json::json!({"method": "remote_pair_with", "params": {"addr": "tower", "code": "123456"}}));
        let r: Request = serde_json::from_str(r#"{"method":"remote_rename","params":{"name":"Tower"}}"#).unwrap();
        assert!(matches!(r, Request::RemoteRename { id: None, .. }));
        let e: crate::Event = serde_json::from_str(r#"{"type":"peer_online","id":"ab","name":"Tower"}"#).unwrap();
        assert!(matches!(e, crate::Event::PeerOnline { .. }));
        let p: PeerInfo = serde_json::from_str(r#"{"id":"ab","name":"T","role":"server","paired_at":"2026-11-21T00:00:00Z"}"#).unwrap();
        assert_eq!((p.state, p.connections), (PeerState::Offline, 0));
        assert!(RemoteSettings::default().is_default());
        let r: Request = serde_json::from_str(r#"{"method":"stop_stream","params":{"id":"s1"}}"#).unwrap();
        assert!(matches!(r, Request::StopStream { .. }));
        let e: crate::Event = serde_json::from_str(r#"{"type":"streams_changed"}"#).unwrap();
        assert!(matches!(e, crate::Event::StreamsChanged));
        assert_eq!(RemoteSettings::default().listen(), "0.0.0.0:47990");
    }

    #[test]
    fn remote_sources_keep_agents_apart() {
        assert_eq!(remote_source_as("laptop", "cli"), "remote:laptop");
        assert_eq!(remote_source_as("laptop", "gui"), "remote:laptop");
        assert_eq!(remote_source_as("laptop", "agent"), "remote:laptop/agent");
        assert!(is_person("cli") && is_person("gui") && is_person("remote:laptop"));
        assert!(!is_person("agent") && !is_person("remote:laptop/agent") && !is_person("remote:") && !is_person("mcp"));
        assert!(is_remote("remote:laptop/agent") && !is_remote("cli"));
    }

    #[test]
    fn local_only_requests() {
        use std::path::PathBuf;
        let import = Request::Import { game: 1.into(), path: PathBuf::from("/x"), language: None };
        assert!(local_only(&import).is_some());
        assert!(local_only(&Request::Setup { library: None, prefixes: None, finish: None }).is_none());
        assert!(local_only(&Request::Setup { library: Some("/g".into()), prefixes: None, finish: None }).is_some());
        assert!(local_only(&Request::Library { refresh: false, all_stores: true }).is_none());
        assert!(local_only(&Request::Shutdown).is_some());
        let builtin = Request::Mods(crate::mods::ModsRequest::Install { game: 1.into(), builtin: Some("dlss".into()), path: None, name: None });
        assert!(local_only(&builtin).is_none());
        let folder = Request::Mods(crate::mods::ModsRequest::Install { game: 1.into(), builtin: None, path: Some("/m".into()), name: None });
        assert!(local_only(&folder).is_some());
        assert!(local_only(&Request::RemotePairBegin).is_none(), "a paired client can pair others");
        assert_eq!(local_only_list().len(), 10);
    }
}
