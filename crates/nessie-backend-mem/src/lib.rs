//! In-memory reference backend for `nessie-store`.
//!
//! A `HashMap`-backed implementation of the full supertrait stack
//! ([`CloneBackend`] plus [`ReplicationBackend`]). It honors every capability tier
//! with no external dependencies, so it is the zero-privilege substrate for the
//! daemon's unit tests and the sanity check that the conformance harness itself is
//! sound. It is **not** a data plane — [`VolumeBackend::access_handle`] returns
//! [`AccessHandle::InMemory`], and its replication "stream" is a logical descriptor
//! of the volume + snapshot state (mem holds no file bytes), not a `zfs send`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "python")]
mod python;

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::sync::{Mutex, MutexGuard};

use nessie_backend_core::{
    AccessHandle, BackendError, Capabilities, CasBackend, CloneBackend, CloneOrigin, Digest,
    ReplicationBackend, Snapshot, SnapshotBackend, SnapshotUuid, Volume, VolumeBackend,
    VolumePatch, VolumeSpec, VolumeState, VolumeStyle, VolumeType, VolumeUuid,
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
        Capabilities::all()
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

    fn as_replication(&self) -> Option<&dyn ReplicationBackend> {
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

/// A parsed mem replication stream — a logical descriptor of the source volume +
/// snapshot (mem holds no file bytes), not a `zfs send` payload.
struct MemReplStream {
    size_bytes: Option<u64>,
    snapshot: String,
    base: Option<String>,
}

/// Serialize a volume + snapshot into the mem replication descriptor format.
fn encode_repl_stream(
    volume: &str,
    size_bytes: Option<u64>,
    snapshot: &str,
    base: Option<&str>,
) -> String {
    format!(
        "NESSIE-MEM-REPL v1\nvolume={volume}\nsize={size}\nsnapshot={snapshot}\nbase={base}\n",
        size = size_bytes.map(|s| s.to_string()).unwrap_or_default(),
        base = base.unwrap_or_default(),
    )
}

/// Parse the descriptor emitted by [`encode_repl_stream`].
fn decode_repl_stream(raw: &str) -> Result<MemReplStream, BackendError> {
    let mut lines = raw.lines();
    if lines.next() != Some("NESSIE-MEM-REPL v1") {
        return Err(BackendError::InvalidArgument(
            "not a nessie-mem replication stream".into(),
        ));
    }
    let mut size_bytes = None;
    let mut snapshot = None;
    let mut base = None;
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "size" if !value.is_empty() => {
                size_bytes = Some(value.parse().map_err(|_| {
                    BackendError::InvalidArgument(format!("invalid size {value:?}"))
                })?);
            }
            "snapshot" => snapshot = Some(value.to_string()),
            "base" if !value.is_empty() => base = Some(value.to_string()),
            _ => {} // ignore volume= and any unknown/empty keys
        }
    }
    Ok(MemReplStream {
        size_bytes,
        snapshot: snapshot.ok_or_else(|| {
            BackendError::InvalidArgument("replication stream missing snapshot".into())
        })?,
        base,
    })
}

impl ReplicationBackend for MemBackend {
    fn send_stream(
        &self,
        vol: &VolumeUuid,
        snap: &str,
        base: Option<&str>,
    ) -> Result<Box<dyn std::io::Read + Send>, BackendError> {
        let g = self.lock();
        let volume = g
            .volumes
            .get(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        let snaps = g
            .snapshots
            .get(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        if !snaps.values().any(|s| s.name == snap) {
            return Err(BackendError::InvalidArgument(format!(
                "snapshot {snap:?} does not exist on the source volume"
            )));
        }
        // An incremental stream's base must exist on the source too.
        if let Some(b) = base
            && !snaps.values().any(|s| s.name == b)
        {
            return Err(BackendError::InvalidArgument(format!(
                "base snapshot {b:?} does not exist on the source volume"
            )));
        }
        let payload = encode_repl_stream(&volume.name, volume.size_bytes, snap, base);
        Ok(Box::new(std::io::Cursor::new(payload.into_bytes())))
    }

    fn receive_stream(
        &self,
        dest: &str,
        stream: &mut dyn std::io::Read,
    ) -> Result<u64, BackendError> {
        let mut raw = String::new();
        std::io::Read::read_to_string(stream, &mut raw)
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        let applied = raw.len() as u64;
        let parsed = decode_repl_stream(&raw)?;

        let mut g = self.lock();
        // Resolve the destination volume by name, or create it for a full stream.
        let dest_uuid = match g.volumes.values().find(|v| v.name == dest).map(|v| v.uuid) {
            Some(u) => {
                // Incremental precondition: the base snapshot must already be here.
                if let Some(b) = &parsed.base {
                    let snaps = g.snapshots.get(&u).expect("live volume has a snapshot map");
                    if !snaps.values().any(|s| &s.name == b) {
                        return Err(BackendError::InvalidArgument(format!(
                            "destination {dest:?} is missing base snapshot {b:?}"
                        )));
                    }
                }
                u
            }
            None => {
                if parsed.base.is_some() {
                    return Err(BackendError::InvalidArgument(format!(
                        "incremental stream for {dest:?}, but the destination volume does not exist"
                    )));
                }
                let vol = new_volume(dest.to_string(), parsed.size_bytes, None);
                let u = vol.uuid;
                g.names.insert(dest.to_string());
                g.snapshots.insert(u, HashMap::new());
                g.volumes.insert(u, vol);
                u
            }
        };

        // Apply the replicated snapshot (idempotent on name).
        let snaps = g
            .snapshots
            .get_mut(&dest_uuid)
            .expect("destination volume has a snapshot map");
        if !snaps.values().any(|s| s.name == parsed.snapshot) {
            let snap = Snapshot {
                uuid: SnapshotUuid::new(),
                name: parsed.snapshot.clone(),
                create_time: None,
                size_consumed: 0,
            };
            snaps.insert(snap.uuid, snap);
        }
        Ok(applied)
    }
}

/// An in-memory content-addressed store: `HashMap<Digest, blob bytes>` behind a
/// `Mutex`. The [`CasBackend`] reference impl and the sanity check that the CAS
/// conformance harness is sound — a separate backend family from [`MemBackend`],
/// mirroring how CAS sits beside the volume trait stack.
pub struct MemCas {
    blobs: Mutex<HashMap<Digest, Vec<u8>>>,
}

impl MemCas {
    /// Create an empty in-memory content-addressed store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blobs: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Digest, Vec<u8>>> {
        self.blobs.lock().expect("mem cas mutex poisoned")
    }
}

impl Default for MemCas {
    fn default() -> Self {
        Self::new()
    }
}

impl CasBackend for MemCas {
    fn has(&self, digest: &Digest) -> Result<bool, BackendError> {
        Ok(self.lock().contains_key(digest))
    }

    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        let bytes = self
            .lock()
            .get(digest)
            .cloned()
            .ok_or_else(|| BackendError::BlobNotFound(digest.clone()))?;
        // Honor the contract: bytes must verify against the digest before serving.
        // In mem this cannot fail, but the reference impl models the guarantee.
        debug_assert!(
            digest.verify(&bytes),
            "mem cas blob failed self-verification"
        );
        Ok(Box::new(Cursor::new(bytes)))
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        let mut bytes = Vec::new();
        source
            .read_to_end(&mut bytes)
            .map_err(|e| BackendError::Internal(format!("cas put: reading source failed: {e}")))?;
        let digest = Digest::compute(&bytes);
        // Idempotent: an existing identical blob is left untouched.
        self.lock().entry(digest.clone()).or_insert(bytes);
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_full_tier_including_replication() {
        let b = MemBackend::new();
        assert_eq!(b.capabilities(), Capabilities::all());
        let snap = b.as_snapshot().expect("snapshot tier");
        assert!(snap.as_clone().is_some());
        assert!(snap.as_replication().is_some());
    }

    #[test]
    fn replication_full_stream_round_trips() {
        let b = MemBackend::new();
        let src = b
            .create_volume(VolumeSpec {
                name: "src".into(),
                size_bytes: Some(2048),
            })
            .unwrap();
        b.create_snapshot(&src.uuid, "snapmirror.aaaa.1").unwrap();
        let repl = b.as_snapshot().unwrap().as_replication().unwrap();

        let mut stream = repl
            .send_stream(&src.uuid, "snapmirror.aaaa.1", None)
            .unwrap();
        let applied = repl.receive_stream("dst", &mut stream).unwrap();
        assert!(applied > 0, "a full stream moves a non-zero byte count");

        // The destination volume now exists with the replicated snapshot + size.
        let dst = b
            .list_volumes()
            .unwrap()
            .into_iter()
            .find(|v| v.name == "dst")
            .expect("destination volume created by receive");
        assert_eq!(dst.size_bytes, Some(2048));
        let dst_snaps = b.list_snapshots(&dst.uuid).unwrap();
        assert!(dst_snaps.iter().any(|s| s.name == "snapmirror.aaaa.1"));
    }

    #[test]
    fn replication_incremental_requires_base_on_destination() {
        let b = MemBackend::new();
        let src = b.create_volume(VolumeSpec::named("isrc")).unwrap();
        b.create_snapshot(&src.uuid, "base").unwrap();
        b.create_snapshot(&src.uuid, "next").unwrap();
        let repl = b.as_snapshot().unwrap().as_replication().unwrap();

        // Incremental into a destination lacking the base is rejected.
        let mut incr = repl.send_stream(&src.uuid, "next", Some("base")).unwrap();
        assert!(matches!(
            repl.receive_stream("idst", &mut incr),
            Err(BackendError::InvalidArgument(_))
        ));

        // After a full baseline, the incremental applies.
        let mut full = repl.send_stream(&src.uuid, "base", None).unwrap();
        repl.receive_stream("idst", &mut full).unwrap();
        let mut incr2 = repl.send_stream(&src.uuid, "next", Some("base")).unwrap();
        repl.receive_stream("idst", &mut incr2).unwrap();

        let idst = b
            .list_volumes()
            .unwrap()
            .into_iter()
            .find(|v| v.name == "idst")
            .unwrap();
        let snaps = b.list_snapshots(&idst.uuid).unwrap();
        assert!(snaps.iter().any(|s| s.name == "base"));
        assert!(snaps.iter().any(|s| s.name == "next"));
    }

    #[test]
    fn send_stream_rejects_unknown_snapshot() {
        let b = MemBackend::new();
        let v = b.create_volume(VolumeSpec::named("nos")).unwrap();
        let repl = b.as_snapshot().unwrap().as_replication().unwrap();
        assert!(matches!(
            repl.send_stream(&v.uuid, "nope", None),
            Err(BackendError::InvalidArgument(_))
        ));
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
