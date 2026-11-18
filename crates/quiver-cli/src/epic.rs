//! `quiver epic …`: Epic accounts (design/STORE-EPIC.md §5). Epic's games are in `quiver
//! library --store epic` and `quiver info epic:<app>`.

use std::io::BufRead;

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::epic::webview::{self, Outcome};
use quiver_api::epic::{EpicAccount, EpicRequest, LoginStart};
use quiver_api::stores::AccountState;
use quiver_api::{Client, Request};

use crate::print_json;

#[derive(Subcommand)]
pub enum EpicAction {
    /// Sign in to Epic in Quiver's sign-in window (or, with --paste, in your browser, then
    /// paste the code Epic shows). Each sign-in adds an account; the first becomes the
    /// default.
    Login {
        /// The code (or the whole page of JSON) from Epic, if you already have it.
        #[arg(long)]
        code: Option<String>,
        /// Sign in in your browser and paste the code, instead of the sign-in window.
        #[arg(long)]
        paste: bool,
        /// Open the sign-in page in your browser (implies --paste).
        #[arg(long)]
        open: bool,
    },
    /// Sign an Epic account out and delete its stored session (default: the default
    /// account). Other launchers' Epic sessions are not affected.
    Logout {
        /// Display name, account id or id prefix.
        account: Option<String>,
    },
    /// Epic accounts in Quiver (* marks the default).
    Accounts {
        #[arg(long)]
        json: bool,
    },
    /// Make an Epic account the default.
    Default { account: String },
    /// An Epic game's cloud saves here and in Epic's cloud, and its backups; `sync`
    /// downloads and uploads now, `restore` brings a backup back.
    Saves {
        game: String,
        #[arg(long)]
        json: bool,
        /// Only what's on this machine (don't ask the cloud).
        #[arg(long)]
        local: bool,
        #[command(subcommand)]
        action: Option<crate::epicgame::SavesAction>,
    },
    /// Epic's play time (for a game, or every game) and the sessions Quiver reports;
    /// `--send` reports queued sessions now.
    Playtime {
        game: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        send: bool,
        /// With `--send`: yes to the first report to your Epic account.
        #[arg(long)]
        yes: bool,
        /// Drop the sessions waiting to be reported (for GAME, or every game): never sent.
        #[arg(long)]
        discard: bool,
    },
    /// The current build of an Epic game as Epic serves it: version, files, sizes, launch
    /// executable, prerequisites. Read-only: nothing is downloaded but the manifest.
    Manifest {
        game: String,
        #[arg(long)]
        json: bool,
    },
    /// Whether quiverd keeps Epic cloud saves fresh between sessions (it checks the cloud
    /// now and then and downloads what's newer, so games are ready offline; it never
    /// uploads on its own). With on/off, changes it.
    SavesBackground {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// A game's digital extras (soundtracks, art books, wallpapers); `install EXTRA`
    /// downloads one into a folder next to the game's (never into it).
    Extras {
        game: String,
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<ExtrasAction>,
    },
}

#[derive(Subcommand)]
pub enum ExtrasAction {
    /// Download (or update) an extra: its id from the list, or its name.
    Install {
        extra: String,
        /// Don't follow the download; it runs on in quiverd.
        #[arg(long)]
        detach: bool,
    },
}

fn epic(r: EpicRequest) -> Request {
    Request::Epic(r)
}

fn print_account(a: &EpicAccount) {
    let state = match a.state {
        AccountState::SignedIn => String::new(),
        _ => "  (sign in again: quiver epic login)".into(),
    };
    let ubi = match &a.ubisoft {
        Some(u) => format!("  Ubisoft: {}", u.name.as_deref().unwrap_or(&u.id)),
        None => String::new(),
    };
    println!("{} {:<24} {}{ubi}{state}", if a.default { "*" } else { " " }, a.display_name, a.account_id);
}

/// Read the pasted code from the terminal: one line, or a pasted block of JSON up to its
/// closing brace.
fn read_pasted() -> Result<String> {
    let stdin = std::io::stdin();
    let mut text = String::new();
    for line in stdin.lock().lines().take(64) {
        let line = line?;
        text.push_str(&line);
        text.push('\n');
        let t = text.trim();
        let open_json = t.starts_with('{') && !t.ends_with('}');
        if !t.is_empty() && !open_json {
            break;
        }
    }
    if text.trim().is_empty() {
        bail!("nothing was pasted");
    }
    Ok(text)
}

/// Sign in in Quiver's sign-in window. `None`: there's no window here (the reason is
/// printed), so the caller falls back to pasting.
async fn window(start: &LoginStart) -> Result<Option<String>> {
    eprintln!("Opening Epic's sign-in window…");
    match webview::sign_in(&start.url, webview::DEFAULT_TIMEOUT).await {
        Outcome::Code(c) => Ok(Some(c.into_inner())),
        Outcome::Cancelled => bail!("Sign-in cancelled"),
        Outcome::TimedOut => bail!("The sign-in window timed out; run `quiver epic login` again"),
        Outcome::Failed(e) => bail!("The sign-in window failed: {e} (`quiver epic login --paste` signs in with your browser instead)"),
        Outcome::Unavailable(why) => {
            eprintln!("The sign-in window isn't available ({why}); install webkit2gtk-4.1 for it. Falling back to pasting the code.\n");
            Ok(None)
        }
    }
}

/// The browser and a pasted code.
fn paste_flow(start: &LoginStart, open: bool) -> Result<String> {
    eprintln!("Open this page and sign in to Epic:\n\n  {}\n", start.url);
    eprintln!("{}\n", start.instructions);
    if open && let Err(e) = std::process::Command::new("xdg-open").arg(&start.url).spawn() {
        eprintln!("(couldn't open a browser: {e})");
    }
    eprint!("Code: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    read_pasted()
}

pub async fn run(client: &Client, action: EpicAction) -> Result<()> {
    match action {
        EpicAction::Login { code, paste, open } => {
            let code = match code {
                Some(c) => c,
                None => {
                    let start: LoginStart = client.call(epic(EpicRequest::LoginBegin)).await?;
                    let from_window = if paste || open { None } else { window(&start).await? };
                    match from_window {
                        Some(c) => c,
                        None => paste_flow(&start, open)?,
                    }
                }
            };
            let a: EpicAccount = client.call(epic(EpicRequest::LoginComplete { code })).await?;
            let linked = a.ubisoft.as_ref().map(|u| format!(" (linked Ubisoft account: {})", u.name.as_deref().unwrap_or(&u.id))).unwrap_or_default();
            println!("Signed in to Epic as {}{}{linked}", a.display_name, if a.default { " (default account)" } else { "" });
            println!("Your Epic games: quiver library --store epic");
        }
        EpicAction::Logout { account } => {
            let a: EpicAccount = client.call(epic(EpicRequest::Logout { account })).await?;
            println!("Signed {} out of Epic in Quiver", a.display_name);
        }
        EpicAction::Accounts { json } => {
            let list: Vec<EpicAccount> = client.call(epic(EpicRequest::Accounts)).await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no Epic accounts (quiver epic login)");
            }
            list.iter().for_each(print_account);
        }
        EpicAction::Default { account } => {
            let a: EpicAccount = client.call(epic(EpicRequest::AccountDefault { account })).await?;
            println!("default Epic account: {}", a.display_name);
        }
        EpicAction::Saves { game, json, local, action } => crate::epicgame::saves(client, &game, json, local, action).await?,
        EpicAction::Playtime { game, json, send, yes, discard } => crate::epicgame::playtime(client, game, json, send, yes, discard).await?,
        EpicAction::SavesBackground { state } => {
            let enabled = state.map(|s| s == "on");
            let v: serde_json::Value = client.call(epic(EpicRequest::Game(quiver_api::epicgame::EpicGameRequest::SavesBackground { enabled }))).await?;
            if v["enabled"].as_bool().unwrap_or(false) {
                println!("on: quiverd checks installed Epic games' cloud saves every 30 minutes (and when the network comes back) and downloads what's newer, backing this PC's copies up first");
            } else {
                println!("off: Epic cloud saves sync only around launches and on request");
            }
        }
        EpicAction::Extras { game, json, action } => {
            let e = crate::resolve_any(client, &game).await?;
            anyhow::ensure!(e.game.store == quiver_api::StoreId::Epic, "{} isn't an Epic game (extras are Epic's)", e.name);
            match action {
                None => {
                    let d: quiver_api::stores::GameDetail = client.call(Request::GameDetail { game: e.game, refresh: false }).await?;
                    if json {
                        return print_json(&d.extras);
                    }
                    if d.extras.is_empty() {
                        println!("{}: no digital extras", e.name);
                        return Ok(());
                    }
                    println!("{} ({}): {} digital extra(s), each its own download next to the game's folder", e.name, e.game, d.extras.len());
                    for x in &d.extras {
                        let state = match (&x.installed, &x.dir) {
                            (Some(v), Some(dir)) => format!("downloaded ({v}): {}", dir.display()),
                            (None, Some(dir)) => format!("not downloaded (goes to {})", dir.display()),
                            _ => "not downloaded".to_owned(),
                        };
                        println!("  {:<40} {}  {state}", x.name, x.id);
                    }
                    println!("download one: quiver epic extras {} install <id>", e.game);
                }
                Some(ExtrasAction::Install { extra, detach }) => {
                    let events = client.subscribe().await?;
                    let id: u64 = client.call(epic(EpicRequest::ExtraInstall { game: e.game, extra })).await?;
                    eprintln!("downloading an extra of {} (job {id})", e.name);
                    if !detach {
                        crate::follow(client, events, id).await?;
                    }
                }
            }
        }
        EpicAction::Manifest { game, json } => {
            let e = crate::resolve_installable(client, &game).await?;
            anyhow::ensure!(e.game.store == quiver_api::StoreId::Epic, "{} isn't an Epic game", e.name);
            let v: serde_json::Value = client.call(epic(EpicRequest::Manifest { game: e.game })).await?;
            if json {
                return print_json(&v);
            }
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
            let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let gb = |b: u64| format!("{:.2} GB", b as f64 / 1e9);
            println!("{} ({}): build {}", s("title"), e.game, s("build_version"));
            println!("  manifest v{}, {} files, {} chunks{}", n("manifest_version"), n("files"), n("chunks"), if n("encrypted") > 0 { ", encrypted" } else { "" });
            println!("  install {}, download {}", gb(n("install_bytes")), gb(n("download_bytes")));
            println!("  launches {} {}", s("launch_exe"), s("launch_command"));
            if let Some(p) = v.get("prerequisites").filter(|p| !p.is_null()) {
                println!("  prerequisites: {} ({} {})", p["name"].as_str().unwrap_or(""), p["path"].as_str().unwrap_or(""), p["args"].as_str().unwrap_or(""));
            }
            if let Some(u) = v.get("update").filter(|u| !u.is_null()) {
                println!("  update from the installed build: {} to write, {} kept, {} removed, {} to download", u["write"], u["keep"], u["remove"], gb(u["download_bytes"].as_u64().unwrap_or(0)));
            }
        }
    }
    Ok(())
}
