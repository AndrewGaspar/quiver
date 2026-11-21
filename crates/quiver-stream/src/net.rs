//! The real transport behind a [`StreamLink`]: `quiver_stream_net::ClientSession`, mapped as
//! [`crate::link`] describes, for `quiver-stream connect`.
//!
//! [`connect`] returns at once with the link, so the window opens and shows "Connecting…"
//! while the session says hello in the background; a host that never answers arrives as
//! [`ControlIn::Closed`]. A few tasks on a small tokio runtime move things across:
//!
//! - `recv_video()` → [`VideoIn::Frame`] / [`VideoIn::Lost`] (the decode thread sends the
//!   `LostFrames`);
//! - `recv_audio()` → [`AudioIn::Packet`]; `recv_control()` → [`ControlIn::Control`];
//! - `take_stats()` and the clock offset once a second → [`ControlIn::Transport`];
//! - the session ending → [`ControlIn::Closed`];
//! - [`Outgoing::Input`] → `send_input_at`, [`Outgoing::Control`] → `send_control`.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossbeam_channel::Receiver;
use quiver_stream_net::{ClientOpts, ClientSession, LinkState, SessionKeys, VideoEvent};
use quiver_stream_proto::{StreamPrefs, StreamTarget};
use serde::Deserialize;

use crate::link::{AudioIn, ControlIn, HostEnd, Outgoing, StreamLink, TransportStats, VideoIn, loopback};

/// Where the stream's host is and the session's keys: `StartStream`'s ticket.
#[derive(Clone, Deserialize)]
pub struct Ticket {
    pub addr: SocketAddr,
    pub session_id: u64,
    /// 64 hex digits.
    pub key: String,
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket").field("addr", &self.addr).field("session_id", &self.session_id).field("key", &"<redacted>").finish()
    }
}

impl Ticket {
    pub fn keys(&self) -> Result<SessionKeys> {
        let k = self.key.trim();
        if k.len() != 64 || !k.is_ascii() {
            bail!("the ticket's key isn't 64 hex digits");
        }
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = u8::from_str_radix(&k[i * 2..i * 2 + 2], 16).context("the ticket's key isn't hex")?;
        }
        Ok(SessionKeys::new(self.session_id, key))
    }
}

/// What `quiver-stream connect` reads on stdin: `StartStream`'s reply (`{ticket, host_name}`),
/// optionally with the target and prefs the stream was started with.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectSpec {
    pub ticket: Ticket,
    #[serde(default)]
    pub host_name: Option<String>,
    #[serde(default)]
    pub target: Option<StreamTarget>,
    #[serde(default)]
    pub prefs: Option<StreamPrefs>,
}

impl ConnectSpec {
    /// From the first line (or the whole) of `text`.
    pub fn parse(text: &str) -> Result<ConnectSpec> {
        serde_json::from_str(text.trim()).context("stdin isn't a stream ticket ({\"ticket\": {\"addr\", \"session_id\", \"key\"}, …})")
    }
}

/// The running adapter. Dropping it ends the session.
pub struct NetLink {
    rt: tokio::runtime::Runtime,
    session: std::sync::Arc<std::sync::Mutex<Option<ClientSession>>>,
}

impl NetLink {
    /// Let the last messages (the Bye) out, then end the session.
    pub fn close(self) {
        let s = self.session.lock().unwrap().take();
        if let Some(s) = s {
            std::thread::sleep(Duration::from_millis(150));
            s.close("the client closed");
            std::thread::sleep(Duration::from_millis(50));
        }
        self.rt.shutdown_timeout(Duration::from_millis(200));
    }
}

/// Start connecting to the ticket's host; the link is live at once.
pub fn connect(ticket: &Ticket, opts: ClientOpts) -> Result<(StreamLink, NetLink)> {
    let keys = ticket.keys()?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("stream-net").enable_all().build()?;
    let (link, host) = loopback();
    let session = std::sync::Arc::new(std::sync::Mutex::new(None));
    let addr = ticket.addr;
    let slot = session.clone();
    rt.spawn(async move {
        match ClientSession::connect(addr, keys, opts).await {
            Ok(s) => {
                tracing::info!("connected to {addr}");
                *slot.lock().unwrap() = Some(s.clone());
                pump(s, host).await;
            }
            Err(e) => {
                let _ = host.control.send(ControlIn::Closed { reason: format!("couldn't reach the stream host at {addr}: {e}") });
            }
        }
    });
    Ok((link, NetLink { rt, session }))
}

async fn pump(s: ClientSession, host: HostEnd) {
    let video = {
        let (s, tx) = (s.clone(), host.video.clone());
        tokio::spawn(async move {
            while let Some(v) = s.recv_video().await {
                let m = match v {
                    VideoEvent::Frame(f) => VideoIn::Frame(f),
                    VideoEvent::Gap { since_seq, through_seq } => VideoIn::Lost { first: since_seq, last: through_seq },
                };
                if tx.send(m).is_err() {
                    return;
                }
            }
        })
    };
    let audio = {
        let (s, tx) = (s.clone(), host.audio.clone());
        tokio::spawn(async move {
            while let Some(p) = s.recv_audio().await {
                if tx.send(AudioIn::Packet(p)).is_err() {
                    return;
                }
            }
        })
    };
    let control = {
        let (s, tx) = (s.clone(), host.control.clone());
        tokio::spawn(async move {
            while let Some(c) = s.recv_control().await {
                if tx.send(ControlIn::Control(c)).is_err() {
                    return;
                }
            }
        })
    };
    // Outgoing: a blocking receiver, so its own thread.
    {
        let (s, rx): (ClientSession, Receiver<Outgoing>) = (s.clone(), host.from_client.clone());
        std::thread::Builder::new()
            .name("stream-out".into())
            .spawn(move || {
                while let Ok(m) = rx.recv() {
                    let r = match m {
                        Outgoing::Input(st) => s.send_input_at(st.event, st.at),
                        Outgoing::Control(c) => s.send_control(c),
                    };
                    if r.is_err() {
                        return;
                    }
                }
            })
            .ok();
    }
    let mut link = s.link_state();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let reason = loop {
        tokio::select! {
            _ = tick.tick() => {
                let Ok(t) = s.take_stats().await else { break "the session ended".to_owned() };
                let stats = TransportStats {
                    packets_received: t.packets_received,
                    packets_lost: t.packets_lost,
                    packets_recovered: t.packets_recovered,
                    rtt_us: (t.rtt_us > 0).then_some(t.rtt_us),
                    jitter_us: (t.jitter_us > 0).then_some(t.jitter_us),
                    clock_offset_us: s.clock_offset(),
                    estimated_kbps: t.estimated_kbps,
                    video_bytes: 0,
                };
                if host.control.send(ControlIn::Transport(stats)).is_err() {
                    break "the window closed".to_owned();
                }
            }
            r = link.changed() => {
                if r.is_err() {
                    break "the session ended".to_owned();
                }
                if let LinkState::Closed(why) = &*link.borrow_and_update() {
                    break why.clone();
                }
            }
        }
    };
    let _ = host.control.send(ControlIn::Closed { reason: format!("the stream ended: {reason}") });
    video.abort();
    audio.abort();
    control.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_parse_and_hide_their_key() {
        let key = "ab".repeat(32);
        let text = format!(r#"{{"ticket":{{"addr":"10.0.0.2:47998","session_id":7,"key":"{key}"}},"host_name":"Tower","target":"TestPattern"}}"#);
        let spec = ConnectSpec::parse(&text).unwrap();
        assert_eq!(spec.ticket.addr.port(), 47998);
        assert_eq!(spec.target, Some(StreamTarget::TestPattern));
        let k = spec.ticket.keys().unwrap();
        assert_eq!((k.session_id, k.key[0]), (7, 0xab));
        assert!(!format!("{:?}", spec.ticket).contains(&key));
        let bad = ConnectSpec::parse(r#"{"ticket":{"addr":"10.0.0.2:1","session_id":1,"key":"zz"}}"#).unwrap();
        assert!(bad.ticket.keys().is_err());
        assert!(ConnectSpec::parse("nope").is_err());
    }
}
