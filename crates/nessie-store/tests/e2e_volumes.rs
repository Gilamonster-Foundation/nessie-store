//! End-to-end: the volume CRUD + FlexClone surface, driven in-process.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use nessie_backend_core::VolumeUuid;
use nessie_backend_mem::MemBackend;
use nessie_store::config::Config;
use nessie_store::identity::Identity;
use nessie_store::{AppState, app};

const ADMIN: &str = "Basic YWRtaW46YWRtaW4="; // admin:admin

fn test_app() -> (Router, AppState) {
    let cfg = Config::default();
    let dir = std::env::temp_dir().join(format!("nessie-vol-{}", uuid::Uuid::new_v4()));
    let identity = Identity::load_or_create(&dir.join("identity.json")).expect("identity");
    std::fs::remove_dir_all(&dir).ok();
    let state = AppState::new(
        Arc::new(MemBackend::new()),
        Arc::new(cfg),
        Arc::new(identity),
    );
    (app(state.clone()), state)
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
async fn create_get_list_delete_lifecycle() {
    let (app, _s) = test_app();

    // create
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "name": "vol1", "size": 1_073_741_824u64 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["record"]["name"], "vol1");
    assert_eq!(body["num_records"], 1);
    assert!(body["job"]["uuid"].is_string());
    let uuid = body["record"]["uuid"].as_str().unwrap().to_string();

    // get
    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/api/storage/volumes/{uuid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["size"], 1_073_741_824u64);

    // list + name filter
    let (status, body) = send(&app, Method::GET, "/api/storage/volumes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["num_records"], 1);
    let (_s, body) = send(&app, Method::GET, "/api/storage/volumes?name=nope", None).await;
    assert_eq!(body["num_records"], 0);

    // delete -> then get is 404
    let (status, body) = send(
        &app,
        Method::DELETE,
        &format!("/api/storage/volumes/{uuid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["job"]["state"], "success");
    let (status, _b) = send(
        &app,
        Method::GET,
        &format!("/api/storage/volumes/{uuid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_resizes_and_sets_junction() {
    let (app, _s) = test_app();
    let (_s, body) = send(
        &app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "name": "vp" })),
    )
    .await;
    let uuid = body["record"]["uuid"].as_str().unwrap().to_string();

    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/api/storage/volumes/{uuid}"),
        Some(json!({ "size": 2_147_483_648u64, "nas": { "path": "/trident_pvc_x" } })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["record"]["nas"]["path"], "/trident_pvc_x");
    assert_eq!(body["record"]["size"], 2_147_483_648u64);
}

#[tokio::test]
async fn flexclone_records_origin() {
    let (app, s) = test_app();
    // parent volume via REST
    let (_s, body) = send(
        &app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "name": "parent" })),
    )
    .await;
    let parent_uuid: VolumeUuid = body["record"]["uuid"].as_str().unwrap().parse().unwrap();
    // parent snapshot via the backend (snapshot REST is a later phase)
    s.backend
        .as_snapshot()
        .unwrap()
        .create_snapshot(&parent_uuid, "base")
        .unwrap();

    // clone via REST
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({
            "name": "child",
            "clone": {
                "is_flexclone": true,
                "parent_volume": { "name": "parent" },
                "parent_snapshot": { "name": "base" }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["record"]["name"], "child");
    assert_eq!(body["record"]["clone"]["is_flexclone"], true);
    assert_eq!(body["record"]["clone"]["parent_volume"]["name"], "parent");
    assert_eq!(body["record"]["clone"]["parent_snapshot"]["name"], "base");
}

#[tokio::test]
async fn create_without_name_is_400() {
    let (app, _s) = test_app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "size": 1024 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "400");
}
