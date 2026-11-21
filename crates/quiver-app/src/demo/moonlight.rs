//! The demo machine's Moonlight server (design/REMOTE.md phase 1b): on, Tailscale only, with a
//! TV waiting to pair and two paired clients. `QUIVER_DEMO_GLOBAL="moonlight=off"` starts it
//! off; `moonlight=idle` on with nothing waiting.
//!
//! Answers the `Moonlight*` requests.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use anyhow::{Result, bail};
use chrono::{Duration as Span, Utc};
use quiver_api::moonlight::{MoonlightClient, MoonlightPairing, MoonlightStatus, ports};
use quiver_api::{Event, Request};
use serde_json::{Value, json};

use super::Demo;

static STATE: LazyLock<Mutex<HashMap<usize, MoonlightStatus>>> = LazyLock::new(Mutex::default);

const TAILNET: [&str; 2] = ["100.101.12.7", "fd7a:115c:a1e0::7"];

fn seed() -> MoonlightStatus {
    let flag = crate::ui::global::demo_flag("moonlight").unwrap_or_default();
    let on = flag != "off";
    let client = |name: &str, fp: &str, addr: &str, days: i64| MoonlightClient {
        name: name.into(),
        address: addr.into(),
        fingerprint: fp.into(),
        paired_at: Utc::now() - Span::days(days),
    };
    MoonlightStatus {
        enabled: on,
        bind: vec!["tailscale0".into()],
        listening: if on { TAILNET.iter().map(|s| s.to_string()).collect() } else { Vec::new() },
        error: None,
        ports: ports(quiver_api::moonlight::DEFAULT_PORT),
        codecs: ["h264", "hevc", "hevc10", "av1", "av1-10"].map(String::from).to_vec(),
        mdns: true,
        advertised_on: Vec::new(),
        name: "laptop".into(),
        pending: (on && flag != "idle").then(|| MoonlightPairing { device_name: "Living-room TV".into(), address: "100.88.4.20".into() }),
        clients: vec![
            client("Steam Deck", "9c1f4e2ab07d33586e1c90f4b2d7a815c3e60f9d2a4b71e85c09d3f6a1b2c4d8", "100.71.3.9", 12),
            client("Pixel 8", "41d0b8e7c2a95f1306e4d7b9a2c8f05e1b3d6a9c7e2f4b80a5c1d3e6f9b2a7c4", "fd7a:115c:a1e0::2f", 2),
        ],
        stream: None,
    }
}

/// Answer a Moonlight request, or hand it back.
pub(super) fn handle(demo: &Demo, req: Request) -> Result<std::result::Result<Value, Request>> {
    if !matches!(
        req,
        Request::MoonlightStatus
            | Request::MoonlightEnable { .. }
            | Request::MoonlightDisable
            | Request::MoonlightPin { .. }
            | Request::MoonlightClients
            | Request::MoonlightUnpair { .. }
    ) {
        return Ok(Err(req));
    }
    let key = demo as *const Demo as usize;
    let mut all = STATE.lock().unwrap();
    let s = all.entry(key).or_insert_with(seed);
    let out = match req {
        Request::MoonlightStatus => json!(s),
        Request::MoonlightEnable { bind, .. } => {
            if let Some(b) = bind {
                s.bind = b;
            }
            s.enabled = true;
            s.listening = if s.bind.is_empty() { vec!["0.0.0.0".into(), "::".into()] } else { TAILNET.iter().map(|a| a.to_string()).collect() };
            s.advertised_on = if s.bind.is_empty() { vec!["enp5s0".into()] } else { Vec::new() };
            demo.emit(Event::MoonlightChanged);
            json!(s)
        }
        Request::MoonlightDisable => {
            s.enabled = false;
            s.listening.clear();
            s.advertised_on.clear();
            s.pending = None;
            demo.emit(Event::MoonlightChanged);
            Value::Null
        }
        Request::MoonlightPin { pin } => {
            let Some(p) = s.pending.take() else { bail!("no Moonlight client is waiting to pair") };
            if pin != "1234" {
                s.pending = Some(p);
                bail!("pairing failed: the PIN didn't match (the demo's PIN is 1234)");
            }
            let c = MoonlightClient { name: p.device_name.clone(), address: p.address, fingerprint: "e2b7a90c41d6f3851a0c7e9d2b4f6a8c1e3d5b7f9a0c2e4d6b8f1a3c5e7d9b0f".into(), paired_at: Utc::now() };
            s.clients.push(c.clone());
            demo.emit(Event::MoonlightPaired { name: c.name.clone(), fingerprint: c.fingerprint.clone() });
            demo.emit(Event::MoonlightChanged);
            json!(c)
        }
        Request::MoonlightClients => json!(s.clients),
        Request::MoonlightUnpair { fingerprint } => {
            let Some(i) = s.clients.iter().position(|c| c.fingerprint.starts_with(&fingerprint) || c.name == fingerprint) else {
                bail!("no paired Moonlight client {fingerprint}")
            };
            let c = s.clients.remove(i);
            demo.emit(Event::MoonlightChanged);
            json!(c)
        }
        other => return Ok(Err(other)),
    };
    Ok(Ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn demo_moonlight() {
        let (demo, _) = Demo::start();
        let s: MoonlightStatus = serde_json::from_value(demo.handle(Request::MoonlightStatus).unwrap()).unwrap();
        assert!(s.enabled && s.pending.is_some() && s.clients.len() == 2);
        assert!(demo.handle(Request::MoonlightPin { pin: "0000".into() }).is_err());
        let c: MoonlightClient = serde_json::from_value(demo.handle(Request::MoonlightPin { pin: "1234".into() }).unwrap()).unwrap();
        assert_eq!(c.name, "Living-room TV");
        demo.handle(Request::MoonlightUnpair { fingerprint: "9c1f".into() }).unwrap();
        let s: MoonlightStatus = serde_json::from_value(demo.handle(Request::MoonlightStatus).unwrap()).unwrap();
        assert_eq!((s.pending.is_none(), s.clients.len()), (true, 2));
    }
}
