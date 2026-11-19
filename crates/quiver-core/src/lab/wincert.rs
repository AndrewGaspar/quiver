//! Trusting a lab instance's EOS-proxy CA inside the instance's *cloned* prefix only
//! (design/LAB.md section 3.1), by editing its `system.reg` while no wineserver runs (the
//! launch has just killed everything).
//!
//! What a game sees: Unreal's SSL code (which the EOS SDK carries) loads the Windows ROOT store
//! (`CertOpenSystemStoreW(0, "ROOT")`, `bUsePlatformProvidedCertificates`) into its OpenSSL.
//! Wine's ROOT store is `HKLM\Software\Microsoft\SystemCertificates\Root\Certificates\<SHA-1>`,
//! value `Blob`: serialized properties, `CERT_HASH_PROP_ID` (3) with the SHA-1 then
//! `CERT_CERT_PROP_ID` (0x20) with the DER, each as `{id, 1, size}` little-endian.
//!
//! Wine's crypt32 syncs that key with the host's certificates on its first use in a session
//! (dlls/crypt32/rootstore.c): it deletes the certificates listed in
//! `HKLM\Software\Wine\HostImportedCertificates` that the host no longer has, and when that key
//! is missing it first lists *every* cached certificate there, so ours would be deleted. For a
//! 32-bit process the key is `Software\Wow6432Node\Wine\…` (the Root key itself is shared).
//! Seen in a Proton 11 prefix: a 32-bit client deleted a CA the 64-bit one had kept, until the
//! Wow6432Node key existed. So both import keys must exist without our hash in them.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

/// Every lab CA's subject starts with this (stale ones are recognised by it).
pub const CA_NAME: &str = "Quiver Lab EOS proxy CA";

const ROOT_KEY: &str = r"Software\\Microsoft\\SystemCertificates\\Root\\Certificates\\";
const IMPORT_KEYS: [&str; 2] = [r"Software\\Wine\\HostImportedCertificates", r"Software\\Wow6432Node\\Wine\\HostImportedCertificates"];

/// The certificate's SHA-1, as Wine names its key (uppercase hex).
pub fn thumbprint(der: &[u8]) -> String {
    hex::encode_upper(Sha1::digest(der))
}

/// Wine's `Blob` for a certificate.
pub fn blob(der: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(der.len() + 44);
    for (id, data) in [(3u32, Sha1::digest(der).to_vec()), (0x20, der.to_vec())] {
        b.extend(id.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
    }
    b
}

/// `"Blob"=hex:…` as Wine writes it (continuation lines of about 80 columns).
fn hex_value(name: &str, bytes: &[u8]) -> String {
    let mut out = format!("\"{name}\"=hex:");
    let mut col = out.len();
    for (i, b) in bytes.iter().enumerate() {
        let piece = format!("{b:02x}{}", if i + 1 < bytes.len() { "," } else { "" });
        if col + piece.len() > 79 && i + 1 < bytes.len() {
            out.push_str("\\\n  ");
            col = 2;
        }
        out.push_str(&piece);
        col += piece.len();
    }
    out
}

/// A registry file: the header, then sections (each starting at its `[key]` line).
struct Hive {
    head: String,
    sections: Vec<String>,
}

impl Hive {
    fn parse(text: &str) -> Hive {
        let mut head = String::new();
        let mut sections: Vec<String> = Vec::new();
        for line in text.split_inclusive('\n') {
            if line.starts_with('[') {
                sections.push(String::new());
            }
            match sections.last_mut() {
                Some(s) => s.push_str(line),
                None => head.push_str(line),
            }
        }
        Hive { head, sections }
    }

    fn render(&self) -> String {
        let mut s = self.head.clone();
        for sec in &self.sections {
            s.push_str(sec);
            if !sec.ends_with('\n') {
                s.push('\n');
            }
        }
        s
    }

    fn key_of(sec: &str) -> &str {
        sec.strip_prefix('[').and_then(|s| s.split_once(']')).map(|(k, _)| k).unwrap_or_default()
    }

    fn find(&self, key: &str) -> Option<usize> {
        self.sections.iter().position(|s| Self::key_of(s).eq_ignore_ascii_case(key))
    }
}

/// The bytes of a section's hex value.
fn value_bytes(sec: &str, name: &str) -> Option<Vec<u8>> {
    let start = sec.find(&format!("\"{name}\"=hex:"))? + name.len() + 7;
    let mut hexs = String::new();
    for line in sec[start..].lines() {
        let l = line.trim();
        let (body, more) = match l.strip_suffix('\\') {
            Some(b) => (b, true),
            None => (l, false),
        };
        hexs.push_str(body);
        if !more {
            break;
        }
    }
    hexs.split(',').filter(|h| !h.is_empty()).map(|h| u8::from_str_radix(h.trim(), 16).ok()).collect()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load(prefix: &Path) -> Result<(std::path::PathBuf, Hive)> {
    let path = prefix.join("system.reg");
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {} (the instance's prefix isn't set up?)", path.display()))?;
    if !text.starts_with("WINE REGISTRY Version 2") {
        bail!("{} isn't a Wine registry file", path.display());
    }
    Ok((path, Hive::parse(&text)))
}

fn save(path: &Path, hive: &Hive) -> Result<()> {
    super::write_atomic(path, hive.render().as_bytes())
}

/// Trust `ca_der` in this prefix's ROOT store (and only it among lab CAs). Returns what was
/// changed (empty: already so).
pub fn install(prefix: &Path, ca_der: &[u8]) -> Result<Vec<String>> {
    let (path, mut hive) = load(prefix)?;
    let thumb = thumbprint(ca_der);
    let mut did = Vec::new();
    let marker = CA_NAME.as_bytes();
    // Stale lab CAs (an instance whose CA was made again) go.
    let before = hive.sections.len();
    hive.sections.retain(|s| {
        let k = Hive::key_of(s);
        let stale = k.len() > ROOT_KEY.len()
            && k[..ROOT_KEY.len()].eq_ignore_ascii_case(ROOT_KEY)
            && !k[ROOT_KEY.len()..].eq_ignore_ascii_case(&thumb)
            && value_bytes(s, "Blob").is_some_and(|b| b.windows(marker.len()).any(|w| w == marker));
        !stale
    });
    if hive.sections.len() != before {
        did.push(format!("removed {} older lab CA(s)", before - hive.sections.len()));
    }
    let key = format!("{ROOT_KEY}{thumb}");
    let want = blob(ca_der);
    match hive.find(&key) {
        Some(i) if value_bytes(&hive.sections[i], "Blob").as_deref() == Some(&want[..]) => {}
        found => {
            let sec = format!("[{key}] {}\n{}\n\n", now(), hex_value("Blob", &want));
            match found {
                Some(i) => hive.sections[i] = sec,
                None => hive.sections.push(sec),
            }
            did.push(format!("trusted the lab CA {thumb} in the instance's ROOT store"));
        }
    }
    for k in IMPORT_KEYS {
        match hive.find(k) {
            Some(i) => {
                let sec = &hive.sections[i];
                let kept: String = sec.split_inclusive('\n').filter(|l| !l.to_ascii_uppercase().starts_with(&format!("\"{thumb}\"="))).collect();
                if kept != *sec {
                    hive.sections[i] = kept;
                    did.push(format!("unlisted the lab CA from {}", k.replace(r"\\", r"\")));
                }
            }
            None => {
                hive.sections.push(format!("[{k}] {}\n\n", now()));
                did.push(format!("created {} (else Wine deletes every root it can't find on the host)", k.replace(r"\\", r"\")));
            }
        }
    }
    if !did.is_empty() {
        save(&path, &hive)?;
    }
    Ok(did)
}

/// Is `ca_der` trusted here, safe from Wine's root sync?
pub fn installed(prefix: &Path, ca_der: &[u8]) -> bool {
    let Ok((_, hive)) = load(prefix) else { return false };
    let thumb = thumbprint(ca_der);
    hive.find(&format!("{ROOT_KEY}{thumb}")).is_some_and(|i| value_bytes(&hive.sections[i], "Blob").as_deref() == Some(&blob(ca_der)[..]))
        && IMPORT_KEYS.iter().all(|k| hive.find(k).is_some_and(|i| !hive.sections[i].to_ascii_uppercase().contains(&format!("\"{thumb}\"="))))
}

/// Stop trusting `ca_der` here (tests' control runs).
pub fn remove(prefix: &Path, ca_der: &[u8]) -> Result<bool> {
    let (path, mut hive) = load(prefix)?;
    let Some(i) = hive.find(&format!("{ROOT_KEY}{}", thumbprint(ca_der))) else { return Ok(false) };
    hive.sections.remove(i);
    save(&path, &hive)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = "WINE REGISTRY Version 2\n;; All keys relative to REGISTRY\\\\Machine\n\n#arch=win64\n\n\
[Software\\\\Microsoft\\\\SystemCertificates\\\\Root\\\\Certificates\\\\010C0695A6981914FFBF5FC6B0B695EA29E912A6] 1795055254\n#time=1dd77cd892ab2ac\n\"Blob\"=hex:03,00\n\n\
[Software\\\\Wine\\\\HostImportedCertificates] 1795055254\n\"010C0695A6981914FFBF5FC6B0B695EA29E912A6\"=dword:00000001\n\n\
[Software\\\\Wine\\\\Ports] 1795055250\n\n";

    #[test]
    fn blobs_are_wines_format() {
        let der = b"0\x82\x01\x00 not really a certificate";
        let b = blob(der);
        assert_eq!(&b[..12], [3, 0, 0, 0, 1, 0, 0, 0, 20, 0, 0, 0]);
        assert_eq!(&b[32..44], [0x20, 0, 0, 0, 1, 0, 0, 0, der.len() as u8, 0, 0, 0]);
        assert_eq!(&b[44..], der);
        let v = hex_value("Blob", &[0xab; 100]);
        assert!(v.lines().all(|l| l.len() <= 80), "{v}");
        let sec = format!("[k] 1\n{v}\n\n");
        assert_eq!(value_bytes(&sec, "Blob").unwrap(), vec![0xab; 100]);
    }

    #[test]
    fn install_is_idempotent_and_survives_wines_sync() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("system.reg"), REG).unwrap();
        let ca = format!("0\u{1}{CA_NAME} (awr) and more DER").into_bytes();
        let did = install(t.path(), &ca).unwrap();
        assert_eq!(did.len(), 2, "{did:?}"); // trusted + the Wow6432Node import key created
        assert!(installed(t.path(), &ca));
        let text = std::fs::read_to_string(t.path().join("system.reg")).unwrap();
        assert!(text.contains(&format!("[Software\\\\Microsoft\\\\SystemCertificates\\\\Root\\\\Certificates\\\\{}]", thumbprint(&ca))));
        assert!(text.contains("[Software\\\\Wow6432Node\\\\Wine\\\\HostImportedCertificates]"));
        // The host's certificates and other keys are kept as they were.
        assert!(text.contains("\"010C0695A6981914FFBF5FC6B0B695EA29E912A6\"=dword:00000001") && text.contains("[Software\\\\Wine\\\\Ports]"));
        assert!(install(t.path(), &ca).unwrap().is_empty());
        // Listed as host-imported (Wine would delete it): unlisted again.
        let listed = text.replace("\"010C0695", &format!("\"{}\"=dword:00000001\n\"010C0695", thumbprint(&ca)));
        std::fs::write(t.path().join("system.reg"), listed).unwrap();
        assert!(!installed(t.path(), &ca));
        assert_eq!(install(t.path(), &ca).unwrap().len(), 1);
        assert!(installed(t.path(), &ca));
        // A new CA replaces the old lab one, never the host's.
        let ca2 = format!("0\u{2}{CA_NAME} (awr) again").into_bytes();
        let did = install(t.path(), &ca2).unwrap();
        assert!(did.iter().any(|d| d.contains("removed 1 older")), "{did:?}");
        assert!(installed(t.path(), &ca2) && !installed(t.path(), &ca));
        assert!(std::fs::read_to_string(t.path().join("system.reg")).unwrap().contains("010C0695A6981914FFBF5FC6B0B695EA29E912A6]"));
        assert!(remove(t.path(), &ca2).unwrap() && !installed(t.path(), &ca2));
    }

    #[test]
    fn refuses_what_isnt_a_prefix() {
        let t = tempfile::tempdir().unwrap();
        assert!(install(t.path(), b"x").is_err());
        std::fs::write(t.path().join("system.reg"), "not a registry").unwrap();
        assert!(install(t.path(), b"x").is_err());
    }
}
