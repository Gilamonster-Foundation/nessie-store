//! Cache-mode replica-gated LRU eviction.
//!
//! **PO-GC-2**: cache eviction never loses a reachable blob swarm-wide. A blob is
//! dropped only when it is safe to re-fetch it later — either **≥ R other holders**
//! exist (via [`ContentRouter::providers`](nessie_backend_core::ContentRouter)) or a
//! [`DurabilityOracle`] confirms a permanent home. The **pinned** class (confirmed
//! AC entries + identity + live roots) is never evicted, and every uncertain path is
//! **fail-safe**: a router error, an under-replicated blob, or a refusing replicator
//! all keep the blob. You can only ever fail *safe* (keep it), never fail *open*
//! (drop the last copy).

use crate::policy::StorageMode;
use crate::roots::RootSource;
use crate::store::CasStore;
use nessie_backend_core::{BackendError, Digest, LocalBlob, ReclaimableCas};
use std::num::NonZeroUsize;

/// Whether some **durable** peer (one that never evicts a reachable blob) holds a
/// blob. A permanent home lets a cache node drop even below `R`. The default
/// [`NoDurableOracle`] answers "unknown" (`false`), degrading to the pure `≥ R` path.
pub trait DurabilityOracle: Send + Sync {
    /// `true` iff a durable peer is known to hold `digest`.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if durability cannot be determined (treated fail-safe by the
    /// caller: uncertainty keeps the blob).
    fn durably_held(&self, digest: &Digest) -> Result<bool, BackendError>;
}

/// The default oracle: never asserts durability, so eviction relies purely on the
/// `≥ R` replica count.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDurableOracle;

impl DurabilityOracle for NoDurableOracle {
    fn durably_held(&self, _digest: &Digest) -> Result<bool, BackendError> {
        Ok(false)
    }
}

/// Pushes a blob outward until enough other holders exist, so an under-replicated
/// last copy becomes safely evictable on a later pass. The default
/// [`RefuseToReplicate`] is a no-op — the crucial fail-safe: with no replicator the
/// blob is simply kept, never dropped.
pub trait Replicator: Send + Sync {
    /// Attempt to replicate `digest` to at least `target` other holders.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if replication cannot be initiated (the caller keeps the blob).
    fn replicate(&self, digest: &Digest, target: NonZeroUsize) -> Result<(), BackendError>;
}

/// The default replicator: does nothing. Under-replicated blobs are kept (fail-safe).
#[derive(Debug, Clone, Copy, Default)]
pub struct RefuseToReplicate;

impl Replicator for RefuseToReplicate {
    fn replicate(&self, _digest: &Digest, _target: NonZeroUsize) -> Result<(), BackendError> {
        Ok(())
    }
}

/// What one cache-eviction pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictReport {
    /// Eviction candidates considered (cold, unpinned, unguarded, past grace).
    pub scanned: usize,
    /// Blobs evicted (dropped locally; still fetchable by digest from a peer).
    pub evicted: usize,
    /// Bytes freed.
    pub bytes_evicted: u64,
    /// Candidates kept because dropping them would risk the last reachable copy
    /// (under-replicated, no durable home) — the fail-safe count.
    pub kept_underreplicated: usize,
    /// Whether the store is still above budget after the pass (only pinned or
    /// under-replicated blobs remained).
    pub still_over_budget: bool,
}

impl<C: ReclaimableCas> CasStore<C> {
    /// Evict cold, replicated, non-pinned blobs down toward the cache byte budget.
    ///
    /// # Errors
    ///
    /// [`BackendError::FeatureNotSupported`] in durable mode; other [`BackendError`]s
    /// propagate from enumerating or reclaiming the local store. A router or oracle
    /// error is **not** fatal — it is treated fail-safe (the candidate is kept).
    pub fn evict_to_budget(
        &self,
        roots: &dyn RootSource,
        oracle: &dyn DurabilityOracle,
        replicator: &dyn Replicator,
    ) -> Result<EvictReport, BackendError> {
        let (budget, r, grace) = match &self.policy.mode {
            StorageMode::Cache(p) => (p.byte_budget, p.replication_factor, p.gc_grace),
            StorageMode::Durable(_) => {
                return Err(BackendError::FeatureNotSupported {
                    capability: "cache-eviction",
                });
            }
        };

        let pinned = roots.roots()?.pinned;
        let local = self.inner.iter_local()?;
        let mut used: u64 = local.iter().map(|b| b.size_bytes).sum();
        let mut report = EvictReport::default();
        if used <= budget {
            return Ok(report);
        }

        let now = self.clock.now();
        let (guarded, written) = {
            let state = self.state();
            (state.in_flight.clone(), state.access.written_snapshot())
        };

        // Candidates: not pinned, not in-flight, not fresh. Coldest (lowest tick) first.
        let mut candidates: Vec<LocalBlob> = local
            .into_iter()
            .filter(|b| {
                let past_grace = written
                    .get(&b.digest)
                    .is_none_or(|w| now.saturating_sub(*w) >= grace);
                !pinned.contains(&b.digest) && !guarded.contains(&b.digest) && past_grace
            })
            .collect();
        report.scanned = candidates.len();
        candidates.sort_by_key(|b| self.state().access.last_touch(&b.digest));

        for LocalBlob { digest, size_bytes } in candidates {
            if used <= budget {
                break;
            }
            if self.is_safe_to_evict(&digest, r, oracle)? {
                if self.inner.reclaim(&digest)? {
                    self.state().access.forget(&digest);
                    let _ = self.router.withdraw(&digest); // de-advertise; it floated away
                    used -= size_bytes;
                    report.evicted += 1;
                    report.bytes_evicted += size_bytes;
                }
            } else {
                // Last reachable copy (or unknown replication): keep it, and nudge it
                // outward so a later pass can evict it. Fail-safe either way.
                let _ = replicator.replicate(&digest, r);
                report.kept_underreplicated += 1;
            }
        }
        report.still_over_budget = used > budget;
        Ok(report)
    }

    /// A blob is safe to drop iff at least `r` **other** peers hold it, or a durable
    /// peer does. Any uncertainty (router/oracle error) answers `false` — fail-safe.
    fn is_safe_to_evict(
        &self,
        digest: &Digest,
        r: NonZeroUsize,
        oracle: &dyn DurabilityOracle,
    ) -> Result<bool, BackendError> {
        let others = match self.router.providers(digest) {
            Ok(holders) => holders.iter().filter(|p| *p != &self.me).count(),
            Err(_) => 0, // router error => assume no known replicas (keep the blob)
        };
        if others >= r.get() {
            return Ok(true);
        }
        Ok(oracle.durably_held(digest).unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use crate::policy::RetentionPolicy;
    use crate::roots::RootRegistry;
    use nessie_backend_core::{CasBackend, ContentRouter, PeerId};
    use nessie_backend_mem::{MemCas, MemRouter, MemSwarm};
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::Duration;

    fn r(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn cache_store(
        me: &str,
        router: Arc<dyn ContentRouter>,
        budget: u64,
        rf: usize,
    ) -> CasStore<MemCas> {
        CasStore::new(
            MemCas::new(),
            RetentionPolicy::cache(budget, r(rf), Duration::from_secs(0)),
            router,
            Arc::new(MockClock::new()),
            PeerId::new(me),
        )
    }

    fn put_committed(store: &CasStore<MemCas>, bytes: &[u8]) -> Digest {
        let d = store.put(&mut Cursor::new(bytes.to_vec())).unwrap();
        store.committed(&d);
        d
    }

    #[test]
    fn eviction_is_rejected_in_durable_mode() {
        let store = CasStore::new(
            MemCas::new(),
            RetentionPolicy::durable(Duration::from_secs(0)),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        );
        assert!(matches!(
            store.evict_to_budget(&RootRegistry::new(), &NoDurableOracle, &RefuseToReplicate),
            Err(BackendError::FeatureNotSupported { .. })
        ));
    }

    #[test]
    fn never_drops_pinned_even_at_zero_budget() {
        let store = cache_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 0, 1);
        let pinned = put_committed(&store, b"pinned-body");
        let roots = RootRegistry::new();
        roots.register(pinned.clone()); // pinned class
        let rep = store
            .evict_to_budget(&roots, &NoDurableOracle, &RefuseToReplicate)
            .unwrap();
        assert!(
            store.has(&pinned).unwrap(),
            "a pinned blob is never evicted"
        );
        assert_eq!(rep.evicted, 0);
    }

    #[test]
    fn never_drops_the_last_copy() {
        // Only self holds the blob (MemRouter, single node), R=1 => 0 others < 1.
        let store = cache_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 0, 1);
        let lonely = put_committed(&store, b"only-copy");
        let rep = store
            .evict_to_budget(&RootRegistry::new(), &NoDurableOracle, &RefuseToReplicate)
            .unwrap();
        assert!(
            store.has(&lonely).unwrap(),
            "the last copy is never dropped"
        );
        assert_eq!(rep.evicted, 0);
        assert_eq!(rep.kept_underreplicated, 1);
        assert!(rep.still_over_budget);
    }

    #[test]
    fn evicts_when_sufficiently_replicated() {
        let swarm = MemSwarm::new();
        let store = cache_store("me", Arc::new(swarm.node(PeerId::new("me"))), 0, 1);
        let d = put_committed(&store, b"replicated");
        // A peer also announces it: now 1 other holder >= R=1.
        swarm.node(PeerId::new("peer")).announce(&d).unwrap();
        let rep = store
            .evict_to_budget(&RootRegistry::new(), &NoDurableOracle, &RefuseToReplicate)
            .unwrap();
        assert!(
            !store.has(&d).unwrap(),
            "a replicated blob floats to the swarm"
        );
        assert_eq!(rep.evicted, 1);
    }

    #[test]
    fn a_durable_oracle_permits_eviction_below_r() {
        struct AlwaysDurable;
        impl DurabilityOracle for AlwaysDurable {
            fn durably_held(&self, _d: &Digest) -> Result<bool, BackendError> {
                Ok(true)
            }
        }
        // Single node, R=2, but the oracle says a durable node holds it.
        let store = cache_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 0, 2);
        let d = put_committed(&store, b"durably-backed");
        let rep = store
            .evict_to_budget(&RootRegistry::new(), &AlwaysDurable, &RefuseToReplicate)
            .unwrap();
        assert!(!store.has(&d).unwrap());
        assert_eq!(rep.evicted, 1);
    }

    #[test]
    fn evicts_coldest_first_and_respects_budget() {
        // Three ~equal blobs, budget fits two; the coldest (oldest touch) goes.
        let swarm = MemSwarm::new();
        let store = cache_store("me", Arc::new(swarm.node(PeerId::new("me"))), 24, 1);
        let a = put_committed(&store, b"aaaaaaaaaa"); // 10 bytes each
        let b = put_committed(&store, b"bbbbbbbbbb");
        let c = put_committed(&store, b"cccccccccc");
        // Replicate all so they are eligible.
        for d in [&a, &b, &c] {
            swarm.node(PeerId::new("peer")).announce(d).unwrap();
        }
        // Rewarm a and b (read) so c stays coldest.
        let _ = store.get(&a).unwrap();
        let _ = store.get(&b).unwrap();
        let rep = store
            .evict_to_budget(&RootRegistry::new(), &NoDurableOracle, &RefuseToReplicate)
            .unwrap();
        assert!(!store.has(&c).unwrap(), "coldest evicted first");
        assert!(store.has(&a).unwrap() && store.has(&b).unwrap());
        assert!(!rep.still_over_budget);
    }

    #[test]
    fn paired_safety_two_node_restore() {
        // nuc1 durable holds the blob; nuc2 cache evicts it and re-fetches by digest.
        let swarm = MemSwarm::new();
        let durable = CasStore::new(
            MemCas::new(),
            RetentionPolicy::durable(Duration::from_secs(0)),
            Arc::new(swarm.node(PeerId::new("nuc1"))),
            Arc::new(MockClock::new()),
            PeerId::new("nuc1"),
        );
        let cache = cache_store("nuc2", Arc::new(swarm.node(PeerId::new("nuc2"))), 0, 1);

        let bytes = b"shared-blob".to_vec();
        let d = durable.put(&mut Cursor::new(bytes.clone())).unwrap();
        durable.committed(&d);
        let d2 = cache.put(&mut Cursor::new(bytes.clone())).unwrap();
        cache.committed(&d2);
        assert_eq!(d, d2);

        // nuc2 evicts (nuc1 is a provider => >= R=1 other holder).
        cache
            .evict_to_budget(&RootRegistry::new(), &NoDurableOracle, &RefuseToReplicate)
            .unwrap();
        assert!(!cache.has(&d).unwrap(), "nuc2 dropped its copy");
        assert!(durable.has(&d).unwrap(), "nuc1 still has it");
        // The swarm still knows nuc1 holds it — a real client re-fetches from there.
        assert!(
            swarm
                .node(PeerId::new("obs"))
                .providers(&d)
                .unwrap()
                .contains(&PeerId::new("nuc1"))
        );
    }
}
