//! An optional content-addressed store node the daemon runs from `[cas]` config.
//!
//! When `[cas]` is present, the daemon builds a [`CasStore`] and schedules periodic
//! retention maintenance — coldest-first LRU eviction, or nothing in write-only mode,
//! per the configured mode. `[cas] backend` picks the blobs' home — the in-memory
//! [`MemCas`] or an on-disk [`DiskCas`] — behind one `Box<dyn ReclaimableCas>`; the
//! router is an in-process [`MemRouter`] until a real network router (NATS /
//! Kademlia) slots in **without touching this maintenance logic**.

use std::sync::Arc;
use std::time::Duration;

use std::io::Read;

use anyhow::Context;
use nessie_backend_core::{
    ActionCacheBackend, BackendError, CasBackend, ContentRouter, Digest, PeerId, ReclaimableCas,
};
use nessie_backend_mem::{MemCas, MemRouter};
use nessie_cas_disk::DiskCas;
use nessie_cas_store::{
    CasStore, Clock, NoDurableOracle, RefuseToReplicate, RetentionPolicy, RootRegistry,
    StorageMode, SystemClock,
};

use crate::config::{CasBackendKind, CasConfig, CasMode};

/// Build a [`RetentionPolicy`] from `[cas]` config.
///
/// # Errors
///
/// Both modes require `byte_budget` (the LRU ceiling, or the write-only capacity).
pub fn retention_policy(cfg: &CasConfig) -> anyhow::Result<RetentionPolicy> {
    let mode = match cfg.mode {
        CasMode::Lru => "lru",
        CasMode::WriteOnly => "write_only",
    };
    let budget = cfg
        .byte_budget
        .with_context(|| format!("[cas] mode = \"{mode}\" requires byte_budget"))?;
    Ok(match cfg.mode {
        CasMode::Lru => RetentionPolicy::lru(budget, Duration::from_secs(cfg.gc_grace_secs)),
        CasMode::WriteOnly => RetentionPolicy::write_only(budget),
    })
}

/// What a maintenance pass did (for logging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceReport {
    /// An LRU eviction pass.
    Evict {
        /// Blobs evicted.
        evicted: usize,
        /// Bytes freed.
        bytes: u64,
    },
    /// Write-only mode: maintenance is a no-op — nothing is ever reclaimed.
    Retained,
}

/// A running CAS node: its retention-managed store, root registry, and schedule.
pub struct CasNode {
    store: CasStore<Box<dyn ReclaimableCas>>,
    roots: RootRegistry,
    interval: Duration,
}

impl CasNode {
    /// Construct the node from `[cas]` config, identifying this node as `me` in the
    /// swarm. Blobs live where `[cas] backend` says; the router is in-process.
    ///
    /// # Errors
    ///
    /// [`retention_policy`] validation errors; `backend = "disk"` without `path`;
    /// a disk root that cannot be opened; a backend that cannot be enumerated.
    pub fn from_config(cfg: &CasConfig, me: PeerId) -> anyhow::Result<Self> {
        let policy = retention_policy(cfg)?;
        let blobs: Box<dyn ReclaimableCas> = match cfg.backend {
            CasBackendKind::Mem => Box::new(MemCas::new()),
            CasBackendKind::Disk => {
                let path = cfg
                    .path
                    .as_ref()
                    .context("[cas] backend = \"disk\" requires path")?;
                Box::new(
                    DiskCas::open(path)
                        .with_context(|| format!("opening [cas] path {}", path.display()))?,
                )
            }
        };
        let store = CasStore::new(
            blobs,
            policy,
            Arc::new(MemRouter::new(me.clone())) as Arc<dyn ContentRouter>,
            Arc::new(SystemClock) as Arc<dyn Clock>,
            me,
        )?;
        Ok(Self {
            store,
            roots: RootRegistry::new(),
            interval: Duration::from_secs(cfg.maintenance_interval_secs),
        })
    }

    /// Run one retention maintenance pass, dispatching by mode. Write-only mode
    /// never reaches the eviction engine, so nothing is reclaimed.
    ///
    /// # Errors
    ///
    /// Propagates eviction backend errors.
    pub fn maintain_once(&self) -> anyhow::Result<MaintenanceReport> {
        match &self.store.policy().mode {
            StorageMode::WriteOnly(_) => Ok(MaintenanceReport::Retained),
            StorageMode::Lru(_) => {
                let report = self.store.evict_to_budget(
                    &self.roots,
                    &NoDurableOracle,
                    &RefuseToReplicate,
                )?;
                Ok(MaintenanceReport::Evict {
                    evicted: report.evicted,
                    bytes: report.bytes_evicted,
                })
            }
        }
    }

    /// The retention-managed store (a future protocol face serves blobs from it).
    #[must_use]
    pub fn store(&self) -> &CasStore<Box<dyn ReclaimableCas>> {
        &self.store
    }

    /// The node's root registry (register confirmed AC entries / pins here).
    #[must_use]
    pub fn roots(&self) -> &RootRegistry {
        &self.roots
    }

    /// The maintenance interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

/// The node *is* a [`CasBackend`] — its retention-managed store — so protocol faces
/// can be handed `Arc<CasNode>` and share one store with maintenance. Every method
/// forwards, including `as_action_cache` (the store has none; a face layers its own).
impl CasBackend for CasNode {
    fn has(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.store.has(digest)
    }

    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        self.store.get(digest)
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        let digest = self.store.put(source)?;
        // A protocol write is a complete operation (the upload *is* the op): release
        // the in-flight guard so it does not pin the blob for the process lifetime.
        self.store.committed(&digest);
        Ok(digest)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        self.store.put_keyed(expected, source)?;
        self.store.committed(expected); // see `put`
        Ok(())
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, BackendError> {
        self.store.size(digest)
    }

    fn as_action_cache(&self) -> Option<&dyn ActionCacheBackend> {
        self.store.as_action_cache()
    }
}

/// Spawn a background task that runs [`CasNode::maintain_once`] every
/// [`CasNode::interval`]. The pass runs on the blocking pool (a real disk backend's
/// sweep is I/O-bound), so it never stalls the async control plane.
pub fn spawn_maintenance(node: Arc<CasNode>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(node.interval());
        loop {
            ticker.tick().await;
            let n = node.clone();
            match tokio::task::spawn_blocking(move || n.maintain_once()).await {
                Ok(Ok(report)) => tracing::info!(?report, "cas retention maintenance pass"),
                Ok(Err(e)) => tracing::error!(%e, "cas retention maintenance failed"),
                Err(e) => tracing::error!(%e, "cas retention maintenance task panicked"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lru_cfg() -> CasConfig {
        CasConfig {
            gc_grace_secs: 0,
            byte_budget: Some(0),
            ..CasConfig::default()
        }
    }

    #[test]
    fn disk_backend_without_path_is_a_clear_error() {
        let cfg = CasConfig {
            backend: CasBackendKind::Disk,
            ..lru_cfg()
        };
        let err = CasNode::from_config(&cfg, PeerId::new("me"))
            .err()
            .expect("disk without path must fail");
        assert!(
            err.to_string().contains("requires path"),
            "error names the missing key: {err}"
        );
    }

    #[test]
    fn both_modes_require_a_budget() {
        for mode in [CasMode::Lru, CasMode::WriteOnly] {
            let mut cfg = CasConfig {
                mode,
                byte_budget: None,
                ..lru_cfg()
            };
            assert!(
                retention_policy(&cfg).is_err(),
                "{mode:?} without byte_budget"
            );
            cfg.byte_budget = Some(1024);
            assert!(retention_policy(&cfg).is_ok());
        }
    }

    #[test]
    fn lru_node_evicts_a_sole_copy_over_budget() {
        let node = CasNode::from_config(&lru_cfg(), PeerId::new("me")).unwrap();
        let d = node.put(&mut &b"sole-copy"[..]).unwrap();
        let report = node.maintain_once().unwrap();
        assert_eq!(
            report,
            MaintenanceReport::Evict {
                evicted: 1,
                bytes: 9
            }
        );
        assert!(!node.has(&d).unwrap());
    }

    #[test]
    fn write_only_node_maintenance_retains_everything() {
        let cfg = CasConfig {
            mode: CasMode::WriteOnly,
            byte_budget: Some(64),
            ..lru_cfg()
        };
        let node = CasNode::from_config(&cfg, PeerId::new("me")).unwrap();
        let d = node.put(&mut &b"keep-me"[..]).unwrap();
        assert_eq!(node.maintain_once().unwrap(), MaintenanceReport::Retained);
        assert!(node.has(&d).unwrap());
    }

    #[test]
    fn write_only_node_refuses_a_write_past_capacity() {
        let cfg = CasConfig {
            mode: CasMode::WriteOnly,
            byte_budget: Some(4),
            ..lru_cfg()
        };
        let node = CasNode::from_config(&cfg, PeerId::new("me")).unwrap();
        assert!(matches!(
            node.put(&mut &b"too-big"[..]),
            Err(BackendError::CapacityExceeded { .. })
        ));
        assert!(!node.has(&Digest::compute(b"too-big")).unwrap());
    }
}
