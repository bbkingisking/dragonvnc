//! Custom rustls certificate verifiers, both directions.
//!
//! There is no CA in this system, so "verifying" a certificate never means
//! chaining to a trust root. It means one of a few things:
//!
//! Client-side (verifying the *server*'s certificate):
//! - [`AcceptAnyForPairing`]: skip trust entirely (but still check the peer
//!   actually holds the private key for the cert it presented). Used only
//!   while a pairing ceremony is in progress; real authentication for that
//!   ceremony comes from the SPAKE2 channel-binding confirmation in
//!   `crate::pairing`, not from this verifier.
//! - [`PinnedVerifier`]: accept only the exact certificate fingerprint that
//!   was pinned during a prior successful pairing (SSH `known_hosts` model).
//!
//! Server-side (verifying the *client*'s certificate — item 5, "the paired
//! device is the login"):
//! - [`AcceptAnyClient`]: same reasoning as `AcceptAnyForPairing`, mirrored —
//!   the server always demands a client certificate (proving key possession
//!   is still real signature-level authentication) but never checks its
//!   identity here. The actual trust decision — is this fingerprint in the
//!   server's `PairedClients` store, or does it need to complete pairing
//!   first — happens one layer up, in `dragonvnc-server`'s connection
//!   handler, exactly like the client-side pairing decision does.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
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

/// Server-side: accepts any client certificate (still proving the peer
/// holds the matching private key — real TLS authentication, just no
/// identity check here) and *requires* one (`client_auth_mandatory`) —
/// every connection presents a client cert, paired or not. See module doc:
/// the actual "is this fingerprint allowed in" decision is made one layer
/// up, after the handshake, against `PairedClients`.
#[derive(Debug)]
pub struct AcceptAnyClient;

impl ClientCertVerifier for AcceptAnyClient {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA, so no root subjects to hint — an empty list tells the
        // client "send whatever client certificate you have".
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        Ok(ClientCertVerified::assertion())
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
