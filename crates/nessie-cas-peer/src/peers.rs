//! The configured peer list and the three HTTP verbs spoken to it.

use std::io::Read;
use std::time::Duration;

use nessie_backend_core::{BackendError, Digest};
use reqwest::blocking::{Body, Client, Response};

/// One static peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Base URL, e.g. `http://host:8443` (no trailing path).
    pub url: String,
    /// Whether the peer is a durable store of record.
    pub durable: bool,
}

impl Peer {
    fn blob_url(&self, digest: &Digest) -> String {
        format!("{}/cas/v1/blobs/{digest}", self.url.trim_end_matches('/'))
    }
}

/// The peer list plus the HTTP client shared by router, swarm, oracle and replicator.
#[derive(Debug)]
pub struct PeerSet {
    peers: Vec<Peer>,
    client: Client,
}

impl PeerSet {
    /// Build the set. The client is created on a scratch thread because
    /// `reqwest::blocking` refuses to be built inside an async runtime, and the
    /// daemon builds its node inside one.
    ///
    /// # Errors
    ///
    /// [`BackendError::Internal`] if the HTTP client cannot be built.
    pub fn new(mut peers: Vec<Peer>) -> Result<Self, BackendError> {
        // Distinct holders only: a URL listed twice must not count as two replicas.
        let mut seen = std::collections::HashSet::new();
        peers.retain(|p| seen.insert(p.url.trim_end_matches('/').to_owned()));
        let client = std::thread::spawn(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
        })
        .join()
        .map_err(|_| BackendError::Internal("peer client thread panicked".into()))?
        .map_err(|e| BackendError::Internal(format!("peer client: {e}")))?;
        Ok(Self { peers, client })
    }

    /// The configured peers, in order.
    #[must_use]
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// `HEAD` — does `peer` hold `digest`? Any failure answers `false`.
    pub(crate) fn has(&self, peer: &Peer, digest: &Digest) -> bool {
        self.head(peer, digest).is_some()
    }

    /// `HEAD` — the blob's length as `peer` reports it, if it holds the blob. No
    /// body is fetched.
    pub(crate) fn size(&self, peer: &Peer, digest: &Digest) -> Option<u64> {
        self.head(peer, digest)?
    }

    /// `Some(content-length)` on a 2xx `HEAD`; `None` on 404 / failure.
    fn head(&self, peer: &Peer, digest: &Digest) -> Option<Option<u64>> {
        let r = self.client.head(peer.blob_url(digest)).send().ok()?;
        r.status().is_success().then(|| {
            // `Response::content_length` is the (empty) HEAD body's; read the header.
            r.headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()?.parse().ok())
        })
    }

    /// `GET` — the peer's response if it has the blob; `None` on 404 / failure.
    pub(crate) fn get(&self, peer: &Peer, digest: &Digest) -> Option<Response> {
        self.client
            .get(peer.blob_url(digest))
            .send()
            .ok()
            .filter(|r| r.status().is_success())
    }

    /// `PUT` — upload `body` (of `len` bytes) under `digest`; true on success.
    pub(crate) fn put(
        &self,
        peer: &Peer,
        digest: &Digest,
        body: Box<dyn Read + Send>,
        len: Option<u64>,
    ) -> bool {
        let body = match len {
            Some(n) => Body::sized(body, n),
            None => Body::new(body),
        };
        self.client
            .put(peer.blob_url(digest))
            .body(body)
            .send()
            .is_ok_and(|r| r.status().is_success())
    }
}
