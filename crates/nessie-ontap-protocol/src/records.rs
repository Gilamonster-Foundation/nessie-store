//! ONTAP resource records and the mapping from substrate-neutral domain types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use nessie_backend_core::{Snapshot, Volume, VolumeState, VolumeType, VolumeUuid};

use crate::duration::iso8601_duration;
use crate::links::Links;

/// A reference to an SVM by name + uuid (embedded in volume records).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SvmRef {
    /// SVM name (e.g. `svm0`).
    pub name: String,
    /// SVM UUID.
    pub uuid: String,
}

/// A bare `{ "name": ... }` reference (clone parents).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameRef {
    /// The referenced name.
    pub name: String,
}

/// The `clone` block on a FlexClone volume record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneInfo {
    /// Always true on a clone record.
    pub is_flexclone: bool,
    /// The parent volume by name.
    pub parent_volume: NameRef,
    /// The parent snapshot by name.
    pub parent_snapshot: NameRef,
}

/// The `nas` block on a volume record (the junction path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NasInfo {
    /// The junction path (ONTAP `nas.path`).
    pub path: String,
}

/// An ONTAP volume record (the single-object GET shape and the create `record`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRecord {
    /// Volume UUID.
    pub uuid: String,
    /// Volume name.
    pub name: String,
    /// Owning SVM.
    pub svm: SvmRef,
    /// Quota in bytes (omitted when unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Operational state (`online`).
    pub state: String,
    /// Volume style (`flexvol`).
    pub style: String,
    /// Access type (`rw`/`dp`), serialized as the JSON key `type`.
    #[serde(rename = "type")]
    pub vol_type: String,
    /// Clone origin, present only on FlexClones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone: Option<CloneInfo>,
    /// NAS junction info, present only once a junction is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nas: Option<NasInfo>,
    /// HAL self-link.
    #[serde(rename = "_links")]
    pub links: Links,
}

impl VolumeRecord {
    /// Attach a `nas.path` to this record (used after a junction PATCH).
    #[must_use]
    pub fn with_nas_path(mut self, path: impl Into<String>) -> Self {
        self.nas = Some(NasInfo { path: path.into() });
        self
    }
}

const fn state_str(s: VolumeState) -> &'static str {
    match s {
        VolumeState::Online => "online",
        VolumeState::Offline => "offline",
        VolumeState::Restricted => "restricted",
    }
}

const fn type_str(t: VolumeType) -> &'static str {
    match t {
        VolumeType::Rw => "rw",
        VolumeType::Dp => "dp",
    }
}

/// Map a domain [`Volume`] onto its ONTAP wire record under `svm`.
#[must_use]
pub fn volume_record(vol: &Volume, svm: &SvmRef) -> VolumeRecord {
    VolumeRecord {
        uuid: vol.uuid.to_string(),
        name: vol.name.clone(),
        svm: svm.clone(),
        size: vol.size_bytes,
        state: state_str(vol.state).to_string(),
        style: "flexvol".to_string(),
        vol_type: type_str(vol.vol_type).to_string(),
        clone: vol.clone.as_ref().map(|o| CloneInfo {
            is_flexclone: true,
            parent_volume: NameRef {
                name: o.parent_volume.clone(),
            },
            parent_snapshot: NameRef {
                name: o.parent_snapshot.clone(),
            },
        }),
        nas: None,
        links: Links::to(format!("/api/storage/volumes/{}", vol.uuid)),
    }
}

/// The `delta` block on a snapshot record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDelta {
    /// Bytes uniquely consumed by the snapshot.
    pub size_consumed: u64,
    /// Time since the snapshot was taken, as an ISO-8601 duration.
    pub time_elapsed: String,
}

/// An ONTAP snapshot record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    /// Snapshot UUID.
    pub uuid: String,
    /// Snapshot name.
    pub name: String,
    /// Creation time (omitted if the substrate does not record one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_time: Option<DateTime<Utc>>,
    /// The delta block (`size_consumed`, `time_elapsed`).
    pub delta: SnapshotDelta,
    /// HAL self-link.
    #[serde(rename = "_links")]
    pub links: Links,
}

/// Map a domain [`Snapshot`] onto its ONTAP wire record. `now` is supplied by the
/// caller (the daemon passes the wall clock; tests pass a fixed instant) so
/// `delta.time_elapsed` is computed without hidden time access.
#[must_use]
pub fn snapshot_record(vol: &VolumeUuid, snap: &Snapshot, now: DateTime<Utc>) -> SnapshotRecord {
    let elapsed = snap
        .create_time
        .map_or(0, |t| (now - t).num_seconds().max(0));
    SnapshotRecord {
        uuid: snap.uuid.to_string(),
        name: snap.name.clone(),
        create_time: snap.create_time,
        delta: SnapshotDelta {
            size_consumed: snap.size_consumed,
            time_elapsed: iso8601_duration(elapsed),
        },
        links: Links::to(format!(
            "/api/storage/volumes/{vol}/snapshots/{}",
            snap.uuid
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::CloneOrigin;
    use serde_json::{json, to_value};
    use uuid::Uuid;

    fn svm() -> SvmRef {
        SvmRef {
            name: "svm0".into(),
            uuid: "11111111-1111-1111-1111-111111111111".into(),
        }
    }

    fn base_volume() -> Volume {
        Volume {
            uuid: VolumeUuid::from(Uuid::nil()),
            name: "vol1".into(),
            size_bytes: Some(1_073_741_824),
            state: VolumeState::Online,
            style: nessie_backend_core::VolumeStyle::Flexvol,
            vol_type: VolumeType::Rw,
            clone: None,
        }
    }

    #[test]
    fn volume_record_is_ontap_faithful() {
        let r = volume_record(&base_volume(), &svm());
        assert_eq!(
            to_value(&r).unwrap(),
            json!({
                "uuid": "00000000-0000-0000-0000-000000000000",
                "name": "vol1",
                "svm": { "name": "svm0", "uuid": "11111111-1111-1111-1111-111111111111" },
                "size": 1_073_741_824u64,
                "state": "online",
                "style": "flexvol",
                "type": "rw",
                "_links": { "self": { "href": "/api/storage/volumes/00000000-0000-0000-0000-000000000000" } }
            })
        );
    }

    #[test]
    fn clone_volume_emits_clone_block() {
        let mut vol = base_volume();
        vol.clone = Some(CloneOrigin {
            parent_volume: "vol1".into(),
            parent_snapshot: "snap1".into(),
        });
        let v = to_value(volume_record(&vol, &svm())).unwrap();
        assert_eq!(
            v["clone"],
            json!({
                "is_flexclone": true,
                "parent_volume": { "name": "vol1" },
                "parent_snapshot": { "name": "snap1" }
            })
        );
    }

    #[test]
    fn unlimited_volume_omits_size() {
        let mut vol = base_volume();
        vol.size_bytes = None;
        let v = to_value(volume_record(&vol, &svm())).unwrap();
        assert!(v.get("size").is_none(), "size omitted when unlimited: {v}");
    }

    #[test]
    fn with_nas_path_attaches_junction() {
        let r = volume_record(&base_volume(), &svm()).with_nas_path("/trident_pvc_x");
        let v = to_value(r).unwrap();
        assert_eq!(v["nas"], json!({ "path": "/trident_pvc_x" }));
    }

    #[test]
    fn snapshot_record_computes_delta_and_links() {
        let t0 = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let now = t0 + chrono::Duration::seconds(3 * 3600 + 27 * 60 + 45);
        let snap = Snapshot {
            uuid: nessie_backend_core::SnapshotUuid::from(Uuid::nil()),
            name: "snap1".into(),
            create_time: Some(t0),
            size_consumed: 12_345,
        };
        let vol = VolumeUuid::from(Uuid::nil());
        let r = snapshot_record(&vol, &snap, now);
        let v = to_value(&r).unwrap();
        assert_eq!(v["name"], "snap1");
        assert_eq!(
            v["delta"],
            json!({ "size_consumed": 12_345, "time_elapsed": "PT3H27M45S" })
        );
        assert_eq!(v["create_time"], to_value(t0).unwrap());
        assert_eq!(
            v["_links"]["self"]["href"],
            "/api/storage/volumes/00000000-0000-0000-0000-000000000000/snapshots/00000000-0000-0000-0000-000000000000"
        );
    }
}
