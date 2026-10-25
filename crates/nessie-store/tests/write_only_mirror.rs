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
