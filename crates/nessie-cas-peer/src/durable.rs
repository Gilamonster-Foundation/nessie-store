//! The cache-eviction gates, answered by the durable peers.

use std::num::NonZeroUsize;
use std::sync::Arc;

use nessie_backend_core::{BackendError, CasBackend, Digest};
use nessie_cas_store::{DurabilityOracle, Replicator};

use crate::PeerSet;

/// `true` iff some peer answers `HEAD 200` **and** reports `x-nessie-mode:
/// write_only`. The `durable` flag in config is only a hint of whom to ask; the peer
/// proves it by its own mode, so an `lru` node mislabelled durable is not counted.
/// An unreachable peer answers "no".
#[derive(Debug, Clone)]
pub struct StaticDurabilityOracle {
    peers: Arc<PeerSet>,
}

impl StaticDurabilityOracle {
    /// Ask the durable members of `peers`.
    #[must_use]
    pub fn new(peers: Arc<PeerSet>) -> Self {
        Self { peers }
    }
}

impl DurabilityOracle for StaticDurabilityOracle {
    fn durably_held(&self, digest: &Digest) -> Result<bool, BackendError> {
        Ok(self
            .peers
            .peers()
            .iter()
            .filter(|p| p.durable)
            .any(|p| self.peers.holds_durably(p, digest)))
    }
}

/// `PUT`s a local blob to durable peers until `target` of them hold it.
pub struct HttpReplicator {
    peers: Arc<PeerSet>,
    local: Arc<dyn CasBackend>,
}

impl HttpReplicator {
    /// Replicate blobs read from `local` (the store *without* peer fallback).
    #[must_use]
    pub fn new(peers: Arc<PeerSet>, local: Arc<dyn CasBackend>) -> Self {
        Self { peers, local }
    }
}

impl HttpReplicator {
    /// Push `digest` to hinted peers until `target` of them are confirmed holding it
    /// *durably*, returning how many are. A peer counts only if it answers `HEAD 200`
    /// **and** reports `x-nessie-mode: write_only` — before our `PUT` or after it. A
    /// peer that reports `lru` may evict the blob at once, so it is skipped (still
    /// pushed to, never counted) and the next peer is tried. This is what
    /// `write_only` mode waits on before acking a client.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the local blob cannot be read.
    pub fn push(&self, digest: &Digest, target: NonZeroUsize) -> Result<usize, BackendError> {
        let mut held = 0;
        for peer in self.peers.peers().iter().filter(|p| p.durable) {
            if held >= target.get() {
                break;
            }
            if !self.peers.holds_durably(peer, digest) {
                let len = self.local.size(digest)?;
                if !self.peers.put(peer, digest, self.local.get(digest)?, len)
                    || !self.peers.holds_durably(peer, digest)
                {
                    continue;
                }
            }
            held += 1;
        }
        Ok(held)
    }
}

impl Replicator for HttpReplicator {
    /// Best effort: [`push`](HttpReplicator::push) and ignore the shortfall. Falling
    /// short is not an error — the caller keeps the blob and the oracle simply keeps
    /// answering "no".
    fn replicate(&self, digest: &Digest, target: NonZeroUsize) -> Result<(), BackendError> {
        self.push(digest, target).map(|_| ())
    }
}
