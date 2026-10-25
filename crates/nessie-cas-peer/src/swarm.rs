//! [`SwarmCas`]: local store with fetch-through from static peers.

use std::io::Read;
use std::sync::Arc;

use nessie_backend_core::{
    ActionCacheBackend, BackendError, CasBackend, Digest, LocalBlob, QuarantinedBlob,
    ReclaimableCas,
};

use crate::PeerSet;

/// Wraps a local store; a `get` miss is filled from the first peer whose body
/// verifies. Composes **beneath** `CasStore`, which stays untouched.
pub struct SwarmCas<C: ReclaimableCas> {
    inner: Arc<C>,
    peers: Arc<PeerSet>,
    cache_fills: bool,
}

impl<C: ReclaimableCas> SwarmCas<C> {
    /// Wrap `inner`, falling back to `peers`.
    #[must_use]
    pub fn new(inner: C, peers: Arc<PeerSet>) -> Self {
        Self {
            inner: Arc::new(inner),
            peers,
            cache_fills: true,
        }
    }

    /// Whether a fetched blob is stored locally (default `true`, for `lru`, where
    /// eviction manages the budget). `false` for `write_only`: the verified bytes
    /// are served but not stored, so nothing lands behind the capacity gate.
    #[must_use]
    pub fn with_cache_fills(mut self, cache_fills: bool) -> Self {
        self.cache_fills = cache_fills;
        self
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

impl<C: ReclaimableCas> SwarmCas<C> {
    /// As [`fetch`](Self::fetch) but keep nothing: the first peer body that hashes
    /// to `digest` is returned from memory. Bad bodies are skipped.
    // ponytail: buffered in RAM; spool to a temp file if write_only blobs outgrow it.
    fn fetch_uncached(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        for peer in self.peers.peers() {
            let Some(mut body) = self.peers.get(peer, digest) else {
                continue;
            };
            let mut bytes = Vec::new();
            if body.read_to_end(&mut bytes).is_ok() && digest.verify(&bytes) {
                return Ok(Box::new(std::io::Cursor::new(bytes)));
            }
        }
        Err(BackendError::BlobNotFound(digest.clone()))
    }
}

impl<C: ReclaimableCas> CasBackend for SwarmCas<C> {
    /// **Local only.** `CasStore` reads this as "already held and counted", so a
    /// blob only a peer holds must answer `false` or it would skip the write_only
    /// capacity gate. Remote readability is [`size`](CasBackend::size) and
    /// [`get`](CasBackend::get).
    fn has(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.inner.has(digest)
    }

    /// Local first; only a plain miss goes to the peers. A local blob that fails
    /// verification is an error, never served and never papered over.
    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        match self.inner.get(digest) {
            Err(BackendError::BlobNotFound(_)) if self.cache_fills => {
                self.fetch(digest)?;
                self.inner.get(digest)
            }
            Err(BackendError::BlobNotFound(_)) => self.fetch_uncached(digest),
            other => other,
        }
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        self.inner.put(source)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        self.inner.put_keyed(expected, source)
    }

    /// Local, else the length a peer reports on `HEAD` — no body fetched. S3 asks
    /// for the size before it reads; `get` still does the verified fetch-through.
    fn size(&self, digest: &Digest) -> Result<Option<u64>, BackendError> {
        if let Some(n) = self.inner.size(digest)? {
            return Ok(Some(n));
        }
        Ok(self
            .peers
            .peers()
            .iter()
            .find_map(|p| self.peers.size(p, digest)))
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

    /// Local only: a peer's copy is that peer's to scrub.
    fn verify(&self, digest: &Digest) -> Result<bool, BackendError> {
        self.inner.verify(digest)
    }

    fn quarantine(&self, digest: &Digest) -> Result<Option<QuarantinedBlob>, BackendError> {
        self.inner.quarantine(digest)
    }

    fn iter_quarantine(&self) -> Result<Vec<QuarantinedBlob>, BackendError> {
        self.inner.iter_quarantine()
    }
}
