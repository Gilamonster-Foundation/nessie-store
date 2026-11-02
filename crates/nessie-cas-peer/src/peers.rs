//! The configured peer list and the three HTTP verbs spoken to it.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use nessie_backend_core::{BackendError, Digest};
use reqwest::blocking::{Body, Client, Response};

/// One static peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Base URL, e.g. `http://host:8443` (`https://` under mTLS) (no trailing path).
    pub url: String,
    /// Whether the peer is a durable store of record.
    pub durable: bool,
}

impl Peer {
    fn blob_url(&self, digest: &Digest) -> String {
        format!("{}/cas/v1/blobs/{digest}", self.url.trim_end_matches('/'))
    }
}

/// What a successful `HEAD` tells us.
struct Head {
    size: Option<u64>,
    mode: Option<String>,
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
    pub fn new(peers: Vec<Peer>) -> Result<Self, BackendError> {
        Self::with_tls(peers, None)
    }

    /// As [`new`](Self::new), but with `tls` the client presents the node
    /// certificate and verifies servers with the config's verifier (mTLS). Peer
    /// URLs are then expected to be `https://`.
    ///
    /// # Errors
    ///
    /// [`BackendError::Internal`] if the HTTP client cannot be built.
    pub fn with_tls(
        mut peers: Vec<Peer>,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Self, BackendError> {
        // Distinct holders only: a URL listed twice must not count as two replicas.
        let mut seen = std::collections::HashSet::new();
        peers.retain(|p| seen.insert(p.url.trim_end_matches('/').to_owned()));
        let client = std::thread::spawn(move || {
            let mut b = Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30));
            if let Some(tls) = tls {
                // A redirect (or an `http://` peer) must not leave the mTLS channel.
                // ponytail: one handshake per peer request, so every request re-runs the
                // server verifier (revocation); bind the verified server chain to the
                // connection if peer throughput needs pooling.
                b = b
                    .pool_max_idle_per_host(0)
                    .use_preconfigured_tls((*tls).clone())
                    .https_only(true)
                    .redirect(reqwest::redirect::Policy::none());
            }
            b.build()
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
        self.head(peer, digest)?.size
    }

    /// `HEAD` — does `peer` hold `digest` *and* report `x-nessie-mode: write_only`?
    /// Only a node that never reclaims is a permanent home; the `durable` flag in
    /// our config is a claim about their config, so we ask them.
    pub(crate) fn holds_durably(&self, peer: &Peer, digest: &Digest) -> bool {
        self.head(peer, digest)
            .is_some_and(|h| h.mode.as_deref() == Some("write_only"))
    }

    /// The facts a 2xx `HEAD` carries; `None` on 404 / failure.
    fn head(&self, peer: &Peer, digest: &Digest) -> Option<Head> {
        let r = self.client.head(peer.blob_url(digest)).send().ok()?;
        let text = |name: &str| r.headers().get(name)?.to_str().ok().map(str::to_owned);
        r.status().is_success().then(|| Head {
            // `Response::content_length` is the (empty) HEAD body's; read the header.
            size: text("content-length").and_then(|v| v.parse().ok()),
            mode: text("x-nessie-mode"),
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use nessie_backend_core::Digest;

    use super::{Peer, PeerSet};

    /// A plaintext server that counts connections and would happily answer a peer
    /// request as a durable holder.
    fn counting_http() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = hits.clone();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                h.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-nessie-mode: write_only\r\nconnection: close\r\n\r\nhi",
                );
            }
        });
        (url, hits)
    }

    /// An admitted (trusted-cert) HTTPS server answering everything with `302 → to`.
    /// Returns its URL and a client config that trusts it.
    fn https_redirector(to: String) -> (String, Arc<rustls::ClientConfig>) {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let der = ck.cert.der().clone();
        let key = rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).unwrap();
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(ring())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![der.clone()], key)
                .unwrap(),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let client = Arc::new(
            rustls::ClientConfig::builder_with_provider(ring())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let l = tokio::net::TcpListener::from_std(l).unwrap();
                    let acc = tokio_rustls::TlsAcceptor::from(server);
                    loop {
                        let (tcp, _) = l.accept().await.unwrap();
                        let (acc, to) = (acc.clone(), to.clone());
                        tokio::spawn(async move {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let Ok(mut s) = acc.accept(tcp).await else { return };
                            let mut buf = [0u8; 1024];
                            let _ = s.read(&mut buf).await;
                            let r = format!(
                                "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            );
                            let _ = s.write_all(r.as_bytes()).await;
                            let _ = s.shutdown().await;
                        });
                    }
                });
        });
        (format!("https://localhost:{port}"), client)
    }

    fn ring() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    fn digest() -> Digest {
        Digest::compute(b"hi")
    }

    /// Regression (PR #157 round 2, P1): an admitted HTTPS peer redirecting to
    /// plain HTTP must not be followed — no plaintext request, no holder claim.
    #[test]
    fn https_peer_redirect_to_http_is_not_followed() {
        let (plain, hits) = counting_http();
        let (url, tls) = https_redirector(format!("{plain}/cas/v1/blobs/x"));
        let peers = PeerSet::with_tls(vec![Peer { url, durable: true }], Some(tls)).unwrap();
        let p = &peers.peers()[0];
        assert!(!peers.holds_durably(p, &digest()));
        assert!(peers.get(p, &digest()).is_none());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "plaintext endpoint was contacted"
        );
    }

    /// With TLS configured, a `http://` peer fails closed (never contacted).
    #[test]
    fn tls_mode_refuses_plain_http_peer() {
        let (plain, hits) = counting_http();
        let (_, tls) = https_redirector(String::new());
        let peers = PeerSet::with_tls(
            vec![Peer {
                url: plain,
                durable: true,
            }],
            Some(tls),
        )
        .unwrap();
        let p = &peers.peers()[0];
        assert!(!peers.holds_durably(p, &digest()));
        assert!(peers.get(p, &digest()).is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
