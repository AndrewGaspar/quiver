//! Where the app's data comes from: the `quiverd` daemon through `ubi-api`, or, with
//! `QUIVER_DEMO=1`, built-in fixtures that answer the same requests. The rest of the app
//! only sees [`Source`], so every screen can be developed and screenshotted in demo mode
//! without a daemon, an account or network access to Ubisoft (art aside).

use std::sync::Arc;

use anyhow::Result;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use ubi_api::{Client, Event, Request, Status};

use crate::demo::Demo;

#[derive(Clone)]
pub enum Source {
    Daemon(Client),
    Demo(Arc<Demo>),
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
            let (demo, events) = Demo::start();
            let source = Source::Demo(demo);
            let status = source.call(Request::Status).await?;
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
            Source::Demo(d) => {
                let v = d.handle(req)?;
                Ok(serde_json::from_value(v)?)
            }
        }
    }
}
