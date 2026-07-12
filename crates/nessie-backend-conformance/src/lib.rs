//! Generic conformance suites for `nessie-store` storage backends.
//!
//! Any backend must pass the suites its [`Capabilities`] promise. The harness is
//! substrate-agnostic: it provisions volumes/snapshots/clones with unique names,
//! exercises the full lifecycle, asserts ONTAP-faithful behavior, and cleans up
//! after itself — so the same suite validates the in-memory backend, the ZFS
//! backend, and every substrate to come.
//!
//! Suites panic with a descriptive message on the first violation, so call them
//! from a `#[test]` in the backend crate:
//!
//! ```no_run
//! # use nessie_backend_core::VolumeBackend;
//! # fn make_backend() -> Box<dyn VolumeBackend> { unimplemented!() }
//! let backend = make_backend();
//! nessie_backend_conformance::run_all(backend.as_ref());
//! ```
//!
//! [`run_all`] picks suites from `backend.capabilities()` and additionally
//! asserts **capability honesty** — that the advertised tiers exactly match the
//! tiers reachable via `as_snapshot()` / `as_clone()`. A backend that advertises
//! a tier it cannot reach (or hides one it can) fails before any data op runs.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "python")]
mod python;

use nessie_backend_core::{
    BackendError, CloneBackend, ReplicationBackend, SnapshotBackend, VolumeBackend, VolumePatch,
    VolumeSpec, VolumeUuid,
};

/// A unique volume/snapshot name so suites never collide with prior state on a
/// persistent substrate.
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", VolumeUuid::new())
}

/// Run every suite the backend's [`Capabilities`] promise, after checking that
/// those capabilities are self-consistent and match the reachable tiers.
///
/// [`Capabilities`]: nessie_backend_core::Capabilities
pub fn run_all(backend: &dyn VolumeBackend) {
    let caps = backend.capabilities();
    assert!(
        caps.is_consistent(),
        "capabilities are not self-consistent: {caps:?}"
    );

    // Capability honesty: advertised tiers must equal reachable tiers.
    let snap = backend.as_snapshot();
    assert_eq!(
        snap.is_some(),
        caps.snapshots,
        "capabilities().snapshots ({}) disagrees with as_snapshot().is_some() ({})",
        caps.snapshots,
        snap.is_some()
    );
    let clone = snap.and_then(SnapshotBackend::as_clone);
    assert_eq!(
        clone.is_some(),
        caps.clones,
        "capabilities().clones ({}) disagrees with as_clone().is_some() ({})",
        caps.clones,
        clone.is_some()
    );
    let repl = snap.and_then(SnapshotBackend::as_replication);
    assert_eq!(
        repl.is_some(),
        caps.replication,
        "capabilities().replication ({}) disagrees with as_replication().is_some() ({})",
        caps.replication,
        repl.is_some()
    );

    run_volume_suite(backend);
    if let Some(snap) = snap {
        run_snapshot_suite(snap);
    }
    if let Some(clone) = clone {
        run_clone_suite(clone);
    }
    if let Some(repl) = repl {
        run_replication_suite(repl);
    }
}

/// The base volume lifecycle: create, get, list, patch, access-handle, delete,
/// and the not-found error contract. Every backend must pass this.
pub fn run_volume_suite(backend: &dyn VolumeBackend) {
    let name = unique("conf-vol");
    let created = backend
        .create_volume(VolumeSpec {
            name: name.clone(),
            size_bytes: Some(1_073_741_824),
        })
        .expect("create_volume should succeed");
    assert_eq!(
        created.name, name,
        "created volume keeps its requested name"
    );

    // get returns the same identity.
    let got = backend
        .get_volume(&created.uuid)
        .expect("get_volume should find the new volume");
    assert_eq!(got.uuid, created.uuid, "get returns the same uuid");
    assert_eq!(got.name, name, "get returns the same name");

    // list contains it.
    let listed = backend.list_volumes().expect("list_volumes should succeed");
    assert!(
        listed.iter().any(|v| v.uuid == created.uuid),
        "list_volumes should contain the new volume"
    );

    // patch is accepted and the volume stays retrievable.
    backend
        .patch_volume(
            &created.uuid,
            VolumePatch {
                size_bytes: Some(2_147_483_648),
                ..VolumePatch::default()
            },
        )
        .expect("patch_volume (resize) should succeed");
    backend
        .get_volume(&created.uuid)
        .expect("volume still retrievable after patch");

    // a data-plane handle is available.
    backend
        .access_handle(&created.uuid)
        .expect("access_handle should succeed for a live volume");

    // delete removes it; subsequent get is a typed not-found.
    backend
        .delete_volume(&created.uuid)
        .expect("delete_volume should succeed");
    assert!(
        matches!(
            backend.get_volume(&created.uuid),
            Err(BackendError::VolumeNotFound(_))
        ),
        "get after delete must be VolumeNotFound"
    );
    let after = backend
        .list_volumes()
        .expect("list_volumes after delete should succeed");
    assert!(
        !after.iter().any(|v| v.uuid == created.uuid),
        "deleted volume must not appear in list_volumes"
    );

    // get of a never-existed uuid is also not-found.
    assert!(
        matches!(
            backend.get_volume(&VolumeUuid::new()),
            Err(BackendError::VolumeNotFound(_))
        ),
        "get of an unknown uuid must be VolumeNotFound"
    );
}

/// The snapshot lifecycle on a [`SnapshotBackend`]: create, list, get, delete,
/// and the not-found contract.
pub fn run_snapshot_suite(backend: &dyn SnapshotBackend) {
    let vol = backend
        .create_volume(VolumeSpec::named(unique("conf-snapvol")))
        .expect("create parent volume");

    let snap_name = unique("snap");
    let snap = backend
        .create_snapshot(&vol.uuid, &snap_name)
        .expect("create_snapshot should succeed");
    assert_eq!(snap.name, snap_name, "snapshot keeps its requested name");

    let listed = backend
        .list_snapshots(&vol.uuid)
        .expect("list_snapshots should succeed");
    assert!(
        listed.iter().any(|s| s.uuid == snap.uuid),
        "list_snapshots should contain the new snapshot"
    );

    let got = backend
        .get_snapshot(&vol.uuid, &snap.uuid)
        .expect("get_snapshot should find the snapshot");
    assert_eq!(got.uuid, snap.uuid, "get_snapshot returns the same uuid");

    backend
        .delete_snapshot(&vol.uuid, &snap.uuid)
        .expect("delete_snapshot should succeed");
    assert!(
        matches!(
            backend.get_snapshot(&vol.uuid, &snap.uuid),
            Err(BackendError::SnapshotNotFound { .. })
        ),
        "get after delete must be SnapshotNotFound"
    );

    backend
        .delete_volume(&vol.uuid)
        .expect("cleanup parent volume");
}

/// The clone (FlexClone) contract on a [`CloneBackend`]: a clone of a snapshot is
/// a writable volume that records its origin.
pub fn run_clone_suite(backend: &dyn CloneBackend) {
    let parent = backend
        .create_volume(VolumeSpec::named(unique("conf-clonesrc")))
        .expect("create parent volume");
    let snap = backend
        .create_snapshot(&parent.uuid, &unique("snap"))
        .expect("create parent snapshot");

    let clone_name = unique("conf-clone");
    let clone = backend
        .create_clone(&parent.uuid, &snap.uuid, &clone_name)
        .expect("create_clone should succeed");
    assert_eq!(clone.name, clone_name, "clone keeps its requested name");
    assert!(clone.is_clone(), "a clone must report is_clone() == true");
    let origin = clone.clone.as_ref().expect("clone records its origin");
    assert_eq!(
        origin.parent_volume, parent.name,
        "clone origin names the parent volume"
    );
    assert_eq!(
        origin.parent_snapshot, snap.name,
        "clone origin names the parent snapshot"
    );

    backend
        .get_volume(&clone.uuid)
        .expect("clone is retrievable as a volume");

    // cleanup (clone first; a substrate may pin the parent snapshot otherwise).
    backend.delete_volume(&clone.uuid).expect("cleanup clone");
    backend
        .delete_snapshot(&parent.uuid, &snap.uuid)
        .expect("cleanup parent snapshot");
    backend
        .delete_volume(&parent.uuid)
        .expect("cleanup parent volume");
}

/// The replication contract on a [`ReplicationBackend`]: a snapshot serialized by
/// `send_stream` and applied by `receive_stream` reproduces the volume + snapshot
/// on the destination and reports a non-zero byte count. Same-instance send→receive
/// exercises the backend contract; the true cross-instance path is a daemon test.
pub fn run_replication_suite(backend: &dyn ReplicationBackend) {
    let src = backend
        .create_volume(VolumeSpec::named(unique("conf-replsrc")))
        .expect("create replication source volume");
    let snap_name = unique("snapmirror");
    let snap = backend
        .create_snapshot(&src.uuid, &snap_name)
        .expect("create source snapshot");

    // A full stream received into a fresh destination volume.
    let mut stream = backend
        .send_stream(&src.uuid, &snap_name, None)
        .expect("send_stream (full) should succeed");
    let dest_name = unique("conf-repldst");
    let applied = backend
        .receive_stream(&dest_name, &mut stream)
        .expect("receive_stream should succeed");
    assert!(
        applied > 0,
        "a replication stream must report a non-zero byte count"
    );

    // The destination volume now exists and carries the replicated snapshot.
    let dest = backend
        .list_volumes()
        .expect("list_volumes after receive")
        .into_iter()
        .find(|v| v.name == dest_name)
        .expect("destination volume materialized after receive");
    let dest_snaps = backend
        .list_snapshots(&dest.uuid)
        .expect("list destination snapshots");
    assert!(
        dest_snaps.iter().any(|s| s.name == snap_name),
        "destination must carry the replicated snapshot {snap_name}"
    );

    // Sending an unknown snapshot is a typed error, not a panic or empty stream.
    assert!(
        matches!(
            backend.send_stream(&src.uuid, &unique("nope"), None),
            Err(BackendError::InvalidArgument(_))
        ),
        "send_stream of an unknown snapshot must be InvalidArgument"
    );

    // cleanup.
    backend
        .delete_volume(&dest.uuid)
        .expect("cleanup destination volume");
    backend
        .delete_snapshot(&src.uuid, &snap.uuid)
        .expect("cleanup source snapshot");
    backend
        .delete_volume(&src.uuid)
        .expect("cleanup source volume");
}
