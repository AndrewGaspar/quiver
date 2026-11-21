//! GameStream's PIN pairing, the server's half, without any I/O.
//!
//! Moonlight shows a 4-digit PIN; the server's owner types it on the server side. The
//! client's requests, in order (all on `/pair`):
//!
//! 1. `phrase=getservercert&salt=S&clientcert=C`: the server derives `K = SHA-256(S ‖ PIN)[..16]`
//!    and answers with its certificate (`plaincert`). This request waits for the PIN.
//! 2. `clientchallenge=E_K(c)`: the server answers `E_K(SHA-256(c ‖ sig(server cert) ‖ s) ‖ sc)`,
//!    where `s` is a fresh server secret and `sc` a fresh server challenge.
//! 3. `serverchallengeresp=E_K(h)`: the client's `h = SHA-256(sc ‖ sig(client cert) ‖ cs)`;
//!    the server keeps `h` and answers `pairingsecret = s ‖ RSA-sign(s)`. The client checks
//!    the signature and the stage-2 hash, and that is where a wrong PIN shows.
//! 4. `clientpairingsecret=cs ‖ RSA-sign(cs)`: the server checks the client's signature with
//!    the client's certificate and that `SHA-256(sc ‖ sig(client cert) ‖ cs) = h`. If both
//!    hold, the client certificate is pinned.
//! 5. `phrase=pairchallenge` over HTTPS with the new client certificate.
//!
//! `E_K` is AES-128-ECB. `sig(cert)` is the certificate's signature bits.

use anyhow::{Context, Result, anyhow, bail};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use sha2::{Digest, Sha256};

use crate::crypto::{Key, ecb_decrypt, ecb_encrypt};
use crate::identity::{Identity, cert_signature, pem_body};

/// The AES key both sides derive from the salt and the PIN.
pub fn derive_key(salt: &[u8; 16], pin: &str) -> Key {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pin.as_bytes());
    let digest = h.finalize();
    digest[..16].try_into().expect("16 of 32 bytes")
}

/// A Moonlight client's certificate, as sent in `clientcert` and later presented on HTTPS.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCert {
    pub pem: String,
    pub der: Vec<u8>,
    /// The certificate's signature bits.
    pub signature: Vec<u8>,
    /// The subject public key (for RSA, the PKCS#1 `RSAPublicKey`).
    pub public_key: Vec<u8>,
}

impl std::fmt::Debug for ClientCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClientCert({})", &self.fingerprint()[..16])
    }
}

impl ClientCert {
    pub fn from_pem(pem: &str) -> Result<Self> {
        let der = pem_body(pem, "CERTIFICATE")?;
        let mut me = Self::from_der(&der)?;
        me.pem = pem.to_owned();
        Ok(me)
    }

    pub fn from_der(der: &[u8]) -> Result<Self> {
        let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow!("client certificate: {e}"))?;
        let public_key = cert.public_key().subject_public_key.data.to_vec();
        Ok(Self { pem: String::new(), der: der.to_vec(), signature: cert_signature(der)?, public_key })
    }

    /// SHA-256 of the DER, hex: the pin stored for a paired client.
    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(&self.der))
    }

    /// Checks an RSASSA-PKCS1-v1_5 SHA-256 signature made with this certificate's key.
    pub fn verify(&self, data: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &self.public_key).verify(data, signature).is_ok()
    }
}

/// How far a pairing has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Stage 1 answered: waiting for the client's challenge.
    CertSent,
    /// Stage 2 answered: waiting for the client's hash.
    ChallengeAnswered,
    /// Stage 3 answered: waiting for the client's secret.
    SecretSent,
    /// Stage 4 passed: the client can be pinned.
    Paired,
}

/// One pairing attempt, from the PIN to the pinned certificate.
pub struct Pairing {
    key: Key,
    pub client: ClientCert,
    server_secret: [u8; 16],
    server_challenge: [u8; 16],
    client_hash: Option<[u8; 32]>,
    stage: Stage,
}

impl Pairing {
    /// Starts once the PIN is known. `server_secret` and `server_challenge` must be fresh
    /// random bytes.
    pub fn new(salt: &[u8; 16], pin: &str, client: ClientCert, server_secret: [u8; 16], server_challenge: [u8; 16]) -> Self {
        Self { key: derive_key(salt, pin), client, server_secret, server_challenge, client_hash: None, stage: Stage::CertSent }
    }

    /// With random secrets from the OS.
    pub fn with_random(salt: &[u8; 16], pin: &str, client: ClientCert) -> Result<Self> {
        let rng = ring::rand::SystemRandom::new();
        let mut s = [0u8; 16];
        let mut c = [0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut s).map_err(|_| anyhow!("no randomness"))?;
        ring::rand::SecureRandom::fill(&rng, &mut c).map_err(|_| anyhow!("no randomness"))?;
        Ok(Self::new(salt, pin, client, s, c))
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Stage 2: the client's encrypted challenge in, `challengeresponse` out.
    pub fn answer_challenge(&mut self, identity: &Identity, encrypted: &[u8]) -> Result<Vec<u8>> {
        if self.stage != Stage::CertSent {
            bail!("client challenge out of order ({:?})", self.stage);
        }
        if encrypted.len() != 16 {
            bail!("client challenge is {} bytes, not 16", encrypted.len());
        }
        let challenge = ecb_decrypt(&self.key, encrypted)?;
        let mut h = Sha256::new();
        h.update(&challenge);
        h.update(identity.cert_signature());
        h.update(self.server_secret);
        let mut plain = h.finalize().to_vec();
        plain.extend_from_slice(&self.server_challenge);
        self.stage = Stage::ChallengeAnswered;
        ecb_encrypt(&self.key, &plain)
    }

    /// Stage 3: the client's encrypted hash in, `pairingsecret` out.
    pub fn answer_server_challenge_response(&mut self, identity: &Identity, encrypted: &[u8]) -> Result<Vec<u8>> {
        if self.stage != Stage::ChallengeAnswered {
            bail!("server challenge response out of order ({:?})", self.stage);
        }
        if encrypted.len() != 32 {
            bail!("server challenge response is {} bytes, not 32", encrypted.len());
        }
        let plain = ecb_decrypt(&self.key, encrypted)?;
        self.client_hash = Some(plain[..32].try_into().expect("32 bytes"));
        let mut out = self.server_secret.to_vec();
        out.extend_from_slice(&identity.sign(&self.server_secret));
        self.stage = Stage::SecretSent;
        Ok(out)
    }

    /// Stage 4: the client's secret and signature. `Ok` means the PIN matched and the
    /// client holds the key of the certificate it sent.
    pub fn check_client_secret(&mut self, secret_and_signature: &[u8]) -> Result<()> {
        if self.stage != Stage::SecretSent {
            bail!("client pairing secret out of order ({:?})", self.stage);
        }
        if secret_and_signature.len() <= 16 {
            bail!("client pairing secret is too short");
        }
        let (secret, signature) = secret_and_signature.split_at(16);
        if !self.client.verify(secret, signature) {
            bail!("the client's pairing secret isn't signed by its certificate's key");
        }
        let mut h = Sha256::new();
        h.update(self.server_challenge);
        h.update(&self.client.signature);
        h.update(secret);
        let expected: [u8; 32] = h.finalize().into();
        let got = self.client_hash.context("no client hash")?;
        if !constant_time_eq(&expected, &got) {
            bail!("the PIN didn't match");
        }
        self.stage = Stage::Paired;
        Ok(())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::tests::{CLIENT_CERT, CLIENT_KEY, server};
    use ring::signature::RsaKeyPair;

    #[test]
    fn key_known_vector() {
        // sha256(00 01 .. 0f "1234") = bad0b4f7cae08eb7c1b5acc763a8ed25 2424..., computed with
        // `printf '\x00...\x0f1234' | sha256sum`.
        let salt: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(hex::encode(derive_key(&salt, "1234")), "bad0b4f7cae08eb7c1b5acc763a8ed25");
    }

    /// The client's side of the exchange, written from the protocol description, so the
    /// server's half can be checked against an independent implementation.
    struct TestClient {
        key: Key,
        cert: ClientCert,
        signer: RsaKeyPair,
        challenge: [u8; 16],
        secret: [u8; 16],
    }

    impl TestClient {
        fn new(salt: &[u8; 16], pin: &str) -> Self {
            let der = pem_body(CLIENT_KEY, "PRIVATE KEY").unwrap();
            Self {
                key: derive_key(salt, pin),
                cert: ClientCert::from_pem(CLIENT_CERT).unwrap(),
                signer: RsaKeyPair::from_pkcs8(&der).unwrap(),
                challenge: [0x5A; 16],
                secret: [0xC3; 16],
            }
        }

        fn sign(&self, data: &[u8]) -> Vec<u8> {
            let mut sig = vec![0u8; self.signer.public().modulus_len()];
            self.signer.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), data, &mut sig).unwrap();
            sig
        }
    }

    fn run(pin_server: &str, pin_client: &str) -> (Result<()>, bool) {
        let identity = server();
        let salt = [9u8; 16];
        let client = TestClient::new(&salt, pin_client);
        let mut p = Pairing::new(&salt, pin_server, client.cert.clone(), [0x11; 16], [0x22; 16]);

        let resp = p.answer_challenge(&identity, &ecb_encrypt(&client.key, &client.challenge).unwrap()).unwrap();
        let resp = ecb_decrypt(&client.key, &resp).unwrap();
        assert_eq!(resp.len(), 48);
        let (server_hash, server_challenge) = resp.split_at(32);

        let mut h = Sha256::new();
        h.update(server_challenge);
        h.update(&client.cert.signature);
        h.update(client.secret);
        let ours = h.finalize().to_vec();
        let secret = p.answer_server_challenge_response(&identity, &ecb_encrypt(&client.key, &ours).unwrap()).unwrap();
        let (server_secret, server_sig) = secret.split_at(16);
        // The client checks the server's signature and its stage-2 hash: that's how
        // Moonlight notices a wrong PIN.
        let server_cert = ClientCert::from_pem(crate::identity::tests::SERVER_CERT).unwrap();
        assert!(server_cert.verify(server_secret, server_sig), "server signature");
        let mut h = Sha256::new();
        h.update(client.challenge);
        h.update(identity.cert_signature());
        h.update(server_secret);
        let client_sees_right_pin = h.finalize().as_slice() == server_hash;

        let mut cps = client.secret.to_vec();
        cps.extend_from_slice(&client.sign(&client.secret));
        (p.check_client_secret(&cps), client_sees_right_pin)
    }

    #[test]
    fn right_pin_pairs() {
        let (server_side, client_side) = run("4321", "4321");
        server_side.unwrap();
        assert!(client_side);
    }

    #[test]
    fn wrong_pin_fails_both_sides() {
        let (server_side, client_side) = run("4321", "4320");
        assert!(server_side.unwrap_err().to_string().contains("PIN"));
        assert!(!client_side);
    }

    #[test]
    fn forged_signature_fails() {
        let identity = server();
        let salt = [1u8; 16];
        let client = TestClient::new(&salt, "1111");
        let mut p = Pairing::new(&salt, "1111", client.cert.clone(), [3; 16], [4; 16]);
        p.answer_challenge(&identity, &[0u8; 16]).unwrap();
        p.answer_server_challenge_response(&identity, &[0u8; 32]).unwrap();
        let mut cps = client.secret.to_vec();
        cps.extend_from_slice(&[0u8; 256]);
        assert!(p.check_client_secret(&cps).unwrap_err().to_string().contains("signed"));
    }

    #[test]
    fn stages_in_order() {
        let identity = server();
        let cert = ClientCert::from_pem(CLIENT_CERT).unwrap();
        let mut p = Pairing::new(&[0; 16], "0000", cert, [0; 16], [0; 16]);
        assert!(p.answer_server_challenge_response(&identity, &[0; 32]).is_err());
        assert!(p.check_client_secret(&[0; 272]).is_err());
        assert!(p.answer_challenge(&identity, &[0; 15]).is_err());
        p.answer_challenge(&identity, &[0; 16]).unwrap();
        assert!(p.answer_challenge(&identity, &[0; 16]).is_err());
    }
}
