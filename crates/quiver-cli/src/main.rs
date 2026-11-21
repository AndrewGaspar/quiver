//! `quiver`: command-line front end. Everything except `quiver dev …` goes through the `quiverd`
//! daemon (started on demand), so the CLI, the GUI and agents share one runtime and one
//! history of changes. Label automated callers with `--source agent` (or `QUIVER_SOURCE`).
//! `--machine NAME` runs a command against a paired machine's quiverd instead
//! (design/REMOTE.md §4; `quiver remote add` pairs one).

mod achievements;
mod daemon;
mod dev;
mod downloads;
mod epic;
mod epicgame;
mod exports;
mod mods;
mod omarchy;
mod perf;
mod profiles;
mod remote;
mod options;
mod info;
mod lab;
mod savestores;
mod setup;
mod sight;
mod steamrun;
mod tweaks;

use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quiver_api::{Account, ActivityEntry, Client, Event, GameRef, LibraryEntry, LoginResult, Request, Revision, SettingOp, Status, StoreId};

#[derive(Parser)]
#[command(name = "quiver", about = "Quiver: a third-party launcher for your Ubisoft library")]
struct Cli {
    /// Who is making changes (recorded in settings history and the activity log).
    #[arg(long, global = true, env = "QUIVER_SOURCE", default_value = "cli")]
    source: String,
    /// Run the command on a paired machine (its name or id; `quiver remote peers`) instead
    /// of this one. Its quiverd records it as `remote:<this machine's name>`.
    #[arg(long, global = true, env = "QUIVER_MACHINE")]
    machine: Option<String>,
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
    /// The stores Quiver reads games from, whose account each follows, and what each does.
    Stores {
        #[arg(long)]
        json: bool,
    },
    /// Owned games and their install state, from every store (Ubisoft, Steam's installed
    /// games, Epic), with each game's ref (`3539`, `steam:620`, `epic:<app>`).
    #[command(visible_alias = "list")]
    Library {
        /// Re-fetch from Ubisoft instead of using the daemon's cache.
        #[arg(long)]
        refresh: bool,
        /// Only one store's games (`ubisoft`, `steam`, `epic`).
        #[arg(long, value_parser = parse_store)]
        store: Option<StoreId>,
        /// Also list owned items the library leaves out (DLC whose game you don't own,
        /// assets, extras, items Epic's catalog doesn't describe), with the reason.
        #[arg(long)]
        all: bool,
        /// Only games with an update waiting: Quiver's installs behind their store's
        /// build (Ubisoft, Epic), and games Steam has an update for. With --refresh,
        /// the stores are asked first.
        #[arg(long)]
        updates: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install or update a game (queued; follows progress unless --detach).
    Install {
        game: String,
        #[arg(long)]
        language: Option<String>,
        /// When the install finishes, add it to Steam (staged while Steam runs).
        #[arg(long)]
        add_to_steam: bool,
        /// When the install finishes, write a .desktop entry.
        #[arg(long)]
        desktop_entry: bool,
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
        /// Don't start quiverd when it isn't running: fail with "quiverd is not running"
        /// instead (for status bars that poll the queue, such as contrib/omarchy).
        #[arg(long)]
        no_start: bool,
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
    /// Keys: proton, language, account, profile, steam_profile, art, achievements, args,
    /// wrappers, env.NAME, dll.NAME.
    /// `account` (username or user id) picks the account the game launches with; `profile`
    /// (a mod profile's name or id, or `vanilla`) the profile Play uses by default;
    /// `steam_profile` the one a launch from Steam runs (`quiver steam-run`; for a Steam game
    /// `profile` sets it too); `art`
    /// (`steam` or `ubisoft`) whose art the app shows first for a game with a Steam copy;
    /// `achievements` (`on`, `off` or `dry-run`) whether earned achievements are reported.
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
    /// Write a launch context for running a game's quiver-bridge yourself (containers, other
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
    /// What Ubisoft's ownership service says about a product (or every product), raw: why
    /// a game is or isn't in the library. Read-only; uses quiverd's session.
    Ownership {
        /// Product id (default: every product on the account).
        product: Option<u32>,
        /// Account (username or user id); default: the game's launch account.
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Games being played right now (from any launcher, including bridges in containers).
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Print daemon events as they happen.
    Watch,
    /// Performance metrics: `quiver perf watch|catalog|sessions|history`.
    Perf {
        #[command(subcommand)]
        action: perf::PerfAction,
    },
    /// The pinned performance HUD, in every game, hidden until Shift+F3:
    /// `quiver hud toggle|next|status|show|hide|layout|enable|disable|set|keybinds`.
    Hud {
        #[command(subcommand)]
        action: perf::HudAction,
    },
    /// The in-game panel: `quiver sight toggle|open SECTION|close|status|hyprpad|keybinds|pad-chord`.
    Sight {
        #[command(subcommand)]
        action: sight::SightAction,
    },
    /// Remote play: `quiver remote enable|disable|status|pair|add|peers|revoke|rename|wake|discover`.
    Remote {
        #[command(subcommand)]
        action: remote::RemoteAction,
    },
    /// The daemon: status, holding off its restart onto a new build, stopping it.
    Daemon {
        #[command(subcommand)]
        action: daemon::DaemonAction,
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
    /// Quiver Tweaks: frame rate, sync, latency, DLSS and frame generation overrides for any
    /// game Quiver launches, globally, per game or per profile:
    /// `quiver tweaks [list|get|set|reset|preset|prefer|explain|plan|status|sessions|history|restore|caps]`.
    Tweaks {
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<tweaks::TweaksAction>,
    },
    /// A game's profiles, each a named setup of its mods and game settings (settings files,
    /// Options, Tweaks, launch overrides, save store): `quiver profiles <game> [list | show P |
    /// create NAME [--from P|--blank] | set P k=v | settings [P own|shared] | default P | …]`.
    Profiles {
        game: String,
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<profiles::ProfilesAction>,
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
    /// Ubisoft Connect level and units; for an Epic game, Epic's play time and achievements.
    Progress {
        game: String,
        #[arg(long)]
        json: bool,
        /// Ask Ubisoft again instead of using the daemon's few-minute cache.
        #[arg(long)]
        refresh: bool,
    },
    /// A game's achievements with this account's unlock status, and Quiver's report journal
    /// (achievements games earned through Quiver, reported to Ubisoft unless the game's
    /// `achievements` setting is `off` or `dry-run`). `report <id>` sends one again.
    Achievements {
        game: String,
        #[arg(long)]
        json: bool,
        /// Ask Ubisoft again instead of using the cached unlock set.
        #[arg(long)]
        refresh: bool,
        #[command(subcommand)]
        action: Option<achievements::AchAction>,
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
    /// Epic Games Store: `quiver epic login|logout|accounts|default`, and an Epic game's
    /// cloud saves and play time (`quiver epic saves|playtime`).
    Epic {
        #[command(subcommand)]
        action: epic::EpicAction,
    },
    /// The Omarchy bar widget for the download queue (`quiver.downloads`): install-plugin,
    /// uninstall-plugin, status. Never run by the installer; only when you ask.
    Omarchy {
        #[command(subcommand)]
        action: omarchy::OmarchyAction,
    },
    /// Lab instances: disposable headless copies of an installed game in a container, with
    /// your real licence from quiverd, for agents developing mods (design/LAB.md):
    /// `quiver lab create|up|launch|wait|shot|key|mouse|set|bench|brief|…`.
    Lab {
        /// Run the command in the background and print a job id (`quiver lab job wait <id>`).
        #[arg(long, global = true)]
        bg: bool,
        #[command(subcommand)]
        action: lab::LabAction,
    },
    /// Run a Steam copy of a Ubisoft game against Quiver instead of Ubisoft Connect. Set it
    /// as the game's Steam launch options: `quiver steam-run --product <id> %command%`
    /// (`quiver steam patch` does it for you; without `--product`, the id comes from the
    /// game's Steam install script or its prefix's registry).
    SteamRun(steamrun::SteamRunArgs),
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
    /// Every account on this machine and its paired machines (Ubisoft, Epic): where each
    /// is signed in, and which machine is playing on it.
    Machines {
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn print_presence(p: &quiver_api::accounts::Presence) {
    if p.accounts.is_empty() {
        println!("no accounts signed in on any machine (quiver login, quiver epic login)");
    }
    for a in &p.accounts {
        println!("{} {} ({})", a.store.label(), a.name, a.account_id);
        for m in &a.machines {
            let here = if m.here { " (this machine)" } else { "" };
            let state = match (&m.playing, m.state) {
                (Some(g), _) => format!("playing {} since {}", g.name, g.started.with_timezone(&chrono::Local).format("%H:%M")),
                (None, quiver_api::stores::AccountState::SignedIn) => "signed in".into(),
                (None, _) => "needs signing in again".into(),
            };
            println!("    {}{here}: {state}{}", m.machine, if m.default { ", default" } else { "" });
        }
    }
    for m in p.machines.iter().filter(|m| !m.reached) {
        println!("({} not reached: {})", m.machine, m.error.as_deref().unwrap_or("?"));
    }
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
    let mut result: LoginResult = client.call(Request::Login { email, password: password.into() }).await?;
    if let LoginResult::TwoFactorRequired = result {
        let code = dev::prompt("2FA code (authenticator app): ")?;
        result = client.call(Request::Login2fa { code: code.into() }).await?;
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

/// A Ubisoft game a command resolved ([`resolve`]): the library entry and its product id.
pub(crate) struct UbiEntry {
    pub product_id: u32,
    pub entry: LibraryEntry,
}

impl std::ops::Deref for UbiEntry {
    type Target = LibraryEntry;
    fn deref(&self) -> &LibraryEntry {
        &self.entry
    }
}

/// Resolve a game argument for a Ubisoft-only command: a product id (`3539`,
/// `ubisoft:3539`), or a unique case-insensitive name match among Ubisoft games. Another
/// store's ref (`steam:620`) is refused with what to do instead.
pub(crate) async fn resolve(client: &Client, game: &str) -> Result<UbiEntry> {
    if let Some(r) = GameRef::parse_ref(game).filter(|r| !r.is_ubisoft()) {
        bail!("{r} is a {} game; this command is for Ubisoft games ({} handles it, or try `quiver play {r}`)", r.store.label(), r.store.label());
    }
    let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false, all_stores: false }).await?;
    let entry = pick(lib, game)?;
    let product_id = entry.ubisoft_id().context("not a Ubisoft game")?;
    Ok(UbiEntry { product_id, entry })
}

/// Another store's launch settings (a Steam game's applied by `quiver steam-run`; an Epic
/// game's by Quiver's own launch): `Some` for a Steam or Epic ref, or a name no Ubisoft game
/// matches but a Steam or Epic game does; `None` means a Ubisoft game (resolve it as before,
/// so Ubisoft names resolve exactly as they always did).
pub(crate) async fn resolve_steam_settings(client: &Client, game: &str) -> Result<Option<(String, GameRef)>> {
    if let Some(r) = GameRef::parse_ref(game) {
        if r.is_ubisoft() {
            return Ok(None);
        }
        if !matches!(r.store, quiver_api::StoreId::Steam | quiver_api::StoreId::Epic) {
            bail!("{r}: {} games have no Quiver launch settings yet", r.store.label());
        }
        let name = resolve_any(client, game).await.map(|e| e.name).unwrap_or_else(|_| r.to_string());
        return Ok(Some((name, r)));
    }
    if resolve(client, game).await.is_ok() {
        return Ok(None);
    }
    match resolve_any(client, game).await {
        Ok(e) if matches!(e.game.store, quiver_api::StoreId::Steam | quiver_api::StoreId::Epic) => Ok(Some((e.name, e.game))),
        _ => Ok(None),
    }
}

/// Resolve a game argument across every store: a ref (`3539`, `ubi:3539`, `steam:620`)
/// or a unique case-insensitive name match. A name in several stores lists the matches
/// with their refs; a full ref picks one.
pub(crate) async fn resolve_any(client: &Client, game: &str) -> Result<LibraryEntry> {
    let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false, all_stores: true }).await?;
    pick(lib, game)
}

/// A game argument for installing, updating, verifying, importing or uninstalling: a
/// Ubisoft game, or an Epic game Quiver installs (`epic:<app>` or its name).
pub(crate) async fn resolve_installable(client: &Client, game: &str) -> Result<LibraryEntry> {
    let e = resolve_any(client, game).await?;
    match e.store() {
        StoreId::Ubisoft | StoreId::Epic => Ok(e),
        s => bail!("{} is a {} game; {} installs it", game_label(&e.game), s.label(), s.label()),
    }
}

/// [`resolve_any`] over a library already fetched.
fn pick(lib: Vec<LibraryEntry>, game: &str) -> Result<LibraryEntry> {
    if let Some(r) = GameRef::parse_ref(game) {
        let what = if r.is_ubisoft() { format!("product {}", r.id) } else { r.to_string() };
        // An Epic copy folded into its Ubisoft entry is found by its own ref too.
        return lib.into_iter().find(|e| e.is_or_has(&r)).with_context(|| format!("{what} is not in your library"));
    }
    let norm = |s: &str| s.to_lowercase().replace(['®', '™', '’', '\''], "");
    let needle = norm(game);
    let mut matches: Vec<LibraryEntry> = lib.into_iter().filter(|e| norm(&e.name).contains(&needle)).collect();
    // An exact name wins over names that merely contain it ("Portal" vs "Portal 2").
    if matches.len() > 1 && matches.iter().filter(|e| norm(&e.name) == needle).count() == 1 {
        matches.retain(|e| norm(&e.name) == needle);
    }
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no game matches '{game}'"),
        _ => {
            let stores: std::collections::BTreeSet<_> = matches.iter().map(|e| e.store()).collect();
            let hint = if stores.len() > 1 { "; they are in different stores, so name one by its ref" } else { "" };
            bail!(
                "'{game}' matches several games: {}{hint}",
                matches.iter().map(|e| format!("{} ({})", e.name, game_label(&e.game))).collect::<Vec<_>>().join(", ")
            )
        }
    }
}

/// `--store` values.
fn parse_store(s: &str) -> std::result::Result<StoreId, String> {
    StoreId::parse(s).ok_or_else(|| format!("unknown store '{s}' (ubisoft, steam, epic)"))
}

/// `quiver library --all`: what the library leaves out, and why.
fn print_hidden(hidden: &[quiver_api::epic::HiddenItem]) {
    if hidden.is_empty() {
        return;
    }
    println!("
Not in the library ({}):", hidden.len());
    let w = hidden.iter().map(|h| h.app_name.len() + 5).max().unwrap_or(0);
    for h in hidden {
        let name = match (&h.title, h.sandbox.as_str()) {
            (Some(t), s) if !s.is_empty() && s != t => format!("{t} [{s}]"),
            (Some(t), _) => t.clone(),
            (None, "") => "(no name)".into(),
            (None, s) => format!("[{s}]"),
        };
        println!("{:>w$}  {name}: {}", format!("epic:{}", h.app_name), h.reason);
    }
}

/// The list's note for an Epic game, or a game also bought on Epic.
fn store_note(e: &LibraryEntry) -> String {
    use quiver_api::stores::Relation;
    if e.store() == StoreId::Epic {
        return match e.records.iter().find(|r| r.relation == Relation::Linked) {
            Some(r) if r.game.is_ubisoft() => format!("  (Epic; a Ubisoft Connect game, not on a signed-in Ubisoft account: {})", game_label(&r.game)),
            _ if e.installed.is_some() => "  (Epic)".into(),
            _ => "  (Epic, not installed)".into(),
        };
    }
    let copies: Vec<String> = e.records.iter().filter(|r| r.relation == Relation::CopyOf).map(|r| r.game.to_string()).collect();
    if copies.is_empty() { String::new() } else { format!("  (also bought as {})", copies.join(", ")) }
}

/// The list's note for another store's game: "(Steam · 12.4 GB · 41 h played · 12/50 achievements)".
fn client_note(c: &quiver_api::stores::ClientInstall) -> String {
    let mut parts = vec![format!("{:.1} GB", c.size_bytes as f64 / 1e9)];
    // Steam's update of it, as its manifest says ("Steam is updating it · 42% of 3.1 GB").
    if let Some(u) = &c.update {
        parts.push(u.describe("Steam"));
    }
    if let Some(p) = c.playtime.filter(|p| p.total_seconds > 0) {
        parts.push(format!("{} h played", p.total_seconds / 3600));
    }
    if let Some(a) = c.achievements.filter(|a| a.total > 0) {
        parts.push(format!("{}/{} achievements", a.unlocked, a.total));
    }
    format!("  ({})", parts.join(" · "))
}

/// How the CLI prints a game's ref: the bare product id for Ubisoft (as always), the full
/// ref for other stores.
pub(crate) fn game_label(g: &GameRef) -> String {
    match g.as_ubisoft() {
        Some(id) => id.to_string(),
        None => g.to_string(),
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
                    eprintln!("\nleft running in quiverd (quiver cancel <game> to stop it)");
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
                if job.kind == quiver_api::JobKind::Import {
                    eprintln!("{} other file(s) were not copied:", job.detail.len());
                }
                for d in &job.detail {
                    eprintln!("  {d}");
                }
            }
            return Ok(());
        }
    }
    bail!("lost connection to quiverd")
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
    if let Some(m) = &cli.machine {
        let here_only = match &cli.cmd {
            Cmd::Dev { .. } => Some("dev"),
            Cmd::SteamRun(_) => Some("steam-run"),
            Cmd::Lab { .. } => Some("lab"),
            Cmd::Daemon { .. } => Some("daemon"),
            Cmd::Omarchy { .. } => Some("omarchy"),
            Cmd::Steam { action: exports::SteamAction::Restart { .. } } => Some("steam restart"),
            _ => None,
        };
        if let Some(cmd) = here_only {
            bail!("`quiver {cmd}` works on this machine only; run it on {m} itself (over SSH)");
        }
    }
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
    // Mostly no daemon at all; connects on its own where it needs one (never starts it).
    if let Cmd::Lab { action, bg } = cli.cmd {
        return lab::run(&cli.source, action, bg).await;
    }
    // Connects on its own (and only `hold` starts the daemon).
    if let Cmd::Daemon { action } = cli.cmd {
        return daemon::run(&cli.source, action).await;
    }
    // The Omarchy bar plugin: files and Omarchy's own commands, no daemon.
    if let Cmd::Omarchy { action } = cli.cmd {
        return omarchy::run(action);
    }
    // A status bar polling the queue must not bring back a quiverd someone stopped.
    let client = if let Some(m) = &cli.machine {
        Client::machine_named(m, &cli.source).await?
    } else if matches!(cli.cmd, Cmd::Queue { no_start: true, .. }) {
        Client::connect_running(&cli.source).await?
    } else {
        Client::connect(&cli.source).await?
    };
    match cli.cmd {
        Cmd::Dev { .. }
        | Cmd::SteamRun(_)
        | Cmd::Steam { action: exports::SteamAction::Restart { .. } }
        | Cmd::Daemon { .. }
        | Cmd::Lab { .. }
        | Cmd::Omarchy { .. } => {
            unreachable!()
        }
        Cmd::Export { action } => exports::export(&client, action).await?,
        Cmd::Remote { action } => remote::run(&client, action).await?,
        Cmd::Steam { action } => exports::steam(&client, action).await?,
        Cmd::Epic { action } => epic::run(&client, action).await?,
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
        Cmd::Account { action: AccountAction::Machines { json } } => {
            let p: quiver_api::accounts::Presence = client.call(Request::Accounts(quiver_api::accounts::AccountsRequest::Presence { wait_ms: None })).await?;
            if json {
                return print_json(&p);
            }
            print_presence(&p);
        }
        Cmd::Account { action: AccountAction::Default { account } } => {
            let () = client.call(Request::AccountDefault { account }).await?;
            let s: Status = client.call(Request::Status).await?;
            println!("default account: {}", s.signed_in.as_deref().unwrap_or("?"));
        }
        Cmd::Stores { json } => {
            let list: Vec<quiver_api::stores::StoreInfo> = client.call(Request::Stores).await?;
            if json {
                return print_json(&list);
            }
            for s in list {
                use quiver_api::stores::AccountState as A;
                let account = match (s.account, &s.account_name) {
                    (A::SignedIn, Some(n)) => format!("signed in as {n}"),
                    (A::ClientManaged, Some(n)) => format!("follows the {} client ({n})", s.store.label()),
                    (A::ClientManaged, None) => format!("follows the {} client", s.store.label()),
                    (A::SignedInElsewhere, _) => "signed in elsewhere".into(),
                    (A::NeedsSignIn, _) | (A::SignedIn, None) => "not signed in".into(),
                };
                let note = s.note.map(|n| format!(" ({n})")).unwrap_or_default();
                println!("{:<8} {:>4} game(s)  {account}{note}", s.store.label(), s.games);
                let c = s.capabilities;
                let caps = [("library", c.library), ("install", c.install), ("play", c.launch), ("settings", c.settings), ("mods", c.mods), ("cloud saves", c.cloud_saves), ("achievements", c.achievements), ("play time", c.playtime)];
                let shown: Vec<String> = caps
                    .iter()
                    .map(|(k, v)| {
                        let how = match v {
                            quiver_api::stores::Support::Quiver => "",
                            quiver_api::stores::Support::Delegated => " (via the client)",
                            quiver_api::stores::Support::ReadOnly => " (read-only)",
                            quiver_api::stores::Support::No => " (no)",
                        };
                        format!("{k}{how}")
                    })
                    .collect();
                println!("         {}", shown.join(", "));
            }
        }
        Cmd::Library { refresh, store, all, updates, json } => {
            let mut lib: Vec<LibraryEntry> = client.call(Request::Library { refresh, all_stores: true }).await?;
            if let Some(s) = store {
                lib.retain(|e| e.stores().contains(&s));
            }
            if updates {
                lib.retain(LibraryEntry::has_update);
                if lib.is_empty() && !json {
                    println!("no updates waiting{}", if refresh { "" } else { " (--refresh asks the stores)" });
                    return Ok(());
                }
            }
            let hidden: Vec<quiver_api::epic::HiddenItem> = if all && store.is_none_or(|s| s == StoreId::Epic) {
                client.call(Request::Epic(quiver_api::epic::EpicRequest::Hidden)).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            if json {
                return if all { print_json(&serde_json::json!({"library": lib, "hidden": hidden})) } else { print_json(&lib) };
            }
            let accounts = client.call::<Status>(Request::Status).await?.accounts;
            // Epic owners are Epic account ids (an older daemon has no Epic accounts).
            let epic: Vec<quiver_api::epic::EpicAccount> = if lib.iter().any(|e| e.store() == StoreId::Epic) {
                client.call(Request::Epic(quiver_api::epic::EpicRequest::Accounts)).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            let owners = |e: &LibraryEntry| {
                e.owned_by
                    .iter()
                    .map(|id| {
                        accounts.iter().find(|a| &a.user_id == id).map(|a| a.name.as_str()).or(epic.iter().find(|a| &a.account_id == id).map(|a| a.display_name.as_str())).unwrap_or(id.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let width = lib.iter().map(|e| owners(e).chars().count()).max().unwrap_or(0).max("owned by".len());
            // Ubisoft ids keep their old column; other stores' refs widen it.
            let idw = lib.iter().map(|e| game_label(&e.game).len()).max().unwrap_or(0).max(6);
            if accounts.len() > 1 {
                println!("{:>idw$}  {:<10} {:<width$}  name", "id", "state", "owned by");
            }
            for e in &lib {
                let state = match (&e.installed, e.update_available, e.running, &e.steam, &e.client) {
                    (_, _, true, _, _) => "running",
                    (Some(_), true, _, _, _) => "update",
                    (Some(_), false, _, _, _) => "installed",
                    (None, _, _, Some(_), _) => "steam",
                    (None, _, _, None, Some(c)) if c.update_pending => "update",
                    (None, _, _, None, Some(c)) if !c.complete => "installing",
                    (None, _, _, None, Some(_)) => e.store().as_str(),
                    (None, _, _, None, None) => "",
                };
                let note = match (&e.steam, &e.client) {
                    (Some(g), _) if e.owned_by.is_empty() => format!("  (Steam app {}; not on a signed-in Ubisoft account)", g.app_id),
                    (Some(g), _) if !e.installable => format!("  (owned on Steam, app {})", g.app_id),
                    (Some(g), _) if e.installed.is_none() => format!("  (Steam app {})", g.app_id),
                    (None, Some(c)) => client_note(c),
                    _ => store_note(e),
                };
                let id = game_label(&e.game);
                if accounts.len() > 1 {
                    println!("{id:>idw$}  {:<10} {:<width$}  {}{note}", state, owners(e), e.name);
                } else {
                    println!("{id:>idw$}  {:<10} {}{note}", state, e.name);
                }
            }
            if all {
                print_hidden(&hidden);
            }
        }
        Cmd::Install { game, language, dry_run: true, json, .. } => downloads::dry_run(&client, &game, language, json).await?,
        Cmd::Queue { action, json, .. } => downloads::queue(&client, action, json).await?,
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
        Cmd::Install { game, language, detach, add_to_steam, desktop_entry, .. } => {
            let e = resolve_installable(&client, &game).await?;
            let events = client.subscribe().await?;
            let after = (add_to_steam || desktop_entry).then_some(quiver_api::downloads::AfterInstall { steam: add_to_steam, desktop: desktop_entry });
            let id: u64 = client.call(Request::Install { game: e.game.into(), language, after }).await?;
            eprintln!("installing {} (job {id})", e.name);
            if !detach {
                follow(&client, events, id).await?;
            }
        }
        Cmd::Import { game, path, language } => {
            let e = resolve_installable(&client, &game).await?;
            let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
            let events = client.subscribe().await?;
            let id: u64 = client.call(Request::Import { game: e.game.into(), path, language }).await?;
            eprintln!("importing {} (job {id})", e.name);
            follow(&client, events, id).await?;
        }
        Cmd::Uninstall { game, prefix } => {
            let e = resolve_installable(&client, &game).await?;
            let v: serde_json::Value = client.call(Request::Uninstall { game: e.game.into(), remove_prefix: prefix }).await?;
            println!("uninstalled {}", e.name);
            // Daemons from before the reply had fields answered null.
            if let Ok(r) = serde_json::from_value::<quiver_api::UninstallResult>(v) {
                for x in &r.exports_removed {
                    println!("  removed {x}");
                }
            }
        }
        Cmd::Play { game, profile, profile_id, vanilla, detach } => {
            let any = resolve_any(&client, &game).await?;
            if let (None, Some(c)) = (any.ubisoft_id(), &any.client) {
                if profile.is_some() || profile_id.is_some() {
                    bail!("{} games play as {} starts them; mod profiles don't apply", any.store().label(), any.store().label());
                }
                let _: u64 = client.call(Request::Play { game: any.game.into(), profile: None }).await?;
                eprintln!("asked {} to start {} ({})", any.store().label(), any.name, c.launch_url.as_deref().unwrap_or("its client"));
                return Ok(());
            }
            // An Epic game Quiver installed plays through Quiver too (mods and profiles apply).
            let e = if any.store() == StoreId::Epic { any } else { resolve(&client, &game_label(&any.game)).await?.entry };
            let events = client.subscribe().await?;
            // `None`: the daemon runs the game's default profile; `vanilla` forces vanilla.
            let profile = profile.or(profile_id).or(vanilla.then(|| quiver_api::mods::VANILLA.to_owned()));
            let default = match &profile {
                None => client
                    .call::<quiver_api::mods::ModsState>(Request::Mods(quiver_api::ModsRequest::List { game: e.game.into() }))
                    .await
                    .ok()
                    .map(|s| mods::default_label(&s)),
                Some(_) => None,
            };
            let id: u64 = client.call(Request::Play { game: e.game.into(), profile: profile.clone() }).await?;
            if let Some(g) = e.steam.as_ref().filter(|_| e.steam_only()) {
                let how = if g.state.uses_quiver() { "it starts through Quiver" } else { "it starts through Ubisoft Connect; `quiver steam patch` switches it" };
                eprintln!("asked Steam to start {} (steam://rungameid/{}; {how})", e.name, g.app_id);
                return Ok(());
            }
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
            let e = resolve_installable(&client, &game).await?;
            let status: Status = client.call(Request::Status).await?;
            let job = status
                .jobs
                .iter()
                .filter(|j| j.game == e.game && !j.done)
                .max_by_key(|j| j.id)
                .with_context(|| format!("nothing running for {}", e.name))?;
            let () = client.call(Request::Cancel { job_id: job.id, discard }).await?;
            println!("cancelling {:?} job {} for {}", job.kind, job.id, e.name);
        }
        Cmd::Stop { game } => {
            let e = resolve_installable(&client, &game).await?;
            let () = client.call(Request::Stop { game: e.game.into() }).await?;
        }
        Cmd::Get { game } => {
            if let Some((name, g)) = resolve_steam_settings(&client, &game).await? {
                let v: serde_json::Value = client.call(Request::SettingsGet { game: g.into() }).await?;
                println!("{name} ({g})");
                print_settings(&v);
                exports::hook_offer(&client, g, "its launch settings").await;
                return Ok(());
            }
            let e = resolve(&client, &game).await?;
            let v: serde_json::Value = client.call(Request::SettingsGet { game: e.product_id.into() }).await?;
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
                && let Ok(s) = client.call::<quiver_api::mods::ModsState>(Request::Mods(quiver_api::ModsRequest::List { game: e.product_id.into()})).await
            {
                println!("  (Play uses {})", mods::default_label(&s));
            }
        }
        Cmd::Set { game, changes, message } => {
            let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
            if let Some((name, g)) = resolve_steam_settings(&client, &game).await? {
                let rev: Revision = client.call(Request::SettingsSet { game: g.into(), ops, message }).await?;
                println!("{name} ({g}): settings revision {}", rev.revision);
                exports::hook_offer(&client, g, "its launch settings").await;
                return Ok(());
            }
            let e = resolve(&client, &game).await?;
            let rev: Revision = client.call(Request::SettingsSet { game: e.product_id.into(), ops, message }).await?;
            println!("{}: settings revision {}", e.name, rev.revision);
        }
        Cmd::History { game } => {
            let (name, g) = match resolve_steam_settings(&client, &game).await? {
                Some(x) => x,
                None => {
                    let e = resolve(&client, &game).await?;
                    (e.name.clone(), e.game)
                }
            };
            let revs: Vec<Revision> = client.call(Request::SettingsHistory { game: g.into() }).await?;
            if revs.is_empty() {
                println!("{name}: no changes (defaults)");
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
            let (name, g) = match resolve_steam_settings(&client, &game).await? {
                Some(x) => x,
                None => {
                    let e = resolve(&client, &game).await?;
                    (e.name.clone(), e.game)
                }
            };
            let rev: Revision = client.call(Request::SettingsRestore { game: g.into(), revision }).await?;
            println!("{name}: restored r{revision} as revision {}", rev.revision);
        }
        Cmd::Playtime { game } => {
            let any = resolve_any(&client, &game).await?;
            if let Some((name, g)) = epicgame::epic_game(&client, &game).await? {
                let p: quiver_api::epicgame::EpicPlaytime =
                    client.call(Request::Epic(quiver_api::epic::EpicRequest::Game(quiver_api::epicgame::EpicGameRequest::Playtime { game: Some(g), refresh: false }))).await?;
                epicgame::print_playtime(Some(&name), &p);
                return Ok(());
            }
            if any.ubisoft_id().is_none() {
                let p: quiver_api::Playtime = client.call(Request::Playtime { game: any.game.into() }).await?;
                let last = p.last_played.map(|t| t.with_timezone(&chrono::Local).format(" (last played %Y-%m-%d %H:%M)").to_string()).unwrap_or_default();
                println!("{}: {}h{:02}m, as {} recorded it{last}", any.name, p.total_seconds / 3600, p.total_seconds % 3600 / 60, any.store().label());
                return Ok(());
            }
            let e = resolve(&client, &game_label(&any.game)).await?;
            let p: quiver_api::Playtime = client.call(Request::Playtime { game: e.game.into() }).await?;
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
                .call(Request::LaunchContext { product_id: e.product_id, savegame_path, install_dir, account, steam_app_id: None })
                .await?;
            quiver_core::private::write_private(&out, serde_json::to_string_pretty(&ctx)?.as_bytes())?;
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
        Cmd::Ownership { product, account, json } => {
            let list: Vec<quiver_api::steamlib::OwnedProduct> = client.call(Request::Ownership { product_id: product, account }).await?;
            if json {
                return print_json(&list);
            }
            for p in &list {
                println!(
                    "{:>6}  type {:<2} state {} owned {:<5} pc build {:<5} {}  {}",
                    p.product_id,
                    p.product_type,
                    p.state,
                    p.owned,
                    p.has_pc_build,
                    if p.listed { "listed".to_owned() } else { format!("not listed: {}", p.not_listed_because.as_deref().unwrap_or("?")) },
                    p.name.as_deref().unwrap_or("")
                );
            }
            if list.is_empty() {
                println!("not in this account's ownership list");
            }
        }
        Cmd::Sessions { json } => {
            let list: Vec<quiver_api::ActiveSession> = client.call(Request::Sessions).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("nothing running");
            } else {
                // Steam games played through `quiver steam-run` need every store's names.
                let all = list.iter().any(|s| s.game.is_some());
                let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false, all_stores: all }).await.unwrap_or_default();
                for s in list {
                    let mins = (chrono::Utc::now() - s.started).num_minutes();
                    let g = s.game_ref();
                    let name = lib.iter().find(|e| e.game == g).map(|e| e.name.clone()).unwrap_or_default();
                    let via = match s.steam_app_id {
                        Some(app) if s.game.is_some() => format!("Steam (app {app})"),
                        Some(app) => format!("Steam app {app}"),
                        None => s.holders.join("+"),
                    };
                    let reported = match (s.game.is_some(), s.reported) {
                        (true, _) => "local history",
                        (false, true) => "reported to Ubisoft",
                        (false, false) => "not reported",
                    };
                    let id = if g.is_ubisoft() { s.product_id.to_string() } else { g.to_string() };
                    println!("{:<6} {:>4} min  via {:<16} {:<20} {name}", id, mins, via, reported);
                }
            }
        }
        Cmd::Watch => {
            let mut events = client.subscribe().await?;
            while let Some(e) = events.recv().await {
                println!("{}", serde_json::to_string(&e)?);
            }
        }
        Cmd::Mods { game, action } => mods::run(&client, &game, action).await?,
        Cmd::Profiles { game, json, action } => profiles::run(&client, &game, action, json).await?,
        Cmd::Perf { action } => perf::run(&client, action).await?,
        Cmd::Tweaks { json, action } => tweaks::run(&client, json, action).await?,
        Cmd::Hud { action } => perf::hud(&client, action).await?,
        Cmd::Sight { action } => sight::run(&client, action).await?,
        Cmd::FixesOnVanilla { value } => mods::fixes_on_vanilla(&client, value).await?,
        Cmd::Info { game, json, refresh, all } => info::info(&client, &game, json, refresh, all).await?,
        Cmd::Progress { game, json, refresh } => match epicgame::epic_game(&client, &game).await? {
            Some((name, g)) => epicgame::progress(&client, &name, g, json, refresh).await?,
            None => info::progress(&client, &game, json, refresh).await?,
        },
        Cmd::Achievements { game, json, refresh, action: None } => achievements::list(&client, &game, json, refresh).await?,
        Cmd::Achievements { game, action: Some(achievements::AchAction::Report { id, dry_run, json }), json: j, .. } => {
            achievements::report(&client, &cli.source, &game, id, dry_run, json || j).await?
        }
        Cmd::Achievements { game, action: Some(achievements::AchAction::Probe { platform, framing, all_products }), .. } => {
            achievements::probe(&client, &game, platform, framing, all_products).await?
        }
        Cmd::Saves { game, json, local, action: None } => match epicgame::epic_game(&client, &game).await? {
            Some(_) => epicgame::saves(&client, &game, json, local, None).await?,
            None => info::saves(&client, &game, json, local).await?,
        },
        Cmd::Saves { game, action: Some(info::SavesAction::Sync { profile }), .. } => info::sync(&client, &game, profile).await?,
        Cmd::Saves { game, action: Some(info::SavesAction::Restore { backup }), .. } => {
            info::restore(&client, &game, backup).await?
        }
        Cmd::Saves { game, action: Some(info::SavesAction::Stores { json }), json: j, .. } => savestores::stores(&client, &game, json || j).await?,
        Cmd::Saves { game, action: Some(info::SavesAction::Store { action }), .. } => savestores::store(&client, &game, action).await?,
        Cmd::Saves { game, action: Some(info::SavesAction::Merge { from, into }), .. } => {
            savestores::merge(&client, &game, from, into).await?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--machine` is global; a remote command's own machine argument stays its own.
    #[test]
    fn remote_and_machine_parse() {
        use remote::RemoteAction;
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("quiver").chain(args.iter().copied()));
        let c = parse(&["remote", "revoke", "laptop"]).unwrap();
        assert!(c.machine.is_none() && matches!(c.cmd, Cmd::Remote { action: RemoteAction::Revoke { ref peer } } if peer == "laptop"));
        let c = parse(&["--machine", "tower", "remote", "wake", "office"]).unwrap();
        assert_eq!(c.machine.as_deref(), Some("tower"));
        assert!(matches!(c.cmd, Cmd::Remote { action: RemoteAction::Wake { ref peer } } if peer == "office"));
        let c = parse(&["library", "--machine", "tower"]).unwrap();
        assert_eq!(c.machine.as_deref(), Some("tower"));
        assert!(matches!(parse(&["remote", "enable", "--listen", "0.0.0.0:47990"]).unwrap().cmd, Cmd::Remote { action: RemoteAction::Enable { listen: Some(_) } }));
        assert!(matches!(parse(&["remote", "rename", "Tower"]).unwrap().cmd, Cmd::Remote { action: RemoteAction::Rename { second: None, .. } }));
        assert!(parse(&["remote", "add", "tower"]).is_err(), "needs the code");
        let c = parse(&["--machine", "tower", "epic", "add-to", "laptop", "--account", "Me"]).unwrap();
        assert_eq!(c.machine.as_deref(), Some("tower"));
        assert!(matches!(c.cmd, Cmd::Epic { action: epic::EpicAction::AddTo { ref peer, account: Some(_) } } if peer == "laptop"));
        assert!(matches!(parse(&["epic", "add-from", "tower"]).unwrap().cmd, Cmd::Epic { action: epic::EpicAction::AddFrom { .. } }));
        assert!(matches!(parse(&["--machine", "tower", "login"]).unwrap().cmd, Cmd::Login { .. }));
        assert!(matches!(parse(&["account", "machines", "--json"]).unwrap().cmd, Cmd::Account { action: AccountAction::Machines { json: true } }));
    }

    #[test]
    fn queue_now_and_after_parse() {
        use downloads::{OnOff, QueueAction};
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("quiver").chain(args.iter().copied())).map(|c| c.cmd);
        assert!(matches!(parse(&["queue", "now", "origins"]), Ok(Cmd::Queue { action: Some(QueueAction::Now { game }), .. }) if game == "origins"));
        assert!(matches!(
            parse(&["queue", "move", "origins", "now"]),
            Ok(Cmd::Queue { action: Some(QueueAction::Move { to: quiver_api::downloads::QueueMove::Now, .. }), .. })
        ));
        assert!(parse(&["queue", "move", "origins", "sideways"]).is_err());
        assert!(matches!(
            parse(&["queue", "after", "origins", "--steam", "on"]),
            Ok(Cmd::Queue { action: Some(QueueAction::After { steam: Some(OnOff::On), desktop: None, .. }), .. })
        ));
        assert!(matches!(
            parse(&["queue", "after", "origins", "--desktop", "off", "--steam", "off"]),
            Ok(Cmd::Queue { action: Some(QueueAction::After { steam: Some(OnOff::Off), desktop: Some(OnOff::Off), .. }), .. })
        ));
        assert!(matches!(parse(&["install", "origins", "--add-to-steam"]), Ok(Cmd::Install { add_to_steam: true, desktop_entry: false, .. })));
    }

    fn entry(game: GameRef, name: &str) -> LibraryEntry {
        serde_json::from_value(serde_json::json!({
            "game": game, "name": name, "installed": null, "update_available": false, "running": false,
        }))
        .unwrap()
    }

    #[test]
    fn resolves_refs_and_names_across_stores() {
        let lib = || {
            vec![
                entry(GameRef::ubisoft(3539), "Assassin's Creed® Origins"),
                entry(GameRef::steam(620), "Portal 2"),
                entry(GameRef::steam(400), "Portal"),
                entry(GameRef::steam(582160), "Assassin's Creed Origins"),
            ]
        };
        assert_eq!(pick(lib(), "3539").unwrap().game, GameRef::ubisoft(3539));
        assert_eq!(pick(lib(), "ubi:3539").unwrap().game, GameRef::ubisoft(3539));
        assert_eq!(pick(lib(), "steam:620").unwrap().game, GameRef::steam(620));
        assert!(pick(lib(), "steam:1").unwrap_err().to_string().contains("steam:1 is not in your library"));
        assert!(pick(lib(), "9").unwrap_err().to_string().contains("product 9"));
        // A name: unique, exact over contained, or ambiguous across stores with refs to pick.
        assert_eq!(pick(lib(), "portal 2").unwrap().game, GameRef::steam(620));
        assert_eq!(pick(lib(), "portal").unwrap().game, GameRef::steam(400));
        let err = pick(lib(), "origins").unwrap_err().to_string();
        assert!(err.contains("(3539)") && err.contains("(steam:582160)") && err.contains("different stores"), "{err}");
        // Names with a colon aren't refs.
        assert!(pick(lib(), "Portal: 2").is_err());
        assert_eq!(game_label(&GameRef::ubisoft(5)), "5");
        assert_eq!(game_label(&GameRef::steam(5)), "steam:5");
    }

    #[test]
    fn epic_copies_resolve_and_say_where_they_are() {
        let camellia = GameRef::parse("epic:Camellia").unwrap();
        let mut origins = entry(GameRef::ubisoft(3539), "Assassin's Creed® Origins");
        origins.records.push(quiver_api::stores::StoreRecord { game: camellia, relation: quiver_api::stores::Relation::CopyOf, owned_by: vec![], note: None });
        let mut fc = entry(GameRef::parse("epic:Fc4").unwrap(), "Far Cry 4");
        fc.records.push(quiver_api::stores::StoreRecord { game: GameRef::ubisoft(420), relation: quiver_api::stores::Relation::Linked, owned_by: vec![], note: None });
        let aw = entry(GameRef::parse("epic:Kestrel").unwrap(), "Alan Wake Remastered");
        let lib = || vec![origins.clone(), fc.clone(), aw.clone()];
        // The folded copy's own ref finds its Ubisoft entry.
        assert_eq!(pick(lib(), "epic:Camellia").unwrap().game, GameRef::ubisoft(3539));
        assert_eq!(pick(lib(), "alan wake").unwrap().game.to_string(), "epic:Kestrel");
        assert_eq!(store_note(&origins), "  (also bought as epic:Camellia)");
        assert!(store_note(&fc).contains("not on a signed-in Ubisoft account: 420"));
        assert_eq!(store_note(&aw), "  (Epic, not installed)");
        assert_eq!(parse_store("Epic"), Ok(StoreId::Epic));
    }
}
