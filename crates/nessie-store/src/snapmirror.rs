//! SnapMirror relationships + transfers + cluster peers.
//!
//! Relationship/peer/transfer bookkeeping is daemon state (an in-memory
//! [`SnapMirrorStore`]); the substrate is touched only at transfer time, when a
//! transfer creates a source snapshot. The actual cross-instance byte movement
//! (binary `zfs send` → HTTP → `zfs receive`) is the live-only data plane; the
//! control surface here records the transfer as ONTAP's synchronous job does.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use nessie_backend_core::{BackendError, SnapshotBackend};

use crate::state::AppState;

/// A cluster peer.
#[derive(Debug, Clone)]
struct Peer {
    uuid: String,
    name: String,
    address: String,
    /// Shared replication token: a sender presents it on the destination's
    /// `/internal/snapmirror/receive`, and the destination validates it against
    /// this peer record. Both sides of a pair hold the same token.
    token: String,
}

/// A SnapMirror relationship.
#[derive(Debug, Clone)]
struct Relationship {
    uuid: String,
    source_path: String,
    destination_path: String,
    peer_uuid: String,
    policy: String,
    state: String,
    healthy: bool,
}

/// A completed SnapMirror transfer record.
#[derive(Debug, Clone)]
struct Transfer {
    uuid: String,
    state: String,
    bytes_transferred: u64,
    snapshot: String,
    end_time: String,
}

#[derive(Default)]
struct Inner {
    peers: HashMap<String, Peer>,
    relationships: HashMap<String, Relationship>,
    transfers: HashMap<String, Vec<Transfer>>,
}

/// In-memory store for peers + relationships (daemon state; backend-agnostic).
#[derive(Default)]
pub struct SnapMirrorStore {
    inner: Mutex<Inner>,
}

impl SnapMirrorStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("snapmirror store mutex poisoned")
    }

    /// True if `token` matches any registered peer's replication token
    /// (constant-time). Gates the internal receive endpoint.
    fn peer_token_valid(&self, token: &str) -> bool {
        let g = self.lock();
        g.peers
            .values()
            .any(|p| crate::auth::ct_eq(p.token.as_bytes(), token.as_bytes()))
    }
}

fn svm_of(path: &str) -> &str {
    path.split_once(':').map_or(path, |(svm, _)| svm)
}

/// The volume component of an ONTAP `svm:vol` path.
fn vol_of(path: &str) -> &str {
    path.split_once(':').map_or(path, |(_, vol)| vol)
}

fn ontap_error(status: StatusCode, code: &str, target: &str, message: String) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message, "target": target } })),
    )
        .into_response()
}

// ---- cluster peers ---------------------------------------------------------

fn peer_obj(p: &Peer) -> Value {
    json!({
        "uuid": p.uuid,
        "name": p.name,
        "ip_address": p.address,
        "status": { "state": "available" },
        // The replication passphrase is returned so the operator can configure the
        // matching peer on the other instance (this is a homelab ONTAP sim, not a
        // secrets vault; the token gates only the internal receive endpoint).
        "authentication": { "passphrase": p.token },
        "_links": { "self": { "href": format!("/api/cluster/peers/{}", p.uuid) } },
    })
}

async fn list_peers(State(s): State<AppState>) -> Json<Value> {
    let g = s.snapmirror.lock();
    let records: Vec<Value> = g.peers.values().map(peer_obj).collect();
    Json(json!({
        "records": records,
        "num_records": g.peers.len(),
        "_links": { "self": { "href": "/api/cluster/peers" } },
    }))
}

async fn get_peer(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    match s.snapmirror.lock().peers.get(&uuid) {
        Some(p) => Json(peer_obj(p)).into_response(),
        None => ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "peer",
            format!("peer {uuid} not found"),
        ),
    }
}

async fn create_peer(State(s): State<AppState>, Json(body): Json<Value>) -> Response {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // Accept either `ip_address` or the ONTAP `remote.ip_addresses[0]` shape.
    let address = body
        .get("ip_address")
        .and_then(Value::as_str)
        .or_else(|| {
            body.get("remote")
                .and_then(|r| r.get("ip_addresses"))
                .and_then(|a| a.get(0))
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .to_string();
    if name.is_empty() || address.is_empty() {
        return ontap_error(
            StatusCode::BAD_REQUEST,
            "400",
            "peer",
            "peer name and ip_address are required".into(),
        );
    }
    // Accept a caller-supplied passphrase (to match the other side of the pair),
    // else mint one and return it in the response.
    let token = body
        .get("authentication")
        .and_then(|a| a.get("passphrase"))
        .and_then(Value::as_str)
        .or_else(|| body.get("passphrase").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let uuid = Uuid::new_v4().to_string();
    let peer = Peer {
        uuid: uuid.clone(),
        name,
        address,
        token,
    };
    let obj = peer_obj(&peer);
    s.snapmirror.lock().peers.insert(uuid, peer);
    (StatusCode::CREATED, Json(obj)).into_response()
}

async fn delete_peer(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    let mut g = s.snapmirror.lock();
    if !g.peers.contains_key(&uuid) {
        return ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "peer",
            format!("peer {uuid} not found"),
        );
    }
    if g.relationships.values().any(|r| r.peer_uuid == uuid) {
        return ontap_error(
            StatusCode::CONFLICT,
            "409",
            "peer",
            "peer is referenced by a SnapMirror relationship".into(),
        );
    }
    g.peers.remove(&uuid);
    Json(json!({ "job": { "uuid": Uuid::new_v4().to_string(), "state": "success" } }))
        .into_response()
}

// ---- snapmirror relationships ---------------------------------------------

fn relationship_obj(r: &Relationship, cluster_name: &str, cluster_uuid: &str) -> Value {
    json!({
        "uuid": r.uuid,
        "source": {
            "path": r.source_path,
            "svm": { "name": svm_of(&r.source_path) },
            "cluster": { "name": cluster_name, "uuid": cluster_uuid },
        },
        "destination": {
            "path": r.destination_path,
            "svm": { "name": svm_of(&r.destination_path) },
        },
        "state": r.state,
        "healthy": r.healthy,
        "policy": { "name": r.policy, "type": "async" },
        "transfer": { "state": "idle" },
        "_links": { "self": { "href": format!("/api/snapmirror/relationships/{}", r.uuid) } },
    })
}

async fn list_relationships(State(s): State<AppState>) -> Json<Value> {
    let g = s.snapmirror.lock();
    let records: Vec<Value> = g
        .relationships
        .values()
        .map(|r| relationship_obj(r, &s.config.cluster_name, &s.identity.cluster_uuid))
        .collect();
    Json(json!({
        "records": records,
        "num_records": g.relationships.len(),
        "_links": { "self": { "href": "/api/snapmirror/relationships" } },
    }))
}

async fn get_relationship(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    let g = s.snapmirror.lock();
    match g.relationships.get(&uuid) {
        Some(r) => Json(relationship_obj(
            r,
            &s.config.cluster_name,
            &s.identity.cluster_uuid,
        ))
        .into_response(),
        None => ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "snapmirror",
            format!("relationship {uuid} not found"),
        ),
    }
}

async fn create_relationship(State(s): State<AppState>, Json(body): Json<Value>) -> Response {
    let source_path = body
        .get("source")
        .and_then(|x| x.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let destination_path = body
        .get("destination")
        .and_then(|x| x.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if source_path.is_empty() || destination_path.is_empty() {
        return ontap_error(
            StatusCode::BAD_REQUEST,
            "400",
            "snapmirror",
            "source.path and destination.path are required".into(),
        );
    }
    let policy = body
        .get("policy")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("MirrorAllSnapshots")
        .to_string();

    let mut g = s.snapmirror.lock();
    // Resolve the peer by source.cluster.name, else the first registered peer.
    let wanted_cluster = body
        .get("source")
        .and_then(|x| x.get("cluster"))
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str);
    let peer_uuid = wanted_cluster
        .and_then(|name| {
            g.peers
                .values()
                .find(|p| p.name == name)
                .map(|p| p.uuid.clone())
        })
        .or_else(|| g.peers.values().next().map(|p| p.uuid.clone()))
        .unwrap_or_default();

    let uuid = Uuid::new_v4().to_string();
    let rel = Relationship {
        uuid: uuid.clone(),
        source_path,
        destination_path,
        peer_uuid,
        policy,
        state: "snapmirrored".to_string(),
        healthy: true,
    };
    let obj = relationship_obj(&rel, &s.config.cluster_name, &s.identity.cluster_uuid);
    g.relationships.insert(uuid, rel);
    (
        StatusCode::CREATED,
        Json(json!({
            "job": { "uuid": Uuid::new_v4().to_string(), "_links": { "self": { "href": "/api/cluster/jobs/x" } } },
            "record": obj,
            "num_records": 1,
        })),
    )
        .into_response()
}

async fn patch_relationship(
    State(s): State<AppState>,
    Path(uuid): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let mut g = s.snapmirror.lock();
    let (cluster_name, cluster_uuid) = (
        s.config.cluster_name.clone(),
        s.identity.cluster_uuid.clone(),
    );
    let Some(rel) = g.relationships.get_mut(&uuid) else {
        return ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "snapmirror",
            format!("relationship {uuid} not found"),
        );
    };
    if let Some(state) = body.get("state").and_then(Value::as_str) {
        rel.state = state.to_string();
        rel.healthy = state != "broken_off";
    }
    if let Some(policy) = body
        .get("policy")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
    {
        rel.policy = policy.to_string();
    }
    Json(relationship_obj(rel, &cluster_name, &cluster_uuid)).into_response()
}

async fn delete_relationship(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    let mut g = s.snapmirror.lock();
    if g.relationships.remove(&uuid).is_none() {
        return ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "snapmirror",
            format!("relationship {uuid} not found"),
        );
    }
    g.transfers.remove(&uuid); // cascade
    Json(json!({ "job": { "uuid": Uuid::new_v4().to_string(), "state": "success" } }))
        .into_response()
}

fn transfer_obj(rel: &str, t: &Transfer) -> Value {
    json!({
        "uuid": t.uuid,
        "state": t.state,
        "bytes_transferred": t.bytes_transferred,
        "end_time": t.end_time,
        "snapshot": t.snapshot,
        "_links": { "self": { "href": format!("/api/snapmirror/relationships/{rel}/transfers/{}", t.uuid) } },
    })
}

async fn list_transfers(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    let g = s.snapmirror.lock();
    if !g.relationships.contains_key(&uuid) {
        return ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "snapmirror",
            format!("relationship {uuid} not found"),
        );
    }
    let records: Vec<Value> = g
        .transfers
        .get(&uuid)
        .map(|v| v.iter().map(|t| transfer_obj(&uuid, t)).collect())
        .unwrap_or_default();
    Json(json!({
        "records": records,
        "num_records": records.len(),
        "_links": { "self": { "href": format!("/api/snapmirror/relationships/{uuid}/transfers") } },
    }))
    .into_response()
}

/// Trigger an on-demand transfer: snapshot the source, then record success.
///
/// The source snapshot is real (created via the snapshot backend); the
/// cross-instance byte movement (binary `zfs send`/`receive`) is the live-only
/// data plane. This matches ONTAP's contract: the work is done before the job
/// envelope returns.
async fn create_transfer(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    let (source_path, seq) = {
        let g = s.snapmirror.lock();
        match g.relationships.get(&uuid) {
            Some(_) => {
                let seq = g.transfers.get(&uuid).map_or(0, Vec::len) + 1;
                (g.relationships[&uuid].source_path.clone(), seq)
            }
            None => {
                return ontap_error(
                    StatusCode::NOT_FOUND,
                    "404",
                    "snapmirror",
                    format!("relationship {uuid} not found"),
                );
            }
        }
    };

    let vol_name = vol_of(&source_path).to_string();
    let rel8: String = uuid.chars().take(8).collect();
    let snap_name = format!("snapmirror.{rel8}.{seq}");

    // Create the source snapshot off the executor.
    let backend = s.backend.clone();
    let snap_for_closure = snap_name.clone();
    let result = crate::blocking::run(move || {
        let b = backend
            .as_snapshot()
            .ok_or(BackendError::FeatureNotSupported {
                capability: "snapshots",
            })?;
        let vol = b
            .list_volumes()?
            .into_iter()
            .find(|v| v.name == vol_name)
            .ok_or_else(|| {
                BackendError::InvalidArgument(format!("source volume {vol_name:?} not found"))
            })?;
        b.create_snapshot(&vol.uuid, &snap_for_closure)?;
        Ok::<(), BackendError>(())
    })
    .await;

    let mut g = s.snapmirror.lock();
    let transfer = match result {
        Ok(()) => {
            if let Some(rel) = g.relationships.get_mut(&uuid) {
                rel.state = "snapmirrored".to_string();
                rel.healthy = true;
            }
            Transfer {
                uuid: Uuid::new_v4().to_string(),
                state: "success".to_string(),
                bytes_transferred: 0,
                snapshot: snap_name,
                end_time: now_rfc3339(),
            }
        }
        Err(e) => {
            if let Some(rel) = g.relationships.get_mut(&uuid) {
                rel.healthy = false;
            }
            let t = Transfer {
                uuid: Uuid::new_v4().to_string(),
                state: "failed".to_string(),
                bytes_transferred: 0,
                snapshot: String::new(),
                end_time: now_rfc3339(),
            };
            g.transfers.entry(uuid.clone()).or_default().push(t);
            return ontap_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "500",
                "snapmirror",
                format!("transfer failed: {}", e.0),
            );
        }
    };
    let record = transfer_obj(&uuid, &transfer);
    g.transfers.entry(uuid).or_default().push(transfer);
    (
        StatusCode::CREATED,
        Json(json!({
            "job": { "uuid": Uuid::new_v4().to_string() },
            "record": record,
            "num_records": 1,
        })),
    )
        .into_response()
}

/// `POST /internal/snapmirror/receive` — NOT an ONTAP path. The peer data-plane
/// endpoint: authenticate the sending peer by its replication token, then apply
/// the replication stream to the destination volume via the backend's
/// [`ReplicationBackend`](nessie_backend_core::ReplicationBackend). Bypasses ONTAP
/// Basic auth (see [`crate::auth`]); the token is the credential here.
async fn internal_receive(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(token) = headers
        .get("x-replication-token")
        .and_then(|v| v.to_str().ok())
    else {
        return ontap_error(
            StatusCode::UNAUTHORIZED,
            "401",
            "snapmirror",
            "X-Replication-Token header is required".into(),
        );
    };
    if !s.snapmirror.peer_token_valid(token) {
        return ontap_error(
            StatusCode::UNAUTHORIZED,
            "401",
            "snapmirror",
            "invalid replication token".into(),
        );
    }
    let Some(dest) = headers
        .get("x-destination-volume")
        .and_then(|v| v.to_str().ok())
    else {
        return ontap_error(
            StatusCode::BAD_REQUEST,
            "400",
            "snapmirror",
            "X-Destination-Volume header is required".into(),
        );
    };
    if body.is_empty() {
        return ontap_error(
            StatusCode::BAD_REQUEST,
            "400",
            "snapmirror",
            "empty replication stream".into(),
        );
    }

    let dest = dest.to_string();
    let backend = s.backend.clone();
    let result = crate::blocking::run(move || {
        let repl = backend
            .as_snapshot()
            .and_then(SnapshotBackend::as_replication)
            .ok_or(BackendError::FeatureNotSupported {
                capability: "replication",
            })?;
        let mut reader = body.as_ref();
        let applied = repl.receive_stream(&dest, &mut reader)?;
        Ok::<(String, u64), BackendError>((dest, applied))
    })
    .await;

    match result {
        Ok((dest, applied)) => Json(json!({
            "status": "received",
            "bytes": applied,
            "destination": dest,
        }))
        .into_response(),
        Err(e) => ontap_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "500",
            "snapmirror",
            format!("receive failed: {}", e.0),
        ),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// SnapMirror + cluster-peer routes (auth + state applied by [`crate::app`]).
pub fn snapmirror_routes() -> Router<AppState> {
    Router::new()
        .route("/api/cluster/peers", get(list_peers).post(create_peer))
        .route(
            "/api/cluster/peers/:uuid",
            get(get_peer).delete(delete_peer),
        )
        .route(
            "/api/snapmirror/relationships",
            get(list_relationships).post(create_relationship),
        )
        .route(
            "/api/snapmirror/relationships/:uuid",
            get(get_relationship)
                .patch(patch_relationship)
                .delete(delete_relationship),
        )
        .route(
            "/api/snapmirror/relationships/:uuid/transfers",
            get(list_transfers).post(create_transfer),
        )
        .route("/internal/snapmirror/receive", post(internal_receive))
}
