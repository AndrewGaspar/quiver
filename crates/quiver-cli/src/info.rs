//! `quiver info`, `quiver progress` and `quiver saves`: the game page's data from a terminal.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use quiver_api::info::{GameInfo, Progress, RestoreResult, SaveBackup, SavesReport, SlotState};
use quiver_api::stores::GameDetail;
use quiver_api::{Client, LibraryEntry, Request, Status};

use crate::{print_json, resolve};

#[derive(Subcommand)]
pub enum SavesAction {
    /// Make a backup's slots current (the saves there now are backed up first). BACKUP is a
    /// backup id from `quiver saves <game>`, a unique part of one, or `latest`.
    Restore { backup: String },
    /// Upload saves waiting for the cloud now (e.g. after an upload failed at exit),
    /// without launching the game.
    Sync {
        /// A mod profile's saves (name or id; its save store must be the cloud store).
        #[arg(long)]
        profile: Option<String>,
    },
    /// Save stores: who uses each, which one syncs with the cloud, slots and last change.
    Stores {
        #[arg(long)]
        json: bool,
    },
    /// Create, rename, retire (files kept) or bring back a save store, pick the cloud
    /// store or vanilla's, or restore an earlier revision.
    Store {
        #[command(subcommand)]
        action: crate::savestores::StoreAction,
    },
    /// Merge one store's slots into another (default: into Default): per slot the newer
    /// file wins; the target's saves are backed up first; FROM is left as it was.
    /// `quiver saves <game> merge FROM [into] [TO]`.
    Merge {
        from: String,
        /// `into TO`, or just `TO`.
        #[arg(num_args = 0..=2)]
        into: Vec<String>,
    },
}

pub async fn sync(client: &Client, game: &str, profile: Option<String>) -> Result<()> {
    let e = resolve(client, game).await?;
    let slots: Vec<u32> = client.call(Request::SavesSync { product_id: e.product_id, profile }).await?;
    if slots.is_empty() {
        println!("{}: nothing waiting to upload", e.name);
    } else {
        println!("{}: uploaded slots {slots:?}", e.name);
    }
    Ok(())
}

/// Another store's game, as its client's files describe it.
fn print_detail(d: &GameDetail) -> Result<()> {
    let store = d.game.store.label();
    println!("{} ({})", d.name, d.game);
    let field = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            println!("  {k:<12} {v}");
        }
    };
    field("developer", d.developer.clone());
    field("publisher", d.publisher.clone());
    field("released", d.release_date.map(|t| t.format("%Y-%m-%d").to_string()));
    field("store page", d.store_url.clone());
    field("account", d.account.clone().map(|a| format!("{a} (the {store} client's)")));
    if let Some(i) = &d.install {
        let state = if !i.complete { " (downloading)" } else if i.update_pending { " (update waiting in Steam)" } else { "" };
        println!("  {:<12} {} · {}{state}", "installed", bytes(i.size_bytes), i.dir.display());
        field("platforms", (!i.platforms.is_empty()).then(|| i.platforms.join(", ")));
        field("plays with", i.launch_url.clone());
    }
    if let Some(p) = &d.playtime {
        let two = p.two_weeks_seconds.map(|s| format!(", {}h{:02}m in the last two weeks", s / 3600, s % 3600 / 60)).unwrap_or_default();
        println!("  {:<12} {}h{:02}m{two}; last {}", "played", p.total_seconds / 3600, p.total_seconds % 3600 / 60, when(p.last_played));
    }
    match &d.achievements {
        Some(a) => println!("  {:<12} {} of {} unlocked", "achievements", a.unlocked, a.total),
        None => println!("  {:<12} unknown ({store} hasn't cached them here)", "achievements"),
    }
    if let Some(c) = &d.cloud {
        let pending = if c.unsynced > 0 { format!(", {} not synced", c.unsynced) } else { String::new() };
        println!("  {:<12} {} file(s), {}{pending}; last upload {} (synced by {store})", "cloud", c.files, bytes(c.bytes), when(c.last_upload));
    }
    if !d.dlc.is_empty() {
        let installed = d.dlc.iter().filter(|x| x.installed).count();
        println!("  {:<12} {} listed, {installed} installed", "DLC", d.dlc.len());
    }
    if !d.unavailable.is_empty() {
        println!("  not available: {}", d.unavailable.join(", "));
    }
    Ok(())
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "?".into())
}

fn hours(s: u64) -> String {
    format!("{}h{:02}m", s / 3600, s % 3600 / 60)
}

async fn account_name(client: &Client, id: Option<&str>) -> String {
    let Some(id) = id else { return "?".into() };
    match client.call::<Status>(Request::Status).await {
        Ok(s) => s.accounts.into_iter().find(|a| a.user_id == id).map(|a| a.name).unwrap_or_else(|| id.to_owned()),
        Err(_) => id.to_owned(),
    }
}

/// Where each piece of art comes from: the first source of each slot, in the order the app
/// tries them (`quiver_api::art`), honouring the game's `art` setting.
fn art_line(e: &LibraryEntry, pref: quiver_api::art::ArtPreference) -> String {
    use quiver_api::art::{ArtSlot, candidates};
    let slots = [("tile", ArtSlot::Tile), ("hero", ArtSlot::Hero), ("logo", ArtSlot::Logo)];
    let parts: Vec<String> = slots
        .iter()
        .map(|(name, slot)| {
            let c = candidates(e, *slot, pref);
            match c.first() {
                Some((o, _)) => format!("{name}: {}", o.describe()),
                None => format!("{name}: none"),
            }
        })
        .collect();
    parts.join(", ")
}

pub async fn info(client: &Client, game: &str, json: bool, refresh: bool, all: bool) -> Result<()> {
    let any = crate::resolve_any(client, game).await?;
    if !any.game.is_ubisoft() {
        let d: GameDetail = client.call(Request::GameDetail { game: any.game, refresh }).await?;
        return if json { print_json(&d) } else { print_detail(&d) };
    }
    let e = resolve(client, &crate::game_label(&any.game)).await?;
    let i: GameInfo = client.call(Request::GameInfo { product_id: e.product_id, refresh }).await?;
    if json {
        return print_json(&i);
    }
    println!("{} ({})", i.name, i.product_id);
    let row = |k: &str, v: &str| println!("  {k:<14} {v}");
    if let Some(p) = &i.publisher {
        row("publisher", p);
    }
    if let Some(s) = &i.sort_string {
        row("series", s);
    }
    if !i.owned_by.is_empty() {
        let mut names = Vec::new();
        for id in &i.owned_by {
            names.push(account_name(client, Some(id)).await);
        }
        row("owned by", &names.join(", "));
    }
    if let Some(inst) = &i.install {
        row("installed", &inst.dir.display().to_string());
        let lang = inst.language.as_deref().unwrap_or("?");
        row("size", &format!("{} ({lang})", bytes(inst.size_bytes)));
        if let Some(v) = &inst.version {
            row("version", v);
        }
        if !inst.languages.is_empty() {
            row("languages", &inst.languages.join(", "));
        }
        let mods = Request::Mods(quiver_api::ModsRequest::List { product_id: e.product_id });
        if let Ok(s) = client.call::<quiver_api::mods::ModsState>(mods).await {
            row("plays", &crate::mods::default_label(&s));
        }
    }
    if let Some(g) = &e.steam {
        let label = if i.install.is_some() { "also on Steam" } else { "installed" };
        row(label, &format!("via Steam (app {}, {})", g.app_id, g.install_dir.display()));
    } else if i.install.is_none() {
        row("installed", "no");
    }
    if let Some(r) = &i.release_date {
        row("released", r);
    }
    let art_setting = client
        .call::<serde_json::Value>(Request::SettingsGet { game: e.product_id.into() })
        .await
        .ok()
        .and_then(|v| v["settings"]["art"].as_str().map(str::to_owned));
    let pref = quiver_api::art::ArtPreference::from_setting(art_setting.as_deref());
    row("art from", &art_line(&e, pref));
    let f = &i.features;
    let mut feats = Vec::new();
    for (on, name) in [(f.cloud_saves, "cloud saves"), (f.achievements, "achievements"), (f.challenges, "challenges"), (f.denuvo, "Denuvo")] {
        if on {
            feats.push(name.to_owned());
        }
    }
    if let Some(t) = &f.game_options {
        feats.push(format!("options tool: {t}"));
    }
    if !feats.is_empty() {
        row("features", &feats.join(", "));
    }
    if !f.player_stats.is_empty() {
        row("stats", &f.player_stats.join(", "));
    }
    for l in &i.links {
        row(&l.kind, &l.url);
    }
    if !i.unavailable.is_empty() {
        row("not available", &i.unavailable.join(", "));
    }
    let shown: Vec<_> = i.addons.iter().filter(|a| all || a.visible).collect();
    let hidden = i.addons.len() - shown.len();
    if !i.addons.is_empty() {
        println!("\nadd-ons ({} shown{}):", shown.len(), if hidden > 0 { format!(", {hidden} hidden; --all lists them") } else { String::new() });
    }
    for a in shown {
        let name = a.display_name();
        let kind = serde_json::to_value(a.kind).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        println!("  {} {:>6}  {:<15} {name}", if a.owned { "✓" } else { " " }, a.product_id, kind);
        if let Some(url) = &a.store_url {
            println!("  {:>24}{url}", "");
        }
    }
    Ok(())
}

pub async fn progress(client: &Client, game: &str, json: bool, refresh: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let p: Progress = client.call(Request::GameProgress { product_id: e.product_id, refresh }).await?;
    if json {
        return print_json(&p);
    }
    let who = account_name(client, p.account.as_deref()).await;
    println!("{} — {who} (fetched {})", e.name, when(Some(p.fetched)));
    match p.playtime_seconds {
        Some(s) => {
            let src = match p.playtime_source.as_deref() {
                Some("playtime_service") => "Ubisoft",
                Some(_) => "the game's play-time stat",
                None => "?",
            };
            println!("  play time      {} ({src})", hours(s))
        }
        None => println!("  play time      not available"),
    }
    if let Some(c) = &p.connect {
        let level = c.level.map(|l| l.to_string()).unwrap_or("?".into());
        let xp = match (c.xp, c.next_level_xp) {
            (Some(x), Some(n)) => format!(", {x}/{n} XP"),
            (Some(x), None) => format!(", {x} XP"),
            _ => String::new(),
        };
        let units = c.units.map(|u| format!(", {u} units")).unwrap_or_default();
        println!("  Connect        level {level}{xp}{units}");
    }
    if !p.stats.is_empty() {
        println!("\nstats:");
        for s in &p.stats {
            println!("  {:<34} {}", s.name.as_deref().unwrap_or(&s.id), s.value.as_deref().unwrap_or("—"));
        }
    }
    if let Some(a) = &p.achievements {
        match a.unlocked_known {
            true => println!("\nachievements: {}/{} unlocked", a.unlocked, a.total),
            false => println!("\nachievements: {} (unlock status unavailable; quiver achievements {})", a.total, e.product_id),
        }
        for x in &a.items {
            let at = x.unlocked_at.map(|t| format!("  {}", when(Some(t)))).unwrap_or_default();
            println!("  {} {}{at}", if x.unlocked { "✓" } else { " " }, x.name.as_deref().unwrap_or(&x.id));
        }
    }
    if !p.challenges.is_empty() {
        let done = p.challenges.iter().filter(|c| c.completed).count();
        println!("\nchallenges: {done}/{} completed", p.challenges.len());
        for c in &p.challenges {
            let prog = match (c.progress, c.target) {
                (Some(v), Some(t)) => format!(" ({v}/{t})"),
                _ => String::new(),
            };
            let reward = c.reward.as_deref().map(|r| format!("  [{r}]")).unwrap_or_default();
            println!("  {} {}{prog}{reward}", if c.completed { "✓" } else { " " }, c.name.as_deref().unwrap_or(&c.id));
        }
    }
    if !p.rewards.is_empty() {
        let owned = p.rewards.iter().filter(|r| r.owned).count();
        println!("\nrewards: {owned}/{} unlocked", p.rewards.len());
        for r in &p.rewards {
            let cost = r.cost.map(|c| format!("  ({c} units)")).unwrap_or_default();
            println!("  {} {}{cost}", if r.owned { "✓" } else { " " }, r.name.as_deref().unwrap_or(&r.id));
        }
    }
    for err in &p.errors {
        eprintln!("unavailable: {err}");
    }
    Ok(())
}

fn print_backups(list: &[SaveBackup]) {
    for b in list {
        let slots: Vec<String> = b.slots.iter().map(|(s, n)| format!("{s} ({})", bytes(*n))).collect();
        println!("  {:<26} {}  slots {}  {}", b.id, when(b.created), slots.join(", "), b.reason.clone().or_else(|| b.name.as_ref().map(|_| "named backup (dated by its folder)".into())).unwrap_or_default());
    }
}

pub async fn saves(client: &Client, game: &str, json: bool, local: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let r: SavesReport = client.call(Request::Saves { product_id: e.product_id, cloud: !local }).await?;
    if json {
        return print_json(&r);
    }
    let who = account_name(client, r.account.as_deref()).await;
    println!("{} — saves of {who}", e.name);
    if let Some(d) = &r.dir {
        println!("  {}", d.display());
    }
    if let Some(err) = &r.cloud_error {
        println!("  cloud saves unreadable: {err}");
    }
    match r.cloud_source.as_deref() {
        Some("connect_record") => println!("  cloud column: Ubisoft Connect's record of its last upload (sizes and times only)"),
        None if !local => println!("  cloud: not available"),
        _ => {}
    }
    if let Some(rec) = &r.connect_record {
        println!("  last upload by Ubisoft Connect: {} from {}", when(rec.modified), rec.machine.as_deref().unwrap_or("?"));
    }
    if r.slots.is_empty() {
        println!("  no save slots");
    } else {
        println!("\n  {:>4}  {:<24} {:<24} state", "slot", "local", "cloud");
        for s in &r.slots {
            let local = s.local.as_ref().map(|f| format!("{}  {}", when(f.modified), bytes(f.size))).unwrap_or("—".into());
            let cloud = match &s.cloud {
                Some(c) => format!("{}  {}", when(c.modified), c.size.map(bytes).unwrap_or("?".into())),
                None if r.cloud_source.is_some() => "—".into(),
                None => "?".into(),
            };
            let state = match s.state {
                SlotState::InSync => "in sync",
                SlotState::LocalOnly => "local only",
                SlotState::CloudOnly => "cloud only",
                SlotState::LocalNewer => "local newer",
                SlotState::CloudNewer => "cloud newer",
                SlotState::Differs => "differs",
                SlotState::Unknown => "",
            };
            let pending = if s.pending_upload { " (upload pending)" } else { "" };
            println!("  {:>4}  {local:<24} {cloud:<24} {state}{pending}", s.slot);
        }
    }
    if !r.backups.is_empty() {
        println!("\nbackups (quiver saves {} restore <id>):", e.product_id);
        print_backups(&r.backups);
    }
    Ok(())
}

pub async fn restore(client: &Client, source: &str, game: &str, backup: String) -> Result<()> {
    let e = resolve(client, game).await?;
    // An agent's restore needs a person's OK: ask whoever is at this terminal, if anyone.
    let mut confirmed = false;
    if source.to_ascii_lowercase().starts_with("agent") && std::io::stdin().is_terminal() {
        eprint!("An agent ({source}) wants to restore {}'s save backup {backup:?}. The current saves are backed up first. Allow? [y/N] ", e.name);
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        confirmed = matches!(line.trim(), "y" | "Y" | "yes");
        if !confirmed {
            anyhow::bail!("not restored");
        }
    }
    let r: RestoreResult = client.call(Request::SavesRestore { product_id: e.product_id, backup, confirmed }).await?;
    println!("{}: restored slots {:?}; they upload to the cloud after the next session", e.name, r.restored);
    if let Some(p) = r.previous {
        println!("the saves that were there are backed up as {} (quiver saves {} restore {})", p.id, e.product_id, p.id);
    }
    Ok(())
}
