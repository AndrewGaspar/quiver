//! `quiver system setup` and `quiver doctor`: the parts of Quiver that need root
//! (design/INSTALL.md, `quiver_api::system`).
//!
//! `--check` (and doctor) only look. Without it, setup writes one script for exactly what's
//! missing, shows it in full, saves it under `~/.local/state/quiver/`, and asks before running
//! `sudo sh` on it: one password prompt, the person's, after reading what it does. Quiver
//! never escalates by itself, and agents never run setup (`--source agent` is refused).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use quiver_api::system::firewall::Wanted;
use quiver_api::system::{self, Facts, Options, Report, State, script};
use quiver_api::{Client, Request};

use crate::print_json;

#[derive(Subcommand)]
pub enum SystemAction {
    /// The pieces that need root (udev rules and groups for game controllers, firewall rules,
    /// lingering): `--check` lists them; without it, one script you read, then run with sudo.
    Setup(SetupArgs),
}

#[derive(Args)]
pub struct SetupArgs {
    /// Only list each piece (ok, missing, outdated, not needed here) and what it unlocks.
    #[arg(long)]
    pub check: bool,
    /// Also allow Quiver's servers that are on (Moonlight, remote play) through the firewall,
    /// from these networks only: `lan` (the subnets of your network interfaces), `tailnet`
    /// (tailscale0). Never from anywhere.
    #[arg(long, value_delimiter = ',', value_name = "lan,tailnet")]
    pub firewall: Vec<String>,
    /// quiverd at boot, before anyone signs in: lingering (in the script only where it needs
    /// root), then `quiver remote boot on`.
    #[arg(long)]
    pub at_boot: bool,
    /// Remove what setup installed (its own files, groups, tagged firewall rules, lingering);
    /// pieces you installed yourself stay.
    #[arg(long, conflicts_with_all = ["firewall", "at_boot", "check"])]
    pub undo: bool,
    /// Only print the script.
    #[arg(long)]
    pub print: bool,
    #[arg(long)]
    pub json: bool,
}

/// The check as people read it.
pub fn print_report(r: &Report) {
    let nets = if r.scopes.is_empty() { "none".to_owned() } else { r.scopes.join(", ") };
    println!("System setup for {} (firewall: {}; networks: {nets})", r.user, r.firewall.name());
    for p in &r.pieces {
        println!("  {:<16} {}", p.state.word(), p.title);
        if p.items.len() > 1 || (p.items.len() == 1 && p.state != State::Ok) {
            for i in &p.items {
                println!("{:<21}{}: {}", "", i.what, i.detail);
            }
        } else {
            println!("{:<21}{}", "", p.summary);
        }
        if !p.unlocks.is_empty() && p.state != State::NotNeeded {
            println!("{:<21}for {}", "", p.unlocks);
        }
        if let Some(f) = &p.fix {
            println!("{:<21}→ {f}", "");
        }
    }
    if let Some(p) = &r.contrib_problem {
        println!("  note: {p}");
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("quiver")
}

pub async fn run(source: &str, action: SystemAction) -> Result<()> {
    let SystemAction::Setup(a) = action;
    let firewall = if a.firewall.is_empty() { None } else { Some(Wanted::parse(&a.firewall).map_err(anyhow::Error::msg)?) };
    let opts = Options { firewall, at_boot: a.at_boot };
    let facts = tokio::task::spawn_blocking(Facts::gather).await?;
    let c = system::check(&facts, &opts);
    if a.check {
        if a.json {
            return print_json(&c.report);
        }
        print_report(&c.report);
        let n = c.report.attention().count();
        if n > 0 {
            println!("\n{n} piece{} to set up: quiver system setup{} (it shows the script before anything runs)", if n == 1 { "" } else { "s" }, if c.report.attention().any(|p| p.id.starts_with("firewall-")) { " --firewall lan,tailnet" } else { "" });
        }
        return Ok(());
    }
    let now = chrono::Local::now();
    let stamp = now.format("%Y-%m-%d %H:%M").to_string();
    let text = if a.undo {
        Some(script::undo(&facts, &stamp))
    } else {
        if !c.plan.is_empty()
            && let Some(p) = &c.report.contrib_problem
        {
            bail!("{p}");
        }
        let src = facts.contrib.clone().unwrap_or_else(|| PathBuf::from("/nonexistent"));
        script::install(&c.report, &c.plan, &facts, &src, &stamp)
    };
    if a.print || a.json {
        if a.json {
            return print_json(&serde_json::json!({ "report": c.report, "script": text }));
        }
        match &text {
            Some(t) => print!("{t}"),
            None => println!("# Nothing here needs root: what this machine uses is set up (quiver system setup --check)."),
        }
        if a.at_boot && !a.undo {
            eprintln!("(--at-boot also runs `quiver remote boot on` once the script has run; --print doesn't)");
        }
        return Ok(());
    }
    if !quiver_api::mods::is_human_source(source) {
        bail!(
            "agents don't run `quiver system setup`: it changes the system as root. Tell the person what `quiver system setup --check` \
             lists as missing and let them run it themselves"
        );
    }
    match text {
        None => {
            println!("Nothing here needs root: what this machine uses is set up.");
            for p in c.report.pieces.iter().filter(|p| matches!(p.state, State::Unknown | State::Conflict)) {
                println!("  {} {}: {}", p.state.word(), p.title, p.summary);
            }
        }
        Some(text) => {
            let dir = state_dir();
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            let path = dir.join(format!("system-setup-{}{}.sh", now.format("%Y%m%d-%H%M%S"), if a.undo { "-undo" } else { "" }));
            std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
            println!("{text}");
            println!("(saved to {})", path.display());
            if !std::io::stdin().is_terminal() {
                println!("Not a terminal, so nothing runs: read it, then run  sudo sh {}", path.display());
                return Ok(());
            }
            let answer = crate::dev::prompt("Run it with sudo? [y/N] ")?;
            if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
                println!("Nothing changed. To run it later: sudo sh {}", path.display());
                return Ok(());
            }
            let st = std::process::Command::new("sudo").arg("sh").arg(&path).status().context("run sudo")?;
            if !st.success() {
                bail!("the script stopped ({st}); nothing after the failing step ran. It's at {}", path.display());
            }
        }
    }
    if a.at_boot && !a.undo {
        // The half that needs no root: quiverd.service enabled, and lingering where polkit
        // lets the person turn it on.
        let client = Client::connect(source).await?;
        let b: quiver_api::service::BootStatus = client.call(Request::RemoteBoot { enabled: true }).await?;
        println!("starts at boot: {}", b.describe());
        if let Some(cmd) = &b.linger_command {
            println!("  lingering still needs root: {cmd}");
        }
    }
    let after = system::check(&tokio::task::spawn_blocking(Facts::gather).await?, &opts).report;
    println!();
    print_report(&after);
    Ok(())
}

/// `quiver doctor`: this build, the daemon, and the system setup check.
pub async fn doctor(source: &str, json: bool) -> Result<()> {
    let facts = tokio::task::spawn_blocking(Facts::gather).await?;
    let report = system::check(&facts, &Options::default()).report;
    let daemon: Option<quiver_api::daemon::DaemonStatus> = match Client::connect_running(source).await {
        Ok(c) => tokio::time::timeout(Duration::from_secs(10), c.call(Request::Daemon(quiver_api::daemon::DaemonRequest::Status))).await.ok().and_then(|r| r.ok()),
        Err(_) => None,
    };
    if json {
        return print_json(&serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "quiverd": daemon,
            "system_setup": report,
            "agents": "report missing pieces to the person; never run quiver system setup or sudo yourself",
        }));
    }
    println!("quiver {}", env!("CARGO_PKG_VERSION"));
    match &daemon {
        Some(d) => println!("quiverd: running {} (pid {})", d.build, d.pid),
        None => println!("quiverd: not running (any quiver command starts it)"),
    }
    println!();
    print_report(&report);
    let n = report.attention().count();
    if n > 0 {
        println!("\nquiver system setup installs what's missing (it shows the script before anything runs).");
    }
    Ok(())
}

/// The one line after turning Moonlight on, when a firewall may block it (`None`: nothing
/// to say). Looks at this machine only.
pub fn moonlight_notice() -> Option<String> {
    let mut facts = Facts::gather();
    facts.features.moonlight = true;
    let r = system::check(&facts, &Options::default()).report;
    let p = r.piece("firewall-moonlight")?;
    match p.state {
        State::Missing => Some(format!("{} may block Moonlight: its ports aren't allowed from {}. Run: quiver system setup --firewall lan,tailnet", r.firewall.name(), p.items.iter().filter(|i| i.state == State::Missing).map(|i| i.what.as_str()).collect::<Vec<_>>().join(", "))),
        State::Unknown if p.fix.is_some() => Some(format!("{} is on and Quiver can't tell without root whether it allows Moonlight's ports. Run: quiver system setup --firewall lan,tailnet", r.firewall.name())),
        State::Unknown => Some(format!("{}", p.summary)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        a: SystemAction,
    }

    #[test]
    fn flags() {
        let SystemAction::Setup(a) = T::try_parse_from(["t", "setup", "--firewall", "lan,tailnet", "--at-boot"]).unwrap().a;
        assert_eq!(Wanted::parse(&a.firewall), Ok(Wanted { lan: true, tailnet: true }));
        assert!(a.at_boot && !a.check);
        assert!(T::try_parse_from(["t", "setup", "--undo", "--firewall", "lan"]).is_err());
        let SystemAction::Setup(a) = T::try_parse_from(["t", "setup", "--firewall", "anywhere"]).unwrap().a;
        assert!(Wanted::parse(&a.firewall).is_err(), "never from anywhere");
    }
}
