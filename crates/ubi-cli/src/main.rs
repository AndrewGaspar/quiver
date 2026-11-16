//! `ubi`: command-line front end. Everything except `ubi dev …` goes through the `ubid`
//! daemon (started on demand), so the CLI, the GUI and agents share one runtime and one
//! history of changes. Label automated callers with `--source agent` (or `UBI_SOURCE`).

mod dev;

use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ubi_api::{ActivityEntry, Client, Event, LibraryEntry, LoginResult, Request, Revision, SettingOp, Status};

#[derive(Parser)]
#[command(name = "ubi", about = "Native Ubisoft launcher for Linux")]
struct Cli {
    /// Who is making changes (recorded in settings history and the activity log).
    #[arg(long, global = true, env = "UBI_SOURCE", default_value = "cli")]
    source: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon and account status, running games and jobs.
    Status,
    /// Sign in (email, password, 2FA).
    Login {
        #[arg(long)]
        email: Option<String>,
    },
    /// Sign out and forget the stored session.
    Logout,
    /// Owned games and their install state.
    Library {
        /// Re-fetch from Ubisoft instead of using the daemon's cache.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install or update a game (follows progress unless --detach).
    Install {
        game: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        detach: bool,
    },
    /// Adopt an existing install (e.g. from Lutris): copies only the game's own files
    /// (reflinks on btrfs/xfs) and lists anything else it found, such as mods.
    Import {
        game: String,
        path: std::path::PathBuf,
        #[arg(long)]
        language: Option<String>,
    },
    /// Remove a game's files (and with --prefix, its Wine prefix and local saves).
    Uninstall {
        game: String,
        #[arg(long)]
        prefix: bool,
    },
    /// Launch a game (follows it until it exits unless --detach).
    Play {
        game: String,
        #[arg(long)]
        detach: bool,
    },
    /// Stop a running game.
    Stop { game: String },
    /// Show a game's launch settings.
    Get { game: String },
    /// Change launch settings: key=value to set, key= to unset.
    /// Keys: proton, language, args, wrappers, env.NAME, dll.NAME.
    Set {
        game: String,
        #[arg(required = true)]
        changes: Vec<String>,
        /// Note recorded with this revision.
        #[arg(short, long)]
        message: Option<String>,
    },
    /// A game's settings history.
    History { game: String },
    /// Make an earlier settings revision current again (0 = defaults).
    Restore { game: String, revision: u32 },
    /// Installed Proton builds.
    Protons,
    /// Recent changes and actions, with who made them.
    Activity {
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// Print daemon events as they happen.
    Watch,
    /// Control the daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Low-level tools that bypass the daemon.
    Dev {
        #[command(subcommand)]
        cmd: dev::DevCmd,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Ask ubid to exit.
    Stop,
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Resolve a game argument: product id, or a unique case-insensitive name match.
async fn resolve(client: &Client, game: &str) -> Result<LibraryEntry> {
    let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false }).await?;
    if let Ok(id) = game.parse::<u32>() {
        return lib.into_iter().find(|e| e.product_id == id).with_context(|| format!("product {id} is not in your library"));
    }
    let norm = |s: &str| s.to_lowercase().replace(['®', '™', '’', '\''], "");
    let needle = norm(game);
    let matches: Vec<LibraryEntry> = lib.into_iter().filter(|e| norm(&e.name).contains(&needle)).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no game matches '{game}'"),
        _ => bail!(
            "'{game}' matches several games: {}",
            matches.iter().map(|e| format!("{} ({})", e.name, e.product_id)).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Print job events for `job_id` until it finishes.
async fn follow(mut events: tokio::sync::mpsc::UnboundedReceiver<Event>, job_id: u64) -> Result<()> {
    let mut last_status = String::new();
    while let Some(e) = events.recv().await {
        let Event::Job(job) = e else { continue };
        if job.id != job_id {
            continue;
        }
        if job.total > 0 && !job.done {
            eprint!(
                "\r{:5.1}%  {:.2}/{:.2} GB  {}   ",
                100.0 * job.downloaded as f64 / job.total as f64,
                job.downloaded as f64 / 1e9,
                job.total as f64 / 1e9,
                job.status
            );
            let _ = std::io::stderr().flush();
        } else if job.status != last_status {
            eprintln!("{}", job.status);
        }
        last_status = job.status.clone();
        if job.done {
            if let Some(err) = job.error {
                bail!(err);
            }
            if !job.detail.is_empty() {
                eprintln!("{} other file(s) were not copied:", job.detail.len());
                for d in &job.detail {
                    eprintln!("  {d}");
                }
            }
            return Ok(());
        }
    }
    bail!("lost connection to ubid")
}

fn print_settings(v: &serde_json::Value) {
    println!("revision {}", v["revision"]);
    let s = &v["settings"];
    if s.as_object().is_none_or(|o| o.is_empty()) {
        println!("  (defaults)");
    }
    for (k, val) in s.as_object().into_iter().flatten() {
        println!("  {k}: {val}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Cmd::Dev { cmd } = cli.cmd {
        return dev::run_dev(cmd).await;
    }
    let client = Client::connect(&cli.source).await?;
    match cli.cmd {
        Cmd::Dev { .. } => unreachable!(),
        Cmd::Status => {
            let s: Status = client.call(Request::Status).await?;
            println!("ubid {}", s.version);
            match &s.signed_in {
                Some(n) => println!("signed in as {n}"),
                None if s.awaiting_2fa => println!("waiting for a 2FA code (ubi login)"),
                None => println!("not signed in (ubi login)"),
            }
            println!("default Proton: {}", s.proton_default.as_deref().unwrap_or("?"));
            if !s.running.is_empty() {
                println!("running: {:?}", s.running);
            }
            for j in s.jobs.iter().filter(|j| !j.done) {
                println!("job {} {:?} {}: {}", j.id, j.kind, j.name, j.status);
            }
        }
        Cmd::Login { email } => {
            let email = match email {
                Some(e) => e,
                None => dev::prompt("Email: ")?,
            };
            let password = rpassword::prompt_password("Password: ")?;
            let mut result: LoginResult = client.call(Request::Login { email, password }).await?;
            if let LoginResult::TwoFactorRequired = result {
                let code = dev::prompt("2FA code (authenticator app): ")?;
                result = client.call(Request::Login2fa { code }).await?;
            }
            match result {
                LoginResult::SignedIn { name } => println!("Signed in as {name}"),
                LoginResult::TwoFactorRequired => bail!("2FA still required"),
            }
        }
        Cmd::Logout => {
            let () = client.call(Request::Logout).await?;
            println!("Signed out.");
        }
        Cmd::Library { refresh, json } => {
            let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh }).await?;
            if json {
                return print_json(&lib);
            }
            for e in &lib {
                let state = match (&e.installed, e.update_available, e.running) {
                    (_, _, true) => "running",
                    (Some(_), true, _) => "update",
                    (Some(_), false, _) => "installed",
                    (None, _, _) => "",
                };
                println!("{:>6}  {:<10} {}", e.product_id, state, e.name);
            }
        }
        Cmd::Install { game, language, detach } => {
            let e = resolve(&client, &game).await?;
            let events = client.subscribe().await?;
            let id: u64 = client.call(Request::Install { product_id: e.product_id, language }).await?;
            eprintln!("installing {} (job {id})", e.name);
            if !detach {
                follow(events, id).await?;
            }
        }
        Cmd::Import { game, path, language } => {
            let e = resolve(&client, &game).await?;
            let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
            let events = client.subscribe().await?;
            let id: u64 = client.call(Request::Import { product_id: e.product_id, path, language }).await?;
            eprintln!("importing {} (job {id})", e.name);
            follow(events, id).await?;
        }
        Cmd::Uninstall { game, prefix } => {
            let e = resolve(&client, &game).await?;
            let () = client.call(Request::Uninstall { product_id: e.product_id, remove_prefix: prefix }).await?;
            println!("uninstalled {}", e.name);
        }
        Cmd::Play { game, detach } => {
            let e = resolve(&client, &game).await?;
            let events = client.subscribe().await?;
            let id: u64 = client.call(Request::Play { product_id: e.product_id }).await?;
            eprintln!("launching {} (job {id})", e.name);
            if !detach {
                follow(events, id).await?;
            }
        }
        Cmd::Stop { game } => {
            let e = resolve(&client, &game).await?;
            let () = client.call(Request::Stop { product_id: e.product_id }).await?;
        }
        Cmd::Get { game } => {
            let e = resolve(&client, &game).await?;
            let v: serde_json::Value = client.call(Request::SettingsGet { product_id: e.product_id }).await?;
            println!("{}", e.name);
            print_settings(&v);
        }
        Cmd::Set { game, changes, message } => {
            let e = resolve(&client, &game).await?;
            let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
            let rev: Revision = client.call(Request::SettingsSet { product_id: e.product_id, ops, message }).await?;
            println!("{}: settings revision {}", e.name, rev.revision);
        }
        Cmd::History { game } => {
            let e = resolve(&client, &game).await?;
            let revs: Vec<Revision> = client.call(Request::SettingsHistory { product_id: e.product_id }).await?;
            if revs.is_empty() {
                println!("{}: no changes (defaults)", e.name);
            }
            for r in revs {
                println!(
                    "r{:<3} {}  by {:<8} {}  {}",
                    r.revision,
                    r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                    r.source,
                    serde_json::to_string(&r.settings)?,
                    r.message.unwrap_or_default()
                );
            }
        }
        Cmd::Restore { game, revision } => {
            let e = resolve(&client, &game).await?;
            let rev: Revision = client.call(Request::SettingsRestore { product_id: e.product_id, revision }).await?;
            println!("{}: restored r{revision} as revision {}", e.name, rev.revision);
        }
        Cmd::Protons => {
            let list: Vec<String> = client.call(Request::ProtonList).await?;
            for p in list {
                println!("{p}");
            }
        }
        Cmd::Activity { n } => {
            let list: Vec<ActivityEntry> = client.call(Request::Activity { limit: n }).await?;
            for a in list.into_iter().rev() {
                println!("{}  {:<8} {}", a.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"), a.source, a.action);
            }
        }
        Cmd::Watch => {
            let mut events = client.subscribe().await?;
            while let Some(e) = events.recv().await {
                println!("{}", serde_json::to_string(&e)?);
            }
        }
        Cmd::Daemon { action: DaemonAction::Stop } => {
            let () = client.call(Request::Shutdown).await?;
            println!("ubid stopping");
        }
    }
    Ok(())
}
