//! `quiver epic …`: Epic accounts (design/STORE-EPIC.md §5). Epic's games are in `quiver
//! library --store epic` and `quiver info epic:<app>`.

use std::io::BufRead;

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::epic::{EpicAccount, EpicRequest, LoginStart};
use quiver_api::stores::AccountState;
use quiver_api::{Client, Request};

use crate::print_json;

#[derive(Subcommand)]
pub enum EpicAction {
    /// Sign in to Epic in your browser, then paste the code Epic shows. Each sign-in adds
    /// an account; the first becomes the default.
    Login {
        /// The code (or the whole page of JSON) from Epic, if you already have it.
        #[arg(long)]
        code: Option<String>,
        /// Also open the sign-in page in your browser.
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

pub async fn run(client: &Client, action: EpicAction) -> Result<()> {
    match action {
        EpicAction::Login { code, open } => {
            let code = match code {
                Some(c) => c,
                None => {
                    let start: LoginStart = client.call(epic(EpicRequest::LoginBegin)).await?;
                    eprintln!("Open this page and sign in to Epic:\n\n  {}\n", start.url);
                    eprintln!("{}\n", start.instructions);
                    if open && let Err(e) = std::process::Command::new("xdg-open").arg(&start.url).spawn() {
                        eprintln!("(couldn't open a browser: {e})");
                    }
                    eprint!("Code: ");
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    read_pasted()?
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
    }
    Ok(())
}
