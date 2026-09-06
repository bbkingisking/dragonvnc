//! Client-side pinned-server registry (the SSH `known_hosts` equivalent).
//! Populated by a successful [`crate::pairing::run`], consulted by
//! [`crate::verifier::PinnedVerifier`] on every later connection.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::identity::Fingerprint;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    // Keyed by whatever the client uses to name a server (hostname, mDNS
    // instance name, user-chosen nickname) — policy for that key lives with
    // the caller, not here.
    pins: HashMap<String, Fingerprint>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, server_id: &str) -> Option<Fingerprint> {
        self.pins.get(server_id).copied()
    }

    pub fn pin(&mut self, server_id: impl Into<String>, fingerprint: Fingerprint) {
        self.pins.insert(server_id.into(), fingerprint);
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
