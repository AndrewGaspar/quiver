//! Epic's build manifests (design/STORE-EPIC.md §3 C4): which files a build has, which
//! chunks each is made of, and how to launch it.
//!
//! Two encodings, read by [`parse`]:
//!
//! - **binary** (magic `0x44BEC00C`): a header, then a zlib-compressed body (SHA-1 checked)
//!   of four sections, each starting with its own size so newer fields can be skipped:
//!   meta (app, build version, launch exe and command, prerequisites), the chunk list
//!   (column-wise: guids, rolling hashes, SHA-1s, groups, window sizes, download sizes; from
//!   feature level 22 also each chunk's secret guid, compressed window size and AES-GCM
//!   tag), the file list (names, symlink targets, SHA-1s, flags, install tags, chunk parts;
//!   MD5 and MIME from level 19, SHA-256 from 20) and custom fields. From level 24 a
//!   manifest may be **encrypted**: launch exe, command, prerequisites and every file name
//!   move into an AES-GCM section keyed by a secret the manifest API returns next to the
//!   manifest's URL ([`Secrets`]).
//! - **JSON** (older builds): the same data with numbers as "blobs", strings of decimal
//!   byte triplets, little-endian.
//!
//! Every read is bounds-checked: a truncated or hostile manifest is an error, never a panic
//! or a huge allocation. The layout follows legendary's reader (read as a spec; no code is
//! shared). [`write`] produces the binary form, for fixtures and tests.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use sha1::{Digest, Sha1};

/// A 128-bit id as Epic stores it: four little-endian `u32`s.
pub type Guid = [u32; 4];

/// Decryption keys from the manifest API (`elements[0].secrets`): the secret guid as 32
/// uppercase hex digits (each `u32` as `{:08X}`) to the AES key in hex.
pub type Secrets = HashMap<String, String>;

pub const MANIFEST_MAGIC: u32 = 0x44BE_C00C;
/// Bodies larger than this are refused (real manifests are a few MiB).
const MAX_BODY: usize = 512 << 20;

/// A parsed manifest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    /// The feature level: what the chunk paths and the newer fields depend on.
    pub version: u32,
    pub meta: Meta,
    pub chunks: Vec<ChunkInfo>,
    pub files: Vec<FileEntry>,
    pub custom: BTreeMap<String, String>,
    /// The header's serialisation version (usually the same as `version`).
    pub header_version: u32,
    /// Index of `chunks` by guid (built by [`parse`]; [`Manifest::chunk`] falls back to a scan).
    #[doc(hidden)]
    pub index: HashMap<Guid, usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub app_name: String,
    pub build_version: String,
    /// Relative to the install folder, with `\` or `/`.
    pub launch_exe: String,
    pub launch_command: String,
    pub prereq_ids: Vec<String>,
    pub prereq_name: String,
    pub prereq_path: String,
    pub prereq_args: String,
    pub build_id: String,
    pub uninstall_action_path: String,
    pub uninstall_action_args: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkInfo {
    pub guid: Guid,
    /// Epic's rolling hash of the data (part of the chunk's path; not checked).
    pub hash: u64,
    /// SHA-1 of the chunk's data (the whole window).
    pub sha1: [u8; 20],
    /// The download path's group folder (`00`..`99`).
    pub group: u8,
    /// Uncompressed size.
    pub window_size: u32,
    /// Download size.
    pub file_size: u64,
    /// The key the chunk is encrypted with (feature level 22+); `None` or zero: plain.
    pub secret_guid: Option<Guid>,
    pub window_size_compressed: u32,
    pub encryption_tag: Option<[u8; 16]>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileEntry {
    /// As the manifest names it (usually `/`-separated, sometimes `\`).
    pub path: String,
    pub symlink_target: String,
    pub sha1: [u8; 20],
    pub flags: u8,
    pub install_tags: Vec<String>,
    pub parts: Vec<ChunkPart>,
    /// The sum of the parts' sizes.
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkPart {
    pub guid: Guid,
    /// Where in the chunk's data the part starts.
    pub offset: u32,
    pub size: u32,
    /// Where in the file the part goes.
    pub file_offset: u64,
}

impl FileEntry {
    pub fn read_only(&self) -> bool {
        self.flags & 0x1 != 0
    }

    pub fn compressed(&self) -> bool {
        self.flags & 0x2 != 0
    }

    pub fn executable(&self) -> bool {
        self.flags & 0x4 != 0
    }

    /// Whether the file belongs to an install with these tags: `None` is every file; a
    /// list is the untagged files plus those with any of the tags.
    pub fn wanted(&self, tags: Option<&[String]>) -> bool {
        match tags {
            None => true,
            Some(t) => self.install_tags.is_empty() || self.install_tags.iter().any(|x| t.contains(x)),
        }
    }
}

/// The chunk folder for a manifest feature level.
pub fn chunk_dir(version: u32) -> &'static str {
    match version {
        22.. => "ChunksV5",
        15.. => "ChunksV4",
        6.. => "ChunksV3",
        3.. => "ChunksV2",
        _ => "Chunks",
    }
}

/// A guid as 32 uppercase hex digits (the key form of [`Secrets`]).
pub fn guid_hex(g: &Guid) -> String {
    g.iter().map(|x| format!("{x:08X}")).collect()
}

fn guid_bytes(g: &Guid) -> [u8; 16] {
    let mut b = [0u8; 16];
    for (i, x) in g.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&x.to_le_bytes());
    }
    b
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The group Epic files a chunk under: CRC-32 of its guid's bytes, mod 100.
pub fn group_of(g: &Guid) -> u8 {
    (crc32fast::hash(&guid_bytes(g)) % 100) as u8
}

impl ChunkInfo {
    /// The chunk's path under a base URL.
    pub fn path(&self, manifest_version: u32) -> String {
        let dir = chunk_dir(manifest_version);
        if manifest_version >= 22 {
            let secret = match self.secret_guid {
                Some(s) if s != [0; 4] => b64(&guid_bytes(&s)),
                _ => "plain".to_owned(),
            };
            return format!("{dir}/{secret}/{:02}/{}_{}.chunk", self.group, b64(&self.hash.to_le_bytes()), b64(&guid_bytes(&self.guid)));
        }
        format!("{dir}/{:02}/{:016X}_{}.chunk", self.group, self.hash, guid_hex(&self.guid))
    }

    /// Whether the chunk needs a key.
    pub fn encrypted(&self) -> bool {
        self.secret_guid.is_some_and(|s| s != [0; 4])
    }
}

impl Manifest {
    pub fn chunk(&self, guid: &Guid) -> Option<&ChunkInfo> {
        match self.index.get(guid) {
            Some(&i) => self.chunks.get(i).filter(|c| c.guid == *guid),
            None if self.index.is_empty() => self.chunks.iter().find(|c| c.guid == *guid),
            None => None,
        }
    }

    /// Rebuild the guid index (after changing `chunks` by hand).
    pub fn reindex(&mut self) {
        self.index = self.chunks.iter().enumerate().map(|(i, c)| (c.guid, i)).collect();
    }

    pub fn files_for(&self, tags: Option<&[String]>) -> Vec<&FileEntry> {
        self.files.iter().filter(|f| f.wanted(tags)).collect()
    }

    /// Bytes on disk once installed.
    pub fn install_size(&self, tags: Option<&[String]>) -> u64 {
        self.files_for(tags).iter().map(|f| f.size).sum()
    }

    /// Bytes to download for a fresh install: every chunk the files use, once.
    pub fn download_size(&self, tags: Option<&[String]>) -> u64 {
        let mut seen = std::collections::HashSet::new();
        let mut n = 0;
        for f in self.files_for(tags) {
            for p in &f.parts {
                if seen.insert(p.guid) {
                    n += self.chunk(&p.guid).map_or(0, |c| c.file_size);
                }
            }
        }
        n
    }

    /// Every install tag the files use.
    pub fn install_tags(&self) -> Vec<String> {
        let mut t: Vec<String> = self.files.iter().flat_map(|f| f.install_tags.iter().cloned()).collect();
        t.sort();
        t.dedup();
        t
    }
}

// ---- reading

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.remaining(), "manifest cut off (wanted {n} bytes at {}, {} left)", self.pos, self.remaining());
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length"))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.arr()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }

    fn guid(&mut self) -> Result<Guid> {
        Ok([self.u32()?, self.u32()?, self.u32()?, self.u32()?])
    }

    /// A count of elements at least `min_size` bytes each: refused if they can't fit.
    fn count(&mut self, min_size: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        ensure!(n.saturating_mul(min_size.max(1)) <= self.remaining(), "manifest count {n} doesn't fit in what's left");
        Ok(n)
    }

    /// Unreal's FString: a length counting the terminator; negative for UTF-16.
    fn fstring(&mut self) -> Result<String> {
        let len = self.i32()?;
        if len == 0 {
            return Ok(String::new());
        }
        if len < 0 {
            let chars = len.unsigned_abs() as usize;
            let raw = self.take(chars.checked_mul(2).context("string length")?)?;
            let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let units = units.strip_suffix(&[0]).unwrap_or(&units);
            return Ok(String::from_utf16_lossy(units));
        }
        let raw = self.take(len as usize)?;
        let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    /// Move to `start + size` (a section's end, as its size field says), if that's ahead.
    fn skip_to(&mut self, start: usize, size: u32, what: &str) -> Result<()> {
        let end = start.checked_add(size as usize).context("section size")?;
        ensure!(end <= self.b.len(), "manifest {what} runs past the end");
        if end > self.pos {
            self.pos = end;
        } else if end < self.pos {
            bail!("manifest {what} is longer than its size field says");
        }
        Ok(())
    }
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).take(limit as u64 + 1).read_to_end(&mut out).context("decompressing")?;
    ensure!(out.len() <= limit, "decompressed data is larger than expected");
    Ok(out)
}

fn aes_key(secrets: &Secrets, guid: &Guid, what: &str) -> Result<Vec<u8>> {
    let hex_key = secrets.get(&guid_hex(guid)).with_context(|| format!("{what} is encrypted and no key for secret {} came with it", guid_hex(guid)))?;
    let key = hex::decode(hex_key.trim()).with_context(|| format!("the key for secret {} isn't hex", guid_hex(guid)))?;
    ensure!(matches!(key.len(), 16 | 32), "the key for secret {} is {} bytes (16 or 32 expected)", guid_hex(guid), key.len());
    Ok(key)
}

/// AES-GCM (128 or 256-bit key) with a 12-byte nonce and a 16-byte tag.
pub(crate) fn gcm_decrypt(key: &[u8], nonce: &[u8], data: &[u8], tag: &[u8; 16]) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    ensure!(nonce.len() == 12, "nonce is {} bytes (12 expected)", nonce.len());
    let mut ct = Vec::with_capacity(data.len() + 16);
    ct.extend_from_slice(data);
    ct.extend_from_slice(tag);
    let nonce = aes_gcm::Nonce::from_slice(nonce);
    let payload = Payload { msg: &ct, aad: &[] };
    let out = match key.len() {
        16 => aes_gcm::Aes128Gcm::new_from_slice(key).expect("key length").decrypt(nonce, payload),
        32 => aes_gcm::Aes256Gcm::new_from_slice(key).expect("key length").decrypt(nonce, payload),
        n => bail!("AES key of {n} bytes"),
    };
    out.map_err(|_| anyhow::anyhow!("decryption failed (wrong key or damaged data)"))
}

/// AES-GCM encryption: the ciphertext and the tag.
pub(crate) fn gcm_encrypt(key: &[u8], nonce: &[u8], data: &[u8]) -> (Vec<u8>, [u8; 16]) {
    use aes_gcm::aead::{Aead, KeyInit};
    let nonce = aes_gcm::Nonce::from_slice(nonce);
    let mut out = match key.len() {
        16 => aes_gcm::Aes128Gcm::new_from_slice(key).expect("key").encrypt(nonce, data),
        _ => aes_gcm::Aes256Gcm::new_from_slice(key).expect("key").encrypt(nonce, data),
    }
    .expect("encrypt");
    let tag: [u8; 16] = out.split_off(out.len() - 16).try_into().expect("tag");
    (out, tag)
}

struct Header {
    stored_as: u8,
    secret_guid: Guid,
    encryption_tag: [u8; 16],
}

/// Read a manifest, binary or JSON. Encrypted manifests need their key in `secrets`.
pub fn parse(bytes: &[u8], secrets: &Secrets) -> Result<Manifest> {
    if bytes.first() == Some(&b'{') {
        return parse_json(bytes);
    }
    let mut r = Reader::new(bytes);
    ensure!(r.u32().ok() == Some(MANIFEST_MAGIC), "not an Epic manifest (no header magic)");
    let header_size = r.u32()?;
    let size_uncompressed = r.u32()? as usize;
    let _size_compressed = r.u32()?;
    let sha: [u8; 20] = r.arr()?;
    let stored_as = r.u8()?;
    let version = r.u32()?;
    let (mut secret_guid, mut encryption_tag) = ([0; 4], [0; 16]);
    if version >= 22 {
        secret_guid = r.guid()?;
        encryption_tag = r.arr()?;
    }
    r.skip_to(0, header_size, "header")?;
    let rest = &bytes[r.pos..];
    let body = if stored_as & 0x1 != 0 {
        ensure!(size_uncompressed <= MAX_BODY, "manifest body of {size_uncompressed} bytes is too large");
        let body = inflate(rest, size_uncompressed.max(1))?;
        ensure!(Sha1::digest(&body).as_slice() == sha, "manifest body doesn't match its SHA-1");
        body
    } else {
        ensure!(rest.len() <= MAX_BODY, "manifest body is too large");
        rest.to_vec()
    };
    let header = Header { stored_as, secret_guid, encryption_tag };
    let mut m = parse_body(&body, &header, secrets)?;
    m.header_version = version;
    m.reindex();
    Ok(m)
}

fn parse_body(body: &[u8], h: &Header, secrets: &Secrets) -> Result<Manifest> {
    let mut r = Reader::new(body);
    let mut m = Manifest::default();
    // Meta.
    let start = r.pos;
    let size = r.u32()?;
    let data_version = r.u8()?;
    let feature_level = r.u32()?;
    let _is_file_data = r.u8()?;
    let _app_id = r.u32()?;
    let meta = &mut m.meta;
    meta.app_name = r.fstring()?;
    meta.build_version = r.fstring()?;
    meta.launch_exe = r.fstring()?;
    meta.launch_command = r.fstring()?;
    let n = r.count(4)?;
    for _ in 0..n {
        meta.prereq_ids.push(r.fstring()?);
    }
    meta.prereq_name = r.fstring()?;
    meta.prereq_path = r.fstring()?;
    meta.prereq_args = r.fstring()?;
    if data_version >= 1 {
        meta.build_id = r.fstring()?;
    }
    if data_version >= 2 {
        meta.uninstall_action_path = r.fstring()?;
        meta.uninstall_action_args = r.fstring()?;
    }
    r.skip_to(start, size, "meta")?;
    m.version = feature_level;

    // Chunk list, column by column.
    let start = r.pos;
    let size = r.u32()?;
    let _cdl_version = r.u8()?;
    let per_chunk = 16 + 8 + 20 + 1 + 4 + 8 + if feature_level >= 22 { 16 + 4 + 16 } else { 0 };
    let n = r.count(per_chunk)?;
    let mut chunks = vec![ChunkInfo::default(); n];
    for c in &mut chunks {
        c.guid = r.guid()?;
    }
    for c in &mut chunks {
        c.hash = r.u64()?;
    }
    for c in &mut chunks {
        c.sha1 = r.arr()?;
    }
    for c in &mut chunks {
        c.group = r.u8()?;
    }
    for c in &mut chunks {
        c.window_size = r.u32()?;
    }
    for c in &mut chunks {
        c.file_size = u64::try_from(r.i64()?).context("negative chunk size")?;
    }
    if feature_level >= 22 {
        for c in &mut chunks {
            let g = r.guid()?;
            c.secret_guid = (g != [0; 4]).then_some(g);
        }
        for c in &mut chunks {
            c.window_size_compressed = r.u32()?;
        }
        for c in &mut chunks {
            let t: [u8; 16] = r.arr()?;
            c.encryption_tag = (t != [0; 16]).then_some(t);
        }
    }
    r.skip_to(start, size, "chunk list")?;
    m.chunks = chunks;

    // File list.
    let start = r.pos;
    let size = r.u32()?;
    let fml_version = r.u8()?;
    let n = r.count(4 + 4 + 20 + 1 + 4 + 4)?;
    let mut files = vec![FileEntry::default(); n];
    for f in &mut files {
        f.path = r.fstring()?;
    }
    for f in &mut files {
        f.symlink_target = r.fstring()?;
    }
    for f in &mut files {
        f.sha1 = r.arr()?;
    }
    for f in &mut files {
        f.flags = r.u8()?;
    }
    for f in &mut files {
        let k = r.count(4)?;
        for _ in 0..k {
            f.install_tags.push(r.fstring()?);
        }
    }
    for f in &mut files {
        let k = r.count(28)?;
        let mut offset = 0u64;
        for _ in 0..k {
            let part_start = r.pos;
            let part_size = r.u32()?;
            ensure!(part_size >= 28, "manifest chunk part of {part_size} bytes");
            let guid = r.guid()?;
            let chunk_offset = r.u32()?;
            let len = r.u32()?;
            r.skip_to(part_start, part_size, "chunk part")?;
            f.parts.push(ChunkPart { guid, offset: chunk_offset, size: len, file_offset: offset });
            offset += len as u64;
        }
        f.size = offset;
    }
    if fml_version >= 1 {
        for _ in 0..n {
            if r.u32()? != 0 {
                r.take(16)?;
            }
        }
        for _ in 0..n {
            r.fstring()?;
        }
    }
    if fml_version >= 2 {
        r.take(n.checked_mul(32).context("size")?)?;
    }
    r.skip_to(start, size, "file list")?;

    // Custom fields.
    if r.remaining() > 0 {
        let start = r.pos;
        let size = r.u32()?;
        let _version = r.u8()?;
        let n = r.count(8)?;
        let keys: Vec<String> = (0..n).map(|_| r.fstring()).collect::<Result<_>>()?;
        let values: Vec<String> = (0..n).map(|_| r.fstring()).collect::<Result<_>>()?;
        m.custom = keys.into_iter().zip(values).collect();
        r.skip_to(start, size, "custom fields")?;
    }

    // The encrypted part (feature level 24+): names and launch data.
    if h.stored_as & 0x2 != 0 {
        ensure!(feature_level >= 24, "manifest says it's encrypted at feature level {feature_level}");
        let start = r.pos;
        let size = r.u32()?;
        let _version = r.u8()?;
        let end = start.checked_add(size as usize).context("size")?;
        ensure!(end <= body.len() && end > r.pos, "manifest encrypted part runs past the end");
        // The cipher-size field isn't trusted (writers disagree on what it counts): the
        // header and ciphertext are read within the section.
        let _cipher_size = r.u32()?;
        let mut s = Reader::new(&body[..end]);
        s.pos = r.pos;
        let hs = s.pos;
        let hsize = s.u32()?;
        let _hversion = s.u32()?;
        let estored = s.u8()?;
        let uncompressed = s.u32()? as usize;
        let _compressed = s.u32()?;
        let iv_len = s.count(1)?;
        let iv = s.take(iv_len)?.to_vec();
        s.skip_to(hs, hsize, "encrypted header")?;
        let clen = s.count(1)?;
        let cipher = s.take(clen)?;
        let key = aes_key(secrets, &h.secret_guid, "this manifest")?;
        let mut plain = gcm_decrypt(&key, &iv, cipher, &h.encryption_tag)?;
        if estored & 0x1 != 0 {
            ensure!(uncompressed <= MAX_BODY, "encrypted part too large");
            plain = inflate(&plain, uncompressed.max(1))?;
        }
        let mut d = Reader::new(&plain);
        m.meta.launch_exe = d.fstring()?;
        m.meta.launch_command = d.fstring()?;
        let k = d.count(4)?;
        m.meta.prereq_ids = (0..k).map(|_| d.fstring()).collect::<Result<_>>()?;
        m.meta.prereq_name = d.fstring()?;
        m.meta.prereq_path = d.fstring()?;
        m.meta.prereq_args = d.fstring()?;
        m.meta.uninstall_action_path = d.fstring()?;
        m.meta.uninstall_action_args = d.fstring()?;
        for f in &mut files {
            f.path = d.fstring()?;
            f.symlink_target = d.fstring()?;
        }
    }
    m.files = files;
    Ok(m)
}

// ---- JSON manifests

/// A "blob": decimal byte triplets, little-endian.
fn blob(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    ensure!(s.len().is_multiple_of(3) && s.bytes().all(|b| b.is_ascii_digit()), "bad number blob {s:?}");
    s.as_bytes()
        .chunks(3)
        .map(|c| {
            let v: u32 = std::str::from_utf8(c).expect("digits").parse().expect("digits");
            u8::try_from(v).map_err(|_| anyhow::anyhow!("bad number blob {s:?}"))
        })
        .collect()
}

fn blob_num(v: &serde_json::Value) -> Result<u64> {
    let s = v.as_str().context("expected a number blob")?;
    let b = blob(s)?;
    ensure!(b.len() <= 8, "number blob too long");
    Ok(b.iter().rev().fold(0u64, |acc, &x| (acc << 8) | x as u64))
}

/// A guid written as 32 hex digits (four big-endian-printed `u32`s).
pub fn parse_guid(s: &str) -> Result<Guid> {
    let s = s.trim().replace('-', "");
    ensure!(s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()), "bad guid {s:?}");
    let mut g = [0u32; 4];
    for (i, x) in g.iter_mut().enumerate() {
        *x = u32::from_str_radix(&s[i * 8..i * 8 + 8], 16)?;
    }
    Ok(g)
}

fn jstr(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_owned()
}

fn parse_json(bytes: &[u8]) -> Result<Manifest> {
    let v: serde_json::Value = serde_json::from_slice(bytes).context("reading a JSON manifest")?;
    let mut m = Manifest {
        version: v.get("ManifestFileVersion").map(blob_num).transpose()?.unwrap_or(0) as u32,
        meta: Meta {
            app_name: jstr(&v, "AppNameString"),
            build_version: jstr(&v, "BuildVersionString"),
            launch_exe: jstr(&v, "LaunchExeString"),
            launch_command: jstr(&v, "LaunchCommand"),
            prereq_ids: v.get("PrereqIds").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
            prereq_name: jstr(&v, "PrereqName"),
            prereq_path: jstr(&v, "PrereqPath"),
            prereq_args: jstr(&v, "PrereqArgs"),
            ..Default::default()
        },
        ..Default::default()
    };
    m.header_version = m.version;
    let map = |k: &str| v.get(k).and_then(|x| x.as_object()).cloned().unwrap_or_default();
    let (hashes, shas, groups, sizes) = (map("ChunkHashList"), map("ChunkShaList"), map("DataGroupList"), map("ChunkFilesizeList"));
    for (g, h) in &hashes {
        let guid = parse_guid(g)?;
        let mut sha1 = [0u8; 20];
        if let Some(s) = shas.get(g).and_then(|s| s.as_str()) {
            let b = hex::decode(s).context("chunk SHA-1")?;
            ensure!(b.len() == 20, "chunk SHA-1 length");
            sha1.copy_from_slice(&b);
        }
        m.chunks.push(ChunkInfo {
            guid,
            hash: blob_num(h)?,
            sha1,
            group: groups.get(g).map(blob_num).transpose()?.map(|x| x as u8).unwrap_or_else(|| group_of(&guid)),
            window_size: 1 << 20,
            file_size: sizes.get(g).map(blob_num).transpose()?.unwrap_or(0),
            ..Default::default()
        });
    }
    for f in v.get("FileManifestList").and_then(|x| x.as_array()).map(Vec::as_slice).unwrap_or_default() {
        let mut e = FileEntry { path: jstr(f, "Filename"), symlink_target: jstr(f, "SymlinkTarget"), ..Default::default() };
        if let Some(h) = f.get("FileHash").and_then(|x| x.as_str()) {
            let b = blob(h)?;
            ensure!(b.len() == 20, "file hash length");
            e.sha1.copy_from_slice(&b);
        }
        let flag = |k: &str| f.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        e.flags = u8::from(flag("bIsReadOnly")) | (u8::from(flag("bIsCompressed")) << 1) | (u8::from(flag("bIsUnixExecutable")) << 2);
        e.install_tags = f.get("InstallTags").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        let mut offset = 0u64;
        for p in f.get("FileChunkParts").and_then(|x| x.as_array()).map(Vec::as_slice).unwrap_or_default() {
            let guid = parse_guid(p.get("Guid").and_then(|x| x.as_str()).context("chunk part guid")?)?;
            let size = p.get("Size").map(blob_num).transpose()?.unwrap_or(0) as u32;
            e.parts.push(ChunkPart { guid, offset: p.get("Offset").map(blob_num).transpose()?.unwrap_or(0) as u32, size, file_offset: offset });
            offset += size as u64;
        }
        e.size = offset;
        m.files.push(e);
    }
    if let Some(c) = v.get("CustomFields").and_then(|x| x.as_object()) {
        m.custom = c.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect();
    }
    m.reindex();
    Ok(m)
}

// ---- writing

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn guid(&mut self, g: &Guid) {
        g.iter().for_each(|x| self.u32(*x));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn fstring(&mut self, s: &str) {
        if s.is_empty() {
            self.u32(0);
        } else if s.is_ascii() {
            self.u32(s.len() as u32 + 1);
            self.bytes(s.as_bytes());
            self.u8(0);
        } else {
            let units: Vec<u16> = s.encode_utf16().collect();
            self.0.extend_from_slice(&(-(units.len() as i32 + 1)).to_le_bytes());
            units.iter().for_each(|u| self.bytes(&u.to_le_bytes()));
            self.bytes(&[0, 0]);
        }
    }
    /// Start a section with a size placeholder; [`Writer::end`] fills it in.
    fn begin(&mut self) -> usize {
        let at = self.0.len();
        self.u32(0);
        at
    }
    fn end(&mut self, at: usize) {
        let size = (self.0.len() - at) as u32;
        self.0[at..at + 4].copy_from_slice(&size.to_le_bytes());
    }
}

/// What [`write_with`] encrypts with: the manifest's secret guid, its key and the nonce.
pub struct Encryption<'a> {
    pub secret_guid: Guid,
    pub key: &'a [u8],
    pub iv: [u8; 12],
}

/// The binary form of a manifest (at `m.version`, at least 18). Fixtures and tests.
pub fn write(m: &Manifest, compress: bool) -> Vec<u8> {
    write_with(m, compress, None)
}

/// [`write`], optionally encrypted (feature level 24+ only; the version is raised to 24).
pub fn write_with(m: &Manifest, compress: bool, enc: Option<Encryption<'_>>) -> Vec<u8> {
    let version = if enc.is_some() { m.version.max(24) } else { m.version.max(18) };
    let data_version: u8 = if version >= 21 { 2 } else { 1 };
    let fml_version: u8 = if version >= 20 { 2 } else if version >= 19 { 1 } else { 0 };
    let hide = enc.is_some();
    let mut w = Writer(Vec::new());
    // Meta.
    let at = w.begin();
    w.u8(data_version);
    w.u32(version);
    w.u8(0);
    w.u32(0);
    w.fstring(&m.meta.app_name);
    w.fstring(&m.meta.build_version);
    let blank = |s: &str| if hide { String::new() } else { s.to_owned() };
    w.fstring(&blank(&m.meta.launch_exe));
    w.fstring(&blank(&m.meta.launch_command));
    let prereqs: &[String] = if hide { &[] } else { &m.meta.prereq_ids };
    w.u32(prereqs.len() as u32);
    prereqs.iter().for_each(|p| w.fstring(p));
    w.fstring(&blank(&m.meta.prereq_name));
    w.fstring(&blank(&m.meta.prereq_path));
    w.fstring(&blank(&m.meta.prereq_args));
    w.fstring(&m.meta.build_id);
    if data_version >= 2 {
        w.fstring(&blank(&m.meta.uninstall_action_path));
        w.fstring(&blank(&m.meta.uninstall_action_args));
    }
    w.end(at);
    // Chunk list.
    let at = w.begin();
    w.u8(0);
    w.u32(m.chunks.len() as u32);
    m.chunks.iter().for_each(|c| w.guid(&c.guid));
    m.chunks.iter().for_each(|c| w.u64(c.hash));
    m.chunks.iter().for_each(|c| w.bytes(&c.sha1));
    m.chunks.iter().for_each(|c| w.u8(c.group));
    m.chunks.iter().for_each(|c| w.u32(c.window_size));
    m.chunks.iter().for_each(|c| w.u64(c.file_size));
    if version >= 22 {
        m.chunks.iter().for_each(|c| w.guid(&c.secret_guid.unwrap_or_default()));
        m.chunks.iter().for_each(|c| w.u32(c.window_size_compressed));
        m.chunks.iter().for_each(|c| w.bytes(&c.encryption_tag.unwrap_or_default()));
    }
    w.end(at);
    // File list.
    let at = w.begin();
    w.u8(fml_version);
    w.u32(m.files.len() as u32);
    m.files.iter().for_each(|f| w.fstring(&blank(&f.path)));
    m.files.iter().for_each(|f| w.fstring(&blank(&f.symlink_target)));
    m.files.iter().for_each(|f| w.bytes(&f.sha1));
    m.files.iter().for_each(|f| w.u8(f.flags));
    for f in &m.files {
        w.u32(f.install_tags.len() as u32);
        f.install_tags.iter().for_each(|t| w.fstring(t));
    }
    for f in &m.files {
        w.u32(f.parts.len() as u32);
        for p in &f.parts {
            w.u32(28);
            w.guid(&p.guid);
            w.u32(p.offset);
            w.u32(p.size);
        }
    }
    if fml_version >= 1 {
        m.files.iter().for_each(|_| w.u32(0));
        m.files.iter().for_each(|_| w.fstring(""));
    }
    if fml_version >= 2 {
        m.files.iter().for_each(|_| w.bytes(&[0; 32]));
    }
    w.end(at);
    // Custom fields.
    let at = w.begin();
    w.u8(0);
    w.u32(m.custom.len() as u32);
    m.custom.keys().for_each(|k| w.fstring(k));
    m.custom.values().for_each(|v| w.fstring(v));
    w.end(at);
    let (mut stored_as, mut secret, mut tag) = (0u8, [0u32; 4], [0u8; 16]);
    if let Some(e) = &enc {
        let mut d = Writer(Vec::new());
        d.fstring(&m.meta.launch_exe);
        d.fstring(&m.meta.launch_command);
        d.u32(m.meta.prereq_ids.len() as u32);
        m.meta.prereq_ids.iter().for_each(|p| d.fstring(p));
        d.fstring(&m.meta.prereq_name);
        d.fstring(&m.meta.prereq_path);
        d.fstring(&m.meta.prereq_args);
        d.fstring(&m.meta.uninstall_action_path);
        d.fstring(&m.meta.uninstall_action_args);
        for f in &m.files {
            d.fstring(&f.path);
            d.fstring(&f.symlink_target);
        }
        let (cipher, t) = gcm_encrypt(e.key, &e.iv, &d.0);
        let at = w.begin();
        w.u8(0);
        let cat = w.begin();
        let hat = w.begin();
        w.u32(0);
        w.u8(0);
        w.u32(d.0.len() as u32);
        w.u32(d.0.len() as u32);
        w.u32(12);
        w.bytes(&e.iv);
        w.end(hat);
        w.u32(cipher.len() as u32);
        w.bytes(&cipher);
        w.end(cat);
        w.end(at);
        stored_as |= 0x2;
        secret = e.secret_guid;
        tag = t;
    }
    let body = w.0;
    let sha: [u8; 20] = Sha1::digest(&body).into();
    let data = if compress {
        stored_as |= 0x1;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, &body).expect("zlib");
        z.finish().expect("zlib")
    } else {
        body.clone()
    };
    let mut out = Writer(Vec::new());
    out.u32(MANIFEST_MAGIC);
    out.u32(if version >= 22 { 41 + 32 } else { 41 });
    out.u32(body.len() as u32);
    out.u32(data.len() as u32);
    out.bytes(&sha);
    out.u8(stored_as);
    out.u32(version);
    if version >= 22 {
        out.guid(&secret);
        out.bytes(&tag);
    }
    out.bytes(&data);
    out.0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small manifest of `files` (path, content, tags), cut into chunks of `window`
    /// bytes, with the chunks' encoded bytes keyed by their path.
    pub fn build(version: u32, files: &[(&str, &[u8], &[&str])], window: usize, secret: Option<(Guid, [u8; 32])>) -> (Manifest, HashMap<String, Vec<u8>>) {
        let mut stream = Vec::new();
        let mut spans = Vec::new();
        for (_, data, _) in files {
            spans.push((stream.len(), data.len()));
            stream.extend_from_slice(data);
        }
        let mut m = Manifest { version, header_version: version, ..Default::default() };
        m.meta = Meta { app_name: "Fixture".into(), build_version: format!("1.0-v{version}"), launch_exe: "Bin\\Game.exe".into(), launch_command: "-fixture".into(), build_id: "b".into(), ..Default::default() };
        let mut encoded = HashMap::new();
        for (i, data) in stream.chunks(window.max(1)).enumerate() {
            let guid = [0x1000 + i as u32, 0x2000, 0x3000, 0x4000 + version];
            let mut padded = data.to_vec();
            padded.resize(window, 0);
            let raw = crate::chunk::encode(guid, &padded, i % 2 == 0, secret.as_ref().map(|(g, k)| (*g, k)));
            let mut info = crate::chunk::chunk_info(guid, &padded, &raw);
            info.secret_guid = secret.map(|(g, _)| g);
            if version < 22 {
                info.window_size_compressed = 0;
            }
            encoded.insert(info.path(version), raw);
            m.chunks.push(info);
        }
        for ((path, data, tags), (start, len)) in files.iter().zip(spans) {
            let mut f = FileEntry { path: (*path).to_owned(), sha1: Sha1::digest(data).into(), install_tags: tags.iter().map(|t| (*t).to_owned()).collect(), size: len as u64, ..Default::default() };
            if path.ends_with(".exe") {
                f.flags = 0x4;
            }
            let mut pos = start;
            while pos < start + len {
                let ci = pos / window;
                let off = pos % window;
                let n = (window - off).min(start + len - pos);
                f.parts.push(ChunkPart { guid: m.chunks[ci].guid, offset: off as u32, size: n as u32, file_offset: (pos - start) as u64 });
                pos += n;
            }
            m.files.push(f);
        }
        m.custom.insert("BuildLabel".into(), "Live".into());
        m.reindex();
        (m, encoded)
    }

    fn sample(version: u32) -> Manifest {
        build(version, &[("Bin/Game.exe", b"MZ hello world, a game", &[]), ("Data/ünïcode.pak", &[7u8; 300], &["hd"]), ("empty.txt", b"", &[])], 64, None).0
    }

    #[test]
    fn round_trips_every_version() {
        for v in [17, 18, 19, 20, 21, 22, 23] {
            let mut m = sample(v);
            m.version = m.version.max(18);
            m.header_version = m.version;
            for compress in [true, false] {
                let back = parse(&write(&m, compress), &Secrets::new()).unwrap();
                assert_eq!(back, m, "v{v} compress={compress}");
            }
        }
        let m = sample(22);
        assert!(m.chunks[0].path(22).starts_with("ChunksV5/plain/"), "{}", m.chunks[0].path(22));
        let old = sample(18);
        let p = old.chunks[0].path(18);
        assert!(p.starts_with(&format!("ChunksV4/{:02}/", old.chunks[0].group)) && p.ends_with(&format!("_{}.chunk", guid_hex(&old.chunks[0].guid))), "{p}");
        assert_eq!(m.install_size(None), 22 + 300);
        assert_eq!(m.files_for(Some(&[])).len(), 2);
        assert_eq!(m.files_for(Some(&["hd".into()])).len(), 3);
        assert!(m.download_size(None) > 0 && m.files[0].executable() && !m.files[0].read_only());
    }

    #[test]
    fn encrypted_manifests_need_their_key() {
        let mut m = sample(24);
        let secret = [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD];
        let key = [9u8; 32];
        let bytes = write_with(&m, true, Some(Encryption { secret_guid: secret, key: &key, iv: [3; 12] }));
        // The names aren't in the clear.
        let e = parse(&bytes, &Secrets::new()).unwrap_err();
        assert!(e.to_string().contains(&guid_hex(&secret)), "{e}");
        let secrets = Secrets::from([(guid_hex(&secret), hex::encode(key))]);
        let back = parse(&bytes, &secrets).unwrap();
        m.header_version = 24;
        assert_eq!(back, m);
        let wrong = Secrets::from([(guid_hex(&secret), hex::encode([1u8; 32]))]);
        assert!(parse(&bytes, &wrong).unwrap_err().to_string().contains("decryption failed"));
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        for v in [18, 22] {
            let full = write(&sample(v), false);
            for cut in 0..full.len() {
                let _ = parse(&full[..cut], &Secrets::new());
            }
            // Flip bytes everywhere: errors or a manifest, never a panic.
            for i in 0..full.len() {
                let mut b = full.clone();
                b[i] ^= 0xFF;
                let _ = parse(&b, &Secrets::new());
            }
        }
        let compressed = write(&sample(18), true);
        let mut bad = compressed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(parse(&bad, &Secrets::new()).is_err());
        assert!(parse(b"nope", &Secrets::new()).is_err());
        // A huge count in a short body.
        let mut huge = write(&sample(18), false);
        let n = huge.len();
        huge.truncate(n - 20);
        assert!(parse(&huge, &Secrets::new()).is_err());
    }

    fn blob_of(v: u64, n: usize) -> String {
        (0..n).map(|i| format!("{:03}", (v >> (8 * i)) & 0xFF)).collect()
    }

    #[test]
    fn json_manifests() {
        let sha = Sha1::digest(b"hi");
        let json = serde_json::json!({
            "ManifestFileVersion": blob_of(13, 4),
            "AppNameString": "Old", "BuildVersionString": "1.2", "LaunchExeString": "Game/Old.exe", "LaunchCommand": "",
            "PrereqIds": ["vc"], "PrereqName": "VC", "PrereqPath": "Redist/vc.exe", "PrereqArgs": "/q",
            "FileManifestList": [{"Filename": "Game/Old.exe", "FileHash": sha.iter().map(|b| format!("{b:03}")).collect::<String>(),
                "FileChunkParts": [{"Guid": "0000000A0000000B0000000C0000000D", "Offset": blob_of(4, 4), "Size": blob_of(2, 4)}],
                "bIsUnixExecutable": true, "InstallTags": ["x"]}],
            "ChunkHashList": {"0000000A0000000B0000000C0000000D": blob_of(0x1122334455667788, 8)},
            "ChunkShaList": {"0000000A0000000B0000000C0000000D": hex::encode([5u8; 20])},
            "DataGroupList": {"0000000A0000000B0000000C0000000D": blob_of(42, 1)},
            "ChunkFilesizeList": {"0000000A0000000B0000000C0000000D": blob_of(1234, 8)},
            "CustomFields": {"k": "v"}
        });
        let m = parse(json.to_string().as_bytes(), &Secrets::new()).unwrap();
        assert_eq!((m.version, m.meta.app_name.as_str(), m.meta.prereq_path.as_str()), (13, "Old", "Redist/vc.exe"));
        let c = &m.chunks[0];
        assert_eq!((c.guid, c.hash, c.group, c.file_size), ([10, 11, 12, 13], 0x1122334455667788, 42, 1234));
        assert_eq!(c.path(13), "ChunksV3/42/1122334455667788_0000000A0000000B0000000C0000000D.chunk");
        let f = &m.files[0];
        assert_eq!((f.size, f.parts[0].offset, f.executable(), f.sha1.as_slice()), (2, 4, true, sha.as_slice()));
        assert_eq!(m.custom["k"], "v");
        assert!(parse(br#"{"ChunkHashList": {"zz": "1"}}"#, &Secrets::new()).is_err());
    }

    /// legendary (the reference implementation) reads what [`write`] writes the same way.
    /// Runs only where Heroic's legendary binary is installed, with a scratch config.
    #[test]
    fn legendary_reads_our_manifests() {
        let bin = std::path::Path::new("/opt/Heroic/resources/app.asar.unpacked/build/bin/x64/linux/legendary");
        if !bin.is_file() {
            eprintln!("legendary not installed: oracle test skipped");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        for v in [18, 21, 22] {
            let m = sample(v);
            let path = tmp.path().join(format!("m{v}.manifest"));
            std::fs::write(&path, write(&m, true)).unwrap();
            let run = |args: &[&str]| {
                let out = std::process::Command::new(bin)
                    .args(args)
                    .env("LEGENDARY_CONFIG_PATH", tmp.path().join("cfg"))
                    .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
                    .env("HOME", tmp.path())
                    .output()
                    .unwrap();
                assert!(out.status.success(), "legendary {args:?}: {}", String::from_utf8_lossy(&out.stderr));
                serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
            };
            let files = run(&["list-files", "--manifest", path.to_str().unwrap(), "--json"]);
            let mut ours: Vec<(String, String, u64, u64)> = m.files.iter().map(|f| (f.path.clone(), hex::encode(f.sha1), f.size, f.flags as u64)).collect();
            ours.sort_by_key(|f| f.0.to_lowercase());
            let theirs: Vec<(String, String, u64, u64)> = files
                .as_array()
                .unwrap()
                .iter()
                .map(|f| (f["filename"].as_str().unwrap().to_owned(), f["sha_hash"].as_str().unwrap().to_owned(), f["file_size"].as_u64().unwrap(), f["flags"].as_u64().unwrap()))
                .collect();
            assert_eq!(theirs, ours, "v{v}");
            let info = run(&["info", path.to_str().unwrap(), "--json", "--offline"]);
            let get = |k: &str| info["manifest"][k].clone();
            assert_eq!(get("launch_exe"), "Bin\\Game.exe", "v{v}: {info}");
            assert_eq!(get("build_version").as_str(), Some(m.meta.build_version.as_str()));
            assert_eq!(get("num_chunks").as_u64(), Some(m.chunks.len() as u64));
            assert_eq!(get("disk_size").as_u64(), Some(m.install_size(None)));
            assert_eq!(get("download_size").as_u64(), Some(m.download_size(None)));
        }
    }
}
