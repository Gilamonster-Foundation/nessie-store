//! The content-addressed storage backend contract.
//!
//! [`CasBackend`] is a **distinct backend family** from the volume supertrait
//! stack in [`crate::traits`]. Its noun is a [`Digest`] — a hash of bytes — not a
//! [`VolumeUuid`](crate::VolumeUuid), so it is deliberately *not* a tier of
//! `VolumeBackend`. A substrate may implement both families; they compose, they
//! do not nest. This is the P2P-native substrate the swarm is built on
//! (`docs/design/p2p-cas-swarm.md`).

use crate::action_cache::ActionCacheBackend;
use crate::digest::Digest;
use crate::error::BackendError;
use std::io::Read;

/// Immutable content-addressed blob storage — the P2P-native substrate.
///
/// A `CasBackend` stores and serves opaque byte blobs keyed by their [`Digest`].
/// The contract is intentionally tiny — `has` / `get` / `put` — because content
/// addressing carries the rest: keys are *computed from content*, integrity is
/// self-verifying, and identical blobs deduplicate automatically.
///
/// `Send + Sync` so the daemon can hold one behind an `Arc<dyn CasBackend>` and
/// dispatch from many async tasks, exactly as it does for `VolumeBackend`.
///
/// # Examples
///
/// The trait is object-safe — the daemon dispatches through a trait object:
///
/// ```
/// use std::sync::Arc;
/// use nessie_backend_core::CasBackend;
///
/// fn store(_backend: Arc<dyn CasBackend>) { /* ... */ }
/// ```
pub trait CasBackend: Send + Sync {
    /// Whether this node holds the blob for `digest` locally (no network fetch).
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] if the local existence check itself fails (I/O
    /// error); a plain "not present" is `Ok(false)`, not an error.
    fn has(&self, digest: &Digest) -> Result<bool, BackendError>;

    /// Open a reader over the blob named by `digest`.
    ///
    /// Implementations **must** verify the bytes hash to `digest` before serving
    /// them (up front, or while streaming), so a blob fetched from an untrusted
    /// peer cannot be tampered with — [`Digest::verify`] is the check.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] if the blob is absent or fails verification.
    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError>;

    /// Store the bytes read from `source`, returning their computed [`Digest`].
    ///
    /// The digest is *computed from the content* (with
    /// [`DigestAlgo::DEFAULT`](crate::DigestAlgo::DEFAULT)), never supplied by the
    /// caller — that is what makes storage content-addressed. Storing a blob that
    /// is already present is an idempotent no-op that returns the same digest.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] if reading `source` or persisting the blob fails.
    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError>;

    /// Store bytes under a **caller-supplied** `expected` digest, re-verifying they
    /// hash to it before committing. This is the write-side dual of
    /// [`get`](CasBackend::get)'s read verification, needed by a protocol face
    /// (REAPI) whose clients key blobs by a digest the face did not compute — the
    /// digest's own algorithm decides how to verify, so a SHA-256-keyed blob is
    /// stored under its SHA-256 digest with no index or recompute.
    ///
    /// Defaults to [`BackendError::FeatureNotSupported`] (the honest decline, like
    /// the accessor idioms); a backend that can honor keyed storage overrides it.
    /// Idempotent: an already-present blob is a no-op.
    ///
    /// # Errors
    ///
    /// [`BackendError::InvalidArgument`] if the bytes do not hash to `expected`;
    /// [`BackendError::FeatureNotSupported`] if the backend does not support keyed
    /// storage; other [`BackendError`]s from reading or persisting.
    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        let _ = (expected, source);
        Err(BackendError::FeatureNotSupported {
            capability: "put_keyed",
        })
    }

    /// The stored byte length of the blob for `digest`, or `None` if absent.
    ///
    /// The default reads and counts the blob (correct for any backend); backends
    /// that track sizes cheaply override it. A protocol face needs this to fill the
    /// `size_bytes` a content digest does not itself carry.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the size cannot be determined (an I/O error other than
    /// absence).
    fn size(&self, digest: &Digest) -> Result<Option<u64>, BackendError> {
        match self.get(digest) {
            Ok(mut reader) => {
                let n = std::io::copy(&mut reader, &mut std::io::sink())
                    .map_err(|e| BackendError::Internal(format!("cas size of {digest}: {e}")))?;
                Ok(Some(n))
            }
            Err(BackendError::BlobNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Upcast to the action-cache tier if this backend attests results, else
    /// `None`. Mirrors [`VolumeBackend::as_snapshot`](crate::VolumeBackend::as_snapshot):
    /// the default `None` is the honest decline, and `as_action_cache().is_some()`
    /// is the in-process capability probe (a REAPI/NFS face downcasts at dispatch
    /// and returns "feature not supported" when it is `None`).
    fn as_action_cache(&self) -> Option<&dyn ActionCacheBackend> {
        None
    }

    /// Upcast to the reclaimable tier if this backend can enumerate and drop its
    /// **local** replicas, else `None`. Same accessor idiom as `as_action_cache`.
    /// A managed durable-GC or cache-eviction `CasStore` requires `Some`; a
    /// read-through / remote view returns `None` and cannot host one.
    fn as_reclaimable(&self) -> Option<&dyn ReclaimableCas> {
        None
    }
}

/// One locally-held blob replica: its digest and on-disk byte size. The unit
/// [`ReclaimableCas::iter_local`] yields, consumed by durable-GC sweep accounting
/// and cache-eviction byte-budget accounting alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBlob {
    /// The blob's content digest.
    pub digest: Digest,
    /// The bytes this replica occupies locally.
    pub size_bytes: u64,
}

/// A [`CasBackend`] whose **local replicas** can be enumerated and reclaimed — the
/// extra capability durable GC and cache eviction both need.
///
/// Content is immutable and self-verifying, so a node dropping its local replica of
/// a blob loses nothing recoverable: the blob can be re-fetched by digest from a
/// peer (git prunes its local object store without changing object identity).
/// Reclaiming is the **only** removal in the CAS family — `has`/`get`/`put` never
/// delete — and a `CasStore` calls it only on a blob it has proven unreachable
/// (durable GC) or sufficiently replicated and unpinned (cache eviction).
pub trait ReclaimableCas: CasBackend {
    /// Every digest held locally, with its byte size — the durable sweep domain and
    /// the cache eviction candidate pool. A point-in-time snapshot; order is
    /// unspecified.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the local store cannot be enumerated.
    fn iter_local(&self) -> Result<Vec<LocalBlob>, BackendError>;

    /// Drop this node's local replica of `digest`. Idempotent: `Ok(false)` if it was
    /// already absent, `Ok(true)` if it was present and removed. Idempotency is what
    /// makes an interrupted sweep safely resumable.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the removal fails for a reason other than absence.
    fn reclaim(&self, digest: &Digest) -> Result<bool, BackendError>;
}
