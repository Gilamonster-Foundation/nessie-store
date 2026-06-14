//! The ONTAP-native error envelope and the [`BackendError`] → HTTP mapping.
//!
//! Real ONTAP returns `{ "error": { "code", "message", "target" } }` — the
//! Python SDK parses exactly this shape. (The Python predecessor returned a
//! framework default `{ "detail": ... }`, a latent SDK-parsing bug this rewrite
//! closes.) The `code` is provisional — the HTTP status as a string — pending the
//! contract tests against NetApp's published OpenAPI document (a later phase).

use serde::{Deserialize, Serialize};

use nessie_backend_core::BackendError;

/// The inner body of an ONTAP error: `{ "code", "message", "target"? }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// A stable error code (provisional: the HTTP status as a string).
    pub code: String,
    /// A human-readable message.
    pub message: String,
    /// The field or resource the error is about, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The ONTAP error envelope: `{ "error": { ... } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// The error body.
    pub error: ErrorBody,
}

/// The HTTP status code ONTAP returns for a given backend error.
#[must_use]
pub fn status_for(err: &BackendError) -> u16 {
    match err {
        BackendError::VolumeNotFound(_) | BackendError::SnapshotNotFound { .. } => 404,
        BackendError::VolumeExists(_) | BackendError::SnapshotExists { .. } => 409,
        BackendError::FeatureNotSupported { .. } => 501,
        BackendError::InvalidArgument(_) => 400,
        BackendError::CommandTimeout { .. } => 504,
        // CommandFailed, Internal, and any future variant are server errors.
        _ => 500,
    }
}

/// The `target` field (the resource/field the error is about), when meaningful.
fn target_for(err: &BackendError) -> Option<String> {
    match err {
        BackendError::VolumeNotFound(_) | BackendError::VolumeExists(_) => Some("volume".into()),
        BackendError::SnapshotNotFound { .. } | BackendError::SnapshotExists { .. } => {
            Some("snapshot".into())
        }
        BackendError::FeatureNotSupported { capability } => Some((*capability).to_string()),
        BackendError::InvalidArgument(_) => Some("request".into()),
        _ => None,
    }
}

/// Build the ONTAP error envelope for a backend error. The `message` is the
/// error's `Display`; the `code` is the HTTP status as a string.
#[must_use]
pub fn envelope_for(err: &BackendError) -> ErrorEnvelope {
    ErrorEnvelope {
        error: ErrorBody {
            code: status_for(err).to_string(),
            message: err.to_string(),
            target: target_for(err),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::VolumeUuid;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn statuses_match_ontap_semantics() {
        assert_eq!(
            status_for(&BackendError::VolumeNotFound(VolumeUuid::new())),
            404
        );
        assert_eq!(status_for(&BackendError::VolumeExists("v".into())), 409);
        assert_eq!(
            status_for(&BackendError::FeatureNotSupported {
                capability: "clones"
            }),
            501
        );
        assert_eq!(status_for(&BackendError::InvalidArgument("x".into())), 400);
        assert_eq!(
            status_for(&BackendError::CommandTimeout {
                command: "zfs".into(),
                after: std::time::Duration::from_secs(1)
            }),
            504
        );
        assert_eq!(status_for(&BackendError::Internal("boom".into())), 500);
    }

    #[test]
    fn envelope_is_ontap_native_shape() {
        let id = VolumeUuid::from(Uuid::nil());
        let env = envelope_for(&BackendError::VolumeNotFound(id));
        assert_eq!(
            serde_json::to_value(&env).unwrap(),
            json!({
                "error": {
                    "code": "404",
                    "message": "volume 00000000-0000-0000-0000-000000000000 not found",
                    "target": "volume"
                }
            })
        );
    }

    #[test]
    fn target_omitted_for_server_errors() {
        let env = envelope_for(&BackendError::Internal("x".into()));
        let v = serde_json::to_value(&env).unwrap();
        assert!(v["error"].get("target").is_none(), "no target on 500: {v}");
    }
}
