//! SnapMirror relationships + cluster peers (control plane).
//!
//! Relationship/peer/transfer bookkeeping is daemon state, not substrate state
//! (ZFS is only touched at transfer time, which lands in a follow-up). This
//! module holds an in-memory [`SnapMirrorStore`] and the ONTAP REST surface over
//! it: `/api/cluster/peers*` and `/api/snapmirror/relationships*`.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::state::AppState;

/// A cluster peer.
#[derive(Debug, Clone)]
struct Peer {
    uuid: String,
    name: String,
    address: String,
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

#[derive(Default)]
struct Inner {
    peers: HashMap<String, Peer>,
    relationships: HashMap<String, Relationship>,
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
}

fn svm_of(path: &str) -> &str {
    path.split_once(':').map_or(path, |(svm, _)| svm)
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
    let uuid = Uuid::new_v4().to_string();
    let peer = Peer {
        uuid: uuid.clone(),
        name,
        address,
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
    Json(json!({ "job": { "uuid": Uuid::new_v4().to_string(), "state": "success" } }))
        .into_response()
}

async fn list_transfers(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    // Transfers (the data plane) land in a follow-up; report an empty, valid
    // collection for a known relationship, 404 otherwise.
    let g = s.snapmirror.lock();
    if !g.relationships.contains_key(&uuid) {
        return ontap_error(
            StatusCode::NOT_FOUND,
            "404",
            "snapmirror",
            format!("relationship {uuid} not found"),
        );
    }
    Json(json!({
        "records": [],
        "num_records": 0,
        "_links": { "self": { "href": format!("/api/snapmirror/relationships/{uuid}/transfers") } },
    }))
    .into_response()
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
            get(list_transfers),
        )
}
