//! Where Epic sessions are kept: the desktop's Secret Service keyring, service `quiver`,
//! one entry per account named `epic:<account id>` (design/STORES.md §3.2), next to
//! Ubisoft's `session:<userId>` entries. Never a plain file.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};

pub const SERVICE: &str = "quiver";

/// The keyring entry for an Epic account.
pub fn entry_name(account_id: &str) -> String {
    format!("epic:{account_id}")
}

/// A place secrets live. Calls may block (D-Bus): callers run them off the async threads.
pub trait SecretStore: Send + Sync {
    fn get(&self, entry: &str) -> Result<Option<String>>;
    fn set(&self, entry: &str, value: &str) -> Result<()>;
    /// Deleting an entry that isn't there is fine.
    fn delete(&self, entry: &str) -> Result<()>;
}

/// The Secret Service keyring (gnome-keyring, KWallet).
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, entry: &str) -> Result<Option<String>> {
        match keyring::Entry::new(SERVICE, entry).context("open secret service entry")?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("read the Epic session from the secret service"),
        }
    }

    fn set(&self, entry: &str, value: &str) -> Result<()> {
        keyring::Entry::new(SERVICE, entry).context("open secret service entry")?.set_password(value).context("write the Epic session to the secret service")
    }

    fn delete(&self, entry: &str) -> Result<()> {
        match keyring::Entry::new(SERVICE, entry).context("open secret service entry")?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("delete the Epic session from the secret service"),
        }
    }
}

/// Secrets in memory, for tests and scratch daemons.
#[derive(Default)]
pub struct Memory(Mutex<HashMap<String, String>>);

impl Memory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

impl SecretStore for Memory {
    fn get(&self, entry: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(entry).cloned())
    }

    fn set(&self, entry: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert(entry.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, entry: &str) -> Result<()> {
        self.0.lock().unwrap().remove(entry);
        Ok(())
    }
}
