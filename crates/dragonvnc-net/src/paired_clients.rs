//! Server-side registry of paired client device fingerprints — the "who is
//! allowed to log in without re-pairing" list, and the server-side
//! counterpart to [`crate::trust_store::TrustStore`] (which is client-side:
//! which *server* fingerprint a client trusts). Populated by a successful
//! [`crate::pairing::run`] on the server side; consulted on every later
//! connection to decide whether the pairing ceremony can be skipped. See
//! PLAN-headless-session.md item 5.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::identity::Fingerprint;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PairedClients {
    fingerprints: HashSet<Fingerprint>,
}

impl PairedClients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_paired(&self, fingerprint: &Fingerprint) -> bool {
        self.fingerprints.contains(fingerprint)
    }

    /// Pins `fingerprint` as a trusted device. Returns `true` if it wasn't
    /// already paired (a genuinely new device).
    pub fn pair(&mut self, fingerprint: Fingerprint) -> bool {
        self.fingerprints.insert(fingerprint)
    }

    /// Un-pins `fingerprint`. Returns `true` if it was actually paired
    /// (i.e. this had an effect).
    pub fn revoke(&mut self, fingerprint: &Fingerprint) -> bool {
        self.fingerprints.remove(fingerprint)
    }

    pub fn list(&self) -> impl Iterator<Item = &Fingerprint> {
        self.fingerprints.iter()
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let bytes = std::fs::read(path)?;
        Ok(bincode::deserialize(&bytes)?)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = bincode::serialize(self)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_revoke_and_persist_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dragonvnc-paired-clients-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paired_clients");

        let fp_a: Fingerprint = [1u8; 32];
        let fp_b: Fingerprint = [2u8; 32];

        let mut store = PairedClients::new();
        assert!(store.pair(fp_a));
        assert!(!store.pair(fp_a)); // already paired, no-op
        store.save_to(&path).unwrap();

        let reloaded = PairedClients::load_from(&path).unwrap();
        assert!(reloaded.is_paired(&fp_a));
        assert!(!reloaded.is_paired(&fp_b));

        let mut store = reloaded;
        assert!(store.revoke(&fp_a));
        assert!(!store.revoke(&fp_a)); // already gone
        assert!(!store.is_paired(&fp_a));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
