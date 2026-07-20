//! The content-addressed storage backend contract.
//!
//! [`CasBackend`] is a **distinct backend family** from the volume supertrait
//! stack in [`crate::traits`]. Its noun is a [`Digest`] — a hash of bytes — not a
//! [`VolumeUuid`](crate::VolumeUuid), so it is deliberately *not* a tier of
//! `VolumeBackend`. A substrate may implement both families; they compose, they
//! do not nest. This is the P2P-native substrate the swarm is built on
//! (`docs/design/p2p-cas-swarm.md`).

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
}
