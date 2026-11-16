//! `ubi`: command-line front end. Everything except `ubi dev …` goes through the `ubid`
//! daemon (started on demand), so the CLI, the GUI and agents share one runtime and one
//! history of changes. Label automated callers with `--source agent` (or `QUIVER_SOURCE`).

mod dev;
mod downloads;
mod exports;
mod mods;
mod info;
mod setup;
mod steamrun;

use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ubi_api::{Account, ActivityEntry, Client, Event, LibraryEntry, LoginResult, Request, Revision, SettingOp, Status};

#[derive(Parser)]
#[command(name = "quiver", about = "Quiver: a third-party launcher for your Ubisoft library")]
struct Cli {
    /// Who is making changes (recorded in settings history and the activity log).
    #[arg(long, global = true, env = "QUIVER_SOURCE", default_value = "cli")]
    source: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon and account status, running games and jobs.
    Status,
    /// Sign in an account (email, password, 2FA). Each sign-in adds an account; the first
    /// becomes the default.
    Login {
        #[arg(long)]
        email: Option<String>,
    },
    /// Sign an account out and forget its stored session (default: the default account).
    Logout {
        /// Username, user id or user-id prefix.
        account: Option<String>,
    },
    /// Signed-in Ubisoft accounts.
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Owned games and their install state.
    Library {
        /// Re-fetch from Ubisoft instead of using the daemon's cache.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install or update a game (queued; follows progress unless --detach).
    Install {
        game: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        detach: bool,
        /// Only show the download and install sizes, free space and languages.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// The download queue (`quiver queue` lists it), or move, remove, settings.
    Queue {
        #[command(subcommand)]
        action: Option<downloads::QueueAction>,
        #[arg(long)]
        json: bool,
    },
    /// Pause a game's download (resumable; it keeps its place in the queue).
    Pause { game: String },
    /// Resume a paused download, or retry a failed one.
    Resume { game: String },
    /// Update an installed game (queued), or with --preview show what would change.
    Update {
        game: String,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check a game's files against its manifest; --repair re-downloads only what is
    /// missing or corrupt. Files the game doesn't ship (mods, extra DLLs) are left alone.
    Verify {
        game: String,
        #[arg(long)]
        repair: bool,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
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
    /// Launch a game (follows it until it exits unless --detach). Without --profile it runs
    /// the game's default profile (`quiver mods <game> default`; vanilla unless set).
    Play {
        game: String,
        /// Mod profile (name or id), or `vanilla`; default: the game's default profile.
        #[arg(long, conflicts_with = "vanilla")]
        profile: Option<String>,
        /// Mod profile by its stable id (what profile .desktop entries and Steam shortcuts use).
        #[arg(long, conflicts_with_all = ["profile", "vanilla"])]
        profile_id: Option<String>,
        /// The game as installed, without mods, whatever the default profile is (same as
        /// `--profile vanilla`).
        #[arg(long)]
        vanilla: bool,
        #[arg(long)]
        detach: bool,
    },
    /// Stop a running game.
    Stop { game: String },
    /// Cancel a game's running install (resume later by installing again) or launch.
    Cancel {
        game: String,
        /// Also delete the partially downloaded files (new installs only).
        #[arg(long)]
        discard: bool,
    },
    /// Show a game's launch settings.
    Get { game: String },
    /// Change launch settings: key=value to set, key= to unset.
    /// Keys: proton, language, account, profile, args, wrappers, env.NAME, dll.NAME.
    /// `account` (username or user id) picks the account the game launches with; `profile`
    /// (a mod profile's name or id, or `vanilla`) the profile Play uses by default.
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
    /// Write a launch context for running a game's ubi-bridge yourself (containers, other
    /// prefixes). Uses the daemon's login, so no extra Ubisoft session is created.
    Context {
        game: String,
        #[arg(short, long)]
        out: std::path::PathBuf,
        /// Windows path for SDK savegames, e.g. C:\...\savegames\<user>\<id>
        #[arg(long)]
        savegame_path: Option<String>,
        /// Host path of the game's install dir (for installed-chunk info).
        #[arg(long)]
        install_dir: Option<std::path::PathBuf>,
        /// Account to issue it for (username or user id); default: the game's launch account.
        #[arg(long)]
        account: Option<String>,
    },
    /// Installed Proton builds.
    Protons,
    /// Play sessions recorded by this launcher.
    Playtime { game: String },
    /// Recent changes and actions, with who made them.
    Activity {
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// Games being played right now (from any launcher, including bridges in containers).
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Print daemon events as they happen.
    Watch,
    /// Control the daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// .desktop entries and Steam shortcuts for games.
    Export {
        #[command(subcommand)]
        action: exports::ExportAction,
    },
    /// Steam users, Ubisoft games installed through Steam (start them through Quiver),
    /// staged changes and restarting Steam.
    Steam {
        #[command(subcommand)]
        action: exports::SteamAction,
    },
    /// Mods and profiles: `quiver mods <game> list | catalog | install … | profile … | plan …`.
    Mods {
        game: String,
        #[command(subcommand)]
        action: mods::ModsAction,
    },
    /// Apply installed built-in fixes (kind = fix) to unmodded launches too: on or off.
    /// Without a value, shows the setting.
    FixesOnVanilla { value: Option<String> },
    /// A game's details: publisher, links, features, install size and languages, add-ons
    /// (owned or not, with store links for the rest).
    Info {
        game: String,
        #[arg(long)]
        json: bool,
        /// Re-read ownership from Ubisoft.
        #[arg(long)]
        refresh: bool,
        /// Also list hidden associated products (internal SKUs, bundles, consumables).
        #[arg(long)]
        all: bool,
    },
    /// A game's Ubisoft progression (play time, stats, challenges, rewards) and your
    /// Ubisoft Connect level and units.
    Progress {
        game: String,
        #[arg(long)]
        json: bool,
        /// Ask Ubisoft again instead of using the daemon's few-minute cache.
        #[arg(long)]
        refresh: bool,
    },
    /// A game's save slots (local and cloud) and backups; `restore` brings a backup back.
    Saves {
        game: String,
        #[arg(long)]
        json: bool,
        /// Only what's on this machine (don't ask the cloud).
        #[arg(long)]
        local: bool,
        #[command(subcommand)]
        action: Option<info::SavesAction>,
    },
    /// First-run setup in the terminal: sign in, where games live, importing existing
    /// installs, Steam, controller, recommended fixes (the app's welcome, same choices).
    Setup {
        /// Only print what was chosen.
        #[arg(long)]
        show: bool,
        #[arg(long)]
        json: bool,
    },
    /// Low-level tools that bypass the daemon.
    Dev {
        #[command(subcommand)]
        cmd: dev::DevCmd,
    },
    /// Run a Steam copy of a Ubisoft game against Quiver instead of Ubisoft Connect. Set it
    /// as the game's Steam launch options: `quiver steam-run --product <id> %command%`
    /// (`quiver steam patch` does it for you; without `--product`, the id comes from the
    /// game's Steam install script or its prefix's registry).
    SteamRun(steamrun::SteamRunArgs),
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Ask ubid to exit.
    Stop,
}

#[derive(Subcommand)]
enum AccountAction {
    /// Signed-in accounts (* marks the default).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Make an account the default (it launches games every account owns).
    Default {
        /// Username, user id or user-id prefix.
        account: String,
    },
    /// Sign in another account (same as `quiver login`).
    Add {
        #[arg(long)]
        email: Option<String>,
    },
    /// Sign an account out (same as `quiver logout`).
    Remove { account: String },
}

pub(crate) fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn print_accounts(accounts: &[Account]) {
    for a in accounts {
        println!("{} {:<20} {}", if a.default { "*" } else { " " }, a.name, a.user_id);
    }
}

async fn login(client: &Client, email: Option<String>) -> Result<()> {
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
    let name = match result {
        LoginResult::SignedIn { name } => name,
        LoginResult::TwoFactorRequired => bail!("2FA still required"),
    };
    let status: Status = client.call(Request::Status).await?;
    let default = status.accounts.iter().find(|a| a.default).map(|a| a.name.as_str());
    match default {
        Some(d) if d == name => println!("Signed in as {name} (default account)"),
        Some(d) => println!("Signed in as {name}; {d} stays the default (quiver account default {name})"),
        None => println!("Signed in as {name}"),
    }
    Ok(())
}

async fn logout(client: &Client, account: Option<String>) -> Result<()> {
    let () = client.call(Request::Logout { account }).await?;
    let status: Status = client.call(Request::Status).await?;
    match status.signed_in {
        Some(d) => println!("Signed out. Default account: {d}"),
        None => println!("Signed out. No accounts are signed in."),
    }
    Ok(())
}

/// Resolve a game argument: product id, or a unique case-insensitive name match.
pub(crate) async fn resolve(client: &Client, game: &str) -> Result<LibraryEntry> {
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

/// Print job events for `job_id` until it finishes. Ctrl-C cancels the job (once; a
/// second Ctrl-C leaves it running in the daemon and exits).
pub(crate) async fn follow(client: &Client, mut events: tokio::sync::mpsc::UnboundedReceiver<Event>, job_id: u64) -> Result<()> {
    let mut last_status = String::new();
    let mut cancelled = false;
    loop {
        let e = tokio::select! {
            e = events.recv() => e,
            _ = tokio::signal::ctrl_c() => {
                if cancelled {
                    eprintln!("\nleft running in ubid (ubi cancel <game> to stop it)");
                    return Ok(());
                }
                cancelled = true;
                eprintln!("\ncancelling… (Ctrl-C again to detach)");
                let _: () = client.call(Request::Cancel { job_id, discard: false }).await?;
                continue;
            }
        };
        let Some(e) = e else { break };
        if matches!(e, Event::Disconnected) {
            eprintln!();
            bail!("quiverd went away while following job {job_id}; jobs don't survive a restart (check `quiver status`)");
        }
        let Event::Job(job) = e else { continue };
        if job.id != job_id {
            continue;
        }
        if job.status == "Paused" && !job.done {
            eprintln!("\npaused (quiver resume <game> to continue)");
            return Ok(());
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
                if job.kind == ubi_api::JobKind::Import {
                    eprintln!("{} other file(s) were not copied:", job.detail.len());
                }
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
    // Restarting Steam only after confirmation; quiverd does it when it's reachable.
    if let Cmd::Steam { action: exports::SteamAction::Restart { yes } } = cli.cmd {
        return exports::steam_restart(yes, &cli.source).await;
    }
    // Connects (and execs Steam's command) on its own; --dry-run needs no daemon.
    if let Cmd::SteamRun(args) = cli.cmd {
        return steamrun::run(&cli.source, args).await;
    }
    let client = Client::connect(&cli.source).await?;
    match cli.cmd {
        Cmd::Dev { .. } | Cmd::SteamRun(_) | Cmd::Steam { action: exports::SteamAction::Restart { .. } } => {
            unreachable!()
        }
        Cmd::Export { action } => exports::export(&client, action).await?,
        Cmd::Steam { action } => exports::steam(&client, action).await?,
        Cmd::Status => {
            let s: Status = client.call(Request::Status).await?;
            println!("quiverd {}", s.version);
            match (&s.signed_in, s.accounts.len()) {
                (Some(n), 0 | 1) => println!("signed in as {n}"),
                (_, 0) if s.awaiting_2fa => println!("waiting for a 2FA code (quiver login)"),
                (_, 0) => println!("not signed in (quiver login)"),
                _ => {
                    println!("accounts (* default):");
                    print_accounts(&s.accounts);
                }
            }
            if s.awaiting_2fa && !s.accounts.is_empty() {
                println!("a sign-in is waiting for its 2FA code (quiver login)");
            }
            println!("default Proton: {}", s.proton_default.as_deref().unwrap_or("?"));
            println!("broker: {}", s.broker);
            if !s.running.is_empty() {
                println!("running: {:?}", s.running);
            }
            for j in s.jobs.iter().filter(|j| !j.done) {
                println!("job {} {:?} {}: {}", j.id, j.kind, j.name, j.status);
            }
        }
        Cmd::Login { email } | Cmd::Account { action: AccountAction::Add { email } } => login(&client, email).await?,
        Cmd::Logout { account } => logout(&client, account).await?,
        Cmd::Account { action: AccountAction::Remove { account } } => logout(&client, Some(account)).await?,
        Cmd::Account { action: AccountAction::List { json } } => {
            let s: Status = client.call(Request::Status).await?;
            if json {
                return print_json(&s.accounts);
            }
            if s.accounts.is_empty() {
                println!("no accounts signed in (quiver login)");
            }
            print_accounts(&s.accounts);
        }
        Cmd::Account { action: AccountAction::Default { account } } => {
            let () = client.call(Request::AccountDefault { account }).await?;
            let s: Status = client.call(Request::Status).await?;
            println!("default account: {}", s.signed_in.as_deref().unwrap_or("?"));
        }
        Cmd::Library { refresh, json } => {
            let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh }).await?;
            if json {
                return print_json(&lib);
            }
            let accounts = client.call::<Status>(Request::Status).await?.accounts;
            let owners = |e: &LibraryEntry| {
                e.owned_by
                    .iter()
                    .map(|id| accounts.iter().find(|a| &a.user_id == id).map_or(id.as_str(), |a| a.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let width = lib.iter().map(|e| owners(e).chars().count()).max().unwrap_or(0).max("owned by".len());
            if accounts.len() > 1 {
                println!("{:>6}  {:<10} {:<width$}  name", "id", "state", "owned by");
            }
            for e in &lib {
                let state = match (&e.installed, e.update_available, e.running) {
                    (_, _, true) => "running",
                    (Some(_), true, _) => "update",
                    (Some(_), false, _) => "installed",
                    (None, _, _) => "",
                };
                if accounts.len() > 1 {
                    println!("{:>6}  {:<10} {:<width$}  {}", e.product_id, state, owners(e), e.name);
                } else {
                    println!("{:>6}  {:<10} {}", e.product_id, state, e.name);
                }
            }
        }
        Cmd::Install { game, language, dry_run: true, json, .. } => downloads::dry_run(&client, &game, language, json).await?,
        Cmd::Queue { action, json } => downloads::queue(&client, action, json).await?,
        Cmd::Setup { show, json } => {
            if show || json {
                setup::show(&client, json).await?
            } else {
                setup::run(&client).await?
            }
        }
        Cmd::Pause { game } => downloads::pause(&client, &game).await?,
        Cmd::Resume { game } => downloads::resume(&client, &game).await?,
        Cmd::Update { game, preview, detach, json } => downloads::update(&client, &game, preview, detach, json).await?,
        Cmd::Verify { game, repair, detach, json } => downloads::verify(&client, &game, repair, detach, json).await?,
        Cmd::Install { game, language, detach, .. } => {
            let e = resolve(&client, &game).await?;
            let events = client.subscribe().await?;
            let id: u64 = client.call(Request::Install { product_id: e.product_id, language }).await?;
            eprintln!("installing {} (job {id})", e.name);
            if !detach {
                follow(&client, events, id).await?;
            }
        }
        Cmd::Import { game, path, language } => {
            let e = resolve(&client, &game).await?;
            let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
            let events = client.subscribe().await?;
            let id: u64 = client.call(Request::Import { product_id: e.product_id, path, language }).await?;
            eprintln!("importing {} (job {id})", e.name);
            follow(&client, events, id).await?;
        }
        Cmd::Uninstall { game, prefix } => {
            let e = resolve(&client, &game).await?;
            let () = client.call(Request::Uninstall { product_id: e.product_id, remove_prefix: prefix }).await?;
            println!("uninstalled {}", e.name);
        }
        Cmd::Play { game, profile, profile_id, vanilla, detach } => {
            let e = resolve(&client, &game).await?;
            let events = client.subscribe().await?;
            // `None`: the daemon runs the game's default profile; `vanilla` forces vanilla.
            let profile = profile.or(profile_id).or(vanilla.then(|| ubi_api::mods::VANILLA.to_owned()));
            let default = match &profile {
                None => client
                    .call::<ubi_api::mods::ModsState>(Request::Mods(ubi_api::ModsRequest::List { product_id: e.product_id }))
                    .await
                    .ok()
                    .map(|s| mods::default_label(&s)),
                Some(_) => None,
            };
            let id: u64 = client.call(Request::Play { product_id: e.product_id, profile: profile.clone() }).await?;
            match (profile, default) {
                (Some(p), _) => eprintln!("launching {} with profile {p} (job {id})", e.name),
                (None, Some(d)) => eprintln!("launching {} with its default, {d} (job {id})", e.name),
                (None, None) => eprintln!("launching {} (job {id})", e.name),
            }
            if !detach {
                follow(&client, events, id).await?;
            }
        }
        Cmd::Cancel { game, discard } => {
            let e = resolve(&client, &game).await?;
            let status: Status = client.call(Request::Status).await?;
            let job = status
                .jobs
                .iter()
                .filter(|j| j.product_id == e.product_id && !j.done)
                .max_by_key(|j| j.id)
                .with_context(|| format!("nothing running for {}", e.name))?;
            let () = client.call(Request::Cancel { job_id: job.id, discard }).await?;
            println!("cancelling {:?} job {} for {}", job.kind, job.id, e.name);
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
            if let Some(id) = v["settings"]["account"].as_str() {
                let s: Status = client.call(Request::Status).await?;
                match s.accounts.iter().find(|a| a.user_id == id) {
                    Some(a) => println!("  (launches as {})", a.name),
                    None => println!("  (launch account {id} is not signed in)"),
                }
            }
            if v["settings"]["profile"].is_string()
                && let Ok(s) = client.call::<ubi_api::mods::ModsState>(Request::Mods(ubi_api::ModsRequest::List { product_id: e.product_id })).await
            {
                println!("  (Play uses {})", mods::default_label(&s));
            }
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
        Cmd::Playtime { game } => {
            let e = resolve(&client, &game).await?;
            let p: ubi_api::Playtime = client.call(Request::Playtime { product_id: e.product_id }).await?;
            match client.call::<serde_json::Value>(Request::RemotePlaytime { product_id: e.product_id }).await {
                Ok(r) => println!("Ubisoft says: {} (raw: {r})", r["seconds"].as_u64().map(|s| format!("{}h{:02}m", s / 3600, s % 3600 / 60)).unwrap_or("?".into())),
                Err(err) => println!("Ubisoft play time unavailable: {err:#}"),
            }
            println!("{}: {}h{:02}m over {} session(s)", e.name, p.total_seconds / 3600, p.total_seconds % 3600 / 60, p.sessions.len());
            for s in p.sessions.iter().rev().take(10) {
                println!(
                    "  {}  {:>4} min  (settings r{})",
                    s.started.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                    s.seconds / 60,
                    s.settings_revision
                );
            }
        }
        Cmd::Context { game, out, savegame_path, install_dir, account } => {
            let e = resolve(&client, &game).await?;
            let ctx: serde_json::Value = client
                .call(Request::LaunchContext { product_id: e.product_id, savegame_path, install_dir, account })
                .await?;
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&out)?;
                f.write_all(serde_json::to_string_pretty(&ctx)?.as_bytes())?;
            }
            eprintln!("wrote {} (broker {}, ticket valid for a few hours)", out.display(), ctx["broker"]);
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
        Cmd::Sessions { json } => {
            let list: Vec<ubi_api::ActiveSession> = client.call(Request::Sessions).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("nothing running");
            } else {
                for s in list {
                    let mins = (chrono::Utc::now() - s.started).num_minutes();
                    println!(
                        "{:<6} {:>4} min  via {:<16} {}",
                        s.product_id,
                        mins,
                        s.holders.join("+"),
                        if s.reported { "reported to Ubisoft" } else { "not reported" }
                    );
                }
            }
        }
        Cmd::Watch => {
            let mut events = client.subscribe().await?;
            while let Some(e) = events.recv().await {
                println!("{}", serde_json::to_string(&e)?);
            }
        }
        Cmd::Mods { game, action } => mods::run(&client, &cli.source, &game, action).await?,
        Cmd::FixesOnVanilla { value } => mods::fixes_on_vanilla(&client, value).await?,
        Cmd::Info { game, json, refresh, all } => info::info(&client, &game, json, refresh, all).await?,
        Cmd::Progress { game, json, refresh } => info::progress(&client, &game, json, refresh).await?,
        Cmd::Saves { game, json, local, action: None } => info::saves(&client, &game, json, local).await?,
        Cmd::Saves { game, action: Some(info::SavesAction::Sync { profile }), .. } => info::sync(&client, &game, profile).await?,
        Cmd::Saves { game, action: Some(info::SavesAction::Restore { backup }), .. } => {
            info::restore(&client, &cli.source, &game, backup).await?
        }
        Cmd::Daemon { action: DaemonAction::Stop } => {
            let () = client.call(Request::Shutdown).await?;
            println!("quiverd stopping");
        }
    }
    Ok(())
}
