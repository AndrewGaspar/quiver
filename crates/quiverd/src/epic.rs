//! `Request::Epic`: Epic accounts (design/STORE-EPIC.md §5). The games come through the
//! store-generic requests, from `epic_core::provider::EpicProvider`.
//!
//! Signing in and out are a person's actions: refused from every source but the app and
//! the terminal (STORES.md §7, item 10: "Sign in, sign out … agents: Deny").

use std::sync::Arc;

use anyhow::{Result, bail};
use quiver_api::epic::EpicRequest;
use quiver_api::{Event, StoreId};
use serde_json::{Value, json};

use crate::Daemon;

fn person_only(source: &str, what: &str) -> Result<()> {
    if !quiver_api::mods::is_human_source(source) {
        bail!("only you can {what}, from the app or a terminal (this request came from '{source}')");
    }
    Ok(())
}

pub(crate) async fn handle(d: &Arc<Daemon>, source: &str, r: EpicRequest) -> Result<Value> {
    Ok(match r {
        EpicRequest::LoginBegin => json!(d.epic.login_start()),
        EpicRequest::LoginComplete { code } => {
            person_only(source, "sign in to Epic")?;
            let a = d.epic.login(&code).await?;
            d.log_activity(source, format!("signed in to Epic as {}", a.display_name), None);
            changed(d);
            // Read the library now, so its catalog is cached before anyone looks.
            let d2 = d.clone();
            tokio::spawn(async move {
                match d2.epic.library(true).await {
                    Ok(lib) => tracing::info!("Epic library: {} game(s)", lib.titles.len()),
                    Err(e) => tracing::info!("Epic library: {e:#}"),
                }
                d2.emit(Event::LibraryChanged);
            });
            json!(a)
        }
        EpicRequest::Logout { account } => {
            person_only(source, "sign out of Epic")?;
            let a = d.epic.logout(account.as_deref()).await?;
            d.log_activity(source, format!("signed out of Epic ({})", a.display_name), None);
            changed(d);
            json!(a)
        }
        EpicRequest::Accounts => json!(d.epic.accounts()?),
        EpicRequest::Hidden => json!(d.epic.hidden_items().await?),
        EpicRequest::AccountDefault { account } => {
            let a = d.epic.set_default(&account)?;
            d.log_activity(source, format!("default Epic account is now {}", a.display_name), None);
            changed(d);
            json!(a)
        }
    })
}

fn changed(d: &Daemon) {
    d.emit(Event::StoreAccountsChanged { store: StoreId::Epic });
    d.emit(Event::LibraryChanged);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_cannot_sign_in_or_out() {
        assert!(person_only("cli", "x").is_ok() && person_only("gui", "x").is_ok());
        for s in ["agent", "agent:claude", "mcp", "steam", "test"] {
            assert!(person_only(s, "sign in to Epic").unwrap_err().to_string().contains("only you can sign in to Epic"));
        }
    }
}
