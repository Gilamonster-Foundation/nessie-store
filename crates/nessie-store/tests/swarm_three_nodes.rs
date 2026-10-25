//! Three in-process nodes on loopback: one durable, two caches with a tight budget.
//!
//! A cache node evicts a cold blob only once a durable peer holds it, and restores
//! it from that peer by digest on the next read. Killing a cache node loses nothing
//! it had pushed out. No wall-clock: `gc_grace_secs = 0` and maintenance is driven
//! by hand.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use nessie_backend_core::{CasBackend as _, Digest, PeerId};
use nessie_store::cas_node::{CasNode, MaintenanceReport};
use nessie_store::cas_routes::peer_routes;
use nessie_store::config::{CasConfig, CasMode, CasPeer};

/// A node plus the thread serving its peer API; `kill` stops the server.
struct Daemon {
    url: String,
    node: Arc<CasNode>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Claim a loopback port, so nodes can name each other before they start.
    fn reserve() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    fn start(cfg: &CasConfig, name: &str) -> Self {
        let (listener, url) = Self::reserve();
        Self::serve(listener, url, cfg, name)
    }

    fn serve(listener: TcpListener, url: String, cfg: &CasConfig, name: &str) -> Self {
        let node = Arc::new(CasNode::from_config(cfg, PeerId::new(name)).unwrap());
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

    /// Stop serving: the node is unreachable from here on.
    fn kill(&mut self) {
        drop(self.stop.take());
        self.thread.take().unwrap().join().unwrap();
    }
}

fn durable() -> CasConfig {
    CasConfig {
        peer_api: true,
        gc_grace_secs: 0,
        ..CasConfig::default()
    }
}

fn cache(durable_url: &str) -> CasConfig {
    CasConfig {
        mode: CasMode::Cache,
        byte_budget: Some(1),
        replication_factor: Some(2),
        gc_grace_secs: 0,
        peer_api: true,
        peers: vec![CasPeer {
            url: durable_url.to_owned(),
            durable: true,
        }],
        ..CasConfig::default()
    }
}

fn read(node: &CasNode, d: &Digest) -> Vec<u8> {
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut node.get(d).unwrap(), &mut v).unwrap();
    v
}

/// Two passes: the first pushes the cold blob to the durable peer and keeps it
/// (never drop the last copy); the second sees the durable copy and evicts.
fn evict(node: &CasNode) -> MaintenanceReport {
    node.maintain_once().unwrap();
    node.maintain_once().unwrap()
}

#[test]
fn evicted_blob_is_restored_from_the_durable_node_by_digest() {
    let d = Daemon::start(&durable(), "durable");
    let a = Daemon::start(&cache(&d.url), "cache-a");

    let digest = a
        .node
        .put(&mut &b"cold blob, evicted then restored"[..])
        .unwrap();
    assert!(!d.node.has(&digest).unwrap(), "not yet on the durable node");

    assert_eq!(
        evict(&a.node),
        MaintenanceReport::Evict {
            evicted: 1,
            bytes: 32
        }
    );
    assert!(
        !a.node.local().has(&digest).unwrap(),
        "evicted from the cache node"
    );
    assert!(d.node.has(&digest).unwrap(), "held by the durable node");

    assert_eq!(read(&a.node, &digest), b"cold blob, evicted then restored");
    assert!(
        a.node.has(&digest).unwrap(),
        "restored locally by fetch-through"
    );
}

#[test]
fn killing_a_cache_node_loses_nothing_it_pushed_out() {
    let d = Daemon::start(&durable(), "durable");
    let mut a = Daemon::start(&cache(&d.url), "cache-a");
    let b = Daemon::start(&cache(&d.url), "cache-b");

    let digest = a.node.put(&mut &b"survives its first holder"[..]).unwrap();
    evict(&a.node);
    a.kill();

    assert_eq!(read(&b.node, &digest), b"survives its first holder");
}

#[test]
fn a_blob_only_the_cache_holds_is_never_evicted() {
    let d = Daemon::start(&durable(), "durable");
    let mut cfg = cache(&d.url);
    cfg.peers[0].durable = false; // nobody durable: the cache holds the last copy
    let a = Daemon::start(&cfg, "cache-a");

    let digest = a.node.put(&mut &b"last copy"[..]).unwrap();
    assert_eq!(
        evict(&a.node),
        MaintenanceReport::Evict {
            evicted: 0,
            bytes: 0
        }
    );
    assert!(a.node.has(&digest).unwrap());
}

/// `PeerSet` builds its blocking client off-thread: constructing a node from inside
/// an async runtime (as `serve()` does) must not panic.
#[tokio::test]
async fn a_node_with_peers_builds_inside_an_async_runtime() {
    let cfg = cache("http://127.0.0.1:1");
    CasNode::from_config(&cfg, PeerId::new("async")).unwrap();
}

mod s3 {
    use super::*;
    use futures::TryStreamExt as _;
    use nessie_backend_core::{CasBackend, DigestAlgo};
    use nessie_s3::NessieS3;
    use s3s::S3;
    use s3s::S3Request;
    use s3s::dto::{GetObjectInput, HeadObjectInput, PutObjectInput, StreamingBlob};

    const BLOB: &[u8] = b"restored through the S3 face";

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

    fn key() -> String {
        let d = Digest::compute_with(DigestAlgo::Sha256, BLOB).to_string();
        let hash = d.rsplit_once(':').unwrap().1.to_owned();
        format!("cas/{}/{hash}", &hash[..2])
    }

    /// S3 asks `size` before it reads, so a blob held only by a peer must not look
    /// absent: HEAD and GET on a locally evicted blob both succeed.
    #[tokio::test]
    async fn s3_get_and_head_restore_a_blob_evicted_locally() {
        let d = Daemon::start(&durable(), "durable");
        let a = Daemon::start(&cache(&d.url), "cache-a");
        let face = NessieS3::new(a.node.clone() as Arc<dyn CasBackend>);

        let body = bytes::Bytes::from_static(BLOB);
        face.put_object(req(PutObjectInput {
            bucket: "cache".to_owned(),
            key: key(),
            body: Some(StreamingBlob::wrap(futures::stream::once(async move {
                Ok::<_, std::io::Error>(body)
            }))),
            ..Default::default()
        }))
        .await
        .expect("S3 put");

        let n = a.node.clone();
        let report = tokio::task::spawn_blocking(move || evict(&n))
            .await
            .unwrap();
        assert_eq!(
            report,
            MaintenanceReport::Evict {
                evicted: 1,
                bytes: BLOB.len() as u64
            }
        );

        let head = face
            .head_object(req(HeadObjectInput {
                bucket: "cache".to_owned(),
                key: key(),
                ..Default::default()
            }))
            .await
            .expect("S3 HEAD of an evicted blob");
        assert_eq!(head.output.content_length, Some(BLOB.len() as i64));

        let got = face
            .get_object(req(GetObjectInput {
                bucket: "cache".to_owned(),
                key: key(),
                ..Default::default()
            }))
            .await
            .expect("S3 GET of an evicted blob");
        assert_eq!(got.output.content_length, Some(BLOB.len() as i64));
        let chunks: Vec<bytes::Bytes> = got.output.body.unwrap().try_collect().await.unwrap();
        assert_eq!(chunks.concat(), BLOB);
    }
}

/// Each node lists the other. A miss on both must be a prompt 404, not a probe that
/// bounces between them: peer routes answer from local storage only.
#[test]
fn two_nodes_listing_each_other_do_not_recurse_on_a_miss() {
    let (la, ua) = Daemon::reserve();
    let (lb, ub) = Daemon::reserve();
    let peered = |other: &str| CasConfig {
        peers: vec![CasPeer {
            url: other.to_owned(),
            durable: false,
        }],
        peer_api: true,
        ..CasConfig::default()
    };
    let a = Daemon::serve(la, ua, &peered(&ub), "a");
    let _b = Daemon::serve(lb, ub, &peered(&a.url), "b");

    let missing = Digest::compute(b"held by nobody");
    let started = std::time::Instant::now(); // hang guard only
    assert!(a.node.get(&missing).is_err());
    assert!(!a.node.has(&missing).unwrap());
    assert_eq!(a.node.size(&missing).unwrap(), None);
    assert!(
        started.elapsed().as_secs() < 5,
        "a recursing probe would run to the request timeout"
    );
}
