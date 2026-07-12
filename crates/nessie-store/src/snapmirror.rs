//! SnapMirror relationships + transfers + cluster peers.
//!
//! Relationship/peer/transfer bookkeeping is daemon state (an in-memory
//! [`SnapMirrorStore`]). A transfer takes a real source snapshot and, if the
//! relationship has a reachable peer, streams it (full, or incremental from the
//! relationship's base cursor) to that peer's `/internal/snapmirror/receive`, which
//! applies it via the backend's
//! [`ReplicationBackend`](nessie_backend_core::ReplicationBackend). Peer-to-peer
//! auth is a per-peer replication token; transfers are serialized per relationship.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio_stream::StreamExt as _;
use uuid::Uuid;

use nessie_backend_core::{BackendError, SnapshotBackend, VolumeUuid};

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
    /// The last snapshot successfully transferred to the destination — the common
    /// base for the next incremental send. Per-relationship (a source fanned out to
    /// several peers has an independent cursor for each). `None` until the first
    /// successful transfer.
    base: Option<String>,
    /// Set while a transfer is running, so overlapping POSTs to the same
    /// relationship are rejected (409) instead of colliding on the snapshot name.
    /// ONTAP serializes transfers per relationship; this mirrors that.
    transfer_in_progress: bool,
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
    /// (constant-time). Gates the internal receive endpoint. An empty presented
    /// token — or an empty stored token — never authorizes (defense in depth).
    fn peer_token_valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let g = self.lock();
        g.peers
            .values()
            .filter(|p| !p.token.is_empty())
            .any(|p| crate::auth::ct_eq(p.token.as_bytes(), token.as_bytes()))
    }
}

/// Clears a relationship's `transfer_in_progress` flag when dropped, so a client
/// disconnect, panic, or early return mid-transfer can't wedge the relationship
/// permanently at "in progress".
struct TransferGuard {
    store: Arc<SnapMirrorStore>,
    uuid: String,
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        if let Some(rel) = self.store.lock().relationships.get_mut(&self.uuid) {
            rel.transfer_in_progress = false;
        }
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

/// Peer as returned on reads (list/get). The replication passphrase is a
/// write-capable cross-instance credential and is **not** echoed back.
fn peer_obj(p: &Peer) -> Value {
    json!({
        "uuid": p.uuid,
        "name": p.name,
        "ip_address": p.address,
        "status": { "state": "available" },
        "_links": { "self": { "href": format!("/api/cluster/peers/{}", p.uuid) } },
    })
}

/// Peer as returned on create — includes the passphrase exactly once, so the
/// operator can copy it to the matching peer on the other instance.
fn peer_obj_with_secret(p: &Peer) -> Value {
    let mut obj = peer_obj(p);
    obj["authentication"] = json!({ "passphrase": p.token });
    obj
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
    // else mint one and return it in the response. An empty passphrase is ignored
    // (never stored) so it cannot later authorize a blank token.
    let token = body
        .get("authentication")
        .and_then(|a| a.get("passphrase"))
        .and_then(Value::as_str)
        .or_else(|| body.get("passphrase").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let uuid = Uuid::new_v4().to_string();
    let peer = Peer {
        uuid: uuid.clone(),
        name,
        address,
        token,
    };
    let obj = peer_obj_with_secret(&peer);
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
        base: None,
        transfer_in_progress: false,
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

/// Trigger an on-demand transfer: snapshot the source, then stream it to the peer.
///
/// Creates a real source snapshot, then — if the relationship has a reachable peer
/// — opens the backend's replication stream (full, or incremental from this
/// relationship's base cursor) and POSTs it to the peer's receive endpoint,
/// recording the honest `bytes_transferred` and advancing the base. A relationship
/// with no reachable peer stays control-plane only (snapshot taken, zero bytes).
/// Matches ONTAP's contract: the work is done before the job envelope returns.
async fn create_transfer(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    // Serialize transfers per relationship (as ONTAP does): reject an overlapping
    // POST with 409 rather than colliding on the snapshot name. The guard clears the
    // flag on every exit — including a mid-transfer client disconnect or panic.
    let (source_path, seq) = {
        let mut g = s.snapmirror.lock();
        let Some(rel) = g.relationships.get_mut(&uuid) else {
            return ontap_error(
                StatusCode::NOT_FOUND,
                "404",
                "snapmirror",
                format!("relationship {uuid} not found"),
            );
        };
        if rel.transfer_in_progress {
            return ontap_error(
                StatusCode::CONFLICT,
                "409",
                "snapmirror",
                "a transfer is already in progress for this relationship".into(),
            );
        }
        rel.transfer_in_progress = true;
        let source_path = rel.source_path.clone();
        let seq = g.transfers.get(&uuid).map_or(0, Vec::len) + 1;
        (source_path, seq)
    };
    let _guard = TransferGuard {
        store: s.snapmirror.clone(),
        uuid: uuid.clone(),
    };

    let vol_name = vol_of(&source_path).to_string();
    let rel8: String = uuid.chars().take(8).collect();
    let snap_name = format!("snapmirror.{rel8}.{seq}");

    // 1. Create the source snapshot off the executor; capture the source vol uuid.
    let backend = s.backend.clone();
    let snap_for_snapshot = snap_name.clone();
    let snap_result = crate::blocking::run(move || {
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
        b.create_snapshot(&vol.uuid, &snap_for_snapshot)?;
        Ok::<VolumeUuid, BackendError>(vol.uuid)
    })
    .await;

    let source_vol = match snap_result {
        Ok(v) => v,
        Err(e) => {
            let mut g = s.snapmirror.lock();
            if let Some(rel) = g.relationships.get_mut(&uuid) {
                rel.healthy = false;
            }
            g.transfers.entry(uuid.clone()).or_default().push(Transfer {
                uuid: Uuid::new_v4().to_string(),
                state: "failed".to_string(),
                bytes_transferred: 0,
                snapshot: String::new(),
                end_time: now_rfc3339(),
            });
            return ontap_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "500",
                "snapmirror",
                format!("transfer failed: {}", e.0),
            );
        }
    };

    // 2. Resolve the peer to stream to, with this relationship's incremental base.
    //    A relationship with no reachable peer stays control-plane only: the
    //    snapshot is taken but no bytes move (bytes_transferred = 0).
    let target = {
        let g = s.snapmirror.lock();
        g.relationships.get(&uuid).and_then(|rel| {
            let dest_vol = vol_of(&rel.destination_path).to_string();
            let base = rel.base.clone();
            g.peers
                .get(&rel.peer_uuid)
                .filter(|p| !p.address.is_empty())
                .map(|p| (dest_vol, p.address.clone(), p.token.clone(), base))
        })
    };

    // 3. Stream to the peer if there is one; else record a control-plane-only success.
    let bytes_transferred = if let Some((dest_vol, address, token, base)) = target {
        let backend = s.backend.clone();
        let snap_for_send = snap_name.clone();
        let stream_result = crate::blocking::run(move || {
            let repl = backend
                .as_snapshot()
                .and_then(SnapshotBackend::as_replication)
                .ok_or(BackendError::FeatureNotSupported {
                    capability: "replication",
                })?;
            let reader = repl.send_stream(&source_vol, &snap_for_send, base.as_deref())?;
            post_replication_stream(
                &address,
                &token,
                &dest_vol,
                &snap_for_send,
                base.as_deref(),
                reader,
            )
        })
        .await;

        match stream_result {
            Ok(bytes) => {
                let mut g = s.snapmirror.lock();
                if let Some(rel) = g.relationships.get_mut(&uuid) {
                    // Don't clobber an operator state (e.g. broken_off) set during the
                    // transfer; always advance the base cursor on a real success.
                    if !matches!(rel.state.as_str(), "broken_off" | "paused" | "quiesced") {
                        rel.state = "snapmirrored".to_string();
                        rel.healthy = true;
                    }
                    rel.base = Some(snap_name.clone());
                }
                bytes
            }
            Err(e) => {
                let mut g = s.snapmirror.lock();
                if let Some(rel) = g.relationships.get_mut(&uuid) {
                    rel.healthy = false;
                }
                g.transfers.entry(uuid.clone()).or_default().push(Transfer {
                    uuid: Uuid::new_v4().to_string(),
                    state: "failed".to_string(),
                    bytes_transferred: 0,
                    snapshot: snap_name.clone(),
                    end_time: now_rfc3339(),
                });
                return ontap_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "500",
                    "snapmirror",
                    format!("replication transfer failed: {}", e.0),
                );
            }
        }
    } else {
        let mut g = s.snapmirror.lock();
        if let Some(rel) = g.relationships.get_mut(&uuid)
            && !matches!(rel.state.as_str(), "broken_off" | "paused" | "quiesced")
        {
            rel.state = "snapmirrored".to_string();
            rel.healthy = true;
        }
        0
    };

    let transfer = Transfer {
        uuid: Uuid::new_v4().to_string(),
        state: "success".to_string(),
        bytes_transferred,
        snapshot: snap_name,
        end_time: now_rfc3339(),
    };
    let record = transfer_obj(&uuid, &transfer);
    {
        let mut g = s.snapmirror.lock();
        if !g.relationships.contains_key(&uuid) {
            return ontap_error(
                StatusCode::NOT_FOUND,
                "404",
                "snapmirror",
                format!("relationship {uuid} was deleted during the transfer"),
            );
        }
        g.transfers.entry(uuid).or_default().push(transfer);
    }
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

/// Build a peer's receive URL. A bare host/IP gets `https://` (ONTAP is HTTPS); an
/// address already carrying a scheme is used as-is (e.g. `http://` in tests).
fn replication_url(address: &str) -> String {
    let base = address.trim_end_matches('/');
    if base.contains("://") {
        format!("{base}/internal/snapmirror/receive")
    } else {
        format!("https://{base}/internal/snapmirror/receive")
    }
}

/// POST a replication stream to a peer's receive endpoint, returning the byte count
/// the peer reports applied. Runs on the blocking pool (the reader is synchronous).
fn post_replication_stream(
    address: &str,
    token: &str,
    dest_vol: &str,
    snapshot: &str,
    base: Option<&str>,
    reader: Box<dyn std::io::Read + Send>,
) -> Result<u64, BackendError> {
    let client = reqwest::blocking::Client::builder()
        // Peer trust is network-gated (a private replication network); a homelab
        // ONTAP sim uses self-signed certs, so validation stays at the network layer.
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| BackendError::Internal(format!("replication client: {e}")))?;
    let mut req = client
        .post(replication_url(address))
        .header("X-Replication-Token", token)
        .header("X-Destination-Volume", dest_vol)
        .header("X-Snapshot", snapshot)
        .body(reqwest::blocking::Body::new(reader));
    if let Some(base) = base {
        req = req.header("X-Base-Snapshot", base);
    }
    let resp = req
        .send()
        .map_err(|e| BackendError::Internal(format!("replication POST failed: {e}")))?;
    let status = resp.status();
    let body: Value = resp.json().unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| body.to_string());
        return Err(BackendError::Internal(format!(
            "peer rejected replication ({status}): {msg}"
        )));
    }
    Ok(body.get("bytes").and_then(Value::as_u64).unwrap_or(0))
}

/// `POST /internal/snapmirror/receive` — NOT an ONTAP path. The peer data-plane
/// endpoint: authenticate the sending peer by its replication token, then apply
/// the replication stream to the destination volume via the backend's
/// [`ReplicationBackend`](nessie_backend_core::ReplicationBackend). Bypasses ONTAP
/// Basic auth (see [`crate::auth`]); the token is the credential here.
async fn internal_receive(State(s): State<AppState>, headers: HeaderMap, body: Body) -> Response {
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

    // Bridge the async request body into a synchronous reader so a large `zfs send`
    // stream flows straight into the backend without ever being buffered in RAM.
    let dest = dest.to_string();
    let backend = s.backend.clone();
    let byte_stream = body
        .into_data_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let async_read = tokio_util::io::StreamReader::new(byte_stream);
    let mut reader = tokio_util::io::SyncIoBridge::new(async_read);
    let result = crate::blocking::run(move || {
        let repl = backend
            .as_snapshot()
            .and_then(SnapshotBackend::as_replication)
            .ok_or(BackendError::FeatureNotSupported {
                capability: "replication",
            })?;
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
        // The peer data plane carries whole `zfs send` streams — the 2 MB default
        // body limit (right for ONTAP REST clients) would reject them, so disable it
        // on just this route. The body is streamed, not buffered (see internal_receive).
        .route(
            "/internal/snapmirror/receive",
            post(internal_receive).layer(DefaultBodyLimit::disable()),
        )
}
