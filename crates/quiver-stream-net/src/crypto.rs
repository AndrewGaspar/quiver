//! Keys, datagram sealing and replay protection.
//!
//! Every datagram is `header ‖ ciphertext ‖ tag`. The 10-byte header is sent in the clear
//! and authenticated as the AEAD's associated data:
//!
//! ```text
//! 0      version (WIRE_VERSION)
//! 1      direction (0 client→host, 1 host→client)
//! 2..10  packet number, big-endian u64, per direction, from 1
//! ```
//!
//! Each direction has its own key, derived with HKDF-SHA256 from the session key, the session
//! id and the direction. The nonce is the direction (4 bytes) followed by the packet number
//! (8 bytes), so a nonce never repeats under one key as long as packet numbers don't, which
//! is why a [`SessionKeys`] must serve one host and one client, once.

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;

/// The wire format's version. A datagram with another version is dropped unread.
pub const WIRE_VERSION: u8 = 1;
/// Bytes of clear header in front of every datagram.
pub const HEADER_LEN: usize = 10;
/// Bytes of AEAD tag behind every datagram.
pub const TAG_LEN: usize = 16;
/// Header plus tag: what sealing adds to a datagram's plaintext.
pub const OVERHEAD: usize = HEADER_LEN + TAG_LEN;

/// The keys for one stream, from the control plane (design/REMOTE.md §2.1).
///
/// The control plane makes a fresh random key for every stream it starts, and hands the same
/// keys to the host and the client over its TLS connection. Keys must not be reused for a
/// second session: packet numbers restart with each session, and a repeated nonce under
/// AES-GCM gives the key away. A client that reconnects after a restart asks for new keys.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKeys {
    /// Names the stream. It is mixed into the derived keys, so two sessions never share keys
    /// even if their secrets did.
    pub session_id: u64,
    /// 32 random bytes.
    pub key: [u8; 32],
}

impl SessionKeys {
    pub fn new(session_id: u64, key: [u8; 32]) -> Self {
        Self { session_id, key }
    }

    /// From the 40-byte form [`to_bytes`](Self::to_bytes) makes: the id big-endian, then the
    /// key.
    pub fn from_bytes(bytes: &[u8; 40]) -> Self {
        let mut id = [0u8; 8];
        id.copy_from_slice(&bytes[..8]);
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes[8..]);
        Self { session_id: u64::from_be_bytes(id), key }
    }

    pub fn to_bytes(&self) -> [u8; 40] {
        let mut out = [0u8; 40];
        out[..8].copy_from_slice(&self.session_id.to_be_bytes());
        out[8..].copy_from_slice(&self.key);
        out
    }

    /// The 40-byte form as 80 hex digits, for passing between test processes.
    pub fn to_hex(&self) -> String {
        self.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim();
        if hex.len() != 80 || !hex.is_ascii() {
            return None;
        }
        let mut bytes = [0u8; 40];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Self::from_bytes(&bytes))
    }

    /// Fresh keys from the system's secure random source, for tests and tools. The control
    /// plane makes the real ones.
    pub fn random() -> Self {
        use ring::rand::SecureRandom;
        let rng = ring::rand::SystemRandom::new();
        let mut bytes = [0u8; 40];
        rng.fill(&mut bytes).expect("system random source");
        Self::from_bytes(&bytes)
    }
}

impl std::fmt::Debug for SessionKeys {
    /// The key itself is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKeys").field("session_id", &self.session_id).finish_non_exhaustive()
    }
}

/// Which way a datagram goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToHost = 0,
    HostToClient = 1,
}

impl Direction {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::ClientToHost),
            1 => Some(Self::HostToClient),
            _ => None,
        }
    }

    pub fn reverse(self) -> Self {
        match self {
            Self::ClientToHost => Self::HostToClient,
            Self::HostToClient => Self::ClientToHost,
        }
    }
}

/// The AEAD. AES-256-GCM is the default: with AES-NI and VAES it is about 3× ChaCha20 on
/// this machine (see the bench). Both ends must use the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cipher {
    Aes256Gcm,
    ChaCha20Poly1305,
}

impl Cipher {
    fn algorithm(self) -> &'static aead::Algorithm {
        match self {
            Self::Aes256Gcm => &aead::AES_256_GCM,
            Self::ChaCha20Poly1305 => &aead::CHACHA20_POLY1305,
        }
    }
}

/// Seals datagrams one way, or opens them.
pub struct PacketKey {
    key: LessSafeKey,
    dir: Direction,
}

impl PacketKey {
    pub fn new(keys: &SessionKeys, dir: Direction, cipher: Cipher) -> Self {
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"quiver-stream-net v1");
        let prk = salt.extract(&keys.key);
        let mut info = [0u8; 9];
        info[..8].copy_from_slice(&keys.session_id.to_be_bytes());
        info[8] = dir as u8;
        let info_parts = [&info[..]];
        let okm = prk.expand(&info_parts, cipher.algorithm()).expect("HKDF output length");
        let key = LessSafeKey::new(UnboundKey::from(okm));
        Self { key, dir }
    }

    pub fn direction(&self) -> Direction {
        self.dir
    }

    fn nonce(&self, pn: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&(self.dir as u32).to_be_bytes());
        n[4..].copy_from_slice(&pn.to_be_bytes());
        Nonce::assume_unique_for_key(n)
    }

    /// Writes the header for `pn` at `buf[start..start + HEADER_LEN]`. Call before the
    /// plaintext goes in behind it, then [`seal`](Self::seal).
    pub fn write_header(&self, buf: &mut Vec<u8>, pn: u64) {
        buf.push(WIRE_VERSION);
        buf.push(self.dir as u8);
        buf.extend_from_slice(&pn.to_be_bytes());
    }

    /// Encrypts the datagram that starts at `buf[start]` (a header from
    /// [`write_header`](Self::write_header), then plaintext, up to the end of `buf`) and
    /// appends the tag.
    pub fn seal(&self, buf: &mut Vec<u8>, start: usize) {
        let pn = header_pn(&buf[start..]).expect("a header was written");
        let (head, body) = buf[start..].split_at_mut(HEADER_LEN);
        let tag = self
            .key
            .seal_in_place_separate_tag(self.nonce(pn), Aad::from(&*head), body)
            .expect("sealing never fails for datagram sizes");
        buf.extend_from_slice(tag.as_ref());
    }

    /// Checks and decrypts a whole datagram in place. Returns the packet number and the
    /// plaintext, or None if the datagram isn't ours, is from the wrong direction, or was
    /// tampered with.
    pub fn open<'a>(&self, datagram: &'a mut [u8]) -> Option<(u64, &'a mut [u8])> {
        if datagram.len() < OVERHEAD
            || datagram[0] != WIRE_VERSION
            || Direction::from_byte(datagram[1]) != Some(self.dir)
        {
            return None;
        }
        let pn = header_pn(datagram)?;
        let (head, body) = datagram.split_at_mut(HEADER_LEN);
        let plain = self.key.open_in_place(self.nonce(pn), Aad::from(&*head), body).ok()?;
        Some((pn, plain))
    }
}

/// The packet number in a datagram's clear header.
pub fn header_pn(datagram: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = datagram.get(2..10)?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// Remembers which packet numbers have been seen, so a replayed datagram is dropped.
///
/// It keeps a bitmap of the last [`ReplayWindow::SIZE`] numbers below the highest seen.
/// Older numbers are refused outright. That is enough for reordering on any real path: at
/// 300 Mbit/s it covers about 130 ms.
#[derive(Debug, Clone)]
pub struct ReplayWindow {
    /// The highest packet number accepted; 0 before the first.
    top: u64,
    /// Bit `i` (word `i / 64`, bit `i % 64`) is set if `top - i` was seen.
    bits: Box<[u64; Self::WORDS]>,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self { top: 0, bits: Box::new([0; Self::WORDS]) }
    }
}

impl ReplayWindow {
    const WORDS: usize = 64;
    pub const SIZE: u64 = (Self::WORDS * 64) as u64;

    /// Whether `pn` would be accepted. Check before decrypting is fine, but only
    /// [`accept`](Self::accept) after the datagram authenticated.
    pub fn is_fresh(&self, pn: u64) -> bool {
        if pn == 0 {
            return false;
        }
        if pn > self.top {
            return true;
        }
        let age = self.top - pn;
        age < Self::SIZE && !self.bit(age)
    }

    /// Marks `pn` seen. Returns false (and changes nothing) if it was a replay.
    pub fn accept(&mut self, pn: u64) -> bool {
        if !self.is_fresh(pn) {
            return false;
        }
        if pn > self.top {
            self.shift(pn - self.top);
            self.top = pn;
        }
        self.set(self.top - pn);
        true
    }

    /// The highest packet number accepted so far.
    pub fn top(&self) -> u64 {
        self.top
    }

    fn bit(&self, age: u64) -> bool {
        self.bits[(age / 64) as usize] & (1 << (age % 64)) != 0
    }

    fn set(&mut self, age: u64) {
        self.bits[(age / 64) as usize] |= 1 << (age % 64);
    }

    /// Moves every bit `n` ages older.
    fn shift(&mut self, n: u64) {
        if n >= Self::SIZE {
            self.bits.fill(0);
            return;
        }
        let words = (n / 64) as usize;
        let bits = (n % 64) as u32;
        for i in (0..Self::WORDS).rev() {
            let src = i.checked_sub(words);
            let hi = src.map_or(0, |s| self.bits[s]);
            let lo = src.and_then(|s| s.checked_sub(1)).map_or(0, |s| self.bits[s]);
            self.bits[i] = if bits == 0 { hi } else { (hi << bits) | (lo >> (64 - bits)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> SessionKeys {
        SessionKeys::new(7, [9; 32])
    }

    fn sealed(key: &PacketKey, pn: u64, body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        key.write_header(&mut buf, pn);
        buf.extend_from_slice(body);
        key.seal(&mut buf, 0);
        buf
    }

    #[test]
    fn seal_then_open_round_trips() {
        for cipher in [Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305] {
            let tx = PacketKey::new(&keys(), Direction::HostToClient, cipher);
            let rx = PacketKey::new(&keys(), Direction::HostToClient, cipher);
            let mut d = sealed(&tx, 42, b"hello");
            assert_eq!(d.len(), 5 + OVERHEAD);
            let (pn, plain) = rx.open(&mut d).unwrap();
            assert_eq!((pn, &*plain), (42, &b"hello"[..]));
        }
    }

    #[test]
    fn forgeries_and_wrong_keys_are_refused() {
        let tx = PacketKey::new(&keys(), Direction::HostToClient, Cipher::Aes256Gcm);
        let rx = PacketKey::new(&keys(), Direction::HostToClient, Cipher::Aes256Gcm);
        let good = sealed(&tx, 1, b"payload");
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 1;
            assert!(rx.open(&mut bad).is_none(), "flipped byte {i} accepted");
        }
        let other = PacketKey::new(&SessionKeys::new(8, [9; 32]), Direction::HostToClient, Cipher::Aes256Gcm);
        assert!(other.open(&mut good.clone()).is_none(), "another session's key opened it");
        let reverse = PacketKey::new(&keys(), Direction::ClientToHost, Cipher::Aes256Gcm);
        assert!(reverse.open(&mut good.clone()).is_none(), "the other direction opened it");
    }

    #[test]
    fn keys_round_trip_as_bytes_and_hide_in_debug() {
        let k = SessionKeys::random();
        assert_eq!(SessionKeys::from_bytes(&k.to_bytes()), k);
        assert_eq!(SessionKeys::from_hex(&k.to_hex()), Some(k.clone()));
        assert_eq!(SessionKeys::from_hex("zz"), None);
        assert!(!format!("{k:?}").contains("key:"));
    }

    #[test]
    fn replay_window_accepts_each_number_once() {
        let mut w = ReplayWindow::default();
        assert!(!w.accept(0));
        assert!(w.accept(5));
        assert!(!w.accept(5));
        assert!(w.accept(3));
        assert!(!w.accept(3));
        assert!(w.accept(1000));
        assert!(w.accept(4));
        assert!(!w.accept(4));
        assert!(w.accept(1000 + ReplayWindow::SIZE - 1));
        assert!(!w.accept(999), "older than the window");
        assert!(w.accept(1001));
    }

    #[test]
    fn replay_window_shift_keeps_bits_across_words() {
        let mut w = ReplayWindow::default();
        for pn in 1..=300 {
            assert!(w.accept(pn));
        }
        w.accept(370);
        for pn in 1..=300 {
            assert!(!w.is_fresh(pn), "{pn} forgotten");
        }
        for pn in 301..370 {
            assert!(w.is_fresh(pn), "{pn} marked seen");
        }
    }
}
