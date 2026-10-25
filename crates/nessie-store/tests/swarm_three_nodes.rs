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
    fn start(cfg: &CasConfig, name: &str) -> Self {
        let node = Arc::new(CasNode::from_config(cfg, PeerId::new(name)).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
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
    assert!(!a.node.has(&digest).unwrap(), "evicted from the cache node");
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
