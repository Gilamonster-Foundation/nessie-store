//! Volume routes — the critical path: CRUD + FlexClone.
//!
//! Mirrors the ONTAP `/api/storage/volumes` surface. Create branches on
//! `clone.is_flexclone`: a clone downcasts the backend to the clone tier and
//! returns the documented "feature not supported" response if the substrate
//! can't honor it. Backend calls run on the blocking pool (see [`crate::blocking`])
//! so a subprocess-backed substrate never stalls the async executor.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;
use uuid::Uuid;

use nessie_backend_core::{BackendError, VolumePatch, VolumeSpec, VolumeUuid};
use nessie_ontap_protocol::{
    CreateResponse, DeleteResponse, HalCollection, VolumeRecord, volume_record,
};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

fn parse_uuid(s: &str) -> Result<VolumeUuid, ApiError> {
    s.parse::<VolumeUuid>()
        .map_err(|_| ApiError(BackendError::InvalidArgument(format!("invalid uuid {s:?}"))))
}

fn job_uuid() -> String {
    Uuid::new_v4().to_string()
}

async fn list_volumes(
    State(s): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<HalCollection<VolumeRecord>>> {
    let backend = s.backend.clone();
    let mut vols = crate::blocking::run(move || backend.list_volumes()).await?;
    // ONTAP `?name=` exact filter; `fields=`/pagination are tolerated (ignored).
    if let Some(name) = q.get("name") {
        vols.retain(|v| &v.name == name);
    }
    let svm = s.svm_ref();
    let records = vols.iter().map(|v| volume_record(v, &svm)).collect();
    Ok(Json(HalCollection::new(records, "/api/storage/volumes")))
}

async fn get_volume(
    State(s): State<AppState>,
    Path(uuid): Path<String>,
) -> ApiResult<Json<VolumeRecord>> {
    let id = parse_uuid(&uuid)?;
    let backend = s.backend.clone();
    let vol = crate::blocking::run(move || backend.get_volume(&id)).await?;
    Ok(Json(volume_record(&vol, &s.svm_ref())))
}

async fn create_volume(State(s): State<AppState>, Json(body): Json<Value>) -> ApiResult<Response> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            ApiError(BackendError::InvalidArgument(
                "volume name is required".into(),
            ))
        })?
        .to_string();

    let clone_spec = body.get("clone");
    let is_flexclone = clone_spec
        .and_then(|c| c.get("is_flexclone"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Parse clone parent names up front (cheap); the backend work runs blocking.
    let (parent_vol_name, parent_snap_name) = if is_flexclone {
        let c = clone_spec.ok_or_else(|| {
            ApiError(BackendError::InvalidArgument("clone block required".into()))
        })?;
        let pv = c
            .get("parent_volume")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ApiError(BackendError::InvalidArgument(
                    "clone.parent_volume.name is required".into(),
                ))
            })?
            .to_string();
        let ps = c
            .get("parent_snapshot")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ApiError(BackendError::InvalidArgument(
                    "clone.parent_snapshot.name is required".into(),
                ))
            })?
            .to_string();
        (pv, ps)
    } else {
        (String::new(), String::new())
    };
    let size = body.get("size").and_then(Value::as_u64);

    let backend = s.backend.clone();
    let vol = crate::blocking::run(move || {
        if is_flexclone {
            // Downcast to the snapshot/clone tiers; honest 501 if unsupported.
            let snap = backend
                .as_snapshot()
                .ok_or(BackendError::FeatureNotSupported {
                    capability: "snapshots",
                })?;
            let clone = snap.as_clone().ok_or(BackendError::FeatureNotSupported {
                capability: "clones",
            })?;
            // ONTAP addresses parents by name; resolve to UUIDs.
            let parent_vol = backend
                .list_volumes()?
                .into_iter()
                .find(|v| v.name == parent_vol_name)
                .ok_or_else(|| {
                    BackendError::InvalidArgument(format!(
                        "parent volume {parent_vol_name:?} not found"
                    ))
                })?;
            let parent_snap = snap
                .list_snapshots(&parent_vol.uuid)?
                .into_iter()
                .find(|sn| sn.name == parent_snap_name)
                .ok_or_else(|| {
                    BackendError::InvalidArgument(format!(
                        "parent snapshot {parent_snap_name:?} not found"
                    ))
                })?;
            clone.create_clone(&parent_vol.uuid, &parent_snap.uuid, &name)
        } else {
            backend.create_volume(VolumeSpec {
                name,
                size_bytes: size,
            })
        }
    })
    .await?;

    let record = volume_record(&vol, &s.svm_ref());
    let location = format!("/api/storage/volumes/{}", vol.uuid);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(CreateResponse::new(&job_uuid(), record)),
    )
        .into_response())
}

async fn patch_volume(
    State(s): State<AppState>,
    Path(uuid): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let id = parse_uuid(&uuid)?;
    let nas = body.get("nas");
    let junction = nas
        .and_then(|n| n.get("path"))
        .and_then(Value::as_str)
        .map(String::from);
    let patch = VolumePatch {
        size_bytes: body.get("size").and_then(Value::as_u64),
        junction_path: junction.clone(),
        export_policy: nas
            .and_then(|n| n.get("export_policy"))
            .and_then(Value::as_str)
            .map(String::from),
    };

    let backend = s.backend.clone();
    let vol = crate::blocking::run(move || backend.patch_volume(&id, patch)).await?;
    let mut record = volume_record(&vol, &s.svm_ref());
    if let Some(j) = junction {
        record = record.with_nas_path(j);
    }
    // Volume PATCH is the one 202 in the surface; no Location header.
    Ok((
        StatusCode::ACCEPTED,
        Json(CreateResponse::new(&job_uuid(), record)),
    )
        .into_response())
}

async fn delete_volume(
    State(s): State<AppState>,
    Path(uuid): Path<String>,
) -> ApiResult<Json<DeleteResponse>> {
    let id = parse_uuid(&uuid)?;
    let backend = s.backend.clone();
    crate::blocking::run(move || backend.delete_volume(&id)).await?;
    Ok(Json(DeleteResponse::success(&id.to_string())))
}

/// Volume routes as a router fragment (auth + state applied by [`crate::app`]).
pub fn volume_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/storage/volumes",
            get(list_volumes).post(create_volume),
        )
        .route(
            "/api/storage/volumes/:uuid",
            get(get_volume).patch(patch_volume).delete(delete_volume),
        )
}
