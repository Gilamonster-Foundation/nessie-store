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
    /// Write-only: digest → the bytes charged when it was admitted (or seeded at
    /// startup). The source of truth: a blob is counted iff it is a key, so a
    /// duplicate never counts twice, and a credit is this number — never the current
    /// length of a file that may have been corrupted since.
    pub(crate) accounted: HashMap<Digest, u64>,
    /// Write-only: quarantine id → the file's real on-disk length. Quarantined bytes
    /// are still retained, so they stay charged.
    pub(crate) quarantined: HashMap<String, u64>,
    /// Write-only: digests with ungated re-puts in flight (a count per digest). A
    /// re-put of an accounted blob adds nothing, so quarantine must not move that
    /// blob's charge away until the write has finished publishing.
    pub(crate) fast_inflight: HashMap<Digest, usize>,
    /// Write-only: `accounted` plus `quarantined` bytes — every byte this node
    /// retains. Changed only under the state lock.
    pub(crate) committed: u64,
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
    /// Write-only: lock-free mirror of `StoreState::committed`, written only while
    /// holding the state lock, so the gate can read it per chunk without locking.
    pub(crate) committed: AtomicU64,
    /// Write-only: bytes reserved by writers still streaming. Always returned by the
    /// writer's [`CapacityGate`] on drop (success, error or panic), never kept.
    pub(crate) pending: AtomicU64,
}

/// Registers an ungated re-put of `digest` for as long as it lives, success, error or
/// panic, so quarantine can see it.
struct FastWrite<'a, C: ReclaimableCas> {
    store: &'a CasStore<C>,
    digest: &'a Digest,
}

impl<C: ReclaimableCas> Drop for FastWrite<'_, C> {
    fn drop(&mut self) {
        let mut state = self.store.state();
        if let Some(n) = state.fast_inflight.get_mut(self.digest) {
            *n -= 1;
            if *n == 0 {
                state.fast_inflight.remove(self.digest);
            }
        }
    }
}

/// Reserves bytes against `capacity` as they stream in and fails the read once
/// `committed + pending` would pass it, so the backend aborts and discards its temp
/// file: a refused write commits nothing. Dropping the gate returns exactly what it
/// reserved, whatever way the write ended.
struct CapacityGate<'a> {
    src: &'a mut dyn Read,
    capacity: u64,
    committed: &'a AtomicU64,
    pending: &'a AtomicU64,
    mine: u64,
    tripped: bool,
}

impl Read for CapacityGate<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.src.read(buf)?;
        let n64 = n as u64;
        let before = self.pending.fetch_add(n64, Ordering::SeqCst);
        let committed = self.committed.load(Ordering::SeqCst);
        if committed.saturating_add(before).saturating_add(n64) > self.capacity {
            self.pending.fetch_sub(n64, Ordering::SeqCst);
            self.tripped = true;
            return Err(io::Error::other("write-only capacity exceeded"));
        }
        self.mine += n64;
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
        for blob in inner.iter_local()? {
            state.access.seed_written(&blob.digest, now);
            state.accounted.insert(blob.digest, blob.size_bytes);
            state.committed += blob.size_bytes;
        }
        for q in inner.iter_quarantine()? {
            state.committed += q.size_bytes;
            state.quarantined.insert(q.id, q.size_bytes);
        }
        Ok(Self {
            inner,
            policy,
            router,
            clock,
            me,
            committed: AtomicU64::new(state.committed),
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

    /// Count `digest`'s `size` bytes as committed, once. `size` is the gate's own
    /// byte count, which `put_keyed` verified against the digest, so the backend is
    /// never asked (a failing lookup could otherwise leave stored bytes uncounted).
    /// Called after the backend holds the blob and *before* the writer's gate returns
    /// its `pending`, so the bytes are never uncounted in between (a transient double
    /// count only ever refuses).
    fn account(&self, digest: &Digest, size: u64) {
        if size == 0 {
            return; // read nothing: the blob already existed (its publisher counts it) or is empty
        }
        let mut state = self.state();
        if let std::collections::hash_map::Entry::Vacant(v) = state.accounted.entry(digest.clone())
        {
            v.insert(size);
            state.committed += size;
            self.committed.store(state.committed, Ordering::SeqCst);
        }
    }

    /// Write-only `put_keyed`: a blob already charged is a no-op (never refused);
    /// otherwise the write streams through the capacity gate.
    fn put_keyed_write_only(
        &self,
        capacity: u64,
        expected: &Digest,
        source: &mut dyn Read,
    ) -> Result<(), BackendError> {
        // Already held: charged, or committed by a writer that has not charged it yet
        // (it holds the reservation). A re-put adds nothing, takes nothing and is
        // never refused. The decision and the registration happen under the state
        // lock, which quarantine also holds, so a charge cannot move to quarantine
        // while this write is still publishing.
        let fast = {
            let mut state = self.state();
            let held =
                state.accounted.contains_key(expected) || self.inner.has(expected).unwrap_or(false);
            if held {
                *state.fast_inflight.entry(expected.clone()).or_default() += 1;
            }
            held
        };
        if fast {
            let _registered = FastWrite {
                store: self,
                digest: expected,
            };
            return self.inner.put_keyed(expected, source);
        }
        let mut gate = CapacityGate {
            src: source,
            capacity,
            committed: &self.committed,
            pending: &self.pending,
            mine: 0,
            tripped: false,
        };
        let out = self.inner.put_keyed(expected, &mut gate);
        if gate.tripped {
            return Err(BackendError::CapacityExceeded {
                used: self.committed.load(Ordering::SeqCst),
                incoming: gate.mine,
                capacity,
            });
        }
        out?;
        self.account(expected, gate.mine); // committed rises first; `gate` drops after
        Ok(())
    }

    /// Book-keeping after a successful write: write-guard, recency, advert.
    fn after_write(&self, digest: &Digest) {
        {
            let mut state = self.state();
            state.in_flight.insert(digest.clone()); // guard until committed/abandoned
            state.access.record_write(digest, self.clock.now());
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

    fn ensure_replicated(&self, digest: &Digest) -> Result<(), BackendError> {
        self.inner.ensure_replicated(digest)
    }

    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        let reader = self.inner.get(digest)?;
        self.state().access.touch(digest); // a read rewarms the blob (LRU)
        Ok(reader)
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        if matches!(self.policy.mode, StorageMode::WriteOnly(_)) {
            // ponytail: write_only unkeyed put buffers the blob in RAM to learn its
            // digest before admission; stream to a spool file if blobs outgrow memory
            let mut body = Vec::new();
            source
                .read_to_end(&mut body)
                .map_err(|e| BackendError::Internal(format!("read source: {e}")))?;
            let digest = Digest::compute(&body);
            self.put_keyed(&digest, &mut body.as_slice())?;
            return Ok(digest);
        }
        let digest = self.inner.put(source)?;
        self.after_write(&digest);
        Ok(digest)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        match &self.policy.mode {
            StorageMode::WriteOnly(p) => self.put_keyed_write_only(p.capacity, expected, source)?,
            StorageMode::Lru(_) => self.inner.put_keyed(expected, source)?,
        }
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

    fn stored_bytes(store: &CasStore<MemCas>) -> u64 {
        store
            .inner
            .iter_local()
            .unwrap()
            .iter()
            .map(|b| b.size_bytes)
            .sum()
    }

    /// Hands out `data` in `chunk`-byte reads, running `hook(read_index)` before each.
    struct Hooked<'a> {
        data: &'a [u8],
        chunk: usize,
        reads: usize,
        hook: Box<dyn FnMut(usize) + Send + 'a>,
    }
    impl Read for Hooked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            (self.hook)(self.reads);
            self.reads += 1;
            let n = self.chunk.min(self.data.len()).min(buf.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// Regression (PR #142 P1): a writer whose gate was built before another writer
    /// finished must still see that writer's committed bytes. Capacity 10, two 6-byte
    /// writers: B is mid-admission (blocked on its first read) while A completes.
    #[test]
    fn write_only_capacity_holds_when_a_stale_writer_resumes() {
        use std::sync::mpsc::channel;
        let store = write_only(10);
        let (a, b) = (b"aaaaaa", b"bbbbbb");
        let (started_tx, started_rx) = channel();
        let (go_tx, go_rx) = channel::<()>();
        let (res_a, res_b) = std::thread::scope(|sc| {
            let store = &store;
            let hb = sc.spawn(move || {
                let mut src = Hooked {
                    data: b,
                    chunk: 6,
                    reads: 0,
                    hook: Box::new(move |i| {
                        if i == 0 {
                            started_tx.send(()).unwrap();
                            go_rx.recv().unwrap();
                        }
                    }),
                };
                store.put_keyed(&Digest::compute(b), &mut src)
            });
            started_rx.recv().unwrap(); // B is inside its gate
            let res_a = store.put_keyed(&Digest::compute(a), &mut Cursor::new(a.to_vec()));
            go_tx.send(()).unwrap();
            (res_a, hb.join().unwrap())
        });
        assert!(res_a.is_ok());
        assert!(cap_err(res_b), "B must be refused: A already holds 6 of 10");
        assert_eq!(stored_bytes(&store), 6);
        assert_eq!(store.pending.load(Ordering::SeqCst), 0);
        assert_eq!(store.committed.load(Ordering::SeqCst), 6);
        assert!(!store.has(&Digest::compute(b)).unwrap());
    }

    /// Two writers both mid-stream at once (barrier after their first chunk): exactly
    /// one fits in capacity 10.
    #[test]
    fn write_only_capacity_holds_for_two_writers_mid_stream() {
        use std::sync::Barrier;
        let store = write_only(10);
        let barrier = Barrier::new(2);
        let (a, b) = (b"aaaaaa", b"bbbbbb");
        let run = |data: &'static [u8; 6]| {
            let barrier = &barrier;
            let store = &store;
            move || {
                let mut src = Hooked {
                    data,
                    chunk: 3,
                    reads: 0,
                    hook: Box::new(move |i| {
                        if i == 1 {
                            barrier.wait(); // both have reserved their first 3 bytes
                        }
                    }),
                };
                store.put_keyed(&Digest::compute(data), &mut src)
            }
        };
        let (ra, rb) = std::thread::scope(|sc| {
            let ha = sc.spawn(run(a));
            let hb = sc.spawn(run(b));
            (ha.join().unwrap(), hb.join().unwrap())
        });
        assert_eq!(
            [ra.is_ok(), rb.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "exactly one writer fits: {ra:?} / {rb:?}"
        );
        assert!(cap_err(if ra.is_ok() { rb } else { ra }));
        assert_eq!(stored_bytes(&store), 6);
        assert_eq!(store.pending.load(Ordering::SeqCst), 0);
        assert_eq!(store.committed.load(Ordering::SeqCst), 6);
    }

    /// Regression (PR #142 P2): re-putting held content is a no-op, even when full.
    #[test]
    fn write_only_unkeyed_reput_of_held_blob_is_free() {
        let store = write_only(5);
        let d = store.put(&mut Cursor::new(b"12345".to_vec())).unwrap();
        let again = store.put(&mut Cursor::new(b"12345".to_vec())).unwrap();
        assert_eq!(d, again);
        assert_eq!(stored_bytes(&store), 5);
        assert_eq!(store.pending.load(Ordering::SeqCst), 0);
        assert_eq!(store.committed.load(Ordering::SeqCst), 5);
    }

    /// A [`MemCas`] whose first `put_keyed` commits and then parks on two barriers
    /// before returning, so a test can run another writer inside that window.
    struct Pausing {
        inner: MemCas,
        first: std::sync::atomic::AtomicBool,
        arrived: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
        size_fails: std::sync::atomic::AtomicBool,
    }
    impl CasBackend for Pausing {
        fn has(&self, d: &Digest) -> Result<bool, BackendError> {
            self.inner.has(d)
        }
        fn get(&self, d: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
            self.inner.get(d)
        }
        fn put(&self, s: &mut dyn Read) -> Result<Digest, BackendError> {
            self.inner.put(s)
        }
        fn put_keyed(&self, d: &Digest, s: &mut dyn Read) -> Result<(), BackendError> {
            self.inner.put_keyed(d, s)?;
            if self.first.swap(false, Ordering::SeqCst) {
                self.arrived.wait(); // committed, not yet returned
                self.release.wait();
            }
            Ok(())
        }
        fn size(&self, d: &Digest) -> Result<Option<u64>, BackendError> {
            if self.size_fails.load(Ordering::SeqCst) {
                return Err(BackendError::Internal("stat failed".into()));
            }
            self.inner.size(d)
        }
    }
    impl ReclaimableCas for Pausing {
        fn iter_local(&self) -> Result<Vec<nessie_backend_core::LocalBlob>, BackendError> {
            self.inner.iter_local()
        }
        fn reclaim(&self, d: &Digest) -> Result<bool, BackendError> {
            self.inner.reclaim(d)
        }
    }

    /// Regression (PR #142 round 3 P1): writer A commits a new blob and pauses before
    /// returning; writer B re-puts the same digest through the already-held fast path
    /// and finishes first. A's bytes must stay counted.
    #[test]
    fn write_only_same_digest_race_keeps_the_blob_counted() {
        let arrived = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let store = CasStore::new(
            Pausing {
                inner: MemCas::new(),
                first: std::sync::atomic::AtomicBool::new(true),
                arrived: arrived.clone(),
                release: release.clone(),
                size_fails: std::sync::atomic::AtomicBool::new(false),
            },
            RetentionPolicy::write_only(10),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        )
        .expect("store");
        let a = b"aaaaaa";
        let da = Digest::compute(a);
        std::thread::scope(|sc| {
            let store = &store;
            let da = &da;
            let ha = sc.spawn(move || store.put_keyed(da, &mut Cursor::new(a.to_vec())));
            arrived.wait();
            store.put_keyed(da, &mut Cursor::new(a.to_vec())).unwrap(); // B, fast path
            release.wait();
            ha.join().unwrap().unwrap();
        });
        let stored: u64 = store
            .inner
            .iter_local()
            .unwrap()
            .iter()
            .map(|b| b.size_bytes)
            .sum();
        assert_eq!(stored, 6);
        let other = Digest::compute(b"bbbbbb");
        assert!(
            cap_err(store.put_keyed(&other, &mut Cursor::new(b"bbbbbb".to_vec()))),
            "6 of 10 are held; another 6 must be refused"
        );
    }

    /// Regression (PR #142 round 3 P2): a source that panics mid-stream must not leak
    /// the bytes it had reserved.
    #[test]
    fn write_only_panicking_source_returns_its_reservation() {
        let store = write_only(10);
        let d = Digest::compute(b"0123456789");
        let mut src = Hooked {
            data: b"0123456789",
            chunk: 3,
            reads: 0,
            hook: Box::new(|i| assert!(i < 1, "source blew up")),
        };
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.put_keyed(&d, &mut src)
        }));
        assert!(out.is_err(), "the panic propagates");
        store
            .put_keyed(&d, &mut Cursor::new(b"0123456789".to_vec()))
            .expect("a full-capacity write still fits");
        assert_eq!(stored_bytes(&store), 10);
    }

    fn pausing(
        first: bool,
        size_fails: bool,
    ) -> (
        CasStore<Pausing>,
        Arc<std::sync::Barrier>,
        Arc<std::sync::Barrier>,
    ) {
        let arrived = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let store = CasStore::new(
            Pausing {
                inner: MemCas::new(),
                first: std::sync::atomic::AtomicBool::new(first),
                arrived: arrived.clone(),
                release: release.clone(),
                size_fails: std::sync::atomic::AtomicBool::new(size_fails),
            },
            RetentionPolicy::write_only(10),
            Arc::new(MemRouter::new(PeerId::new("me"))),
            Arc::new(MockClock::new()),
            PeerId::new("me"),
        )
        .expect("store");
        (store, arrived, release)
    }

    /// Regression (PR #142 round 4 P1): accounting must not depend on a fallible
    /// `size()` lookup. The write succeeds while `size()` errors; its bytes still count.
    #[test]
    fn write_only_counts_a_write_even_when_size_lookup_fails() {
        let (store, _, _) = pausing(false, true);
        let a = b"aaaaaa";
        store
            .put_keyed(&Digest::compute(a), &mut Cursor::new(a.to_vec()))
            .unwrap();
        assert_eq!(store.committed.load(Ordering::SeqCst), 6);
        assert_eq!(store.pending.load(Ordering::SeqCst), 0);
        let b = b"bbbbbb";
        assert!(cap_err(
            store.put_keyed(&Digest::compute(b), &mut Cursor::new(b.to_vec()))
        ));
    }

    /// Same-digest schedule with `size()` failing throughout.
    #[test]
    fn write_only_same_digest_race_survives_size_failure() {
        let (store, arrived, release) = pausing(true, true);
        let a = b"aaaaaa";
        let da = Digest::compute(a);
        std::thread::scope(|sc| {
            let (store, da) = (&store, &da);
            let ha = sc.spawn(move || store.put_keyed(da, &mut Cursor::new(a.to_vec())));
            arrived.wait();
            store.put_keyed(da, &mut Cursor::new(a.to_vec())).unwrap(); // fast path
            release.wait();
            ha.join().unwrap().unwrap();
        });
        assert_eq!(store.committed.load(Ordering::SeqCst), 6);
        assert_eq!(store.pending.load(Ordering::SeqCst), 0);
        let b = b"bbbbbb";
        assert!(cap_err(
            store.put_keyed(&Digest::compute(b), &mut Cursor::new(b.to_vec()))
        ));
    }
}
