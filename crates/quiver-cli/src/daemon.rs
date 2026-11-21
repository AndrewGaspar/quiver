//! `quiver daemon …`: the daemon's build, a staged update and what a restart onto it waits
//! on, holds, and stopping it.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quiver_api::daemon::{DaemonRequest, DaemonStatus, Hold, human_secs, parse_duration};
use quiver_api::{Client, Event, Request};

#[derive(Subcommand)]
pub(crate) enum DaemonAction {
    /// Ask quiverd to exit (refused while a game runs). The next quiver command, or the
    /// app, starts it again.
    Stop,
    /// quiverd's build, whether a new one is installed, and what restarting onto it waits
    /// on (games, jobs, perf subscriptions, holds).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Keep quiverd from restarting onto a new build (it restarts on its own once nothing
    /// needs it). Without --for, holds until this command exits (Ctrl-C); with it, for that
    /// long (at most 24h) and returns at once.
    Hold {
        /// Why: shown in `quiver daemon status`.
        #[arg(long)]
        reason: String,
        /// How long: 90s, 30m, 2h, 1h30m.
        #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration)]
        duration: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Release a hold (its number from `quiver daemon status`); without one, the only hold.
    Release {
        id: Option<u64>,
        /// Every hold.
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
}

/// Answers from a daemon built before `daemon` requests never come (it can't tell which
/// request it couldn't read), so don't wait forever.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

async fn call<T: serde::de::DeserializeOwned>(client: &Client, req: DaemonRequest) -> Result<T> {
    match tokio::time::timeout(ANSWER_WITHIN + quiver_api::client::RESTART_RETRY, client.call(Request::Daemon(req))).await {
        Ok(r) => r,
        Err(_) => bail!("quiverd didn't answer; it may be a build from before `quiver daemon status` (restart it once: quiver daemon stop)"),
    }
}

pub(crate) async fn run(source: &str, action: DaemonAction) -> Result<()> {
    // Only a hold starts quiverd: there is nothing to look at or stop otherwise.
    let running = || async { Client::connect_running(source).await.context("quiverd isn't running") };
    match action {
        DaemonAction::Stop => {
            let client = match Client::connect_running(source).await {
                Ok(c) => c,
                Err(_) => {
                    println!("quiverd isn't running");
                    return Ok(());
                }
            };
            let () = client.call(Request::Shutdown).await?;
            println!("quiverd stopping");
        }
        DaemonAction::Status { json } => {
            let s: DaemonStatus = call(&running().await?, DaemonRequest::Status).await?;
            if json {
                return crate::print_json(&s);
            }
            print_status(&s);
        }
        DaemonAction::Hold { reason, duration, json } => {
            let client = Client::connect(source).await?;
            let req = || DaemonRequest::Hold { reason: reason.clone(), for_secs: duration };
            if duration.is_some() {
                let h: Hold = call(&client, req()).await?;
                if json {
                    return crate::print_json(&h);
                }
                println!("{}", h.describe(chrono::Utc::now()));
                println!("release it early with: quiver daemon release {}", h.id);
                return Ok(());
            }
            // Held by this connection: renewed if the daemon comes back after a crash or a
            // `quiver daemon stop`, released when this exits.
            let mut events = client.subscribe().await?;
            let h: Hold = call(&client, req()).await?;
            if json {
                crate::print_json(&h)?;
            } else {
                eprintln!("{} — Ctrl-C to release", h.describe(chrono::Utc::now()));
            }
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    e = events.recv() => match e {
                        None => bail!("lost quiverd"),
                        Some(Event::Reconnected) => {
                            let h: Hold = call(&client, req()).await?;
                            eprintln!("quiverd came back; {}", h.describe(chrono::Utc::now()));
                        }
                        Some(_) => {}
                    },
                }
            }
            eprintln!("released");
        }
        DaemonAction::Release { id, all } => {
            let released: Vec<Hold> = call(&running().await?, DaemonRequest::Release { id, all }).await?;
            if released.is_empty() {
                println!("no holds");
            }
            for h in released {
                println!("released hold #{}: {}", h.id, h.reason);
            }
        }
    }
    Ok(())
}

fn print_status(s: &DaemonStatus) {
    let now = chrono::Utc::now();
    let commit = s.commit.as_deref().map(|c| format!(", commit {}", &c[..c.len().min(10)])).unwrap_or_default();
    println!("quiverd {}{commit}, pid {}", s.build, s.pid);
    let up = human_secs((now - s.started).num_seconds().max(0) as u64);
    match &s.restarted_from {
        Some(from) => println!("up {up}; restarted onto this build from {from} (restart {} of this process)", s.restarts),
        None => println!("up {up}"),
    }
    if let Some(exe) = &s.exe {
        println!("binary: {exe}");
    }
    match (&s.unit, s.in_unit) {
        (Some(u), true) => println!("unit: {u}"),
        (Some(u), false) => println!("runs outside quiverd.service (in {u})"),
        (None, _) => {}
    }
    if let Some(r) = &s.relocation {
        println!("  {r}");
    }
    let busy = if s.waiting_on.is_empty() { "idle".to_owned() } else { format!("busy: {}", s.waiting_on.join(", ")) };
    match (&s.update, s.auto_restart) {
        _ if s.restarting => println!("restarting onto the new build now"),
        (Some(u), auto) => {
            let what = u.version.as_deref().unwrap_or("a new build");
            println!("update staged: {what} (installed {} ago)", human_secs((now - u.since).num_seconds().max(0) as u64));
            if let Some(n) = &u.note {
                println!("  {n}");
            }
            if !auto {
                println!("  auto-restart is off (QUIVER_AUTO_RESTART=0); restart it with: quiver daemon stop");
            } else if s.waiting_on.is_empty() {
                let idle = s.idle_for_secs.unwrap_or(0);
                println!("  idle for {}; restarts onto it after {} idle", human_secs(idle), human_secs(s.idle_secs));
            } else {
                println!("  waiting on: {}", s.waiting_on.join(", "));
            }
        }
        (None, true) => println!("no update staged ({busy}); restarts onto a new build once {} idle", human_secs(s.idle_secs)),
        (None, false) => println!("no update staged ({busy}); auto-restart is off"),
    }
    if !s.holds.is_empty() {
        println!("holds:");
        for h in &s.holds {
            println!("  {}", h.describe(now));
        }
    }
}
