//! The [`CasStore`] wrapper — a managed [`CasBackend`] that adds the state
//! retention needs: an in-flight write-guard, per-blob write-time (for the grace
//! window) and LRU recency. Eviction lives in `evict.rs`; the write-only capacity
//! gate lives here, on the write path.

use crate::clock::Clock;
use crate::policy::{RetentionPolicy, StorageMode};
use nessie_backend_core::{
    BackendError, CasBackend, ContentRouter, Digest, PeerId, ReclaimableCas,
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Per-blob write-time (durable/cache grace window) and recency (cache LRU order).
#[derive(Default)]
pub(crate) struct AccessLog {
    written_at: HashMap<Digest, Duration>,
    last_touch: HashMap<Digest, u64>,
    next_tick: u64,
}

impl AccessLog {
    /// Mark `digest` most-recently-used (a monotonic tick, so ordering is total).
    pub(crate) fn touch(&mut self, digest: &Digest) {
        let tick = self.next_tick;
        self.next_tick += 1;
        self.last_touch.insert(digest.clone(), tick);
    }

    /// Record a write at `now`: sets the grace-window start and marks it MRU.
    fn record_write(&mut self, digest: &Digest, now: Duration) {
        self.written_at.insert(digest.clone(), now);
        self.touch(digest);
    }

    /// Seed `digest`'s write-time without touching recency (restart re-seeding).
    fn seed_written(&mut self, digest: &Digest, now: Duration) {
        self.written_at.insert(digest.clone(), now);
    }

    /// A snapshot of every known write-time, for a GC pass's grace check.
    pub(crate) fn written_snapshot(&self) -> HashMap<Digest, Duration> {
        self.written_at.clone()
    }

    /// The recency tick of `digest` (lower = colder; 0 = never touched = coldest).
    pub(crate) fn last_touch(&self, digest: &Digest) -> u64 {
        self.last_touch.get(digest).copied().unwrap_or(0)
    }

    /// Whether `digest` has no recorded write yet (first sight of this blob).
    pub(crate) fn is_new(&self, digest: &Digest) -> bool {
        !self.written_at.contains_key(digest)
    }

    /// Forget a blob's bookkeeping (called after it is reclaimed/evicted).
    pub(crate) fn forget(&mut self, digest: &Digest) {
        self.written_at.remove(digest);
        self.last_touch.remove(digest);
    }
}

#[derive(Default)]
pub(crate) struct StoreState {
    /// Blobs written but not yet committed/abandoned — guarded from GC so a just-put
    /// body cannot be swept before the op that will reference it lands. This is the
    /// PRIMARY, in-process race defense; `gc_grace` is the cross-restart backstop.
    pub(crate) in_flight: HashSet<Digest>,
    pub(crate) access: AccessLog,
}

/// A retention-managed [`CasBackend`]: delegates the CAS contract to an inner
/// [`ReclaimableCas`] while tracking the state GC needs. It announces each put to
/// the [`ContentRouter`] (best-effort) and guards it until
/// [`committed`](CasStore::committed) or [`abandon`](CasStore::abandon).
pub struct CasStore<C: ReclaimableCas> {
    pub(crate) inner: C,
    pub(crate) policy: RetentionPolicy,
    pub(crate) router: Arc<dyn ContentRouter>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) me: PeerId,
    pub(crate) state: Mutex<StoreState>,
    /// Write-only mode: bytes durably held.
    used: AtomicU64,
    /// Write-only mode: bytes of writes streaming in right now, not yet committed.
    pending: AtomicU64,
}

/// Counts bytes as they stream in and fails the read once `used + pending` would
/// pass `capacity`, so the backend aborts and discards its temp file: a refused
/// write commits nothing.
struct CapacityGate<'a> {
    src: &'a mut dyn Read,
    used: u64,
    capacity: u64,
    pending: &'a AtomicU64,
    mine: u64,
    tripped: bool,
}

impl Read for CapacityGate<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.src.read(buf)?;
        let n64 = n as u64;
        let before = self.pending.fetch_add(n64, Ordering::SeqCst);
        self.mine += n64;
        if self.used.saturating_add(before).saturating_add(n64) > self.capacity {
            self.tripped = true;
            return Err(io::Error::other("write-only capacity exceeded"));
        }
        Ok(n)
    }
}

impl Drop for CapacityGate<'_> {
    fn drop(&mut self) {
        self.pending.fetch_sub(self.mine, Ordering::SeqCst);
    }
}

impl<C: ReclaimableCas> CasStore<C> {
    /// Wrap `inner` with retention `policy`, advertising to `router` as node `me`
    /// and timing grace windows with `clock`.
    ///
    /// # Errors
    ///
    /// Any [`BackendError`] from enumerating the backend's existing blobs. This
    /// fails closed: a store that cannot seed write-times from `iter_local()` must
    /// not exist, because GC treats an unknown write-time as outside the grace
    /// window and would sweep young blobs the moment the backend recovered.
    pub fn new(
        inner: C,
        policy: RetentionPolicy,
        router: Arc<dyn ContentRouter>,
        clock: Arc<dyn Clock>,
        me: PeerId,
    ) -> Result<Self, BackendError> {
        // The AccessLog is in memory, so after a restart every surviving blob has an
        // unknown write-time, which GC treats as outside the grace window. Seed them
        // all as "written now": the first sweep is delayed by one grace window.
        let mut state = StoreState::default();
        let now = clock.now();
        let mut used = 0;
        for blob in inner.iter_local()? {
            state.access.seed_written(&blob.digest, now);
            used += blob.size_bytes;
        }
        Ok(Self {
            inner,
            policy,
            router,
            clock,
            me,
            used: AtomicU64::new(used),
            pending: AtomicU64::new(0),
            state: Mutex::new(state),
        })
    }

    /// This store's retention policy.
    #[must_use]
    pub fn policy(&self) -> &RetentionPolicy {
        &self.policy
    }

    /// This node's routing identity (the one it announces under).
    #[must_use]
    pub fn me(&self) -> &PeerId {
        &self.me
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().expect("cas store mutex poisoned")
    }

    /// Run a backend write through the write-only capacity gate (a plain call in lru
    /// mode). A refused write returns [`BackendError::CapacityExceeded`] with nothing
    /// committed. A keyed write of a blob already held is a no-op, never refused.
    fn gated<T>(
        &self,
        known: Option<&Digest>,
        source: &mut dyn Read,
        write: impl FnOnce(&mut dyn Read) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let StorageMode::WriteOnly(p) = &self.policy.mode else {
            return write(source);
        };
        if known.is_some_and(|d| self.inner.has(d).unwrap_or(false)) {
            return write(source); // already held: adds no bytes, so never refused
        }
        let mut gate = CapacityGate {
            src: source,
            used: self.used.load(Ordering::SeqCst),
            capacity: p.capacity,
            pending: &self.pending,
            mine: 0,
            tripped: false,
        };
        let out = write(&mut gate);
        if gate.tripped {
            return Err(BackendError::CapacityExceeded {
                used: gate.used,
                incoming: gate.mine,
                capacity: gate.capacity,
            });
        }
        out
    }

    /// Book-keeping after a successful write: write-guard, recency, usage, advert.
    fn after_write(&self, digest: &Digest) {
        {
            let mut state = self.state();
            let fresh = state.access.is_new(digest);
            state.in_flight.insert(digest.clone()); // guard until committed/abandoned
            state.access.record_write(digest, self.clock.now());
            if fresh && let Ok(Some(n)) = self.inner.size(digest) {
                self.used.fetch_add(n, Ordering::SeqCst);
            }
        }
        // Best-effort advertise: the data plane never depends on the router, so a
        // routing failure must not fail a successful local store.
        let _ = self.router.announce(digest);
    }

    fn release_guard(&self, digest: &Digest) {
        self.state().in_flight.remove(digest);
    }

    /// Mark `digest` committed: it is now referenced by a registered root, so its
    /// in-flight write-guard is released and GC treats it by reachability alone.
    pub fn committed(&self, digest: &Digest) {
        self.release_guard(digest);
    }

    /// Abandon `digest`: it will not be referenced. The write-guard is released, so a
    /// later GC pass may reclaim it (it is unreachable).
    pub fn abandon(&self, digest: &Digest) {
        self.release_guard(digest);
    }
}

impl<C: ReclaimableCas> CasBackend for CasStore<C> {
    fn has(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.inner.has(digest)
    }

    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        let reader = self.inner.get(digest)?;
        self.state().access.touch(digest); // a read rewarms the blob (LRU)
        Ok(reader)
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        let digest = self.gated(None, source, |s| self.inner.put(s))?;
        self.after_write(&digest);
        Ok(digest)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        self.gated(Some(expected), source, |s| {
            self.inner.put_keyed(expected, s)
        })?;
        self.after_write(expected);
        Ok(())
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, BackendError> {
        self.inner.size(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use nessie_backend_mem::{MemCas, MemRouter, MemSwarm};
    use std::io::Cursor;

    fn lru_store(me: &str, router: Arc<dyn ContentRouter>) -> CasStore<MemCas> {
        CasStore::new(
            MemCas::new(),
            RetentionPolicy::lru(u64::MAX, Duration::from_secs(60)),
            router,
            Arc::new(MockClock::new()),
            PeerId::new(me),
        )
        .expect("store")
    }

    #[test]
    fn cas_store_is_a_transparent_cas() {
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))));
        nessie_cas_conformance::run_all(&store);
    }

    #[test]
    fn put_announces_to_the_swarm() {
        let swarm = MemSwarm::new();
        let me = PeerId::new("node-me");
        let store = lru_store("node-me", Arc::new(swarm.node(me.clone())));
        let d = store.put(&mut Cursor::new(b"hello".to_vec())).unwrap();
        let observer = swarm.node(PeerId::new("observer"));
        assert_eq!(observer.providers(&d).unwrap(), vec![me]);
    }

    #[test]
    fn put_guards_in_flight_until_committed() {
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))));
        let d = store.put(&mut Cursor::new(b"body".to_vec())).unwrap();
        assert!(
            store.state().in_flight.contains(&d),
            "a just-put blob is guarded"
        );
        store.committed(&d);
        assert!(
            !store.state().in_flight.contains(&d),
            "committed releases the guard"
        );
    }

    #[test]
    fn abandon_also_releases_the_guard() {
        let store = lru_store("me", Arc::new(MemRouter::new(PeerId::new("me"))));
        let d = store.put(&mut Cursor::new(b"body".to_vec())).unwrap();
        store.abandon(&d);
        assert!(!store.state().in_flight.contains(&d));
    }

    fn write_only(capacity: u64) -> CasStore<MemCas> {
        CasStore::new(
            MemCas::new(),
            RetentionPolicy::write_only(capacity),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        )
        .expect("store")
    }

    fn cap_err(r: Result<impl std::fmt::Debug, BackendError>) -> bool {
        matches!(r, Err(BackendError::CapacityExceeded { .. }))
    }

    #[test]
    fn write_only_refuses_a_put_past_capacity_and_stores_nothing() {
        let store = write_only(10);
        let ok = store.put(&mut Cursor::new(b"0123456789".to_vec())).unwrap();
        let over = Digest::compute(b"x");
        assert!(cap_err(store.put(&mut Cursor::new(b"x".to_vec()))));
        assert!(
            !store.has(&over).unwrap(),
            "a refused write commits nothing"
        );
        assert!(store.has(&ok).unwrap());
        assert_eq!(store.inner.iter_local().unwrap().len(), 1);
    }

    #[test]
    fn write_only_refuses_a_put_keyed_past_capacity() {
        let store = write_only(4);
        let d = Digest::compute(b"too-big");
        assert!(cap_err(
            store.put_keyed(&d, &mut Cursor::new(b"too-big".to_vec()))
        ));
        assert!(!store.has(&d).unwrap());
    }

    #[test]
    fn write_only_accepts_exactly_capacity_and_rewrites_are_free() {
        let store = write_only(5);
        let d = store.put(&mut Cursor::new(b"12345".to_vec())).unwrap();
        // The same content again adds no bytes under put_keyed.
        store
            .put_keyed(&d, &mut Cursor::new(b"12345".to_vec()))
            .unwrap();
        assert!(store.has(&d).unwrap());
    }

    #[test]
    fn write_only_refusal_does_not_leak_pending_bytes() {
        let store = write_only(4);
        assert!(cap_err(store.put(&mut Cursor::new(b"too-big".to_vec()))));
        // If the refused stream's bytes stayed counted, this would be refused too.
        store.put(&mut Cursor::new(b"abcd".to_vec())).unwrap();
    }

    /// Delegates to a [`MemCas`] but panics if anything is reclaimed.
    struct NoReclaim(MemCas);
    impl CasBackend for NoReclaim {
        fn has(&self, d: &Digest) -> Result<bool, BackendError> {
            self.0.has(d)
        }
        fn get(&self, d: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
            self.0.get(d)
        }
        fn put(&self, s: &mut dyn Read) -> Result<Digest, BackendError> {
            self.0.put(s)
        }
        fn put_keyed(&self, d: &Digest, s: &mut dyn Read) -> Result<(), BackendError> {
            self.0.put_keyed(d, s)
        }
        fn size(&self, d: &Digest) -> Result<Option<u64>, BackendError> {
            self.0.size(d)
        }
    }
    impl ReclaimableCas for NoReclaim {
        fn iter_local(&self) -> Result<Vec<nessie_backend_core::LocalBlob>, BackendError> {
            self.0.iter_local()
        }
        fn reclaim(&self, _d: &Digest) -> Result<bool, BackendError> {
            panic!("write_only must never reclaim");
        }
    }

    #[test]
    fn write_only_never_reclaims_under_eviction_at_any_budget() {
        let store = CasStore::new(
            NoReclaim(MemCas::new()),
            RetentionPolicy::write_only(1 << 20),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        )
        .expect("store");
        let d = store.put(&mut Cursor::new(b"keep".to_vec())).unwrap();
        store.committed(&d);
        // The engine refuses the pass outright, so the panicking reclaim is never hit.
        assert!(
            store
                .evict_to_budget(
                    &crate::roots::RootRegistry::new(),
                    &crate::evict::NoDurableOracle,
                    &crate::evict::RefuseToReplicate,
                )
                .is_err()
        );
        assert!(store.has(&d).unwrap());
    }
}
