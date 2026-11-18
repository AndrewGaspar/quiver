//! Epic's binary manifest and chunk formats, as far as cloud saves use them
//! (design/STORE-EPIC.md §3 C9): read a save's manifest and its chunks back into files, and
//! package a save folder into a manifest and chunks to upload.
//!
//! A cloud save is the same container format as a game build: a manifest (magic
//! `0x44BEC00C`, zlib) listing files as runs of parts of chunks, and chunks (magic
//! `0xB1FE3AA2`, zlib, 1 MiB windows) named `ChunksV4/<group>/<hash>_<guid>.chunk`. The
//! layout was taken from the format as legendary and egs-api describe it (read as a
//! specification; no code from either is used). Game manifests (installs, stage 2) have
//! their own parser; this one reads only what a save needs and refuses what it doesn't
//! understand (an encrypted manifest, a chunk part pointing outside its chunk) instead of
//! guessing.
//!
//! Every length read from the network is checked before it is used: a short or corrupt
//! reply is an error, never a panic.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};

use anyhow::{Context, Result, bail, ensure};
use sha1::{Digest, Sha1};

pub const MANIFEST_MAGIC: u32 = 0x44BE_C00C;
pub const CHUNK_MAGIC: u32 = 0xB1FE_3AA2;
/// The uncompressed size of a chunk (its window).
pub const CHUNK_WINDOW: usize = 1024 * 1024;
/// The feature level written into save manifests (the level whose chunks live in
/// `ChunksV4`, as current EGL saves do).
pub const FEATURE_LEVEL: u32 = 18;
/// Manifest header: magic, header size, two sizes, SHA-1, storage flags, version.
const MANIFEST_HEADER: u32 = 41;
/// Chunk header version 3: … plus SHA-1, hash type and uncompressed size.
const CHUNK_HEADER_V3: u32 = 66;
const STORED_COMPRESSED: u8 = 0x1;
const STORED_ENCRYPTED: u8 = 0x2;
/// A file's chunk part record: its own size, GUID, offset, size.
const CHUNK_PART_SIZE: u32 = 28;

/// A chunk's id: four little-endian u32s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Guid(pub [u32; 4]);

impl Guid {
    pub fn random() -> Result<Guid> {
        let mut b = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
        Ok(Guid::from_bytes(&b))
    }

    fn from_bytes(b: &[u8; 16]) -> Guid {
        let w = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Guid([w(0), w(4), w(8), w(12)])
    }

    fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        for (i, w) in self.0.iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        b
    }

    /// As chunk paths spell it: 32 upper-case hex digits.
    pub fn hex(self) -> String {
        format!("{:08X}{:08X}{:08X}{:08X}", self.0[0], self.0[1], self.0[2], self.0[3])
    }

    /// The chunk's folder number, 0–99 (CRC-32 of the GUID's bytes).
    pub fn group(self) -> u8 {
        (crc32fast::hash(&self.to_bytes()) % 100) as u8
    }
}

/// Epic's 64-bit rolling hash of a chunk's data (`FRollingHash`, polynomial
/// `0xC96C5795D7870F42`).
pub fn rolling_hash(data: &[u8]) -> u64 {
    static TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u64; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut v = i as u64;
            for _ in 0..8 {
                v = if v & 1 == 1 { (v >> 1) ^ 0xC96C_5795_D787_0F42 } else { v >> 1 };
            }
            *slot = v;
        }
        t
    });
    data.iter().fold(0u64, |h, &b| h.rotate_left(1) ^ table[b as usize])
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

/// A chunk as a manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    pub guid: Guid,
    pub hash: u64,
    pub sha: [u8; 20],
    pub group: u8,
    /// Uncompressed size.
    pub window: u32,
    /// Size of the chunk file.
    pub file_size: u64,
}

impl ChunkInfo {
    /// The chunk's path in the cloud folder, for a manifest of `feature_level`.
    pub fn path(&self, feature_level: u32) -> String {
        if feature_level >= 22 {
            // ChunksV5: `plain/<group>/<b64url hash>_<b64url guid>` (saves are never encrypted).
            use base64::Engine;
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            return format!("ChunksV5/plain/{:02}/{}_{}.chunk", self.group, b64.encode(self.hash.to_le_bytes()), b64.encode(self.guid.to_bytes()));
        }
        let dir = match feature_level {
            l if l >= 15 => "ChunksV4",
            l if l >= 6 => "ChunksV3",
            l if l >= 3 => "ChunksV2",
            _ => "Chunks",
        };
        format!("{dir}/{:02}/{:016X}_{}.chunk", self.group, self.hash, self.guid.hex())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPart {
    pub guid: Guid,
    pub offset: u32,
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Relative to the save folder, with `/`.
    pub name: String,
    pub sha: [u8; 20],
    pub parts: Vec<ChunkPart>,
}

impl FileEntry {
    pub fn size(&self) -> u64 {
        self.parts.iter().map(|p| p.size as u64).sum()
    }
}

/// A save's manifest: what's in it and where its bytes are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub feature_level: u32,
    pub app_name: String,
    /// For saves: the UTC time it was made, `2026.11.18-10.15.00`.
    pub build_version: String,
    pub chunks: Vec<ChunkInfo>,
    pub files: Vec<FileEntry>,
    pub custom: BTreeMap<String, String>,
}

impl Manifest {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(FileEntry::size).sum()
    }

    pub fn chunk_paths(&self) -> Vec<String> {
        self.chunks.iter().map(|c| c.path(self.feature_level)).collect()
    }
}

// ---- reading

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).context("the manifest ends early")?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    fn sha(&mut self) -> Result<[u8; 20]> {
        Ok(self.take(20)?.try_into()?)
    }

    fn guid(&mut self) -> Result<Guid> {
        Ok(Guid::from_bytes(self.take(16)?.try_into()?))
    }

    /// A count of things at least `min` bytes each, checked against what's left.
    fn count(&mut self, min: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        ensure!(n.saturating_mul(min.max(1)) <= self.b.len() - self.at, "the manifest lists more entries than it holds");
        Ok(n)
    }

    /// An `FString`: length with the terminating NUL; negative means UTF-16.
    fn fstring(&mut self) -> Result<String> {
        let n = self.i32()?;
        if n == 0 {
            return Ok(String::new());
        }
        if n < 0 {
            let units = n.unsigned_abs() as usize;
            let raw = self.take(units.checked_mul(2).context("string too long")?)?;
            let u: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            return Ok(String::from_utf16_lossy(&u).trim_end_matches('\0').to_owned());
        }
        let raw = self.take(n as usize)?;
        Ok(String::from_utf8_lossy(raw).trim_end_matches('\0').to_owned())
    }

    /// Continue after a section that said it is `size` bytes long from `start`.
    fn skip_to(&mut self, start: usize, size: u32) -> Result<()> {
        let end = start.checked_add(size as usize).context("bad section size")?;
        ensure!(end >= self.at && end <= self.b.len(), "a manifest section is shorter than its contents");
        self.at = end;
        Ok(())
    }
}

/// Parse a manifest file as the cloud serves it.
pub fn read_manifest(data: &[u8]) -> Result<Manifest> {
    if data.first() == Some(&b'{') {
        bail!("this save's manifest is in Epic's JSON format, which Quiver doesn't read yet");
    }
    let mut r = Reader::new(data);
    ensure!(r.u32()? == MANIFEST_MAGIC, "not an Epic manifest");
    let header_size = r.u32()?;
    let size_uncompressed = r.u32()? as usize;
    let size_compressed = r.u32()? as usize;
    let sha = r.sha()?;
    let stored_as = r.u8()?;
    let version = if header_size >= MANIFEST_HEADER { r.u32()? } else { 0 };
    ensure!(header_size as usize <= data.len(), "the manifest ends early");
    ensure!(stored_as & STORED_ENCRYPTED == 0, "this save's manifest is encrypted, which saves never are: not reading it");
    let raw = &data[header_size as usize..];
    let body = if stored_as & STORED_COMPRESSED != 0 {
        ensure!(size_uncompressed <= 512 << 20, "the manifest says it is {size_uncompressed} bytes");
        let raw = raw.get(..size_compressed).context("the manifest ends early")?;
        let mut out = Vec::with_capacity(size_uncompressed);
        flate2::read::ZlibDecoder::new(raw).take(size_uncompressed as u64 + 1).read_to_end(&mut out).context("inflate the manifest")?;
        out
    } else {
        raw.get(..size_uncompressed).context("the manifest ends early")?.to_vec()
    };
    ensure!(body.len() == size_uncompressed, "the manifest is {} bytes, not the {size_uncompressed} it says", body.len());
    ensure!(self::sha1(&body) == sha, "the manifest's SHA-1 doesn't match");
    let mut m = parse_body(&body)?;
    if m.feature_level == 0 {
        m.feature_level = version;
    }
    Ok(m)
}

fn parse_body(body: &[u8]) -> Result<Manifest> {
    let mut r = Reader::new(body);
    let mut m = Manifest::default();
    // ManifestMeta
    let start = r.at;
    let size = r.u32()?;
    let data_version = r.u8()?;
    m.feature_level = r.u32()?;
    let _is_file_data = r.u8()?;
    let _app_id = r.u32()?;
    m.app_name = r.fstring()?;
    m.build_version = r.fstring()?;
    let _launch_exe = r.fstring()?;
    let _launch_command = r.fstring()?;
    for _ in 0..r.count(4)? {
        r.fstring()?;
    }
    for _ in 0..3 {
        r.fstring()?; // prereq name, path, args
    }
    let _ = data_version; // build id (v1), uninstall action (v2): not needed
    r.skip_to(start, size)?;
    // ChunkDataList, column by column.
    let start = r.at;
    let size = r.u32()?;
    let _version = r.u8()?;
    let n = r.count(16 + 8 + 20 + 1 + 4 + 8)?;
    let guids = (0..n).map(|_| r.guid()).collect::<Result<Vec<_>>>()?;
    let hashes = (0..n).map(|_| r.u64()).collect::<Result<Vec<_>>>()?;
    let shas = (0..n).map(|_| r.sha()).collect::<Result<Vec<_>>>()?;
    let groups = (0..n).map(|_| r.u8()).collect::<Result<Vec<_>>>()?;
    let windows = (0..n).map(|_| r.u32()).collect::<Result<Vec<_>>>()?;
    let sizes = (0..n).map(|_| r.u64()).collect::<Result<Vec<_>>>()?;
    m.chunks = (0..n)
        .map(|i| ChunkInfo { guid: guids[i], hash: hashes[i], sha: shas[i], group: groups[i], window: windows[i], file_size: sizes[i] })
        .collect();
    r.skip_to(start, size)?;
    // FileManifestList
    let start = r.at;
    let size = r.u32()?;
    let _version = r.u8()?;
    let n = r.count(4 + 4 + 20 + 1 + 4 + 4)?;
    let names = (0..n).map(|_| r.fstring()).collect::<Result<Vec<_>>>()?;
    let links = (0..n).map(|_| r.fstring()).collect::<Result<Vec<_>>>()?;
    let shas = (0..n).map(|_| r.sha()).collect::<Result<Vec<_>>>()?;
    let _flags = (0..n).map(|_| r.u8()).collect::<Result<Vec<_>>>()?;
    for _ in 0..n {
        for _ in 0..r.count(4)? {
            r.fstring()?; // install tags
        }
    }
    let mut parts = Vec::with_capacity(n);
    for _ in 0..n {
        let k = r.count(CHUNK_PART_SIZE as usize)?;
        let mut list = Vec::with_capacity(k);
        for _ in 0..k {
            let p0 = r.at;
            let psize = r.u32()?;
            let guid = r.guid()?;
            let offset = r.u32()?;
            let size = r.u32()?;
            list.push(ChunkPart { guid, offset, size });
            r.skip_to(p0, psize.max(CHUNK_PART_SIZE))?;
        }
        parts.push(list);
    }
    // Later columns (MD5, MIME type, SHA-256) aren't needed.
    r.skip_to(start, size)?;
    for (i, ((name, link), sha)) in names.into_iter().zip(links).zip(shas).enumerate() {
        if !link.is_empty() {
            tracing::info!(file = name, "a cloud save lists a symbolic link; skipped");
            continue;
        }
        m.files.push(FileEntry { name, sha, parts: std::mem::take(&mut parts[i]) });
    }
    // CustomFields (absent in very old manifests).
    if r.at + 9 <= body.len() {
        let start = r.at;
        let size = r.u32()?;
        let _version = r.u8()?;
        let n = r.count(8)?;
        let keys = (0..n).map(|_| r.fstring()).collect::<Result<Vec<_>>>()?;
        let values = (0..n).map(|_| r.fstring()).collect::<Result<Vec<_>>>()?;
        m.custom = keys.into_iter().zip(values).collect();
        r.skip_to(start, size)?;
    }
    Ok(m)
}

/// A chunk file's data (inflated and checked against its own hashes).
pub fn read_chunk(data: &[u8]) -> Result<(Guid, Vec<u8>)> {
    let mut r = Reader::new(data);
    ensure!(r.u32()? == CHUNK_MAGIC, "not an Epic chunk");
    let version = r.u32()?;
    let header_size = r.u32()? as usize;
    let compressed = r.u32()? as usize;
    let guid = r.guid()?;
    let hash = r.u64()?;
    let stored_as = r.u8()?;
    let (mut sha, mut hash_type) = (None, 0u8);
    if version >= 2 {
        sha = Some(r.sha()?);
        hash_type = r.u8()?;
    }
    let uncompressed = if version >= 3 { r.u32()? as usize } else { CHUNK_WINDOW };
    ensure!(stored_as & STORED_ENCRYPTED == 0, "the chunk is encrypted");
    ensure!(uncompressed <= 16 * CHUNK_WINDOW, "the chunk says it is {uncompressed} bytes");
    let raw = data.get(header_size..).and_then(|d| d.get(..compressed)).context("the chunk ends early")?;
    let out = if stored_as & STORED_COMPRESSED != 0 {
        let mut out = Vec::with_capacity(uncompressed);
        flate2::read::ZlibDecoder::new(raw).take(uncompressed as u64 + 1).read_to_end(&mut out).context("inflate a chunk")?;
        out
    } else {
        raw.to_vec()
    };
    if version >= 3 {
        ensure!(out.len() == uncompressed, "the chunk is {} bytes, not the {uncompressed} it says", out.len());
    }
    if let Some(s) = sha.filter(|_| hash_type & 0x2 != 0) {
        ensure!(self::sha1(&out) == s, "a chunk's SHA-1 doesn't match");
    } else if hash_type & 0x1 != 0 || version < 2 {
        ensure!(rolling_hash(&out) == hash, "a chunk's hash doesn't match");
    }
    Ok((guid, out))
}

/// The files of a manifest, from its chunks' data (by GUID). Each file is checked against
/// its SHA-1.
pub fn assemble(m: &Manifest, chunks: &HashMap<Guid, Vec<u8>>) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::with_capacity(m.files.len());
    for f in &m.files {
        let mut data = Vec::with_capacity(f.size() as usize);
        for p in &f.parts {
            let c = chunks.get(&p.guid).with_context(|| format!("{}: a chunk is missing from the cloud", f.name))?;
            let end = (p.offset as usize).checked_add(p.size as usize).filter(|&e| e <= c.len()).with_context(|| format!("{}: a part points outside its chunk", f.name))?;
            data.extend_from_slice(&c[p.offset as usize..end]);
        }
        ensure!(self::sha1(&data) == f.sha, "{}: the downloaded file doesn't match its SHA-1", f.name);
        out.push((f.name.clone(), data));
    }
    Ok(out)
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
    fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn fstring(&mut self, s: &str) {
        if s.is_empty() {
            self.u32(0);
        } else if s.is_ascii() {
            self.u32(s.len() as u32 + 1);
            self.raw(s.as_bytes());
            self.u8(0);
        } else {
            let u: Vec<u16> = s.encode_utf16().chain([0]).collect();
            self.0.extend_from_slice(&(-(u.len() as i32)).to_le_bytes());
            for c in u {
                self.0.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    /// A section: its size first, counted from the size field itself.
    fn section(&mut self, f: impl FnOnce(&mut Writer)) {
        let start = self.0.len();
        self.u32(0);
        f(self);
        let size = (self.0.len() - start) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_le_bytes());
    }
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(data)?;
    Ok(z.finish()?)
}

/// The manifest file for `m` (compressed).
pub fn write_manifest(m: &Manifest) -> Result<Vec<u8>> {
    let mut w = Writer(Vec::new());
    w.section(|w| {
        w.u8(0); // data version
        w.u32(m.feature_level);
        w.u8(0); // not file data
        w.u32(0); // app id
        w.fstring(&m.app_name);
        w.fstring(&m.build_version);
        w.fstring(""); // launch exe
        w.fstring(""); // launch command
        w.u32(0); // prereq ids
        for _ in 0..3 {
            w.fstring("");
        }
    });
    w.section(|w| {
        w.u8(0);
        w.u32(m.chunks.len() as u32);
        m.chunks.iter().for_each(|c| w.raw(&c.guid.to_bytes()));
        m.chunks.iter().for_each(|c| w.u64(c.hash));
        m.chunks.iter().for_each(|c| w.raw(&c.sha));
        m.chunks.iter().for_each(|c| w.u8(c.group));
        m.chunks.iter().for_each(|c| w.u32(c.window));
        m.chunks.iter().for_each(|c| w.u64(c.file_size));
    });
    w.section(|w| {
        w.u8(0);
        w.u32(m.files.len() as u32);
        m.files.iter().for_each(|f| w.fstring(&f.name));
        m.files.iter().for_each(|_| w.fstring(""));
        m.files.iter().for_each(|f| w.raw(&f.sha));
        m.files.iter().for_each(|_| w.u8(0));
        m.files.iter().for_each(|_| w.u32(0)); // install tags
        for f in &m.files {
            w.u32(f.parts.len() as u32);
            for p in &f.parts {
                w.u32(CHUNK_PART_SIZE);
                w.raw(&p.guid.to_bytes());
                w.u32(p.offset);
                w.u32(p.size);
            }
        }
    });
    w.section(|w| {
        w.u8(0);
        w.u32(m.custom.len() as u32);
        m.custom.keys().for_each(|k| w.fstring(k));
        m.custom.values().for_each(|v| w.fstring(v));
    });
    let body = w.0;
    let compressed = deflate(&body)?;
    let mut out = Writer(Vec::with_capacity(compressed.len() + MANIFEST_HEADER as usize));
    out.u32(MANIFEST_MAGIC);
    out.u32(MANIFEST_HEADER);
    out.u32(body.len() as u32);
    out.u32(compressed.len() as u32);
    out.raw(&sha1(&body));
    out.u8(STORED_COMPRESSED);
    out.u32(m.feature_level);
    out.raw(&compressed);
    Ok(out.0)
}

/// A chunk file for one window of data (padded to the window, compressed).
pub fn write_chunk(guid: Guid, window: &[u8]) -> Result<(ChunkInfo, Vec<u8>)> {
    ensure!(window.len() == CHUNK_WINDOW, "a chunk is one whole window");
    let hash = rolling_hash(window);
    let sha = sha1(window);
    let data = deflate(window)?;
    let mut w = Writer(Vec::with_capacity(data.len() + CHUNK_HEADER_V3 as usize));
    w.u32(CHUNK_MAGIC);
    w.u32(3);
    w.u32(CHUNK_HEADER_V3);
    w.u32(data.len() as u32);
    w.raw(&guid.to_bytes());
    w.u64(hash);
    w.u8(STORED_COMPRESSED);
    w.raw(&sha);
    w.u8(0x3); // rolling hash and SHA-1
    w.u32(CHUNK_WINDOW as u32);
    w.raw(&data);
    let info = ChunkInfo { guid, hash, sha, group: guid.group(), window: CHUNK_WINDOW as u32, file_size: w.0.len() as u64 };
    Ok((info, w.0))
}

/// What packaging a save folder produced: the manifest, its file, and the chunk files by
/// their cloud path.
pub struct Package {
    pub manifest: Manifest,
    pub manifest_file: Vec<u8>,
    pub chunks: Vec<(String, Vec<u8>)>,
}

/// Package files (relative name, bytes) into a save manifest named `build_version` (a UTC
/// time, `2026.11.18-10.15.00`), filling 1 MiB windows in order. `guid` makes each chunk's
/// id (random in use, counted in tests).
pub fn package(
    app_name: &str,
    build_version: &str,
    custom: BTreeMap<String, String>,
    files: &[(String, Vec<u8>)],
    mut guid: impl FnMut() -> Result<Guid>,
) -> Result<Package> {
    let mut chunks: Vec<(ChunkInfo, Vec<u8>)> = Vec::new();
    let mut entries = Vec::with_capacity(files.len());
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK_WINDOW);
    let mut current = guid()?;
    let flush = |window: &mut Vec<u8>, current: &mut Guid, chunks: &mut Vec<(ChunkInfo, Vec<u8>)>, guid: &mut dyn FnMut() -> Result<Guid>| -> Result<()> {
        window.resize(CHUNK_WINDOW, 0);
        chunks.push(write_chunk(*current, window)?);
        window.clear();
        *current = guid()?;
        Ok(())
    };
    for (name, data) in files {
        let mut parts = Vec::new();
        let mut rest: &[u8] = data;
        while !rest.is_empty() {
            let room = CHUNK_WINDOW - window.len();
            let n = room.min(rest.len());
            parts.push(ChunkPart { guid: current, offset: window.len() as u32, size: n as u32 });
            window.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if window.len() == CHUNK_WINDOW {
                flush(&mut window, &mut current, &mut chunks, &mut guid)?;
            }
        }
        entries.push(FileEntry { name: name.clone(), sha: sha1(data), parts });
    }
    if !window.is_empty() {
        flush(&mut window, &mut current, &mut chunks, &mut guid)?;
    }
    let manifest = Manifest {
        feature_level: FEATURE_LEVEL,
        app_name: app_name.to_owned(),
        build_version: build_version.to_owned(),
        chunks: chunks.iter().map(|(c, _)| c.clone()).collect(),
        files: entries,
        custom,
    };
    let manifest_file = write_manifest(&manifest)?;
    let chunks = chunks.into_iter().map(|(c, data)| (c.path(FEATURE_LEVEL), data)).collect();
    Ok(Package { manifest, manifest_file, chunks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter() -> impl FnMut() -> Result<Guid> {
        let mut n = 0u32;
        move || {
            n += 1;
            Ok(Guid([n, 0xA, 0xB, 0xC]))
        }
    }

    #[test]
    fn rolling_hash_known_values() {
        assert_eq!(rolling_hash(b""), 0);
        assert_eq!(rolling_hash(&[0; 64]), 0);
        // 0x80 is a single shift of the polynomial's lowest bit through eight rounds.
        let mut v: u64 = 0x80;
        for _ in 0..8 {
            v = if v & 1 == 1 { (v >> 1) ^ 0xC96C_5795_D787_0F42 } else { v >> 1 };
        }
        assert_eq!(v, 0xC96C_5795_D787_0F42);
        assert_eq!(rolling_hash(&[0x80]), 0xC96C_5795_D787_0F42);
        assert_eq!(rolling_hash(&[0x80, 0]), 0xC96C_5795_D787_0F42u64.rotate_left(1));
    }

    #[test]
    fn guid_paths() {
        let c = ChunkInfo { guid: Guid([0x1234, 0xABCDEF01, 0, 7]), hash: 0xDEAD, sha: [0; 20], group: 7, window: 1, file_size: 1 };
        assert_eq!(c.path(18), "ChunksV4/07/000000000000DEAD_00001234ABCDEF010000000000000007.chunk");
        assert!(c.path(22).starts_with("ChunksV5/plain/07/"));
        assert_eq!(c.path(3), "ChunksV2/07/000000000000DEAD_00001234ABCDEF010000000000000007.chunk");
        assert!(Guid([1, 2, 3, 4]).group() < 100);
    }

    #[test]
    fn package_round_trips() {
        let big: Vec<u8> = (0..(CHUNK_WINDOW + 5000)).map(|i| (i * 7 % 251) as u8).collect();
        let files = vec![
            ("Save1.sav".to_owned(), b"hello save".to_vec()),
            ("profiles/Ünicode name.dat".to_owned(), big.clone()),
            ("empty.cfg".to_owned(), Vec::new()),
        ];
        let custom = BTreeMap::from([("CloudSaveFolder".to_owned(), "{AppData}/Game/".to_owned())]);
        let p = package("Wren", "2026.11.18-10.15.00", custom, &files, counter()).unwrap();
        assert_eq!(p.chunks.len(), 2);
        assert!(p.chunks[0].0.starts_with("ChunksV4/"));
        let m = read_manifest(&p.manifest_file).unwrap();
        assert_eq!(m, p.manifest);
        assert_eq!((m.app_name.as_str(), m.build_version.as_str(), m.feature_level), ("Wren", "2026.11.18-10.15.00", 18));
        assert_eq!(m.custom["CloudSaveFolder"], "{AppData}/Game/");
        // The big file spans both chunks; the first starts at offset 0 after the small one.
        assert_eq!(m.files[1].parts.len(), 2);
        assert_eq!(m.files[1].parts[0].offset, 10);
        let chunks: HashMap<Guid, Vec<u8>> = p.chunks.iter().map(|(_, d)| read_chunk(d).unwrap()).collect();
        assert!(chunks.values().all(|c| c.len() == CHUNK_WINDOW));
        let back = assemble(&m, &chunks).unwrap();
        assert_eq!(back, files);
        assert_eq!(m.bytes(), (10 + big.len()) as u64);
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic() {
        let files = vec![("a".to_owned(), b"x".to_vec())];
        let p = package("A", "v", BTreeMap::new(), &files, counter()).unwrap();
        for n in [0, 3, 20, 41, 60, p.manifest_file.len() - 1] {
            assert!(read_manifest(&p.manifest_file[..n]).is_err(), "{n}");
        }
        let mut bad = p.manifest_file.clone();
        *bad.last_mut().unwrap() ^= 0xFF;
        assert!(read_manifest(&bad).is_err());
        let mut enc = p.manifest_file.clone();
        enc[36] |= STORED_ENCRYPTED;
        assert!(read_manifest(&enc).unwrap_err().to_string().contains("encrypted"));
        let chunk = &p.chunks[0].1;
        for n in [0, 10, 66, chunk.len() - 1] {
            assert!(read_chunk(&chunk[..n]).is_err(), "{n}");
        }
        assert!(read_manifest(b"{\"ManifestFileVersion\":\"013000000000\"}").unwrap_err().to_string().contains("JSON"));
        // A part outside its chunk.
        let mut m = read_manifest(&p.manifest_file).unwrap();
        m.files[0].parts[0].offset = CHUNK_WINDOW as u32;
        let chunks: HashMap<Guid, Vec<u8>> = p.chunks.iter().map(|(_, d)| read_chunk(d).unwrap()).collect();
        assert!(assemble(&m, &chunks).unwrap_err().to_string().contains("outside"));
    }
}
