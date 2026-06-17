//! Static cluster-metadata routes + the router builder.
//!
//! These endpoints report a faithful one-node, one-SVM cluster so ONTAP client
//! drivers complete discovery (Trident aborts on a zero node-serial or churning
//! UUIDs). Volume/snapshot/snapmirror routes (the dynamic, backend-backed
//! surface) land in later phases. Query params like `fields=` are tolerated
//! (accepted and ignored), per the client-compatibility requirement.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use serde_json::{Value, json};

use nessie_ontap_protocol::JobStatus;

use crate::auth::require_basic_auth;
use crate::state::AppState;

fn version_json(v: &str) -> Value {
    let parts: Vec<u64> = v.split('.').filter_map(|p| p.parse().ok()).collect();
    json!({
        "full": format!("NetApp Release {v}"),
        "generation": parts.first().copied().unwrap_or(9),
        "major": parts.get(1).copied().unwrap_or(0),
        "minor": parts.get(2).copied().unwrap_or(0),
    })
}

fn ontap_404(target: &str, message: String) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "404", "message": message, "target": target } })),
    )
        .into_response()
}

async fn cluster(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": s.config.cluster_name,
        "uuid": s.identity.cluster_uuid,
        "version": version_json(&s.config.ontap_version),
        "_links": { "self": { "href": "/api/cluster" } },
    }))
}

async fn nodes(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "records": [{
            "uuid": s.identity.node_uuid,
            "name": format!("{}-01", s.config.cluster_name),
            "serial_number": s.config.node_serial_number,
            "_links": { "self": { "href": format!("/api/cluster/nodes/{}", s.identity.node_uuid) } },
        }],
        "num_records": 1,
        "_links": { "self": { "href": "/api/cluster/nodes" } },
    }))
}

async fn job(Path(job_uuid): Path<String>) -> Json<JobStatus> {
    // ZFS-style ops are synchronous; any polled job reports success.
    Json(JobStatus::success(&job_uuid))
}

fn svm_obj(s: &AppState) -> Value {
    json!({
        "uuid": s.identity.svm_uuid,
        "name": s.config.svm_name,
        "state": "running",
        "_links": { "self": { "href": format!("/api/svm/svms/{}", s.identity.svm_uuid) } },
    })
}

async fn svms(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "records": [svm_obj(&s)],
        "num_records": 1,
        "_links": { "self": { "href": "/api/svm/svms" } },
    }))
}

async fn svm_by_uuid(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    if uuid == s.identity.svm_uuid {
        Json(svm_obj(&s)).into_response()
    } else {
        ontap_404("svm", format!("SVM {uuid} not found"))
    }
}

fn aggregate_obj(s: &AppState) -> Value {
    json!({
        "uuid": s.identity.aggregate_uuid,
        "name": "aggr1",
        "_links": { "self": { "href": format!("/api/storage/aggregates/{}", s.identity.aggregate_uuid) } },
    })
}

async fn aggregates(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "records": [aggregate_obj(&s)],
        "num_records": 1,
        "_links": { "self": { "href": "/api/storage/aggregates" } },
    }))
}

async fn aggregate_by_uuid(State(s): State<AppState>, Path(uuid): Path<String>) -> Response {
    if uuid == s.identity.aggregate_uuid {
        Json(aggregate_obj(&s)).into_response()
    } else {
        ontap_404("aggregate", format!("aggregate {uuid} not found"))
    }
}

async fn interfaces(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "records": [{
            "uuid": s.identity.lif_uuid,
            "name": "data_nfs",
            "ip": { "address": s.config.data_lif },
            "services": ["data_nfs"],
            "svm": { "name": s.config.svm_name, "uuid": s.identity.svm_uuid },
            "_links": { "self": { "href": format!("/api/network/ip/interfaces/{}", s.identity.lif_uuid) } },
        }],
        "num_records": 1,
        "_links": { "self": { "href": "/api/network/ip/interfaces" } },
    }))
}

/// Build the daemon's HTTP router (auth-wrapped), bound to `state`.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/api/cluster", get(cluster))
        .route("/api/cluster/nodes", get(nodes))
        .route("/api/cluster/jobs/:job_uuid", get(job))
        .route("/api/svm/svms", get(svms))
        .route("/api/svm/svms/:uuid", get(svm_by_uuid))
        .route("/api/storage/aggregates", get(aggregates))
        .route("/api/storage/aggregates/:uuid", get(aggregate_by_uuid))
        .route("/api/network/ip/interfaces", get(interfaces))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_basic_auth,
        ))
        .with_state(state)
}
