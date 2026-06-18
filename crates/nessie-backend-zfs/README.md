# nessie-backend-zfs

The ZFS-backed storage backend for
[nessie-store](https://github.com/Gilamonster-Foundation/nessie-store) — the
`VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend` stack mapped onto
`zfs`/`zpool`/`exportfs`. Volumes are datasets, snapshots are `zfs snapshot`,
FlexClones are `zfs clone`, and the data plane is an NFS export. Every command
goes through a `CommandRunner` seam, so it is exact and testable.

## Install

```bash
pip install nessie-backend-zfs      # Python
cargo add nessie-backend-zfs        # Rust
```

## Usage (Python)

Two extension points. **Outside** — drive real ZFS (needs `zfs`/`zpool` +
privilege):

```python
from nessie_backend_zfs import ZfsBackend

b = ZfsBackend(pool="ontap-sim", data_lif="192.168.1.100")
vol = b.create_volume("build-cache", size_bytes=10 * 2**30)
snap = b.create_snapshot(vol["uuid"], "pre-build")
clone = b.create_clone(vol["uuid"], snap["uuid"], "build-cache-pr1234")
print(b.access_handle(vol["uuid"]))   # {'kind': 'nfs_export', 'server': ..., 'path': ...}
```

**Inside** — pass a `runner(argv) -> {"success", "stdout", "stderr"}` callable
and every command is routed through *your* function. Mock it for tests, audit
it, or wrap it in `sudo`:

```python
from nessie_backend_zfs import ZfsBackend

def runner(argv):
    print("nessie ran:", " ".join(argv))      # audit every command
    return {"success": True, "stdout": "", "stderr": ""}

b = ZfsBackend(pool="tank", runner=runner)
vol = b.create_volume("vol1", size_bytes=1 << 30)   # prints: nessie ran: zfs create -o quota=...
```

Methods return plain dicts in the substrate-neutral **domain** shape and raise
`NessieError` on failure.

## Usage (Rust)

```rust
use nessie_backend_core::{VolumeBackend, VolumeSpec};
use nessie_backend_zfs::{SystemRunner, ZfsBackend, ZfsConfig};

let cfg = ZfsConfig { pool: "ontap-sim".into(), ..ZfsConfig::default() };
let b = ZfsBackend::new(SystemRunner, cfg);
let vol = b.create_volume(VolumeSpec::named("build-cache")).unwrap();
```

The `CommandRunner` trait is the same seam the Python `runner` rides on — swap
`SystemRunner` for a mock to assert the exact argv in unit tests.

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
