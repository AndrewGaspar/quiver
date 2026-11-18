//! `quiver epic saves|playtime` and the Epic side of `quiver progress`, `quiver playtime`
//! and `quiver saves` (design/STORE-EPIC.md §9.5).

use std::io::{IsTerminal, Write};

use anyhow::{Result, bail, ensure};
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use quiver_api::epic::EpicRequest;
use quiver_api::epicgame::{EpicAchievements, EpicBackup, EpicGameRequest, EpicPlaytime, EpicSaves, FileState, Resolve, SaveMode, SessionReport, SyncReport};
use quiver_api::{Client, GameRef, Request, StoreId};

use crate::print_json;

#[derive(Subcommand)]
pub enum SavesAction {
    /// Download what's newer in Epic's cloud and upload what's newer here. The first
    /// upload ever asks for your yes (`--yes` gives it).
    Sync {
        /// Settle a conflict: keep this machine's files (`local`) or the cloud's (`cloud`).
        #[arg(long, value_parser = ["local", "cloud"])]
        keep: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Put a backup's files back (the files there now are backed up first). BACKUP is an
    /// id from `quiver epic saves <game>`, a unique part of one, or `latest`.
    Restore { backup: String },
}

fn game_req(r: EpicGameRequest) -> Request {
    Request::Epic(EpicRequest::Game(r))
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "—".into())
}

fn hours(s: u64) -> String {
    format!("{}h{:02}m", s / 3600, s % 3600 / 60)
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// The Epic game a name or ref means (an Epic copy folded into a Ubisoft entry too), and
/// its name.
pub async fn epic_game(client: &Client, game: &str) -> Result<Option<(String, GameRef)>> {
    if let Some(r) = GameRef::parse_ref(game).filter(|r| r.store == StoreId::Epic) {
        let name = crate::resolve_any(client, game).await.map(|e| e.name).unwrap_or_else(|_| r.to_string());
        return Ok(Some((name, r)));
    }
    let e = crate::resolve_any(client, game).await?;
    if e.game.store == StoreId::Epic {
        return Ok(Some((e.name, e.game)));
    }
    Ok(None)
}

async fn need_epic(client: &Client, game: &str) -> Result<(String, GameRef)> {
    match epic_game(client, game).await? {
        Some(g) => Ok(g),
        None => bail!("{game} is not an Epic game (its saves and progress are its store's: quiver saves / quiver progress)"),
    }
}

/// Ask the person at this terminal (a yes/no), for writes to their Epic account.
fn ask(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub async fn saves(client: &Client, game: &str, json: bool, local: bool, action: Option<SavesAction>) -> Result<()> {
    let (name, g) = need_epic(client, game).await?;
    match action {
        None => {
            let s: EpicSaves = client.call(game_req(EpicGameRequest::Saves { game: g, cloud: !local })).await?;
            if json {
                return print_json(&s);
            }
            print_saves(&name, &s);
        }
        Some(SavesAction::Sync { keep, yes }) => {
            let resolve = keep.as_deref().map(|k| if k == "local" { Resolve::KeepLocal } else { Resolve::KeepCloud });
            let mut confirmed = yes || resolve.is_some();
            let first = |r: &anyhow::Error| r.to_string().contains("first upload");
            let r = client.call::<SyncReport>(game_req(EpicGameRequest::SavesSync { game: g, resolve, confirmed })).await;
            let r = match r {
                Err(e) if first(&e) && !confirmed => {
                    if !ask(&format!("Upload {name}'s saves to your Epic account? This is Quiver's first upload; after it, saves upload after each session."))? {
                        bail!("{e:#}");
                    }
                    confirmed = true;
                    client.call::<SyncReport>(game_req(EpicGameRequest::SavesSync { game: g, resolve, confirmed })).await?
                }
                r => r?,
            };
            if json {
                return print_json(&r);
            }
            println!("{name}: {}", r.summary());
            for f in &r.downloaded {
                println!("  ↓ {f}");
            }
            for f in &r.uploaded {
                println!("  ↑ {f}");
            }
            if let Some(v) = &r.version {
                println!("  new cloud version {v}");
            }
            if let Some(b) = &r.backup {
                println!("  this machine's copies were backed up as {} (quiver epic saves {g} restore {})", b.id, b.id);
            }
            for f in &r.conflicts {
                println!("  ! {f} changed here and in the cloud");
            }
            if !r.conflicts.is_empty() {
                println!("  keep one side: quiver epic saves {g} sync --keep local|cloud");
            }
        }
        Some(SavesAction::Restore { backup }) => {
            let v: serde_json::Value = client.call(game_req(EpicGameRequest::SavesRestore { game: g, backup })).await?;
            let b: EpicBackup = serde_json::from_value(v["restored"].clone())?;
            println!("{name}: restored backup {} ({} file(s)); it uploads with the next sync", b.id, b.files);
            if let Ok(p) = serde_json::from_value::<EpicBackup>(v["previous"].clone()) {
                println!("the saves that were there are backed up as {} (quiver epic saves {g} restore {})", p.id, p.id);
            }
        }
    }
    Ok(())
}

/// The Saves view's lines about keeping saves fresh between sessions.
pub fn background_lines(b: &quiver_api::epicgame::SavesFreshness, game: GameRef) -> Vec<String> {
    let mut out = Vec::new();
    if !b.enabled {
        out.push("background  off: saves sync only around launches and on request (quiver epic saves-background on)".to_owned());
        return out;
    }
    let mut first = format!("background  checked {}", when(b.checked_at));
    if let Some(v) = &b.cloud_version {
        first.push_str(&format!(" (cloud {v})"));
    }
    match b.downloaded_at {
        Some(t) => first.push_str(&format!(" · last download {} ({} file(s))", when(Some(t)), b.downloaded.len())),
        None => first.push_str(" · nothing downloaded in the background yet"),
    }
    out.push(first);
    if let Some(n) = &b.note {
        out.push(format!("            not checked: {n}"));
    }
    if let Some(e) = &b.error {
        out.push(format!("            last check failed ({} in a row): {e}; next {}", b.failures, when(b.next_check)));
    } else if b.note.is_none() {
        out.push(format!("            next check {}", when(b.next_check)));
    }
    if !b.conflicts.is_empty() {
        out.push(format!("            CONFLICT: {} changed here and in the cloud; nothing syncs until you choose (quiver epic saves {game} sync --keep local|cloud)", b.conflicts.join(", ")));
    }
    if let Some(t) = b.upload_pending {
        let why = b.upload_error.as_deref().map(|e| format!(": {e}")).unwrap_or_default();
        out.push(format!("            the session of {} didn't upload{why}; tried again in the background once uploads are allowed", when(Some(t))));
    }
    out
}

fn state_word(s: FileState) -> &'static str {
    match s {
        FileState::InSync => "in sync",
        FileState::LocalNewer => "newer here",
        FileState::CloudNewer => "newer in cloud",
        FileState::Conflict => "CONFLICT",
        FileState::DeletedHere => "deleted here",
        FileState::Unknown => "",
    }
}

pub fn print_saves(name: &str, s: &EpicSaves) {
    println!("{name} ({}) — Epic cloud saves of {}", s.game, s.account.as_deref().unwrap_or("?"));
    if s.mode == SaveMode::InGame {
        println!("  saves sync in game");
        if let Some(n) = &s.next {
            println!("  {n}");
        }
        return;
    }
    println!("  folder      {}", s.folder.as_deref().unwrap_or("?"));
    match (&s.dir, &s.dir_note) {
        (Some(d), _) => println!("  here        {}", d.display()),
        (None, Some(n)) => println!("  here        not known yet: {n}"),
        _ => {}
    }
    if !s.include.is_empty() || !s.exclude.is_empty() {
        println!("  files       only {} · never {}", if s.include.is_empty() { "all".into() } else { s.include.join(", ") }, if s.exclude.is_empty() { "—".into() } else { s.exclude.join(", ") });
    }
    match &s.cloud {
        Some(c) => println!("  cloud       version {} ({}, {} file(s), {}); {} version(s) kept", c.name, when(c.at), c.files, bytes(c.bytes), s.cloud_versions),
        None if s.cloud_error.is_some() => println!("  cloud       unreadable: {}", s.cloud_error.as_deref().unwrap_or_default()),
        None => println!("  cloud       nothing uploaded yet"),
    }
    if let Some(l) = &s.last_sync {
        println!("  last sync   {} ({})", when(Some(l.at)), l.by);
    }
    if !s.uploads.open() {
        println!("  uploads     wait for your first yes (quiver epic saves {} sync --yes)", s.game);
    }
    if let Some(b) = &s.background {
        for line in background_lines(b, s.game) {
            println!("  {line}");
        }
    }
    if !s.files.is_empty() {
        println!("\n  {:<40} {:<26} {:<26} state", "file", "here", "cloud");
        for f in &s.files {
            let side = |x: &Option<quiver_api::epicgame::SaveSide>| x.as_ref().map(|x| format!("{}  {}", when(x.modified), bytes(x.size))).unwrap_or_else(|| "—".into());
            println!("  {:<40} {:<26} {:<26} {}", f.path, side(&f.local), side(&f.cloud), state_word(f.state));
        }
    }
    if let Some(n) = &s.next {
        println!("\n  {n}");
    }
    if !s.backups.is_empty() {
        println!("\nbackups (quiver epic saves {} restore <id>):", s.game);
        for b in &s.backups {
            println!("  {:<20} {}  {} file(s), {}  {}", b.id, when(b.created), b.files, bytes(b.bytes), b.reason.as_deref().unwrap_or(""));
        }
    }
}

fn report_word(s: SessionReport) -> &'static str {
    match s {
        SessionReport::Queued => "queued",
        SessionReport::Held => "waiting for your first yes",
        SessionReport::Off => "kept here (reporting off)",
        SessionReport::Reported => "reported",
        SessionReport::Failed => "refused by Epic",
        SessionReport::Unconfirmed => "sent, no answer (not resent)",
        SessionReport::Discarded => "dropped (not reported)",
        SessionReport::Unknown => "?",
    }
}

fn print_queue(p: &EpicPlaytime) {
    if p.queue.is_empty() {
        return;
    }
    println!("\n  sessions Quiver reports to Epic:");
    for q in p.queue.iter().take(15) {
        let err = q.last_error.as_deref().map(|e| format!("  ({e})")).unwrap_or_default();
        println!("  {}  {:>4} min  {}{err}", when(Some(q.started)), q.seconds / 60, report_word(q.state));
    }
}

pub async fn playtime(client: &Client, game: Option<String>, json: bool, send: bool, yes: bool, discard: bool) -> Result<()> {
    if discard {
        ensure!(!send, "--send and --discard don't go together");
        let g = match &game {
            Some(x) => Some(need_epic(client, x).await?),
            None => None,
        };
        let p: EpicPlaytime = client.call(game_req(EpicGameRequest::PlaytimeDiscard { game: g.as_ref().map(|g| g.1) })).await?;
        if json {
            return print_json(&p);
        }
        if p.queue.is_empty() {
            println!("nothing waiting to be reported");
        }
        for q in &p.queue {
            println!("dropped  {}  {}  {:>4} min  (never sent to Epic)", q.game, when(Some(q.started)), q.seconds / 60);
        }
        return Ok(());
    }
    match send {
        true => {
            let mut confirmed = yes;
            let r = client.call::<EpicPlaytime>(game_req(EpicGameRequest::PlaytimeSend { confirmed })).await;
            let p = match r {
                Err(e) if e.to_string().contains("first play-time report") && !confirmed => {
                    if !ask("Report your queued play sessions to your Epic account? This is Quiver's first report; after it, sessions are reported when they end.")? {
                        bail!("{e:#}");
                    }
                    confirmed = true;
                    client.call::<EpicPlaytime>(game_req(EpicGameRequest::PlaytimeSend { confirmed })).await?
                }
                r => r?,
            };
            if json {
                return print_json(&p);
            }
            if p.queue.is_empty() {
                println!("nothing waiting to be reported");
            }
            for q in &p.queue {
                println!("{}  {}  {:>4} min  {}", q.game, when(Some(q.started)), q.seconds / 60, report_word(q.state));
            }
        }
        false => {
            let g = match &game {
                Some(x) => Some(need_epic(client, x).await?),
                None => None,
            };
            let p: EpicPlaytime = client.call(game_req(EpicGameRequest::Playtime { game: g.as_ref().map(|g| g.1), refresh: true })).await?;
            if json {
                return print_json(&p);
            }
            let names = if g.is_none() { game_names(client, &p).await } else { Default::default() };
            print_playtime_named(g.as_ref().map(|g| g.0.as_str()), &p, &names);
        }
    }
    Ok(())
}

/// Game names for Epic's per-app totals (app names are ids like `Tautog` or a 32-hex
/// item id): the title the library knows, else the app name as Epic gave it.
async fn game_names(client: &Client, p: &EpicPlaytime) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for (app, _) in p.games.iter().take(25) {
        let Ok(g) = GameRef::new(StoreId::Epic, app) else { continue };
        if let Ok(d) = client.call::<quiver_api::stores::GameDetail>(Request::GameDetail { game: g, refresh: false }).await
            && !d.name.is_empty()
        {
            out.insert(app.clone(), d.name);
        }
    }
    out
}

pub fn print_playtime(name: Option<&str>, p: &EpicPlaytime) {
    print_playtime_named(name, p, &Default::default());
}

fn print_playtime_named(name: Option<&str>, p: &EpicPlaytime, names: &std::collections::HashMap<String, String>) {
    match (name, p.total_seconds) {
        (Some(n), Some(s)) => println!("{n}: {} on Epic", hours(s)),
        (Some(n), None) => println!("{n}: Epic's play time unavailable{}", p.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default()),
        (None, _) => {
            if let Some(e) = &p.error {
                println!("Epic's play time unavailable: {e}");
            }
            for (app, s) in p.games.iter().take(25) {
                match names.get(app) {
                    Some(n) => println!("  {:>8}  {n}  (epic:{app})", hours(*s)),
                    None => println!("  {:>8}  epic:{app}", hours(*s)),
                }
            }
        }
    }
    let reporting = match (p.reporting, p.reports.open()) {
        (false, _) => "off (report_playtime)",
        (true, false) => "on, waiting for your first yes (quiver epic playtime --send --yes)",
        (true, true) => "on",
    };
    println!("  reporting   {reporting}");
    print_queue(p);
}

/// `quiver progress epic:<app>`: Epic's play time and the account's achievements.
pub async fn progress(client: &Client, name: &str, g: GameRef, json: bool, refresh: bool) -> Result<()> {
    let a: EpicAchievements = client.call(game_req(EpicGameRequest::Achievements { game: g, refresh })).await?;
    let p: EpicPlaytime = client.call(game_req(EpicGameRequest::Playtime { game: Some(g), refresh })).await?;
    if json {
        return print_json(&serde_json::json!({ "achievements": a, "playtime": p }));
    }
    println!("{name} — {} (Epic, fetched {})", a.account.as_deref().unwrap_or("?"), when(a.fetched.or(p.fetched)));
    match p.total_seconds {
        Some(s) => println!("  play time      {} (Epic)", hours(s)),
        None => println!("  play time      not available{}", p.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()),
    }
    match &a.achievements {
        Some(x) if x.total == 0 => println!("\nachievements: none in Epic's store for this game"),
        Some(x) => {
            let xp = a.xp.map(|(m, t)| format!(", {m}/{t} XP")).unwrap_or_default();
            println!("\nachievements: {}/{} unlocked{xp} (the game reports them to Epic itself)", x.unlocked, x.total);
            let mut list: Vec<_> = x.list.iter().collect();
            list.sort_by_key(|a| (!a.unlocked, std::cmp::Reverse(a.unlocked_at)));
            for x in list {
                let name = if x.hidden && !x.unlocked { "(hidden)".to_owned() } else { x.name.clone() };
                let at = x.unlocked_at.map(|t| format!("  {}", when(Some(t)))).unwrap_or_default();
                println!("  {} {name}{at}", if x.unlocked { "✓" } else { " " });
            }
        }
        None => println!("\nachievements: unknown"),
    }
    for e in &a.errors {
        eprintln!("unavailable: {e}");
    }
    Ok(())
}
