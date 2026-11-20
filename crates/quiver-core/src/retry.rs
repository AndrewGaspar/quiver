//! Whether a failed download is worth trying again later.
//!
//! A download that fails for a passing reason (the network dropped, a service closed its
//! connection, a server answered 5xx) should wait and go again on the same game; one that
//! fails for good (the disk is full, the game isn't owned, a slice never matches its hash)
//! should fail at once. Errors say which with a marker anywhere in their chain:
//! [`Transient`] or [`Permanent`], as the error itself or as context. Errors without one
//! are judged by what they wrap ([`classify`]); anything unknown counts as permanent, so a
//! bug never loops.

use std::fmt;

/// An error worth trying again later. Displays as its message, so it can be the error
/// itself ([`transient`]) or context on another (`e.context(Transient(..))`).
#[derive(Debug, Clone)]
pub struct Transient(pub String);

/// An error that trying again won't fix.
#[derive(Debug, Clone)]
pub struct Permanent(pub String);

impl fmt::Display for Transient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Permanent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Transient {}
impl std::error::Error for Permanent {}

/// A transient error with this message.
pub fn transient(msg: impl fmt::Display) -> anyhow::Error {
    anyhow::Error::new(Transient(msg.to_string()))
}

/// A permanent error with this message.
pub fn permanent(msg: impl fmt::Display) -> anyhow::Error {
    anyhow::Error::new(Permanent(msg.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Transient,
    Permanent,
}

/// Transient or permanent. A [`Permanent`] marker anywhere wins over a [`Transient`] one;
/// then I/O errors by kind (a full or read-only disk is permanent; resets, refusals,
/// timeouts and unreachable networks are not); then HTTP client errors (timeouts,
/// connection and DNS failures, cut-off bodies, 5xx/408/429 are transient). Anything else
/// is permanent.
pub fn classify(e: &anyhow::Error) -> Kind {
    if let Some(k) = marked(e) {
        return k;
    }
    for c in e.chain() {
        if let Some(io) = c.downcast_ref::<std::io::Error>()
            && let Some(k) = io_kind(io.kind())
        {
            return k;
        }
        if let Some(r) = c.downcast_ref::<reqwest::Error>() {
            return reqwest_kind(r);
        }
    }
    Kind::Permanent
}

/// What a marker in the chain says, if there is one ([`Permanent`] first).
pub fn marked(e: &anyhow::Error) -> Option<Kind> {
    if e.downcast_ref::<Permanent>().is_some() || e.chain().any(|c| c.is::<Permanent>()) {
        Some(Kind::Permanent)
    } else if e.downcast_ref::<Transient>().is_some() || e.chain().any(|c| c.is::<Transient>()) {
        Some(Kind::Transient)
    } else {
        None
    }
}

pub fn is_transient(e: &anyhow::Error) -> bool {
    classify(e) == Kind::Transient
}

fn io_kind(k: std::io::ErrorKind) -> Option<Kind> {
    use std::io::ErrorKind as K;
    match k {
        K::StorageFull | K::ReadOnlyFilesystem | K::PermissionDenied | K::QuotaExceeded | K::FileTooLarge => Some(Kind::Permanent),
        K::ConnectionReset
        | K::ConnectionAborted
        | K::ConnectionRefused
        | K::NotConnected
        | K::BrokenPipe
        | K::TimedOut
        | K::UnexpectedEof
        | K::HostUnreachable
        | K::NetworkUnreachable
        | K::NetworkDown
        | K::AddrNotAvailable
        | K::Interrupted => Some(Kind::Transient),
        _ => None,
    }
}

fn reqwest_kind(e: &reqwest::Error) -> Kind {
    if let Some(s) = e.status() {
        return http_status(s.as_u16());
    }
    if e.is_timeout() || e.is_connect() || e.is_body() || e.is_request() || e.is_decode() {
        Kind::Transient
    } else {
        Kind::Permanent
    }
}

/// An HTTP error status: 5xx, 408 (timeout), 425 and 429 (slow down) are transient.
pub fn http_status(status: u16) -> Kind {
    match status {
        408 | 425 | 429 | 500..=599 => Kind::Transient,
        _ => Kind::Permanent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_kinds() {
        let t = transient("download_service connection 14 was closed by the server").context("requesting download URLs");
        assert_eq!(classify(&t), Kind::Transient);
        assert_eq!(format!("{t:#}"), "requesting download URLs: download_service connection 14 was closed by the server");
        let p = anyhow::anyhow!("HTTP 403").context(Permanent("a.bin: gave up".into())).context("outer");
        assert_eq!(classify(&p), Kind::Permanent);
        assert_eq!(format!("{p:#}"), "outer: a.bin: gave up: HTTP 403");
        // Permanent wins over transient.
        let both = transient("x").context(Permanent("y".into()));
        assert_eq!(classify(&both), Kind::Permanent);

        let full: anyhow::Error = std::io::Error::from(std::io::ErrorKind::StorageFull).into();
        assert_eq!(classify(&full.context("writing a.bin")), Kind::Permanent);
        let reset: anyhow::Error = std::io::Error::from(std::io::ErrorKind::ConnectionReset).into();
        assert_eq!(classify(&reset.context("x")), Kind::Transient);
        assert_eq!(classify(&anyhow::anyhow!("manifest uses LZHAM")), Kind::Permanent);
        assert_eq!(http_status(503), Kind::Transient);
        assert_eq!(http_status(403), Kind::Permanent);
    }
}
