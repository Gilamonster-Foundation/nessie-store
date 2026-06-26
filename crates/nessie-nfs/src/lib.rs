//! Embedded userspace NFSv3 server for nessie-store.
//!
//! Serves a real on-disk directory tree (a ZFS dataset mountpoint, or any path)
//! over NFSv3 **in-process** — no host kernel NFS server, no `rpc.nfsd`, no
//! `exportfs`, no `rpcbind`/portmapper. Built on the `nessie-nfsserve` crate (a
//! vendored, hardened fork of HuggingFace's `nfsserve` — the NFSv3
//! wire/transport layer); this crate supplies the filesystem
//! ([`PassthroughFs`]) that maps NFS operations onto `std`/`tokio` file I/O.
//!
//! Clients mount it with an explicit fixed port, e.g.:
//! ```text
//! mount -t nfs -o nfsvers=3,proto=tcp,port=2049,mountport=2049,nolock,noacl \
//!     <host>:/ /mnt/point
//! ```
//!
//! ## Stable file handles
//!
//! Unlike the upstream `mirrorfs` example (which allocates fileids from a counter
//! that resets every boot, so clients see `NFS3ERR_STALE` after a restart), this
//! server derives the NFS fileid from the underlying **`(st_dev, st_ino)`** pair
//! (both stable for the life of the file on ZFS) and encodes file handles
//! **without a generation number**, with a fixed `serverid`. Folding in `st_dev`
//! means a single export spanning sibling ZFS datasets (which each have an
//! independent inode namespace) never aliases. Handles therefore survive a daemon
//! restart. A handle the server has not yet resolved to a path (e.g. a deep path
//! cached by a client across a restart) returns `NFS3ERR_STALE`, prompting the
//! client to re-resolve from the mount root.
//!
//! ## Limitations (honest, per NFSv3 + this implementation)
//!
//! - **No NLM/NSM locking** — clients must mount with `nolock`; locks are
//!   client-local only.
//! - **NFSv3 only** — no v4/pNFS/delegations/Kerberos.
//! - **No per-export client ACL / authentication** — there is no per-client
//!   access check; gate access at the network layer (bind address / firewall).
//!   The AUTH_UNIX credential *is* honored for **ownership**: a created file/dir
//!   is chowned to the calling uid (group left to set-GID inheritance), and a
//!   SETATTR chown is applied — but the credential is trusted, not verified.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::SeekFrom;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use nessie_nfsserve::UnixCred;
use nessie_nfsserve::fs_util::{metadata_to_fattr3, path_setattr};
use nessie_nfsserve::nfs::{
    fattr3, fileid3, filename3, nfs_fh3, nfspath3, nfsstat3, sattr3, set_gid3, set_uid3,
};
use nessie_nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nessie_nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

/// The set-GID bit (`S_ISGID`) on a directory mode: children inherit the
/// directory's group (and, for subdirectories, the set-GID bit itself). This is
/// the mechanism behind the shared pod/host workspace contract (a mode-2775
/// volume root), so we must not clobber the inherited group when chowning.
const S_ISGID: u32 = 0o2000;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// A fixed `serverid`/cookieverf so file handles stay valid across restarts.
const SERVER_VERIFIER: [u8; 8] = *b"nessieV1";

fn io_to_nfs(e: &std::io::Error) -> nfsstat3 {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => nfsstat3::NFS3ERR_NOENT,
        PermissionDenied => nfsstat3::NFS3ERR_ACCES,
        AlreadyExists => nfsstat3::NFS3ERR_EXIST,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

/// The SplitMix64 finalizer — a fast, well-distributed bijective bit-mixer.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Derive a stable NFS fileid from a file's `(st_dev, st_ino)` pair.
///
/// Each ZFS dataset is its own filesystem with an independent inode namespace, so
/// a parent volume and its clones reuse low inode numbers. Keying handles on
/// `st_ino` alone (as the upstream examples do) makes those reused inodes **alias**
/// when a single export spans sibling datasets — a client reading one clone could
/// get another's file, or `NFS3ERR_STALE`. Folding `st_dev` in keeps the fileid
/// distinct across datasets; it is stable for the life of the file (both
/// components are), so handles still survive a daemon restart (F4).
fn fileid_of(meta: &std::fs::Metadata) -> fileid3 {
    fileid_from(meta.dev(), meta.ino())
}

/// The pure `(dev, ino) -> fileid` core of [`fileid_of`], split out so the
/// cross-device aliasing property is unit-testable without a real second
/// filesystem.
fn fileid_from(dev: u64, ino: u64) -> fileid3 {
    splitmix64(dev ^ splitmix64(ino))
}

/// An NFSv3 filesystem that passes operations through to a real directory tree.
pub struct PassthroughFs {
    root: PathBuf,
    /// fileid (= inode number) -> on-disk path. Populated lazily as the tree is
    /// traversed; a miss yields `NFS3ERR_STALE` so the client re-resolves.
    map: Mutex<HashMap<fileid3, PathBuf>>,
    root_id: fileid3,
}

impl PassthroughFs {
    /// Build a passthrough server rooted at `root` (which must exist).
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into().canonicalize()?;
        let root_id = fileid_of(&std::fs::symlink_metadata(&root)?);
        let mut map = HashMap::new();
        map.insert(root_id, root.clone());
        Ok(Self {
            root,
            map: Mutex::new(map),
            root_id,
        })
    }

    fn path_of(&self, id: fileid3) -> Result<PathBuf, nfsstat3> {
        self.map
            .lock()
            .expect("map lock")
            .get(&id)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    /// Register `path` and return its (stable, `(dev,ino)`-derived) fileid.
    fn register(&self, path: &Path) -> Result<(fileid3, std::fs::Metadata), nfsstat3> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| io_to_nfs(&e))?;
        let id = fileid_of(&meta);
        self.map
            .lock()
            .expect("map lock")
            .insert(id, path.to_path_buf());
        Ok((id, meta))
    }

    /// Resolve a single path component against a directory, refusing escape.
    fn child_path(&self, dir: &Path, name: &[u8]) -> Result<PathBuf, nfsstat3> {
        if name.is_empty() || name == b"." {
            return Ok(dir.to_path_buf());
        }
        if name == b".." {
            // Clamp at the export root — never traverse above it.
            return Ok(if dir == self.root {
                self.root.clone()
            } else {
                dir.parent().unwrap_or(&self.root).to_path_buf()
            });
        }
        if name.contains(&b'/') || name.contains(&0) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        Ok(dir.join(OsStr::from_bytes(name)))
    }

    /// Best-effort: own a freshly created object (`id`, inside directory `dirid`)
    /// as the calling client (F5) instead of the daemon (`root:root`).
    ///
    /// The owner (uid) is set to the caller. The group (gid) is left to the
    /// kernel's set-GID inheritance when the parent directory carries `S_ISGID`
    /// (the shared pod/host workspace contract: a mode-2775 root makes children
    /// inherit the shared group) — otherwise it is set to the caller's primary
    /// gid. A chown failure (e.g. the daemon lacks `CAP_CHOWN`) is logged, not
    /// fatal: the object is still created, just owned by the daemon, so a
    /// misconfigured deployment degrades rather than failing every create.
    fn chown_new(&self, id: fileid3, dirid: fileid3, cred: &UnixCred) {
        let (Ok(path), Ok(parent)) = (self.path_of(id), self.path_of(dirid)) else {
            return;
        };
        let parent_setgid = std::fs::symlink_metadata(&parent)
            .map(|m| m.mode() & S_ISGID != 0)
            .unwrap_or(false);
        let gid = if parent_setgid { None } else { Some(cred.gid) };
        if let Err(e) = std::os::unix::fs::chown(&path, Some(cred.uid), gid) {
            tracing::warn!(
                path = %path.display(),
                uid = cred.uid,
                error = %e,
                "nessie-nfs: could not chown new object to caller (need CAP_CHOWN?); leaving daemon ownership",
            );
        }
    }
}

#[async_trait]
impl NFSFileSystem for PassthroughFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        self.root_id
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = self.path_of(dirid)?;
        let child = self.child_path(&dir, filename.as_ref())?;
        let (id, _meta) = self.register(&child)?;
        Ok(id)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|e| io_to_nfs(&e))?;
        Ok(metadata_to_fattr3(id, &meta))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        path_setattr(&path, &setattr).await?;
        // path_setattr deliberately ignores uid/gid; honor an explicit client
        // chown here so SETATTR ownership changes actually take effect (F5). A
        // failure (e.g. no CAP_CHOWN) is surfaced — the client asked for this.
        let uid = match setattr.uid {
            set_uid3::uid(u) => Some(u),
            _ => None,
        };
        let gid = match setattr.gid {
            set_gid3::gid(g) => Some(g),
            _ => None,
        };
        if uid.is_some() || gid.is_some() {
            std::os::unix::fs::chown(&path, uid, gid).map_err(|e| io_to_nfs(&e))?;
        }
        self.getattr(id).await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.path_of(id)?;
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let len = file.metadata().await.map_err(|e| io_to_nfs(&e))?.len();
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let mut buf = vec![0u8; count as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = file
                .read(&mut buf[filled..])
                .await
                .map_err(|e| io_to_nfs(&e))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        let eof = offset.saturating_add(filled as u64) >= len;
        Ok((buf, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        // An UNSTABLE write: durability is deferred to a later COMMIT.
        let (attr, _stable) = self.write_stable(id, offset, data, false).await?;
        Ok(attr)
    }

    async fn write_stable(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
        stable: bool,
    ) -> Result<(fattr3, bool), nfsstat3> {
        let path = self.path_of(id)?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| io_to_nfs(&e))?;
        file.write_all(data).await.map_err(|e| io_to_nfs(&e))?;
        file.flush().await.map_err(|e| io_to_nfs(&e))?;
        if stable {
            // FILE_SYNC/DATA_SYNC: the data must be on stable storage before we
            // acknowledge. fdatasync flushes the file's dirty pages to disk.
            file.sync_data().await.map_err(|e| io_to_nfs(&e))?;
        }
        // Otherwise this is an UNSTABLE write — left in the page cache until the
        // client issues COMMIT (see `commit`). We honestly report `stable` so the
        // server never claims FILE_SYNC for data that is not yet durable.
        Ok((self.getattr(id).await?, stable))
    }

    async fn commit(&self, id: fileid3, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        // fsync flushes *all* of the inode's dirty pages, regardless of which
        // descriptor (or a now-closed one from an earlier UNSTABLE write) dirtied
        // them — so opening the path fresh and syncing it is sufficient and keeps
        // this server stateless. A client `fsync()` returns only after this lands.
        let path = self.path_of(id)?;
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        file.sync_data().await.map_err(|e| io_to_nfs(&e))?;
        Ok(())
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = self.child_path(&dir, filename.as_ref())?;
        tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let _ = path_setattr(&path, &attr).await;
        let (id, meta) = self.register(&path)?;
        Ok((id, metadata_to_fattr3(id, &meta)))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = self.child_path(&dir, filename.as_ref())?;
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let (id, _meta) = self.register(&path)?;
        Ok(id)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = self.child_path(&dir, dirname.as_ref())?;
        tokio::fs::create_dir(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let (id, meta) = self.register(&path)?;
        Ok((id, metadata_to_fattr3(id, &meta)))
    }

    async fn create_with_cred(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
        cred: &UnixCred,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let (id, _) = self.create(dirid, filename, attr).await?;
        self.chown_new(id, dirid, cred);
        Ok((id, self.getattr(id).await?))
    }

    async fn create_exclusive_with_cred(
        &self,
        dirid: fileid3,
        filename: &filename3,
        cred: &UnixCred,
    ) -> Result<fileid3, nfsstat3> {
        let id = self.create_exclusive(dirid, filename).await?;
        self.chown_new(id, dirid, cred);
        Ok(id)
    }

    async fn mkdir_with_cred(
        &self,
        dirid: fileid3,
        dirname: &filename3,
        cred: &UnixCred,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let (id, _) = self.mkdir(dirid, dirname).await?;
        // A new directory under a set-GID parent already inherits the parent's
        // group and the set-GID bit (kernel behavior); chown_new preserves that.
        self.chown_new(id, dirid, cred);
        Ok((id, self.getattr(id).await?))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = self.child_path(&dir, filename.as_ref())?;
        let meta = std::fs::symlink_metadata(&path).map_err(|e| io_to_nfs(&e))?;
        if meta.is_dir() {
            tokio::fs::remove_dir(&path)
                .await
                .map_err(|e| io_to_nfs(&e))?;
        } else {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| io_to_nfs(&e))?;
        }
        self.map.lock().expect("map lock").remove(&fileid_of(&meta));
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_dir = self.path_of(from_dirid)?;
        let to_dir = self.path_of(to_dirid)?;
        let from = self.child_path(&from_dir, from_filename.as_ref())?;
        let to = self.child_path(&to_dir, to_filename.as_ref())?;
        tokio::fs::rename(&from, &to)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let _ = self.register(&to);
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir = self.path_of(dirid)?;
        let dmeta = std::fs::symlink_metadata(&dir).map_err(|e| io_to_nfs(&e))?;
        if !dmeta.is_dir() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        // Order entries by stable fileid (a (dev,ino) hash) so the NFS cookie
        // (start_after) is meaningful and pagination never duplicates or drops
        // entries.
        let mut by_id: BTreeMap<fileid3, PathBuf> = BTreeMap::new();
        let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| io_to_nfs(&e))?;
        while let Some(ent) = rd.next_entry().await.map_err(|e| io_to_nfs(&e))? {
            let meta = match ent.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            by_id.insert(fileid_of(&meta), ent.path());
        }
        let total_after = by_id.range((start_after + 1)..).count();
        let mut entries = Vec::new();
        for (id, path) in by_id.range((start_after + 1)..) {
            if entries.len() >= max_entries {
                break;
            }
            let meta = std::fs::symlink_metadata(path).map_err(|e| io_to_nfs(&e))?;
            self.map.lock().expect("map lock").insert(*id, path.clone());
            let name = path
                .file_name()
                .map(|n| filename3::from(n.as_bytes()))
                .unwrap_or_else(|| filename3::from(&b""[..]));
            entries.push(DirEntry {
                fileid: *id,
                name,
                attr: metadata_to_fattr3(*id, &meta),
            });
        }
        let end = entries.len() == total_after;
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = self.child_path(&dir, linkname.as_ref())?;
        let target = PathBuf::from(OsStr::from_bytes(symlink.as_ref()));
        tokio::fs::symlink(&target, &path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        let (id, meta) = self.register(&path)?;
        Ok((id, metadata_to_fattr3(id, &meta)))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let path = self.path_of(id)?;
        let target = tokio::fs::read_link(&path)
            .await
            .map_err(|e| io_to_nfs(&e))?;
        Ok(nfspath3::from(target.as_os_str().as_bytes()))
    }

    // --- Stable file handles: encode the fileid with no generation number, and
    // a fixed serverid, so handles survive a daemon restart. ---

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        nfs_fh3 {
            data: id.to_le_bytes().to_vec(),
        }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        let bytes: [u8; 8] = fh
            .data
            .as_slice()
            .try_into()
            .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?;
        Ok(fileid3::from_le_bytes(bytes))
    }

    fn serverid(&self) -> nessie_nfsserve::nfs::cookieverf3 {
        SERVER_VERIFIER
    }
}

/// Serve `root` over NFSv3 on `bind` (e.g. `"0.0.0.0:2049"`), forever.
///
/// `export_name` is the path clients mount (`<host>:/<export_name>`); use `""`
/// for the bare root (`<host>:/`). Returns only on a fatal listener error.
pub async fn serve(root: impl Into<PathBuf>, bind: &str, export_name: &str) -> std::io::Result<()> {
    let fs = PassthroughFs::new(root)?;
    let mut listener = NFSTcpListener::bind(bind, fs).await?;
    if !export_name.is_empty() {
        listener.with_export_name(export_name);
    }
    tracing::info!(%bind, export = %export_name, "nessie-nfs: serving NFSv3 (userspace, no host kernel)");
    listener.handle_forever().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_handle_roundtrips_without_generation() {
        // A handle must decode back to the same fileid — and crucially does NOT
        // embed a per-process generation, so it stays valid across restarts.
        let fs = dummy();
        for id in [1u64, 42, u64::MAX, 0xDEAD_BEEF] {
            let fh = fs.id_to_fh(id);
            assert_eq!(fh.data.len(), 8, "handle is just the 8-byte fileid");
            assert_eq!(fs.fh_to_id(&fh).unwrap(), id);
        }
    }

    #[test]
    fn fileid_distinguishes_same_inode_across_devices() {
        // F4: the same st_ino on two different ZFS datasets (distinct st_dev) must
        // map to DIFFERENT fileids, or a single export aliases sibling clones.
        let ino = 34usize as u64; // a low inode reused across datasets
        assert_ne!(
            fileid_from(0x10, ino),
            fileid_from(0x11, ino),
            "same inode on different devices must not collide"
        );
        // And distinct inodes on the same device differ too.
        assert_ne!(fileid_from(0x10, 34), fileid_from(0x10, 35));
    }

    #[test]
    fn fileid_is_deterministic() {
        // Stability across calls (and thus across restarts) is what keeps handles
        // valid: the same (dev, ino) always yields the same fileid.
        assert_eq!(fileid_from(0x42, 1000), fileid_from(0x42, 1000));
        assert_eq!(splitmix64(12345), splitmix64(12345));
        assert_ne!(splitmix64(0), splitmix64(1));
    }

    #[test]
    fn bad_handle_length_is_rejected() {
        let fs = dummy();
        let bad = nfs_fh3 {
            data: vec![1, 2, 3],
        };
        assert!(matches!(
            fs.fh_to_id(&bad),
            Err(nfsstat3::NFS3ERR_BADHANDLE)
        ));
    }

    #[test]
    fn serverid_is_fixed() {
        // A fixed serverid is what lets clients keep their handles across a
        // restart instead of getting NFS3ERR_STALE.
        assert_eq!(dummy().serverid(), SERVER_VERIFIER);
    }

    // A PassthroughFs whose root we never touch — fine for pure handle-codec
    // tests (no filesystem access), honoring the no-real-fs-in-unit-tests rule.
    fn dummy() -> PassthroughFs {
        PassthroughFs {
            root: PathBuf::from("/nonexistent"),
            map: Mutex::new(HashMap::new()),
            root_id: 1,
        }
    }
}
