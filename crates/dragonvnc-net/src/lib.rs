//! Transport and trust layer for dragonvnc: QUIC endpoints (via `quinn`) and
//! the no-CA pairing/pinning identity model described in `DESIGN.md`.

pub mod endpoint;
pub mod identity;
pub mod paired_clients;
pub mod pairing;
pub mod trust_store;
pub mod verifier;

pub use identity::{ClientIdentity, Fingerprint, ServerIdentity};
pub use paired_clients::PairedClients;
pub use pairing::PairingCode;
pub use trust_store::TrustStore;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn any_loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)
    }

    /// Runs both ends of one pairing ceremony over real loopback QUIC/UDP
    /// sockets: server accepts with its real identity, client connects with
    /// the accept-any verifier and its own client identity (item 5: mutual
    /// TLS both ways), both run SPAKE2 + channel-binding confirmation on a
    /// bidi stream, and we assert each side's derived fingerprint matches
    /// the other's actual identity.
    #[tokio::test]
    async fn pairing_succeeds_and_yields_correct_fingerprint() {
        let identity = ServerIdentity::generate("dragonvnc-test-server").unwrap();
        let server_ep = endpoint::server_endpoint(any_loopback(), &identity).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

        let client_identity = ClientIdentity::generate("dragonvnc-test-client").unwrap();
        let client_fingerprint_expected = client_identity.fingerprint();

        let code = PairingCode::generate();
        let code_for_server = code.clone();

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("no incoming connection");
            let connection = incoming.await.expect("server handshake failed");
            let (mut send, mut recv) = connection.accept_bi().await.expect("no bidi stream");
            pairing::run(&connection, &mut send, &mut recv, &code_for_server)
                .await
                .expect("server-side pairing failed")
        });

        let client_ep = endpoint::client_endpoint_for_pairing(any_loopback(), &client_identity).unwrap();
        let connection = client_ep
            .connect(server_addr, "dragonvnc")
            .expect("client connect() setup failed")
            .await
            .expect("client handshake failed");
        let (mut send, mut recv) = connection.open_bi().await.expect("open_bi failed");
        let client_fingerprint = pairing::run(&connection, &mut send, &mut recv, &code)
            .await
            .expect("client-side pairing failed");

        let server_fingerprint = server_task.await.expect("server task panicked");

        assert_eq!(client_fingerprint, Some(identity.fingerprint()));
        // Item 5: both sides now present a TLS client cert (mutual TLS), so
        // the server side of this symmetric exchange gets a real peer
        // fingerprint back too — this is what makes pinning the *client*
        // (not just the server) possible.
        assert_eq!(server_fingerprint, Some(client_fingerprint_expected));
    }

    /// A wrong pairing code must fail the ceremony, not silently succeed.
    #[tokio::test]
    async fn pairing_fails_with_wrong_code() {
        let identity = ServerIdentity::generate("dragonvnc-test-server").unwrap();
        let server_ep = endpoint::server_endpoint(any_loopback(), &identity).unwrap();
        let server_addr = server_ep.local_addr().unwrap();
        let client_identity = ClientIdentity::generate("dragonvnc-test-client").unwrap();

        let server_code = PairingCode::generate();
        let client_code = PairingCode::generate(); // different code

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            pairing::run(&connection, &mut send, &mut recv, &server_code).await
        });

        let client_ep = endpoint::client_endpoint_for_pairing(any_loopback(), &client_identity).unwrap();
        let connection = client_ep
            .connect(server_addr, "dragonvnc")
            .unwrap()
            .await
            .unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let client_result = pairing::run(&connection, &mut send, &mut recv, &client_code).await;

        let server_result = server_task.await.unwrap();

        assert!(client_result.is_err(), "pairing must not succeed with mismatched codes");
        assert!(server_result.is_err(), "pairing must not succeed with mismatched codes");
    }

    /// Once pinned, a client using `client_endpoint_pinned` must accept the
    /// real server again with no pairing step...
    #[tokio::test]
    async fn pinned_reconnect_accepts_real_server() {
        let identity = ServerIdentity::generate("dragonvnc-test-server").unwrap();
        let fingerprint = identity.fingerprint();
        let server_ep = endpoint::server_endpoint(any_loopback(), &identity).unwrap();
        let server_addr = server_ep.local_addr().unwrap();
        let client_identity = ClientIdentity::generate("dragonvnc-test-client").unwrap();

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            // Just prove the handshake completed; nothing else to do.
            connection.closed().await;
        });

        let client_ep = endpoint::client_endpoint_pinned(any_loopback(), fingerprint, &client_identity).unwrap();
        let connection = client_ep
            .connect(server_addr, "dragonvnc")
            .unwrap()
            .await
            .expect("pinned client must accept the real server's certificate");
        connection.close(0u32.into(), b"done");

        server_task.await.unwrap();
    }

    /// ...and must reject an impostor presenting a different identity, i.e.
    /// the pin actually does something.
    #[tokio::test]
    async fn pinned_reconnect_rejects_impostor() {
        let real_identity = ServerIdentity::generate("real-server").unwrap();
        let real_fingerprint = real_identity.fingerprint();
        let client_identity = ClientIdentity::generate("dragonvnc-test-client").unwrap();

        // A different server identity standing in for an impostor/MITM.
        let impostor_identity = ServerIdentity::generate("impostor-server").unwrap();
        let impostor_ep = endpoint::server_endpoint(any_loopback(), &impostor_identity).unwrap();
        let impostor_addr = impostor_ep.local_addr().unwrap();

        let impostor_task = tokio::spawn(async move {
            // The impostor will get a connection attempt that fails at the
            // TLS layer before any application data flows; just drain it.
            let _ = impostor_ep.accept().await;
        });

        let client_ep = endpoint::client_endpoint_pinned(any_loopback(), real_fingerprint, &client_identity).unwrap();
        let result = client_ep.connect(impostor_addr, "dragonvnc").unwrap().await;

        assert!(result.is_err(), "pinned client must reject a non-matching certificate");

        impostor_task.abort();
    }

    /// A client presenting no valid certificate at all can't complete the
    /// TLS handshake against a server that now mandates client auth (item
    /// 5) — proven by temporarily forcing `with_no_client_auth` on a bare
    /// rustls client config (standing in for "no `ClientIdentity` at all",
    /// which this crate's own client endpoints never actually produce).
    #[tokio::test]
    async fn server_rejects_a_client_presenting_no_certificate() {
        let identity = ServerIdentity::generate("dragonvnc-test-server").unwrap();
        let server_ep = endpoint::server_endpoint(any_loopback(), &identity).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            // The handshake never completes, so this either gets a failed
            // incoming or times out — either is fine, we just need the
            // task to end so `impostor_task`-style hangs don't wedge.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                if let Some(incoming) = server_ep.accept().await {
                    let _ = incoming.await;
                }
            })
            .await;
        });

        endpoint::ensure_crypto_provider();
        let mut rustls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(verifier::AcceptAnyForPairing))
            .with_no_client_auth();
        rustls_config.alpn_protocols = vec![endpoint::ALPN.to_vec()];
        let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config).unwrap();
        let client_config = quinn::ClientConfig::new(std::sync::Arc::new(quic_crypto));
        let mut endpoint = quinn::Endpoint::client(any_loopback()).unwrap();
        endpoint.set_default_client_config(client_config);

        // The client-side handshake future can resolve `Ok` before the
        // server's mandatory-client-cert rejection (a CONNECTION_CLOSE sent
        // after processing the client's empty Certificate message) arrives
        // back — so the real assertion is that the connection doesn't stay
        // usable, checked via `closed()` rather than `connect()` itself.
        let connect_result = endpoint.connect(server_addr, "dragonvnc").unwrap().await;
        match connect_result {
            Err(_) => {} // rejected immediately — also acceptable
            Ok(connection) => {
                let close = tokio::time::timeout(std::time::Duration::from_secs(2), connection.closed())
                    .await
                    .expect("connection neither failed nor closed — server accepted a client with no certificate");
                tracing::debug!(?close, "connection closed as expected");
            }
        }

        server_task.await.unwrap();
    }
}
