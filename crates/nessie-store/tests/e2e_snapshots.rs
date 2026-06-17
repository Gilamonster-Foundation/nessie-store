//! End-to-end: the snapshot surface under a volume, driven in-process.

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
    let dir = std::env::temp_dir().join(format!("nessie-snap-{}", uuid::Uuid::new_v4()));
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

async fn make_volume(app: &Router, name: &str) -> String {
    let (_s, body) = send(
        app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "name": name })),
    )
    .await;
    body["record"]["uuid"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn snapshot_lifecycle() {
    let app = test_app();
    let vol = make_volume(&app, "vol1").await;

    // create
    let (status, body) = send(
        &app,
        Method::POST,
        &format!("/api/storage/volumes/{vol}/snapshots"),
        Some(json!({ "name": "snap1" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["record"]["name"], "snap1");
    assert!(body["record"]["delta"]["time_elapsed"].is_string());
    let snap = body["record"]["uuid"].as_str().unwrap().to_string();

    // list
    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/api/storage/volumes/{vol}/snapshots"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["num_records"], 1);

    // get
    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/api/storage/volumes/{vol}/snapshots/{snap}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["uuid"], snap);

    // delete -> then get is 404
    let (status, body) = send(
        &app,
        Method::DELETE,
        &format!("/api/storage/volumes/{vol}/snapshots/{snap}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["job"]["state"], "success");
    let (status, _b) = send(
        &app,
        Method::GET,
        &format!("/api/storage/volumes/{vol}/snapshots/{snap}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn snapshot_on_unknown_volume_is_404() {
    let app = test_app();
    let (status, _b) = send(
        &app,
        Method::POST,
        "/api/storage/volumes/00000000-0000-0000-0000-000000000000/snapshots",
        Some(json!({ "name": "s" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn snapshot_without_name_is_400() {
    let app = test_app();
    let vol = make_volume(&app, "vol2").await;
    let (status, _b) = send(
        &app,
        Method::POST,
        &format!("/api/storage/volumes/{vol}/snapshots"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
