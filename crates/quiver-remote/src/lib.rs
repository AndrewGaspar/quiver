//! Quiver's remote-play control plane (design/REMOTE.md §3, §4): who a machine is, which
//! machines it is paired with, and how they talk.
//!
//! - [`identity`]: this machine's Ed25519 key, certificate, name and id.
//! - [`peers`]: paired machines (`peers.json`), with the key each must present.
//! - [`tls`]: mutual TLS 1.3 with the peer's key pinned.
//! - [`pair`]: pairing with a 6-digit code (SPAKE2, then both certificate hashes
//!   confirmed under the PAKE key).
//! - [`wire`]: the first lines on a connection (what the client wants, the answer).
//! - [`discovery`]: mDNS `_quiver._tcp`.
//! - [`wol`]: Wake-on-LAN packets.
//! - [`ticket`]: tickets for starting a stream.
//!
//! quiverd serves the listener (off by default); `quiver-api`'s `Client` connects with
//! [`connect`], and the quiverd protocol then runs over the TLS connection unchanged.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::BufReader;

pub mod discovery;
pub mod identity;
pub mod pair;
pub mod peers;
pub mod ticket;
pub mod tls;
pub mod wire;
pub mod wol;

pub use identity::{Identity, Pin};
pub use peers::{Machine, Peer, Peers, Role};
pub use ticket::StreamTicket;

/// The control plane's TCP port.
pub const DEFAULT_PORT: u16 = 47990;
/// What the listener binds by default: every address, since a Tailscale or WireGuard
/// address is the expected way in from outside.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:47990";

/// `<data>/remote`: `$XDG_DATA_HOME/quiver/remote` (quiverd's data dir, `remote`).
pub fn remote_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("quiver").join("remote")
}

/// An open, authenticated control connection to a paired machine.
pub type ApiStream = BufReader<tls::ClientStream>;

/// Connect to a paired server for `intent` (`Api` or `Ping`): TLS with its key pinned,
/// then the hello. Fails with a [`wire::Refused`] when it no longer knows this machine.
pub async fn connect(me: &Identity, machine: &Machine, intent: wire::Intent) -> Result<(ApiStream, wire::Welcome)> {
    let (tls, _) = tls::connect(me, &machine.addr, Some(machine.pin)).await.with_context(|| format!("{} ({})", machine.name, machine.addr))?;
    let mut stream = BufReader::new(tls);
    let hello = wire::Hello { quiver_remote: wire::VERSION, intent, name: me.name().to_owned(), pake: None };
    wire::send(&mut stream, &hello).await?;
    let answer: wire::Answer = tokio::time::timeout(tls::CONNECT_TIMEOUT, wire::recv(&mut stream))
        .await
        .with_context(|| format!("{} didn't answer", machine.name))??;
    let welcome = answer.ok.context("no welcome")?;
    Ok((stream, welcome))
}

/// [`connect`] for the API, to the server peer `query` (a name or id) as listed in `dir`.
/// Reads this machine's identity and peers from `dir` (never makes them).
pub async fn connect_named(dir: &Path, query: &str) -> Result<(ApiStream, wire::Welcome, Machine)> {
    let me = Identity::load(dir)?;
    let peers = Peers::load(dir)?;
    let peer = peers.one(query, Some(Role::Server))?;
    let machine = peer.machine().with_context(|| format!("{} has no known address", peer.name))?;
    let (s, w) = connect(&me, &machine, wire::Intent::Api).await?;
    Ok((s, w, machine))
}
