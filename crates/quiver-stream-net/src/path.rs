//! The client's address, as the host knows it, and the path MTU.
//!
//! **Address.** The host never trusts a source address on its own. When an authenticated,
//! fresh datagram (the newest packet number so far) arrives from an address other than the
//! current one, the host sends a random token to that address and switches only when the
//! token comes back from it. That handles the first hello, Wi-Fi roaming and NAT
//! rebinding the same way, and someone who copies a datagram and replays it from elsewhere
//! can't pull the stream away: the replay window drops the copy, and a copy that beats the
//! original can't answer the challenge from its own address unless it's on the path anyway.
//!
//! **MTU.** Datagrams start at [`BASE_DATAGRAM`] bytes, which fits any path (1280-byte IPv6
//! minimum, WireGuard and Tailscale included). The host then probes bigger sizes with DF
//! set; the client acks each probe that arrives, and the host moves up. A path change
//! starts again from the base.

use std::net::SocketAddr;

use quiver_stream_proto::Micros;

/// The datagram size every path carries: 1280 − 40 (IPv6) − 8 (UDP).
pub const BASE_DATAGRAM: usize = 1232;
/// Sizes to try above the base: Ethernet's 1500 under IPv6, then under IPv4.
const LADDER: [usize; 2] = [1452, 1472];

fn random_token() -> u64 {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 8];
    ring::rand::SystemRandom::new().fill(&mut b).expect("system random source");
    u64::from_ne_bytes(b)
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    addr: SocketAddr,
    token: u64,
    sent_at: Option<Micros>,
    tries: u32,
}

#[derive(Debug, Default)]
pub struct PathValidator {
    pub current: Option<SocketAddr>,
    candidate: Option<Candidate>,
}

/// What a validated response changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    First(SocketAddr),
    Migrated { from: SocketAddr, to: SocketAddr },
}

impl PathValidator {
    const RETRY_US: Micros = 50_000;
    const MAX_TRIES: u32 = 20;

    /// An authenticated datagram arrived from `from`; `newest` if its packet number is the
    /// highest yet.
    pub fn on_packet(&mut self, from: SocketAddr, newest: bool) {
        if !newest || Some(from) == self.current {
            return;
        }
        if self.candidate.is_some_and(|c| c.addr == from) {
            return;
        }
        self.candidate = Some(Candidate { addr: from, token: random_token(), sent_at: None, tries: 0 });
    }

    /// A challenge to send now: `(to, token)`.
    pub fn poll_challenge(&mut self, now: Micros) -> Option<(SocketAddr, u64)> {
        let c = self.candidate.as_mut()?;
        if c.sent_at.is_some_and(|t| now < t + Self::RETRY_US) {
            return None;
        }
        if c.tries >= Self::MAX_TRIES {
            self.candidate = None;
            return None;
        }
        c.sent_at = Some(now);
        c.tries += 1;
        Some((c.addr, c.token))
    }

    pub fn next_timeout(&self) -> Option<Micros> {
        self.candidate.map(|c| c.sent_at.map_or(0, |t| t + Self::RETRY_US))
    }

    /// A response arrived from `from`. Returns the change if it validates the candidate.
    pub fn on_response(&mut self, from: SocketAddr, token: u64) -> Option<PathChange> {
        let c = self.candidate.filter(|c| c.addr == from && c.token == token)?;
        self.candidate = None;
        let change = match self.current {
            None => PathChange::First(c.addr),
            Some(old) => PathChange::Migrated { from: old, to: c.addr },
        };
        self.current = Some(c.addr);
        Some(change)
    }
}

/// The host's PMTU search.
#[derive(Debug)]
pub struct MtuProber {
    /// The largest datagram known to get through.
    pub current: usize,
    max: usize,
    next: usize,
    probe: Option<(usize, Micros, u32)>,
    start_at: Micros,
}

impl MtuProber {
    const TIMEOUT_US: Micros = 300_000;
    const TRIES: u32 = 3;

    pub fn new(max: usize, enabled: bool) -> Self {
        let max = if enabled { max.max(BASE_DATAGRAM) } else { BASE_DATAGRAM };
        Self { current: BASE_DATAGRAM, max, next: 0, probe: None, start_at: Micros::MAX }
    }

    /// Starts (again) from the base, probing after `delay`.
    pub fn restart(&mut self, now: Micros, delay: Micros) {
        self.current = BASE_DATAGRAM;
        self.next = 0;
        self.probe = None;
        self.start_at = now + delay;
    }

    fn candidate(&self) -> Option<usize> {
        LADDER.get(self.next).copied().filter(|s| *s <= self.max)
    }

    /// A probe of this size to send now.
    pub fn poll(&mut self, now: Micros) -> Option<usize> {
        if now < self.start_at {
            return None;
        }
        let size = self.candidate()?;
        match self.probe {
            Some((s, at, tries)) if s == size => {
                if now < at + Self::TIMEOUT_US {
                    return None;
                }
                if tries >= Self::TRIES {
                    self.next = LADDER.len();
                    self.probe = None;
                    return None;
                }
                self.probe = Some((size, now, tries + 1));
            }
            _ => self.probe = Some((size, now, 1)),
        }
        Some(size)
    }

    pub fn next_timeout(&self) -> Option<Micros> {
        self.candidate()?;
        Some(match self.probe {
            Some((_, at, _)) => at + Self::TIMEOUT_US,
            None => self.start_at,
        })
    }

    pub fn on_ack(&mut self, size: usize) {
        if let Some(c) = self.candidate().filter(|c| size >= *c) {
            self.current = self.current.max(c);
            self.next += 1;
            self.probe = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn an_address_is_adopted_only_once_validated() {
        let mut p = PathValidator::default();
        p.on_packet(addr(1), true);
        assert_eq!(p.current, None);
        let (to, token) = p.poll_challenge(0).unwrap();
        assert_eq!(to, addr(1));
        assert_eq!(p.on_response(addr(1), token ^ 1), None, "wrong token");
        assert_eq!(p.on_response(addr(2), token), None, "wrong address");
        assert_eq!(p.on_response(addr(1), token), Some(PathChange::First(addr(1))));
        // A reordered (not newest) packet from elsewhere is ignored; a fresh one challenges.
        p.on_packet(addr(3), false);
        assert!(p.poll_challenge(1).is_none());
        p.on_packet(addr(3), true);
        let (_, t) = p.poll_challenge(2).unwrap();
        assert!(p.poll_challenge(3).is_none(), "not again before the retry interval");
        assert_eq!(p.on_response(addr(3), t), Some(PathChange::Migrated { from: addr(1), to: addr(3) }));
        assert_eq!(p.current, Some(addr(3)));
    }

    #[test]
    fn mtu_climbs_the_ladder_and_stops_at_a_black_hole() {
        let mut m = MtuProber::new(1472, true);
        m.restart(0, 100);
        assert_eq!(m.poll(50), None);
        assert_eq!(m.poll(100), Some(1452));
        m.on_ack(1452);
        assert_eq!(m.current, 1452);
        assert_eq!(m.poll(101), Some(1472));
        assert_eq!(m.poll(102), None);
        let mut t = 101;
        for _ in 0..2 {
            t += MtuProber::TIMEOUT_US;
            assert_eq!(m.poll(t), Some(1472));
        }
        assert_eq!(m.poll(t + MtuProber::TIMEOUT_US), None);
        assert_eq!(m.next_timeout(), None, "gave up");
        assert_eq!(m.current, 1452);
    }

    #[test]
    fn mtu_probing_can_be_off() {
        let mut m = MtuProber::new(1472, false);
        m.restart(0, 0);
        assert_eq!(m.poll(0), None);
    }
}
