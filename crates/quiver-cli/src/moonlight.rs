//! `quiver remote moonlight …`: the Moonlight (GameStream) server, so Moonlight clients on
//! TVs, phones and other PCs can stream from this machine. Off by default. With
//! `--machine`, these run on that machine (entering the PIN for a headless server, too).

use anyhow::Result;
use clap::Subcommand;
use quiver_api::moonlight::{MoonlightClient, MoonlightStatus, is_tailnet_v6};
use quiver_api::{Client, Request};

use crate::print_json;

#[derive(Subcommand)]
pub enum MoonlightAction {
    /// Whether it's on, where it listens, its ports, a pairing waiting for a PIN, and the
    /// paired clients.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Turn it on (it stays on across restarts).
    Enable {
        /// Listen only here: an address or an interface such as tailscale0 (repeatable).
        /// Default: what was set before, else every address.
        #[arg(long)]
        bind: Vec<String>,
        /// Listen on every address again (forget --bind).
        #[arg(long, conflicts_with = "bind")]
        everywhere: bool,
        /// Codecs to offer (repeatable): h264, hevc, hevc10, av1, av1-10. Default: all.
        #[arg(long)]
        codec: Vec<String>,
        /// Don't advertise it on the LAN (mDNS); Moonlight then needs its address typed.
        #[arg(long)]
        no_mdns: bool,
        /// Advertise it on the LAN again (the default).
        #[arg(long, conflicts_with = "no_mdns")]
        mdns: bool,
    },
    /// Turn it off; a stream ends.
    Disable,
    /// Enter the PIN Moonlight shows, for the client waiting to pair.
    Pin { pin: String },
    /// Paired Moonlight clients.
    Clients {
        #[arg(long)]
        json: bool,
    },
    /// Forget a Moonlight client (its fingerprint, a prefix of it, or its name).
    Unpair {
        #[arg(value_name = "CLIENT")]
        client: String,
    },
}

fn print_clients(clients: &[MoonlightClient]) {
    if clients.is_empty() {
        println!("no paired Moonlight clients");
    }
    for c in clients {
        println!("{:<16} {}  from {}  paired {}", c.name, &c.fingerprint[..16.min(c.fingerprint.len())], c.address, c.paired_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"));
    }
}

fn print_status(s: &MoonlightStatus) {
    match (s.enabled, s.listening.is_empty(), &s.error) {
        (true, false, _) => println!("moonlight: on as \"{}\", listening on {}", s.name, s.listening.join(", ")),
        (true, true, Some(e)) => println!("moonlight: on, but not listening: {e}"),
        (true, true, None) => println!("moonlight: on (starting)"),
        (false, ..) => println!("moonlight: off (quiver remote moonlight enable [--bind tailscale0])"),
    }
    if !s.codecs.is_empty() {
        println!("codecs: {}", s.codecs.join(", "));
    }
    match (s.mdns, s.advertised_on.is_empty()) {
        (true, false) => println!("LAN: Moonlight finds it by itself (mDNS on {})", s.advertised_on.join(", ")),
        (true, true) if s.enabled => println!("LAN: not advertised (no LAN interface bound); add it in Moonlight by address"),
        (false, _) => println!("LAN: not advertised (--no-mdns); add it in Moonlight by address"),
        _ => {}
    }
    let tcp: Vec<String> = s.ports.iter().filter(|p| p.protocol == "tcp").map(|p| p.port.to_string()).collect();
    let udp: Vec<String> = s.ports.iter().filter(|p| p.protocol == "udp").map(|p| p.port.to_string()).collect();
    println!("ports: TCP {}; UDP {}", tcp.join(", "), udp.join(", "));
    if s.listening.iter().any(|a| is_tailnet_v6(a)) || s.bind.iter().any(|a| a.starts_with("tailscale")) {
        println!("tip: set Moonlight's video packet size to 1024 for a tailnet IPv6 address (it asks for 1392, too big for Tailscale)");
    }
    if let Some(st) = &s.stream {
        println!("{}: {} to {}", if st.streaming { "streaming" } else { "launched" }, st.app, st.client);
    }
    if let Some(p) = &s.pending {
        println!();
        println!("{} at {} wants to pair: quiver remote moonlight pin <the PIN it shows>", p.device_name, p.address);
    }
    println!();
    print_clients(&s.clients);
}

pub async fn run(client: &Client, action: MoonlightAction) -> Result<()> {
    match action {
        MoonlightAction::Status { json } => {
            let s: MoonlightStatus = client.call(Request::MoonlightStatus).await?;
            if json {
                return print_json(&s);
            }
            print_status(&s);
        }
        MoonlightAction::Enable { bind, everywhere, codec, no_mdns, mdns } => {
            let bind = if everywhere { Some(Vec::new()) } else { Some(bind).filter(|b| !b.is_empty()) };
            let codecs = Some(codec).filter(|c| !c.is_empty());
            let mdns = if no_mdns { Some(false) } else if mdns { Some(true) } else { None };
            let s: MoonlightStatus = client.call(Request::MoonlightEnable { bind, codecs, mdns }).await?;
            print_status(&s);
            println!();
            println!("In Moonlight, add this machine by its address (or Tailscale name), then enter the PIN it shows with `quiver remote moonlight pin`.");
        }
        MoonlightAction::Disable => {
            let () = client.call(Request::MoonlightDisable).await?;
            println!("moonlight: off");
        }
        MoonlightAction::Pin { pin } => {
            let c: MoonlightClient = client.call(Request::MoonlightPin { pin }).await?;
            println!("paired {} ({}) from {}", c.name, &c.fingerprint[..16.min(c.fingerprint.len())], c.address);
        }
        MoonlightAction::Clients { json } => {
            let clients: Vec<MoonlightClient> = client.call(Request::MoonlightClients).await?;
            if json {
                return print_json(&clients);
            }
            print_clients(&clients);
        }
        MoonlightAction::Unpair { client: id } => {
            let c: MoonlightClient = client.call(Request::MoonlightUnpair { fingerprint: id }).await?;
            println!("unpaired {} ({})", c.name, &c.fingerprint[..16.min(c.fingerprint.len())]);
        }
    }
    Ok(())
}
