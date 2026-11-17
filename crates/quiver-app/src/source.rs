//! Where the app's data comes from: the `quiverd` daemon through `quiver-api`, or, with
//! `QUIVER_DEMO=1`, built-in fixtures that answer the same requests. The rest of the app
//! only sees [`Source`], so every screen can be developed and screenshotted in demo mode
//! without a daemon, an account or network access to Ubisoft (art aside).

use std::sync::Arc;

use anyhow::Result;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use quiver_api::{Client, Event, Request, Status};

use crate::demo::Demo;

#[derive(Clone)]
pub enum Source {
    Daemon(Client),
    Demo(Arc<Demo>),
}

/// Demo mode with `QUIVER_DEMO_DAEMON=1`: mods and options requests (and their events) go to
/// the daemon at `XDG_RUNTIME_DIR`, the rest stays fixtures. For checking the options screens
/// against a scratch quiverd (its own XDG dirs; `QUIVER_TEST_SESSION` for a "running" game):
/// it never starts a daemon, so point `XDG_RUNTIME_DIR` at the scratch one.
static DEMO_DAEMON: std::sync::OnceLock<Client> = std::sync::OnceLock::new();

async fn demo_daemon() -> Result<Option<Client>> {
    if !std::env::var("QUIVER_DEMO_DAEMON").is_ok_and(|v| v == "1") {
        return Ok(None);
    }
    let sock = quiver_api::socket_path();
    if std::os::unix::net::UnixStream::connect(&sock).is_err() {
        anyhow::bail!("QUIVER_DEMO_DAEMON: no quiverd listening at {}", sock.display());
    }
    Ok(Some(Client::connect("gui").await?))
}

/// `QUIVER_DEMO=1` (or `true`/`yes`) selects demo data.
pub fn demo_requested() -> bool {
    std::env::var("QUIVER_DEMO").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

impl Source {
    /// Connect and subscribe to events. In demo mode this never touches the daemon socket
    /// and never spawns `quiverd`. The daemon's event stream outlives daemon restarts: the
    /// client reconnects by itself and reports it with `Event::Disconnected` and
    /// `Event::Reconnected`.
    pub async fn connect(demo: bool) -> Result<(Source, Status, mpsc::UnboundedReceiver<Event>)> {
        if demo {
            let (demo, mut events) = Demo::start();
            let source = Source::Demo(demo);
            let status = source.call(Request::Status).await?;
            if let Some(client) = demo_daemon().await? {
                // Mods and options from a (scratch) daemon, everything else from fixtures.
                let mut daemon_events = client.subscribe().await?;
                let (tx, rx) = mpsc::unbounded_channel();
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    while let Some(e) = events.recv().await {
                        if tx.send(e).is_err() {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    while let Some(e) = daemon_events.recv().await {
                        let ours = e.options().is_some()
                            || matches!(e, Event::ModsChanged { .. } | Event::ModChannel { .. } | Event::GameStarted { .. } | Event::GameStopped { .. });
                        if ours && tx2.send(e).is_err() {
                            break;
                        }
                    }
                });
                let _ = DEMO_DAEMON.set(client);
                return Ok((source, status, rx));
            }
            return Ok((source, status, events));
        }
        let client = Client::connect("gui").await?;
        let status: Status = client.call(Request::Status).await?;
        let events = client.subscribe().await?;
        Ok((Source::Daemon(client), status, events))
    }

    pub async fn call<T: DeserializeOwned>(&self, req: Request) -> Result<T> {
        match self {
            Source::Daemon(c) => c.call(req).await,
            Source::Demo(_) if matches!(req, Request::Mods(_) | Request::Options(_)) && DEMO_DAEMON.get().is_some() => {
                DEMO_DAEMON.get().unwrap().call(req).await
            }
            Source::Demo(d) => {
                let v = d.handle(req)?;
                Ok(serde_json::from_value(v)?)
            }
        }
    }
}
