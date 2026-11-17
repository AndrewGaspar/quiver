//! `quiver export …` and `quiver steam …`.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::exports::{
    ExportRecord, ExportState, ExportStatus, PatchState, RestartReport, SteamExportResult, SteamGame, SteamGames, SteamPatchResult,
    SteamUsers,
};
use quiver_api::{Client, Request};
use quiver_core::exports::{Paths, steam};

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
    /// Restart Steam so it shows staged shortcuts and launch options. Always asks first.
    Restart {
        /// Confirm without the prompt (for your own scripts when there's no terminal).
        /// Agents must ask you instead of passing this.
        #[arg(long)]
        yes: bool,
    },
    /// Write staged changes now (only while Steam is closed).
    Apply,
    /// Ubisoft games installed through Steam, and whether each starts through Quiver
    /// instead of Ubisoft Connect.
    Games {
        /// Read this Steam user's launch options (default: whoever installed each game).
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Make a Steam game start through Quiver instead of Ubisoft Connect: its launch
    /// options get `quiver … steam-run --product <id>` in front of `%command%`, keeping
    /// your own (ones already set up, with or without `--product`, are left alone). Nothing
    /// in the game's prefix changes; `unpatch` undoes it.
    Patch {
        /// Steam app id, the game's name (or part of it), its Ubisoft product id, or `all`.
        game: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Make a Steam game start through Ubisoft Connect again: its launch options go back to
    /// what they were.
    Unpatch {
        /// Steam app id, the game's name (or part of it), its Ubisoft product id, or `all`.
        game: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Whether quiverd offers the switch when a Ubisoft game is newly installed in Steam.
    Offers {
        /// `on` or `off`; without it, show the setting.
        state: Option<String>,
    },
}

/// Steam games matching `q` (app id, product id, `all`, or part of the name).
fn pick_games<'a>(games: &'a [SteamGame], q: &str) -> Result<Vec<&'a SteamGame>> {
    if q.eq_ignore_ascii_case("all") {
        return Ok(games.iter().collect());
    }
    if let Ok(n) = q.parse::<u32>() {
        let hits: Vec<&SteamGame> = games.iter().filter(|g| g.app_id == n || g.product_id == Some(n)).collect();
        if !hits.is_empty() {
            return Ok(hits);
        }
    }
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect::<String>();
    let hits: Vec<&SteamGame> = games.iter().filter(|g| norm(&g.name).contains(&norm(q))).collect();
    let list = || games.iter().map(|g| format!("{} ({})", g.name, g.app_id)).collect::<Vec<_>>().join(", ");
    match hits.len() {
        0 if games.is_empty() => bail!("no Ubisoft games installed through Steam"),
        0 => bail!("no Ubisoft game in Steam matches '{q}' (installed: {})", list()),
        1 => Ok(hits),
        _ => bail!("'{q}' matches several Steam games; use the app id ({})", hits.iter().map(|g| format!("{} {}", g.app_id, g.name)).collect::<Vec<_>>().join(", ")),
    }
}

async fn patch(client: &Client, game: &str, user: Option<String>, undo: bool) -> Result<()> {
    let info: SteamGames = client.call(Request::SteamGames { steam_user: user.clone() }).await?;
    if info.root.is_none() {
        bail!("Steam not found");
    }
    let all = game.eq_ignore_ascii_case("all");
    let mut staged = false;
    for g in pick_games(&info.games, game)? {
        if all && (undo != g.state.uses_quiver() || (!undo && !g.patchable)) {
            continue;
        }
        let req = if undo {
            Request::SteamUnpatch { app_id: g.app_id, steam_user: user.clone() }
        } else {
            Request::SteamPatch { app_id: g.app_id, steam_user: user.clone() }
        };
        let r: SteamPatchResult = match client.call(req).await {
            Ok(r) => r,
            Err(e) if all => {
                eprintln!("{}: {e:#}", g.name);
                continue;
            }
            Err(e) => return Err(e),
        };
        staged |= r.staged;
        let what = if undo { "starts through Ubisoft Connect" } else { "starts through Quiver" };
        match (r.unchanged, r.staged) {
            (true, _) if r.dropped_staged => println!("{} {what}; the change that was waiting for Steam is dropped", r.name),
            (true, _) => println!("{} already {what}", r.name),
            (false, false) => println!("{} now {what}", r.name),
            (false, true) => println!("{} {what} once Steam restarts", r.name),
        }
        println!("  launch options: {}", if r.launch_options.is_empty() { "(none)" } else { &r.launch_options });
    }
    if staged {
        println!("{}", restart_hint());
    }
    Ok(())
}

/// A game by name or id. Removing works by id even when the library can't be fetched.
async fn product(client: &Client, game: &str) -> Result<(u32, String)> {
    match resolve(client, game).await {
        Ok(e) => Ok((e.product_id, e.name.clone())),
        Err(err) => match game.parse::<u32>() {
            Ok(id) => Ok((id, id.to_string())),
            Err(_) => Err(err),
        },
    }
}

pub(crate) fn restart_hint() -> &'static str {
    "Steam is running, so this takes effect after it restarts. Run `quiver steam restart` to restart it now (it asks first), or it applies the next time Steam is closed."
}

/// Without `--user`, with several Steam users and none chosen yet: ask in a terminal.
pub(crate) async fn pick_user(client: &Client, user: Option<String>) -> Result<Option<String>> {
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
                    quiver_api::exports::ExportKind::Desktop => "desktop".to_owned(),
                    quiver_api::exports::ExportKind::Steam => format!("steam:{}", s.record.steam_user.as_deref().unwrap_or("?")),
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
        SteamAction::Games { user, json } => {
            let info: SteamGames = client.call(Request::SteamGames { steam_user: user }).await?;
            if json {
                return print_json(&info);
            }
            if info.root.is_none() {
                println!("Steam not found");
                return Ok(());
            }
            if info.games.is_empty() {
                println!("no Ubisoft games installed through Steam");
            }
            for g in &info.games {
                let state = match g.state {
                    PatchState::On => "quiver",
                    PatchState::ByHand => "quiver (set by hand)",
                    PatchState::Waiting => "quiver after Steam restarts",
                    PatchState::WaitingUndo => "connect after Steam restarts",
                    PatchState::Off => "ubisoft connect",
                    PatchState::Unknown | PatchState::Other => "?",
                };
                let product = g.product_id.map(|p| format!("product {p}")).unwrap_or_else(|| "product ?".into());
                println!("{:<9} {:<28} {}  ({product}, Steam user {})", g.app_id, state, g.name, g.steam_user.as_deref().unwrap_or("?"));
                if !g.launch_options.is_empty() {
                    println!("          launch options: {}", g.launch_options);
                }
                if let Some(n) = &g.note {
                    println!("          {n}");
                }
            }
            if info.games.iter().any(|g| matches!(g.state, PatchState::Waiting | PatchState::WaitingUndo)) {
                println!("{}", restart_hint());
            }
            if !info.offers {
                println!("(offers for new installs are off: quiver steam offers on)");
            }
        }
        SteamAction::Patch { game, user } => patch(client, &game, user, false).await?,
        SteamAction::Unpatch { game, user } => patch(client, &game, user, true).await?,
        SteamAction::Offers { state } => {
            let enabled = match state.as_deref().map(str::to_ascii_lowercase).as_deref() {
                None => None,
                Some("on" | "yes" | "true") => Some(true),
                Some("off" | "no" | "false") => Some(false),
                Some(s) => bail!("expected on or off, got '{s}'"),
            };
            let v: serde_json::Value = client.call(Request::SteamOffers { enabled }).await?;
            if v["enabled"].as_bool().unwrap_or(true) {
                println!("On: when a Ubisoft game is installed through Steam, Quiver offers to start it instead of Ubisoft Connect");
            } else {
                println!("Off: Quiver doesn't offer anything for new Steam installs (quiver steam patch still works)");
            }
        }
        SteamAction::Restart { .. } => unreachable!("handled on its own (it may not need the daemon)"),
    }
    Ok(())
}

/// The daemon, unless it's unreachable (then the CLI does it itself).
async fn daemon_for_restart(source: &str) -> Option<Client> {
    match Client::connect(source).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("quiverd isn't reachable ({e:#}); doing it from here");
            None
        }
    }
}

/// A daemon too old for a request (it never ran it), as opposed to one that refused it.
fn unknown_request(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("unknown variant")
}

/// What's left staged after writing: an error naming it, rather than "done".
fn fail_if_left(left: &[String]) -> Result<()> {
    if !left.is_empty() {
        bail!("{} change(s) couldn't be written and still wait for Steam: {}", left.len(), left.join("; "));
    }
    Ok(())
}

/// `quiver steam restart`: only after the user confirms in the terminal, or passes `--yes`
/// themselves. quiverd does the restart (it knows what's staged, whatever this binary's
/// version); without a reachable daemon the CLI does it itself.
pub async fn steam_restart(yes: bool, source: &str) -> Result<()> {
    let paths = Paths::from_env();
    let Some(st) = steam::Steam::find(&paths.home) else { bail!("Steam not found") };
    let client = daemon_for_restart(source).await;
    let pending = match &client {
        Some(c) => c.call::<SteamUsers>(Request::SteamUsers).await.map(|u| u.staged).unwrap_or_else(|_| steam::staged_descriptions(&paths)),
        None => steam::staged_descriptions(&paths),
    };
    if !steam::is_running() {
        let n = match &client {
            Some(c) => {
                let v: serde_json::Value = c.call(Request::SteamApplyStaged).await?;
                let left = c.call::<SteamUsers>(Request::SteamUsers).await?.staged;
                fail_if_left(&left)?;
                v["applied"].as_u64().unwrap_or(0) as usize
            }
            None => {
                let n = steam::apply_staged(&paths, false)?;
                fail_if_left(&steam::staged_descriptions(&paths))?;
                n
            }
        };
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
            eprintln!("No changes are waiting for Steam.");
        } else {
            eprintln!("Waiting for a Steam restart:");
            for s in &pending {
                eprintln!("  {s}");
            }
        }
        let answer = dev::prompt("Restart Steam now? [y/N] ")?;
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("Not restarting. Staged changes are written the next time Steam is closed.");
            return Ok(());
        }
    }
    eprintln!("Closing Steam…");
    // The person confirmed just now (the prompt, or their own --yes).
    let by_daemon = match &client {
        Some(c) => match c.call::<RestartReport>(Request::SteamRestart { confirmed: true }).await {
            Ok(r) => Some(r),
            Err(e) if unknown_request(&e) => {
                eprintln!("quiverd is too old to restart Steam; doing it from here (restart quiverd after updating Quiver)");
                None
            }
            Err(e) => return Err(e),
        },
        None => None,
    };
    let r = match by_daemon {
        Some(r) => r,
        None => tokio::task::spawn_blocking(move || steam::restart_steam(&paths, &st, Duration::from_secs(90))).await??,
    };
    println!("Steam restarted; wrote {} staged change(s).", r.applied);
    Ok(())
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[test]
    fn old_daemons_and_leftovers() {
        assert!(unknown_request(&anyhow::anyhow!("bad request: unknown variant `steam_restart`, expected one of `status`")));
        assert!(!unknown_request(&anyhow::anyhow!("restarting Steam needs the person's confirmation every time")));
        assert!(fail_if_left(&[]).is_ok());
        let e = fail_if_left(&["start through Quiver: \"Far Cry 5\"".into()]).unwrap_err().to_string();
        assert!(e.starts_with("1 change(s) couldn't be written") && e.contains("Far Cry 5"), "{e}");
    }
}
