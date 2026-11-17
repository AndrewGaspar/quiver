//! Just enough of a zip reader for Ubisoft's asset archives (achievement sets): the
//! central directory, stored and deflated entries.

use std::io::Read;

use anyhow::{Context, Result, bail, ensure};

/// Largest entry we inflate (asset archives hold small text files and icons).
const MAX_ENTRY: u64 = 32 << 20;

fn u16_at(b: &[u8], o: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(b.get(o..o + 2).context("zip truncated")?.try_into()?))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(o..o + 4).context("zip truncated")?.try_into()?))
}

/// Every file in the archive: (name, contents). Directories are skipped.
pub fn entries(zip: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    // End of central directory: 22 bytes plus a comment of up to 64 KiB.
    let floor = zip.len().saturating_sub(22 + 0xffff);
    let eocd = (floor..zip.len().saturating_sub(21))
        .rev()
        .find(|&i| zip[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .context("not a zip archive")?;
    let count = u16_at(zip, eocd + 10)? as usize;
    let mut p = u32_at(zip, eocd + 16)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        ensure!(u32_at(zip, p)? == 0x0201_4b50, "bad central directory entry");
        let method = u16_at(zip, p + 10)?;
        let csize = u32_at(zip, p + 20)? as usize;
        let usize_ = u32_at(zip, p + 24)? as u64;
        let (nlen, elen, clen) = (u16_at(zip, p + 28)? as usize, u16_at(zip, p + 30)? as usize, u16_at(zip, p + 32)? as usize);
        let local = u32_at(zip, p + 42)? as usize;
        let name = String::from_utf8_lossy(zip.get(p + 46..p + 46 + nlen).context("zip truncated")?).into_owned();
        p += 46 + nlen + elen + clen;
        if name.ends_with('/') {
            continue;
        }
        ensure!(usize_ <= MAX_ENTRY, "zip entry {name} is too large");
        ensure!(u32_at(zip, local)? == 0x0403_4b50, "bad local header for {name}");
        let start = local + 30 + u16_at(zip, local + 26)? as usize + u16_at(zip, local + 28)? as usize;
        let data = zip.get(start..start + csize).context("zip truncated")?;
        let bytes = match method {
            0 => data.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(usize_ as usize);
                flate2::read::DeflateDecoder::new(data).take(MAX_ENTRY).read_to_end(&mut v)?;
                v
            }
            m => bail!("zip entry {name}: unsupported compression {m}"),
        };
        out.push((name, bytes));
    }
    Ok(out)
}
