//! Domain types for volumes and snapshots.
//!
//! These are clean, substrate-neutral types. The ONTAP HAL wire shapes (with
//! `_links`, `records`/`num_records`, the `job`+`record` envelope) live in the
//! `nessie-ontap-protocol` crate, which maps these domain types onto the wire.
//! Keeping the two separate means a backend never has to know about HAL, and the
//! protocol layer never has to know about a substrate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{SnapshotUuid, VolumeUuid};

/// Operational state of a volume. ONTAP reports `online` for a healthy volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VolumeState {
    /// The volume is mounted and serving.
    #[default]
    Online,
    /// The volume exists but is not serving.
    Offline,
    /// The volume is being created or otherwise transitioning.
    Restricted,
}

/// ONTAP volume style. nessie-store models flexible volumes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VolumeStyle {
    /// A flexible volume (the only style nessie-store exposes).
    #[default]
    Flexvol,
}

/// ONTAP volume access type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VolumeType {
    /// Read-write (the default for provisioned volumes and FlexClones).
    #[default]
    Rw,
    /// Data-protection (a SnapMirror destination).
    Dp,
}

/// Where a FlexClone diverged from. Present only on cloned volumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneOrigin {
    /// Name of the parent volume the clone was taken from.
    pub parent_volume: String,
    /// Name of the parent snapshot the clone was taken from.
    pub parent_snapshot: String,
}

/// A volume as a backend reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Stable identifier.
    pub uuid: VolumeUuid,
    /// ONTAP volume name (the short name, e.g. `vol1`).
    pub name: String,
    /// Quota in bytes, or `None` for an unlimited volume.
    pub size_bytes: Option<u64>,
    /// Operational state.
    pub state: VolumeState,
    /// Volume style.
    pub style: VolumeStyle,
    /// Access type.
    pub vol_type: VolumeType,
    /// Clone origin, present iff this volume is a FlexClone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone: Option<CloneOrigin>,
}

impl Volume {
    /// True if this volume is a FlexClone of some parent snapshot.
    #[must_use]
    pub fn is_clone(&self) -> bool {
        self.clone.is_some()
    }
}

/// A snapshot as a backend reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Stable identifier.
    pub uuid: SnapshotUuid,
    /// Snapshot name.
    pub name: String,
    /// Creation time, if the substrate records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<DateTime<Utc>>,
    /// Bytes uniquely consumed by this snapshot (ONTAP `delta.size_consumed`).
    pub size_consumed: u64,
}

/// Input to [`crate::VolumeBackend::create_volume`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    /// Desired volume name.
    pub name: String,
    /// Desired quota in bytes, or `None` for unlimited.
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

impl VolumeSpec {
    /// Construct a spec with just a name and no quota.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            size_bytes: None,
        }
    }
}

/// A partial update to a volume (ONTAP `PATCH /api/storage/volumes/{uuid}`).
///
/// Every field is optional; `None` means "leave unchanged". This mirrors the
/// sparse PATCH bodies real ONTAP clients (notably Trident) send repeatedly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumePatch {
    /// New quota in bytes.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// New junction path (ONTAP `nas.path`); must start with `/`.
    #[serde(default)]
    pub junction_path: Option<String>,
    /// Export policy name (honored as metadata; rule enforcement is host-level).
    #[serde(default)]
    pub export_policy: Option<String>,
}

impl VolumePatch {
    /// True if the patch carries no changes (every field is `None`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size_bytes.is_none() && self.junction_path.is_none() && self.export_policy.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_volume() -> Volume {
        Volume {
            uuid: VolumeUuid::default(),
            name: "vol1".to_string(),
            size_bytes: Some(1_073_741_824),
            state: VolumeState::Online,
            style: VolumeStyle::Flexvol,
            vol_type: VolumeType::Rw,
            clone: None,
        }
    }

    #[test]
    fn volume_serde_roundtrip() {
        let v = sample_volume();
        let json = serde_json::to_string(&v).expect("serialize");
        let back: Volume = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, back);
    }

    #[test]
    fn clone_origin_is_omitted_when_absent() {
        let json = serde_json::to_string(&sample_volume()).expect("serialize");
        assert!(
            !json.contains("clone"),
            "non-clone volume must not emit a clone key: {json}"
        );
    }

    #[test]
    fn clone_volume_roundtrips_with_origin() {
        let mut v = sample_volume();
        v.clone = Some(CloneOrigin {
            parent_volume: "vol1".into(),
            parent_snapshot: "snap1".into(),
        });
        assert!(v.is_clone());
        let json = serde_json::to_string(&v).expect("serialize");
        let back: Volume = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, back);
    }

    #[test]
    fn enums_render_in_ontap_casing() {
        assert_eq!(
            serde_json::to_string(&VolumeState::Online).unwrap(),
            "\"online\""
        );
        assert_eq!(
            serde_json::to_string(&VolumeStyle::Flexvol).unwrap(),
            "\"flexvol\""
        );
        assert_eq!(serde_json::to_string(&VolumeType::Rw).unwrap(), "\"rw\"");
    }

    #[test]
    fn empty_patch_is_detected() {
        assert!(VolumePatch::default().is_empty());
        assert!(
            !VolumePatch {
                size_bytes: Some(1),
                ..Default::default()
            }
            .is_empty()
        );
    }
}
