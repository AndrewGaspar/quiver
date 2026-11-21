//! `quiver remote …`: remote play's control plane (design/REMOTE.md §3). Turning the
//! listener on and off, pairing (the server prints a code; the client adds it), paired
//! machines, Wake-on-LAN and LAN discovery. With `--machine`, these run on that machine.

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::remote::{Discovered, PairCode, PeerInfo, PeerState, RemoteStatus, Role};
use quiver_api::{Client, Event, Request};

use crate::print_json;

#[derive(Subcommand)]
pub enum RemoteAction {
    /// This machine's name and id, the listener, the pairing window and paired machines.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Turn the listener on (it stays on across restarts). Nothing listens until you do.
    Enable {
        /// What to bind (default 0.0.0.0:47990, every address: reach it over the LAN, or a
        /// Tailscale/WireGuard address; don't port-forward it).
        #[arg(long)]
        listen: Option<String>,
    },
    /// Turn the listener off; remote connections drop.
    Disable,
    /// Print a 6-digit code to pair another machine with this one, and wait (2 minutes,
    /// three wrong tries). Type the code on the other machine: Settings → Machines → Add,
    /// or `quiver remote add <this machine's address> <code>`.
    Pair {
        #[arg(long)]
        json: bool,
    },
    /// Pair with the machine at ADDR (host or host:port) using the code it shows.
    Add { addr: String, code: String },
    /// Paired machines, and whether each can be reached.
    Peers {
        #[arg(long)]
        json: bool,
    },
    /// Forget a paired machine (name, id or id prefix); its connections drop at once.
    Revoke { machine: String },
    /// `rename NAME`: rename this machine. `rename MACHINE NAME`: rename a paired one here.
    Rename {
        first: String,
        second: Option<String>,
    },
    /// Wake a paired machine with Wake-on-LAN (its MAC is learned when pairing on a LAN).
    Wake { machine: String },
    /// Quiver machines on the LAN (mDNS).
    Discover {
        /// Seconds to listen.
        #[arg(long, default_value_t = 2)]
        wait: u64,
        #[arg(long)]
        json: bool,
    },
}

fn state_word(p: &PeerInfo) -> String {
    match (p.role, p.state) {
        (Role::Client, PeerState::Online) => format!("connected ({})", p.connections),
        (_, PeerState::Online) => "online".into(),
        (_, PeerState::Asleep) => "asleep (quiver remote wake)".into(),
        (_, PeerState::Offline) => "offline".into(),
    }
}

fn print_peers(peers: &[PeerInfo]) {
    if peers.is_empty() {
        println!("no paired machines");
        return;
    }
    for p in peers {
        let role = match p.role {
            Role::Server => "server",
            Role::Client => "client",
        };
        let addr = p.addr.as_deref().map(|a| format!("  {a}")).unwrap_or_default();
        let seen = p.last_seen.map(|t| format!("  last seen {}", t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"))).unwrap_or_default();
        println!("{:<20} {}  {role:<6} {}{addr}{seen}", p.name, p.id, state_word(p));
    }
}

fn spaced(code: &str) -> String {
    if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code.to_owned() }
}

pub async fn run(client: &Client, action: RemoteAction) -> Result<()> {
    match action {
        RemoteAction::Status { json } => {
            let s: RemoteStatus = client.call(Request::RemoteStatus).await?;
            if json {
                return print_json(&s);
            }
            println!("{} ({})", s.name, s.id);
            match (&s.listening, s.enabled, &s.error) {
                (Some(a), _, _) => println!("remote: on, listening on {a}{}", if s.discoverable { ", on the LAN (mDNS)" } else { "" }),
                (None, true, Some(e)) => println!("remote: on, but not listening: {e}"),
                (None, true, None) => println!("remote: on (starting)"),
                (None, false, _) => println!("remote: off (quiver remote enable)"),
            }
            if let Some(p) = &s.pairing {
                println!("pairing: open until {} ({} tries left)", p.expires_at.with_timezone(&chrono::Local).format("%H:%M:%S"), p.tries_left);
            }
            println!();
            print_peers(&s.peers);
        }
        RemoteAction::Enable { listen } => {
            let s: RemoteStatus = client.call(Request::RemoteEnable { listen }).await?;
            println!("remote: on, listening on {}", s.listening.as_deref().unwrap_or(&s.listen));
            println!("pair another machine with `quiver remote pair` here");
        }
        RemoteAction::Disable => {
            let () = client.call(Request::RemoteDisable).await?;
            println!("remote: off");
        }
        RemoteAction::Pair { json } => pair(client, json).await?,
        RemoteAction::Add { addr, code } => {
            let p: PeerInfo = client.call(Request::RemotePairWith { addr, code }).await?;
            println!("paired with {} ({})", p.name, p.id);
            if p.mac.is_some() {
                println!("it can be woken with `quiver remote wake {}`", p.name);
            }
            println!("run commands there with `quiver --machine {} …`", p.name);
        }
        RemoteAction::Peers { json } => {
            let peers: Vec<PeerInfo> = client.call(Request::RemotePeers).await?;
            if json {
                return print_json(&peers);
            }
            print_peers(&peers);
        }
        RemoteAction::Revoke { machine } => {
            let gone: Vec<PeerInfo> = client.call(Request::RemoteRevoke { id: machine }).await?;
            if let Some(p) = gone.first() {
                println!("revoked {} ({})", p.name, p.id);
            }
        }
        RemoteAction::Rename { first, second } => match second {
            None => {
                let s: RemoteStatus = client.call(Request::RemoteRename { id: None, name: first }).await?;
                println!("this machine is now {}", s.name);
            }
            Some(name) => {
                let _: Vec<PeerInfo> = client.call(Request::RemoteRename { id: Some(first.clone()), name: name.clone() }).await?;
                println!("{first} is now {name} here");
            }
        },
        RemoteAction::Wake { machine } => {
            let () = client.call(Request::RemoteWake { id: machine.clone() }).await?;
            println!("sent a wake-up to {machine}");
        }
        RemoteAction::Discover { wait, json } => {
            let found: Vec<Discovered> = client.call(Request::RemoteDiscover { wait_ms: Some(wait.saturating_mul(1000)) }).await?;
            if json {
                return print_json(&found);
            }
            let peers: Vec<PeerInfo> = client.call(Request::RemotePeers).await.unwrap_or_default();
            if found.is_empty() {
                println!("no Quiver machines found on the LAN");
            }
            for d in found {
                let paired = if peers.iter().any(|p| p.id == d.id) { "paired" } else { "not paired" };
                println!("{:<20} {}  {paired}  {}", d.name, d.id, d.addrs.join(", "));
            }
        }
    }
    Ok(())
}

/// Open a pairing window, print the code, and wait for it to end (Ctrl-C closes it).
async fn pair(client: &Client, json: bool) -> Result<()> {
    let mut events = client.subscribe().await?;
    let code: PairCode = client.call(Request::RemotePairBegin).await?;
    if json {
        print_json(&code)?;
    } else {
        println!("Pairing code for {}: {}", code.name, spaced(&code.code));
        println!();
        println!("On the other machine: Settings → Machines → Add, or");
        let addr = code.addrs.first().cloned().unwrap_or_else(|| "<this machine's address>".into());
        println!("  quiver remote add {addr} {}", code.code);
        if code.addrs.len() > 1 {
            println!("(this machine's addresses: {})", code.addrs.join(", "));
        }
        println!();
        println!("Waiting up to 2 minutes (Ctrl-C to stop)…");
    }
    loop {
        tokio::select! {
            e = events.recv() => match e {
                Some(Event::PairingEnded { outcome, name, id }) => {
                    match outcome.as_str() {
                        "paired" => {
                            println!("Paired with {} ({}).", name.unwrap_or_default(), id.unwrap_or_default());
                            return Ok(());
                        }
                        "expired" => bail!("the code expired; run `quiver remote pair` again"),
                        "failed" => bail!("three wrong codes: pairing closed; run `quiver remote pair` again"),
                        "cancelled" => bail!("pairing was closed"),
                        other => bail!("pairing ended: {other}"),
                    }
                }
                Some(_) => {}
                None => bail!("lost the connection to quiverd"),
            },
            _ = tokio::signal::ctrl_c() => {
                let _ = client.call::<()>(Request::RemotePairEnd).await;
                bail!("pairing closed");
            }
        }
    }
}
