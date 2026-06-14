//! The data-plane contract: how a client actually reaches a volume's bytes.
//!
//! The daemon does not broker bytes. A backend returns an [`AccessHandle`] from
//! [`crate::VolumeBackend::access_handle`], and the client mounts/fetches directly
//! against the substrate. This matches how real ONTAP works — control plane and
//! data plane on separate networks — and keeps nessie-store from becoming a byte
//! funnel. The handle is substrate-native; each variant is the contract a
//! data-plane client honors for that substrate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

/// A substrate-native handle a client uses to read/write a volume's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessHandle {
    /// An NFS export (ZFS and bare-NFS backends).
    NfsExport {
        /// The data-LIF host or IP a client mounts.
        server: String,
        /// The exported path on that server.
        path: PathBuf,
    },
    /// A presigned URL with an expiry (S3 backend).
    S3Presigned {
        /// The presigned URL.
        url: Url,
        /// When the URL stops working.
        expires_at: DateTime<Utc>,
    },
    /// A git remote + ref (git-lfs backend).
    GitRef {
        /// The git remote URL.
        remote: Url,
        /// The ref name (branch/tag) that is the volume/snapshot.
        ref_name: String,
    },
    /// A Perforce stream (p4d backend).
    P4Stream {
        /// The `P4PORT` to connect to.
        p4port: String,
        /// The stream path.
        stream: String,
    },
    /// Passthrough to a real ONTAP cluster (ontap-passthrough backend).
    OntapPassthrough {
        /// The cluster management LIF.
        mgmt_lif: Url,
        /// The data LIF clients mount.
        data_lif: Url,
    },
    /// No external data plane (the in-memory backend; conformance-only).
    InMemory,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfs_export_roundtrips_and_is_tagged() {
        let h = AccessHandle::NfsExport {
            server: "192.168.1.100".into(),
            path: PathBuf::from("/srv/tank/vol1"),
        };
        let json = serde_json::to_string(&h).expect("serialize");
        assert!(json.contains("\"kind\":\"nfs_export\""), "tagged: {json}");
        let back: AccessHandle = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(h, back);
    }

    #[test]
    fn in_memory_roundtrips() {
        let json = serde_json::to_string(&AccessHandle::InMemory).expect("serialize");
        assert_eq!(json, "{\"kind\":\"in_memory\"}");
        let back: AccessHandle = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(AccessHandle::InMemory, back);
    }

    #[test]
    fn s3_presigned_roundtrips() {
        let h = AccessHandle::S3Presigned {
            url: Url::parse("https://s3.example.com/bucket/obj?sig=abc").unwrap(),
            expires_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        let back: AccessHandle = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(h, back);
    }
}
