//! In-memory reference backend for `nessie-store`.
//!
//! A `HashMap`-backed implementation of the full supertrait stack
//! ([`CloneBackend`]). It honors every capability tier with no external
//! dependencies, so it is the zero-privilege substrate for the daemon's unit
//! tests and the sanity check that the conformance harness itself is sound. It
//! is **not** a data plane — [`VolumeBackend::access_handle`] returns
//! [`AccessHandle::InMemory`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "python")]
mod python;

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use nessie_backend_core::{
    AccessHandle, BackendError, Capabilities, CloneBackend, CloneOrigin, Snapshot, SnapshotBackend,
    SnapshotUuid, Volume, VolumeBackend, VolumePatch, VolumeSpec, VolumeState, VolumeStyle,
    VolumeType, VolumeUuid,
};

#[derive(Default)]
struct Inner {
    volumes: HashMap<VolumeUuid, Volume>,
    /// Snapshots per volume; every live volume has an entry (possibly empty).
    snapshots: HashMap<VolumeUuid, HashMap<SnapshotUuid, Snapshot>>,
    /// Volume names in use (uniqueness is enforced like ONTAP volume names).
    names: HashSet<String>,
}

/// An in-memory backend. Cheap to construct; clone-free shared state behind a
/// `Mutex` so it satisfies the `Send + Sync` bound the daemon requires.
pub struct MemBackend {
    inner: Mutex<Inner>,
}

impl MemBackend {
    /// Create an empty in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("mem backend mutex poisoned")
    }
}

impl Default for MemBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn new_volume(name: String, size_bytes: Option<u64>, clone: Option<CloneOrigin>) -> Volume {
    Volume {
        uuid: VolumeUuid::new(),
        name,
        size_bytes,
        state: VolumeState::Online,
        style: VolumeStyle::Flexvol,
        vol_type: VolumeType::Rw,
        clone,
    }
}

impl VolumeBackend for MemBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::clones()
    }

    fn list_volumes(&self) -> Result<Vec<Volume>, BackendError> {
        Ok(self.lock().volumes.values().cloned().collect())
    }

    fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError> {
        let mut g = self.lock();
        if g.names.contains(&spec.name) {
            return Err(BackendError::VolumeExists(spec.name));
        }
        let vol = new_volume(spec.name.clone(), spec.size_bytes, None);
        g.names.insert(spec.name);
        g.snapshots.insert(vol.uuid, HashMap::new());
        g.volumes.insert(vol.uuid, vol.clone());
        Ok(vol)
    }

    fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError> {
        self.lock()
            .volumes
            .get(uuid)
            .cloned()
            .ok_or(BackendError::VolumeNotFound(*uuid))
    }

    fn delete_volume(&self, uuid: &VolumeUuid) -> Result<(), BackendError> {
        let mut g = self.lock();
        let vol = g
            .volumes
            .remove(uuid)
            .ok_or(BackendError::VolumeNotFound(*uuid))?;
        g.names.remove(&vol.name);
        g.snapshots.remove(uuid); // cascade: drop the volume's snapshots
        Ok(())
    }

    fn patch_volume(&self, uuid: &VolumeUuid, patch: VolumePatch) -> Result<Volume, BackendError> {
        let mut g = self.lock();
        // ONTAP returns 404 for a missing volume before validating the body.
        if !g.volumes.contains_key(uuid) {
            return Err(BackendError::VolumeNotFound(*uuid));
        }
        if let Some(jp) = &patch.junction_path
            && !jp.starts_with('/')
        {
            return Err(BackendError::InvalidArgument(format!(
                "nas.path must start with '/' (got {jp:?})"
            )));
        }
        let vol = g
            .volumes
            .get_mut(uuid)
            .expect("existence checked immediately above");
        if let Some(size) = patch.size_bytes {
            vol.size_bytes = Some(size);
        }
        // junction_path and export_policy are accepted metadata; the in-memory
        // backend has no real namespace or NFS export to relocate.
        Ok(vol.clone())
    }

    fn access_handle(&self, uuid: &VolumeUuid) -> Result<AccessHandle, BackendError> {
        if self.lock().volumes.contains_key(uuid) {
            Ok(AccessHandle::InMemory)
        } else {
            Err(BackendError::VolumeNotFound(*uuid))
        }
    }

    fn as_snapshot(&self) -> Option<&dyn SnapshotBackend> {
        Some(self)
    }
}

impl SnapshotBackend for MemBackend {
    fn list_snapshots(&self, vol: &VolumeUuid) -> Result<Vec<Snapshot>, BackendError> {
        let g = self.lock();
        let snaps = g
            .snapshots
            .get(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        Ok(snaps.values().cloned().collect())
    }

    fn create_snapshot(&self, vol: &VolumeUuid, name: &str) -> Result<Snapshot, BackendError> {
        let mut g = self.lock();
        let snaps = g
            .snapshots
            .get_mut(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        if snaps.values().any(|s| s.name == name) {
            return Err(BackendError::SnapshotExists {
                volume: *vol,
                name: name.to_string(),
            });
        }
        let snap = Snapshot {
            uuid: SnapshotUuid::new(),
            name: name.to_string(),
            create_time: None, // the in-memory backend does not track wall-clock time
            size_consumed: 0,
        };
        snaps.insert(snap.uuid, snap.clone());
        Ok(snap)
    }

    fn get_snapshot(
        &self,
        vol: &VolumeUuid,
        snap: &SnapshotUuid,
    ) -> Result<Snapshot, BackendError> {
        let g = self.lock();
        let snaps = g
            .snapshots
            .get(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        snaps
            .get(snap)
            .cloned()
            .ok_or(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            })
    }

    fn delete_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid) -> Result<(), BackendError> {
        let mut g = self.lock();
        let snaps = g
            .snapshots
            .get_mut(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        snaps
            .remove(snap)
            .map(|_| ())
            .ok_or(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            })
    }

    fn as_clone(&self) -> Option<&dyn CloneBackend> {
        Some(self)
    }
}

impl CloneBackend for MemBackend {
    fn create_clone(
        &self,
        parent_vol: &VolumeUuid,
        parent_snap: &SnapshotUuid,
        new_name: &str,
    ) -> Result<Volume, BackendError> {
        let mut g = self.lock();
        let (parent_name, parent_size) = {
            let p = g
                .volumes
                .get(parent_vol)
                .ok_or(BackendError::VolumeNotFound(*parent_vol))?;
            (p.name.clone(), p.size_bytes)
        };
        let snap_name = {
            let snaps = g
                .snapshots
                .get(parent_vol)
                .ok_or(BackendError::VolumeNotFound(*parent_vol))?;
            snaps
                .get(parent_snap)
                .ok_or(BackendError::SnapshotNotFound {
                    volume: *parent_vol,
                    snapshot: *parent_snap,
                })?
                .name
                .clone()
        };
        if g.names.contains(new_name) {
            return Err(BackendError::VolumeExists(new_name.to_string()));
        }
        let vol = new_volume(
            new_name.to_string(),
            parent_size,
            Some(CloneOrigin {
                parent_volume: parent_name,
                parent_snapshot: snap_name,
            }),
        );
        g.names.insert(new_name.to_string());
        g.snapshots.insert(vol.uuid, HashMap::new());
        g.volumes.insert(vol.uuid, vol.clone());
        Ok(vol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_full_clone_tier() {
        let b = MemBackend::new();
        assert_eq!(b.capabilities(), Capabilities::clones());
        assert!(b.as_snapshot().is_some());
        assert!(b.as_snapshot().and_then(|s| s.as_clone()).is_some());
    }

    #[test]
    fn duplicate_volume_name_is_rejected() {
        let b = MemBackend::new();
        b.create_volume(VolumeSpec::named("dup")).unwrap();
        assert!(matches!(
            b.create_volume(VolumeSpec::named("dup")),
            Err(BackendError::VolumeExists(_))
        ));
    }

    #[test]
    fn name_freed_after_delete_allows_recreate() {
        let b = MemBackend::new();
        let v = b.create_volume(VolumeSpec::named("reuse")).unwrap();
        b.delete_volume(&v.uuid).unwrap();
        // The name is free again.
        assert!(b.create_volume(VolumeSpec::named("reuse")).is_ok());
    }

    #[test]
    fn resize_is_reflected() {
        let b = MemBackend::new();
        let v = b
            .create_volume(VolumeSpec {
                name: "rs".into(),
                size_bytes: Some(1024),
            })
            .unwrap();
        let patched = b
            .patch_volume(
                &v.uuid,
                VolumePatch {
                    size_bytes: Some(4096),
                    ..VolumePatch::default()
                },
            )
            .unwrap();
        assert_eq!(patched.size_bytes, Some(4096));
        assert_eq!(b.get_volume(&v.uuid).unwrap().size_bytes, Some(4096));
    }

    #[test]
    fn junction_path_must_be_absolute() {
        let b = MemBackend::new();
        let v = b.create_volume(VolumeSpec::named("jp")).unwrap();
        assert!(matches!(
            b.patch_volume(
                &v.uuid,
                VolumePatch {
                    junction_path: Some("relative".into()),
                    ..VolumePatch::default()
                }
            ),
            Err(BackendError::InvalidArgument(_))
        ));
        // an absolute path is accepted
        assert!(
            b.patch_volume(
                &v.uuid,
                VolumePatch {
                    junction_path: Some("/ok".into()),
                    ..VolumePatch::default()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn duplicate_snapshot_name_is_rejected() {
        let b = MemBackend::new();
        let v = b.create_volume(VolumeSpec::named("snapdup")).unwrap();
        b.create_snapshot(&v.uuid, "s1").unwrap();
        assert!(matches!(
            b.create_snapshot(&v.uuid, "s1"),
            Err(BackendError::SnapshotExists { .. })
        ));
    }

    #[test]
    fn deleting_volume_cascades_snapshots() {
        let b = MemBackend::new();
        let v = b.create_volume(VolumeSpec::named("casc")).unwrap();
        b.create_snapshot(&v.uuid, "s1").unwrap();
        b.delete_volume(&v.uuid).unwrap();
        // The volume's snapshot map is gone; listing returns VolumeNotFound.
        assert!(matches!(
            b.list_snapshots(&v.uuid),
            Err(BackendError::VolumeNotFound(_))
        ));
    }

    #[test]
    fn clone_records_origin_names() {
        let b = MemBackend::new();
        let v = b.create_volume(VolumeSpec::named("src")).unwrap();
        let s = b.create_snapshot(&v.uuid, "base").unwrap();
        let c = b.create_clone(&v.uuid, &s.uuid, "child").unwrap();
        let origin = c.clone.unwrap();
        assert_eq!(origin.parent_volume, "src");
        assert_eq!(origin.parent_snapshot, "base");
    }
}
