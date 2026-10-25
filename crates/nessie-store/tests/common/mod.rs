//! Shared in-process daemon harness for the multi-node CAS tests.
#![allow(dead_code)] // each test crate uses a different subset

use std::net::TcpListener;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nessie_backend_core::PeerId;
use nessie_store::cas_node::CasNode;
use nessie_store::cas_routes::peer_routes;
use nessie_store::config::{CasConfig, CasMode, CasPeer};

pub struct Daemon {
    pub url: String,
    pub node: Arc<CasNode>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// Ports handed out as "dead" URLs. The kernel recycles a dropped listener's port at
/// once, so without this a parallel test's live node could land on another test's
/// dead peer and answer it.
static DEAD_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

fn bind_fresh(dead: bool) -> (TcpListener, String) {
    loop {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut dead_ports = DEAD_PORTS.lock().unwrap();
        if dead_ports.contains(&addr.port()) {
            continue;
        }
        if dead {
            dead_ports.push(addr.port());
        }
        listener.set_nonblocking(true).unwrap();
        return (listener, format!("http://{addr}"));
    }
}

pub fn reserve() -> (TcpListener, String) {
    bind_fresh(false)
}

/// A URL nothing listens on (until a test calls [`start_at`]).
pub fn dead_url() -> String {
    bind_fresh(true).1
}

impl Daemon {
    pub fn serve(listener: TcpListener, url: String, cfg: &CasConfig) -> Self {
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

    pub fn start(cfg: &CasConfig) -> Self {
        let (l, url) = reserve();
        Self::serve(l, url, cfg)
    }

    pub fn kill(&mut self) {
        drop(self.stop.take());
        self.thread.take().unwrap().join().unwrap();
    }
}

pub fn peer(url: &str) -> CasPeer {
    CasPeer {
        url: url.to_owned(),
        durable: true,
    }
}

pub fn node_cfg(mode: CasMode, r: Option<usize>, peers: &[&str]) -> CasConfig {
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

pub fn wo(r: usize, peers: &[&str]) -> CasConfig {
    node_cfg(CasMode::WriteOnly, Some(r), peers)
}

/// Bring a node up on the address a dead URL named, so "the peer comes back".
pub fn start_at(url: &str, cfg: &CasConfig) -> Daemon {
    let addr = url.trim_start_matches("http://");
    let listener = TcpListener::bind(addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    Daemon::serve(listener, url.to_owned(), cfg)
}
