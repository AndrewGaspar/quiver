//! Signed-in accounts. Sessions (tickets) live in the desktop secret service
//! (gnome-keyring / KWallet), one entry per account (`session:<userId>`); the non-secret
//! index of accounts and which one is the default lives in `<data>/accounts.json`.

use std::path::{Path, PathBuf};

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

/// The non-secret account index (`accounts.json`). Pure: all keyring access is outside it.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    accounts: Vec<AccountRef>,
}

impl Index {
    /// The default account: the one chosen, else the first signed in.
    fn default_id(&self) -> Option<String> {
        self.default
            .clone()
            .filter(|d| self.accounts.iter().any(|a| &a.user_id == d))
            .or_else(|| self.accounts.first().map(|a| a.user_id.clone()))
    }

    /// Add an account or update its name. The first account becomes the default.
    /// Returns whether anything changed.
    fn add(&mut self, account: AccountRef) -> bool {
        let mut changed = match self.accounts.iter_mut().find(|a| a.user_id == account.user_id) {
            Some(a) if a.name == account.name => false,
            Some(a) => {
                a.name = account.name;
                true
            }
            None => {
                self.accounts.push(account);
                true
            }
        };
        if self.default.is_none() {
            self.default = self.accounts.first().map(|a| a.user_id.clone());
            changed = true;
        }
        changed
    }

    /// Remove an account. If it was the default, the next remaining account is.
    fn remove(&mut self, user_id: &str) -> Option<AccountRef> {
        let removed = self.accounts.iter().position(|a| a.user_id == user_id).map(|i| self.accounts.remove(i));
        if self.default.as_deref() == Some(user_id) {
            self.default = self.accounts.first().map(|a| a.user_id.clone());
        }
        removed
    }

    fn set_default(&mut self, user_id: &str) -> Result<()> {
        anyhow::ensure!(self.accounts.iter().any(|a| a.user_id == user_id), "no signed-in account {user_id}");
        self.default = Some(user_id.to_owned());
        Ok(())
    }

    /// Find an account by user id, username (case-insensitive) or a unique user-id prefix.
    fn resolve(&self, query: &str) -> Result<AccountRef> {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "empty account name");
        if let Some(a) = self.accounts.iter().find(|a| a.user_id == query || a.name.eq_ignore_ascii_case(query)) {
            return Ok(a.clone());
        }
        let matches: Vec<_> = self.accounts.iter().filter(|a| a.user_id.starts_with(query)).collect();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] if self.accounts.is_empty() => anyhow::bail!("no accounts are signed in (run `quiver login`)"),
            [] => anyhow::bail!(
                "no signed-in account matches '{query}' (signed in: {})",
                self.accounts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
            _ => anyhow::bail!("'{query}' matches more than one account"),
        }
    }
}

fn index_path() -> PathBuf {
    data_dir().join("accounts.json")
}

/// The index file, or `None` if there is none yet.
fn load_index_file(path: &Path) -> Result<Option<Index>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).context("corrupt accounts.json")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read accounts.json"),
    }
}

fn write_index_file(path: &Path, index: &Index) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("accounts.json path")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(index)?)?;
    std::fs::rename(&tmp, path).context("write accounts.json")?;
    Ok(())
}

fn read_index() -> Result<Index> {
    match load_index_file(&index_path())? {
        Some(index) => Ok(index),
        None => migrate_single(),
    }
}

fn write_index(index: &Index) -> Result<()> {
    write_index_file(&index_path(), index)
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
        index.add(AccountRef { user_id: session.user_id.clone(), name: session.name.clone() });
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

/// Signed-in accounts (in sign-in order) and the default's user id, from one read.
pub fn accounts_and_default() -> Result<(Vec<AccountRef>, Option<String>)> {
    let index = read_index()?;
    let default = index.default_id();
    Ok((index.accounts, default))
}

/// The default account's user id (the first signed in, unless changed).
pub fn default_account() -> Result<Option<String>> {
    Ok(read_index()?.default_id())
}

pub fn set_default(user_id: &str) -> Result<()> {
    let mut index = read_index()?;
    index.set_default(user_id)?;
    write_index(&index)
}

/// Find an account by user id, username (case-insensitive) or a unique user-id prefix.
pub fn resolve(query: &str) -> Result<AccountRef> {
    read_index()?.resolve(query)
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
    if index.add(AccountRef { user_id: session.user_id.clone(), name: session.name.clone() }) {
        write_index(&index)?;
    }
    Ok(())
}

/// Sign an account out (`None`: the default). If it was the default, the next one is.
pub fn clear(user_id: Option<&str>) -> Result<Option<AccountRef>> {
    let Some(id) = resolve_id(user_id)? else { return Ok(None) };
    match entry(&id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(e).context("delete session from secret service"),
    }
    let mut index = read_index()?;
    let removed = index.remove(&id);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(id: &str, name: &str) -> AccountRef {
        AccountRef { user_id: id.into(), name: name.into() }
    }

    #[test]
    fn add_remove_default() {
        let mut i = Index::default();
        assert_eq!(i.default_id(), None);
        assert!(i.add(acct("aaaa-1", "Main")));
        assert_eq!(i.default_id().as_deref(), Some("aaaa-1"));
        assert!(i.add(acct("bbbb-2", "Family")));
        assert!(i.add(acct("bbcc-3", "Third")));
        // Re-login with the same name changes nothing; a rename is recorded.
        assert!(!i.add(acct("bbbb-2", "Family")));
        assert!(i.add(acct("bbbb-2", "Fam")));
        assert_eq!(i.accounts.len(), 3);
        assert_eq!(i.default_id().as_deref(), Some("aaaa-1"));

        i.set_default("bbcc-3").unwrap();
        assert_eq!(i.default_id().as_deref(), Some("bbcc-3"));
        assert!(i.set_default("nope").is_err());

        // Removing a non-default keeps the default; removing the default promotes the next.
        assert_eq!(i.remove("aaaa-1"), Some(acct("aaaa-1", "Main")));
        assert_eq!(i.default_id().as_deref(), Some("bbcc-3"));
        assert_eq!(i.remove("bbcc-3"), Some(acct("bbcc-3", "Third")));
        assert_eq!(i.default_id().as_deref(), Some("bbbb-2"));
        assert_eq!(i.remove("missing"), None);
        assert_eq!(i.remove("bbbb-2"), Some(acct("bbbb-2", "Fam")));
        assert_eq!(i.default, None);
        assert_eq!(i.default_id(), None);
        // The next sign-in becomes the default again.
        i.add(acct("cccc-4", "New"));
        assert_eq!(i.default.as_deref(), Some("cccc-4"));
    }

    #[test]
    fn stale_default_falls_back() {
        let i = Index { default: Some("gone".into()), accounts: vec![acct("a", "A")] };
        assert_eq!(i.default_id().as_deref(), Some("a"));
    }

    #[test]
    fn resolve_by_id_name_prefix() {
        let mut i = Index::default();
        assert!(i.resolve("x").unwrap_err().to_string().contains("quiver login"));
        i.add(acct("aaaa-1", "Main"));
        i.add(acct("bbbb-2", "Family"));
        i.add(acct("bbcc-3", "Third"));
        assert_eq!(i.resolve("bbbb-2").unwrap().name, "Family");
        assert_eq!(i.resolve("family").unwrap().user_id, "bbbb-2");
        assert_eq!(i.resolve(" FAMILY ").unwrap().user_id, "bbbb-2");
        assert_eq!(i.resolve("aa").unwrap().user_id, "aaaa-1");
        assert!(i.resolve("bb").unwrap_err().to_string().contains("more than one"));
        assert!(i.resolve("zz").unwrap_err().to_string().contains("Main, Family, Third"));
        assert!(i.resolve("").is_err());
    }

    /// Index file round trip under a temporary `XDG_DATA_HOME`. The file always exists
    /// before `read_index` runs, so the keyring migration is never reached.
    #[test]
    fn index_file_under_xdg_data_home() {
        let tmp = std::env::temp_dir().join(format!("quiver-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: the only test in this crate that touches the environment.
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
        let path = index_path();
        assert!(path.starts_with(&tmp), "{}", path.display());
        assert_eq!(load_index_file(&path).unwrap(), None);

        let mut i = Index::default();
        i.add(acct("aaaa-1", "Main"));
        i.add(acct("bbbb-2", "Family"));
        write_index(&i).unwrap();
        assert_eq!(read_index().unwrap(), i);
        assert_eq!(accounts().unwrap().len(), 2);
        assert_eq!(default_account().unwrap().as_deref(), Some("aaaa-1"));
        set_default("bbbb-2").unwrap();
        assert_eq!(accounts_and_default().unwrap().1.as_deref(), Some("bbbb-2"));
        assert_eq!(resolve("main").unwrap().user_id, "aaaa-1");
        assert!(set_default("nobody").is_err());
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["default"], "bbbb-2");
        assert_eq!(json["accounts"][1]["name"], "Family");

        std::fs::write(&path, "{not json").unwrap();
        assert!(read_index().is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
