//! Signed-in accounts. Sessions (tickets) live in the desktop secret service
//! (gnome-keyring / KWallet), one entry per account (`session:<userId>`); the non-secret
//! index of accounts and which one is the default lives in `<data>/accounts.json`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth::{Auth, Session};
use crate::launcher::data_dir;

const SERVICE: &str = "quiver";
/// Pre-Quiver service name; a session stored there is moved on first use.
const LEGACY_SERVICE: &str = "ubisoft-linux";
/// Single-account entry name used before multiple accounts.
const LEGACY_ACCOUNT: &str = "session";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountRef {
    pub user_id: String,
    /// Ubisoft username (name on platform).
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    accounts: Vec<AccountRef>,
}

fn index_path() -> std::path::PathBuf {
    data_dir().join("accounts.json")
}

fn read_index() -> Result<Index> {
    match std::fs::read_to_string(index_path()) {
        Ok(text) => serde_json::from_str(&text).context("corrupt accounts.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => migrate_single(),
        Err(e) => Err(e).context("read accounts.json"),
    }
}

fn write_index(index: &Index) -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(index_path(), serde_json::to_vec_pretty(index)?)?;
    Ok(())
}

fn entry(user_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("session:{user_id}")).context("open secret service entry")
}

/// Before multiple accounts there was one keyring entry (`quiver`/`session`, or
/// `ubisoft-linux`/`session` before the rename). Move it to its per-account entry.
fn migrate_single() -> Result<Index> {
    let mut index = Index::default();
    for service in [SERVICE, LEGACY_SERVICE] {
        let old = keyring::Entry::new(service, LEGACY_ACCOUNT).context("open secret service entry")?;
        let json = match old.get_password() {
            Ok(json) => json,
            Err(keyring::Error::NoEntry) => continue,
            Err(e) => return Err(e).context("read stored session"),
        };
        let session: Session = serde_json::from_str(&json).context("corrupt stored session")?;
        entry(&session.user_id)?.set_password(&json).context("write session to secret service")?;
        index.default = Some(session.user_id.clone());
        index.accounts.push(AccountRef { user_id: session.user_id.clone(), name: session.name.clone() });
        write_index(&index)?;
        let _ = old.delete_credential();
        tracing::info!("moved stored session for {} to its per-account keyring entry", session.name);
        break;
    }
    Ok(index)
}

/// Signed-in accounts, in sign-in order.
pub fn accounts() -> Result<Vec<AccountRef>> {
    Ok(read_index()?.accounts)
}

/// The default account's user id (the first signed in, unless changed).
pub fn default_account() -> Result<Option<String>> {
    let index = read_index()?;
    Ok(index.default.or_else(|| index.accounts.first().map(|a| a.user_id.clone())))
}

pub fn set_default(user_id: &str) -> Result<()> {
    let mut index = read_index()?;
    anyhow::ensure!(index.accounts.iter().any(|a| a.user_id == user_id), "no signed-in account {user_id}");
    index.default = Some(user_id.to_owned());
    write_index(&index)
}

/// Find an account by user id, username (case-insensitive) or a unique user-id prefix.
pub fn resolve(query: &str) -> Result<AccountRef> {
    let accounts = accounts()?;
    if let Some(a) = accounts.iter().find(|a| a.user_id == query || a.name.eq_ignore_ascii_case(query)) {
        return Ok(a.clone());
    }
    let matches: Vec<_> = accounts.iter().filter(|a| a.user_id.starts_with(query)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!("no signed-in account matches '{query}'"),
        _ => anyhow::bail!("'{query}' matches more than one account"),
    }
}

fn resolve_id(user_id: Option<&str>) -> Result<Option<String>> {
    match user_id {
        Some(id) => Ok(Some(id.to_owned())),
        None => default_account(),
    }
}

/// The stored session for an account (`None`: the default account).
pub fn load(user_id: Option<&str>) -> Result<Option<Session>> {
    let Some(id) = resolve_id(user_id)? else { return Ok(None) };
    match entry(&id)?.get_password() {
        Ok(json) => Ok(Some(serde_json::from_str(&json).context("corrupt stored session")?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("read session from secret service"),
    }
}

/// Store a session, adding its account (the first account becomes the default).
pub fn save(session: &Session) -> Result<()> {
    entry(&session.user_id)?
        .set_password(&serde_json::to_string(session)?)
        .context("write session to secret service")?;
    let mut index = read_index()?;
    match index.accounts.iter_mut().find(|a| a.user_id == session.user_id) {
        Some(a) if a.name == session.name => return Ok(()),
        Some(a) => a.name = session.name.clone(),
        None => index.accounts.push(AccountRef { user_id: session.user_id.clone(), name: session.name.clone() }),
    }
    if index.default.is_none() {
        index.default = Some(session.user_id.clone());
    }
    write_index(&index)
}

/// Sign an account out (`None`: the default). If it was the default, the next one is.
pub fn clear(user_id: Option<&str>) -> Result<Option<AccountRef>> {
    let Some(id) = resolve_id(user_id)? else { return Ok(None) };
    match entry(&id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(e).context("delete session from secret service"),
    }
    let mut index = read_index()?;
    let removed = index.accounts.iter().position(|a| a.user_id == id).map(|i| index.accounts.remove(i));
    if index.default.as_deref() == Some(&id) {
        index.default = index.accounts.first().map(|a| a.user_id.clone());
    }
    write_index(&index)?;
    Ok(removed)
}

/// An account's session (`None`: the default), refreshed through the remember-me ticket if
/// close to expiry. Errors if that account isn't signed in.
pub async fn current(auth: &Auth, user_id: Option<&str>) -> Result<Session> {
    let session = load(user_id)?.context("not logged in (run `quiver login`)")?;
    if !session.expires_within(chrono::Duration::minutes(10)) {
        return Ok(session);
    }
    let rm = session
        .remember_me_ticket
        .as_deref()
        .with_context(|| format!("session for {} expired and has no remember-me ticket; run `quiver login`", session.name))?;
    let fresh = auth
        .refresh(rm)
        .await
        .with_context(|| format!("session refresh for {} failed; run `quiver login`", session.name))?;
    save(&fresh)?;
    Ok(fresh)
}
