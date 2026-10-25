//! An optional content-addressed store node the daemon runs from `[cas]` config.
//!
//! When `[cas]` is present, the daemon builds a [`CasStore`] and schedules periodic
//! retention maintenance — durable reachability GC or cache replica-gated eviction,
//! per the configured mode. `[cas] backend` picks the blobs' home — the in-memory
//! [`MemCas`] or an on-disk [`DiskCas`] — behind one `Box<dyn ReclaimableCas>`; the
//! router is an in-process [`MemRouter`] until a real network router (NATS /
//! Kademlia) slots in **without touching this maintenance logic**.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use std::io::Read;

use anyhow::Context;
use nessie_backend_core::{
    ActionCacheBackend, BackendError, CasBackend, ContentRouter, Digest, PeerId, ReclaimableCas,
};
use nessie_backend_mem::{MemCas, MemRouter};
use nessie_cas_disk::DiskCas;
use nessie_cas_store::{
    CanonicalResolver, CasStore, Clock, NoDurableOracle, RefuseToReplicate, RetentionPolicy,
    RootRegistry, StorageMode, SystemClock,
};

use crate::config::{CasBackendKind, CasConfig, CasMode};

/// Build a [`RetentionPolicy`] from `[cas]` config, validating cache parameters.
///
/// # Errors
///
/// Cache mode requires `byte_budget` and a `replication_factor` ≥ 1.
pub fn retention_policy(cfg: &CasConfig) -> anyhow::Result<RetentionPolicy> {
    let grace = Duration::from_secs(cfg.gc_grace_secs);
    match cfg.mode {
        CasMode::Durable => Ok(RetentionPolicy::durable(grace)),
        CasMode::Cache => {
            let budget = cfg
                .byte_budget
                .context("[cas] mode = \"cache\" requires byte_budget")?;
            let r = cfg.replication_factor.unwrap_or(2);
            let r = NonZeroUsize::new(r).context("[cas] replication_factor must be >= 1")?;
            Ok(RetentionPolicy::cache(budget, r, grace))
        }
    }
}

/// What a maintenance pass did (for logging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceReport {
    /// A durable GC pass.
    Gc {
        /// Blobs reclaimed.
        reclaimed: usize,
        /// Bytes freed.
        bytes: u64,
    },
    /// A cache eviction pass.
    Evict {
        /// Blobs evicted.
        evicted: usize,
        /// Bytes freed.
        bytes: u64,
    },
}

/// A running CAS node: its retention-managed store, root registry, and schedule.
pub struct CasNode {
    store: CasStore<Box<dyn ReclaimableCas>>,
    roots: RootRegistry,
    face_backed: AtomicBool,
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
            face_backed: AtomicBool::new(false),
            interval: Duration::from_secs(cfg.maintenance_interval_secs),
        })
    }

    /// Record that a protocol face (REAPI / S3) writes into this store.
    ///
    /// Face writes are never registered as roots, and the root registry is in
    /// memory, so after a restart durable GC would sweep every uploaded blob.
    /// Until persistent roots exist, a face-backed durable node retains everything:
    /// [`CasNode::maintain_once`] skips the GC sweep. Cache mode is unaffected (its
    /// eviction is replica-gated and refuses a sole copy).
    pub fn mark_face_backed(&self) {
        // ponytail: retain-all, no persistent roots yet; durable GC returns once
        // roots persist (follow-up #137).
        self.face_backed.store(true, Ordering::Relaxed);
    }

    /// Run one retention maintenance pass, dispatching by mode.
    ///
    /// # Errors
    ///
    /// Propagates GC / eviction backend errors.
    pub fn maintain_once(&self) -> anyhow::Result<MaintenanceReport> {
        match &self.store.policy().mode {
            StorageMode::Durable(_) if self.face_backed.load(Ordering::Relaxed) => {
                Ok(MaintenanceReport::Gc {
                    reclaimed: 0,
                    bytes: 0,
                })
            }
            StorageMode::Durable(_) => {
                let report = self.store.gc(&self.roots, &CanonicalResolver)?;
                Ok(MaintenanceReport::Gc {
                    reclaimed: report.reclaimed,
                    bytes: report.bytes_reclaimed,
                })
            }
            StorageMode::Cache(_) => {
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

    fn durable_cfg() -> CasConfig {
        CasConfig {
            gc_grace_secs: 0,
            ..CasConfig::default()
        }
    }

    #[test]
    fn disk_backend_without_path_is_a_clear_error() {
        let cfg = CasConfig {
            backend: CasBackendKind::Disk,
            ..durable_cfg()
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
    fn cache_policy_requires_a_budget() {
        let mut cfg = durable_cfg();
        cfg.mode = CasMode::Cache;
        assert!(
            retention_policy(&cfg).is_err(),
            "cache without byte_budget is rejected"
        );
        cfg.byte_budget = Some(1024);
        assert!(retention_policy(&cfg).is_ok());
    }

    #[test]
    fn zero_replication_factor_is_rejected() {
        let cfg = CasConfig {
            mode: CasMode::Cache,
            byte_budget: Some(1024),
            replication_factor: Some(0),
            ..durable_cfg()
        };
        assert!(retention_policy(&cfg).is_err());
    }

    #[test]
    fn durable_node_maintenance_runs_gc() {
        let node = CasNode::from_config(&durable_cfg(), PeerId::new("me")).unwrap();
        // Fresh node: nothing to reclaim, but the durable branch dispatches.
        assert_eq!(
            node.maintain_once().unwrap(),
            MaintenanceReport::Gc {
                reclaimed: 0,
                bytes: 0
            }
        );
    }

    #[test]
    fn cache_node_maintenance_runs_eviction() {
        let cfg = CasConfig {
            mode: CasMode::Cache,
            byte_budget: Some(0),
            replication_factor: Some(1),
            ..durable_cfg()
        };
        let node = CasNode::from_config(&cfg, PeerId::new("me")).unwrap();
        // With only self as a holder, nothing is safe to evict (fail-safe), but the
        // cache branch dispatches.
        assert_eq!(
            node.maintain_once().unwrap(),
            MaintenanceReport::Evict {
                evicted: 0,
                bytes: 0
            }
        );
    }
}
