//! `quiver achievements <game>`: unlock status from Ubisoft, Quiver's report journal, and
//! resending a journaled achievement.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use ubi_api::Client;
use ubi_api::Request;
use ubi_api::achievements::{AchievementsReport, AchievementsRequest, ReportResult, ReportState};

use crate::{print_json, resolve};

#[derive(Subcommand)]
pub enum AchAction {
    /// Send an achievement the game earned through Quiver (it is in the journal) to Ubisoft
    /// again. Agents need a person's approval; `--dry-run` only shows the request.
    Report {
        id: u32,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// One raw read of Ubisoft's achievement service, for checking the protocol by hand
    /// (read-only): what was sent and what came back.
    Probe {
        /// `ach_frontend` platform id (default: learned, else PC).
        #[arg(long)]
        platform: Option<String>,
        /// `raw` (default) or `wrapped`.
        #[arg(long)]
        framing: Option<String>,
        /// Ask for every product instead of this game's.
        #[arg(long)]
        all_products: bool,
    },
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

pub async fn list(client: &Client, game: &str, json: bool, refresh: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let r: AchievementsReport = client.call(Request::Achievements(AchievementsRequest::List { product_id: e.product_id, refresh })).await?;
    if json {
        return print_json(&r);
    }
    let from = match (&r.achievements, r.fetched) {
        (Some(a), Some(t)) if a.unlocked_known => format!("from {}, read {}", a.source.as_deref().unwrap_or("Ubisoft"), when(Some(t))),
        _ => "Ubisoft's unlock status unavailable".into(),
    };
    match &r.achievements {
        Some(a) => println!("{} — {}/{} unlocked ({from}) · reporting {}", e.name, a.unlocked, a.total, r.reporting),
        None => println!("{} — no achievements ({from}) · reporting {}", e.name, r.reporting),
    }
    for x in r.achievements.iter().flat_map(|a| &a.items) {
        let state = x.report.map(|s| format!("  [{}]", s.label())).unwrap_or_default();
        println!(
            "  {} {:>3}  {:<40} {}{state}",
            if x.unlocked { "✓" } else { " " },
            x.id,
            x.name.as_deref().unwrap_or("?"),
            when(x.unlocked_at)
        );
    }
    if !r.journal.is_empty() {
        println!("\njournal (earned through Quiver):");
        for j in &r.journal {
            let confirmed = match j.confirmed {
                Some(true) => ", confirmed by a read-back",
                Some(false) => ", NOT shown by a read-back",
                None => "",
            };
            let err = j.last_error.as_deref().map(|e| format!("\n        last error: {e}")).unwrap_or_default();
            println!(
                "  {:>3}  {:<28} earned {}  reported {}  attempts {}{confirmed}{err}",
                j.achievement_id,
                j.state.label(),
                when(Some(j.earned_at)),
                if j.reported_at.is_some() { when(j.reported_at) } else { "—".into() },
                j.attempts
            );
        }
    }
    for err in &r.errors {
        eprintln!("unavailable: {err}");
    }
    Ok(())
}

pub async fn report(client: &Client, source: &str, game: &str, id: u32, dry_run: bool, json: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    // Agent policy: Ask. Whoever is at this terminal decides; without one the daemon refuses.
    let mut confirmed = false;
    if !dry_run && source.to_ascii_lowercase().starts_with("agent") && std::io::stdin().is_terminal() {
        eprint!("An agent ({source}) wants to send {}'s achievement {id} to Ubisoft again. Allow? [y/N] ", e.name);
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        confirmed = matches!(line.trim(), "y" | "Y" | "yes");
        if !confirmed {
            anyhow::bail!("not sent");
        }
    }
    let r: ReportResult = client
        .call(Request::Achievements(AchievementsRequest::Report { product_id: e.product_id, achievement_id: id, dry_run, confirmed }))
        .await?;
    if json {
        return print_json(&r);
    }
    if r.sent {
        let confirmed = match (r.entry.state, r.entry.confirmed) {
            (ReportState::Reported, Some(true)) => "; a read-back shows it unlocked",
            (ReportState::Reported, Some(false)) => "; but a read-back does NOT show it (Ubisoft may ignore this service for the game)",
            _ => "",
        };
        println!("{}: achievement {id} sent to Ubisoft{confirmed}", e.name);
    } else {
        println!("{}: dry run, nothing sent. The request would be:", e.name);
        println!("{}", serde_json::to_string_pretty(&r.request)?);
        println!("bytes: {}", r.request_hex);
    }
    Ok(())
}

pub async fn probe(client: &Client, game: &str, platform: Option<String>, framing: Option<String>, all_products: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let v: serde_json::Value = client
        .call(Request::Achievements(AchievementsRequest::Probe { product_id: e.product_id, platform_id: platform, framing, all_products }))
        .await?;
    print_json(&v)
}
