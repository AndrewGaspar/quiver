//! `appcache/appinfo.vdf`: the client's cache of PICS app info (name, type, developer,
//! DLC list, launch configs, …) for every app it has looked at.
//!
//! All numbers are little-endian. The header is
//!
//! | field | v27, v28 | v29 |
//! |---|---|---|
//! | magic `u32` | `0x07564427`, `0x07564428` | `0x07564429` |
//! | universe `u32` | yes | yes |
//! | string table offset `i64` | — | yes |
//!
//! then one entry per app until an app id of 0:
//!
//! | field | |
//! |---|---|
//! | app id `u32` | |
//! | size `u32` | bytes after this field, up to the next app id |
//! | info state `u32`, last updated `u32` (unix secs), PICS token `u64` | |
//! | SHA-1 of the text KeyValues `[u8; 20]`, change number `u32` | |
//! | SHA-1 of the binary KeyValues `[u8; 20]` | v28 and v29 only |
//! | binary KeyValues: one `appinfo` map | |
//!
//! In v29 a KeyValues key is a `u32` index into the string table at the end of the file
//! (`u32` count, then that many NUL-terminated strings); in v27 and v28 keys are inline
//! NUL-terminated strings, as in [`crate::exports::vdf`]. Apps nobody asked for are
//! skipped by their size without being decoded.
//!
//! Timing (this machine, Nov 2026; `steamlocal::tests::real_files`): the file was v29,
//! 2.9 MB with 1223 apps. Decoding all of them took 9 ms in a release build (47 ms debug);
//! skipping all of them 0.1 ms. A synthetic 100 MB file (those entries repeated 36 times)
//! is skipped end to end in 5 ms release (9 ms debug), so on a big cache the cost is the
//! file read plus about 30 µs per selected app (release).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::kv::{Kv, Reader};

const V27: u32 = 0x0756_4427;
const V28: u32 = 0x0756_4428;
const V29: u32 = 0x0756_4429;

/// What Quiver uses from one app's entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppInfo {
    pub app_id: u32,
    /// `common/name`.
    pub name: Option<String>,
    /// `common/type`, lowercased: `game`, `tool`, `application`, `dlc`, `demo`, `config`,
    /// `music`, `video`, `beta`, …
    pub kind: Option<String>,
    /// `common/oslist` (`windows,macos,linux`), split on commas.
    pub oslist: Vec<String>,
    /// `extended/developer`, else the `common/associations` entries of type `developer`
    /// (joined with ", ").
    pub developer: Option<String>,
    /// `extended/publisher`, else the `common/associations` entries of type `publisher`.
    pub publisher: Option<String>,
    /// First release, unix seconds: `common/original_release_date` (set when the game came
    /// out before its Steam release), else `common/steam_release_date`.
    pub release: Option<i64>,
    /// `extended/listofdlc`, a comma list of app ids.
    pub dlc: Vec<u32>,
    /// `common/review_percentage`, when Steam has a review score.
    pub review_percentage: Option<u32>,
    /// `ufs/quota`: the app's Steam Cloud allowance in bytes (`None` or 0: no Steam Cloud).
    pub cloud_quota: Option<u64>,
}

impl AppInfo {
    /// Whether Steam Cloud syncs the app's saves (it has a cloud quota).
    pub fn steam_cloud(&self) -> bool {
        self.cloud_quota.is_some_and(|q| q > 0)
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            bail!("appinfo.vdf truncated at byte {}", self.pos);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// v29's string table: `u32` count, then NUL-terminated strings.
fn string_table(buf: &[u8]) -> Result<Vec<&[u8]>> {
    let mut c = Cursor { buf, pos: 0 };
    let n = c.u32()? as usize;
    // Every string takes at least its NUL, so a count beyond the bytes left is garbage
    // (and must not become a huge allocation).
    if n > buf.len() - 4 {
        bail!("appinfo.vdf string table claims {n} strings in {} bytes", buf.len() - 4);
    }
    let mut out = Vec::with_capacity(n);
    let mut rest = &buf[4..];
    for _ in 0..n {
        let Some(len) = rest.iter().position(|&b| b == 0) else { bail!("appinfo.vdf string table truncated") };
        out.push(&rest[..len]);
        rest = &rest[len + 1..];
    }
    Ok(out)
}

/// The apps `want` selects, by app id.
pub fn parse(buf: &[u8], want: impl Fn(u32) -> bool) -> Result<HashMap<u32, AppInfo>> {
    let mut c = Cursor { buf, pos: 0 };
    let magic = c.u32()?;
    let _universe = c.u32()?;
    let (apps_end, table) = match magic {
        V29 => {
            let off = c.i64()?;
            let off = usize::try_from(off).ok().filter(|&o| o >= c.pos && o <= buf.len());
            let Some(off) = off else { bail!("appinfo.vdf string table offset out of range") };
            (off, Some(string_table(&buf[off..])?))
        }
        V27 | V28 => (buf.len(), None),
        m => bail!("unknown appinfo.vdf version (magic 0x{m:08x})"),
    };
    // Per-app header after the size field.
    let header = if magic == V27 { 40 } else { 60 };
    let mut c = Cursor { buf: &buf[..apps_end], pos: c.pos };
    let mut out = HashMap::new();
    loop {
        let app_id = c.u32().context("appinfo.vdf ends without its end marker")?;
        if app_id == 0 {
            break;
        }
        let size = c.u32()? as usize;
        let entry = c.take(size).with_context(|| format!("appinfo.vdf entry for app {app_id}"))?;
        if !want(app_id) {
            continue;
        }
        if entry.len() < header {
            bail!("appinfo.vdf entry for app {app_id} is shorter than its header");
        }
        let kv = Reader::new(&entry[header..], table.as_deref())
            .root()
            .with_context(|| format!("appinfo.vdf entry for app {app_id}"))?;
        out.insert(app_id, extract(app_id, &kv));
    }
    Ok(out)
}

/// Read `<steam_root>/appcache/appinfo.vdf`.
pub fn read(steam_root: &Path, want: impl Fn(u32) -> bool) -> Result<HashMap<u32, AppInfo>> {
    let path = steam_root.join("appcache/appinfo.vdf");
    let buf = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    parse(&buf, want)
}

fn nonempty(s: impl AsRef<str>) -> Option<String> {
    let s = s.as_ref().trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// `common/associations`: `{"0": {"type": "developer", "name": "…"}, …}`.
fn associations(common: Option<&Kv>, kind: &str) -> Option<String> {
    let names: Vec<String> = common?
        .get("associations")?
        .entries()
        .iter()
        .filter(|(_, a)| a.str_at("type").is_some_and(|t| t.eq_ignore_ascii_case(kind)))
        .filter_map(|(_, a)| nonempty(a.str_at("name")?))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

fn extract(app_id: u32, kv: &Kv) -> AppInfo {
    // The entry is one `appinfo` map; tolerate a bare one.
    let root = kv.get("appinfo").unwrap_or(kv);
    let common = root.get("common");
    let extended = root.get("extended");
    let cstr = |k: &str| common.and_then(|c| c.str_at(k)).and_then(nonempty);
    let estr = |k: &str| extended.and_then(|e| e.str_at(k)).and_then(nonempty);
    let date = |k: &str| common.and_then(|c| c.int_at(k)).filter(|&t| t > 0);
    let list = |s: Option<String>| -> Vec<String> {
        s.map(|s| s.split(',').filter_map(nonempty).collect()).unwrap_or_default()
    };
    AppInfo {
        app_id,
        name: cstr("name"),
        kind: cstr("type").map(|t| t.to_ascii_lowercase()),
        oslist: list(cstr("oslist")),
        developer: estr("developer").or_else(|| associations(common, "developer")),
        publisher: estr("publisher").or_else(|| associations(common, "publisher")),
        release: date("original_release_date").or_else(|| date("steam_release_date")),
        dlc: list(estr("listofdlc")).iter().filter_map(|d| d.parse().ok()).collect(),
        review_percentage: common.and_then(|c| c.int_at("review_percentage")).and_then(|p| u32::try_from(p).ok()),
        cloud_quota: root.get("ufs").and_then(|u| u.int_at("quota")).and_then(|q| u64::try_from(q).ok()),
    }
}

/// Every key path under `appinfo` with how many of the selected apps have it, for
/// investigating format changes (keys only, no values).
#[cfg(test)]
pub(crate) fn key_shapes(buf: &[u8], want: impl Fn(u32) -> bool) -> Result<std::collections::BTreeMap<String, usize>> {
    use super::kv::key_str;
    fn walk(prefix: &str, kv: &Kv, out: &mut std::collections::BTreeSet<String>) {
        for (k, v) in kv.entries() {
            let k = key_str(k);
            let k = if k.parse::<u64>().is_ok() { "#".into() } else { k };
            let p = format!("{prefix}/{k}");
            out.insert(p.clone());
            walk(&p, v, out);
        }
    }
    let mut c = Cursor { buf, pos: 0 };
    let magic = c.u32()?;
    c.u32()?;
    if magic != V29 {
        bail!("key_shapes only reads v29");
    }
    let off = c.i64()? as usize;
    let table = string_table(&buf[off..])?;
    let mut c = Cursor { buf: &buf[..off], pos: 16 };
    let mut counts = std::collections::BTreeMap::new();
    loop {
        let id = c.u32()?;
        if id == 0 {
            break;
        }
        let size = c.u32()? as usize;
        let entry = c.take(size)?;
        if !want(id) {
            continue;
        }
        let kv = Reader::new(&entry[60..], Some(&table)).root()?;
        let mut set = std::collections::BTreeSet::new();
        walk("", &kv, &mut set);
        for p in set {
            *counts.entry(p).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::super::kv::build::Kb;
    use super::*;

    fn app_kv(b: &mut Kb, id: u32, name: &str, kind: &str) {
        b.map("appinfo", |b| {
            b.int("appid", id);
            b.map("common", |b| {
                b.str("name", name).str("type", kind).str("oslist", "windows, linux").str("steam_release_date", "1600000000");
                b.int("original_release_date", 1500000000).int("review_percentage", 91);
                b.map("associations", |b| {
                    b.map("0", |b| {
                        b.str("type", "developer").str("name", "Dev A");
                    });
                    b.map("1", |b| {
                        b.str("type", "publisher").str("name", "Pub");
                    });
                    b.map("2", |b| {
                        b.str("type", "developer").str("name", "Dev B");
                    });
                });
            });
            b.map("extended", |b| {
                b.str("listofdlc", "10,20,x,30");
            });
        });
        b.end();
    }

    /// An appinfo.vdf of the given version holding these apps.
    fn file(magic: u32, apps: &[(u32, &str, &str)]) -> Vec<u8> {
        let mut kb = if magic == V29 { Kb::indexed() } else { Kb::default() };
        let mut entries = Vec::new();
        for &(id, name, kind) in apps {
            kb.out.clear();
            app_kv(&mut kb, id, name, kind);
            let mut e = Vec::new();
            e.extend_from_slice(&2u32.to_le_bytes()); // info state
            e.extend_from_slice(&1_700_000_000u32.to_le_bytes());
            e.extend_from_slice(&0u64.to_le_bytes());
            e.extend_from_slice(&[0xaa; 20]);
            e.extend_from_slice(&123u32.to_le_bytes());
            if magic != V27 {
                e.extend_from_slice(&[0xbb; 20]);
            }
            e.extend_from_slice(&kb.out);
            entries.push((id, e));
        }
        let mut out = Vec::new();
        out.extend_from_slice(&magic.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        let table_at = out.len();
        if magic == V29 {
            out.extend_from_slice(&0i64.to_le_bytes());
        }
        for (id, e) in entries {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(e.len() as u32).to_le_bytes());
            out.extend_from_slice(&e);
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        if let Some(t) = kb.table {
            let off = out.len() as i64;
            out[table_at..table_at + 8].copy_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&(t.len() as u32).to_le_bytes());
            for s in t {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
        }
        out
    }

    const APPS: &[(u32, &str, &str)] = &[(292030, "The Witcher 3: Wild Hunt", "Game"), (228980, "Steamworks Common Redistributables", "Config"), (1493710, "Proton Experimental", "Tool")];

    #[test]
    fn v29_with_string_table() {
        let buf = file(V29, APPS);
        let apps = parse(&buf, |_| true).unwrap();
        assert_eq!(apps.len(), 3);
        let w = &apps[&292030];
        assert_eq!(w.name.as_deref(), Some("The Witcher 3: Wild Hunt"));
        assert_eq!(w.kind.as_deref(), Some("game"));
        assert_eq!(w.oslist, ["windows", "linux"]);
        assert_eq!(w.developer.as_deref(), Some("Dev A, Dev B"));
        assert_eq!(w.publisher.as_deref(), Some("Pub"));
        assert_eq!(w.release, Some(1500000000));
        assert_eq!(w.dlc, [10, 20, 30]);
        assert_eq!(w.review_percentage, Some(91));
        assert_eq!(apps[&228980].kind.as_deref(), Some("config"));
    }

    #[test]
    fn v28_and_v27_inline_keys() {
        for magic in [V28, V27] {
            let apps = parse(&file(magic, APPS), |_| true).unwrap();
            assert_eq!(apps[&1493710].kind.as_deref(), Some("tool"), "{magic:x}");
            assert_eq!(apps[&292030].dlc, [10, 20, 30]);
        }
    }

    #[test]
    fn skips_unwanted_without_decoding() {
        let mut buf = file(V28, APPS);
        // Garble the first app's KeyValues (after its 8-byte id/size and 60-byte header):
        // skipping it must not notice.
        buf[8 + 8 + 60] = 0x7f;
        let apps = parse(&buf, |id| id != 292030).unwrap();
        assert_eq!(apps.keys().copied().collect::<std::collections::BTreeSet<_>>(), [228980, 1493710].into());
        assert!(parse(&buf, |_| true).is_err());
    }

    #[test]
    fn extended_developer_wins() {
        let mut kb = Kb::default();
        kb.map("appinfo", |b| {
            b.map("common", |b| {
                b.str("name", "X").int("steam_release_date", 1600000000);
                b.map("associations", |b| {
                    b.map("0", |b| {
                        b.str("type", "developer").str("name", "Assoc");
                    });
                });
            });
            b.map("extended", |b| {
                b.str("developer", "Ext").str("publisher", "");
            });
        });
        kb.end();
        let info = extract(1, &super::super::kv::parse(&kb.out).unwrap());
        assert_eq!(info.developer.as_deref(), Some("Ext"));
        assert_eq!(info.publisher, None);
        assert_eq!(info.release, Some(1600000000));
        assert!(info.dlc.is_empty() && info.oslist.is_empty() && info.kind.is_none());
    }

    #[test]
    fn errors_not_panics() {
        assert!(parse(b"", |_| true).is_err());
        assert!(parse(&0x0756_4426u32.to_le_bytes(), |_| true).is_err());
        let mut unknown = file(V29, APPS);
        unknown[0] = 0x30;
        assert!(format!("{:#}", parse(&unknown, |_| true).unwrap_err()).contains("unknown appinfo.vdf version"));
        let good = file(V29, APPS);
        // Every truncation fails cleanly (the string table offset then points past the end,
        // an entry runs out, or the end marker is missing).
        for n in 0..good.len() {
            assert!(parse(&good[..n], |_| true).is_err(), "truncated to {n}");
        }
        // Bad string table offset.
        let mut bad = good.clone();
        bad[8..16].copy_from_slice(&(i64::MAX).to_le_bytes());
        assert!(parse(&bad, |_| true).is_err());
        let mut bad = good.clone();
        bad[8..16].copy_from_slice(&(-5i64).to_le_bytes());
        assert!(parse(&bad, |_| true).is_err());
        // Flip each byte: never a panic.
        for i in 0..good.len() {
            let mut g = good.clone();
            g[i] ^= 0xff;
            let _ = parse(&g, |_| true);
        }
        let v27 = file(V27, APPS);
        for n in 0..v27.len() {
            assert!(parse(&v27[..n], |_| true).is_err(), "v27 truncated to {n}");
        }
    }
}
