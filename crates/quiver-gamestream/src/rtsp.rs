//! RTSP as Moonlight speaks it on TCP 48010: one request per connection, the server answers
//! and closes. The handshake is OPTIONS, DESCRIBE (the server's SDP: codecs, audio layouts,
//! features), SETUP ×3 (audio, video, control: their UDP ports and session ids), ANNOUNCE
//! (the client's SDP: everything it wants, see [`crate::sdp`]) and PLAY.
//!
//! When `/launch` returned an `rtspenc://` URL, every message is AES-GCM sealed under the
//! launch key: `u32 BE 0x80000000 | length`, `u32 BE sequence`, a 16-byte tag, then the
//! ciphertext. The IV is the sequence (little-endian) in bytes 0..4 and `C` `R` (client) or
//! `H` `R` (host) in bytes 10 and 11.

use anyhow::{Result, anyhow, bail};

use crate::crypto::{Key, gcm_open, gcm_seal, marked_iv};

const ENCRYPTED_BIT: u32 = 0x8000_0000;
/// No request Moonlight sends comes near this.
pub const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn cseq(&self) -> u32 {
        self.header("CSeq").and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    /// What a SETUP is for: `audio`, `video` or `control` (from `streamid=video/0/0`).
    pub fn stream_kind(&self) -> Option<&str> {
        let s = self.target.split("streamid=").nth(1)?;
        Some(s.split('/').next().unwrap_or(s))
    }
}

/// Parses a plaintext request from the start of `buf`. `Ok(None)` means more bytes are
/// needed. On success, also returns how many bytes it used.
pub fn parse_request(buf: &[u8]) -> Result<Option<(Request, usize)>> {
    let Some(end) = find(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_MESSAGE {
            bail!("RTSP headers over {MAX_MESSAGE} bytes");
        }
        return Ok(None);
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| anyhow!("RTSP headers aren't UTF-8"))?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let (Some(method), Some(target), Some(proto)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("bad RTSP request line {first:?}");
    };
    if proto != "RTSP/1.0" {
        bail!("not RTSP/1.0: {proto:?}");
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { bail!("bad RTSP header {line:?}") };
        headers.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    let mut req = Request { method: method.to_owned(), target: target.to_owned(), headers, body: Vec::new() };
    let len: usize = req.header("Content-Length").map(|v| v.parse()).transpose().map_err(|_| anyhow!("bad Content-Length"))?.unwrap_or(0);
    if len > MAX_MESSAGE {
        bail!("RTSP body of {len} bytes");
    }
    let body_start = end + 4;
    if buf.len() < body_start + len {
        return Ok(None);
    }
    req.body = buf[body_start..body_start + len].to_vec();
    Ok(Some((req, body_start + len)))
}

/// A response: status line, `CSeq`, the given headers, and the body.
pub fn response(status: u16, reason: &str, cseq: u32, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut s = format!("RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        s.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    s.push_str("\r\n");
    let mut out = s.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Seals a host message (encrypted RTSP).
pub fn seal(key: &Key, seq: u32, plaintext: &[u8]) -> Vec<u8> {
    let mut body = plaintext.to_vec();
    let tag = gcm_seal(key, &marked_iv(seq, b'H', b'R'), &mut body);
    let mut out = Vec::with_capacity(24 + body.len());
    out.extend_from_slice(&(ENCRYPTED_BIT | body.len() as u32).to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&body);
    out
}

/// Opens a client message (encrypted RTSP) from the start of `buf`: the plaintext, its
/// sequence number, and how many bytes it used. `Ok(None)` means more bytes are needed.
pub fn open(key: &Key, buf: &[u8]) -> Result<Option<(Vec<u8>, u32, usize)>> {
    if buf.len() < 24 {
        return Ok(None);
    }
    let type_len = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    if type_len & ENCRYPTED_BIT == 0 {
        bail!("an unencrypted RTSP message where an encrypted one was expected");
    }
    let len = (type_len & !ENCRYPTED_BIT) as usize;
    if len > MAX_MESSAGE {
        bail!("encrypted RTSP message of {len} bytes");
    }
    if buf.len() < 24 + len {
        return Ok(None);
    }
    let seq = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    let tag: [u8; 16] = buf[8..24].try_into().unwrap();
    let mut body = buf[24..24 + len].to_vec();
    gcm_open(key, &marked_iv(seq, b'C', b'R'), &mut body, &tag)?;
    Ok(Some((body, seq, 24 + len)))
}

/// Whether `buf` starts like an encrypted message rather than a plaintext request.
pub fn looks_encrypted(buf: &[u8]) -> bool {
    buf.first().is_some_and(|b| b & 0x80 != 0)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requests() {
        let raw = b"OPTIONS rtsp://10.0.0.2:48010 RTSP/1.0\r\nCSeq: 1\r\nX-GS-ClientVersion: 14\r\nHost: 10.0.0.2\r\n\r\n";
        let (r, used) = parse_request(raw).unwrap().unwrap();
        assert_eq!(used, raw.len());
        assert_eq!((r.method.as_str(), r.cseq()), ("OPTIONS", 1));
        assert_eq!(r.header("host"), Some("10.0.0.2"));
        assert!(parse_request(&raw[..raw.len() - 2]).unwrap().is_none());

        let sdp = "v=0\r\na=x-nv-video[0].clientViewportWd:1920 \r\n";
        let raw = format!(
            "ANNOUNCE streamid=control/13/0 RTSP/1.0\r\nCSeq: 6\r\nSession: DEADBEEFCAFE\r\nContent-type: application/sdp\r\nContent-length: {}\r\n\r\n{sdp}",
            sdp.len()
        );
        let (r, _) = parse_request(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(r.body, sdp.as_bytes());
        assert_eq!(r.stream_kind(), Some("control"));
        assert!(parse_request(&raw.as_bytes()[..raw.len() - 1]).unwrap().is_none(), "body still coming");

        let raw = b"SETUP streamid=video/0/0 RTSP/1.0\r\nCSeq: 4\r\n\r\n";
        assert_eq!(parse_request(raw).unwrap().unwrap().0.stream_kind(), Some("video"));
        assert!(parse_request(b"GET / HTTP/1.1\r\n\r\n").is_err());
    }

    #[test]
    fn responses() {
        let r = response(200, "OK", 3, &[("Session", "DEADBEEFCAFE;timeout = 90".into())], b"");
        assert_eq!(r, b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: DEADBEEFCAFE;timeout = 90\r\n\r\n");
        let r = response(200, "OK", 2, &[], b"a=x:1\n");
        assert!(r.ends_with(b"Content-Length: 6\r\n\r\na=x:1\n"));
    }

    #[test]
    fn encrypted_framing() {
        let key = [5u8; 16];
        // A client message, sealed as Moonlight seals it.
        let msg = b"OPTIONS rtsp://h:48010 RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let mut body = msg.to_vec();
        let tag = gcm_seal(&key, &marked_iv(1, b'C', b'R'), &mut body);
        let mut wire = (0x8000_0000u32 | body.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&1u32.to_be_bytes());
        wire.extend_from_slice(&tag);
        wire.extend_from_slice(&body);
        assert!(looks_encrypted(&wire));
        assert!(open(&key, &wire[..30]).unwrap().is_none());
        let (plain, seq, used) = open(&key, &wire).unwrap().unwrap();
        assert_eq!((plain.as_slice(), seq, used), (&msg[..], 1, wire.len()));
        // The host's answer opens with the host marker.
        let sealed = seal(&key, 1, b"RTSP/1.0 200 OK\r\n\r\n");
        let tag: [u8; 16] = sealed[8..24].try_into().unwrap();
        let mut b = sealed[24..].to_vec();
        gcm_open(&key, &marked_iv(1, b'H', b'R'), &mut b, &tag).unwrap();
        assert_eq!(b, b"RTSP/1.0 200 OK\r\n\r\n");
        assert!(open(&key, &sealed).is_err(), "a host message doesn't open as a client one");
        assert!(!looks_encrypted(b"OPTIONS"));
    }
}
