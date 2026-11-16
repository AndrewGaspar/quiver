//! `quiver export …` and `quiver steam …`.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use ubi_api::exports::{ExportRecord, ExportState, ExportStatus, SteamExportResult, SteamUsers};
use ubi_api::{Client, Request};
use ubi_core::exports::{Paths, steam};

use crate::{dev, print_json, resolve};

#[derive(Subcommand)]
pub enum ExportAction {
    /// Write the game's .desktop entry (app launchers), or remove it.
    Desktop {
        game: String,
        #[arg(long)]
        remove: bool,
    },
    /// Add the game to Steam as a non-Steam shortcut (with its art), or remove it. If Steam
    /// is running the change waits for a Steam restart.
    Steam {
        game: String,
        #[arg(long)]
        remove: bool,
        /// Steam user (id or name); needed once when the machine has several, then remembered.
        #[arg(long)]
        user: Option<String>,
    },
    /// Exports Quiver made, and whether each is still as written.
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum SteamAction {
    /// Steam users on this machine and which one gets shortcuts.
    Users {
        #[arg(long)]
        json: bool,
    },
    /// Restart Steam so it shows staged shortcuts. Always asks first.
    Restart {
        /// Confirm without the prompt (for your own scripts when there's no terminal).
        /// Agents must ask you instead of passing this.
        #[arg(long)]
        yes: bool,
    },
    /// Write staged shortcut changes now (only while Steam is closed).
    Apply,
}

/// A game by name or id. Removing works by id even when the library can't be fetched.
async fn product(client: &Client, game: &str) -> Result<(u32, String)> {
    match resolve(client, game).await {
        Ok(e) => Ok((e.product_id, e.name)),
        Err(err) => match game.parse::<u32>() {
            Ok(id) => Ok((id, id.to_string())),
            Err(_) => Err(err),
        },
    }
}

fn restart_hint() -> &'static str {
    "Steam is running, so it will show this after it restarts. Run `quiver steam restart` to restart it now (it asks first), or it applies the next time Steam is closed."
}

/// Without `--user`, with several Steam users and none chosen yet: ask in a terminal.
async fn pick_user(client: &Client, user: Option<String>) -> Result<Option<String>> {
    if user.is_some() {
        return Ok(user);
    }
    let info: SteamUsers = client.call(Request::SteamUsers).await?;
    if info.selected.is_some() || info.users.len() < 2 {
        return Ok(None);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(None); // the daemon's error lists the users
    }
    eprintln!("Which Steam user gets the shortcut? (remembered for next time)");
    for (i, u) in info.users.iter().enumerate() {
        eprintln!("  {}) {} ({})", i + 1, u.persona_name.as_deref().unwrap_or("?"), u.id);
    }
    let answer = dev::prompt("Number: ")?;
    match answer.parse::<usize>().ok().and_then(|n| info.users.get(n.wrapping_sub(1))) {
        Some(u) => Ok(Some(u.id.clone())),
        None => bail!("no Steam user chosen"),
    }
}

pub async fn export(client: &Client, action: ExportAction) -> Result<()> {
    match action {
        ExportAction::Desktop { game, remove } => {
            let (product_id, name) = product(client, &game).await?;
            if remove {
                let v: serde_json::Value = client.call(Request::ExportDesktop { product_id, remove }).await?;
                if v["removed"].as_bool() == Some(true) {
                    println!("removed {}", v["path"].as_str().unwrap_or_default());
                } else {
                    println!("{name} had no .desktop entry");
                }
            } else {
                let r: ExportRecord = client.call(Request::ExportDesktop { product_id, remove }).await?;
                println!("{}: wrote {}", r.name, r.path.display());
            }
        }
        ExportAction::Steam { game, remove, user } => {
            let (product_id, name) = product(client, &game).await?;
            let steam_user = if remove { user } else { pick_user(client, user).await? };
            let r: SteamExportResult = client.call(Request::ExportSteam { product_id, remove, steam_user }).await?;
            let what = if remove { "removed the Steam shortcut for" } else { "added a Steam shortcut for" };
            println!("{what} {name} (Steam user {}, appid {})", r.steam_user, r.appid);
            if r.applied_staged > 0 {
                println!("also wrote {} earlier staged change(s)", r.applied_staged);
            }
            if r.staged {
                println!("{}", restart_hint());
            }
        }
        ExportAction::List { json } => {
            let list: Vec<ExportStatus> = client.call(Request::ExportList).await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no exports");
            }
            for s in list {
                let state = match s.state {
                    ExportState::Ok => "ok",
                    ExportState::WaitingForSteam => "restart Steam",
                    ExportState::Missing => "missing",
                    ExportState::Changed => "changed",
                };
                let kind = match s.record.kind {
                    ubi_api::exports::ExportKind::Desktop => "desktop".to_owned(),
                    ubi_api::exports::ExportKind::Steam => format!("steam:{}", s.record.steam_user.as_deref().unwrap_or("?")),
                };
                println!("{:<14} {:<16} {:<14} {}  ({})", state, kind, s.record.key, s.record.name, s.record.path.display());
            }
        }
    }
    Ok(())
}

pub async fn steam(client: &Client, action: SteamAction) -> Result<()> {
    match action {
        SteamAction::Users { json } => {
            let info: SteamUsers = client.call(Request::SteamUsers).await?;
            if json {
                return print_json(&info);
            }
            let Some(root) = &info.root else {
                println!("Steam not found");
                return Ok(());
            };
            println!("Steam at {} ({})", root.display(), if info.running { "running" } else { "not running" });
            for u in &info.users {
                println!(
                    "{} {:<12} {}{}",
                    if info.selected.as_deref() == Some(&u.id) { "*" } else { " " },
                    u.id,
                    u.persona_name.as_deref().unwrap_or("?"),
                    if u.most_recent { "  (last signed in)" } else { "" }
                );
            }
            if info.selected.is_none() && info.users.len() > 1 {
                println!("no user chosen for shortcuts yet (quiver export steam <game> --user <id|name>)");
            }
            if !info.staged.is_empty() {
                println!("waiting for a Steam restart:");
                for s in &info.staged {
                    println!("  {s}");
                }
            }
        }
        SteamAction::Apply => {
            let v: serde_json::Value = client.call(Request::SteamApplyStaged).await?;
            println!("wrote {} staged change(s)", v["applied"]);
        }
        SteamAction::Restart { .. } => unreachable!("handled before connecting to the daemon"),
    }
    Ok(())
}

/// `quiver steam restart`: runs here in the CLI (never in the daemon), and only after the
/// user confirms in the terminal, or passes `--yes` themselves.
pub fn steam_restart(yes: bool, source: &str) -> Result<()> {
    let paths = Paths::from_env();
    let Some(st) = steam::Steam::find(&paths.home) else { bail!("Steam not found") };
    let pending = steam::staged(&paths);
    if !steam::is_running() {
        let n = steam::apply_staged(&paths, false)?;
        println!("Steam isn't running; wrote {n} staged change(s). Steam shows them when it next starts.");
        return Ok(());
    }
    if steam::game_running() {
        bail!("a game launched from Steam is running; restart Steam after it exits");
    }
    if yes && source != "cli" {
        bail!("--yes is only for you, not for '{source}': restarting Steam needs the user's own confirmation");
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("restarting Steam needs your confirmation: run this in a terminal (or pass --yes yourself)");
        }
        if pending.is_empty() {
            eprintln!("No shortcut changes are waiting for Steam.");
        } else {
            eprintln!("Waiting for a Steam restart:");
            for s in &pending {
                eprintln!("  {}", s.describe());
            }
        }
        let answer = dev::prompt("Restart Steam now? [y/N] ")?;
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("Not restarting. Staged changes are written the next time Steam is closed.");
            return Ok(());
        }
    }
    eprintln!("Closing Steam…");
    let r = steam::restart_steam(&paths, &st, Duration::from_secs(90))?;
    println!("Steam restarted; wrote {} shortcut change(s).", r.applied);
    Ok(())
}
