//! `quiver saves <game> stores|store|merge`: a game's save stores (named sets of save
//! slots that profiles point at, independently of their mods).

use std::io::{IsTerminal, Write};

use anyhow::{Result, bail};
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use ubi_api::savestores::{MergeOutcome, MergeReport, SaveStoreInfo, SaveStoreOp, SaveStoresReport, SaveStoresRevision};
use ubi_api::{Client, Request};

use crate::mods::resolve_installed as resolve;
use crate::print_json;

#[derive(Subcommand)]
pub enum StoreAction {
    /// A new, empty store (`savegames/<user>/<product>-<id>`).
    Create { name: String },
    Rename { store: String, name: String },
    /// Hide a store no profile uses; its files stay (unretire brings it back).
    Retire { store: String },
    Unretire { store: String },
    /// The one store that syncs with Ubisoft's cloud (`none`: no store does).
    Cloud { store: String },
    /// The store vanilla launches use (normally Default).
    Vanilla { store: String },
    /// The stores' revisions.
    History,
    /// Make an earlier revision current again.
    Restore { revision: u32 },
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "—".into())
}

/// For "Ask" actions under an agent source: ask whoever is at this terminal, if anyone.
fn ask(source: &str, what: &str) -> Result<bool> {
    if !source.to_ascii_lowercase().starts_with("agent") {
        return Ok(false);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("An agent ({source}) wants to {what}. Allow? [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim(), "y" | "Y" | "yes") {
        bail!("not done");
    }
    Ok(true)
}

fn print_store(i: &SaveStoreInfo, vanilla: &str) {
    let s = &i.store;
    let mut flags = Vec::new();
    if s.cloud {
        flags.push("cloud".to_owned());
    }
    if s.id == vanilla {
        flags.push("vanilla".to_owned());
    }
    let users: Vec<&str> = i.used_by.iter().map(|u| u.name.as_str()).collect();
    println!(
        "  {:<9} {:<20} {:>2} slot{}  last {}  {}{}",
        s.id,
        s.name,
        i.slots,
        if i.slots == 1 { " " } else { "s" },
        when(i.last_modified),
        if users.is_empty() { "used by nothing".to_owned() } else { format!("used by {}", users.join(", ")) },
        if flags.is_empty() { String::new() } else { format!("  [{}]", flags.join(", ")) }
    );
    println!("            savegames/<user>/{}{}", s.folder, s.note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default());
}

pub async fn stores(client: &Client, game: &str, json: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let r: SaveStoresReport = client.call(Request::SaveStores { product_id: e.product_id }).await?;
    if json {
        return print_json(&r);
    }
    println!("{} — save stores (r{})", e.name, r.revision);
    match &r.cloud {
        Some(id) => {
            let name = r.stores.iter().find(|s| &s.store.id == id).map_or(id.as_str(), |s| s.store.name.as_str());
            println!("  only '{name}' syncs with Ubisoft's cloud (one set of saves per game and account); the others stay on this PC");
        }
        None => println!("  no store syncs with Ubisoft's cloud (quiver saves {} store cloud default)", e.product_id),
    }
    for s in &r.stores {
        print_store(s, &r.vanilla);
    }
    if !r.retired.is_empty() {
        println!("retired (files kept; quiver saves {} store unretire ID):", e.product_id);
        for s in &r.retired {
            print_store(s, &r.vanilla);
        }
    }
    if !r.unassigned.is_empty() {
        let names: Vec<&str> = r.unassigned.iter().map(|u| u.name.as_str()).collect();
        println!("not given a store yet: {} (quiver mods {} migrate, or launch them)", names.join(", "), e.product_id);
    }
    Ok(())
}

fn print_revision(r: &SaveStoresRevision) {
    let live: Vec<String> = r
        .stores
        .iter()
        .filter(|s| !s.retired)
        .map(|s| format!("{}{}", s.name, if s.cloud { " (cloud)" } else { "" }))
        .collect();
    println!(
        "r{:<3} {}  by {:<8} {}  [{}; vanilla: {}]",
        r.revision,
        r.at.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
        r.source,
        r.message.as_deref().unwrap_or(""),
        live.join(", "),
        r.get(&r.vanilla).map_or(r.vanilla.as_str(), |s| s.name.as_str())
    );
}

pub async fn store(client: &Client, source: &str, game: &str, action: StoreAction) -> Result<()> {
    let e = resolve(client, game).await?;
    let product_id = e.product_id;
    let op = match action {
        StoreAction::History => {
            let revs: Vec<SaveStoresRevision> = client.call(Request::SaveStoreHistory { product_id }).await?;
            if revs.is_empty() {
                println!("{}: nothing recorded yet (Default only, synced with the cloud)", e.name);
            }
            revs.iter().for_each(print_revision);
            return Ok(());
        }
        StoreAction::Create { name } => SaveStoreOp::Create { name },
        StoreAction::Rename { store, name } => SaveStoreOp::Rename { store, name },
        StoreAction::Retire { store } => SaveStoreOp::Retire { store },
        StoreAction::Unretire { store } => SaveStoreOp::Unretire { store },
        StoreAction::Cloud { store } => SaveStoreOp::Cloud { store: Some(store) },
        StoreAction::Vanilla { store } => SaveStoreOp::Vanilla { store },
        StoreAction::Restore { revision } => SaveStoreOp::Restore { revision },
    };
    let confirmed = match &op {
        SaveStoreOp::Cloud { store } => ask(source, &format!("make '{}' {}'s cloud save store", store.as_deref().unwrap_or("none"), e.name))?,
        SaveStoreOp::Restore { revision } => ask(source, &format!("restore {}'s save stores to revision {revision}", e.name))?,
        _ => false,
    };
    let r: SaveStoresRevision = client.call(Request::SaveStoreEdit { product_id, op: op.clone(), message: None, confirmed }).await?;
    print!("{}: ", e.name);
    print_revision(&r);
    if let SaveStoreOp::Cloud { .. } = op {
        println!(
            "the next launch with that store downloads the cloud's slots over the ones here (they're backed up first; slots marked for upload win)"
        );
    }
    Ok(())
}

fn outcome(o: MergeOutcome) -> &'static str {
    match o {
        MergeOutcome::Copied => "copied in (only the source had it)",
        MergeOutcome::Replaced => "replaced by the source's newer copy",
        MergeOutcome::Kept => "kept (the target's is newer)",
        MergeOutcome::Same => "same in both",
        MergeOutcome::TargetOnly => "only in the target",
    }
}

pub async fn merge(client: &Client, source: &str, game: &str, from: String, into: Vec<String>) -> Result<()> {
    let e = resolve(client, game).await?;
    let into = match into.as_slice() {
        [] => None,
        [kw] if kw == "into" => bail!("merge FROM into TO: name the target store"),
        [kw, to] if kw == "into" => Some(to.clone()),
        [to] => Some(to.clone()),
        _ => bail!("merge FROM [into] [TO]"),
    };
    let target = into.clone().unwrap_or_else(|| "default".into());
    let confirmed = ask(source, &format!("merge {}'s save store '{from}' into '{target}' (its saves are backed up first)", e.name))?;
    let r: MergeReport = client.call(Request::SavesMerge { product_id: e.product_id, from, into, confirmed }).await?;
    println!("{}: merged save store '{}' into '{}'", e.name, r.from.name, r.into.name);
    if r.slots.is_empty() {
        println!("  '{}' has no save slots on this PC; nothing to do", r.from.name);
    }
    let accounts: std::collections::BTreeSet<&str> = r.slots.iter().map(|s| s.account.as_str()).collect();
    for s in &r.slots {
        let who = if accounts.len() > 1 { format!("{} ", s.account) } else { String::new() };
        println!(
            "  {who}slot {:>2}: {:<38} from {}  into {}",
            s.slot,
            outcome(s.outcome),
            when(s.from_modified),
            when(s.into_modified)
        );
    }
    for b in &r.backups {
        println!("  saves in '{}' before the merge: backup {} (quiver saves {} restore {})", r.into.name, b.id, e.product_id, b.id);
    }
    if r.marked_for_upload && r.slots.iter().any(|s| matches!(s.outcome, MergeOutcome::Copied | MergeOutcome::Replaced)) {
        println!("  '{}' syncs with the cloud: the slots taken from '{}' upload after the next session", r.into.name, r.from.name);
    }
    println!("  '{}' is unchanged. Point profiles at '{}' (quiver mods {} profile set PROFILE saves={}), then retire it if you like (files kept): quiver saves {} store retire {}", r.from.name, r.into.name, e.product_id, r.into.id, e.product_id, r.from.id);
    Ok(())
}
