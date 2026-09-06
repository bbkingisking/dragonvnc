//! Pairing ceremony: bootstraps trust with no CA and no prior shared secret
//! beyond a short one-time code shown by the server and typed into the
//! client.
//!
//! Both sides run the exact same symmetric SPAKE2 exchange (there's no
//! distinguished "A"/"B" role — whichever process is dialing vs. listening
//! at the QUIC level is irrelevant to the pairing math). SPAKE2 alone would
//! prove "both sides know the code", but the QUIC/TLS connection it's
//! running over was accepted with [`crate::verifier::AcceptAnyForPairing`],
//! i.e. not yet authenticated — an active MITM could terminate TLS and
//! relay two separate connections. To catch that, the SPAKE2-derived key is
//! combined with this *specific* connection's TLS exporter secret
//! (RFC 5705) before either side confirms success. A MITM relaying two
//! separate TLS connections produces two different exporter secrets, so the
//! confirmation values won't match and pairing fails — even though the
//! MITM could complete SPAKE2 on each leg individually.
//!
//! On success, each side learns the other's certificate fingerprint (via
//! `Connection::peer_identity`) already bound to a verified shared secret,
//! so it's safe to pin.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::identity::{fingerprint_of_der, Fingerprint};

const PAIRING_CONTEXT: &[u8] = b"dragonvnc-pairing-v1";
const EXPORTER_LABEL: &[u8] = b"dragonvnc-pairing-exporter-v1";
const EXPORTER_LEN: usize = 32;
const CONFIRM_LEN: usize = 32;

/// Alphabet with visually-ambiguous characters removed (no 0/O, 1/I/l),
/// so a human can read a code off a screen and type it without transposing.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// Generates a fresh random code. 8 characters from a 31-symbol alphabet
    /// is ~39 bits of entropy — irrelevant against online guessing since the
    /// server should rate-limit/expire pairing attempts (a few tries, a
    /// couple of minutes), which is the actual defense for any human-typed
    /// PAKE password.
    pub fn generate() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let s: String = (0..8)
            .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
            .collect();
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PairingCode {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("i/o error during pairing exchange: {0}")]
    Io(#[from] std::io::Error),
    #[error("quinn write error: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("quinn read error: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("spake2 exchange failed: {0}")]
    Spake(String),
    #[error("could not export TLS keying material (output buffer length unsupported)")]
    Exporter,
    #[error("pairing confirmation mismatch — wrong code, or a man-in-the-middle")]
    ConfirmationMismatch,
}

/// Runs the pairing ceremony over an already-open bidirectional QUIC stream
/// on a connection that was accepted with `AcceptAnyForPairing`. Symmetric on
/// both ends: call this identically from the dialing and the listening side.
///
/// Returns the peer's certificate fingerprint, safe to pin, on success — or
/// `None` if the peer presented no certificate, which is the expected case
/// on the *server* side in v1 (server-only auth, no client TLS identity;
/// see DESIGN.md). A client getting `None` back is the real error case (the
/// server always has one) and should treat that as pairing having failed.
pub async fn run(
    connection: &quinn::Connection,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    code: &PairingCode,
) -> Result<Option<Fingerprint>, PairingError> {
    let (state, outbound) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_str().as_bytes()),
        &Identity::new(PAIRING_CONTEXT),
    );

    send_frame(send, &outbound).await?;
    let inbound = recv_frame(recv).await?;

    let shared_key = state
        .finish(&inbound)
        .map_err(|e| PairingError::Spake(format!("{e:?}")))?;

    let mut exporter = [0u8; EXPORTER_LEN];
    connection
        .export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|_| PairingError::Exporter)?;

    let my_confirm = confirm_value(&shared_key, &exporter);

    send_frame(send, &my_confirm).await?;
    let peer_confirm = recv_frame(recv).await?;

    use subtle::ConstantTimeEq;
    if !bool::from(peer_confirm.ct_eq(&my_confirm[..])) {
        return Err(PairingError::ConfirmationMismatch);
    }

    let fingerprint = connection
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok())
        .and_then(|certs| certs.first().map(fingerprint_of_der));

    Ok(fingerprint)
}

fn confirm_value(shared_key: &[u8], exporter: &[u8; EXPORTER_LEN]) -> [u8; CONFIRM_LEN] {
    let mut mac = Hmac::<Sha256>::new_from_slice(shared_key)
        .expect("HMAC accepts keys of any length");
    mac.update(b"dragonvnc-pairing-confirm-v1");
    mac.update(exporter);
    mac.finalize().into_bytes().into()
}

async fn send_frame(send: &mut quinn::SendStream, bytes: &[u8]) -> Result<(), PairingError> {
    let len = (bytes.len() as u32).to_be_bytes();
    send.write_all(&len).await?;
    send.write_all(bytes).await?;
    Ok(())
}

async fn recv_frame(recv: &mut quinn::RecvStream) -> Result<Vec<u8>, PairingError> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}
