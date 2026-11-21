//! Demo machines (design/REMOTE.md §5): this PC ("laptop") is paired with "Tower", which is
//! up and has its own library and download, and with "Office", which is asleep. Tower is
//! another [`Demo`] (its library is [`tower_state`]); Office never answers.
//!
//! Answers the `Remote*` requests and `StartStream` for both demo machines.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use anyhow::{Result, bail};
use chrono::{Duration as Span, Utc};
use quiver_api::remote::{Discovered, PairCode, PeerInfo, PeerState, RemoteStatus, Role};
use quiver_api::{ActivityEntry, Event, Job, JobKind, Request};
use serde_json::{Value, json};

use super::{Demo, FIXTURES, INSTALLING, QUEUED, State, game_for};

/// The demo machines' ids.
pub const TOWER: &str = "7f3a9c2e51d04b86";
pub const OFFICE: &str = "c41e08b7a2f95d13";
const LAPTOP: &str = "2b8e5d0c9a7f6431";
/// Tower's own download.
const TOWER_JOB: u64 = 7;

struct Remote {
    status: RemoteStatus,
}

static REMOTE: LazyLock<Mutex<HashMap<usize, Remote>>> = LazyLock::new(Mutex::default);

fn peer(id: &str, name: &str, role: Role, state: PeerState, mac: bool, days: i64) -> PeerInfo {
    PeerInfo {
        id: id.into(),
        name: name.into(),
        role,
        addr: (role == Role::Server).then(|| format!("{}.lan:47990", name.to_lowercase())),
        mac: mac.then(|| "3c:7c:3f:1e:a2:90".into()),
        paired_at: Utc::now() - Span::days(30),
        last_seen: Some(Utc::now() - Span::days(days)),
        state,
        connections: usize::from(role == Role::Client && state == PeerState::Online),
    }
}

fn seed(machine: Option<&str>) -> Remote {
    let status = match machine {
        None => RemoteStatus {
            name: "laptop".into(),
            id: LAPTOP.into(),
            enabled: false,
            listen: quiver_remote::DEFAULT_LISTEN.into(),
            peers: vec![
                peer(TOWER, "Tower", Role::Server, PeerState::Online, true, 0),
                peer(OFFICE, "Office", Role::Server, PeerState::Asleep, true, 3),
            ],
            local_only: quiver_api::remote::local_only_list(),
            ..Default::default()
        },
        Some(name) => RemoteStatus {
            name: name.into(),
            id: TOWER.into(),
            enabled: true,
            listen: quiver_remote::DEFAULT_LISTEN.into(),
            listening: Some(quiver_remote::DEFAULT_LISTEN.into()),
            discoverable: true,
            peers: vec![peer(LAPTOP, "laptop", Role::Client, PeerState::Online, false, 0)],
            local_only: quiver_api::remote::local_only_list(),
            ..Default::default()
        },
    };
    Remote { status }
}

/// Answer a remote-play request, or hand it back.
pub(super) fn handle(demo: &Demo, st: &State, req: Request) -> Result<std::result::Result<Value, Request>> {
    // Tower's library as built (the stores' demo adds this PC's Steam installs to it).
    if demo.machine.is_some() && matches!(req, Request::Library { .. }) {
        return Ok(Ok(json!(st.entries)));
    }
    // Tower's queue is its one download (the fixtures' queue is this PC's).
    if demo.machine.is_some() && matches!(req, Request::Queue) {
        let q: Vec<Value> = st
            .jobs
            .iter()
            .filter(|j| j.id == TOWER_JOB && !j.done)
            .enumerate()
            .map(|(i, j)| {
                json!({
                    "product_id": j.game.as_ubisoft().unwrap_or(0), "name": j.name, "kind": "install",
                    "state": if i == 0 { "downloading" } else { "queued" }, "position": i + 1, "job_id": j.id,
                    "source": "remote:laptop", "added": Utc::now() - Span::minutes(12),
                    "done_bytes": j.downloaded, "total_bytes": j.total, "speed": 61_000_000u64
                })
            })
            .collect();
        return Ok(Ok(Value::Array(q)));
    }
    let key = demo as *const Demo as usize;
    let mut all = REMOTE.lock().unwrap();
    let r = all.entry(key).or_insert_with(|| seed(demo.machine.as_deref()));
    let out = match req {
        Request::RemoteStatus => json!(r.status),
        Request::RemotePeers => json!(r.status.peers),
        Request::RemoteEnable { listen } => {
            r.status.enabled = true;
            r.status.listening = Some(listen.unwrap_or_else(|| r.status.listen.clone()));
            r.status.discoverable = true;
            demo.emit(Event::RemoteChanged);
            json!(r.status)
        }
        Request::RemoteDisable => {
            r.status.enabled = false;
            r.status.listening = None;
            r.status.discoverable = false;
            demo.emit(Event::RemoteChanged);
            Value::Null
        }
        Request::RemotePairBegin => {
            if r.status.listening.is_none() {
                bail!("remote is off on this machine; turn it on first (`quiver remote enable`)");
            }
            let expires_at = Utc::now() + Span::minutes(2);
            r.status.pairing = Some(quiver_api::remote::PairingState { expires_at, tries_left: 3 });
            json!(PairCode { code: "418207".into(), name: r.status.name.clone(), expires_at, addrs: vec!["192.168.1.24".into(), "100.88.12.7".into()] })
        }
        Request::RemotePairEnd => {
            r.status.pairing = None;
            Value::Null
        }
        Request::RemoteDiscover { .. } => json!([
            Discovered { name: "Tower".into(), id: TOWER.into(), addrs: vec!["192.168.1.40:47990".into()] },
            Discovered { name: "Den TV".into(), id: "91d3e07a4c5b2f68".into(), addrs: vec!["192.168.1.57:47990".into()] },
        ]),
        Request::RemotePairWith { addr, code } => {
            if code.trim() != "418207" {
                bail!("wrong code for {addr} (three wrong codes end its pairing window)");
            }
            let p = peer("91d3e07a4c5b2f68", "Den TV", Role::Server, PeerState::Online, true, 0);
            r.status.peers.push(p.clone());
            demo.emit(Event::RemoteChanged);
            json!(p)
        }
        Request::RemoteRevoke { id } => {
            let gone: Vec<PeerInfo> = r.status.peers.iter().filter(|p| p.id == id || p.name.eq_ignore_ascii_case(&id)).cloned().collect();
            if gone.is_empty() {
                bail!("no paired machine '{id}'");
            }
            r.status.peers.retain(|p| !gone.iter().any(|g| g.id == p.id));
            demo.emit(Event::RemoteChanged);
            json!(gone)
        }
        Request::RemoteRename { id: None, name } => {
            r.status.name = quiver_remote::identity::clean_name(&name)?;
            demo.emit(Event::RemoteChanged);
            json!(r.status)
        }
        Request::RemoteRename { id: Some(id), name } => {
            let name = quiver_remote::identity::clean_name(&name)?;
            for p in r.status.peers.iter_mut().filter(|p| p.id == id) {
                p.name = name.clone();
            }
            demo.emit(Event::RemoteChanged);
            json!(r.status.peers)
        }
        Request::RemoteWake { id } => {
            let Some(p) = r.status.peers.iter().find(|p| p.id == id) else { bail!("no paired machine '{id}'") };
            if p.mac.is_none() {
                bail!("{}'s MAC isn't known", p.name);
            }
            Value::Null
        }
        Request::StartStream { .. } => bail!("streaming isn't implemented yet"),
        other => return Ok(Err(other)),
    };
    Ok(Ok(out))
}

/// Tower's library and queue: Origins and AC Mirage installed (Mirage is still downloading
/// on this PC), one game only Tower has, and a download of its own.
pub(super) fn tower_state() -> State {
    let mut st = super::build_state();
    let here: Vec<u32> = FIXTURES.iter().map(|f| f.id).chain([INSTALLING, QUEUED]).collect();
    let mut spare = st.entries.iter().filter_map(|e| e.ubisoft_id()).filter(|id| !here.contains(id));
    let only_tower = spare.next().unwrap_or(QUEUED);
    let downloading = spare.next().unwrap_or(QUEUED);
    for e in &mut st.entries {
        let Some(id) = e.ubisoft_id() else {
            // Other stores' games stay on this PC only.
            e.installed = None;
            e.client = None;
            e.steam = None;
            continue;
        };
        e.update_available = false;
        // Steam copies are this PC's.
        e.steam = None;
        e.client = None;
        e.installed = [3539, INSTALLING, only_tower].contains(&id).then(|| game_for(id, &e.name, None));
    }
    let name = |id: u32| st.entries.iter().find(|e| e.ubisoft_id() == Some(id)).map(|e| e.name.clone()).unwrap_or_default();
    st.jobs = vec![Job {
        id: TOWER_JOB,
        kind: JobKind::Install,
        game: quiver_api::GameRef::ubisoft(downloading),
        name: name(downloading),
        status: "Downloading · 38%".into(),
        downloaded: 14_800_000_000,
        total: 38_900_000_000,
        done: false,
        error: None,
        detail: vec![],
    }];
    st.activity = vec![ActivityEntry {
        at: Utc::now() - Span::minutes(12),
        source: "remote:laptop".into(),
        action: "install started".into(),
        game: Some(quiver_api::GameRef::ubisoft(downloading)),
    }];
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::LibraryEntry;
    use quiver_api::downloads::QueueEntry;

    #[tokio::test]
    async fn demo_machines() {
        let (here, _) = Demo::start();
        let s: RemoteStatus = serde_json::from_value(here.handle(Request::RemoteStatus).unwrap()).unwrap();
        assert_eq!((s.name.as_str(), s.enabled), ("laptop", false));
        let peers: Vec<(&str, PeerState)> = s.peers.iter().map(|p| (p.name.as_str(), p.state)).collect();
        assert_eq!(peers, [("Tower", PeerState::Online), ("Office", PeerState::Asleep)]);
        // Pairing needs remote on; a wrong code is refused.
        assert!(here.handle(Request::RemotePairBegin).is_err());
        here.handle(Request::RemoteEnable { listen: None }).unwrap();
        let code: PairCode = serde_json::from_value(here.handle(Request::RemotePairBegin).unwrap()).unwrap();
        assert_eq!(code.code.len(), 6);
        assert!(here.handle(Request::RemotePairWith { addr: "den".into(), code: "000000".into() }).is_err());

        // Tower: its own library and its one download.
        let (tower, _) = Demo::start_remote("Tower");
        let lib: Vec<LibraryEntry> = serde_json::from_value(tower.handle(Request::Library { refresh: false, all_stores: true }).unwrap()).unwrap();
        let installed: Vec<u32> = lib.iter().filter(|e| e.on_disk()).filter_map(|e| e.ubisoft_id()).collect();
        assert!(installed.contains(&3539) && installed.contains(&INSTALLING) && installed.len() == 3, "{installed:?}");
        assert_eq!(lib.iter().filter(|e| e.on_disk()).count(), 3, "nothing else is installed there");
        let q: Vec<QueueEntry> = serde_json::from_value(tower.handle(Request::Queue).unwrap()).unwrap();
        assert_eq!(q.len(), 1);
        assert!(q[0].state.is_active());
        let s: RemoteStatus = serde_json::from_value(tower.handle(Request::RemoteStatus).unwrap()).unwrap();
        assert_eq!((s.name.as_str(), s.listening.is_some()), ("Tower", true));
    }
}
