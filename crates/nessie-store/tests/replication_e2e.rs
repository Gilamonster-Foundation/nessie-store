//! Two in-process nessie-store instances actually replicate a volume: instance A
//! takes a snapshot and streams it to instance B over HTTP; B applies it and the
//! destination volume materializes. This is the automated form of a two-instance
//! practical test — A is driven in-process, B is a real server on a loopback port.

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
const TOKEN: &str = "shared-replication-passphrase";

fn instance(tag: &str) -> Router {
    let cfg = Config::default();
    let dir = std::env::temp_dir().join(format!("nessie-repl-{tag}-{}", uuid::Uuid::new_v4()));
    let identity = Identity::load_or_create(&dir.join("identity.json")).expect("identity");
    std::fs::remove_dir_all(&dir).ok();
    app(AppState::new(
        Arc::new(MemBackend::new()),
        Arc::new(cfg),
        Arc::new(identity),
    ))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_instances_replicate_a_volume() {
    // --- Destination instance B: a real HTTP server on a loopback port. ---
    let b = instance("dst");
    // B accepts the shared token from its peer record for the source.
    let (st, _) = send(
        &b,
        Method::POST,
        "/api/cluster/peers",
        Some(json!({
            "name": "sourceA",
            "ip_address": "http://unused",
            "authentication": { "passphrase": TOKEN }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_addr = listener.local_addr().unwrap();
    let b_query = b.clone(); // shares B's backend Arc; used to inspect B's state
    tokio::spawn(async move {
        axum::serve(listener, b.into_make_service()).await.unwrap();
    });

    // --- Source instance A: driven in-process. ---
    let a = instance("src");
    let (st, _) = send(
        &a,
        Method::POST,
        "/api/storage/volumes",
        Some(json!({ "name": "src" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    // Peer B: the shared token + B's real loopback address.
    let (st, _) = send(
        &a,
        Method::POST,
        "/api/cluster/peers",
        Some(json!({
            "name": "clusterB",
            "ip_address": format!("http://{b_addr}"),
            "authentication": { "passphrase": TOKEN }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, rel_body) = send(
        &a,
        Method::POST,
        "/api/snapmirror/relationships",
        Some(json!({
            "source": { "path": "svm0:src", "cluster": { "name": "clusterB" } },
            "destination": { "path": "svm0:dst" }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let rel = rel_body["record"]["uuid"].as_str().unwrap().to_string();

    // --- First transfer: A snapshots + streams a full send to B. ---
    let (st, xfer) = send(
        &a,
        Method::POST,
        &format!("/api/snapmirror/relationships/{rel}/transfers"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "transfer response: {xfer}");
    assert_eq!(xfer["record"]["state"], "success");
    assert!(
        xfer["record"]["bytes_transferred"].as_u64().unwrap() > 0,
        "a real transfer moves a non-zero byte count: {xfer}"
    );

    // --- B now holds the replicated destination volume. ---
    let (st, vols) = send(&b_query, Method::GET, "/api/storage/volumes", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        vols["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "dst"),
        "destination instance must hold the replicated volume: {vols}"
    );

    // --- Second transfer is incremental (base cursor advanced) and applies. ---
    let (st, xfer2) = send(
        &a,
        Method::POST,
        &format!("/api/snapmirror/relationships/{rel}/transfers"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "second transfer: {xfer2}");
    assert_eq!(xfer2["record"]["state"], "success");
    assert!(xfer2["record"]["bytes_transferred"].as_u64().unwrap() > 0);

    // B's destination volume now carries BOTH replicated snapshots: the incremental
    // applied on top of the base, which the receiver only accepts if the base is
    // already present (so the per-relationship base cursor advanced).
    let (_s, vols2) = send(&b_query, Method::GET, "/api/storage/volumes", None).await;
    let dst_uuid = vols2["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "dst")
        .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, snaps) = send(
        &b_query,
        Method::GET,
        &format!("/api/storage/volumes/{dst_uuid}/snapshots"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rel8: String = rel.chars().take(8).collect();
    let names: Vec<String> = snaps["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.contains(&format!("snapmirror.{rel8}.1")),
        "destination missing the base snapshot: {snaps}"
    );
    assert!(
        names.contains(&format!("snapmirror.{rel8}.2")),
        "destination missing the incremental snapshot: {snaps}"
    );
}
