//! One authenticated demux connection per signed-in account.
//!
//! Ubisoft allows a single live demux login per account: authenticating a second socket
//! closes the first (`ConnectionClosedPush` with `Connection_MultipleLogin`). Everything
//! long-lived (the daemon, the broker, installs, launches) must therefore share one socket
//! per account and multiplex its services over it. [`Accounts`] holds one [`Online`] per
//! signed-in account.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::auth::{Auth, Session};
use crate::demux::Demux;
use crate::store;

pub struct Online {
    pub auth: Auth,
    /// The account this connection belongs to; `None` follows the default account.
    user_id: Option<String>,
    demux: tokio::sync::Mutex<Option<Demux>>,
    /// Serializes session refreshes (each one rotates the remember-me ticket).
    refresh: tokio::sync::Mutex<()>,
}

impl Online {
    /// The default account (whichever it is when used).
    pub fn new(auth: Auth) -> Arc<Self> {
        Self::build(auth, None)
    }

    pub fn for_account(auth: Auth, user_id: &str) -> Arc<Self> {
        Self::build(auth, Some(user_id.to_owned()))
    }

    fn build(auth: Auth, user_id: Option<String>) -> Arc<Self> {
        Arc::new(Self { auth, user_id, demux: tokio::sync::Mutex::new(None), refresh: tokio::sync::Mutex::new(()) })
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// The account's session, refreshed through the remember-me ticket when close to expiry.
    pub async fn session(&self) -> Result<Session> {
        let _guard = self.refresh.lock().await;
        store::current(&self.auth, self.user_id.as_deref()).await
    }

    /// The account's demux socket, (re)connecting and authenticating if needed.
    pub async fn demux(&self) -> Result<Demux> {
        let mut slot = self.demux.lock().await;
        if let Some(d) = slot.as_ref().filter(|d| d.is_alive()) {
            return Ok(d.clone());
        }
        let session = self.session().await?;
        let d = Demux::connect().await?;
        d.authenticate(&session.ticket).await?;
        tracing::info!(account = %session.name, "demux connected");
        *slot = Some(d.clone());
        Ok(d)
    }

    /// Drop the socket (after sign-out or a new sign-in).
    pub async fn reset(&self) {
        *self.demux.lock().await = None;
    }
}

/// One [`Online`] per signed-in account, created on first use.
#[derive(Default)]
pub struct Accounts {
    by_id: Mutex<HashMap<String, Arc<Online>>>,
}

impl Accounts {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The connection for an account (`None`: the default account).
    pub fn get(&self, user_id: Option<&str>) -> Result<Arc<Online>> {
        let id = match user_id {
            Some(id) => id.to_owned(),
            None => store::default_account()?.context("not signed in (run `quiver login`)")?,
        };
        let mut map = self.by_id.lock().unwrap();
        if let Some(o) = map.get(&id) {
            return Ok(o.clone());
        }
        anyhow::ensure!(store::accounts()?.iter().any(|a| a.user_id == id), "account {id} is not signed in");
        let o = Online::for_account(Auth::default(), &id);
        map.insert(id, o.clone());
        Ok(o)
    }

    /// Every signed-in account's connection, default first.
    pub fn all(&self) -> Result<Vec<Arc<Online>>> {
        let default = store::default_account()?;
        let mut ids: Vec<String> = store::accounts()?.into_iter().map(|a| a.user_id).collect();
        ids.sort_by_key(|id| Some(id) != default.as_ref());
        ids.iter().map(|id| self.get(Some(id))).collect()
    }

    /// Forget an account's connection (after sign-out), or reset it (after a new sign-in).
    pub async fn reset(&self, user_id: &str) {
        let o = self.by_id.lock().unwrap().remove(user_id);
        if let Some(o) = o {
            o.reset().await;
        }
    }
}
