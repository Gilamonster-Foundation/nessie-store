//! Pending restores draw on the scrub budget and never fetch what the capacity gate
//! would refuse (#140 round 3). A counting stub peer shows what crossed the wire.

mod common;

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::get;
use common::{Daemon, dead_url, wo};
use nessie_backend_core::CasBackend as _;
use nessie_store::config::{CasBackendKind, CasConfig};

const N: usize = 16;

#[derive(Default)]
struct Stub {
    blobs: HashMap<String, Vec<u8>>,
    gets: AtomicUsize,
    heads: AtomicUsize,
}

async fn get_blob(
    State(s): State<Arc<Stub>>,
    UrlPath(d): UrlPath<String>,
) -> Result<Vec<u8>, StatusCode> {
    s.gets.fetch_add(1, Ordering::SeqCst);
    s.blobs.get(&d).cloned().ok_or(StatusCode::NOT_FOUND)
}

async fn head_blob(
    State(s): State<Arc<Stub>>,
    UrlPath(d): UrlPath<String>,
) -> Result<[(&'static str, String); 2], StatusCode> {
    s.heads.fetch_add(1, Ordering::SeqCst);
    let n = s.blobs.get(&d).map(Vec::len).ok_or(StatusCode::NOT_FOUND)?;
    Ok([
        ("content-length", n.to_string()),
        ("x-nessie-mode", "write_only".to_owned()),
    ])
}

/// Serve `stub` at `url` (a `dead_url`) for the rest of the process.
fn serve_stub(url: &str, stub: Arc<Stub>) {
    let listener = TcpListener::bind(url.trim_start_matches("http://")).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let app = axum::Router::new()
                    .route("/cas/v1/blobs/:d", get(get_blob).head(head_blob))
                    .with_state(stub);
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, app).await.unwrap();
            });
    });
}

fn fresh_root() -> PathBuf {
    static C: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nessie-pending-{}-{}",
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn cfg(root: &Path, peer: &str, capacity: usize, budget: u64) -> CasConfig {
    CasConfig {
        backend: CasBackendKind::Disk,
        path: Some(root.to_owned()),
        byte_budget: Some(capacity as u64),
        scrub_bytes_per_pass: budget,
        ..CasConfig {
            peers: vec![common::peer(peer)],
            ..wo(1, &[])
        }
    }
}

/// Three corrupt blobs quarantined with their peer down; returns the stub peer
/// (not yet serving) holding their good bytes.
fn three_pending(root: &Path, peer: &str, capacity: usize) -> Arc<Stub> {
    let mut stub = Stub::default();
    let a = Daemon::start(&cfg(root, peer, capacity, 1 << 30));
    for i in 0..3 {
        let body = format!("blob-{i:0>11}").into_bytes();
        assert_eq!(body.len(), N);
        let d = a.node.put(&mut body.as_slice()).unwrap();
        let hex = d.to_string().split_once(':').unwrap().1.to_owned();
        std::fs::write(
            root.join(d.algo().name()).join(&hex[..2]).join(&hex[2..]),
            [0xAA; N],
        )
        .unwrap();
        stub.blobs.insert(d.to_string(), body);
    }
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(
        (s.corrupt, s.pending_restore),
        (3, 3),
        "peer is down: all stay quarantined"
    );
    Arc::new(stub)
}

#[test]
fn a_full_store_fetches_nothing_for_its_pending_restores() {
    let root = fresh_root();
    let peer = dead_url();
    // 3 quarantined files fill the 3 * N capacity exactly.
    let stub = three_pending(&root, &peer, 3 * N);
    serve_stub(&peer, stub.clone());

    let a = Daemon::start(&cfg(&root, &peer, 3 * N, 1));
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(
        (s.restored, s.pending_restore, s.pending_no_capacity),
        (0, 3, 3)
    );
    assert_eq!(
        stub.gets.load(Ordering::SeqCst),
        0,
        "no body was fetched and discarded"
    );
    assert!(
        stub.heads.load(Ordering::SeqCst) >= 3,
        "sizes were asked for"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn with_room_a_pass_restores_one_budgets_worth_and_the_next_resumes() {
    let root = fresh_root();
    let peer = dead_url();
    let stub = three_pending(&root, &peer, 6 * N);
    serve_stub(&peer, stub.clone());

    // Budget of one blob: each pass restores exactly one pending blob.
    let a = Daemon::start(&cfg(&root, &peer, 6 * N, N as u64));
    for (pass, pending) in [(1, 2), (2, 1), (3, 0)] {
        let s = a.node.maintain_once().unwrap().scrub().clone();
        assert_eq!(s.pending_restore, pending, "pass {pass}");
        assert_eq!(stub.gets.load(Ordering::SeqCst), pass, "one fetch per pass");
    }
    assert_eq!(a.node.store().local_blobs().unwrap().len(), 3);
    let _ = std::fs::remove_dir_all(root);
}
