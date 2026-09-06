//! Server long-term identity: a self-signed Ed25519 certificate. There is no
//! CA — trust comes from the pairing ceremony (`crate::pairing`) pinning this
//! certificate's fingerprint on the client, not from anyone signing it.

use std::path::Path;

use rcgen::{CertificateParams, KeyPair};
use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Fingerprint = [u8; 32];

pub fn fingerprint_of_der(cert: &CertificateDer<'_>) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update(cert.as_ref());
    hasher.finalize().into()
}

/// A server's long-term self-signed identity. Generate once and persist the
/// key material across restarts (a fresh identity each launch would make
/// every prior pairing's pin useless).
pub struct ServerIdentity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivatePkcs8KeyDer<'static>,
}

impl ServerIdentity {
    /// Generates a fresh Ed25519 self-signed certificate. `name` is a purely
    /// cosmetic subject/SAN (QUIC connection uses the pin, not the name, for
    /// trust) so it can be anything — a hostname or a nickname.
    pub fn generate(name: &str) -> anyhow::Result<Self> {
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let params = CertificateParams::new(vec![name.to_string()])?;
        let cert = params.self_signed(&key_pair)?;
        Ok(Self {
            cert_der: cert.der().clone(),
            key_der: PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint_of_der(&self.cert_der)
    }

    /// Loads a previously-saved identity from `path`, or generates and
    /// persists a fresh one if none exists yet. Servers should call this
    /// (not `generate`) so restarts keep the same fingerprint and don't
    /// invalidate every client's existing pin.
    pub fn load_or_generate(path: &Path, name: &str) -> anyhow::Result<Self> {
        if path.exists() {
            let bytes = std::fs::read(path)?;
            let stored: StoredIdentity = bincode::deserialize(&bytes)?;
            Ok(Self {
                cert_der: CertificateDer::from(stored.cert_der),
                key_der: PrivatePkcs8KeyDer::from(stored.key_der),
            })
        } else {
            let identity = Self::generate(name)?;
            identity.save_to(path)?;
            Ok(identity)
        }
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stored = StoredIdentity {
            cert_der: self.cert_der.to_vec(),
            key_der: self.key_der.secret_pkcs8_der().to_vec(),
        };
        let bytes = bincode::serialize(&stored)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}
