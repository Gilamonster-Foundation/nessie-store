//! The peer wire: `GET|HEAD|PUT /cas/v1/blobs/{digest}` over the node's store.
//!
//! The digest segment is the `algo:hex` text form (`blake3:ab12…`), colon
//! unescaped; a percent-encoded colon decodes to the same thing. Mounted by the
//! daemon only when `[cas] peer_api = true`.
//!
//! **Lab only: unauthenticated; mTLS is the fast-follow.** Integrity does not rest
//! on the wire — `get` and `put_keyed` verify every byte against the digest.

use std::io::{Cursor, Read};
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use nessie_backend_core::{BackendError, CasBackend, Digest};

use crate::cas_node::CasNode;

/// Largest blob accepted over `PUT` (bodies are buffered, then verified).
// ponytail: buffered, not streamed; stream via SyncIoBridge when blobs outgrow RAM.
const MAX_BLOB: usize = 256 * 1024 * 1024;

/// The peer routes, served from `node`'s store (never a new one).
pub fn peer_routes(node: Arc<CasNode>) -> Router {
    Router::new()
        .route(
            "/cas/v1/blobs/:digest",
            get(get_blob).head(head_blob).put(put_blob),
        )
        .layer(DefaultBodyLimit::max(MAX_BLOB))
        .with_state(node)
}

fn parse(d: &str) -> Result<Digest, StatusCode> {
    d.parse().map_err(|_| StatusCode::BAD_REQUEST)
}

fn status(e: &BackendError) -> StatusCode {
    match e {
        BackendError::BlobNotFound(_) => StatusCode::NOT_FOUND,
        BackendError::InvalidArgument(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, BackendError> + Send + 'static,
) -> Result<T, StatusCode> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|e| status(&e))
}

/// The serving node's retention mode, so a peer can verify a `durable` claim.
const MODE_HEADER: &str = "x-nessie-mode";

async fn get_blob(
    State(node): State<Arc<CasNode>>,
    Path(d): Path<String>,
) -> Result<([(&'static str, &'static str); 1], Vec<u8>), StatusCode> {
    let digest = parse(&d)?;
    let mode = node.mode_name();
    let body = blocking(move || {
        let mut out = Vec::new();
        node.local()
            .get(&digest)?
            .read_to_end(&mut out)
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(out)
    })
    .await?;
    Ok(([(MODE_HEADER, mode)], body))
}

/// Local-only, with the stored length in `Content-Length` so a peer can answer a
/// size probe without fetching the body, and the node's mode in `x-nessie-mode`.
async fn head_blob(
    State(node): State<Arc<CasNode>>,
    Path(d): Path<String>,
) -> Result<[(&'static str, String); 2], StatusCode> {
    let digest = parse(&d)?;
    let mode = node.mode_name();
    match blocking(move || node.local().size(&digest)).await? {
        Some(n) => Ok([
            ("content-length", n.to_string()),
            (MODE_HEADER, mode.to_owned()),
        ]),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn put_blob(
    State(node): State<Arc<CasNode>>,
    Path(d): Path<String>,
    body: Bytes,
) -> StatusCode {
    let Ok(digest) = parse(&d) else {
        return StatusCode::BAD_REQUEST;
    };
    match blocking(move || node.put_keyed(&digest, &mut Cursor::new(body))).await {
        Ok(()) => StatusCode::CREATED,
        Err(s) => s,
    }
}
