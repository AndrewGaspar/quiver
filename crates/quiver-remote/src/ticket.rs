//! Stream tickets: what `StartStream` hands the client so it can reach the stream host
//! (design/REMOTE.md §2.1). The media transport has no handshake of its own; its keys come
//! from here, over the control plane's TLS.

use std::fmt;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Where a stream's host listens and the session's key. The transport's `SessionKeys`
/// (`quiver-stream-net`) is `{session_id, key}`. `Debug` leaves the key out; the key goes
/// over the wire as hex and must never be logged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamTicket {
    /// The host's UDP address.
    pub addr: SocketAddr,
    pub session_id: u64,
    #[serde(with = "hex_key")]
    pub key: [u8; 32],
}

impl StreamTicket {
    /// A ticket for `addr` with a fresh random session id and key.
    pub fn new(addr: SocketAddr) -> StreamTicket {
        use ring::rand::SecureRandom as _;
        let rng = ring::rand::SystemRandom::new();
        let mut key = [0u8; 32];
        let mut id = [0u8; 8];
        rng.fill(&mut key).expect("system randomness");
        rng.fill(&mut id).expect("system randomness");
        StreamTicket { addr, session_id: u64::from_le_bytes(id), key }
    }
}

impl fmt::Debug for StreamTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamTicket").field("addr", &self.addr).field("session_id", &self.session_id).field("key", &"<redacted>").finish()
    }
}

mod hex_key {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(k: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(k))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).ok().and_then(|v| v.try_into().ok()).ok_or_else(|| serde::de::Error::custom("key: expected 64 hex digits"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_key() {
        let t = StreamTicket::new("10.0.0.2:47998".parse().unwrap());
        let dbg = format!("{t:?}");
        assert!(!dbg.contains(&hex::encode(t.key)) && dbg.contains("redacted") && dbg.contains("47998"), "{dbg}");
        let back: StreamTicket = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
        assert_ne!(StreamTicket::new(t.addr).key, t.key);
    }
}
