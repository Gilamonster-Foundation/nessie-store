//! Mapping [`BackendError`] to a gRPC [`tonic::Status`].

use nessie_backend_core::BackendError;
use tonic::{Code, Status};

/// Translate a backend error into the gRPC status a REAPI client expects.
#[must_use]
pub fn status_from_backend(err: &BackendError) -> Status {
    let code = match err {
        BackendError::BlobNotFound(_) | BackendError::VolumeNotFound(_) => Code::NotFound,
        BackendError::InvalidArgument(_) => Code::InvalidArgument,
        BackendError::FeatureNotSupported { .. } => Code::Unimplemented,
        // A confirmed-result conflict (only under a Byzantine ≥ k / non-determinism)
        // is a precondition failure, distinct from a plain miss.
        BackendError::ActionResultConflict { .. } => Code::FailedPrecondition,
        BackendError::AttestationUnverified { .. } => Code::PermissionDenied,
        _ => Code::Internal,
    };
    Status::new(code, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::Digest;

    #[test]
    fn maps_the_common_cases() {
        assert_eq!(
            status_from_backend(&BackendError::BlobNotFound(Digest::compute(b""))).code(),
            Code::NotFound
        );
        assert_eq!(
            status_from_backend(&BackendError::InvalidArgument("x".into())).code(),
            Code::InvalidArgument
        );
        assert_eq!(
            status_from_backend(&BackendError::FeatureNotSupported {
                capability: "put_keyed"
            })
            .code(),
            Code::Unimplemented
        );
        assert_eq!(
            status_from_backend(&BackendError::ActionResultConflict {
                action: Digest::compute(b"a")
            })
            .code(),
            Code::FailedPrecondition
        );
        assert_eq!(
            status_from_backend(&BackendError::Internal("boom".into())).code(),
            Code::Internal
        );
    }
}
