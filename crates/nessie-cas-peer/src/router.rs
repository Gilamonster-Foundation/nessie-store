//! [`StaticRouter`]: the configured peers as a [`ContentRouter`].

use std::sync::Arc;

use nessie_backend_core::{ContentRouter, Digest, PeerId, RouterError};

use crate::PeerSet;

/// Routes by asking the static peers. `announce` / `withdraw` are no-ops: peers
/// learn what we hold only by asking us.
#[derive(Debug, Clone)]
pub struct StaticRouter {
    peers: Arc<PeerSet>,
}

impl StaticRouter {
    /// Route over `peers`.
    #[must_use]
    pub fn new(peers: Arc<PeerSet>) -> Self {
        Self { peers }
    }
}

impl ContentRouter for StaticRouter {
    /// The peers that answer `HEAD 200` for `digest`, identified by URL. Honest by
    /// construction: cache eviction counts these as replicas, so a peer that does
    /// not hold the blob must not appear.
    fn providers(&self, digest: &Digest) -> Result<Vec<PeerId>, RouterError> {
        Ok(self
            .peers
            .peers()
            .iter()
            .filter(|p| self.peers.has(p, digest))
            .map(|p| PeerId::new(p.url.clone()))
            .collect())
    }

    fn announce(&self, _digest: &Digest) -> Result<(), RouterError> {
        Ok(())
    }

    fn withdraw(&self, _digest: &Digest) -> Result<(), RouterError> {
        Ok(())
    }
}
