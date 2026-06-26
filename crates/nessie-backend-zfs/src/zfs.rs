//! The ZFS backend: maps the supertrait stack onto `zfs`/`zpool`/`exportfs`.
//!
//! Volumes are ZFS datasets, snapshots are `zfs snapshot`, FlexClones are
//! `zfs clone`. The hard-won fidelity invariants from the Python predecessor are
//! preserved and regression-tested: idempotent `set_mountpoint` (a redundant
//! `zfs set mountpoint` triggers an unmount/remount that evicts kernel NFS
//! exports), durable per-volume exports under `/etc/exports.d/`,
//! unexport-before-destroy ordering, busy-retry-with-backoff on destroy, and
//! path-traversal sanitization of export filenames.

#![allow(clippy::result_large_err)] // BackendError is the project-wide error type.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nessie_backend_core::{
    AccessHandle, BackendError, Capabilities, CloneBackend, CloneOrigin, Snapshot, SnapshotBackend,
    SnapshotUuid, Volume, VolumeBackend, VolumePatch, VolumeSpec, VolumeState, VolumeStyle,
    VolumeType, VolumeUuid,
};

use crate::runner::CommandRunner;

const EXPORT_OPTS: &str = "rw,sync,no_subtree_check,no_root_squash,crossmnt";

/// Configuration for the ZFS backend.
#[derive(Debug, Clone)]
pub struct ZfsConfig {
    /// ZFS pool name datasets are created under.
    pub pool: String,
    /// Data-LIF host/IP reported in the NFS access handle.
    pub data_lif: String,
    /// NFS export client specs (e.g. CIDRs). Empty disables exports.
    pub nfs_clients: Vec<String>,
    /// `chown` target applied to a dataset root on junction set (best-effort).
    pub dataset_owner: Option<String>,
    /// `chmod` mode applied to a dataset root on junction set (best-effort).
    pub dataset_mode: Option<String>,
    /// Directory for durable per-volume export files.
    pub exports_dir: PathBuf,
    /// NFSv4 pseudo-root; a junction `/j` mounts at `<srv_root>/j`.
    pub srv_root: PathBuf,
    /// Drive the HOST kernel NFS server (`/etc/exports.d/` + `exportfs -r`).
    /// Set `false` when nessie-store's embedded userspace NFS server serves the
    /// `srv_root` tree itself — then no host kernel NFS server is involved.
    pub manage_kernel_exports: bool,
    /// Base backoff for busy-retry on destroy (doubles each attempt).
    pub retry_base: Duration,
    /// Maximum destroy retries on a "busy" error.
    pub max_destroy_retries: u32,
}

impl Default for ZfsConfig {
    fn default() -> Self {
        Self {
            pool: "ontap-sim".into(),
            data_lif: "127.0.0.1".into(),
            nfs_clients: Vec::new(),
            dataset_owner: None,
            dataset_mode: None,
            exports_dir: PathBuf::from("/etc/exports.d"),
            srv_root: PathBuf::from("/srv"),
            manage_kernel_exports: true,
            retry_base: Duration::from_secs(1),
            max_destroy_retries: 5,
        }
    }
}

/// UUID ↔ ZFS-name mapping. ZFS keys by dataset name; the trait keys by UUID.
#[derive(Default)]
struct Registry {
    vol_by_uuid: HashMap<VolumeUuid, String>,
    vol_by_name: HashMap<String, VolumeUuid>,
    snap_by_uuid: HashMap<SnapshotUuid, (VolumeUuid, String)>,
}

impl Registry {
    fn register_vol(&mut self, name: &str) -> VolumeUuid {
        if let Some(u) = self.vol_by_name.get(name) {
            return *u;
        }
        let u = VolumeUuid::new();
        self.vol_by_uuid.insert(u, name.to_string());
        self.vol_by_name.insert(name.to_string(), u);
        u
    }

    fn vol_name(&self, u: &VolumeUuid) -> Option<String> {
        self.vol_by_uuid.get(u).cloned()
    }

    fn remove_vol(&mut self, u: &VolumeUuid) {
        if let Some(name) = self.vol_by_uuid.remove(u) {
            self.vol_by_name.remove(&name);
            self.snap_by_uuid.retain(|_, (vu, _)| vu != u);
        }
    }

    fn register_snap(&mut self, vol: VolumeUuid, name: &str) -> SnapshotUuid {
        if let Some((u, _)) = self
            .snap_by_uuid
            .iter()
            .find(|(_, (vu, n))| *vu == vol && n == name)
        {
            return *u;
        }
        let u = SnapshotUuid::new();
        self.snap_by_uuid.insert(u, (vol, name.to_string()));
        u
    }

    fn snap(&self, u: &SnapshotUuid) -> Option<(VolumeUuid, String)> {
        self.snap_by_uuid.get(u).cloned()
    }

    fn remove_snap(&mut self, u: &SnapshotUuid) {
        self.snap_by_uuid.remove(u);
    }
}

/// Sanitize a volume name into a safe export filename component: keep only
/// `[A-Za-z0-9_.-]` and drop leading dots (refuses `../` traversal out of
/// `/etc/exports.d/`). An empty result means the caller skips the export.
fn safe_filename_component(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    kept.trim_start_matches('.').to_string()
}

/// Parse a ZFS `origin` field (`pool/vol@snap`) into a clone origin.
fn parse_origin(origin: &str, pool: &str) -> Option<CloneOrigin> {
    if origin.is_empty() || origin == "-" {
        return None;
    }
    let (dataset, snap) = origin.rsplit_once('@')?;
    let parent_volume = dataset
        .strip_prefix(&format!("{pool}/"))
        .unwrap_or(dataset)
        .rsplit('/')
        .next()
        .unwrap_or(dataset)
        .to_string();
    Some(CloneOrigin {
        parent_volume,
        parent_snapshot: snap.to_string(),
    })
}

fn plain_volume(uuid: VolumeUuid, name: String, clone: Option<CloneOrigin>) -> Volume {
    Volume {
        uuid,
        name,
        size_bytes: None,
        state: VolumeState::Online,
        style: VolumeStyle::Flexvol,
        vol_type: VolumeType::Rw,
        clone,
    }
}

/// ZFS-backed implementation of the supertrait stack.
pub struct ZfsBackend<R: CommandRunner> {
    runner: R,
    cfg: ZfsConfig,
    reg: Mutex<Registry>,
}

impl<R: CommandRunner> ZfsBackend<R> {
    /// Construct a backend over `runner` with `cfg`.
    pub fn new(runner: R, cfg: ZfsConfig) -> Self {
        Self {
            runner,
            cfg,
            reg: Mutex::new(Registry::default()),
        }
    }

    fn reg(&self) -> MutexGuard<'_, Registry> {
        self.reg.lock().expect("zfs registry mutex poisoned")
    }

    fn full(&self, name: &str) -> String {
        format!("{}/{}", self.cfg.pool, name)
    }

    /// Run a command, mapping a non-zero exit to [`BackendError::CommandFailed`].
    fn run_checked(&self, argv: &[&str]) -> Result<String, BackendError> {
        let out = self.runner.run(argv)?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(BackendError::CommandFailed {
                command: argv.join(" "),
                stderr: out.stderr,
            })
        }
    }

    /// Idempotent mountpoint set: a no-op (returns `false`) when already at
    /// `target`, because every `zfs set mountpoint` remounts and evicts NFS
    /// exports. Returns `true` if it changed.
    fn set_mountpoint(&self, name: &str, target: &str) -> Result<bool, BackendError> {
        let full = self.full(name);
        let current =
            self.run_checked(&["zfs", "get", "-H", "-o", "value", "mountpoint", &full])?;
        if current.trim() == target {
            return Ok(false);
        }
        let set = format!("mountpoint={target}");
        self.run_checked(&["zfs", "set", &set, &full])?;
        Ok(true)
    }

    fn exports_file(&self, name: &str) -> Option<PathBuf> {
        let safe = safe_filename_component(name);
        if safe.is_empty() {
            None
        } else {
            Some(self.cfg.exports_dir.join(format!("trident_{safe}.exports")))
        }
    }

    /// Durable per-volume NFS export (best-effort; never fails the operation).
    fn nfs_export(&self, name: &str, mountpoint: &str) {
        // The embedded userspace NFS server serves the srv_root tree directly, so
        // there is no host kernel export table to maintain.
        if !self.cfg.manage_kernel_exports {
            return;
        }
        if self.cfg.nfs_clients.is_empty() {
            return;
        }
        let Some(path) = self.exports_file(name) else {
            return;
        };
        let mut content = String::new();
        for client in &self.cfg.nfs_clients {
            content.push_str(&format!("{mountpoint} {client}({EXPORT_OPTS})\n"));
        }
        if std::fs::create_dir_all(&self.cfg.exports_dir).is_ok() {
            if let Err(e) = std::fs::write(&path, content) {
                tracing::warn!(?path, %e, "nfs export write failed (best-effort)");
                return;
            }
            let _ = self.runner.run(&["exportfs", "-r"]);
        }
    }

    /// Remove a durable export (best-effort), before destroy.
    fn nfs_unexport(&self, name: &str) {
        // Embedded NFS plane: nothing in the host kernel export table to remove.
        if !self.cfg.manage_kernel_exports {
            return;
        }
        if let Some(path) = self.exports_file(name) {
            let _ = std::fs::remove_file(&path);
        }
        let _ = self.runner.run(&["exportfs", "-r"]);
    }

    /// Best-effort chown/chmod of a dataset root (NFS opts out of kubelet fsGroup).
    fn set_permissions(&self, mountpoint: &str) {
        if let Some(owner) = &self.cfg.dataset_owner {
            let _ = self.runner.run(&["chown", owner, mountpoint]);
        }
        if let Some(mode) = &self.cfg.dataset_mode {
            let _ = self.runner.run(&["chmod", mode, mountpoint]);
        }
    }

    /// Destroy with unexport-first and busy-retry-with-backoff.
    fn destroy_with_retry(&self, name: &str) -> Result<(), BackendError> {
        let full = self.full(name);
        let mut attempt = 0u32;
        loop {
            let out = self.runner.run(&["zfs", "destroy", "-r", "-f", &full])?;
            if out.success {
                return Ok(());
            }
            if out.stderr.contains("busy") && attempt < self.cfg.max_destroy_retries {
                let backoff = self.cfg.retry_base * 2u32.pow(attempt);
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
                attempt += 1;
                continue;
            }
            return Err(BackendError::CommandFailed {
                command: format!("zfs destroy -r -f {full}"),
                stderr: out.stderr,
            });
        }
    }
}

impl<R: CommandRunner> VolumeBackend for ZfsBackend<R> {
    fn capabilities(&self) -> Capabilities {
        Capabilities::clones()
    }

    fn list_volumes(&self) -> Result<Vec<Volume>, BackendError> {
        let pool = self.cfg.pool.clone();
        let out = self.run_checked(&[
            "zfs",
            "list",
            "-H",
            "-r",
            "-o",
            "name,mountpoint,used,avail,origin",
            "-t",
            "filesystem",
            &pool,
        ])?;
        let prefix = format!("{pool}/");
        let mut reg = self.reg();
        let mut vols = Vec::new();
        for line in out.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                continue;
            }
            if f[0] == pool {
                continue; // skip the pool root dataset
            }
            let short = f[0].strip_prefix(&prefix).unwrap_or(f[0]).to_string();
            let clone = parse_origin(f[4], &pool);
            let uuid = reg.register_vol(&short);
            vols.push(plain_volume(uuid, short, clone));
        }
        Ok(vols)
    }

    fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError> {
        let full = self.full(&spec.name);
        let quota = spec.size_bytes.map(|b| format!("quota={b}"));
        // Place the volume under the NFS export root at create time so it is
        // mountable immediately, without a follow-up junction PATCH (F1).
        let mountpoint = format!("mountpoint={}/{}", self.cfg.srv_root.display(), spec.name);
        let mut argv: Vec<&str> = vec!["zfs", "create", "-o", &mountpoint];
        if let Some(q) = &quota {
            argv.push("-o");
            argv.push(q);
        }
        argv.push(&full);
        self.run_checked(&argv)?;
        let uuid = self.reg().register_vol(&spec.name);
        Ok(Volume {
            size_bytes: spec.size_bytes,
            ..plain_volume(uuid, spec.name, None)
        })
    }

    fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError> {
        let name = self
            .reg()
            .vol_name(uuid)
            .ok_or(BackendError::VolumeNotFound(*uuid))?;
        let full = self.full(&name);
        let out = self.runner.run(&[
            "zfs",
            "list",
            "-H",
            "-o",
            "name,mountpoint,used,avail,origin",
            &full,
        ])?;
        if !out.success {
            return Err(BackendError::VolumeNotFound(*uuid));
        }
        let line = out.stdout.lines().next().unwrap_or_default();
        let f: Vec<&str> = line.split('\t').collect();
        let clone = parse_origin(f.get(4).copied().unwrap_or("-"), &self.cfg.pool);
        Ok(plain_volume(*uuid, name, clone))
    }

    fn delete_volume(&self, uuid: &VolumeUuid) -> Result<(), BackendError> {
        let name = self
            .reg()
            .vol_name(uuid)
            .ok_or(BackendError::VolumeNotFound(*uuid))?;
        // crossmnt auto-exports children and blocks `zfs destroy -f`; unexport first.
        self.nfs_unexport(&name);
        self.destroy_with_retry(&name)?;
        self.reg().remove_vol(uuid);
        Ok(())
    }

    fn patch_volume(&self, uuid: &VolumeUuid, patch: VolumePatch) -> Result<Volume, BackendError> {
        let name = self
            .reg()
            .vol_name(uuid)
            .ok_or(BackendError::VolumeNotFound(*uuid))?;
        if let Some(jp) = &patch.junction_path
            && !jp.starts_with('/')
        {
            return Err(BackendError::InvalidArgument(format!(
                "nas.path must start with '/' (got {jp:?})"
            )));
        }
        if let Some(size) = patch.size_bytes {
            let full = self.full(&name);
            let quota = format!("quota={size}");
            self.run_checked(&["zfs", "set", &quota, &full])?;
        }
        if let Some(jp) = &patch.junction_path {
            let target = format!("{}{}", self.cfg.srv_root.display(), jp);
            self.set_mountpoint(&name, &target)?;
            self.nfs_export(&name, &target); // best-effort
            self.set_permissions(&target); // best-effort
        }
        // export_policy is accepted as metadata; host-level rules apply.
        self.get_volume(uuid)
    }

    fn access_handle(&self, uuid: &VolumeUuid) -> Result<AccessHandle, BackendError> {
        let name = self
            .reg()
            .vol_name(uuid)
            .ok_or(BackendError::VolumeNotFound(*uuid))?;
        let full = self.full(&name);
        let mount = self.run_checked(&["zfs", "get", "-H", "-o", "value", "mountpoint", &full])?;
        Ok(AccessHandle::NfsExport {
            server: self.cfg.data_lif.clone(),
            path: PathBuf::from(mount.trim()),
        })
    }

    fn as_snapshot(&self) -> Option<&dyn SnapshotBackend> {
        Some(self)
    }
}

impl<R: CommandRunner> SnapshotBackend for ZfsBackend<R> {
    fn list_snapshots(&self, vol: &VolumeUuid) -> Result<Vec<Snapshot>, BackendError> {
        let name = self
            .reg()
            .vol_name(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        let full = self.full(&name);
        let out = self.run_checked(&[
            "zfs",
            "list",
            "-Hp",
            "-r",
            "-o",
            "name,creation,used,refer",
            "-t",
            "snapshot",
            &full,
        ])?;
        let mut reg = self.reg();
        let mut snaps = Vec::new();
        for line in out.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 4 {
                continue;
            }
            let snap_name = f[0].rsplit_once('@').map_or(f[0], |(_, s)| s).to_string();
            let creation: i64 = f[1].parse().unwrap_or(0);
            let used: u64 = f[2].parse().unwrap_or(0);
            let uuid = reg.register_snap(*vol, &snap_name);
            snaps.push(Snapshot {
                uuid,
                name: snap_name,
                create_time: chrono::DateTime::from_timestamp(creation, 0),
                size_consumed: used,
            });
        }
        Ok(snaps)
    }

    fn create_snapshot(&self, vol: &VolumeUuid, name: &str) -> Result<Snapshot, BackendError> {
        let ds = self
            .reg()
            .vol_name(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        let target = format!("{}/{ds}@{name}", self.cfg.pool);
        self.run_checked(&["zfs", "snapshot", &target])?;
        let uuid = self.reg().register_snap(*vol, name);
        Ok(Snapshot {
            uuid,
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
        let (rvol, sname) = self
            .reg()
            .snap(snap)
            .ok_or(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            })?;
        if &rvol != vol {
            return Err(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            });
        }
        self.list_snapshots(vol)?
            .into_iter()
            .find(|s| s.name == sname)
            .ok_or(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            })
    }

    fn delete_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid) -> Result<(), BackendError> {
        let (rvol, sname) = self
            .reg()
            .snap(snap)
            .ok_or(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            })?;
        if &rvol != vol {
            return Err(BackendError::SnapshotNotFound {
                volume: *vol,
                snapshot: *snap,
            });
        }
        let ds = self
            .reg()
            .vol_name(vol)
            .ok_or(BackendError::VolumeNotFound(*vol))?;
        let target = format!("{}/{ds}@{sname}", self.cfg.pool);
        self.run_checked(&["zfs", "destroy", &target])?;
        self.reg().remove_snap(snap);
        Ok(())
    }

    fn as_clone(&self) -> Option<&dyn CloneBackend> {
        Some(self)
    }
}

impl<R: CommandRunner> CloneBackend for ZfsBackend<R> {
    fn create_clone(
        &self,
        parent_vol: &VolumeUuid,
        parent_snap: &SnapshotUuid,
        new_name: &str,
    ) -> Result<Volume, BackendError> {
        let pname = self
            .reg()
            .vol_name(parent_vol)
            .ok_or(BackendError::VolumeNotFound(*parent_vol))?;
        let (rvol, sname) = self
            .reg()
            .snap(parent_snap)
            .ok_or(BackendError::SnapshotNotFound {
                volume: *parent_vol,
                snapshot: *parent_snap,
            })?;
        if &rvol != parent_vol {
            return Err(BackendError::SnapshotNotFound {
                volume: *parent_vol,
                snapshot: *parent_snap,
            });
        }
        let src = format!("{}/{pname}@{sname}", self.cfg.pool);
        let dst = self.full(new_name);
        // Land the clone under the NFS export root so it is mountable from this
        // single POST — no second junction PATCH needed (F1).
        let mountpoint = format!("mountpoint={}/{}", self.cfg.srv_root.display(), new_name);
        self.run_checked(&["zfs", "clone", "-o", &mountpoint, &src, &dst])?;
        let uuid = self.reg().register_vol(new_name);
        Ok(plain_volume(
            uuid,
            new_name.to_string(),
            Some(CloneOrigin {
                parent_volume: pname,
                parent_snapshot: sname,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[derive(Default)]
    struct Mock {
        responses: Mutex<VecDeque<CommandOutput>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Mock {
        fn ok(stdout: &str) -> CommandOutput {
            CommandOutput {
                success: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        }
        fn fail(stderr: &str) -> CommandOutput {
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_string(),
            }
        }
        fn push(&self, o: CommandOutput) {
            self.responses.lock().unwrap().push_back(o);
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
        fn last(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl CommandRunner for Mock {
        fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError> {
            self.calls
                .lock()
                .unwrap()
                .push(argv.iter().map(|s| s.to_string()).collect());
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Mock::ok("")))
        }
    }

    fn cfg() -> ZfsConfig {
        ZfsConfig {
            pool: "tank".into(),
            retry_base: Duration::ZERO, // no real sleeping in tests
            ..ZfsConfig::default()
        }
    }

    fn backend(mock: Arc<Mock>) -> ZfsBackend<Arc<Mock>> {
        ZfsBackend::new(mock, cfg())
    }

    #[test]
    fn create_volume_emits_quota_argv() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok(""));
        let b = backend(mock.clone());
        let v = b
            .create_volume(VolumeSpec {
                name: "vol1".into(),
                size_bytes: Some(1024),
            })
            .unwrap();
        assert_eq!(v.name, "vol1");
        assert_eq!(v.size_bytes, Some(1024));
        assert_eq!(
            mock.last(),
            [
                "zfs",
                "create",
                "-o",
                "mountpoint=/srv/vol1",
                "-o",
                "quota=1024",
                "tank/vol1"
            ]
        );
    }

    #[test]
    fn create_volume_without_size_omits_quota() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok(""));
        let b = backend(mock.clone());
        b.create_volume(VolumeSpec::named("v")).unwrap();
        assert_eq!(
            mock.last(),
            ["zfs", "create", "-o", "mountpoint=/srv/v", "tank/v"]
        );
    }

    #[test]
    fn list_volumes_skips_pool_root_and_detects_clone() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok(
            "tank\t/tank\t10K\t1G\t-\n\
             tank/vol1\t/tank/vol1\t24K\t1G\t-\n\
             tank/clone1\t/tank/clone1\t24K\t1G\ttank/vol1@snap1\n",
        ));
        let b = backend(mock);
        let vols = b.list_volumes().unwrap();
        assert_eq!(vols.len(), 2, "pool root must be skipped");
        let clone = vols.iter().find(|v| v.name == "clone1").unwrap();
        let origin = clone.clone.as_ref().unwrap();
        assert_eq!(origin.parent_volume, "vol1");
        assert_eq!(origin.parent_snapshot, "snap1");
        assert!(
            vols.iter()
                .find(|v| v.name == "vol1")
                .unwrap()
                .clone
                .is_none()
        );
    }

    #[test]
    fn get_unknown_uuid_is_not_found() {
        let b = backend(Arc::new(Mock::default()));
        assert!(matches!(
            b.get_volume(&VolumeUuid::new()),
            Err(BackendError::VolumeNotFound(_))
        ));
    }

    #[test]
    fn delete_unexports_before_destroy_and_retries_on_busy() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok("")); // create
        mock.push(Mock::ok("")); // exportfs -r (unexport)
        mock.push(Mock::fail("dataset is busy")); // destroy attempt 1
        mock.push(Mock::ok("")); // destroy attempt 2
        let b = backend(mock.clone());
        let v = b.create_volume(VolumeSpec::named("vol1")).unwrap();
        b.delete_volume(&v.uuid).unwrap();

        let calls = mock.calls();
        // order: create, exportfs -r, destroy(busy), destroy(ok)
        assert_eq!(calls[1], ["exportfs", "-r"], "unexport before destroy");
        assert_eq!(calls[2], ["zfs", "destroy", "-r", "-f", "tank/vol1"]);
        assert_eq!(calls[3], ["zfs", "destroy", "-r", "-f", "tank/vol1"]);
        assert!(matches!(
            b.get_volume(&v.uuid),
            Err(BackendError::VolumeNotFound(_))
        ));
    }

    #[test]
    fn embedded_nfs_mode_emits_no_exportfs() {
        // With the embedded userspace NFS server serving the srv_root tree, the
        // backend must never drive the host kernel export table — no `exportfs`,
        // even when nfs_clients is set.
        let mock = Arc::new(Mock::default());
        let mut c = cfg();
        c.manage_kernel_exports = false;
        c.nfs_clients = vec!["10.0.0.0/8".into()];
        let b = ZfsBackend::new(mock.clone(), c);
        let v = b.create_volume(VolumeSpec::named("vol1")).unwrap();
        b.delete_volume(&v.uuid).unwrap();
        assert!(
            !mock
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("exportfs")),
            "embedded mode must not call exportfs; got {:?}",
            mock.calls()
        );
    }

    #[test]
    fn set_mountpoint_is_idempotent() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok("")); // create
        // patch: zfs get mountpoint returns the SAME target -> no `zfs set`.
        mock.push(Mock::ok("/srv/j\n")); // zfs get mountpoint
        // get_volume re-fetch at end of patch:
        mock.push(Mock::ok("tank/v\t/srv/j\t24K\t1G\t-\n"));
        let b = backend(mock.clone());
        let v = b.create_volume(VolumeSpec::named("v")).unwrap();
        b.patch_volume(
            &v.uuid,
            VolumePatch {
                junction_path: Some("/j".into()),
                ..VolumePatch::default()
            },
        )
        .unwrap();
        let saw_set = mock.calls().iter().any(|c| {
            c.first().map(String::as_str) == Some("zfs")
                && c.get(1).map(String::as_str) == Some("set")
        });
        assert!(
            !saw_set,
            "redundant mountpoint set must be skipped (avoids export eviction)"
        );
    }

    #[test]
    fn patch_rejects_relative_junction_without_running_commands() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok("")); // create
        let b = backend(mock.clone());
        let v = b.create_volume(VolumeSpec::named("v")).unwrap();
        let before = mock.calls().len();
        let res = b.patch_volume(
            &v.uuid,
            VolumePatch {
                junction_path: Some("relative".into()),
                ..VolumePatch::default()
            },
        );
        assert!(matches!(res, Err(BackendError::InvalidArgument(_))));
        assert_eq!(
            mock.calls().len(),
            before,
            "no commands run on a rejected patch"
        );
    }

    #[test]
    fn create_snapshot_and_clone_emit_expected_argv() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok("")); // create vol
        mock.push(Mock::ok("")); // snapshot
        mock.push(Mock::ok("")); // clone
        let b = backend(mock.clone());
        let v = b.create_volume(VolumeSpec::named("vol1")).unwrap();
        let s = b.create_snapshot(&v.uuid, "snap1").unwrap();
        assert_eq!(mock.last(), ["zfs", "snapshot", "tank/vol1@snap1"]);
        let c = b.create_clone(&v.uuid, &s.uuid, "clone1").unwrap();
        assert_eq!(
            mock.last(),
            [
                "zfs",
                "clone",
                "-o",
                "mountpoint=/srv/clone1",
                "tank/vol1@snap1",
                "tank/clone1"
            ]
        );
        let origin = c.clone.unwrap();
        assert_eq!(origin.parent_volume, "vol1");
        assert_eq!(origin.parent_snapshot, "snap1");
    }

    #[test]
    fn fresh_volume_and_clone_land_under_srv_root_mountpoint() {
        // F1: a volume/clone must mount under the NFS export root from a single
        // create (no follow-up junction PATCH), or it is unreachable over NFS.
        // Before the fix neither command set `-o mountpoint`, so the dataset
        // inherited `/<pool>/<name>` instead of `/srv/<name>`.
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok("")); // create vol
        mock.push(Mock::ok("")); // snapshot
        mock.push(Mock::ok("")); // clone
        let b = backend(mock.clone());

        let v = b.create_volume(VolumeSpec::named("data")).unwrap();
        assert!(
            mock.last().contains(&"mountpoint=/srv/data".to_string()),
            "volume must be placed under srv_root: {:?}",
            mock.last()
        );

        let s = b.create_snapshot(&v.uuid, "snap").unwrap();
        b.create_clone(&v.uuid, &s.uuid, "kid").unwrap();
        assert!(
            mock.last().contains(&"mountpoint=/srv/kid".to_string()),
            "clone must be placed under srv_root: {:?}",
            mock.last()
        );
    }

    #[test]
    fn list_snapshots_parses_creation_and_size() {
        let mock = Arc::new(Mock::default());
        mock.push(Mock::ok("")); // create vol
        mock.push(Mock::ok("tank/vol1@snap1\t1700000000\t12345\t99\n"));
        let b = backend(mock.clone());
        let v = b.create_volume(VolumeSpec::named("vol1")).unwrap();
        let snaps = b.list_snapshots(&v.uuid).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "snap1");
        assert_eq!(snaps[0].size_consumed, 12345);
        assert!(snaps[0].create_time.is_some());
    }

    #[test]
    fn safe_filename_component_refuses_traversal() {
        assert_eq!(safe_filename_component("../../etc/passwd"), "etcpasswd");
        assert_eq!(safe_filename_component("..hidden"), "hidden");
        assert_eq!(
            safe_filename_component("trident_pvc-1.2"),
            "trident_pvc-1.2"
        );
        assert_eq!(safe_filename_component("$(rm -rf)"), "rm-rf");
    }

    #[test]
    fn advertises_clone_tier() {
        let b = backend(Arc::new(Mock::default()));
        assert_eq!(b.capabilities(), Capabilities::clones());
        assert!(b.as_snapshot().and_then(|s| s.as_clone()).is_some());
    }
}
