//! [`SwarmCas`]: local store with fetch-through from static peers.

use std::io::Read;
use std::sync::Arc;

use nessie_backend_core::{
    ActionCacheBackend, BackendError, CasBackend, Digest, LocalBlob, ReclaimableCas,
};

use crate::PeerSet;

/// Wraps a local store; a `get` miss is filled from the first peer whose body
/// verifies. Composes **beneath** `CasStore`, which stays untouched.
pub struct SwarmCas<C: ReclaimableCas> {
    inner: Arc<C>,
    peers: Arc<PeerSet>,
}

impl<C: ReclaimableCas> SwarmCas<C> {
    /// Wrap `inner`, falling back to `peers`.
    #[must_use]
    pub fn new(inner: C, peers: Arc<PeerSet>) -> Self {
        Self {
            inner: Arc::new(inner),
            peers,
        }
    }

    /// The local store alone, with no peer fallback — what a replicator reads from
    /// so replicating never recurses into fetch-through.
    #[must_use]
    pub fn local(&self) -> Arc<C> {
        self.inner.clone()
    }

    /// Pull `digest` from the first peer that serves bytes hashing to it. A peer
    /// that serves a tampered or truncated body stores nothing (`put_keyed`
    /// commits only verified bytes) and the next peer is tried.
    fn fetch(&self, digest: &Digest) -> Result<(), BackendError> {
        for peer in self.peers.peers() {
            let Some(mut body) = self.peers.get(peer, digest) else {
                continue;
            };
            if self.inner.put_keyed(digest, &mut body).is_ok() {
                return Ok(());
            }
        }
        Err(BackendError::BlobNotFound(digest.clone()))
    }
}

impl<C: ReclaimableCas> CasBackend for SwarmCas<C> {
    fn has(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.inner.has(digest)
    }

    /// Local first; only a plain miss goes to the peers. A local blob that fails
    /// verification is an error, never served and never papered over.
    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        match self.inner.get(digest) {
            Err(BackendError::BlobNotFound(_)) => {
                self.fetch(digest)?;
                self.inner.get(digest)
            }
            other => other,
        }
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        self.inner.put(source)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        self.inner.put_keyed(expected, source)
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, BackendError> {
        self.inner.size(digest)
    }

    fn as_action_cache(&self) -> Option<&dyn ActionCacheBackend> {
        self.inner.as_action_cache()
    }

    fn as_reclaimable(&self) -> Option<&dyn ReclaimableCas> {
        Some(self)
    }
}

impl<C: ReclaimableCas> ReclaimableCas for SwarmCas<C> {
    fn iter_local(&self) -> Result<Vec<LocalBlob>, BackendError> {
        self.inner.iter_local()
    }

    fn reclaim(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.inner.reclaim(digest)
    }
}
