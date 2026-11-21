//! The server's GameStream identity: an RSA-2048 key, a self-signed certificate, and a
//! stable unique id.
//!
//! It is separate from Quiver's own machine key (Ed25519, design/REMOTE.md §3): Moonlight
//! clients verify the server's pairing signature as SHA256withRSA, so the key must be RSA,
//! and pinning it in Moonlight shouldn't pin Quiver's identity.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// The key, certificate and id a GameStream server presents.
pub struct Identity {
    cert_pem: String,
    cert_der: Vec<u8>,
    key_pkcs8_der: Vec<u8>,
    signer: RsaKeyPair,
    cert_signature: Vec<u8>,
    unique_id: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("unique_id", &self.unique_id).finish_non_exhaustive()
    }
}

const KEY_FILE: &str = "gamestream-key.pem";
const CERT_FILE: &str = "gamestream-cert.pem";
const ID_FILE: &str = "gamestream-id";

impl Identity {
    /// Loads the identity from `dir`, or makes one and saves it there (the key file 0600).
    /// Making one takes a few seconds in a debug build (RSA key generation).
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let key = dir.join(KEY_FILE);
        let cert = dir.join(CERT_FILE);
        let id = dir.join(ID_FILE);
        if key.exists() && cert.exists() && id.exists() {
            let key_pem = std::fs::read_to_string(&key).with_context(|| format!("read {}", key.display()))?;
            let cert_pem = std::fs::read_to_string(&cert).with_context(|| format!("read {}", cert.display()))?;
            let unique_id = std::fs::read_to_string(&id)?.trim().to_owned();
            return Self::from_pem(&cert_pem, &key_pem, unique_id);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let me = Self::generate()?;
        write_private(&key, me.key_pem().as_bytes())?;
        std::fs::write(&cert, me.cert_pem.as_bytes())?;
        std::fs::write(&id, format!("{}\n", me.unique_id))?;
        Ok(me)
    }

    /// A fresh identity, not saved anywhere.
    pub fn generate() -> Result<Self> {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).context("generate the RSA key")?;
        let pkcs8 = key.to_pkcs8_der().context("encode the RSA key")?;
        let key_der = pkcs8.as_bytes().to_vec();
        let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(key_der.as_slice()),
            &rcgen::PKCS_RSA_SHA256,
        )
        .context("load the RSA key for signing")?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, "Quiver GameStream");
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2054, 1, 1);
        let cert = params.self_signed(&key_pair).context("sign the certificate")?;
        let mut id = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut id).map_err(|_| anyhow!("no randomness"))?;
        Self::from_parts(cert.pem(), cert.der().to_vec(), key_der, format_uuid(&id))
    }

    /// An identity from a PEM certificate and a PEM PKCS#8 RSA key.
    pub fn from_pem(cert_pem: &str, key_pem: &str, unique_id: String) -> Result<Self> {
        let cert_der = pem_body(cert_pem, "CERTIFICATE")?;
        let key_der = pem_body(key_pem, "PRIVATE KEY")?;
        Self::from_parts(cert_pem.to_owned(), cert_der, key_der, unique_id)
    }

    fn from_parts(cert_pem: String, cert_der: Vec<u8>, key_pkcs8_der: Vec<u8>, unique_id: String) -> Result<Self> {
        let signer = RsaKeyPair::from_pkcs8(&key_pkcs8_der).map_err(|e| anyhow!("the GameStream key isn't a usable RSA key: {e}"))?;
        let cert_signature = cert_signature(&cert_der)?;
        Ok(Self { cert_pem, cert_der, key_pkcs8_der, signer, cert_signature, unique_id })
    }

    /// The certificate as PEM text (what `/pair` sends as `plaincert`, hex-encoded).
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// The signature bits of our own certificate, which go into the pairing hashes.
    pub fn cert_signature(&self) -> &[u8] {
        &self.cert_signature
    }

    /// The id Moonlight files this server under (`uniqueid` in `/serverinfo`).
    pub fn unique_id(&self) -> &str {
        &self.unique_id
    }

    /// SHA-256 of the certificate, hex: what a client pins.
    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(&self.cert_der))
    }

    /// An RSASSA-PKCS1-v1_5 SHA-256 signature of `data`.
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let mut sig = vec![0u8; self.signer.public().modulus_len()];
        self.signer
            .sign(&RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), data, &mut sig)
            .expect("RSA signing with a valid key");
        sig
    }

    /// The certificate chain and key for the HTTPS listener.
    pub fn tls_parts(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (
            vec![CertificateDer::from(self.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone())),
        )
    }

    fn key_pem(&self) -> String {
        pem_wrap("PRIVATE KEY", &self.key_pkcs8_der)
    }
}

/// The raw signature bits of an X.509 certificate (what OpenSSL's `X509_get0_signature`
/// returns, and what both sides hash during pairing).
pub fn cert_signature(der: &[u8]) -> Result<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow!("not an X.509 certificate: {e}"))?;
    Ok(cert.signature_value.data.to_vec())
}

/// The DER body of the first PEM block labelled `label`.
pub fn pem_body(pem: &str, label: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin).ok_or_else(|| anyhow!("no {label} PEM block"))? + begin.len();
    let stop = pem[start..].find(&end).ok_or_else(|| anyhow!("unterminated {label} PEM block"))? + start;
    let b64: String = pem[start..stop].chars().filter(|c| !c.is_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).context("PEM base64")?;
    if der.is_empty() {
        bail!("empty {label} PEM block");
    }
    Ok(der)
}

fn pem_wrap(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn format_uuid(b: &[u8; 16]) -> String {
    let h = hex::encode_upper(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Writes a file only the owner can read.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const SERVER_CERT: &str = include_str!("../tests/fixtures/server-cert.pem");
    pub const SERVER_KEY: &str = include_str!("../tests/fixtures/server-key.pem");
    pub const CLIENT_CERT: &str = include_str!("../tests/fixtures/client-cert.pem");
    pub const CLIENT_KEY: &str = include_str!("../tests/fixtures/client-key.pem");

    pub fn server() -> Identity {
        Identity::from_pem(SERVER_CERT, SERVER_KEY, "TEST-SERVER".into()).unwrap()
    }

    #[test]
    fn signature_matches_openssl() {
        // `openssl dgst -sha256 -sign server-key.pem` over bytes 00..0f (PKCS#1 v1.5 is
        // deterministic, so ours must be byte-identical).
        let expected = include_str!("../tests/fixtures/server-sign-00-0f.hex").trim();
        let data: Vec<u8> = (0u8..16).collect();
        assert_eq!(hex::encode(server().sign(&data)), expected);
    }

    #[test]
    fn cert_signature_and_fingerprint() {
        let id = server();
        // From `openssl x509 -noout -text`: the signature value starts 0a:46:16:08...
        assert!(hex::encode(id.cert_signature()).starts_with("0a461608596157d902e6b61dfb9dd682"));
        assert_eq!(id.cert_signature().len(), 256);
        // `openssl x509 -outform der | sha256sum`.
        assert_eq!(id.fingerprint(), "3aeace0982526757d0c395159a138ee2f3c9c8d7675e369a742984c1bb6d772f");
    }

    #[test]
    fn pem_roundtrip() {
        let der = pem_body(SERVER_KEY, "PRIVATE KEY").unwrap();
        assert_eq!(pem_body(&pem_wrap("PRIVATE KEY", &der), "PRIVATE KEY").unwrap(), der);
        assert!(pem_body(SERVER_CERT, "PRIVATE KEY").is_err());
    }

    #[test]
    fn uuid_format() {
        assert_eq!(format_uuid(&[0xAB; 16]), "ABABABAB-ABAB-ABAB-ABAB-ABABABABABAB");
    }
}
