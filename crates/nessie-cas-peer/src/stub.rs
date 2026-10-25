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

async fn get_blob(State(b): State<Blobs>, Path(d): Path<String>) -> Result<Vec<u8>, StatusCode> {
    b.lock()
        .unwrap()
        .get(&d)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

async fn head_blob(
    State(b): State<Blobs>,
    Path(d): Path<String>,
) -> Result<[(&'static str, String); 1], StatusCode> {
    let n = b
        .lock()
        .unwrap()
        .get(&d)
        .map(Vec::len)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok([("content-length", n.to_string())])
}

async fn put_blob(State(b): State<Blobs>, Path(d): Path<String>, body: Bytes) -> StatusCode {
    b.lock().unwrap().insert(d, body.to_vec());
    StatusCode::CREATED
}

/// Serve a fresh stub on loopback; returns its base URL and its blob map.
pub(crate) fn spawn() -> (String, Blobs) {
    let blobs = Blobs::default();
    let app = Router::new()
        .route(
            "/cas/v1/blobs/:d",
            get(get_blob).head(head_blob).put(put_blob),
        )
        .with_state(blobs.clone());
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
