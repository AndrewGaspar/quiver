//! The HTTPS listener's TLS: the server's own certificate, and client certificates accepted
//! at the TLS layer (Moonlight's are self-signed) and checked against the pinned list per
//! request, so an unpaired client still gets GameStream's "401" answer rather than a broken
//! handshake.
//!
//! The handshake signature is still verified against the presented certificate's key, so a
//! client can't present a pinned certificate without holding its key.

use std::sync::Arc;

use anyhow::Result;
use ring::signature as sig;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};

use crate::identity::Identity;

pub fn server_config(identity: &Identity) -> Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (chain, key) = identity.tls_parts();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_client_cert_verifier(Arc::new(PinnedLater))
        .with_single_cert(chain, key)?;
    Ok(Arc::new(config))
}

/// Accepts any client certificate whose handshake signature checks out.
#[derive(Debug)]
struct PinnedLater;

const SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PKCS1_SHA256,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::ECDSA_NISTP256_SHA256,
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::ED25519,
];

fn verify(message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
    let alg: &dyn sig::VerificationAlgorithm = match dss.scheme {
        SignatureScheme::RSA_PSS_SHA256 => &sig::RSA_PSS_2048_8192_SHA256,
        SignatureScheme::RSA_PSS_SHA384 => &sig::RSA_PSS_2048_8192_SHA384,
        SignatureScheme::RSA_PSS_SHA512 => &sig::RSA_PSS_2048_8192_SHA512,
        SignatureScheme::RSA_PKCS1_SHA256 => &sig::RSA_PKCS1_2048_8192_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384 => &sig::RSA_PKCS1_2048_8192_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512 => &sig::RSA_PKCS1_2048_8192_SHA512,
        SignatureScheme::ECDSA_NISTP256_SHA256 => &sig::ECDSA_P256_SHA256_ASN1,
        SignatureScheme::ECDSA_NISTP384_SHA384 => &sig::ECDSA_P384_SHA384_ASN1,
        SignatureScheme::ED25519 => &sig::ED25519,
        _ => return Err(Error::PeerMisbehaved(rustls::PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme)),
    };
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).map_err(|_| Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    let key = parsed.public_key().subject_public_key.data.as_ref();
    sig::UnparsedPublicKey::new(alg, key)
        .verify(message, dss.signature())
        .map(|()| HandshakeSignatureValid::assertion())
        .map_err(|_| Error::InvalidCertificate(rustls::CertificateError::BadSignature))
}

impl ClientCertVerifier for PinnedLater {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        verify(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        verify(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        SCHEMES.to_vec()
    }
}
