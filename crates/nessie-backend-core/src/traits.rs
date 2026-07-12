//! The storage-backend supertrait stack.
//!
//! Three tiers, each a supertrait of the one below, matching the substrate
//! capability tiers exactly. A backend implements only the tier it can honor and
//! overrides [`VolumeBackend::as_snapshot`] / [`SnapshotBackend::as_clone`] to
//! return `Some(self)` when it can honor a higher tier. The default `None` gives
//! every plain [`VolumeBackend`] a correct answer for the tiers it lacks, and the
//! REST router downcasts at dispatch — no silent emulation.
//!
//! All traits are `Send + Sync` so the daemon can hold a backend behind an
//! `Arc<dyn VolumeBackend>` and dispatch from many async tasks. Supertrait
//! upcasting (`&dyn CloneBackend` → `&dyn VolumeBackend`) is relied upon and is
//! stable on the repo MSRV (1.88).

use crate::access::AccessHandle;
use crate::capabilities::Capabilities;
use crate::error::BackendError;
use crate::ids::{SnapshotUuid, VolumeUuid};
use crate::types::{Snapshot, Volume, VolumePatch, VolumeSpec};

/// The base tier: volume CRUD plus the data-plane handle. Every backend
/// implements this.
pub trait VolumeBackend: Send + Sync {
    /// What this backend can do. Must be honest and self-consistent
    /// (see [`Capabilities::is_consistent`]).
    fn capabilities(&self) -> Capabilities;

    /// List all volumes.
    fn list_volumes(&self) -> Result<Vec<Volume>, BackendError>;

    /// Create a new volume from `spec`.
    fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError>;

    /// Fetch a single volume by UUID.
    fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError>;

    /// Delete a volume (and, for substrates that cascade, its snapshots).
    fn delete_volume(&self, uuid: &VolumeUuid) -> Result<(), BackendError>;

    /// Apply a partial update to a volume (resize, junction path, export policy).
    fn patch_volume(&self, uuid: &VolumeUuid, patch: VolumePatch) -> Result<Volume, BackendError>;

    /// Return the substrate-native data-plane handle for a volume.
    fn access_handle(&self, uuid: &VolumeUuid) -> Result<AccessHandle, BackendError>;

    /// Upcast to the snapshot tier if this backend honors it. Defaults to `None`.
    fn as_snapshot(&self) -> Option<&dyn SnapshotBackend> {
        None
    }
}

/// The snapshot tier: point-in-time snapshots of a volume.
pub trait SnapshotBackend: VolumeBackend {
    /// List snapshots of a volume.
    fn list_snapshots(&self, vol: &VolumeUuid) -> Result<Vec<Snapshot>, BackendError>;

    /// Create a named snapshot of a volume.
    fn create_snapshot(&self, vol: &VolumeUuid, name: &str) -> Result<Snapshot, BackendError>;

    /// Fetch a single snapshot by UUID.
    fn get_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid)
    -> Result<Snapshot, BackendError>;

    /// Delete a snapshot.
    fn delete_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid) -> Result<(), BackendError>;

    /// Upcast to the clone tier if this backend honors it. Defaults to `None`.
    fn as_clone(&self) -> Option<&dyn CloneBackend> {
        None
    }

    /// Upcast to the replication tier if this backend honors it. Defaults to `None`.
    fn as_replication(&self) -> Option<&dyn ReplicationBackend> {
        None
    }
}

/// The clone tier: writable FlexClones diverging from a snapshot.
pub trait CloneBackend: SnapshotBackend {
    /// Create a writable clone of `parent_snap` (on `parent_vol`) named `new_name`.
    fn create_clone(
        &self,
        parent_vol: &VolumeUuid,
        parent_snap: &SnapshotUuid,
        new_name: &str,
    ) -> Result<Volume, BackendError>;
}

/// The replication tier: SnapMirror-style cross-instance streaming.
///
/// A replication-capable backend can serialize a snapshot into a substrate-native
/// byte stream (for ZFS, `zfs send`) and apply such a stream to a destination
/// volume (`zfs receive`). It branches from [`SnapshotBackend`] — replication needs
/// snapshots — and is independent of [`CloneBackend`]: a backend may honor either,
/// both, or neither.
///
/// Snapshots are named deterministically by the SnapMirror layer, and those
/// **names are the cross-instance contract**: an incremental stream names the
/// common base snapshot, which the destination must already hold. Backends address
/// replication snapshots by name, not by the local [`SnapshotUuid`] (which differs
/// between instances).
pub trait ReplicationBackend: SnapshotBackend {
    /// Open a replication stream for the snapshot named `snap` on volume `vol`.
    ///
    /// With `base = Some(name)`, produce an **incremental** stream from that base
    /// snapshot (the destination must already hold `base`); with `base = None`, a
    /// full stream. The returned reader streams the substrate-native replication
    /// payload and owns any underlying process; a transfer failure surfaces as a
    /// read error.
    fn send_stream(
        &self,
        vol: &VolumeUuid,
        snap: &str,
        base: Option<&str>,
    ) -> Result<Box<dyn std::io::Read + Send>, BackendError>;

    /// Apply a replication stream to the destination volume named `dest`, creating
    /// or updating it, and return the number of bytes applied.
    fn receive_stream(
        &self,
        dest: &str,
        stream: &mut dyn std::io::Read,
    ) -> Result<u64, BackendError>;
}

#[cfg(test)]
mod tests {
    //! A minimal in-test backend exercises the trait shape, the default
    //! accessors, and supertrait upcasting. The real reference implementation
    //! lives in `nessie-backend-mem` (Phase 2) and is validated by the
    //! conformance harness.

    use super::*;
    use crate::types::{VolumeState, VolumeStyle, VolumeType};

    /// A do-nothing volume-only backend: honors the base tier, declines the rest.
    struct VolumeOnly;

    fn fake_volume(name: &str) -> Volume {
        Volume {
            uuid: VolumeUuid::new(),
            name: name.to_string(),
            size_bytes: None,
            state: VolumeState::Online,
            style: VolumeStyle::Flexvol,
            vol_type: VolumeType::Rw,
            clone: None,
        }
    }

    impl VolumeBackend for VolumeOnly {
        fn capabilities(&self) -> Capabilities {
            Capabilities::volume_only()
        }
        fn list_volumes(&self) -> Result<Vec<Volume>, BackendError> {
            Ok(vec![])
        }
        fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError> {
            Ok(fake_volume(&spec.name))
        }
        fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError> {
            Err(BackendError::VolumeNotFound(*uuid))
        }
        fn delete_volume(&self, _uuid: &VolumeUuid) -> Result<(), BackendError> {
            Ok(())
        }
        fn patch_volume(
            &self,
            _uuid: &VolumeUuid,
            _patch: VolumePatch,
        ) -> Result<Volume, BackendError> {
            Ok(fake_volume("patched"))
        }
        fn access_handle(&self, _uuid: &VolumeUuid) -> Result<AccessHandle, BackendError> {
            Ok(AccessHandle::InMemory)
        }
    }

    /// A backend that honors every tier, wiring the upcast accessors.
    struct FullTier;

    impl VolumeBackend for FullTier {
        fn capabilities(&self) -> Capabilities {
            Capabilities::all()
        }
        fn list_volumes(&self) -> Result<Vec<Volume>, BackendError> {
            Ok(vec![])
        }
        fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError> {
            Ok(fake_volume(&spec.name))
        }
        fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError> {
            Err(BackendError::VolumeNotFound(*uuid))
        }
        fn delete_volume(&self, _uuid: &VolumeUuid) -> Result<(), BackendError> {
            Ok(())
        }
        fn patch_volume(
            &self,
            _uuid: &VolumeUuid,
            _patch: VolumePatch,
        ) -> Result<Volume, BackendError> {
            Ok(fake_volume("patched"))
        }
        fn access_handle(&self, _uuid: &VolumeUuid) -> Result<AccessHandle, BackendError> {
            Ok(AccessHandle::InMemory)
        }
        fn as_snapshot(&self) -> Option<&dyn SnapshotBackend> {
            Some(self)
        }
    }

    impl SnapshotBackend for FullTier {
        fn list_snapshots(&self, _vol: &VolumeUuid) -> Result<Vec<Snapshot>, BackendError> {
            Ok(vec![])
        }
        fn create_snapshot(&self, _vol: &VolumeUuid, name: &str) -> Result<Snapshot, BackendError> {
            Ok(Snapshot {
                uuid: SnapshotUuid::new(),
                name: name.to_string(),
                create_time: None,
                size_consumed: 0,
            })
        }
        fn get_snapshot(
            &self,
            vol: &VolumeUuid,
            snap: &SnapshotUuid,
        ) -> Result<Snapshot, BackendError> {
            Err(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            })
        }
        fn delete_snapshot(
            &self,
            _vol: &VolumeUuid,
            _snap: &SnapshotUuid,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn as_clone(&self) -> Option<&dyn CloneBackend> {
            Some(self)
        }
        fn as_replication(&self) -> Option<&dyn ReplicationBackend> {
            Some(self)
        }
    }

    impl CloneBackend for FullTier {
        fn create_clone(
            &self,
            _parent_vol: &VolumeUuid,
            _parent_snap: &SnapshotUuid,
            new_name: &str,
        ) -> Result<Volume, BackendError> {
            Ok(fake_volume(new_name))
        }
    }

    impl ReplicationBackend for FullTier {
        fn send_stream(
            &self,
            _vol: &VolumeUuid,
            snap: &str,
            base: Option<&str>,
        ) -> Result<Box<dyn std::io::Read + Send>, BackendError> {
            // A trivial "stream": full = "full:<snap>", incremental = "incr:<base>:<snap>".
            let payload = match base {
                Some(b) => format!("incr:{b}:{snap}"),
                None => format!("full:{snap}"),
            };
            Ok(Box::new(std::io::Cursor::new(payload.into_bytes())))
        }
        fn receive_stream(
            &self,
            _dest: &str,
            stream: &mut dyn std::io::Read,
        ) -> Result<u64, BackendError> {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(stream, &mut buf)
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            Ok(buf.len() as u64)
        }
    }

    #[test]
    fn volume_only_declines_higher_tiers() {
        let b = VolumeOnly;
        assert!(b.as_snapshot().is_none());
        assert_eq!(b.capabilities(), Capabilities::volume_only());
    }

    #[test]
    fn full_tier_exposes_snapshot_and_clone() {
        let b = FullTier;
        let snap = b.as_snapshot().expect("snapshot tier present");
        let clone = snap.as_clone().expect("clone tier present");
        let v = clone
            .create_clone(&VolumeUuid::new(), &SnapshotUuid::new(), "clone1")
            .expect("clone created");
        assert_eq!(v.name, "clone1");
    }

    #[test]
    fn full_tier_exposes_replication_and_round_trips() {
        let b = FullTier;
        let snap = b.as_snapshot().expect("snapshot tier present");
        let repl = snap.as_replication().expect("replication tier present");

        // A full stream carries the snapshot name.
        let mut full = repl
            .send_stream(&VolumeUuid::new(), "snapmirror.abc.1", None)
            .expect("send full");
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut full, &mut buf).expect("read full");
        assert_eq!(buf, b"full:snapmirror.abc.1");

        // An incremental stream names the common base snapshot.
        let mut incr = repl
            .send_stream(
                &VolumeUuid::new(),
                "snapmirror.abc.2",
                Some("snapmirror.abc.1"),
            )
            .expect("send incremental");
        let mut ibuf = Vec::new();
        std::io::Read::read_to_end(&mut incr, &mut ibuf).expect("read incremental");
        assert_eq!(ibuf, b"incr:snapmirror.abc.1:snapmirror.abc.2");

        // Receiving reports the number of bytes applied.
        let applied = repl
            .receive_stream("vol1_dr", &mut buf.as_slice())
            .expect("receive");
        assert_eq!(applied, buf.len() as u64);
    }

    #[test]
    fn replication_upcasts_to_snapshot_and_volume() {
        let b = FullTier;
        let repl: &dyn ReplicationBackend = b.as_snapshot().unwrap().as_replication().unwrap();
        // Upcast through the supertrait edge (stable on MSRV 1.88).
        let as_snap: &dyn SnapshotBackend = repl;
        let as_vol: &dyn VolumeBackend = as_snap;
        assert!(as_vol.capabilities().replication);
    }

    #[test]
    fn supertrait_upcast_clone_to_volume() {
        let b = FullTier;
        let clone: &dyn CloneBackend = b.as_snapshot().unwrap().as_clone().unwrap();
        // Upcast through both supertrait edges (stable on MSRV 1.88).
        let as_snap: &dyn SnapshotBackend = clone;
        let as_vol: &dyn VolumeBackend = as_snap;
        assert!(as_vol.capabilities().clones);
    }

    #[test]
    fn backend_is_object_safe_behind_arc() {
        use std::sync::Arc;
        let b: Arc<dyn VolumeBackend> = Arc::new(FullTier);
        assert!(b.create_volume(VolumeSpec::named("v")).is_ok());
        // The Arc<dyn> must be Send + Sync to live in the daemon.
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        assert_send_sync(&b);
    }
}
