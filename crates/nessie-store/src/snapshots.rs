//! Snapshot routes — `/api/storage/volumes/{vol}/snapshots`.
//!
//! Gated on the backend's snapshot tier (`as_snapshot()`); a substrate that
//! can't take snapshots returns the documented "feature not supported" response.
//! Records carry the ONTAP `delta` block; `time_elapsed` is computed from the
//! wall clock at request time (a display value, not a coordination primitive).

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use nessie_backend_core::{BackendError, SnapshotUuid, VolumeUuid};
use nessie_ontap_protocol::{
    CreateResponse, DeleteResponse, HalCollection, SnapshotRecord, snapshot_record,
};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

fn vol_uuid(s: &str) -> Result<VolumeUuid, ApiError> {
    s.parse().map_err(|_| {
        ApiError(BackendError::InvalidArgument(format!(
            "invalid volume uuid {s:?}"
        )))
    })
}

fn snap_uuid(s: &str) -> Result<SnapshotUuid, ApiError> {
    s.parse().map_err(|_| {
        ApiError(BackendError::InvalidArgument(format!(
            "invalid snapshot uuid {s:?}"
        )))
    })
}

/// Snapshot-tier "not supported" error (the substrate can't take snapshots).
fn no_snapshots() -> BackendError {
    BackendError::FeatureNotSupported {
        capability: "snapshots",
    }
}

async fn list_snapshots(
    State(s): State<AppState>,
    Path(vol): Path<String>,
) -> ApiResult<Json<HalCollection<SnapshotRecord>>> {
    let vid = vol_uuid(&vol)?;
    let backend = s.backend.clone();
    let snaps = crate::blocking::run(move || {
        let b = backend.as_snapshot().ok_or_else(no_snapshots)?;
        b.list_snapshots(&vid)
    })
    .await?;
    let now = Utc::now();
    let records = snaps
        .iter()
        .map(|sn| snapshot_record(&vid, sn, now))
        .collect();
    Ok(Json(HalCollection::new(
        records,
        format!("/api/storage/volumes/{vid}/snapshots"),
    )))
}

async fn get_snapshot(
    State(s): State<AppState>,
    Path((vol, snap)): Path<(String, String)>,
) -> ApiResult<Json<SnapshotRecord>> {
    let vid = vol_uuid(&vol)?;
    let sid = snap_uuid(&snap)?;
    let backend = s.backend.clone();
    let snapshot = crate::blocking::run(move || {
        let b = backend.as_snapshot().ok_or_else(no_snapshots)?;
        b.get_snapshot(&vid, &sid)
    })
    .await?;
    Ok(Json(snapshot_record(&vid, &snapshot, Utc::now())))
}

async fn create_snapshot(
    State(s): State<AppState>,
    Path(vol): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let vid = vol_uuid(&vol)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            ApiError(BackendError::InvalidArgument(
                "snapshot name is required".into(),
            ))
        })?
        .to_string();
    let backend = s.backend.clone();
    let snap = crate::blocking::run(move || {
        let b = backend.as_snapshot().ok_or_else(no_snapshots)?;
        b.create_snapshot(&vid, &name)
    })
    .await?;
    let record = snapshot_record(&vid, &snap, Utc::now());
    let location = format!("/api/storage/volumes/{vid}/snapshots/{}", snap.uuid);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(CreateResponse::new(&Uuid::new_v4().to_string(), record)),
    )
        .into_response())
}

async fn delete_snapshot(
    State(s): State<AppState>,
    Path((vol, snap)): Path<(String, String)>,
) -> ApiResult<Json<DeleteResponse>> {
    let vid = vol_uuid(&vol)?;
    let sid = snap_uuid(&snap)?;
    let backend = s.backend.clone();
    crate::blocking::run(move || {
        let b = backend.as_snapshot().ok_or_else(no_snapshots)?;
        b.delete_snapshot(&vid, &sid)
    })
    .await?;
    Ok(Json(DeleteResponse::success(&sid.to_string())))
}

/// Snapshot routes as a router fragment (auth + state applied by [`crate::app`]).
pub fn snapshot_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/storage/volumes/:vol_uuid/snapshots",
            get(list_snapshots).post(create_snapshot),
        )
        .route(
            "/api/storage/volumes/:vol_uuid/snapshots/:snap_uuid",
            get(get_snapshot).delete(delete_snapshot),
        )
}
