//! Durable-mode mark-and-sweep garbage collection.
//!
//! **PO-GC-1**: `gc()` reclaims a local blob only if it is *not* in the reachable
//! closure of the roots — `swept ∩ reachable = ∅`. Two race guards keep a
//! just-written body from being swept before the op that references it lands: the
//! in-process **write-guard** (`in_flight`, airtight within a process) and the
//! **`gc_grace`** window (the cross-restart backstop). Cache mode rejects `gc()` —
//! running mark-sweep on a node that may have evicted a root's body is unsafe.

use crate::policy::StorageMode;
use crate::reach::reachable_closure;
use crate::resolver::ReferenceResolver;
use crate::roots::RootSource;
use crate::store::CasStore;
use nessie_backend_core::{BackendError, LocalBlob, ReclaimableCas};

/// What one durable GC pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Local blobs examined.
    pub scanned: usize,
    /// Size of the reachable closure (the "mark" set).
    pub reachable: usize,
    /// Blobs reclaimed (swept).
    pub reclaimed: usize,
    /// Bytes freed by the sweep.
    pub bytes_reclaimed: u64,
    /// Unreachable blobs kept this pass by a race guard (in-flight or within grace).
    pub kept_by_grace: usize,
}

impl<C: ReclaimableCas> CasStore<C> {
    /// Run durable mark-and-sweep: reclaim every local blob that is neither reachable
    /// from `roots` nor protected by a race guard.
    ///
    /// # Errors
    ///
    /// [`BackendError::FeatureNotSupported`] in cache mode; other [`BackendError`]s
    /// propagate from enumerating, reading, or reclaiming the local store.
    pub fn gc(
        &self,
        roots: &dyn RootSource,
        resolver: &dyn ReferenceResolver,
    ) -> Result<GcReport, BackendError> {
        let grace = match &self.policy.mode {
            StorageMode::Durable(p) => p.gc_grace,
            StorageMode::Cache(_) => {
                return Err(BackendError::FeatureNotSupported {
                    capability: "durable-gc",
                });
            }
        };

        // Snapshot the sweep candidates FIRST, then read roots: a blob written after
        // this snapshot is simply not a candidate this pass.
        let local = self.inner.iter_local()?;
        let root_set = roots.roots()?;
        let live = reachable_closure(&root_set.seed, &self.inner, resolver)?; // MARK
        let now = self.clock.now();
        let (guarded, written) = {
            let state = self.state();
            (state.in_flight.clone(), state.access.written_snapshot())
        };

        let mut report = GcReport {
            scanned: local.len(),
            reachable: live.len(),
            ..Default::default()
        };

        for LocalBlob { digest, size_bytes } in local {
            if live.contains(&digest) {
                continue; // reachable — the ONLY thing that keeps a blob (PO-GC-1)
            }
            let within_grace = written
                .get(&digest)
                .is_some_and(|w| now.saturating_sub(*w) < grace);
            if guarded.contains(&digest) || within_grace {
                report.kept_by_grace += 1; // race guard — unreachable but too young
                continue;
            }
            // SWEEP: unreachable, unguarded, past grace.
            if self.inner.reclaim(&digest)? {
                report.reclaimed += 1;
                report.bytes_reclaimed += size_bytes;
                self.state().access.forget(&digest);
                let _ = self.router.withdraw(&digest); // best-effort de-advertise
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use crate::policy::RetentionPolicy;
    use crate::resolver::CanonicalResolver;
    use crate::roots::{RootRegistry, RootSource};
    use nessie_backend_core::{
        ActionResult, BackendError, CasBackend, Digest, LocalBlob, OutputFile, PeerId,
        ReclaimableCas,
    };
    use nessie_backend_mem::{MemCas, MemRouter};
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        store: CasStore<MemCas>,
        clock: Arc<MockClock>,
        roots: RootRegistry,
    }

    fn fixture(grace_secs: u64) -> Fixture {
        let clock = Arc::new(MockClock::new());
        let store = CasStore::new(
            MemCas::new(),
            RetentionPolicy::durable(Duration::from_secs(grace_secs)),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            clock.clone(),
            PeerId::new("me"),
        )
        .expect("store");
        Fixture {
            store,
            clock,
            roots: RootRegistry::new(),
        }
    }

    fn put(store: &CasStore<MemCas>, bytes: &[u8]) -> Digest {
        store.put(&mut Cursor::new(bytes.to_vec())).unwrap()
    }

    fn action_result_over(leaf: &Digest) -> ActionResult {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            OutputFile {
                digest: leaf.clone(),
                is_executable: false,
            },
        );
        ActionResult {
            outputs,
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
        }
    }

    #[test]
    fn gc_reclaims_unreachable_but_keeps_reachable() {
        let f = fixture(0); // no grace
        // A confirmed body referencing a leaf; both committed (guard released).
        let leaf = put(&f.store, b"leaf");
        let body = put(&f.store, &action_result_over(&leaf).to_canonical_bytes());
        let orphan = put(&f.store, b"orphan-garbage");
        f.store.committed(&leaf);
        f.store.committed(&body);
        f.store.committed(&orphan);
        // Register the body as a root (a confirmed AC entry).
        f.roots.register(body.clone());

        let report = f.store.gc(&f.roots, &CanonicalResolver).unwrap();

        // Reachable (body + leaf) survive; the orphan is swept — swept ∩ reachable = ∅.
        assert!(f.store.has(&body).unwrap() && f.store.has(&leaf).unwrap());
        assert!(
            !f.store.has(&orphan).unwrap(),
            "unreachable orphan reclaimed"
        );
        assert_eq!(report.reclaimed, 1);
        assert_eq!(report.reachable, 2);
    }

    #[test]
    fn gc_never_touches_reachable_even_at_zero_grace() {
        let f = fixture(0);
        let leaf = put(&f.store, b"leaf");
        let body = put(&f.store, &action_result_over(&leaf).to_canonical_bytes());
        f.store.committed(&leaf);
        f.store.committed(&body);
        f.roots.register(body.clone());
        let reach =
            crate::reachable_closure(&f.roots.roots().unwrap().seed, &f.store, &CanonicalResolver)
                .unwrap();
        f.store.gc(&f.roots, &CanonicalResolver).unwrap();
        for d in reach {
            assert!(f.store.has(&d).unwrap(), "reachable {d} must survive GC");
        }
    }

    #[test]
    fn in_flight_and_grace_protect_the_put_race() {
        // (a) an uncommitted (in-flight) unreachable blob is NOT swept.
        let f = fixture(0);
        let pending = put(&f.store, b"pending"); // never committed => in_flight
        let r = f.store.gc(&f.roots, &CanonicalResolver).unwrap();
        assert!(f.store.has(&pending).unwrap(), "in-flight blob survives");
        assert_eq!(r.kept_by_grace, 1);

        // (b) a committed unreachable blob younger than grace is kept, then swept
        // once the clock passes grace.
        let g = fixture(100);
        let garbage = put(&g.store, b"garbage");
        g.store.committed(&garbage);
        g.store.gc(&g.roots, &CanonicalResolver).unwrap();
        assert!(g.store.has(&garbage).unwrap(), "within grace: kept");
        g.clock.advance(Duration::from_secs(101));
        g.store.gc(&g.roots, &CanonicalResolver).unwrap();
        assert!(!g.store.has(&garbage).unwrap(), "past grace: swept");
    }

    #[test]
    fn restart_does_not_sweep_young_blobs() {
        // #126: write-times are in memory, so a rebuilt store must re-seed them from
        // the backend or the first post-restart pass sweeps blobs written seconds ago.
        let clock = Arc::new(MockClock::new());
        let first = CasStore::new(
            MemCas::new(),
            RetentionPolicy::durable(Duration::from_secs(100)),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            clock.clone(),
            PeerId::new("me"),
        )
        .expect("store");
        let garbage = put(&first, b"garbage");
        first.committed(&garbage);
        let CasStore { inner, .. } = first; // "restart": same backend, empty AccessLog
        let store = CasStore::new(
            inner,
            RetentionPolicy::durable(Duration::from_secs(100)),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            clock.clone(),
            PeerId::new("me"),
        )
        .expect("store");
        let roots = RootRegistry::new();

        store.gc(&roots, &CanonicalResolver).unwrap();
        assert!(
            store.has(&garbage).unwrap(),
            "within grace after restart: kept"
        );
        clock.advance(Duration::from_secs(101));
        store.gc(&roots, &CanonicalResolver).unwrap();
        assert!(!store.has(&garbage).unwrap(), "past grace: swept");
    }

    /// A backend whose first `iter_local` fails and later ones succeed (a transient
    /// startup fault), sharing its blobs across clones like a durable store would.
    #[derive(Clone)]
    struct FlakyOnce {
        cas: Arc<MemCas>,
        failed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CasBackend for FlakyOnce {
        fn has(&self, d: &Digest) -> Result<bool, BackendError> {
            self.cas.has(d)
        }
        fn get(&self, d: &Digest) -> Result<Box<dyn std::io::Read + Send>, BackendError> {
            self.cas.get(d)
        }
        fn put(&self, src: &mut dyn std::io::Read) -> Result<Digest, BackendError> {
            self.cas.put(src)
        }
    }

    impl ReclaimableCas for FlakyOnce {
        fn iter_local(&self) -> Result<Vec<LocalBlob>, BackendError> {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(BackendError::Internal("transient enumeration fault".into()));
            }
            self.cas.iter_local()
        }
        fn reclaim(&self, d: &Digest) -> Result<bool, BackendError> {
            self.cas.reclaim(d)
        }
    }

    #[test]
    fn failed_startup_enumeration_cannot_yield_a_store_that_sweeps_young_blobs() {
        // #126 review P1: swallowing the seed error left a store with no write-times;
        // once the backend recovered, GC saw unknown (=old) blobs and swept them.
        let backend = FlakyOnce {
            cas: Arc::new(MemCas::new()),
            failed: Arc::default(),
        };
        let young = backend
            .cas
            .put(&mut Cursor::new(b"young".to_vec()))
            .unwrap();
        let clock = Arc::new(MockClock::new());
        let build = || {
            CasStore::new(
                backend.clone(),
                RetentionPolicy::durable(Duration::from_secs(100)),
                Arc::new(MemRouter::new(PeerId::new("me"))),
                clock.clone(),
                PeerId::new("me"),
            )
        };

        assert!(build().is_err(), "fail closed: no store without a seed");
        let store = build().expect("second enumeration succeeds");
        store.gc(&RootRegistry::new(), &CanonicalResolver).unwrap();
        assert!(
            store.has(&young).unwrap(),
            "young blob survives the first GC"
        );
    }

    #[test]
    fn gc_is_idempotent() {
        let f = fixture(0);
        let orphan = put(&f.store, b"orphan");
        f.store.committed(&orphan);
        let first = f.store.gc(&f.roots, &CanonicalResolver).unwrap();
        let second = f.store.gc(&f.roots, &CanonicalResolver).unwrap();
        assert_eq!(first.reclaimed, 1);
        assert_eq!(
            second.reclaimed, 0,
            "nothing left to sweep on the second pass"
        );
    }

    #[test]
    fn gc_is_rejected_in_cache_mode() {
        let store = CasStore::new(
            MemCas::new(),
            RetentionPolicy::cache(
                1024,
                std::num::NonZeroUsize::new(2).unwrap(),
                Duration::from_secs(60),
            ),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        )
        .expect("store");
        let roots = RootRegistry::new();
        assert!(matches!(
            store.gc(&roots, &CanonicalResolver),
            Err(BackendError::FeatureNotSupported { .. })
        ));
    }
}
