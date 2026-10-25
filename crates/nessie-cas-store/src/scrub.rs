//! What a scrub pass needs from the store: read-only verification that leaves LRU
//! recency alone, and the two ways to take a corrupt blob out (keeping the books).

use nessie_backend_core::{BackendError, Digest, LocalBlob, ReclaimableCas};

use crate::CasStore;

impl<C: ReclaimableCas> CasStore<C> {
    /// Every blob held locally. A point-in-time snapshot, order unspecified.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the backend cannot be enumerated.
    pub fn local_blobs(&self) -> Result<Vec<LocalBlob>, BackendError> {
        self.inner.iter_local()
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

    /// Move a corrupt blob aside (never deleted) and stop counting its `size` bytes
    /// against capacity, so restoring it is admitted like a new write. `Ok(false)`
    /// if it was not held.
    ///
    /// # Errors
    ///
    /// As [`ReclaimableCas::quarantine`].
    pub fn quarantine(&self, digest: &Digest, size: u64) -> Result<bool, BackendError> {
        self.release(self.inner.quarantine(digest)?, digest, size)
    }

    /// Drop a corrupt blob from a cache — it is refetched on the next read.
    /// `Ok(false)` if it was not held.
    ///
    /// # Errors
    ///
    /// As [`ReclaimableCas::reclaim`].
    pub fn reclaim_corrupt(&self, digest: &Digest, size: u64) -> Result<bool, BackendError> {
        self.release(self.inner.reclaim(digest)?, digest, size)
    }

    fn release(&self, gone: bool, digest: &Digest, size: u64) -> Result<bool, BackendError> {
        if gone {
            self.unaccount(digest, size);
            self.state().access.forget(digest);
            let _ = self.router.withdraw(digest); // de-advertise, best effort
        }
        Ok(gone)
    }
}
