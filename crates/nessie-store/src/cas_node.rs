//! An optional content-addressed store node the daemon runs from `[cas]` config.
//!
//! When `[cas]` is present, the daemon builds a [`CasStore`] and schedules periodic
//! retention maintenance — durable reachability GC or cache replica-gated eviction,
//! per the configured mode. The store is currently backed by the in-memory
//! [`MemCas`] and an in-process [`MemRouter`]; a persistent disk backend and a real
//! network router (NATS / Kademlia) slot in later **without touching this
//! maintenance logic** — they are just different `ReclaimableCas` / `ContentRouter`
//! implementations behind the same seams.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use nessie_backend_core::{ContentRouter, PeerId};
use nessie_backend_mem::{MemCas, MemRouter};
use nessie_cas_store::{
    CanonicalResolver, CasStore, Clock, NoDurableOracle, RefuseToReplicate, RetentionPolicy,
    RootRegistry, StorageMode, SystemClock,
};

use crate::config::{CasConfig, CasMode};

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
    store: CasStore<MemCas>,
    roots: RootRegistry,
    interval: Duration,
}

impl CasNode {
    /// Construct the node from `[cas]` config, identifying this node as `me` in the
    /// swarm. Uses the in-memory CAS + in-process router for now.
    ///
    /// # Errors
    ///
    /// Propagates [`retention_policy`] validation errors.
    pub fn from_config(cfg: &CasConfig, me: PeerId) -> anyhow::Result<Self> {
        let policy = retention_policy(cfg)?;
        let store = CasStore::new(
            MemCas::new(),
            policy,
            Arc::new(MemRouter::new(me.clone())) as Arc<dyn ContentRouter>,
            Arc::new(SystemClock) as Arc<dyn Clock>,
            me,
        );
        Ok(Self {
            store,
            roots: RootRegistry::new(),
            interval: Duration::from_secs(cfg.maintenance_interval_secs),
        })
    }

    /// Run one retention maintenance pass, dispatching by mode.
    ///
    /// # Errors
    ///
    /// Propagates GC / eviction backend errors.
    pub fn maintain_once(&self) -> anyhow::Result<MaintenanceReport> {
        match &self.store.policy().mode {
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
    pub fn store(&self) -> &CasStore<MemCas> {
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
            mode: CasMode::Durable,
            gc_grace_secs: 0,
            maintenance_interval_secs: 300,
            byte_budget: None,
            replication_factor: None,
        }
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
