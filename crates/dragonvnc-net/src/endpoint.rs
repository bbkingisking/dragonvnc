//! Building `quinn::Endpoint`s for the two situations this crate cares
//! about: a server presenting its long-term identity, and a client either
//! mid-pairing (accept-any verifier) or reconnecting to an already-pinned
//! server (fingerprint-checked verifier).

use std::net::SocketAddr;
use std::sync::{Arc, Once};

use rustls::pki_types::PrivateKeyDer;

use crate::identity::{Fingerprint, ServerIdentity};
use crate::verifier::{AcceptAnyForPairing, PinnedVerifier};

pub const ALPN: &[u8] = b"dragonvnc/1";

static CRYPTO_PROVIDER_INIT: Once = Once::new();

/// Installs rustls's default crypto provider (ring) process-wide. Must run
/// before building any rustls config; idempotent.
pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing rustls default crypto provider (should only happen once)");
    });
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut cfg = quinn::TransportConfig::default();
    // LAN-oriented defaults for v1 (see DESIGN.md network-scope decision).
    // Revisit once WAN/relay support is in scope.
    cfg.max_idle_timeout(Some(std::time::Duration::from_secs(15).try_into().unwrap()));
    cfg.keep_alive_interval(Some(std::time::Duration::from_secs(3)));
    Arc::new(cfg)
}

pub fn server_endpoint(
    bind_addr: SocketAddr,
    identity: &ServerIdentity,
) -> anyhow::Result<quinn::Endpoint> {
    ensure_crypto_provider();

    let certs = vec![identity.cert_der.clone()];
    let key = PrivateKeyDer::Pkcs8(identity.key_der.clone_key());

    let mut rustls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    rustls_config.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    server_config.transport_config(transport_config());

    let endpoint = quinn::Endpoint::server(server_config, bind_addr)?;
    Ok(endpoint)
}

fn client_endpoint_with_verifier(
    bind_addr: SocketAddr,
    verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
) -> anyhow::Result<quinn::Endpoint> {
    ensure_crypto_provider();

    let mut rustls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    rustls_config.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(transport_config());

    let mut endpoint = quinn::Endpoint::client(bind_addr)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// A client endpoint willing to talk to *any* server identity. Only ever
/// used for the duration of a pairing ceremony — see `crate::pairing`.
pub fn client_endpoint_for_pairing(bind_addr: SocketAddr) -> anyhow::Result<quinn::Endpoint> {
    client_endpoint_with_verifier(bind_addr, Arc::new(AcceptAnyForPairing))
}

/// A client endpoint that only accepts the given pinned server fingerprint.
/// This is the normal, no-code-needed reconnection path.
pub fn client_endpoint_pinned(
    bind_addr: SocketAddr,
    expected: Fingerprint,
) -> anyhow::Result<quinn::Endpoint> {
    client_endpoint_with_verifier(bind_addr, Arc::new(PinnedVerifier { expected }))
}
