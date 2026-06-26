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
//! server derives the NFS fileid from the underlying **inode number** (`st_ino`,
//! stable for the life of the file on ZFS) and encodes file handles **without a
//! generation number**, with a fixed `serverid`. Handles therefore survive a
//! daemon restart. A handle for an inode the server has not yet resolved to a
//! path (e.g. a deep path cached by a client across a restart) returns
//! `NFS3ERR_STALE`, prompting the client to re-resolve from the mount root.
//!
//! ## Limitations (honest, per NFSv3 + this implementation)
//!
//! - **No NLM/NSM locking** — clients must mount with `nolock`; locks are
//!   client-local only.
//! - **NFSv3 only** — no v4/pNFS/delegations/Kerberos.
//! - **No per-export client ACL / authentication** — it is AUTH_UNIX/anon; gate
//!   access at the network layer (bind address / firewall) for now.

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
use nessie_nfsserve::fs_util::{metadata_to_fattr3, path_setattr};
use nessie_nfsserve::nfs::{fattr3, fileid3, filename3, nfs_fh3, nfspath3, nfsstat3, sattr3};
use nessie_nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nessie_nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
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
        let root_id = std::fs::symlink_metadata(&root)?.ino();
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

    /// Register `path` and return its (stable, inode-derived) fileid.
    fn register(&self, path: &Path) -> Result<(fileid3, std::fs::Metadata), nfsstat3> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| io_to_nfs(&e))?;
        let id = meta.ino();
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
        self.map.lock().expect("map lock").remove(&meta.ino());
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
        // Order entries by stable fileid (inode) so the NFS cookie (start_after)
        // is meaningful and pagination never duplicates or drops entries.
        let mut by_id: BTreeMap<fileid3, PathBuf> = BTreeMap::new();
        let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| io_to_nfs(&e))?;
        while let Some(ent) = rd.next_entry().await.map_err(|e| io_to_nfs(&e))? {
            let meta = match ent.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            by_id.insert(meta.ino(), ent.path());
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
