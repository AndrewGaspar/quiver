//! This machine's identity: an Ed25519 key, a self-signed certificate for rustls, a name and
//! a stable id (design/REMOTE.md §3).
//!
//! Files, in the remote dir (`<data>/remote`, 0700):
//! - `machine.key`: the private key (PKCS#8 PEM), 0600. Never printed or logged.
//! - `machine.crt`: the certificate (PEM). It can be made again from the key; peers pin the
//!   key, not the certificate, so a new certificate changes nothing for them.
//! - `machine.json`: the name (`{"name": "tower"}`), the hostname until renamed.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The OID of Ed25519 keys (RFC 8410).
const ED25519_OID: &str = "1.3.101.112";

/// A machine's pin: the SHA-256 of its Ed25519 public key. Peers store it at pairing and
/// every later connection must present a certificate with that key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pin(pub [u8; 32]);

impl Pin {
    /// The pin of the key in a DER certificate. Only Ed25519 keys are accepted.
    pub fn of_cert(der: &[u8]) -> Result<Pin> {
        let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow::anyhow!("not a certificate: {e}"))?;
        let spki = &cert.tbs_certificate.subject_pki;
        if spki.algorithm.algorithm.to_id_string() != ED25519_OID {
            bail!("the certificate's key isn't Ed25519");
        }
        Ok(Pin::of_key(&spki.subject_public_key.data))
    }

    /// The pin of a raw Ed25519 public key (32 bytes).
    pub fn of_key(raw: &[u8]) -> Pin {
        Pin(sha256(raw))
    }

    /// The machine id this pin gives: the first 8 bytes in hex.
    pub fn id(&self) -> String {
        hex::encode(&self.0[..8])
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pin({})", self.to_hex())
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::str::FromStr for Pin {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Pin> {
        let v = hex::decode(s.trim()).context("pin: not hex")?;
        Ok(Pin(v.try_into().map_err(|_| anyhow::anyhow!("pin: expected 32 bytes"))?))
    }
}

impl Serialize for Pin {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Pin {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Pin, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// This machine's key, certificate, name and id. `Debug` leaves the key out.
pub struct Identity {
    dir: PathBuf,
    name: String,
    pin: Pin,
    /// PKCS#8 DER of the private key.
    key_der: Vec<u8>,
    cert_der: Vec<u8>,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("name", &self.name).field("id", &self.id()).field("key", &"<redacted>").finish()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct MachineFile {
    #[serde(default)]
    name: Option<String>,
}

impl Identity {
    /// The identity in `dir`, made there (key, certificate, name) the first time.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        create_private_dir(dir)?;
        let key_path = dir.join("machine.key");
        let key = match std::fs::read_to_string(&key_path) {
            Ok(pem) => rcgen::KeyPair::from_pem(&pem).with_context(|| format!("reading {}", key_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).context("making the machine key")?;
                write_private(&key_path, key.serialize_pem().as_bytes())?;
                tracing::info!("made this machine's remote key ({})", Pin::of_key(key.public_key_raw()).id());
                key
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", key_path.display())),
        };
        if key.algorithm() != &rcgen::PKCS_ED25519 {
            bail!("{} isn't an Ed25519 key", key_path.display());
        }
        let pin = Pin::of_key(key.public_key_raw());
        let cert_path = dir.join("machine.crt");
        let existing = std::fs::read_to_string(&cert_path).ok().and_then(|pem| pem_to_der(&pem)).filter(|der| Pin::of_cert(der).ok() == Some(pin));
        let cert_der = match existing {
            Some(der) => der,
            None => {
                let mut params = rcgen::CertificateParams::new(vec!["quiver".to_owned()]).context("certificate parameters")?;
                params.distinguished_name.push(rcgen::DnType::CommonName, format!("quiver {}", pin.id()));
                let cert = params.self_signed(&key).context("making the machine certificate")?;
                write_atomic(&cert_path, cert.pem().as_bytes(), 0o644)?;
                cert.der().to_vec()
            }
        };
        let file: MachineFile = std::fs::read(dir.join("machine.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let name = file.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(hostname);
        Ok(Identity { dir: dir.to_owned(), name, pin, key_der: key.serialize_der(), cert_der })
    }

    /// The identity in `dir` if one was made there; never makes one.
    pub fn load(dir: &Path) -> Result<Identity> {
        if !dir.join("machine.key").is_file() {
            bail!("this machine has no remote identity yet (pair it with `quiver remote add`)");
        }
        Identity::load_or_create(dir)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The stable id: the first 8 bytes of the key's hash, in hex.
    pub fn id(&self) -> String {
        self.pin.id()
    }

    pub fn pin(&self) -> Pin {
        self.pin
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// SHA-256 of the certificate: what pairing confirms.
    pub fn cert_hash(&self) -> [u8; 32] {
        sha256(&self.cert_der)
    }

    pub(crate) fn key_der(&self) -> &[u8] {
        &self.key_der
    }

    /// Rename this machine (what peers and mDNS see from now on).
    pub fn rename(&mut self, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        let file = MachineFile { name: Some(name.clone()) };
        write_atomic(&self.dir.join("machine.json"), &serde_json::to_vec_pretty(&file)?, 0o644)?;
        self.name = name;
        Ok(())
    }
}

/// A machine name as given, trimmed: 1 to 64 characters, no control characters.
pub fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        bail!("a machine name is 1 to 64 characters, without control characters");
    }
    Ok(name.to_owned())
}

/// The kernel's hostname, else "quiver".
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "quiver".into())
}

fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    base64::engine::general_purpose::STANDARD.decode(body.trim()).ok()
}

/// Make `dir` (and its parents) with mode 0700 for `dir` itself.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Write a secret: a new file, 0600 from the start, renamed into place.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic(path, data, 0o600)
}

/// Write `data` to a temporary file beside `path` with `mode`, then rename it over `path`,
/// so readers in other processes never see half a file.
pub(crate) fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("remote");
        let a = Identity::load_or_create(&dir).unwrap();
        assert_eq!(std::fs::metadata(dir.join("machine.key")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(a.id().len(), 16);
        assert_eq!(Pin::of_cert(a.cert_der()).unwrap(), a.pin());
        let b = Identity::load(&dir).unwrap();
        assert_eq!((b.id(), b.cert_der()), (a.id(), a.cert_der()), "same key, same certificate");
        // A lost certificate is made again; the pin (the key's) doesn't change.
        std::fs::remove_file(dir.join("machine.crt")).unwrap();
        let c = Identity::load(&dir).unwrap();
        assert_eq!(c.pin(), a.pin());
        // Debug never shows the key.
        let dbg = format!("{c:?}");
        assert!(dbg.contains("redacted") && !dbg.contains(&hex::encode(c.key_der())), "{dbg}");
    }

    #[test]
    fn rename_persists() {
        let t = tempfile::tempdir().unwrap();
        let mut a = Identity::load_or_create(t.path()).unwrap();
        assert_eq!(a.name(), hostname());
        a.rename("  Tower ").unwrap();
        assert_eq!(Identity::load(t.path()).unwrap().name(), "Tower");
        assert!(a.rename("").is_err() && a.rename("a\nb").is_err());
    }

    #[test]
    fn load_never_creates() {
        let t = tempfile::tempdir().unwrap();
        assert!(Identity::load(&t.path().join("none")).is_err());
        assert!(!t.path().join("none/machine.key").exists());
    }

    #[test]
    fn pin_round_trips() {
        let p = Pin([7; 32]);
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Pin>(&s).unwrap(), p);
        assert!("abc".parse::<Pin>().is_err());
    }
}
