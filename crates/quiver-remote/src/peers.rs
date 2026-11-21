//! Paired machines, in `<data>/remote/peers.json` (design/REMOTE.md §3).
//!
//! quiverd writes the file; the app and the CLI read it to connect to a paired machine
//! themselves ([`Machine`]). Writes go to a temporary file renamed into place, so a reader
//! never sees half of one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identity::{Pin, write_atomic};

/// What a peer is to this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// It paired with this machine and may use it: its connections to this machine's
    /// listener are served (as the person, `source: remote:<name>`).
    Client,
    /// This machine paired with it, and connects to it (the library, streams).
    Server,
}

/// One paired machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    /// Its id ([`Pin::id`]).
    pub id: String,
    /// Its name as it gave it at pairing, or as renamed here.
    pub name: String,
    /// Its key's pin: every connection must present that key.
    pub pin: Pin,
    pub role: Role,
    /// Where it was reached (`host:port`), for servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// Its network card's MAC, learned at pairing, for Wake-on-LAN (servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub paired_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

impl Peer {
    /// Where to connect to it, for a server with a known address.
    pub fn machine(&self) -> Option<Machine> {
        (self.role == Role::Server).then_some(())?;
        Some(Machine { id: self.id.clone(), name: self.name.clone(), addr: self.addr.clone()?, pin: self.pin })
    }
}

/// A paired machine to connect to: `quiver_api::Client::machine`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub pin: Pin,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    peers: Vec<Peer>,
}

/// The peers file, loaded.
#[derive(Debug, Clone)]
pub struct Peers {
    path: PathBuf,
    list: Vec<Peer>,
}

impl Peers {
    /// `peers.json` in the remote dir; empty when there is none yet.
    pub fn load(dir: &Path) -> Result<Peers> {
        let path = dir.join("peers.json");
        let list = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<File>(&b).with_context(|| format!("reading {}", path.display()))?.peers,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Peers { path, list })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            crate::identity::create_private_dir(dir)?;
        }
        let file = File { peers: self.list.clone() };
        write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?, 0o600)
    }

    pub fn list(&self) -> &[Peer] {
        &self.list
    }

    /// The peer with this key in this role.
    pub fn by_pin(&self, pin: &Pin, role: Role) -> Option<&Peer> {
        self.list.iter().find(|p| p.pin == *pin && p.role == role)
    }

    pub fn by_pin_mut(&mut self, pin: &Pin, role: Role) -> Option<&mut Peer> {
        self.list.iter_mut().find(|p| p.pin == *pin && p.role == role)
    }

    /// Add a newly paired peer, replacing an earlier pairing with the same key and role.
    pub fn upsert(&mut self, peer: Peer) {
        self.list.retain(|p| !(p.pin == peer.pin && p.role == peer.role));
        self.list.push(peer);
    }

    /// Peers matching `query`: an id, a unique id prefix, or a name (any case). In `role`
    /// when given. Several for a name two peers share.
    pub fn find(&self, query: &str, role: Option<Role>) -> Vec<&Peer> {
        let q = query.trim();
        let in_role = |p: &&Peer| role.is_none_or(|r| p.role == r);
        let exact: Vec<&Peer> = self.list.iter().filter(in_role).filter(|p| p.id == q).collect();
        if !exact.is_empty() {
            return exact;
        }
        let named: Vec<&Peer> = self.list.iter().filter(in_role).filter(|p| p.name.eq_ignore_ascii_case(q)).collect();
        if !named.is_empty() {
            return named;
        }
        if q.len() >= 4 {
            return self.list.iter().filter(in_role).filter(|p| p.id.starts_with(&q.to_ascii_lowercase())).collect();
        }
        Vec::new()
    }

    /// The one peer `query` names ([`Self::find`]), or an error saying why not.
    pub fn one(&self, query: &str, role: Option<Role>) -> Result<&Peer> {
        match self.find(query, role).as_slice() {
            [p] => Ok(p),
            [] => bail!("no paired machine '{query}' (see `quiver remote peers`)"),
            many => bail!(
                "'{query}' names {} machines ({}); use an id",
                many.len(),
                many.iter().map(|p| format!("{} {}", p.name, p.id)).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Remove every peer with this id (both roles). Returns what was removed.
    pub fn remove(&mut self, id: &str) -> Vec<Peer> {
        let (gone, keep): (Vec<Peer>, Vec<Peer>) = std::mem::take(&mut self.list).into_iter().partition(|p| p.id == id);
        self.list = keep;
        gone
    }

    /// Rename the peers with this id (a local label; the peer's own name is its business).
    pub fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        let name = crate::identity::clean_name(name)?;
        let mut any = false;
        for p in self.list.iter_mut().filter(|p| p.id == id) {
            p.name = name.clone();
            any = true;
        }
        if !any {
            bail!("no paired machine {id}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id_byte: u8, name: &str, role: Role) -> Peer {
        let pin = Pin([id_byte; 32]);
        Peer { id: pin.id(), name: name.into(), pin, role, addr: Some("10.0.0.2:47990".into()), mac: None, paired_at: Utc::now(), last_seen: None }
    }

    #[test]
    fn find_by_id_prefix_and_name() {
        let t = tempfile::tempdir().unwrap();
        let mut p = Peers::load(t.path()).unwrap();
        assert!(p.list().is_empty());
        p.upsert(peer(1, "Tower", Role::Server));
        p.upsert(peer(2, "laptop", Role::Client));
        p.upsert(peer(1, "Tower", Role::Server)); // paired again: replaced
        p.save().unwrap();
        let p = Peers::load(t.path()).unwrap();
        assert_eq!(p.list().len(), 2);
        assert_eq!(p.one("tower", None).unwrap().id, Pin([1; 32]).id());
        assert_eq!(p.one("0101", None).unwrap().name, "Tower");
        assert!(p.one("laptop", Some(Role::Server)).is_err());
        assert!(p.one("01", None).is_err(), "prefixes shorter than 4 never match");
        assert!(p.one("Tower", None).unwrap().machine().is_some());
        assert!(p.one("laptop", None).unwrap().machine().is_none(), "a client isn't connected to");
    }

    #[test]
    fn remove_and_rename() {
        let t = tempfile::tempdir().unwrap();
        let mut p = Peers::load(t.path()).unwrap();
        p.upsert(peer(1, "a", Role::Server));
        p.upsert(peer(1, "a", Role::Client));
        let id = Pin([1; 32]).id();
        p.rename(&id, "Tower").unwrap();
        assert!(p.list().iter().all(|x| x.name == "Tower"));
        assert_eq!(p.remove(&id).len(), 2);
        assert!(p.rename(&id, "x").is_err());
    }
}
