//! End-to-end: drive the daemon's router in-process (no socket) and assert the
//! static-metadata surface + Basic-auth contract an ONTAP client relies on.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt; // for `oneshot`

use nessie_backend_mem::MemBackend;
use nessie_store::config::Config;
use nessie_store::identity::Identity;
use nessie_store::{AppState, app};

// base64("admin:admin")
const ADMIN: &str = "Basic YWRtaW46YWRtaW4=";
// base64("admin:wrong")
const WRONG: &str = "Basic YWRtaW46d3Jvbmc=";

fn test_app() -> (Router, AppState) {
    let cfg = Config::default();
    let dir = std::env::temp_dir().join(format!("nessie-e2e-{}", uuid::Uuid::new_v4()));
    let identity = Identity::load_or_create(&dir.join("identity.json")).expect("identity");
    std::fs::remove_dir_all(&dir).ok();
    let backend = Arc::new(MemBackend::new());
    let state = AppState::new(backend, Arc::new(cfg), Arc::new(identity));
    (app(state.clone()), state)
}

async fn get(app: &Router, path: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(path);
    if let Some(a) = auth {
        builder = builder.header(header::AUTHORIZATION, a);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

#[tokio::test]
async fn unauthenticated_is_rejected_with_challenge() {
    let (app, _s) = test_app();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cluster")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let challenge = resp
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        challenge.contains("Basic"),
        "challenge header: {challenge:?}"
    );
}

#[tokio::test]
async fn wrong_credentials_rejected() {
    let (app, _s) = test_app();
    let (status, _) = get(&app, "/api/cluster", Some(WRONG)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cluster_reports_identity_and_version() {
    let (app, s) = test_app();
    let (status, body) = get(&app, "/api/cluster", Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["uuid"], s.identity.cluster_uuid);
    assert_eq!(body["name"], "nessie-store");
    assert_eq!(body["version"]["generation"], 9);
    assert_eq!(body["version"]["major"], 14);
    assert_eq!(body["version"]["minor"], 1);
}

#[tokio::test]
async fn nodes_report_nonzero_serial() {
    let (app, _s) = test_app();
    let (status, body) = get(&app, "/api/cluster/nodes", Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["num_records"], 1);
    let serial = body["records"][0]["serial_number"].as_str().unwrap_or("");
    assert!(
        !serial.is_empty(),
        "node serial must be non-empty (Trident aborts otherwise)"
    );
}

#[tokio::test]
async fn svm_lookup_hits_and_misses() {
    let (app, s) = test_app();
    let (status, body) = get(&app, "/api/svm/svms", Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["records"][0]["uuid"], s.identity.svm_uuid);

    let path = format!("/api/svm/svms/{}", s.identity.svm_uuid);
    let (status, _) = get(&app, &path, Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(
        &app,
        "/api/svm/svms/00000000-0000-0000-0000-000000000000",
        Some(ADMIN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["target"], "svm");
}

#[tokio::test]
async fn job_poll_always_succeeds() {
    let (app, _s) = test_app();
    let (status, body) = get(&app, "/api/cluster/jobs/any-uuid-123", Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "success");
}

#[tokio::test]
async fn network_interface_reports_data_lif() {
    let (app, _s) = test_app();
    let (status, body) = get(&app, "/api/network/ip/interfaces", Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["records"][0]["name"], "data_nfs");
    assert_eq!(body["records"][0]["ip"]["address"], "127.0.0.1");
}
