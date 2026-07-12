//! End-to-end: cluster peers + SnapMirror relationships, driven in-process.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use nessie_backend_core::{SnapshotBackend, VolumeBackend, VolumeSpec};
use nessie_backend_mem::MemBackend;
use nessie_store::config::Config;
use nessie_store::identity::Identity;
use nessie_store::{AppState, app};

/// A valid mem replication stream (as `send_stream` would produce), for driving
/// the receive endpoint without a second live instance.
fn mem_replication_stream(source_vol: &str, snapshot: &str) -> Vec<u8> {
    use std::io::Read as _;
    let b = MemBackend::new();
    let v = b.create_volume(VolumeSpec::named(source_vol)).unwrap();
    b.create_snapshot(&v.uuid, snapshot).unwrap();
    let repl = b.as_snapshot().unwrap().as_replication().unwrap();
    let mut s = repl.send_stream(&v.uuid, snapshot, None).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    out
}

const ADMIN: &str = "Basic YWRtaW46YWRtaW4="; // admin:admin

fn test_app() -> Router {
    let cfg = Config::default();
    let dir = std::env::temp_dir().join(format!("nessie-sm-{}", uuid::Uuid::new_v4()));
    let identity = Identity::load_or_create(&dir.join("identity.json")).expect("identity");
    std::fs::remove_dir_all(&dir).ok();
    let state = AppState::new(
        Arc::new(MemBackend::new()),
        Arc::new(cfg),
        Arc::new(identity),
    );
    app(state)
}

async fn send(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, ADMIN);
    let req = match body {
        Some(v) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn peer_lifecycle_and_referenced_delete_conflict() {
    let app = test_app();

    // create peer
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/cluster/peers",
        Some(json!({ "name": "cluster2", "ip_address": "192.168.1.200" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let peer = body["uuid"].as_str().unwrap().to_string();

    // list + get
    let (status, body) = send(&app, Method::GET, "/api/cluster/peers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["num_records"], 1);
    let (status, _b) = send(
        &app,
        Method::GET,
        &format!("/api/cluster/peers/{peer}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // a relationship referencing the peer makes delete a 409
    let (status, _b) = send(
        &app,
        Method::POST,
        "/api/snapmirror/relationships",
        Some(json!({
            "source": { "path": "svm0:vol1", "cluster": { "name": "cluster2" } },
            "destination": { "path": "svm0:vol1_dr" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = send(
        &app,
        Method::DELETE,
        &format!("/api/cluster/peers/{peer}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["target"], "peer");
}

#[tokio::test]
async fn relationship_lifecycle() {
    let app = test_app();
    // need a peer for resolution (else peer_uuid is empty, still allowed)
    send(
        &app,
        Method::POST,
        "/api/cluster/peers",
        Some(json!({ "name": "c2", "ip_address": "10.0.0.2" })),
    )
    .await;

    // create
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/snapmirror/relationships",
        Some(json!({
            "source": { "path": "svm0:vol1", "cluster": { "name": "c2" } },
            "destination": { "path": "svm0:vol1_dr" },
            "policy": { "name": "MirrorAllSnapshots" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let rel = body["record"]["uuid"].as_str().unwrap().to_string();
    assert_eq!(body["record"]["state"], "snapmirrored");
    assert_eq!(body["record"]["source"]["svm"]["name"], "svm0");

    // get
    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/api/snapmirror/relationships/{rel}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["healthy"], true);

    // patch -> break_off flips healthy
    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/api/snapmirror/relationships/{rel}"),
        Some(json!({ "state": "broken_off" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "broken_off");
    assert_eq!(body["healthy"], false);

    // transfers list is empty (data plane is a follow-up)
    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/api/snapmirror/relationships/{rel}/transfers"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["num_records"], 0);

    // delete -> then get is 404
    let (status, _b) = send(
        &app,
        Method::DELETE,
        &format!("/api/snapmirror/relationships/{rel}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _b) = send(
        &app,
        Method::GET,
        &format!("/api/snapmirror/relationships/{rel}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn relationship_requires_paths() {
    let app = test_app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/snapmirror/relationships",
        Some(json!({ "source": { "path": "svm0:vol1" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "400");
}

#[tokio::test]
async fn transfer_snapshots_source_and_records_success() {
    let app = test_app();
    // source volume the relationship points at (svm0:vol1 -> "vol1")
    send(
        &app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "name": "vol1" })),
    )
    .await;
    let (_s, body) = send(
        &app,
        Method::POST,
        "/api/snapmirror/relationships",
        Some(json!({
            "source": { "path": "svm0:vol1" },
            "destination": { "path": "svm0:vol1_dr" }
        })),
    )
    .await;
    let rel = body["record"]["uuid"].as_str().unwrap().to_string();

    // trigger a transfer
    let (status, body) = send(
        &app,
        Method::POST,
        &format!("/api/snapmirror/relationships/{rel}/transfers"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["record"]["state"], "success");
    assert_eq!(
        body["record"]["snapshot"],
        format!("snapmirror.{}.1", &rel[..8])
    );

    // it now lists
    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/api/snapmirror/relationships/{rel}/transfers"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["num_records"], 1);
    assert_eq!(body["records"][0]["state"], "success");
}

#[tokio::test]
async fn internal_receive_authenticates_by_token_and_applies_stream() {
    let app = test_app();

    // No token -> 401 (the endpoint bypasses Basic auth; the token is the credential).
    let req = Request::builder()
        .method(Method::POST)
        .uri("/internal/snapmirror/receive")
        .header("x-destination-volume", "vol1_dr")
        .body(Body::from(mem_replication_stream("s", "snapmirror.aaaa.1")))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Register a peer to mint a replication token.
    let (status, peer) = send(
        &app,
        Method::POST,
        "/api/cluster/peers",
        Some(json!({ "name": "src-cluster", "ip_address": "10.0.0.9" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = peer["authentication"]["passphrase"]
        .as_str()
        .unwrap()
        .to_string();

    // A valid token + a real replication stream -> 200, and the destination volume
    // materializes with the replicated snapshot.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/internal/snapmirror/receive")
        .header("x-replication-token", &token)
        .header("x-destination-volume", "vol1_dr")
        .body(Body::from(mem_replication_stream(
            "srcvol",
            "snapmirror.aaaa.1",
        )))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "received");
    assert!(body["bytes"].as_u64().unwrap() > 0);

    let (_s, vols) = send(&app, Method::GET, "/api/storage/volumes", None).await;
    assert!(
        vols["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "vol1_dr"),
        "receive must create the destination volume"
    );

    // A wrong token -> 401.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/internal/snapmirror/receive")
        .header("x-replication-token", "not-the-token")
        .header("x-destination-volume", "vol1_dr2")
        .body(Body::from(mem_replication_stream("s", "snapmirror.aaaa.2")))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
