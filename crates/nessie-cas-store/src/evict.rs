//! LRU eviction: over `byte_budget`, drop the coldest blobs, in two passes.
//!
//! Pass 1 drops blobs with at least one **other known holder** (a router provider
//! other than this node, or a [`DurabilityOracle`] hit); pass 2 drops **sole
//! copies**. This is a cache: a sole copy can go, by design. The **pinned** class
//! (confirmed AC entries + identity + live roots), in-flight writes and writes
//! younger than `gc_grace` are never evicted. Write-only mode never reaches this
//! code: [`CasStore::evict_to_budget`] refuses it.

use crate::policy::StorageMode;
use crate::roots::RootSource;
use crate::store::CasStore;
use nessie_backend_core::{BackendError, Digest, LocalBlob, ReclaimableCas};
use std::num::NonZeroUsize;

/// Whether some other node is known to hold a blob beyond the router's providers.
/// The default [`NoDurableOracle`] answers "unknown" (`false`).
pub trait DurabilityOracle: Send + Sync {
    /// `true` iff another holder is known to hold `digest`.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if holders cannot be determined (the caller treats the
    /// blob as a sole copy).
    fn durably_held(&self, digest: &Digest) -> Result<bool, BackendError>;
}

/// The default oracle: never asserts another holder.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDurableOracle;

impl DurabilityOracle for NoDurableOracle {
    fn durably_held(&self, _digest: &Digest) -> Result<bool, BackendError> {
        Ok(false)
    }
}

/// Pushes a blob outward before a sole copy is dropped. The default
/// [`RefuseToReplicate`] is a no-op.
pub trait Replicator: Send + Sync {
    /// Attempt to replicate `digest` to at least `target` other holders.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if replication cannot be initiated (ignored: best-effort).
    fn replicate(&self, digest: &Digest, target: NonZeroUsize) -> Result<(), BackendError>;
}

/// The default replicator: does nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefuseToReplicate;

impl Replicator for RefuseToReplicate {
    fn replicate(&self, _digest: &Digest, _target: NonZeroUsize) -> Result<(), BackendError> {
        Ok(())
    }
}

/// What one eviction pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictReport {
    /// Eviction candidates considered (unpinned, unguarded, past grace).
    pub scanned: usize,
    /// Blobs evicted.
    pub evicted: usize,
    /// Of those, sole copies (no other known holder) — dropped by design.
    pub evicted_sole: usize,
    /// Bytes freed.
    pub bytes_evicted: u64,
    /// Whether the store is still above budget after the pass (only pinned,
    /// in-flight or fresh blobs remained).
    pub still_over_budget: bool,
}

impl<C: ReclaimableCas> CasStore<C> {
    /// Evict the coldest blobs down to the LRU byte budget: replicated blobs first,
    /// then sole copies.
    ///
    /// # Errors
    ///
    /// [`BackendError::FeatureNotSupported`] in write-only mode; other
    /// [`BackendError`]s propagate from enumerating or reclaiming the local store. A
    /// router or oracle error is not fatal: the blob is treated as a sole copy.
    pub fn evict_to_budget(
        &self,
        roots: &dyn RootSource,
        oracle: &dyn DurabilityOracle,
        replicator: &dyn Replicator,
    ) -> Result<EvictReport, BackendError> {
        let (budget, grace) = match &self.policy.mode {
            StorageMode::Lru(p) => (p.byte_budget, p.gc_grace),
            StorageMode::WriteOnly(_) => {
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

        // Stable partition keeps coldest-first within each pass.
        let (replicated, sole): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|b| self.has_other_holder(&b.digest, oracle));

        for (blobs, is_sole) in [(replicated, false), (sole, true)] {
            for LocalBlob { digest, size_bytes } in blobs {
                if used <= budget {
                    break;
                }
                if is_sole {
                    let _ = replicator.replicate(&digest, NonZeroUsize::MIN); // best-effort
                }
                if self.inner.reclaim(&digest)? {
                    self.state().access.forget(&digest);
                    let _ = self.router.withdraw(&digest); // de-advertise
                    used -= size_bytes;
                    report.evicted += 1;
                    report.evicted_sole += usize::from(is_sole);
                    report.bytes_evicted += size_bytes;
                }
            }
        }
        report.still_over_budget = used > budget;
        Ok(report)
    }

    /// Another holder is known iff a router provider other than this node exists or
    /// the oracle says so. Any error answers `false` (treated as a sole copy).
    fn has_other_holder(&self, digest: &Digest, oracle: &dyn DurabilityOracle) -> bool {
        let others = self
            .router
            .providers(digest)
            .map_or(0, |h| h.iter().filter(|p| *p != &self.me).count());
        others >= 1 || oracle.durably_held(digest).unwrap_or(false)
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

    fn lru_store(me: &str, router: Arc<dyn ContentRouter>, budget: u64) -> CasStore<MemCas> {
        CasStore::new(
            MemCas::new(),
            RetentionPolicy::lru(budget, Duration::from_secs(0)),
            router,
            Arc::new(MockClock::new()),
            PeerId::new(me),
        )
        .expect("store")
    }

    fn put_committed(store: &CasStore<MemCas>, bytes: &[u8]) -> Digest {
        let d = store.put(&mut Cursor::new(bytes.to_vec())).unwrap();
        store.committed(&d);
        d
    }

    fn evict(store: &CasStore<MemCas>, roots: &RootRegistry) -> EvictReport {
        store
            .evict_to_budget(roots, &NoDurableOracle, &RefuseToReplicate)
            .unwrap()
    }

    #[test]
    fn eviction_is_rejected_in_write_only_mode() {
        let store = CasStore::new(
            MemCas::new(),
            RetentionPolicy::write_only(1024),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        )
        .expect("store");
        assert!(matches!(
            store.evict_to_budget(&RootRegistry::new(), &NoDurableOracle, &RefuseToReplicate),
            Err(BackendError::FeatureNotSupported { .. })
        ));
    }

    #[test]
    fn never_drops_pinned_even_at_zero_budget() {
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 0);
        let pinned = put_committed(&store, b"pinned-body");
        let roots = RootRegistry::new();
        roots.register(pinned.clone());
        let rep = evict(&store, &roots);
        assert!(
            store.has(&pinned).unwrap(),
            "a pinned blob is never evicted"
        );
        assert_eq!(rep.evicted, 0);
        assert!(rep.still_over_budget);
    }

    #[test]
    fn never_drops_an_in_flight_write() {
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 0);
        let d = store
            .put(&mut Cursor::new(b"uncommitted".to_vec()))
            .unwrap();
        let rep = evict(&store, &RootRegistry::new());
        assert!(
            store.has(&d).unwrap(),
            "an in-flight write is never evicted"
        );
        assert_eq!(rep.evicted, 0);
        store.committed(&d);
        assert_eq!(evict(&store, &RootRegistry::new()).evicted, 1);
    }

    #[test]
    fn a_sole_copy_is_evicted_over_budget_with_no_peers() {
        // Single node, no peers: the cache drops its coldest sole copy, by design.
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 10);
        let old = put_committed(&store, b"aaaaaaaaaa");
        let new = put_committed(&store, b"bbbbbbbbbb");
        let rep = evict(&store, &RootRegistry::new());
        assert!(!store.has(&old).unwrap(), "coldest sole copy goes");
        assert!(store.has(&new).unwrap());
        assert_eq!((rep.evicted, rep.evicted_sole), (1, 1));
        assert!(!rep.still_over_budget);
    }

    #[test]
    fn replicated_blobs_go_before_colder_sole_copies() {
        let swarm = MemSwarm::new();
        let store = lru_store("me", Arc::new(swarm.node(PeerId::new("me"))), 10);
        let cold_sole = put_committed(&store, b"aaaaaaaaaa"); // coldest, no other holder
        let warm_repl = put_committed(&store, b"bbbbbbbbbb");
        swarm
            .node(PeerId::new("peer"))
            .announce(&warm_repl)
            .unwrap();
        let rep = evict(&store, &RootRegistry::new());
        assert!(
            !store.has(&warm_repl).unwrap(),
            "replicated blob evicted first"
        );
        assert!(
            store.has(&cold_sole).unwrap(),
            "sole copy survives while it can"
        );
        assert_eq!((rep.evicted, rep.evicted_sole), (1, 0));
    }

    #[test]
    fn an_oracle_holder_counts_as_another_holder() {
        struct Holds;
        impl DurabilityOracle for Holds {
            fn durably_held(&self, _d: &Digest) -> Result<bool, BackendError> {
                Ok(true)
            }
        }
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 0);
        put_committed(&store, b"held-elsewhere");
        let rep = store
            .evict_to_budget(&RootRegistry::new(), &Holds, &RefuseToReplicate)
            .unwrap();
        assert_eq!((rep.evicted, rep.evicted_sole), (1, 0));
    }

    #[test]
    fn evicts_coldest_first_and_respects_budget() {
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))), 24);
        let a = put_committed(&store, b"aaaaaaaaaa"); // 10 bytes each
        let b = put_committed(&store, b"bbbbbbbbbb");
        let c = put_committed(&store, b"cccccccccc");
        let _ = store.get(&a).unwrap(); // rewarm a and b so c is coldest
        let _ = store.get(&b).unwrap();
        let rep = evict(&store, &RootRegistry::new());
        assert!(!store.has(&c).unwrap(), "coldest evicted first");
        assert!(store.has(&a).unwrap() && store.has(&b).unwrap());
        assert!(!rep.still_over_budget);
    }

    #[test]
    fn fresh_writes_inside_grace_survive() {
        let clock = Arc::new(MockClock::new());
        let store = CasStore::new(
            MemCas::new(),
            RetentionPolicy::lru(0, Duration::from_secs(60)),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            clock.clone(),
            PeerId::new("me"),
        )
        .expect("store");
        let d = put_committed(&store, b"fresh");
        assert_eq!(evict(&store, &RootRegistry::new()).evicted, 0);
        clock.advance(Duration::from_secs(61));
        assert_eq!(evict(&store, &RootRegistry::new()).evicted, 1);
        assert!(!store.has(&d).unwrap());
    }
}
