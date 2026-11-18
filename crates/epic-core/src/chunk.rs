//! Epic's chunk files (`*.chunk` on the CDN): a header (magic `0xB1FE3AA2`, versions 1 to
//! 4), then the data, zlib-compressed (`stored_as & 1`) and, from header version 4,
//! possibly AES-GCM encrypted (`stored_as & 2`; nonce: the first 12 bytes of the data's
//! SHA-1, tag in the header, key from the manifest API's secrets). A chunk's data is a
//! window (usually 1 MiB) that file parts are cut from. [`decode`] checks the data's
//! SHA-1 against the header and the manifest before anything is written.

use anyhow::{Context, Result, bail, ensure};
use sha1::{Digest, Sha1};

use crate::manifest::{ChunkInfo, Guid, Secrets, gcm_decrypt, gcm_encrypt, group_of, guid_hex};

pub const CHUNK_MAGIC: u32 = 0xB1FE_3AA2;
/// Windows larger than this are refused (Epic's are 1 MiB).
const MAX_WINDOW: usize = 64 << 20;

/// A chunk's header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub size: u32,
    pub compressed_size: u32,
    pub guid: Guid,
    pub hash: u64,
    pub stored_as: u8,
    pub sha1: Option<[u8; 20]>,
    pub hash_type: u8,
    pub uncompressed_size: Option<u32>,
    pub secret_guid: Option<Guid>,
    pub encryption_tag: Option<[u8; 16]>,
}

fn rd<const N: usize>(b: &[u8], at: &mut usize) -> Result<[u8; N]> {
    ensure!(*at + N <= b.len(), "chunk cut off in its header");
    let v = b[*at..*at + N].try_into().expect("len");
    *at += N;
    Ok(v)
}

fn guid_of(b: [u8; 16]) -> Guid {
    std::array::from_fn(|i| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().expect("len")))
}

/// Read a chunk's header.
pub fn header(raw: &[u8]) -> Result<Header> {
    let mut at = 0;
    ensure!(u32::from_le_bytes(rd(raw, &mut at)?) == CHUNK_MAGIC, "not an Epic chunk (no header magic)");
    let mut h = Header {
        version: u32::from_le_bytes(rd(raw, &mut at)?),
        size: u32::from_le_bytes(rd(raw, &mut at)?),
        compressed_size: u32::from_le_bytes(rd(raw, &mut at)?),
        guid: guid_of(rd(raw, &mut at)?),
        hash: u64::from_le_bytes(rd(raw, &mut at)?),
        stored_as: rd::<1>(raw, &mut at)?[0],
        ..Default::default()
    };
    if h.version >= 2 {
        h.sha1 = Some(rd(raw, &mut at)?);
        h.hash_type = rd::<1>(raw, &mut at)?[0];
    }
    if h.version >= 3 {
        h.uncompressed_size = Some(u32::from_le_bytes(rd(raw, &mut at)?));
    }
    if h.version >= 4 {
        let g = guid_of(rd(raw, &mut at)?);
        h.secret_guid = (g != [0; 4]).then_some(g);
        h.encryption_tag = Some(rd(raw, &mut at)?);
    }
    ensure!(h.size as usize >= at && h.size as usize <= raw.len(), "chunk header size {} doesn't fit", h.size);
    Ok(h)
}

/// The data of a downloaded chunk: decrypted, decompressed and checked against its SHA-1
/// (the header's and, given `info`, the manifest's) and guid.
pub fn decode(raw: &[u8], info: Option<&ChunkInfo>, secrets: &Secrets) -> Result<Vec<u8>> {
    let h = header(raw)?;
    if let Some(i) = info {
        ensure!(h.guid == i.guid, "chunk {} arrived as chunk {}", guid_hex(&i.guid), guid_hex(&h.guid));
    }
    let start = h.size as usize;
    let len = h.compressed_size as usize;
    ensure!(start.checked_add(len).is_some_and(|e| e <= raw.len()), "chunk {} is cut off ({} of {len} bytes)", guid_hex(&h.guid), raw.len() - start);
    let mut data = raw[start..start + len].to_vec();
    let expected_sha = h.sha1.filter(|s| *s != [0; 20]).or(info.map(|i| i.sha1).filter(|s| *s != [0; 20]));
    if h.stored_as & 0x2 != 0 {
        let secret = h.secret_guid.or(info.and_then(|i| i.secret_guid)).with_context(|| format!("chunk {} is encrypted but names no key", guid_hex(&h.guid)))?;
        let hex_key = secrets.get(&guid_hex(&secret)).with_context(|| format!("chunk {} is encrypted and no key for secret {} came with the manifest", guid_hex(&h.guid), guid_hex(&secret)))?;
        let key = hex::decode(hex_key.trim()).context("chunk key isn't hex")?;
        let tag = h.encryption_tag.filter(|t| *t != [0; 16]).or(info.and_then(|i| i.encryption_tag)).context("encrypted chunk without a tag")?;
        let sha = expected_sha.context("encrypted chunk without a SHA-1 (the nonce)")?;
        data = gcm_decrypt(&key, &sha[..12], &data, &tag).with_context(|| format!("chunk {}", guid_hex(&h.guid)))?;
    }
    if h.stored_as & 0x1 != 0 {
        let limit = h.uncompressed_size.map(|u| u as usize).or(info.map(|i| i.window_size as usize)).unwrap_or(1 << 20).min(MAX_WINDOW);
        let mut out = Vec::with_capacity(limit);
        let mut z = std::io::Read::take(flate2::read::ZlibDecoder::new(data.as_slice()), limit as u64 + 1);
        std::io::Read::read_to_end(&mut z, &mut out).with_context(|| format!("decompressing chunk {}", guid_hex(&h.guid)))?;
        ensure!(out.len() <= limit, "chunk {} is larger than its window", guid_hex(&h.guid));
        data = out;
    }
    let sha: [u8; 20] = Sha1::digest(&data).into();
    for (what, want) in [("header", h.sha1.filter(|s| *s != [0; 20])), ("manifest", info.map(|i| i.sha1).filter(|s| *s != [0; 20]))] {
        if let Some(want) = want
            && want != sha
        {
            bail!("chunk {} doesn't match its {what} SHA-1 (damaged download)", guid_hex(&h.guid));
        }
    }
    Ok(data)
}

/// A chunk file for `data` (header version 3, or 4 when encrypted). Fixtures and tests.
pub fn encode(guid: Guid, data: &[u8], compress: bool, encrypt: Option<(Guid, &[u8; 32])>) -> Vec<u8> {
    let sha: [u8; 20] = Sha1::digest(data).into();
    let mut stored_as = 0u8;
    let mut body = data.to_vec();
    if compress {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, data).expect("zlib");
        body = z.finish().expect("zlib");
        stored_as |= 1;
    }
    let mut tag = [0u8; 16];
    if let Some((_, key)) = encrypt {
        let (c, t) = gcm_encrypt(key, &sha[..12], &body);
        body = c;
        tag = t;
        stored_as |= 2;
    }
    let version = if encrypt.is_some() { 4u32 } else { 3 };
    let header_size: u32 = if version >= 4 { 98 } else { 66 };
    let mut out = Vec::with_capacity(header_size as usize + body.len());
    out.extend_from_slice(&CHUNK_MAGIC.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&header_size.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    guid.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
    out.extend_from_slice(&0u64.to_le_bytes());
    out.push(stored_as);
    out.extend_from_slice(&sha);
    out.push(2); // SHA-1 only
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    if let Some((secret, _)) = encrypt {
        secret.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
        out.extend_from_slice(&tag);
    }
    out.extend_from_slice(&body);
    out
}

/// The manifest's entry for a chunk made by [`encode`] from `data`. Fixtures and tests.
pub fn chunk_info(guid: Guid, data: &[u8], encoded: &[u8]) -> ChunkInfo {
    let h = header(encoded).expect("an encoded chunk");
    ChunkInfo {
        guid,
        hash: h.hash,
        sha1: Sha1::digest(data).into(),
        group: group_of(&guid),
        window_size: data.len() as u32,
        file_size: encoded.len() as u64,
        secret_guid: h.secret_guid,
        window_size_compressed: h.compressed_size,
        encryption_tag: h.encryption_tag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_compressed_and_encrypted_chunks() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let guid = [1, 2, 3, 4];
        for compress in [false, true] {
            let raw = encode(guid, &data, compress, None);
            let info = chunk_info(guid, &data, &raw);
            assert_eq!(decode(&raw, Some(&info), &Secrets::new()).unwrap(), data);
            assert_eq!(decode(&raw, None, &Secrets::new()).unwrap(), data);
        }
        let secret = [9, 9, 9, 9];
        let key = [0x42u8; 32];
        let raw = encode(guid, &data, true, Some((secret, &key)));
        let info = chunk_info(guid, &data, &raw);
        assert_eq!(info.secret_guid, Some(secret));
        let e = decode(&raw, Some(&info), &Secrets::new()).unwrap_err();
        assert!(e.to_string().contains(&guid_hex(&secret)), "{e}");
        let secrets = Secrets::from([(guid_hex(&secret), hex::encode(key))]);
        assert_eq!(decode(&raw, Some(&info), &secrets).unwrap(), data);
        let mut tampered = raw.clone();
        let n = tampered.len();
        tampered[n - 1] ^= 1;
        assert!(decode(&tampered, Some(&info), &secrets).is_err());
    }

    #[test]
    fn wrong_data_guid_or_truncation_is_refused() {
        let data = vec![5u8; 1000];
        let raw = encode([1, 1, 1, 1], &data, false, None);
        let mut info = chunk_info([1, 1, 1, 1], &data, &raw);
        let mut bad = raw.clone();
        let n = bad.len();
        bad[n - 1] = 6;
        assert!(decode(&bad, Some(&info), &Secrets::new()).unwrap_err().to_string().contains("SHA-1"));
        info.guid = [2, 2, 2, 2];
        assert!(decode(&raw, Some(&info), &Secrets::new()).unwrap_err().to_string().contains("arrived as"));
        for cut in 0..raw.len() {
            assert!(decode(&raw[..cut], None, &Secrets::new()).is_err());
        }
        // A manifest SHA-1 that disagrees.
        let mut info = chunk_info([1, 1, 1, 1], &data, &raw);
        info.sha1 = [1; 20];
        assert!(decode(&raw, Some(&info), &Secrets::new()).is_err());
    }
}
