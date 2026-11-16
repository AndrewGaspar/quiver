//! Steam's binary VDF (KeyValues), as used by `userdata/<id>/config/shortcuts.vdf`.
//!
//! Layout: a node is `<type byte> <key, NUL-terminated> <value>`; a map's children run
//! until a `0x08` byte. The file itself is a map (normally holding one `shortcuts` map),
//! so it ends `08 08`.
//!
//! | type | value |
//! |---|---|
//! | `00` | nested map |
//! | `01` | string, NUL-terminated |
//! | `02` | int32, little-endian |
//! | `03` | float32, little-endian |
//! | `04` | pointer (int32) |
//! | `05` | wide string (UTF-16LE, two NUL bytes) |
//! | `06` | color (int32) |
//! | `07` | uint64, little-endian |
//! | `0a` | int64, little-endian |
//!
//! Keys and strings are kept as raw bytes and numbers as their raw bits, so a parsed
//! file serializes back byte for byte, including fields this module knows nothing about.

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Map(Map),
    Str(Vec<u8>),
    Int32(u32),
    Float32(u32),
    Ptr(u32),
    WStr(Vec<u8>),
    Color(u32),
    UInt64(u64),
    Int64(u64),
}

/// An ordered map (Steam keeps and expects field order).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map(pub Vec<(Vec<u8>, Value)>);

/// A whole binary VDF file.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub root: Map,
    /// Bytes after the root's terminator, if any (kept for an exact round trip).
    pub trailer: Vec<u8>,
    /// Whether the root map had its `0x08` terminator (some writers omit it).
    pub terminated: bool,
}

impl Map {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| eq_key(k, key)).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0.iter_mut().find(|(k, _)| eq_key(k, key)).map(|(_, v)| v)
    }

    /// Replace the value under `key` (matched case-insensitively, keeping the existing
    /// spelling and position), or append it.
    pub fn set(&mut self, key: &str, value: Value) {
        match self.get_mut(key) {
            Some(v) => *v = value,
            None => self.0.push((key.as_bytes().to_vec(), value)),
        }
    }

    pub fn str(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            Value::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        }
    }

    pub fn int(&self, key: &str) -> Option<u32> {
        match self.get(key)? {
            Value::Int32(n) => Some(*n),
            _ => None,
        }
    }

    pub fn map_mut(&mut self, key: &str) -> Option<&mut Map> {
        match self.get_mut(key)? {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }
}

/// Steam has written both `AppName`/`appname` and `Exe`/`exe` over the years.
fn eq_key(k: &[u8], key: &str) -> bool {
    k.eq_ignore_ascii_case(key.as_bytes())
}

pub fn str_value(s: &str) -> Value {
    Value::Str(s.as_bytes().to_vec())
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let Some(&b) = self.buf.get(self.pos) else { bail!("truncated VDF at byte {}", self.pos) };
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.buf.len() - self.pos < n {
            bail!("truncated VDF at byte {}", self.pos);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn cstr(&mut self) -> Result<Vec<u8>> {
        let Some(len) = self.buf[self.pos..].iter().position(|&b| b == 0) else {
            bail!("unterminated string at byte {}", self.pos)
        };
        let s = self.buf[self.pos..self.pos + len].to_vec();
        self.pos += len + 1;
        Ok(s)
    }

    fn wstr(&mut self) -> Result<Vec<u8>> {
        let start = self.pos;
        loop {
            let pair = self.take(2)?;
            if pair == [0, 0] {
                return Ok(self.buf[start..self.pos - 2].to_vec());
            }
        }
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Children until `0x08`. Returns whether the terminator was found (vs end of input,
    /// accepted only at the top level).
    fn map(&mut self, top: bool) -> Result<(Map, bool)> {
        let mut map = Map::default();
        loop {
            if top && self.pos == self.buf.len() {
                return Ok((map, false));
            }
            let ty = self.byte()?;
            if ty == 0x08 {
                return Ok((map, true));
            }
            let key = self.cstr()?;
            let value = match ty {
                0x00 => Value::Map(self.map(false)?.0),
                0x01 => Value::Str(self.cstr()?),
                0x02 => Value::Int32(self.u32()?),
                0x03 => Value::Float32(self.u32()?),
                0x04 => Value::Ptr(self.u32()?),
                0x05 => Value::WStr(self.wstr()?),
                0x06 => Value::Color(self.u32()?),
                0x07 => Value::UInt64(self.u64()?),
                0x0a => Value::Int64(self.u64()?),
                t => bail!("unknown VDF type 0x{t:02x} at byte {}", self.pos - key.len() - 2),
            };
            map.0.push((key, value));
        }
    }
}

pub fn parse(buf: &[u8]) -> Result<Document> {
    let mut r = Reader { buf, pos: 0 };
    let (root, terminated) = r.map(true)?;
    Ok(Document { root, trailer: buf[r.pos..].to_vec(), terminated })
}

fn write_map(out: &mut Vec<u8>, map: &Map) {
    for (key, value) in &map.0 {
        let ty = match value {
            Value::Map(_) => 0x00,
            Value::Str(_) => 0x01,
            Value::Int32(_) => 0x02,
            Value::Float32(_) => 0x03,
            Value::Ptr(_) => 0x04,
            Value::WStr(_) => 0x05,
            Value::Color(_) => 0x06,
            Value::UInt64(_) => 0x07,
            Value::Int64(_) => 0x0a,
        };
        out.push(ty);
        out.extend_from_slice(key);
        out.push(0);
        match value {
            Value::Map(m) => {
                write_map(out, m);
                out.push(0x08);
            }
            Value::Str(s) => {
                out.extend_from_slice(s);
                out.push(0);
            }
            Value::WStr(s) => {
                out.extend_from_slice(s);
                out.extend_from_slice(&[0, 0]);
            }
            Value::Int32(n) | Value::Float32(n) | Value::Ptr(n) | Value::Color(n) => out.extend_from_slice(&n.to_le_bytes()),
            Value::UInt64(n) | Value::Int64(n) => out.extend_from_slice(&n.to_le_bytes()),
        }
    }
}

pub fn serialize(doc: &Document) -> Vec<u8> {
    let mut out = Vec::new();
    write_map(&mut out, &doc.root);
    if doc.terminated {
        out.push(0x08);
    }
    out.extend_from_slice(&doc.trailer);
    out
}

// ---------------------------------------------------------------------------------------
// shortcuts.vdf

/// A `shortcuts.vdf` file: `{ "shortcuts": { "0": {…}, "1": {…}, … } }`.
pub struct Shortcuts {
    pub doc: Document,
}

/// The fields Quiver sets on a shortcut it owns. Everything else in the entry is left
/// alone (or, for a new entry, given Steam's defaults).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShortcutFields {
    pub appid: u32,
    pub app_name: String,
    /// Quoted, as Steam writes it: `"/path/to/quiver"`.
    pub exe: String,
    /// Quoted directory.
    pub start_dir: String,
    pub icon: String,
    pub launch_options: String,
}

impl Default for Shortcuts {
    fn default() -> Self {
        let mut root = Map::default();
        root.0.push((b"shortcuts".to_vec(), Value::Map(Map::default())));
        Self { doc: Document { root, trailer: Vec::new(), terminated: true } }
    }
}

impl Shortcuts {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.is_empty() {
            return Ok(Self::default());
        }
        let doc = parse(buf)?;
        if !matches!(doc.root.get("shortcuts"), Some(Value::Map(_))) {
            bail!("not a shortcuts.vdf (no \"shortcuts\" map)");
        }
        Ok(Self { doc })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serialize(&self.doc)
    }

    fn list(&self) -> &Map {
        match self.doc.root.get("shortcuts") {
            Some(Value::Map(m)) => m,
            _ => unreachable!("checked in parse"),
        }
    }

    fn list_mut(&mut self) -> &mut Map {
        self.doc.root.map_mut("shortcuts").expect("checked in parse")
    }

    /// Every entry (index key, fields).
    pub fn entries(&self) -> impl Iterator<Item = &Map> {
        self.list().0.iter().filter_map(|(_, v)| match v {
            Value::Map(m) => Some(m),
            _ => None,
        })
    }

    pub fn find(&self, appid: u32) -> Option<&Map> {
        self.entries().find(|e| e.int("appid") == Some(appid))
    }

    /// Our fields of an existing entry.
    pub fn fields(&self, appid: u32) -> Option<ShortcutFields> {
        let e = self.find(appid)?;
        Some(ShortcutFields {
            appid,
            app_name: e.str("AppName").unwrap_or_default(),
            exe: e.str("Exe").unwrap_or_default(),
            start_dir: e.str("StartDir").unwrap_or_default(),
            icon: e.str("icon").unwrap_or_default(),
            launch_options: e.str("LaunchOptions").unwrap_or_default(),
        })
    }

    /// Add or update the entry with `fields.appid`. Returns whether anything changed.
    pub fn upsert(&mut self, fields: &ShortcutFields) -> bool {
        let set_ours = |e: &mut Map| {
            e.set("appid", Value::Int32(fields.appid));
            e.set("AppName", str_value(&fields.app_name));
            e.set("Exe", str_value(&fields.exe));
            e.set("StartDir", str_value(&fields.start_dir));
            e.set("icon", str_value(&fields.icon));
            e.set("LaunchOptions", str_value(&fields.launch_options));
        };
        let list = self.list_mut();
        for (_, v) in list.0.iter_mut() {
            if let Value::Map(e) = v {
                if e.int("appid") == Some(fields.appid) {
                    let before = e.clone();
                    set_ours(e);
                    return *e != before;
                }
            }
        }
        // A new entry, with the fields and order current Steam clients write.
        let mut e = Map::default();
        e.set("appid", Value::Int32(fields.appid));
        e.set("AppName", str_value(&fields.app_name));
        e.set("Exe", str_value(&fields.exe));
        e.set("StartDir", str_value(&fields.start_dir));
        e.set("icon", str_value(&fields.icon));
        e.set("ShortcutPath", str_value(""));
        e.set("LaunchOptions", str_value(&fields.launch_options));
        e.set("IsHidden", Value::Int32(0));
        e.set("AllowDesktopConfig", Value::Int32(1));
        e.set("AllowOverlay", Value::Int32(1));
        e.set("OpenVR", Value::Int32(0));
        e.set("Devkit", Value::Int32(0));
        e.set("DevkitGameID", str_value(""));
        e.set("DevkitOverrideAppID", Value::Int32(0));
        e.set("LastPlayTime", Value::Int32(0));
        e.set("FlatpakAppID", str_value(""));
        e.set("sortas", str_value(""));
        e.set("tags", Value::Map(Map::default()));
        let index = list.0.len().to_string();
        list.0.push((index.into_bytes(), Value::Map(e)));
        true
    }

    /// Remove the entry with `appid`, renumbering the rest (`"0"`, `"1"`, …). Returns
    /// whether it was there.
    pub fn remove(&mut self, appid: u32) -> bool {
        let list = self.list_mut();
        let before = list.0.len();
        list.0.retain(|(_, v)| !matches!(v, Value::Map(e) if e.int("appid") == Some(appid)));
        if list.0.len() == before {
            return false;
        }
        for (i, (k, _)) in list.0.iter_mut().enumerate() {
            *k = i.to_string().into_bytes();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check the parser against real files without changing them (read-only):
    /// `QUIVER_VDF_CHECK=<path>[:<path>…] cargo test -p ubi-core real_file -- --ignored`
    #[test]
    #[ignore]
    fn real_file_round_trips() {
        let paths = std::env::var("QUIVER_VDF_CHECK").expect("set QUIVER_VDF_CHECK");
        for p in paths.split(':') {
            let bytes = std::fs::read(p).unwrap();
            let sc = Shortcuts::parse(&bytes).unwrap();
            assert_eq!(sc.to_bytes(), bytes, "{p}");
            for e in sc.entries() {
                eprintln!("{p}: appid {:>10} {:?}", e.int("appid").unwrap_or_default(), e.str("AppName").unwrap_or_default());
            }
        }
    }

    /// Build an entry the way Steam writes one (all the fields of a real client's file),
    /// plus some fields this module doesn't know.
    fn steam_entry(out: &mut Vec<u8>, index: &str, appid: u32, name: &str, tags: &[&str], extra: bool) {
        let s = |out: &mut Vec<u8>, k: &str, v: &str| {
            out.push(1);
            out.extend_from_slice(k.as_bytes());
            out.push(0);
            out.extend_from_slice(v.as_bytes());
            out.push(0);
        };
        let i = |out: &mut Vec<u8>, k: &str, v: u32| {
            out.push(2);
            out.extend_from_slice(k.as_bytes());
            out.push(0);
            out.extend_from_slice(&v.to_le_bytes());
        };
        out.push(0);
        out.extend_from_slice(index.as_bytes());
        out.push(0);
        i(out, "appid", appid);
        s(out, "AppName", name);
        s(out, "Exe", "\"/usr/bin/true\"");
        s(out, "StartDir", "\"/home/u/\"");
        s(out, "icon", "");
        s(out, "ShortcutPath", "");
        s(out, "LaunchOptions", "SDL_VIDEODRIVER=x11 %command%");
        i(out, "IsHidden", 0);
        i(out, "AllowDesktopConfig", 1);
        i(out, "AllowOverlay", 1);
        i(out, "OpenVR", 0);
        i(out, "Devkit", 0);
        s(out, "DevkitGameID", "");
        i(out, "DevkitOverrideAppID", 0);
        i(out, "LastPlayTime", 0x6a81_2c46);
        s(out, "FlatpakAppID", "");
        s(out, "sortas", "");
        if extra {
            // Unknown future fields of every type.
            out.push(3);
            out.extend_from_slice(b"SomeFloat\0");
            out.extend_from_slice(&1.5f32.to_le_bytes());
            out.push(7);
            out.extend_from_slice(b"SomeU64\0");
            out.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
            out.push(0x0a);
            out.extend_from_slice(b"SomeI64\0");
            out.extend_from_slice(&(-2i64).to_le_bytes());
            out.push(5);
            out.extend_from_slice(b"SomeWide\0");
            out.extend_from_slice(&[b'h', 0, b'i', 0, 0, 0]);
            out.push(6);
            out.extend_from_slice(b"SomeColor\0");
            out.extend_from_slice(&0xff00_80ffu32.to_le_bytes());
            out.push(4);
            out.extend_from_slice(b"SomePtr\0");
            out.extend_from_slice(&7u32.to_le_bytes());
            // Non-UTF-8 string bytes.
            out.push(1);
            out.extend_from_slice(b"Latin1\0caf\xe9\0");
            out.push(0);
            out.extend_from_slice(b"Nested\0");
            out.push(1);
            out.extend_from_slice(b"inner\0x\0");
            out.push(8);
        }
        out.push(0);
        out.extend_from_slice(b"tags\0");
        for (n, t) in tags.iter().enumerate() {
            s(out, &n.to_string(), t);
        }
        out.push(8); // tags
        out.push(8); // entry
    }

    fn fixture() -> Vec<u8> {
        let mut out = b"\0shortcuts\0".to_vec();
        steam_entry(&mut out, "0", 0xd738_5f4a, "Jak and Daxter", &[], false);
        steam_entry(&mut out, "1", 0xd6c2_369a, "EA App", &["favorite", "Lutris"], true);
        steam_entry(&mut out, "2", 0xa849_02ee, "Big Walk VR", &["VR"], false);
        out.extend_from_slice(&[8, 8]);
        out
    }

    #[test]
    fn round_trips_byte_exact() {
        let bytes = fixture();
        let doc = parse(&bytes).unwrap();
        assert!(doc.terminated);
        assert_eq!(serialize(&doc), bytes);
        let sc = Shortcuts::parse(&bytes).unwrap();
        assert_eq!(sc.entries().count(), 3);
        assert_eq!(sc.to_bytes(), bytes);
        let e = sc.find(0xd6c2_369a).unwrap();
        assert_eq!(e.str("AppName").as_deref(), Some("EA App"));
        let Some(Value::Map(tags)) = e.get("tags") else { panic!() };
        assert_eq!(tags.str("1").as_deref(), Some("Lutris"));
        assert_eq!(e.get("SomeU64"), Some(&Value::UInt64(0x1122_3344_5566_7788)));
    }

    #[test]
    fn round_trips_odd_layouts() {
        // Missing root terminator, and trailing bytes after it.
        let mut bytes = fixture();
        bytes.pop();
        assert_eq!(serialize(&parse(&bytes).unwrap()), bytes);
        let mut bytes = fixture();
        bytes.extend_from_slice(b"\0\0");
        assert_eq!(serialize(&parse(&bytes).unwrap()), bytes);
        // Lowercase key spellings from older clients are found and kept.
        let old = b"\0shortcuts\0\x000\0\x02appid\0\x01\x02\x03\x04\x01appname\0Old\0\x01exe\0\"x\"\0\x08\x08\x08";
        let mut sc = Shortcuts::parse(old).unwrap();
        assert_eq!(sc.to_bytes(), old);
        assert_eq!(sc.fields(0x0403_0201).unwrap().app_name, "Old");
        let f = ShortcutFields { app_name: "New".into(), ..sc.fields(0x0403_0201).unwrap() };
        assert!(sc.upsert(&f));
        let e = sc.find(0x0403_0201).unwrap();
        assert_eq!(e.0[1].0, b"appname");
        assert_eq!(e.str("appname").as_deref(), Some("New"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"\0shortcuts\0\x09bad\0").is_err());
        assert!(parse(b"\0shortcuts\0\x01name\0unterminated").is_err());
        assert!(Shortcuts::parse(b"\x01other\0x\0\x08").is_err());
        assert!(parse(b"\0shortcuts\0\x02appid\0\x01\x02").is_err());
    }

    fn ours(appid: u32, name: &str) -> ShortcutFields {
        ShortcutFields {
            appid,
            app_name: name.into(),
            exe: "\"/opt/quiver/quiver\"".into(),
            start_dir: "\"/opt/quiver/\"".into(),
            icon: "/icons/x.png".into(),
            launch_options: "play 3539".into(),
        }
    }

    #[test]
    fn upsert_update_remove() {
        let bytes = fixture();
        let mut sc = Shortcuts::parse(&bytes).unwrap();
        let appid = 0x8000_1234;
        assert!(sc.upsert(&ours(appid, "Origins")));
        assert_eq!(sc.entries().count(), 4);
        // Existing entries untouched: the old file is a prefix of the new one, minus the
        // closing `08 08`.
        let new = sc.to_bytes();
        assert_eq!(&new[..bytes.len() - 2], &bytes[..bytes.len() - 2]);
        let reparsed = Shortcuts::parse(&new).unwrap();
        assert_eq!(reparsed.fields(appid).unwrap(), ours(appid, "Origins"));
        assert_eq!(reparsed.find(appid).unwrap().int("AllowOverlay"), Some(1));
        assert_eq!(reparsed.list().0[3].0, b"3");

        // Rename: same entry, unknown fields kept.
        let mut sc = reparsed;
        assert!(!sc.upsert(&ours(appid, "Origins")));
        assert!(sc.upsert(&ours(appid, "AC Origins")));
        assert_eq!(sc.entries().count(), 4);
        assert_eq!(sc.fields(appid).unwrap().app_name, "AC Origins");

        // Update an entry that has unknown fields: they survive.
        let mut f = sc.fields(0xd6c2_369a).unwrap();
        f.app_name = "EA".into();
        sc.upsert(&f);
        assert_eq!(sc.find(0xd6c2_369a).unwrap().get("SomeI64"), Some(&Value::Int64((-2i64) as u64)));

        // Remove the middle entry: the rest are renumbered and otherwise unchanged.
        assert!(sc.remove(0xd6c2_369a));
        assert!(!sc.remove(0xd6c2_369a));
        let keys: Vec<_> = sc.list().0.iter().map(|(k, _)| String::from_utf8(k.clone()).unwrap()).collect();
        assert_eq!(keys, ["0", "1", "2"]);
        assert!(sc.remove(appid));
        // Back to the original file without the removed entry.
        let mut expected = b"\0shortcuts\0".to_vec();
        steam_entry(&mut expected, "0", 0xd738_5f4a, "Jak and Daxter", &[], false);
        steam_entry(&mut expected, "1", 0xa849_02ee, "Big Walk VR", &["VR"], false);
        expected.extend_from_slice(&[8, 8]);
        assert_eq!(sc.to_bytes(), expected);
    }

    #[test]
    fn empty_file() {
        let mut sc = Shortcuts::parse(b"").unwrap();
        assert_eq!(sc.to_bytes(), b"\0shortcuts\0\x08\x08");
        sc.upsert(&ours(0x8000_0001, "A"));
        let sc = Shortcuts::parse(&sc.to_bytes()).unwrap();
        assert_eq!(sc.fields(0x8000_0001).unwrap().app_name, "A");
    }
}
