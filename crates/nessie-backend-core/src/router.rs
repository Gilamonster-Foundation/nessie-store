//! Content routing — the swarm's answer to "who holds this digest?".
//!
//! [`ContentRouter`] is the one seam behind which the two settled routers sit — a
//! NATS rendezvous and a Kademlia DHT (both first-class; see the design doc's
//! settled decision #1). CAS/AC never see which is in play. The data plane stays
//! P2P regardless: a client fetches bytes *directly* from a listed provider
//! ([`AccessHandle::CasBlob`](crate::AccessHandle)); only discovery differs.
//!
//! The in-process reference implementation (`MemRouter`, in `nessie-backend-mem`)
//! models a swarm's provider registry with no network, so the storage-modes layer
//! (cache-mode eviction's replica gate) is testable against real code.

use crate::digest::Digest;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// A peer's identity in the routing layer — an opaque address (a multiaddr-like
/// string for v0). Distinct from [`SignerId`](crate::SignerId): routing identity
/// (where to fetch bytes) is a different concern from signing identity (who
/// attested a result). Serializes transparently as a plain string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerId(String);

impl PeerId {
    /// Wrap a peer address string.
    #[must_use]
    pub fn new(addr: impl Into<String>) -> Self {
        Self(addr.into())
    }

    /// Borrow the peer address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PeerId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

/// Why a [`ContentRouter`] operation failed. Non-exhaustive: real routers (NATS,
/// Kademlia) add transport-specific causes without breaking matches.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RouterError {
    /// The routing substrate could not be reached (network / broker down).
    #[error("content router unavailable: {0}")]
    Unavailable(String),
}

/// Resolves and advertises blob locations across the swarm.
///
/// `Send + Sync` so it can be held behind an `Arc<dyn ContentRouter>` and shared
/// by the daemon and the storage layer.
pub trait ContentRouter: Send + Sync {
    /// Peers believed to hold the blob for `digest` (empty if none are known).
    ///
    /// A router may or may not include the calling node in the result; a caller
    /// that needs a replica count *excluding itself* (cache-mode eviction) must
    /// filter by its own [`PeerId`].
    ///
    /// # Errors
    ///
    /// [`RouterError`] if the routing substrate cannot be queried.
    fn providers(&self, digest: &Digest) -> Result<Vec<PeerId>, RouterError>;

    /// Announce that **this** node holds `digest`. Idempotent.
    ///
    /// # Errors
    ///
    /// [`RouterError`] if the announcement cannot be published.
    fn announce(&self, digest: &Digest) -> Result<(), RouterError>;

    /// Withdraw this node's announcement for `digest` — e.g. after evicting it in
    /// cache mode. Idempotent (withdrawing an un-announced digest is a no-op).
    ///
    /// # Errors
    ///
    /// [`RouterError`] if the withdrawal cannot be published.
    fn withdraw(&self, digest: &Digest) -> Result<(), RouterError>;
}
