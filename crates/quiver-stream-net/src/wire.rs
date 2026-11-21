//! The plaintext inside a datagram: a run of chunks.
//!
//! Each chunk is `kind: u8, len: u16 (big-endian), body: [u8; len]`. A datagram can carry
//! several, so an ack, a ping and input events can share one. Integers are big-endian.
//! Unknown kinds are skipped, which lets a newer peer add chunks.

use std::fmt;

/// The kinds of chunk. The numbers are wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Client → host until answered: "I have the keys, here I am."
    Hello = 0x01,
    /// Host → client: the client's address is validated and the session is up.
    HelloAck = 0x02,
    /// Host → a new client address: echo this token from there.
    PathChallenge = 0x03,
    PathResponse = 0x04,
    Ping = 0x05,
    Pong = 0x06,
    Close = 0x07,
    /// Host → client: the host's bandwidth estimate and FEC level, for the client's stats.
    Status = 0x08,
    /// One shard of a video frame (data or FEC).
    Video = 0x10,
    Audio = 0x11,
    Input = 0x20,
    InputAck = 0x21,
    Control = 0x30,
    ControlAck = 0x31,
    /// Client → host: transport-wide receive feedback for the bandwidth estimator.
    Feedback = 0x40,
    /// Client → host: resend these shards of a frame.
    Nack = 0x41,
    /// Filler, for path-MTU probes and bandwidth probes.
    Padding = 0x50,
    /// Client → host: a path-MTU probe of this size arrived.
    MtuAck = 0x51,
}

impl Kind {
    pub fn from_byte(b: u8) -> Option<Self> {
        use Kind::*;
        Some(match b {
            0x01 => Hello,
            0x02 => HelloAck,
            0x03 => PathChallenge,
            0x04 => PathResponse,
            0x05 => Ping,
            0x06 => Pong,
            0x07 => Close,
            0x08 => Status,
            0x10 => Video,
            0x11 => Audio,
            0x20 => Input,
            0x21 => InputAck,
            0x30 => Control,
            0x31 => ControlAck,
            0x40 => Feedback,
            0x41 => Nack,
            0x50 => Padding,
            0x51 => MtuAck,
            _ => return None,
        })
    }
}

/// Bytes of chunk header (kind and length).
pub const CHUNK_HEADER: usize = 3;

/// A malformed chunk body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed chunk")
    }
}

impl std::error::Error for Malformed {}

/// Appends big-endian fields to a buffer.
pub struct Writer<'a> {
    pub buf: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        Self { buf }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.u16(v as u16)
    }

    /// LEB128, 7 bits a byte.
    pub fn varint(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
        self
    }

    /// A signed varint, zigzag-coded so small negatives stay short.
    pub fn svarint(&mut self, v: i64) -> &mut Self {
        self.varint(((v << 1) ^ (v >> 63)) as u64)
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// A u16 length, then the bytes.
    pub fn blob16(&mut self, v: &[u8]) -> &mut Self {
        self.u16(v.len() as u16).bytes(v)
    }
}

/// Writes one chunk: header, then whatever `body` appends. The length is filled in after.
pub fn chunk(buf: &mut Vec<u8>, kind: Kind, body: impl FnOnce(&mut Writer<'_>)) {
    let start = buf.len();
    buf.extend_from_slice(&[kind as u8, 0, 0]);
    body(&mut Writer::new(buf));
    let len = buf.len() - start - CHUNK_HEADER;
    debug_assert!(len <= u16::MAX as usize, "chunk body too long");
    buf[start + 1..start + 3].copy_from_slice(&(len as u16).to_be_bytes());
}

/// Reads big-endian fields from a slice, failing on a short read.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }

    pub fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    pub fn remaining(&self) -> usize {
        self.rest.len()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.rest.len() < n {
            return Err(Malformed);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    /// Everything left.
    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        Ok(self.take(N)?.try_into().expect("took N bytes"))
    }

    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Malformed> {
        self.array().map(u16::from_be_bytes)
    }

    pub fn u32(&mut self) -> Result<u32, Malformed> {
        self.array().map(u32::from_be_bytes)
    }

    pub fn u64(&mut self) -> Result<u64, Malformed> {
        self.array().map(u64::from_be_bytes)
    }

    pub fn i16(&mut self) -> Result<i16, Malformed> {
        self.u16().map(|v| v as i16)
    }

    pub fn varint(&mut self) -> Result<u64, Malformed> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Malformed)
    }

    pub fn svarint(&mut self) -> Result<i64, Malformed> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    pub fn blob16(&mut self) -> Result<&'a [u8], Malformed> {
        let n = self.u16()? as usize;
        self.take(n)
    }
}

/// Splits a datagram's plaintext into `(kind, body)` chunks. Unknown kinds come out as
/// `None` so the caller can skip them; a truncated chunk ends the iteration with an error.
pub struct Chunks<'a> {
    reader: Reader<'a>,
}

impl<'a> Chunks<'a> {
    pub fn new(plain: &'a [u8]) -> Self {
        Self { reader: Reader::new(plain) }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Result<(Option<Kind>, &'a [u8]), Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.reader.is_empty() {
            return None;
        }
        let item = (|| {
            let kind = self.reader.u8()?;
            let body = self.reader.blob16()?;
            Ok((Kind::from_byte(kind), body))
        })();
        if item.is_err() {
            self.reader = Reader::new(&[]);
        }
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip() {
        let mut buf = Vec::new();
        let mut w = Writer::new(&mut buf);
        w.u8(1).u16(2).u32(3).u64(4).i16(-5).varint(300).svarint(-70).blob16(b"xy");
        let mut r = Reader::new(&buf);
        assert_eq!(r.u8(), Ok(1));
        assert_eq!(r.u16(), Ok(2));
        assert_eq!(r.u32(), Ok(3));
        assert_eq!(r.u64(), Ok(4));
        assert_eq!(r.i16(), Ok(-5));
        assert_eq!(r.varint(), Ok(300));
        assert_eq!(r.svarint(), Ok(-70));
        assert_eq!(r.blob16(), Ok(&b"xy"[..]));
        assert!(r.is_empty());
        assert_eq!(r.u8(), Err(Malformed));
    }

    #[test]
    fn varints_cover_the_extremes() {
        for v in [0, 1, 127, 128, u64::MAX] {
            let mut buf = Vec::new();
            Writer::new(&mut buf).varint(v);
            assert_eq!(Reader::new(&buf).varint(), Ok(v));
        }
        for v in [i64::MIN, -1, 0, 1, i64::MAX] {
            let mut buf = Vec::new();
            Writer::new(&mut buf).svarint(v);
            assert_eq!(Reader::new(&buf).svarint(), Ok(v));
        }
    }

    #[test]
    fn chunks_split_and_skip_unknown_kinds() {
        let mut buf = Vec::new();
        chunk(&mut buf, Kind::Ping, |w| {
            w.u32(9);
        });
        buf.extend_from_slice(&[0xee, 0, 1, 0xff]);
        chunk(&mut buf, Kind::Padding, |_| {});
        let got: Vec<_> = Chunks::new(&buf).collect::<Result<_, _>>().unwrap();
        assert_eq!(got, vec![(Some(Kind::Ping), &[0, 0, 0, 9][..]), (None, &[0xff][..]), (Some(Kind::Padding), &[][..])]);
        let truncated = &buf[..buf.len() - 4];
        assert!(Chunks::new(truncated).any(|c| c.is_err()));
    }
}
