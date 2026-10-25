//! `write_only` acknowledges a write only after R digest-verified copies exist
//! (`[cas] replication_factor`, #139). In-process nodes on loopback; no wall-clock.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use nessie_backend_core::{BackendError, CasBackend as _, Digest, PeerId};
use nessie_store::cas_node::CasNode;
use nessie_store::cas_routes::peer_routes;
use nessie_store::config::{CasConfig, CasMode, CasPeer};

struct Daemon {
    url: String,
    node: Arc<CasNode>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

fn reserve() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

/// A URL nothing listens on.
fn dead_url() -> String {
    reserve().1
}

impl Daemon {
    fn serve(listener: TcpListener, url: String, cfg: &CasConfig) -> Self {
        let node = Arc::new(CasNode::from_config(cfg, PeerId::new(&url)).unwrap());
        let app = peer_routes(node.clone());
        let (stop, rx) = channel::<()>();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let l = tokio::net::TcpListener::from_std(listener).unwrap();
                    axum::serve(l, app)
                        .with_graceful_shutdown(async move {
                            let _ = tokio::task::spawn_blocking(move || rx.recv()).await;
                        })
                        .await
                        .unwrap();
                });
        });
        Self {
            url,
            node,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn start(cfg: &CasConfig) -> Self {
        let (l, url) = reserve();
        Self::serve(l, url, cfg)
    }

    fn kill(&mut self) {
        drop(self.stop.take());
        self.thread.take().unwrap().join().unwrap();
    }
}

fn peer(url: &str) -> CasPeer {
    CasPeer {
        url: url.to_owned(),
        durable: true,
    }
}

fn node_cfg(mode: CasMode, r: Option<usize>, peers: &[&str]) -> CasConfig {
    CasConfig {
        mode,
        byte_budget: Some(1 << 20),
        gc_grace_secs: 0,
        peer_api: true,
        replication_factor: r,
        peers: peers.iter().map(|u| peer(u)).collect(),
        ..CasConfig::default()
    }
}

fn wo(r: usize, peers: &[&str]) -> CasConfig {
    node_cfg(CasMode::WriteOnly, Some(r), peers)
}

fn underreplicated(r: Result<Digest, BackendError>) -> (usize, usize) {
    match r {
        Err(BackendError::Underreplicated {
            confirmed,
            required,
        }) => (confirmed, required),
        other => panic!("expected Underreplicated, got {other:?}"),
    }
}

#[test]
fn put_acks_only_after_a_peer_holds_the_blob() {
    let mut peer_d = Daemon::start(&wo(1, &[]));
    let a = Daemon::start(&wo(2, &[&peer_d.url]));
    let d = a.node.put(&mut &b"mirrored"[..]).unwrap();
    assert!(peer_d.node.has(&d).unwrap(), "peer holds it at ack time");
    assert!(a.node.has(&d).unwrap());

    // keyed path (S3 flat keys / REAPI) is mirrored too
    let d2 = Digest::compute(b"keyed");
    a.node.put_keyed(&d2, &mut &b"keyed"[..]).unwrap();
    assert!(peer_d.node.has(&d2).unwrap());

    // peer gone before the push: the write fails, the local copy stays
    peer_d.kill();
    let r = a.node.put(&mut &b"after the peer died"[..]);
    assert_eq!(underreplicated(r), (0, 1));
    assert!(
        a.node
            .has(&Digest::compute(b"after the peer died"))
            .unwrap()
    );
}

#[test]
fn r3_with_one_peer_down_fails_with_the_true_count_and_keeps_the_local_copy() {
    let up = Daemon::start(&wo(1, &[]));
    let down = dead_url();
    let a = Daemon::start(&wo(3, &[&up.url, &down]));
    let r = a.node.put(&mut &b"two peers wanted"[..]);
    assert_eq!(underreplicated(r), (1, 2));
    assert!(a.node.has(&Digest::compute(b"two peers wanted")).unwrap());
}

#[test]
fn an_lru_peer_does_not_count_toward_r() {
    let lru = Daemon::start(&node_cfg(CasMode::Lru, None, &[]));
    let a = Daemon::start(&wo(2, &[&lru.url]));
    let r = a.node.put(&mut &b"lru is not a mirror"[..]);
    assert_eq!(underreplicated(r), (0, 1));
}

#[test]
fn a_peer_put_writes_locally_and_does_not_fan_out_again() {
    // B's only peer is dead: if B's peer-PUT fanned out it would fail, and A's
    // write would not be acknowledged.
    let b = Daemon::start(&wo(2, &[&dead_url()]));
    let a = Daemon::start(&wo(2, &[&b.url]));
    let d = a.node.put(&mut &b"one hop only"[..]).unwrap();
    assert!(b.node.has(&d).unwrap());
}

#[test]
fn write_only_refuses_to_start_when_r_exceeds_peers() {
    let err = CasNode::from_config(&wo(3, &[&dead_url()]), PeerId::new("me"))
        .err()
        .expect("R-1 > peers must refuse");
    assert!(err.to_string().contains("replication_factor"), "{err}");
    // default R = 2 with no peers refuses too
    let dflt = node_cfg(CasMode::WriteOnly, None, &[]);
    assert!(CasNode::from_config(&dflt, PeerId::new("me")).is_err());
    // R = 1 is a legal single-node archive
    assert!(CasNode::from_config(&wo(1, &[]), PeerId::new("me")).is_ok());
}

#[test]
fn replication_factor_is_rejected_in_lru() {
    let cfg = node_cfg(CasMode::Lru, Some(2), &[]);
    let err = CasNode::from_config(&cfg, PeerId::new("me"))
        .err()
        .expect("lru + replication_factor must fail");
    assert!(err.to_string().contains("replication_factor"), "{err}");
}

/// Bring a node up on the address a dead URL named, so "the peer comes back".
fn start_at(url: &str, cfg: &CasConfig) -> Daemon {
    let addr = url.trim_start_matches("http://");
    let listener = TcpListener::bind(addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    Daemon::serve(listener, url.to_owned(), cfg)
}

fn faces_cfg(cas: CasConfig) -> nessie_store::config::Config {
    nessie_store::config::Config {
        s3: Some(nessie_store::config::S3ServerConfig {
            enabled: true,
            secret_key: "not-the-default".to_owned(),
            ..Default::default()
        }),
        cas: Some(cas),
        ..Default::default()
    }
}

/// #145 review: the startup probe must not wait on peers, or two mirrored nodes
/// with no listener up yet could never start.
#[test]
fn a_cold_mirrored_cluster_builds_its_faces() {
    for _ in 0..2 {
        let cfg = faces_cfg(wo(2, &[&dead_url()]));
        nessie_store::faces::build_faces(&cfg).expect("probe is local-only");
    }
}

/// #145 review: a retry's ByteStream status re-pushes via `ensure_replicated`.
#[test]
fn ensure_replicated_repushes_once_the_peer_is_back() {
    let peer_url = dead_url();
    let a = Daemon::start(&wo(2, &[&peer_url]));
    let d = Digest::compute(b"late peer");
    assert_eq!(underreplicated(a.node.put(&mut &b"late peer"[..])), (0, 1));
    assert!(a.node.ensure_replicated(&d).is_err());
    let peer = start_at(&peer_url, &wo(1, &[]));
    a.node.ensure_replicated(&d).unwrap();
    assert!(peer.node.has(&d).unwrap());
    // not held here at all: nothing to replicate
    assert!(matches!(
        a.node.ensure_replicated(&Digest::compute(b"absent")),
        Err(BackendError::BlobNotFound(_))
    ));
}

/// The real S3 face over the shared backend `build_faces` wires.
mod s3 {
    use super::*;
    use nessie_backend_core::DigestAlgo;
    use s3s::S3;
    use s3s::S3Request;
    use s3s::dto::{PutObjectInput, StreamingBlob};

    const BLOB: &[u8] = b"mirrored through the S3 face";

    fn req<T>(input: T) -> S3Request<T> {
        S3Request {
            input,
            method: http::Method::GET,
            uri: http::Uri::from_static("/"),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    async fn put(face: &nessie_s3::NessieS3) -> Result<(), s3s::S3Error> {
        let d = Digest::compute_with(DigestAlgo::Sha256, BLOB).to_string();
        let hash = d.rsplit_once(':').unwrap().1.to_owned();
        let body = bytes::Bytes::from_static(BLOB);
        face.put_object(req(PutObjectInput {
            bucket: "cache".to_owned(),
            key: format!("cas/{}/{hash}", &hash[..2]),
            body: Some(StreamingBlob::wrap(futures::stream::once(async move {
                Ok::<_, std::io::Error>(body)
            }))),
            ..Default::default()
        }))
        .await
        .map(|_| ())
    }

    #[tokio::test]
    async fn s3_put_is_acked_only_after_the_peer_holds_it_and_retries_recover() {
        let digest = Digest::compute_with(DigestAlgo::Sha256, BLOB);
        let peer_url = dead_url();

        let cfg = faces_cfg(wo(2, &[&peer_url]));
        let faces = nessie_store::faces::build_faces(&cfg).unwrap();
        let node = faces.node.clone().unwrap();
        let (backend, _signer) = faces.backend.unwrap();
        let face = nessie_s3::NessieS3::new(backend);

        // Peer down: 503, local copy retained.
        let err = put(&face).await.unwrap_err();
        assert_eq!(err.status_code().map(|s| s.as_u16()), Some(503));
        assert!(node.has(&digest).unwrap(), "local copy kept");

        // Peer up: the same PUT is acknowledged, and the peer holds the blob.
        let peer = start_at(&peer_url, &wo(1, &[]));
        put(&face).await.expect("retry succeeds");
        assert!(peer.node.has(&digest).unwrap(), "peer holds it at ack time");
    }
}
