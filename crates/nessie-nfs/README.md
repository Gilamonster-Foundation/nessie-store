# nessie-nfs

The **embedded userspace NFSv3 server** for
[nessie-store](https://github.com/Gilamonster-Foundation/nessie-store) — it
exports a real on-disk directory tree over NFS **in-process**, with **no host
kernel NFS server**, no `rpc.nfsd`, no `exportfs`, and no `rpcbind`/portmapper.
Built on the [`nfsserve`](https://github.com/huggingface/nfsserve) wire layer;
this crate supplies the filesystem that maps NFS ops onto `tokio` file I/O.

It is the data plane for the ZFS backend: the daemon serves the ZFS dataset
mountpoints itself, so an operator no longer needs `nfs-kernel-server` installed.

## Usage (Rust)

```rust
use nessie_nfs::serve;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Export /srv/ontap over NFSv3 on 0.0.0.0:2049; clients mount `<host>:/`.
    serve("/srv/ontap", "0.0.0.0:2049", "").await
}
```

Or drive the filesystem directly (e.g. to compose it yourself):

```rust
use nessie_nfs::PassthroughFs;
use nfsserve::vfs::NFSFileSystem;

let fs = PassthroughFs::new("/srv/ontap")?;
let root = fs.root_dir();
let attrs = fs.getattr(root).await?;
# Ok::<(), nfsserve::nfs::nfsstat3>(())
```

## Mounting (client)

A standard Linux client mounts it with an explicit fixed port — no rpcbind:

```bash
sudo mount -t nfs \
  -o nfsvers=3,proto=tcp,port=2049,mountport=2049,nolock,noacl \
  <host>:/ /mnt/point
```

`port` + `mountport` eliminate every portmapper lookup; `nolock` is required
(there is no NLM lock manager). For Kubernetes/Trident, set the same options via
`nfsMountOptions` on an NFSv3 backend.

## What it is / isn't

- **Stable file handles** across daemon restarts: the NFS fileid is the
  underlying inode (`st_ino`) and handles carry no per-process generation, so
  mounts don't go `ESTALE` on restart.
- **NFSv3 only**; **no NLM/NSM locking** (mount `nolock`); **no per-export ACL /
  auth** (AUTH_UNIX/anon — gate access at the network layer).

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
Depends on `nfsserve` (BSD-3-Clause).
