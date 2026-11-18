//! The non-secret index of Epic accounts: `<data>/epic/accounts.json` (ids, display names,
//! the default, the linked Ubisoft account, whether the session still works). Sessions
//! are in the keyring (`secrets`).
//!
//! STORES.md §3.2 planned one `accounts.json` for every store, with a `store` field per
//! entry. That file is Ubisoft's, and a quiverd from before stores rewrites it without
//! fields it doesn't know, which would drop Epic's accounts. So Epic's index is its own file
//! (decided while building, 2026-11-17).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use quiver_api::epic::{EpicAccount, LinkedAccount};
use quiver_api::stores::AccountState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub account_id: String,
    pub display_name: String,
    /// The Ubisoft account linked on Epic's side (`externalAuths`, type `ubisoft`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ubisoft: Option<LinkedAccount>,
    /// Epic stopped accepting the session: shown as "sign in again", never retried.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_sign_in: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub accounts: Vec<Entry>,
}

impl Index {
    pub fn path(epic_dir: &Path) -> PathBuf {
        epic_dir.join("accounts.json")
    }

    pub fn load(epic_dir: &Path) -> Result<Index> {
        match std::fs::read_to_string(Self::path(epic_dir)) {
            Ok(text) => serde_json::from_str(&text).context("corrupt epic/accounts.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e).context("read epic/accounts.json"),
        }
    }

    pub fn save(&self, epic_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(epic_dir)?;
        let path = Self::path(epic_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path).context("write epic/accounts.json")
    }

    /// The default: the one chosen, else the first.
    pub fn default_id(&self) -> Option<&str> {
        self.default.as_deref().filter(|d| self.accounts.iter().any(|a| a.account_id == *d)).or_else(|| self.accounts.first().map(|a| a.account_id.as_str()))
    }

    /// Add or update an account (signed in again: no longer `needs_sign_in`). The first
    /// becomes the default.
    pub fn upsert(&mut self, e: Entry) {
        match self.accounts.iter_mut().find(|a| a.account_id == e.account_id) {
            Some(a) => *a = e,
            None => self.accounts.push(e),
        }
        if self.default_id() != self.default.as_deref() {
            self.default = self.default_id().map(str::to_owned);
        }
    }

    pub fn remove(&mut self, account_id: &str) -> Option<Entry> {
        let i = self.accounts.iter().position(|a| a.account_id == account_id)?;
        let e = self.accounts.remove(i);
        if self.default.as_deref() == Some(account_id) {
            self.default = self.accounts.first().map(|a| a.account_id.clone());
        }
        Some(e)
    }

    pub fn get_mut(&mut self, account_id: &str) -> Option<&mut Entry> {
        self.accounts.iter_mut().find(|a| a.account_id == account_id)
    }

    /// An account by id, display name (any case) or a unique id prefix; `None`: the default.
    pub fn resolve(&self, query: Option<&str>) -> Result<&Entry> {
        let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) else {
            let id = self.default_id().context("no Epic account is signed in (quiver epic login)")?;
            return Ok(self.accounts.iter().find(|a| a.account_id == id).expect("default is in the list"));
        };
        if let Some(a) = self.accounts.iter().find(|a| a.account_id == q || a.display_name.eq_ignore_ascii_case(q)) {
            return Ok(a);
        }
        let m: Vec<&Entry> = self.accounts.iter().filter(|a| a.account_id.starts_with(q)).collect();
        match m.as_slice() {
            [one] => Ok(one),
            [] if self.accounts.is_empty() => bail!("no Epic account is signed in (quiver epic login)"),
            [] => bail!("no Epic account matches '{q}' (signed in: {})", self.accounts.iter().map(|a| a.display_name.as_str()).collect::<Vec<_>>().join(", ")),
            _ => bail!("'{q}' matches more than one Epic account"),
        }
    }

    /// Accounts for front ends, default first, then in sign-in order.
    pub fn list(&self) -> Vec<EpicAccount> {
        let default = self.default_id();
        let mut v: Vec<EpicAccount> = self
            .accounts
            .iter()
            .map(|a| EpicAccount {
                account_id: a.account_id.clone(),
                display_name: a.display_name.clone(),
                default: Some(a.account_id.as_str()) == default,
                state: if a.needs_sign_in { AccountState::NeedsSignIn } else { AccountState::SignedIn },
                ubisoft: a.ubisoft.clone(),
            })
            .collect();
        v.sort_by_key(|a| !a.default);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, name: &str) -> Entry {
        Entry { account_id: id.into(), display_name: name.into(), ubisoft: None, needs_sign_in: false }
    }

    #[test]
    fn default_add_remove_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ix = Index::load(tmp.path()).unwrap();
        assert!(ix.resolve(None).unwrap_err().to_string().contains("quiver epic login"));
        ix.upsert(e("aaa111", "Alpha"));
        ix.upsert(e("bbb222", "Beta"));
        assert_eq!(ix.default_id(), Some("aaa111"));
        assert_eq!(ix.resolve(Some("beta")).unwrap().account_id, "bbb222");
        assert_eq!(ix.resolve(Some("bb")).unwrap().account_id, "bbb222");
        assert!(ix.resolve(Some("zzz")).unwrap_err().to_string().contains("Alpha, Beta"));
        ix.default = Some("bbb222".into());
        assert_eq!(ix.list().iter().map(|a| (a.display_name.as_str(), a.default)).collect::<Vec<_>>(), [("Beta", true), ("Alpha", false)]);
        // Signing in again clears "needs sign-in" and keeps the default.
        ix.get_mut("aaa111").unwrap().needs_sign_in = true;
        assert_eq!(ix.list()[1].state, AccountState::NeedsSignIn);
        ix.upsert(e("aaa111", "Alpha2"));
        assert_eq!((ix.list()[1].state, ix.default_id()), (AccountState::SignedIn, Some("bbb222")));
        ix.save(tmp.path()).unwrap();
        let mut back = Index::load(tmp.path()).unwrap();
        assert_eq!(back, ix);
        back.remove("bbb222");
        assert_eq!(back.default_id(), Some("aaa111"));
    }
}
