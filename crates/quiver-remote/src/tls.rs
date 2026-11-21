//! Mutual TLS between machines (design/REMOTE.md §3).
//!
//! Both ends present their machine certificate (self-signed, Ed25519) and TLS 1.3 only is
//! spoken. Certificates aren't checked against any authority; the key is what counts:
//!
//! - **The client** pins the server: [`connect`] with a [`Pin`] refuses, inside the TLS
//!   handshake, a server whose certificate carries any other key. Only pairing connects
//!   without one (it then confirms the certificate under the PAKE key, `pair`).
//! - **The server** takes any client certificate whose handshake signature is good (TLS
//!   proves the client holds the key), and [`Acceptor::accept`] hands back the client's
//!   pin. quiverd then serves it only if the pin is a paired client's, or pairs it.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{self, DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::identity::{Identity, Pin, sha256};

pub type ClientStream = tokio_rustls::client::TlsStream<TcpStream>;
pub type ServerStream = tokio_rustls::server::TlsStream<TcpStream>;

/// ALPN: refuses anything that isn't a Quiver control connection.
const ALPN: &[u8] = b"quiver-remote/1";
/// TCP connect plus TLS handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn algorithms() -> WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

/// What the other end presented: its key's pin and its certificate's hash.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PeerCert {
    pub pin: Pin,
    /// SHA-256 of the DER certificate (what pairing confirms).
    pub cert_hash: [u8; 32],
}

impl fmt::Debug for PeerCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerCert({})", self.pin.id())
    }
}

impl PeerCert {
    fn of(der: &[u8]) -> Result<PeerCert> {
        Ok(PeerCert { pin: Pin::of_cert(der)?, cert_hash: sha256(der) })
    }
}

fn general(e: impl fmt::Display) -> rustls::Error {
    rustls::Error::General(e.to_string())
}

/// The client's check of the server: an Ed25519 certificate, with the pinned key if one is
/// expected.
#[derive(Debug)]
struct ServerPin {
    expect: Option<Pin>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ServerPin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let pin = Pin::of_cert(end_entity).map_err(general)?;
        match self.expect {
            Some(want) if want != pin => Err(general(format!("the machine's key changed (expected {}, got {})", want.id(), pin.id()))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(general("TLS 1.2 isn't spoken"))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// The server's check of a client: any Ed25519 certificate (the pin is checked after the
/// handshake, against the peers).
#[derive(Debug)]
struct AnyClientKey {
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientKey {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        Pin::of_cert(end_entity).map_err(general)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(general("TLS 1.2 isn't spoken"))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn key(me: &Identity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(me.key_der().to_vec()))
}

/// Accepts control connections with this machine's certificate.
#[derive(Clone)]
pub struct Acceptor(tokio_rustls::TlsAcceptor);

impl Acceptor {
    pub fn new(me: &Identity) -> Result<Acceptor> {
        let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(AnyClientKey { algs: algorithms() }))
            .with_single_cert(vec![CertificateDer::from(me.cert_der().to_vec())], key(me))
            .context("the machine certificate")?;
        cfg.alpn_protocols = vec![ALPN.to_vec()];
        Ok(Acceptor(tokio_rustls::TlsAcceptor::from(Arc::new(cfg))))
    }

    /// The TLS handshake on an accepted connection, and what the client presented.
    pub async fn accept(&self, tcp: TcpStream) -> Result<(ServerStream, PeerCert)> {
        let tls = tokio::time::timeout(CONNECT_TIMEOUT, self.0.accept(tcp)).await.context("TLS handshake timed out")??;
        let cert = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).context("no client certificate")?;
        let peer = PeerCert::of(cert)?;
        Ok((tls, peer))
    }
}

/// Connect to `addr` (`host:port`) with this machine's certificate. With `expect`, the
/// server must present that key (else the handshake fails); without (pairing only), any
/// Ed25519 key is taken and reported.
pub async fn connect(me: &Identity, addr: &str, expect: Option<Pin>) -> Result<(ClientStream, PeerCert)> {
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ServerPin { expect, algs: algorithms() }))
        .with_client_auth_cert(vec![CertificateDer::from(me.cert_der().to_vec())], key(me))
        .context("the machine certificate")?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let run = async {
        let tcp = TcpStream::connect(addr).await.with_context(|| format!("connecting to {addr}"))?;
        let _ = tcp.set_nodelay(true);
        // Names aren't checked (the key is); any valid name will do.
        let name = ServerName::try_from("quiver").expect("a valid name");
        connector.connect(name, tcp).await.map_err(|e| anyhow!(e).context(format!("TLS with {addr}")))
    };
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, run).await.map_err(|_| anyhow!("{addr} didn't answer within {}s", CONNECT_TIMEOUT.as_secs()))??;
    let cert = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).context("no server certificate")?;
    let peer = PeerCert::of(cert)?;
    Ok((tls, peer))
}

/// `addr` with the default port when it has none (`tower` → `tower:47990`).
pub fn with_default_port(addr: &str) -> String {
    let a = addr.trim();
    let has_port = if let Some(rest) = a.strip_prefix('[') {
        rest.contains("]:")
    } else {
        a.matches(':').count() == 1
    };
    if has_port {
        a.to_owned()
    } else if a.contains(':') {
        format!("[{a}]:{}", crate::DEFAULT_PORT)
    } else {
        format!("{a}:{}", crate::DEFAULT_PORT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port() {
        assert_eq!(with_default_port("tower"), "tower:47990");
        assert_eq!(with_default_port("10.0.0.2:5000"), "10.0.0.2:5000");
        assert_eq!(with_default_port("::1"), "[::1]:47990");
        assert_eq!(with_default_port("[::1]:9"), "[::1]:9");
    }

    async fn pair_of_ends(expect: impl FnOnce(&Identity) -> Option<Pin>) -> (Result<PeerCert>, Result<PeerCert>, Identity, Identity) {
        let t = tempfile::tempdir().unwrap();
        let server = Identity::load_or_create(&t.path().join("s")).unwrap();
        let client = Identity::load_or_create(&t.path().join("c")).unwrap();
        let acceptor = Acceptor::new(&server).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accept = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let r = acceptor.accept(tcp).await;
            r.map(|(_, p)| p)
        });
        let want = expect(&server);
        let c = connect(&client, &addr, want).await.map(|(_, p)| p);
        let s = accept.await.unwrap();
        (c, s, server, client)
    }

    #[tokio::test]
    async fn mutual_tls_reports_both_keys() {
        let (c, s, server, client) = pair_of_ends(|s| Some(s.pin())).await;
        let c = c.unwrap();
        assert_eq!(c.pin, server.pin());
        assert_eq!(c.cert_hash, server.cert_hash());
        assert_eq!(s.unwrap().pin, client.pin());
    }

    #[tokio::test]
    async fn a_different_key_fails_the_handshake() {
        let (c, s, _, _) = pair_of_ends(|_| Some(Pin([9; 32]))).await;
        let e = format!("{:#}", c.unwrap_err());
        assert!(e.contains("key changed"), "{e}");
        assert!(s.is_err());
    }
}
