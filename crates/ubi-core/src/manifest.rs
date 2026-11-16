//! Download manifests (`uplay_install.manifest`, `<hash>.manifest`, `.metadata`, `.licenses`).
//!
//! On-disk layout:
//! ```text
//! u32 LE  format version (1)
//! u32 LE  signature length N (344)
//! u32 LE  compressed payload length M
//! N bytes base64 signature
//! M bytes zlib stream of a protobuf (`download.Manifest` etc.)
//! ```

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use ubi_proto::download;

pub struct SignedBlob {
    pub version: u32,
    pub signature: String,
    pub payload: Vec<u8>,
}

pub fn read_signed(data: &[u8]) -> Result<SignedBlob> {
    ensure!(data.len() >= 12, "manifest too short");
    let u32_at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
    let version = u32_at(0);
    let sig_len = u32_at(4) as usize;
    let compressed_len = u32_at(8) as usize;
    let body = 12 + sig_len;
    let Some(compressed) = data.get(body..body + compressed_len) else {
        bail!("manifest truncated: need {} bytes, have {}", body + compressed_len, data.len());
    };
    let signature = String::from_utf8_lossy(&data[12..body]).into_owned();
    let mut payload = Vec::new();
    flate2::read::ZlibDecoder::new(compressed)
        .read_to_end(&mut payload)
        .context("inflate manifest")?;
    Ok(SignedBlob { version, signature, payload })
}

pub fn parse_manifest(data: &[u8]) -> Result<download::Manifest> {
    let blob = read_signed(data)?;
    download::Manifest::decode(blob.payload.as_slice()).context("decode download.Manifest")
}

pub fn load_manifest(path: &Path) -> Result<download::Manifest> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    parse_manifest(&data)
}

#[derive(Debug, Default, serde::Serialize)]
pub struct ManifestSummary {
    pub game_version: Option<String>,
    pub chunks: usize,
    pub files: usize,
    pub dirs: usize,
    pub slices: usize,
    pub install_bytes: u64,
    pub download_bytes: u64,
    pub languages: Vec<String>,
    pub compressed: bool,
}

pub fn summarize(m: &download::Manifest) -> ManifestSummary {
    let mut s = ManifestSummary {
        game_version: m.game_version.clone(),
        chunks: m.chunks.len(),
        languages: m.languages.iter().filter_map(|l| l.code.clone()).collect(),
        compressed: m.is_compressed.unwrap_or(false),
        ..Default::default()
    };
    for f in m.chunks.iter().flat_map(|c| &c.files) {
        if f.is_dir {
            s.dirs += 1;
            continue;
        }
        s.files += 1;
        s.install_bytes += f.size;
        s.slices += f.slice_list.len().max(f.slices.len());
        s.download_bytes += f
            .slice_list
            .iter()
            .map(|sl| u64::from(sl.download_size.unwrap_or(sl.size)))
            .sum::<u64>();
    }
    s
}
