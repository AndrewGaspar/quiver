//! One shared, authenticated demux connection per process.
//!
//! Ubisoft allows a single live demux login per account: authenticating a second socket
//! closes the first (`ConnectionClosedPush` with `Connection_MultipleLogin`). Everything
//! long-lived (the daemon, the broker, installs, launches) must therefore share one socket
//! and multiplex its services over it.

use std::sync::Arc;

use anyhow::Result;

use crate::auth::{Auth, Session};
use crate::demux::Demux;

pub struct Online {
    pub auth: Auth,
    demux: tokio::sync::Mutex<Option<Demux>>,
}

impl Online {
    pub fn new(auth: Auth) -> Arc<Self> {
        Arc::new(Self { auth, demux: tokio::sync::Mutex::new(None) })
    }

    /// The current session, refreshed through the remember-me ticket when close to expiry.
    pub async fn session(&self) -> Result<Session> {
        crate::store::current(&self.auth).await
    }

    /// The shared demux socket, (re)connecting and authenticating if needed.
    pub async fn demux(&self) -> Result<Demux> {
        let mut slot = self.demux.lock().await;
        if let Some(d) = slot.as_ref().filter(|d| d.is_alive()) {
            return Ok(d.clone());
        }
        let session = self.session().await?;
        let d = Demux::connect().await?;
        d.authenticate(&session.ticket).await?;
        tracing::info!("demux connected");
        *slot = Some(d.clone());
        Ok(d)
    }

    /// Drop the shared socket (after sign-out or a new sign-in).
    pub async fn reset(&self) {
        *self.demux.lock().await = None;
    }
}
