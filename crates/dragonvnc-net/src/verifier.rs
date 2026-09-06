//! Custom rustls server-certificate verifiers.
//!
//! There is no CA in this system, so "verifying" a certificate never means
//! chaining to a trust root. It means one of two things:
//!
//! - [`AcceptAnyForPairing`]: skip trust entirely (but still check the peer
//!   actually holds the private key for the cert it presented). Used only
//!   while a pairing ceremony is in progress; real authentication for that
//!   ceremony comes from the SPAKE2 channel-binding confirmation in
//!   `crate::pairing`, not from this verifier.
//! - [`PinnedVerifier`]: accept only the exact certificate fingerprint that
//!   was pinned during a prior successful pairing (SSH `known_hosts` model).

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use subtle::ConstantTimeEq;

use crate::identity::{fingerprint_of_der, Fingerprint};

fn provider() -> &'static CryptoProvider {
    // Installed once at process start via `crate::endpoint::ensure_crypto_provider`.
    rustls::crypto::CryptoProvider::get_default()
        .expect("rustls default crypto provider not installed")
}

#[derive(Debug)]
pub struct AcceptAnyForPairing;

impl ServerCertVerifier for AcceptAnyForPairing {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // Deliberately no chain/identity validation: this mode only runs
        // during pairing, where `pairing::run` provides the real
        // authentication via a channel-bound SPAKE2 confirmation.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider().signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
pub struct PinnedVerifier {
    pub expected: Fingerprint,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = fingerprint_of_der(end_entity);
        if bool::from(actual.ct_eq(&self.expected)) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(
                "server certificate fingerprint does not match pinned identity".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider().signature_verification_algorithms.supported_schemes()
    }
}
