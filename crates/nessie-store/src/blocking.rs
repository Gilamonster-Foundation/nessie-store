//! Run a synchronous backend operation off the async executor.
//!
//! The `VolumeBackend` trait is synchronous and substrate impls (ZFS, p4d, …)
//! block on subprocesses. Calling them directly from an async handler would
//! stall a tokio worker thread, so every backend call goes through here onto the
//! blocking pool. A panic in the closure becomes an internal error rather than
//! taking down the worker.

use nessie_backend_core::BackendError;

use crate::error::ApiError;

/// Execute a blocking backend closure on tokio's blocking pool, mapping the
/// result into an [`ApiError`].
pub async fn run<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, BackendError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| {
            ApiError(BackendError::Internal(format!(
                "backend task panicked: {e}"
            )))
        })?
        .map_err(ApiError)
}
