//! Persists the session in the desktop secret service (gnome-keyring / KWallet).

use anyhow::{Context, Result};

use crate::auth::{Auth, Session};

const SERVICE: &str = "ubisoft-linux";
const ACCOUNT: &str = "session";

fn entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT).context("open secret service entry")
}

pub fn load() -> Result<Option<Session>> {
    match entry()?.get_password() {
        Ok(json) => Ok(Some(serde_json::from_str(&json).context("corrupt stored session")?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("read session from secret service"),
    }
}

pub fn save(session: &Session) -> Result<()> {
    entry()?
        .set_password(&serde_json::to_string(session)?)
        .context("write session to secret service")
}

pub fn clear() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete session from secret service"),
    }
}

/// Load the stored session, refreshing it via the remember-me ticket if it is
/// close to expiry. Errors if nobody is logged in.
pub async fn current(auth: &Auth) -> Result<Session> {
    let session = load()?.context("not logged in (run `ubi login`)")?;
    if !session.expires_within(chrono::Duration::minutes(10)) {
        return Ok(session);
    }
    let rm = session
        .remember_me_ticket
        .as_deref()
        .context("session expired and no remember-me ticket; run `ubi login`")?;
    let fresh = auth.refresh(rm).await.context("session refresh failed; run `ubi login`")?;
    save(&fresh)?;
    Ok(fresh)
}
