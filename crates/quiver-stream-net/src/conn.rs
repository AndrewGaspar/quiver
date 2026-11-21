//! What both ends share: sealing and opening datagrams, packing small chunks, and the
//! bodies of the session-level chunks (hello, ping, status, close).

use std::collections::VecDeque;
use std::net::SocketAddr;

use quiver_stream_proto::Micros;

use crate::crypto::{Cipher, Direction, OVERHEAD, PacketKey, ReplayWindow, SessionKeys};
use crate::io::Class;
use crate::wire::{self, CHUNK_HEADER, Kind, Malformed, Reader};

/// One datagram a core wants sent. Its bytes were appended to the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transmit {
    pub dest: SocketAddr,
    pub len: usize,
    pub class: Class,
}

/// The sending half: our key and the next packet number.
pub struct Sealer {
    key: PacketKey,
    next_pn: u64,
}

impl Sealer {
    pub fn new(keys: &SessionKeys, dir: Direction, cipher: Cipher) -> Self {
        Self { key: PacketKey::new(keys, dir, cipher), next_pn: 1 }
    }

    /// Seals one datagram whose plaintext `body` appends to `out`. Returns its packet number
    /// and length.
    pub fn seal(&mut self, out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) -> (u64, usize) {
        let start = out.len();
        let pn = self.next_pn;
        self.next_pn += 1;
        self.key.write_header(out, pn);
        body(out);
        self.key.seal(out, start);
        (pn, out.len() - start)
    }

    pub fn packets_sent(&self) -> u64 {
        self.next_pn - 1
    }
}

/// The receiving half: the peer's key and the replay window.
pub struct Opener {
    key: PacketKey,
    replay: ReplayWindow,
    pub rejected: u64,
    pub replayed: u64,
}

/// A datagram that opened.
pub struct Opened<'a> {
    pub pn: u64,
    /// Its packet number is the highest yet.
    pub newest: bool,
    pub plain: &'a [u8],
}

impl Opener {
    pub fn new(keys: &SessionKeys, dir: Direction, cipher: Cipher) -> Self {
        Self { key: PacketKey::new(keys, dir, cipher), replay: ReplayWindow::default(), rejected: 0, replayed: 0 }
    }

    pub fn open<'a>(&mut self, data: &'a mut [u8]) -> Option<Opened<'a>> {
        // Cheap check first, so a flood of replays costs no decryption.
        if let Some(pn) = crate::crypto::header_pn(data)
            && !self.replay.is_fresh(pn)
        {
            self.replayed += 1;
            return None;
        }
        let Some((pn, plain)) = self.key.open(data) else {
            self.rejected += 1;
            return None;
        };
        let newest = pn > self.replay.top();
        if !self.replay.accept(pn) {
            self.replayed += 1;
            return None;
        }
        Some(Opened { pn, newest, plain })
    }
}

/// Small chunks waiting for a datagram, packed several to one.
#[derive(Debug, Default)]
pub struct SmallChunks {
    queue: VecDeque<Vec<u8>>,
}

impl SmallChunks {
    pub fn push(&mut self, kind: Kind, body: impl FnOnce(&mut wire::Writer<'_>)) {
        let mut c = Vec::new();
        wire::chunk(&mut c, kind, body);
        self.queue.push_back(c);
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Appends whole chunks to `out` while they fit in `budget` bytes. Always takes at
    /// least one, so an oversized chunk can't block the queue.
    pub fn pack(&mut self, out: &mut Vec<u8>, budget: usize) -> usize {
        let mut used = 0;
        while let Some(c) = self.queue.front() {
            if used > 0 && used + c.len() > budget {
                break;
            }
            used += c.len();
            out.extend_from_slice(c);
            self.queue.pop_front();
        }
        used
    }
}

/// The plaintext budget for a datagram of `datagram` bytes.
pub fn plain_budget(datagram: usize) -> usize {
    datagram - OVERHEAD
}

/// Room left for a chunk body after its header.
pub fn body_budget(plain: usize) -> usize {
    plain.saturating_sub(CHUNK_HEADER)
}

// --- session-level chunk bodies -------------------------------------------------------------

pub fn put_ping(w: &mut wire::Writer<'_>, id: u32, sent: Micros) {
    w.u32(id).u64(sent);
}

pub fn get_ping(body: &[u8]) -> Result<(u32, Micros), Malformed> {
    let mut r = Reader::new(body);
    Ok((r.u32()?, r.u64()?))
}

/// A pong: the ping's id and send time, then our receive and send times.
pub fn put_pong(w: &mut wire::Writer<'_>, id: u32, ping_sent: Micros, received: Micros, sent: Micros) {
    w.u32(id).u64(ping_sent).u64(received).u64(sent);
}

pub fn get_pong(body: &[u8]) -> Result<(u32, Micros, Micros, Micros), Malformed> {
    let mut r = Reader::new(body);
    Ok((r.u32()?, r.u64()?, r.u64()?, r.u64()?))
}

pub fn get_u64(body: &[u8]) -> Result<u64, Malformed> {
    Reader::new(body).u64()
}

/// The host's view, sent to the client every [`STATUS_INTERVAL`] for its stats.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Status {
    /// What the bandwidth estimator lets the host send in all, kbit/s.
    pub budget_kbps: u32,
    /// What the encoder is asked for, kbit/s.
    pub encoder_kbps: u32,
    /// Recovery shards over all shards, ‰.
    pub fec_permille: u16,
    /// Loss the host measured, ‰.
    pub loss_permille: u16,
}

pub const STATUS_INTERVAL: Micros = 250_000;

pub fn put_status(w: &mut wire::Writer<'_>, s: &Status) {
    w.u32(s.budget_kbps).u32(s.encoder_kbps).u16(s.fec_permille).u16(s.loss_permille);
}

pub fn get_status(body: &[u8]) -> Result<Status, Malformed> {
    let mut r = Reader::new(body);
    Ok(Status { budget_kbps: r.u32()?, encoder_kbps: r.u32()?, fec_permille: r.u16()?, loss_permille: r.u16()? })
}

/// Padding flags: the datagram is a path-MTU probe, so ack its size.
pub const PAD_MTU_PROBE: u8 = 1;

/// Appends a `Padding` chunk that brings `out` to `target` bytes (or as near as a chunk
/// allows).
pub fn put_padding(out: &mut Vec<u8>, target: usize, flags: u8) {
    let body = target.saturating_sub(out.len() + CHUNK_HEADER).max(1);
    wire::chunk(out, Kind::Padding, |w| {
        w.u8(flags);
        w.buf.resize(w.buf.len() + body - 1, 0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_datagrams_open_once() {
        let keys = SessionKeys::new(1, [3; 32]);
        let mut s = Sealer::new(&keys, Direction::ClientToHost, Cipher::Aes256Gcm);
        let mut o = Opener::new(&keys, Direction::ClientToHost, Cipher::Aes256Gcm);
        let mut out = Vec::new();
        let (pn, len) = s.seal(&mut out, |b| b.extend_from_slice(b"hi"));
        assert_eq!((pn, len), (1, 2 + OVERHEAD));
        let mut copy = out.clone();
        let opened = o.open(&mut out).unwrap();
        assert!(opened.newest);
        assert_eq!(opened.plain, b"hi");
        assert!(o.open(&mut copy).is_none());
        assert_eq!(o.replayed, 1);
    }

    #[test]
    fn small_chunks_pack_to_the_budget() {
        let mut q = SmallChunks::default();
        for i in 0..10u32 {
            q.push(Kind::Ping, |w| {
                w.u32(i).u64(0);
            });
        }
        let mut out = Vec::new();
        assert_eq!(q.pack(&mut out, 50), 45, "three 15-byte chunks");
        assert!(!q.is_empty());
    }

    #[test]
    fn padding_reaches_the_size() {
        let mut out = vec![0; 10];
        put_padding(&mut out, 1400, PAD_MTU_PROBE);
        assert_eq!(out.len(), 1400);
    }

    #[test]
    fn status_round_trips() {
        let s = Status { budget_kbps: 1, encoder_kbps: 2, fec_permille: 3, loss_permille: 4 };
        let mut b = Vec::new();
        put_status(&mut wire::Writer::new(&mut b), &s);
        assert_eq!(get_status(&b).unwrap(), s);
    }
}
