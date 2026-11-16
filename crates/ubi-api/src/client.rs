//! Async client for `ubid`. Starts the daemon if it isn't running.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};

use crate::{Envelope, Event, Reply, Request, socket_path};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>;

#[derive(Clone)]
pub struct Client {
    writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    pending: Pending,
    events: Arc<Mutex<Option<mpsc::UnboundedSender<Event>>>>,
    next_id: Arc<AtomicU64>,
    source: String,
}

impl Client {
    /// Connect to the daemon, starting it if needed. `source` labels this client in the
    /// daemon's activity log and settings history (e.g. "gui", "cli", "agent").
    pub async fn connect(source: &str) -> Result<Self> {
        let path = socket_path();
        let stream = match UnixStream::connect(&path).await {
            Ok(s) => s,
            Err(_) => {
                spawn_daemon()?;
                let mut last = None;
                let mut stream = None;
                for _ in 0..100 {
                    match UnixStream::connect(&path).await {
                        Ok(s) => {
                            stream = Some(s);
                            break;
                        }
                        Err(e) => last = Some(e),
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                stream.ok_or_else(|| anyhow!("quiverd did not start ({last:?}); see {}", daemon_log().display()))?
            }
        };
        let (r, w) = stream.into_split();
        let client = Self {
            writer: Arc::new(tokio::sync::Mutex::new(w)),
            pending: Arc::default(),
            events: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            source: source.to_owned(),
        };
        let pending = client.pending.clone();
        let events = client.events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Reply>(&line) {
                    Ok(Reply::Ok { id, result }) => {
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(Ok(result));
                        }
                    }
                    Ok(Reply::Err { id, error }) => {
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(Err(error));
                        }
                    }
                    Ok(Reply::Event(e)) => {
                        if let Some(tx) = events.lock().unwrap().as_ref() {
                            let _ = tx.send(e);
                        }
                    }
                    Err(_) => {}
                }
            }
            // Connection closed: fail everything still waiting.
            pending.lock().unwrap().clear();
            events.lock().unwrap().take();
        });
        Ok(client)
    }

    /// Send a request and decode its result.
    pub async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&Envelope { id, source: self.source.clone(), request })?;
        line.push(b'\n');
        self.writer.lock().await.write_all(&line).await.context("quiverd connection lost")?;
        let value = rx.await.map_err(|_| anyhow!("quiverd connection lost"))?.map_err(|e| anyhow!(e))?;
        serde_json::from_value(value).context("unexpected reply from quiverd")
    }

    /// Receive daemon events on this connection from now on.
    pub async fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.events.lock().unwrap() = Some(tx);
        let () = self.call(Request::Subscribe).await?;
        Ok(rx)
    }
}

pub fn daemon_log() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join("quiver/quiverd.log")
}

/// Start `quiverd` in the background: next to this executable, else from PATH.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("quiverd")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("quiverd"));
    let log = daemon_log();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.stdin(std::process::Stdio::null()).stdout(out.try_clone()?).stderr(out);
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the client's terminal closing
    }
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(e) => bail!("could not start {}: {e}", exe.display()),
    }
}
