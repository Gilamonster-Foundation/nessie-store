//! Test-only stub peer: a map of `algo:hex` → bytes behind `/cas/v1/blobs/:d`.
//! Bytes are stored as given (no verification), so tests can plant a tampered body.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;

pub(crate) type Blobs = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Blob map plus the `x-nessie-mode` this stub reports on HEAD.
#[derive(Clone)]
struct Stub {
    blobs: Blobs,
    mode: &'static str,
}

async fn get_blob(State(s): State<Stub>, Path(d): Path<String>) -> Result<Vec<u8>, StatusCode> {
    s.blobs
        .lock()
        .unwrap()
        .get(&d)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

async fn head_blob(
    State(s): State<Stub>,
    Path(d): Path<String>,
) -> Result<[(&'static str, String); 2], StatusCode> {
    let n = s
        .blobs
        .lock()
        .unwrap()
        .get(&d)
        .map(Vec::len)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok([
        ("content-length", n.to_string()),
        ("x-nessie-mode", s.mode.to_owned()),
    ])
}

async fn put_blob(State(s): State<Stub>, Path(d): Path<String>, body: Bytes) -> StatusCode {
    s.blobs.lock().unwrap().insert(d, body.to_vec());
    StatusCode::CREATED
}

/// Serve a fresh `write_only` stub on loopback; returns its base URL and blob map.
pub(crate) fn spawn() -> (String, Blobs) {
    spawn_mode("write_only")
}

/// As [`spawn`], reporting `mode` in `x-nessie-mode`.
pub(crate) fn spawn_mode(mode: &'static str) -> (String, Blobs) {
    let blobs = Blobs::default();
    let app = Router::new()
        .route(
            "/cas/v1/blobs/:d",
            get(get_blob).head(head_blob).put(put_blob),
        )
        .with_state(Stub {
            blobs: blobs.clone(),
            mode,
        });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, app).await.unwrap();
            });
    });
    (format!("http://{addr}"), blobs)
}
