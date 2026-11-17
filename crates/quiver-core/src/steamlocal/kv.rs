//! A read-only binary KeyValues reader that borrows from the input, for the cache files
//! in this module. The node layout is the one [`crate::exports::vdf`] documents
//! (`<type> <key> <value>`, maps closed by `0x08`); this reader differs in two ways:
//!
//! - keys can be `u32` indices into a string table (`appinfo.vdf` v29) instead of inline
//!   NUL-terminated strings;
//! - nothing is copied, and nesting is limited so a garbled file fails instead of
//!   overflowing the stack.

use std::borrow::Cow;

use anyhow::{Result, bail};

/// Deeper than anything Steam writes (appinfo goes about 8 levels deep).
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kv<'a> {
    Map(Vec<(&'a [u8], Kv<'a>)>),
    Str(&'a [u8]),
    /// `02` int32, `04` pointer and `06` color, as their raw bits.
    Int(u32),
    Float(f32),
    UInt64(u64),
    Int64(i64),
    /// UTF-16LE bytes, without the terminator.
    WStr(&'a [u8]),
}

impl<'a> Kv<'a> {
    /// Child by key, case-insensitively.
    pub fn get(&self, key: &str) -> Option<&Kv<'a>> {
        match self {
            Kv::Map(m) => m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key.as_bytes())).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn entries(&self) -> &[(&'a [u8], Kv<'a>)] {
        match self {
            Kv::Map(m) => m,
            _ => &[],
        }
    }

    /// The value as text: strings as they are (lossy UTF-8), numbers formatted.
    pub fn text(&self) -> Option<Cow<'a, str>> {
        match self {
            Kv::Str(s) => Some(String::from_utf8_lossy(s)),
            Kv::WStr(w) => {
                let units: Vec<u16> = w.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                Some(Cow::Owned(String::from_utf16_lossy(&units)))
            }
            Kv::Int(n) => Some(Cow::Owned((*n as i32).to_string())),
            Kv::UInt64(n) => Some(Cow::Owned(n.to_string())),
            Kv::Int64(n) => Some(Cow::Owned(n.to_string())),
            Kv::Float(f) => Some(Cow::Owned(f.to_string())),
            Kv::Map(_) => None,
        }
    }

    /// The value as an integer: Steam stores the same field as int32 in one file and as
    /// a decimal string in the next, so both are accepted. int32 is read as signed.
    pub fn int(&self) -> Option<i64> {
        match self {
            Kv::Int(n) => Some(*n as i32 as i64),
            Kv::UInt64(n) => i64::try_from(*n).ok(),
            Kv::Int64(n) => Some(*n),
            Kv::Str(s) => std::str::from_utf8(s).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn str_at(&self, key: &str) -> Option<Cow<'a, str>> {
        self.get(key)?.text()
    }

    pub fn int_at(&self, key: &str) -> Option<i64> {
        self.get(key)?.int()
    }
}

/// A map key's text.
pub(crate) fn key_str<'a>(k: &'a [u8]) -> Cow<'a, str> {
    String::from_utf8_lossy(k)
}

pub(crate) struct Reader<'a, 't> {
    buf: &'a [u8],
    pos: usize,
    /// Key string table: keys are `u32` indices into it.
    table: Option<&'t [&'a [u8]]>,
}

impl<'a, 't> Reader<'a, 't> {
    pub fn new(buf: &'a [u8], table: Option<&'t [&'a [u8]]>) -> Self {
        Self { buf, pos: 0, table }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            bail!("truncated KeyValues at byte {}", self.pos);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn cstr(&mut self) -> Result<&'a [u8]> {
        let rest = &self.buf[self.pos..];
        let Some(len) = rest.iter().position(|&b| b == 0) else { bail!("unterminated string at byte {}", self.pos) };
        self.pos += len + 1;
        Ok(&rest[..len])
    }

    fn wstr(&mut self) -> Result<&'a [u8]> {
        let start = self.pos;
        loop {
            if self.take(2)? == [0, 0] {
                return Ok(&self.buf[start..self.pos - 2]);
            }
        }
    }

    fn key(&mut self) -> Result<&'a [u8]> {
        match self.table {
            None => self.cstr(),
            Some(t) => {
                let i = self.u32()? as usize;
                match t.get(i) {
                    Some(k) => Ok(k),
                    None => bail!("key index {i} outside the string table ({} entries)", t.len()),
                }
            }
        }
    }

    /// The top-level map: children until `0x08` or the end of the input.
    pub fn root(&mut self) -> Result<Kv<'a>> {
        self.map(0, true)
    }

    fn map(&mut self, depth: usize, top: bool) -> Result<Kv<'a>> {
        if depth > MAX_DEPTH {
            bail!("KeyValues nested too deeply at byte {}", self.pos);
        }
        let mut out = Vec::new();
        loop {
            if top && self.pos == self.buf.len() {
                return Ok(Kv::Map(out));
            }
            let at = self.pos;
            let ty = self.byte()?;
            if ty == 0x08 {
                return Ok(Kv::Map(out));
            }
            let key = self.key()?;
            let value = match ty {
                0x00 => self.map(depth + 1, false)?,
                0x01 => Kv::Str(self.cstr()?),
                0x02 | 0x04 | 0x06 => Kv::Int(self.u32()?),
                0x03 => Kv::Float(f32::from_bits(self.u32()?)),
                0x05 => Kv::WStr(self.wstr()?),
                0x07 => Kv::UInt64(self.u64()?),
                0x0a => Kv::Int64(self.u64()? as i64),
                t => bail!("unknown KeyValues type 0x{t:02x} at byte {at}"),
            };
            out.push((key, value));
        }
    }
}

/// Parse a whole buffer with inline keys.
pub(crate) fn parse(buf: &[u8]) -> Result<Kv<'_>> {
    Reader::new(buf, None).root()
}

/// Builds binary KeyValues for tests.
#[cfg(test)]
pub(crate) mod build {
    /// A node: `map("k", |b| …)`, `str`, `int`.
    #[derive(Default)]
    pub struct Kb {
        pub out: Vec<u8>,
        /// When set, keys are written as indices into this table (appended as needed).
        pub table: Option<Vec<String>>,
    }

    impl Kb {
        pub fn indexed() -> Self {
            Self { out: Vec::new(), table: Some(Vec::new()) }
        }

        fn key(&mut self, k: &str) {
            match &mut self.table {
                None => {
                    self.out.extend_from_slice(k.as_bytes());
                    self.out.push(0);
                }
                Some(t) => {
                    let i = t.iter().position(|s| s == k).unwrap_or_else(|| {
                        t.push(k.to_owned());
                        t.len() - 1
                    });
                    self.out.extend_from_slice(&(i as u32).to_le_bytes());
                }
            }
        }

        pub fn map(&mut self, k: &str, f: impl FnOnce(&mut Self)) -> &mut Self {
            self.out.push(0x00);
            self.key(k);
            f(self);
            self.out.push(0x08);
            self
        }

        pub fn str(&mut self, k: &str, v: &str) -> &mut Self {
            self.out.push(0x01);
            self.key(k);
            self.out.extend_from_slice(v.as_bytes());
            self.out.push(0);
            self
        }

        pub fn int(&mut self, k: &str, v: u32) -> &mut Self {
            self.out.push(0x02);
            self.key(k);
            self.out.extend_from_slice(&v.to_le_bytes());
            self
        }

        pub fn end(&mut self) -> &mut Self {
            self.out.push(0x08);
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build::Kb;
    use super::*;

    #[test]
    fn inline_keys_and_lookups() {
        let mut b = Kb::default();
        b.map("Root", |b| {
            b.str("Name", "x").int("n", 7).str("s", " 12 ").int("neg", u32::MAX);
        });
        b.end();
        let kv = parse(&b.out).unwrap();
        let r = kv.get("root").unwrap();
        assert_eq!(r.str_at("NAME").as_deref(), Some("x"));
        assert_eq!(r.int_at("n"), Some(7));
        assert_eq!(r.int_at("s"), Some(12));
        assert_eq!(r.int_at("neg"), Some(-1));
        assert_eq!(r.int_at("name"), None);
    }

    #[test]
    fn garbage_fails_without_panicking() {
        for buf in [&b"\x00abc"[..], b"\x01k\x00v", b"\x02k\x00\x01", b"\x09k\x00", b"\x05k\x00a\x00b"] {
            assert!(parse(buf).is_err(), "{buf:?}");
        }
        let deep: Vec<u8> = std::iter::repeat_n([0u8, b'k', 0], 1000).flatten().collect();
        assert!(parse(&deep).is_err());
        let table: Vec<&[u8]> = vec![b"a"];
        assert!(Reader::new(b"\x01\x05\x00\x00\x00v\x00", Some(&table)).root().is_err());
    }
}
