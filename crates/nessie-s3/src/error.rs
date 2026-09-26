//! Mapping [`BackendError`] to the S3 error a cache client expects.
//!
//! The dual of `nessie-reapi`'s `status_from_backend`. One mapping matters more
//! than the rest: a blob we do not hold is `NoSuchKey`, because that is the only
//! code bazel-remote's S3 proxy translates into a clean cache *miss* — every other
//! error it propagates as an internal error, which surfaces to Bazel as a failed
//! build rather than a cold cache.

use nessie_backend_core::BackendError;
use s3s::{S3Error, S3ErrorCode};

/// Translate a backend error into the S3 error to put on the wire.
#[must_use]
pub fn s3_error_from_backend(err: &BackendError) -> S3Error {
    let code = match err {
        // The miss. Must stay NoSuchKey — see the module note.
        BackendError::BlobNotFound(_) => S3ErrorCode::NoSuchKey,
        // A blob whose bytes do not hash to the key naming it. `InvalidDigest` is
        // S3's own word for exactly this, and it is what `put_keyed` rejects.
        BackendError::InvalidArgument(_) => S3ErrorCode::InvalidDigest,
        BackendError::FeatureNotSupported { .. } => S3ErrorCode::NotImplemented,
        BackendError::AttestationUnverified { .. } => S3ErrorCode::AccessDenied,
        _ => S3ErrorCode::InternalError,
    };
    S3Error::with_message(code, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::Digest;

    #[test]
    fn an_absent_blob_is_the_code_that_means_cache_miss() {
        // Regression guard: bazel-remote only treats NoSuchKey as a miss; any other
        // code becomes an internal error and fails the Bazel build.
        let e = s3_error_from_backend(&BackendError::BlobNotFound(Digest::compute(b"")));
        assert_eq!(*e.code(), S3ErrorCode::NoSuchKey);
    }

    #[test]
    fn maps_the_remaining_cases() {
        assert_eq!(
            *s3_error_from_backend(&BackendError::InvalidArgument("x".into())).code(),
            S3ErrorCode::InvalidDigest
        );
        assert_eq!(
            *s3_error_from_backend(&BackendError::FeatureNotSupported {
                capability: "put_keyed"
            })
            .code(),
            S3ErrorCode::NotImplemented
        );
        assert_eq!(
            *s3_error_from_backend(&BackendError::Internal("boom".into())).code(),
            S3ErrorCode::InternalError
        );
    }

    #[test]
    fn the_backend_message_survives_translation() {
        let d = Digest::compute(b"");
        let e = s3_error_from_backend(&BackendError::BlobNotFound(d.clone()));
        assert!(
            e.message().is_some_and(|m| m.contains(&d.to_string())),
            "the digest must reach the client's log"
        );
    }
}
