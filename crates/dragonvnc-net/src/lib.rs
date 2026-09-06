//! Transport and trust layer for dragonvnc: QUIC endpoints (via `quinn`) and
//! the no-CA pairing/pinning identity model described in `DESIGN.md`.

pub mod endpoint;
pub mod identity;
pub mod pairing;
pub mod trust_store;
pub mod verifier;

pub use identity::{Fingerprint, ServerIdentity};
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
    /// the accept-any verifier, both run SPAKE2 + channel-binding
    /// confirmation on a bidi stream, and we assert the client's derived
    /// fingerprint matches the server's actual identity.
    #[tokio::test]
    async fn pairing_succeeds_and_yields_correct_fingerprint() {
        let identity = ServerIdentity::generate("dragonvnc-test-server").unwrap();
        let server_ep = endpoint::server_endpoint(any_loopback(), &identity).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

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

        let client_ep = endpoint::client_endpoint_for_pairing(any_loopback()).unwrap();
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
        // v1 has no client TLS identity (server-only auth — see DESIGN.md),
        // so the server side of a symmetric exchange correctly gets no
        // peer certificate to fingerprint.
        assert_eq!(server_fingerprint, None);
    }

    /// A wrong pairing code must fail the ceremony, not silently succeed.
    #[tokio::test]
    async fn pairing_fails_with_wrong_code() {
        let identity = ServerIdentity::generate("dragonvnc-test-server").unwrap();
        let server_ep = endpoint::server_endpoint(any_loopback(), &identity).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

        let server_code = PairingCode::generate();
        let client_code = PairingCode::generate(); // different code

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            pairing::run(&connection, &mut send, &mut recv, &server_code).await
        });

        let client_ep = endpoint::client_endpoint_for_pairing(any_loopback()).unwrap();
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

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            // Just prove the handshake completed; nothing else to do.
            connection.closed().await;
        });

        let client_ep = endpoint::client_endpoint_pinned(any_loopback(), fingerprint).unwrap();
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

        // A different server identity standing in for an impostor/MITM.
        let impostor_identity = ServerIdentity::generate("impostor-server").unwrap();
        let impostor_ep = endpoint::server_endpoint(any_loopback(), &impostor_identity).unwrap();
        let impostor_addr = impostor_ep.local_addr().unwrap();

        let impostor_task = tokio::spawn(async move {
            // The impostor will get a connection attempt that fails at the
            // TLS layer before any application data flows; just drain it.
            let _ = impostor_ep.accept().await;
        });

        let client_ep = endpoint::client_endpoint_pinned(any_loopback(), real_fingerprint).unwrap();
        let result = client_ep.connect(impostor_addr, "dragonvnc").unwrap().await;

        assert!(result.is_err(), "pinned client must reject a non-matching certificate");

        impostor_task.abort();
    }
}
