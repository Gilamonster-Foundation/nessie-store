//! End-to-end: cluster peers + SnapMirror relationships, driven in-process.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use nessie_backend_mem::MemBackend;
use nessie_store::config::Config;
use nessie_store::identity::Identity;
use nessie_store::{AppState, app};

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
