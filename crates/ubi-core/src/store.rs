//! Persists the session in the desktop secret service (gnome-keyring / KWallet).

use anyhow::{Context, Result};

use crate::auth::{Auth, Session};

const SERVICE: &str = "quiver";
/// Pre-Quiver service name; a session stored there is moved on first load.
const LEGACY_SERVICE: &str = "ubisoft-linux";
const ACCOUNT: &str = "session";

fn entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT).context("open secret service entry")
}

pub fn load() -> Result<Option<Session>> {
    match entry()?.get_password() {
        Ok(json) => Ok(Some(serde_json::from_str(&json).context("corrupt stored session")?)),
        Err(keyring::Error::NoEntry) => migrate_legacy(),
        Err(e) => Err(e).context("read session from secret service"),
    }
}

fn migrate_legacy() -> Result<Option<Session>> {
    let old = keyring::Entry::new(LEGACY_SERVICE, ACCOUNT).context("open secret service entry")?;
    let json = match old.get_password() {
        Ok(json) => json,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(e) => return Err(e).context("read legacy session from secret service"),
    };
    let session: Session = serde_json::from_str(&json).context("corrupt stored session")?;
    save(&session)?;
    let _ = old.delete_credential();
    tracing::info!("moved stored session to the \"{SERVICE}\" keyring entry");
    Ok(Some(session))
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
    let session = load()?.context("not logged in (run `quiver login`)")?;
    if !session.expires_within(chrono::Duration::minutes(10)) {
        return Ok(session);
    }
    let rm = session
        .remember_me_ticket
        .as_deref()
        .context("session expired and no remember-me ticket; run `quiver login`")?;
    let fresh = auth.refresh(rm).await.context("session refresh failed; run `quiver login`")?;
    save(&fresh)?;
    Ok(fresh)
}
