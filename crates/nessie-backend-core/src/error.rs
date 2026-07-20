//! The error type every backend operation returns.
//!
//! Variants are named for the *failure mode*, not the call site. The protocol
//! crate maps each variant onto the matching ONTAP HTTP status and the native
//! `{"error":{"code","message","target"}}` envelope (e.g. [`BackendError::FeatureNotSupported`]
//! → the documented "feature not supported" 4xx, [`BackendError::VolumeNotFound`]
//! → 404). [`BackendError::CommandTimeout`] is first-class so subprocess-backed
//! substrates (ZFS, p4d) can report a hung command distinctly from a failed one.

use std::time::Duration;
use thiserror::Error;

use crate::digest::Digest;
use crate::ids::{SnapshotUuid, VolumeUuid};

/// An error from a storage backend operation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BackendError {
    /// No volume with this UUID exists.
    #[error("volume {0} not found")]
    VolumeNotFound(VolumeUuid),

    /// No snapshot with this UUID exists on the given volume.
    #[error("snapshot {snapshot} not found on volume {volume}")]
    SnapshotNotFound {
        /// The volume that was searched.
        volume: VolumeUuid,
        /// The snapshot that was not found.
        snapshot: SnapshotUuid,
    },

    /// No blob with this digest is present in the content-addressed store.
    #[error("blob {0} not found")]
    BlobNotFound(Digest),

    /// A volume with this name already exists.
    #[error("volume {0:?} already exists")]
    VolumeExists(String),

    /// A snapshot with this name already exists on the volume.
    #[error("snapshot {name:?} already exists on volume {volume}")]
    SnapshotExists {
        /// The volume the snapshot was being created on.
        volume: VolumeUuid,
        /// The conflicting snapshot name.
        name: String,
    },

    /// The backend does not honor the requested capability tier. The string
    /// names the missing capability (e.g. `"snapshots"`, `"clones"`); the
    /// protocol layer renders the documented ONTAP "feature not supported"
    /// response.
    #[error("backend does not support {capability}")]
    FeatureNotSupported {
        /// The capability the caller asked for that this backend lacks.
        capability: &'static str,
    },

    /// The request was malformed (e.g. a junction path not starting with `/`).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A subprocess command exceeded its timeout. Distinct from a command that
    /// ran and failed.
    #[error("command {command:?} timed out after {after:?}")]
    CommandTimeout {
        /// The command line that hung.
        command: String,
        /// How long it ran before timing out.
        after: Duration,
    },

    /// A subprocess command ran and failed.
    #[error("command {command:?} failed: {stderr}")]
    CommandFailed {
        /// The command line that failed.
        command: String,
        /// The captured standard error.
        stderr: String,
    },

    /// An unexpected internal error.
    #[error("internal backend error: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_message_includes_uuid() {
        let id = VolumeUuid::from(uuid::Uuid::nil());
        let e = BackendError::VolumeNotFound(id);
        assert_eq!(
            e.to_string(),
            "volume 00000000-0000-0000-0000-000000000000 not found"
        );
    }

    #[test]
    fn blob_not_found_message_includes_digest() {
        let d = Digest::compute(b"");
        let e = BackendError::BlobNotFound(d.clone());
        assert_eq!(e.to_string(), format!("blob {d} not found"));
    }

    #[test]
    fn feature_not_supported_names_the_capability() {
        let e = BackendError::FeatureNotSupported {
            capability: "clones",
        };
        assert!(e.to_string().contains("clones"));
    }

    #[test]
    fn command_timeout_is_distinct_from_failure() {
        let t = BackendError::CommandTimeout {
            command: "zfs list".into(),
            after: Duration::from_secs(30),
        };
        let f = BackendError::CommandFailed {
            command: "zfs list".into(),
            stderr: "boom".into(),
        };
        assert_ne!(t.to_string(), f.to_string());
    }
}
