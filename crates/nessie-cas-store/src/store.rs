//! The [`CasStore`] wrapper — a managed [`CasBackend`] that adds the state durable
//! GC needs: an in-flight write-guard and per-blob write-time (for the grace
//! window). The durable mark-and-sweep itself lives in `gc.rs`; cache-mode LRU
//! recency and eviction land in a following slice.

use crate::clock::Clock;
use crate::policy::RetentionPolicy;
use nessie_backend_core::{
    BackendError, CasBackend, ContentRouter, Digest, PeerId, ReclaimableCas,
};
use std::collections::{HashMap, HashSet};
use std::io::Read;
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

    /// A snapshot of every known write-time, for a GC pass's grace check.
    pub(crate) fn written_snapshot(&self) -> HashMap<Digest, Duration> {
        self.written_at.clone()
    }

    /// The recency tick of `digest` (lower = colder; 0 = never touched = coldest).
    pub(crate) fn last_touch(&self, digest: &Digest) -> u64 {
        self.last_touch.get(digest).copied().unwrap_or(0)
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
}

impl<C: ReclaimableCas> CasStore<C> {
    /// Wrap `inner` with retention `policy`, advertising to `router` as node `me`
    /// and timing grace windows with `clock`.
    pub fn new(
        inner: C,
        policy: RetentionPolicy,
        router: Arc<dyn ContentRouter>,
        clock: Arc<dyn Clock>,
        me: PeerId,
    ) -> Self {
        Self {
            inner,
            policy,
            router,
            clock,
            me,
            state: Mutex::new(StoreState::default()),
        }
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
        let digest = self.inner.put(source)?;
        {
            let mut state = self.state();
            state.in_flight.insert(digest.clone()); // guard until committed/abandoned
            state.access.record_write(&digest, self.clock.now());
        }
        // Best-effort advertise: the data plane never depends on the router, so a
        // routing failure must not fail a successful local store.
        let _ = self.router.announce(&digest);
        Ok(digest)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        self.inner.put_keyed(expected, source)?;
        {
            let mut state = self.state();
            state.in_flight.insert(expected.clone()); // same write-guard as put
            state.access.record_write(expected, self.clock.now());
        }
        let _ = self.router.announce(expected);
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

    fn durable_store(me: &str, router: Arc<dyn ContentRouter>) -> CasStore<MemCas> {
        CasStore::new(
            MemCas::new(),
            RetentionPolicy::durable(Duration::from_secs(60)),
            router,
            Arc::new(MockClock::new()),
            PeerId::new(me),
        )
    }

    #[test]
    fn cas_store_is_a_transparent_cas() {
        let store = durable_store("me", Arc::new(MemRouter::new(PeerId::new("me"))));
        nessie_cas_conformance::run_all(&store);
    }

    #[test]
    fn put_announces_to_the_swarm() {
        let swarm = MemSwarm::new();
        let me = PeerId::new("node-me");
        let store = durable_store("node-me", Arc::new(swarm.node(me.clone())));
        let d = store.put(&mut Cursor::new(b"hello".to_vec())).unwrap();
        let observer = swarm.node(PeerId::new("observer"));
        assert_eq!(observer.providers(&d).unwrap(), vec![me]);
    }

    #[test]
    fn put_guards_in_flight_until_committed() {
        let store = durable_store("me", Arc::new(MemRouter::new(PeerId::new("me"))));
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
        let store = durable_store("me", Arc::new(MemRouter::new(PeerId::new("me"))));
        let d = store.put(&mut Cursor::new(b"body".to_vec())).unwrap();
        store.abandon(&d);
        assert!(!store.state().in_flight.contains(&d));
    }
}
