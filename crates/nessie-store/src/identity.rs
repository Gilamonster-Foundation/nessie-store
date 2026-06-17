//! Stable cluster identity, minted once and persisted.
//!
//! ONTAP clients (Trident especially) cache the cluster + SVM UUIDs at bind time
//! and mark the backend **offline** if they ever change. So every identifier the
//! static metadata reports is minted on first boot and persisted to
//! `data_dir/identity.json`, surviving restarts (and code redeploys).

use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The set of stable UUIDs the control plane reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// Cluster UUID (`GET /api/cluster`).
    pub cluster_uuid: String,
    /// SVM UUID (`GET /api/svm/svms`).
    pub svm_uuid: String,
    /// Single node UUID (`GET /api/cluster/nodes`).
    pub node_uuid: String,
    /// Single aggregate UUID (`GET /api/storage/aggregates`).
    pub aggregate_uuid: String,
    /// Data-LIF interface UUID (`GET /api/network/ip/interfaces`).
    pub lif_uuid: String,
}

impl Identity {
    fn mint() -> Self {
        Self {
            cluster_uuid: Uuid::new_v4().to_string(),
            svm_uuid: Uuid::new_v4().to_string(),
            node_uuid: Uuid::new_v4().to_string(),
            aggregate_uuid: Uuid::new_v4().to_string(),
            lif_uuid: Uuid::new_v4().to_string(),
        }
    }

    /// Load the persisted identity, or mint a fresh one and persist it.
    ///
    /// This is the *only* place identity is created; once written it is never
    /// regenerated, so clients that cached the UUIDs stay bound.
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading identity {}: {e}", path.display()))?;
            let id: Identity = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing identity {}: {e}", path.display()))?;
            Ok(id)
        } else {
            let id = Self::mint();
            id.persist(path)?;
            Ok(id)
        }
    }

    fn persist(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).expect("identity serializes");
        std::fs::write(path, text)
            .map_err(|e| anyhow::anyhow!("writing identity {}: {e}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_persists_and_is_stable_across_loads() {
        let dir = std::env::temp_dir().join(format!("nessie-id-{}", Uuid::new_v4()));
        let path = dir.join("identity.json");

        let first = Identity::load_or_create(&path).expect("mint");
        let second = Identity::load_or_create(&path).expect("reload");
        // The load-bearing invariant: identity survives restarts unchanged.
        assert_eq!(first.cluster_uuid, second.cluster_uuid);
        assert_eq!(first.svm_uuid, second.svm_uuid);
        assert_eq!(first.aggregate_uuid, second.aggregate_uuid);

        std::fs::remove_dir_all(&dir).ok();
    }
}
