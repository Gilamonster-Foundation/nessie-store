//! An optional content-addressed store node the daemon runs from `[cas]` config.
//!
//! When `[cas]` is present, the daemon builds a [`CasStore`] and schedules periodic
//! retention maintenance — coldest-first LRU eviction, or nothing in write-only mode,
//! per the configured mode. `[cas] backend` picks the blobs' home — the in-memory
//! [`MemCas`] or an on-disk [`DiskCas`] — behind one `Box<dyn ReclaimableCas>`; the
//! router is an in-process [`MemRouter`] until a real network router (NATS /
//! Kademlia) slots in **without touching this maintenance logic**.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use std::io::Read;

use anyhow::Context;
use nessie_backend_core::{
    ActionCacheBackend, BackendError, CasBackend, ContentRouter, Digest, PeerId, ReclaimableCas,
};
use nessie_backend_mem::{MemCas, MemRouter};
use nessie_cas_disk::DiskCas;
use nessie_cas_peer::{
    HttpReplicator, Peer, PeerSet, StaticDurabilityOracle, StaticRouter, SwarmCas,
};
use nessie_cas_store::{
    CasStore, Clock, DurabilityOracle, NoDurableOracle, RefuseToReplicate, Replicator,
    RetentionPolicy, RootRegistry, StorageMode, SystemClock,
};

use crate::config::{CasBackendKind, CasConfig, CasMode};

mod scrub;
pub use scrub::ScrubReport;

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

/// `[cas] replication_factor` default: this node plus one verified peer.
const DEFAULT_REPLICATION_FACTOR: usize = 2;

/// The `write_only` mirror: the pusher and how many peer copies must confirm.
struct Mirror {
    replicator: HttpReplicator,
    /// `replication_factor - 1`.
    peer_copies: NonZeroUsize,
}

/// Peer copies a `write_only` write must reach (`R - 1`), `None` when none are
/// needed (`lru`, or R = 1 — a single-node archive, warned about once here).
///
/// # Errors
///
/// `replication_factor` under `lru`, R = 0, or R - 1 above the number of `durable`
/// peers (only those can confirm, so more could never be met).
fn peer_copies(cfg: &CasConfig) -> anyhow::Result<Option<NonZeroUsize>> {
    if cfg.mode == CasMode::Lru {
        anyhow::ensure!(
            cfg.replication_factor.is_none(),
            "[cas] replication_factor applies only to mode = \"write_only\""
        );
        return Ok(None);
    }
    let r = cfg.replication_factor.unwrap_or(DEFAULT_REPLICATION_FACTOR);
    anyhow::ensure!(r >= 1, "[cas] replication_factor must be at least 1");
    let durable = cfg.effective_peers()?.iter().filter(|p| p.durable).count();
    anyhow::ensure!(
        r - 1 <= durable,
        "[cas] replication_factor = {r} needs {} durable peers but only {durable} are configured",
        r - 1
    );
    if r == 1 {
        tracing::warn!("[cas] replication_factor = 1: write_only with no redundancy");
    }
    Ok(NonZeroUsize::new(r - 1))
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
        /// The scrub that followed.
        scrub: ScrubReport,
    },
    /// Write-only mode: nothing is ever reclaimed, so the pass only scrubs and repairs.
    Retained {
        /// The scrub and repair.
        scrub: ScrubReport,
    },
}

impl MaintenanceReport {
    /// What the scrub half of the pass did, in either mode.
    #[must_use]
    pub fn scrub(&self) -> &ScrubReport {
        match self {
            Self::Evict { scrub, .. } | Self::Retained { scrub } => scrub,
        }
    }
}

/// A running CAS node: its retention-managed store, root registry, and schedule.
pub struct CasNode {
    store: CasStore<Box<dyn ReclaimableCas>>,
    roots: RootRegistry,
    interval: Duration,
    oracle: Box<dyn DurabilityOracle>,
    replicator: Box<dyn Replicator>,
    /// The blobs held here, without peer fallback (`None`: no peers, the store is
    /// already local-only).
    local: Option<Arc<dyn CasBackend>>,
    /// `write_only` with `replication_factor > 1`: acknowledge a write only after
    /// the peers confirm.
    mirror: Option<Mirror>,
    /// Bytes a scrub pass may verify before it stops (`0`: scrub off).
    scrub_budget: u64,
    /// The last digest the scrub verified; the next pass resumes after it. In memory,
    /// so a restart rescans from the start of the inventory.
    scrub_cursor: std::sync::Mutex<Option<String>>,
    /// As `scrub_cursor`, over the quarantined digests awaiting a restore.
    pending_cursor: std::sync::Mutex<Option<String>>,
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
        let peer_copies = peer_copies(cfg)?;
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
        // With peers: fetch-through beneath the store, and the static router /
        // oracle / replicator in place of the in-process ones.
        let peer_list = cfg.effective_peers()?;
        let mut mirror = None;
        let (blobs, router, oracle, replicator, local): (_, Arc<dyn ContentRouter>, _, _, _) =
            if peer_list.is_empty() {
                (
                    blobs,
                    Arc::new(MemRouter::new(me.clone())),
                    Box::new(NoDurableOracle) as Box<dyn DurabilityOracle>,
                    Box::new(RefuseToReplicate) as Box<dyn Replicator>,
                    None,
                )
            } else {
                let peers = Arc::new(PeerSet::new(
                    peer_list
                        .iter()
                        .map(|p| Peer {
                            url: p.url.clone(),
                            durable: p.durable,
                        })
                        .collect(),
                )?);
                // write_only: serve fetched bytes without storing them (capacity gate).
                let swarm = SwarmCas::new(blobs, peers.clone())
                    .with_cache_fills(matches!(policy.mode, StorageMode::Lru(_)));
                let local: Arc<dyn CasBackend> = swarm.local();
                mirror = peer_copies.map(|peer_copies| Mirror {
                    replicator: HttpReplicator::new(peers.clone(), local.clone()),
                    peer_copies,
                });
                (
                    Box::new(swarm) as Box<dyn ReclaimableCas>,
                    Arc::new(StaticRouter::new(peers.clone())),
                    Box::new(StaticDurabilityOracle::new(peers.clone())),
                    Box::new(HttpReplicator::new(peers, local.clone())),
                    Some(local),
                )
            };
        let store = CasStore::new(
            blobs,
            policy,
            router,
            Arc::new(SystemClock) as Arc<dyn Clock>,
            me,
        )?;
        Ok(Self {
            store,
            roots: RootRegistry::new(),
            interval: Duration::from_secs(cfg.maintenance_interval_secs),
            oracle,
            replicator,
            local,
            mirror,
            scrub_budget: cfg.scrub_bytes_per_pass,
            scrub_cursor: std::sync::Mutex::new(None),
            pending_cursor: std::sync::Mutex::new(None),
        })
    }

    /// Write `expected`'s bytes to **this node only** — through the capacity gate, with
    /// no mirror fan-out. This is what a peer's `PUT /cas/v1/blobs` runs, so a push
    /// from another node never pushes again.
    ///
    /// # Errors
    ///
    /// As [`CasBackend::put_keyed`], minus [`BackendError::Underreplicated`].
    pub fn put_keyed_local(
        &self,
        expected: &Digest,
        source: &mut dyn Read,
    ) -> Result<(), BackendError> {
        self.store.put_keyed(expected, source)?;
        self.store.committed(expected); // see `put`
        Ok(())
    }

    /// After a local write: in `write_only`, wait for `replication_factor - 1` peers
    /// to hold a verified copy. The local copy stays on a shortfall.
    fn mirror(&self, digest: &Digest) -> Result<(), BackendError> {
        let Some(m) = &self.mirror else {
            return Ok(());
        };
        let confirmed = m.replicator.push(digest, m.peer_copies)?;
        if confirmed < m.peer_copies.get() {
            return Err(BackendError::Underreplicated {
                confirmed,
                required: m.peer_copies.get(),
            });
        }
        Ok(())
    }

    /// Run one maintenance pass, dispatching by mode: eviction first in `lru`
    /// (write-only never reaches the eviction engine, so nothing is reclaimed), then
    /// the scrub and repair in both.
    ///
    /// # Errors
    ///
    /// Propagates eviction and inventory backend errors.
    pub fn maintain_once(&self) -> anyhow::Result<MaintenanceReport> {
        match &self.store.policy().mode {
            StorageMode::WriteOnly(_) => Ok(MaintenanceReport::Retained {
                scrub: self.scrub_pass()?,
            }),
            StorageMode::Lru(_) => {
                let report = self.store.evict_to_budget(
                    &self.roots,
                    self.oracle.as_ref(),
                    self.replicator.as_ref(),
                )?;
                Ok(MaintenanceReport::Evict {
                    evicted: report.evicted,
                    bytes: report.bytes_evicted,
                    scrub: self.scrub_pass()?,
                })
            }
        }
    }

    /// The retention-managed store (a future protocol face serves blobs from it).
    #[must_use]
    pub fn store(&self) -> &CasStore<Box<dyn ReclaimableCas>> {
        &self.store
    }

    /// The blobs held on this node **only** — no fetch-through, no peer probing. The
    /// peer routes answer from this, so two nodes listing each other cannot recurse.
    #[must_use]
    pub fn local(&self) -> &dyn CasBackend {
        self.local.as_deref().unwrap_or(&self.store)
    }

    /// This node's retention mode as peers see it in `x-nessie-mode`. A peer's
    /// `durable` hint is trusted only when it reports `write_only`.
    #[must_use]
    pub fn mode_name(&self) -> &'static str {
        match self.store.policy().mode {
            StorageMode::WriteOnly(_) => "write_only",
            StorageMode::Lru(_) => "lru",
        }
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
        self.mirror(&digest)?;
        Ok(digest)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        self.put_keyed_local(expected, source)?;
        self.mirror(expected)
    }

    /// `write_only` mirror: the blob must be held here, then `R - 1` peers must hold
    /// it too (re-pushing if they do not). A no-op without a mirror.
    fn ensure_replicated(&self, digest: &Digest) -> Result<(), BackendError> {
        if self.mirror.is_none() {
            return Ok(());
        }
        if !self.store.has(digest)? {
            return Err(BackendError::BlobNotFound(digest.clone()));
        }
        self.mirror(digest)
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
                bytes: 9,
                scrub: ScrubReport::default()
            }
        );
        assert!(!node.has(&d).unwrap());
    }

    #[test]
    fn write_only_node_maintenance_retains_everything() {
        let cfg = CasConfig {
            mode: CasMode::WriteOnly,
            replication_factor: Some(1),
            byte_budget: Some(64),
            ..lru_cfg()
        };
        let node = CasNode::from_config(&cfg, PeerId::new("me")).unwrap();
        let d = node.put(&mut &b"keep-me"[..]).unwrap();
        let report = node.maintain_once().unwrap();
        assert_eq!(report.scrub().scanned, 1);
        assert!(matches!(report, MaintenanceReport::Retained { .. }));
        assert!(node.has(&d).unwrap());
    }

    #[test]
    fn write_only_node_refuses_a_write_past_capacity() {
        let cfg = CasConfig {
            mode: CasMode::WriteOnly,
            replication_factor: Some(1),
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
