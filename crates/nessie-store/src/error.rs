//! Mapping backend errors onto ONTAP-faithful HTTP responses.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use nessie_backend_core::BackendError;
use nessie_ontap_protocol::{envelope_for, status_for};

/// A backend error wrapped so it renders as the ONTAP-native error envelope
/// (`{"error":{code,message,target}}`) with the matching HTTP status.
pub struct ApiError(pub BackendError);

impl From<BackendError> for ApiError {
    fn from(e: BackendError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(status_for(&self.0)).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(envelope_for(&self.0))).into_response()
    }
}

/// Result alias for handlers that may fail with a backend error.
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::VolumeUuid;

    #[test]
    fn maps_not_found_to_404() {
        let resp = ApiError(BackendError::VolumeNotFound(VolumeUuid::new())).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn maps_feature_not_supported_to_501() {
        let resp = ApiError(BackendError::FeatureNotSupported {
            capability: "clones",
        })
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
