//! What a scrub pass needs from the store: read-only verification that leaves LRU
//! recency alone, and the transitions that take a corrupt blob out while keeping the
//! write-only books true.

use std::sync::atomic::Ordering;

use nessie_backend_core::{BackendError, Digest, LocalBlob, QuarantinedBlob, ReclaimableCas};

use crate::{CasStore, StorageMode};

/// What [`CasStore::quarantine`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quarantine {
    /// Moved aside; its charge now sits on the quarantined file.
    Moved(QuarantinedBlob),
    /// Not held, nothing to move.
    Absent,
    /// An ungated re-put of the digest is in flight; try again on a later pass.
    Deferred,
}

impl<C: ReclaimableCas> CasStore<C> {
    /// Every blob held locally. A point-in-time snapshot, order unspecified.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the backend cannot be enumerated.
    pub fn local_blobs(&self) -> Result<Vec<LocalBlob>, BackendError> {
        self.inner.iter_local()
    }

    /// Every quarantined copy still retained (live blobs are not listed).
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the quarantine cannot be enumerated.
    pub fn quarantined(&self) -> Result<Vec<QuarantinedBlob>, BackendError> {
        self.inner.iter_quarantine()
    }

    /// Bytes charged against `write_only` capacity: live blobs as admitted plus every
    /// quarantined file at its real size. Meaningful in `write_only` only.
    #[must_use]
    pub fn committed_bytes(&self) -> u64 {
        self.committed.load(Ordering::SeqCst)
    }

    /// Does the local copy of `digest` still hash to it? Unlike a `get` this does not
    /// rewarm the blob, so a scrub never disturbs LRU order.
    ///
    /// # Errors
    ///
    /// [`BackendError::BlobNotFound`] if it is not held; otherwise as
    /// [`ReclaimableCas::verify`].
    pub fn verify_local(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.inner.verify(digest)
    }

    /// Move a corrupt blob aside (never deleted, never overwriting an earlier
    /// quarantined copy). In one step under the state lock it credits the size the
    /// blob was *charged* and debits the quarantine file's real size, so the bytes
    /// stay counted; that can leave the node over capacity, which only refuses new
    /// writes. A blob with an ungated re-put in flight is left alone
    /// ([`Quarantine::Deferred`]): that write would publish a copy nothing is
    /// charged for, and it may well heal the blob.
    ///
    /// # Errors
    ///
    /// As [`ReclaimableCas::quarantine`].
    pub fn quarantine(&self, digest: &Digest) -> Result<Quarantine, BackendError> {
        let mut state = self.state();
        if state.fast_inflight.contains_key(digest) {
            return Ok(Quarantine::Deferred);
        }
        let Some(q) = self.inner.quarantine(digest)? else {
            return Ok(Quarantine::Absent);
        };
        let credit = state.accounted.remove(digest).unwrap_or(0);
        state.committed = state.committed.saturating_sub(credit) + q.size_bytes;
        state.quarantined.insert(q.id.clone(), q.size_bytes);
        self.committed.store(state.committed, Ordering::SeqCst);
        state.access.forget(digest);
        drop(state);
        let _ = self.router.withdraw(digest); // de-advertise, best effort
        Ok(Quarantine::Moved(q))
    }

    /// Would a new `size`-byte write be admitted by the `write_only` capacity gate
    /// right now? Always `true` outside `write_only`.
    #[must_use]
    pub fn admits(&self, size: u64) -> bool {
        let StorageMode::WriteOnly(p) = &self.policy.mode else {
            return true;
        };
        let used = self.committed.load(Ordering::SeqCst) + self.pending.load(Ordering::SeqCst);
        used.saturating_add(size) <= p.capacity
    }

    /// Drop a corrupt blob from a cache — it is refetched on the next read. Credits
    /// what was charged. `Ok(false)` if it was not held.
    ///
    /// # Errors
    ///
    /// As [`ReclaimableCas::reclaim`].
    pub fn reclaim_corrupt(&self, digest: &Digest) -> Result<bool, BackendError> {
        let mut state = self.state();
        if !self.inner.reclaim(digest)? {
            return Ok(false);
        }
        let credit = state.accounted.remove(digest).unwrap_or(0);
        state.committed = state.committed.saturating_sub(credit);
        self.committed.store(state.committed, Ordering::SeqCst);
        state.access.forget(digest);
        drop(state);
        let _ = self.router.withdraw(digest);
        Ok(true)
    }
}
