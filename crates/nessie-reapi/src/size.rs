//! The blob-size seam the emit path depends on.
//!
//! `GetActionResult` fills each output's REAPI `size_bytes` — a length a content
//! digest does not itself carry. A narrow [`SizeSource`] lets the mappers stay
//! unit-testable with a hand-built `{digest → size}` table, decoupled from the whole
//! `CasBackend`.

use crate::status::status_from_backend;
use nessie_backend_core::{CasBackend, Digest};
use std::sync::Arc;
use tonic::Status;

/// Resolves a blob's byte size (for REAPI `Digest.size_bytes`).
pub trait SizeSource: Send + Sync {
    /// The byte size of `digest`'s blob.
    ///
    /// # Errors
    ///
    /// [`tonic::Status::not_found`] if the blob is absent; other statuses on error.
    fn size_of(&self, digest: &Digest) -> Result<u64, Status>;
}

/// A [`SizeSource`] backed by a [`CasBackend`]'s `size`.
pub struct CasSizeSource(pub Arc<dyn CasBackend>);

impl SizeSource for CasSizeSource {
    fn size_of(&self, digest: &Digest) -> Result<u64, Status> {
        self.0
            .size(digest)
            .map_err(|e| status_from_backend(&e))?
            .ok_or_else(|| Status::not_found(format!("blob {digest} not found")))
    }
}
