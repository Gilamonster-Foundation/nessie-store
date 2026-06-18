# nessie-backend-mem

The in-memory reference backend for
[nessie-store](https://github.com/Gilamonster-Foundation/nessie-store) — a
`HashMap`-backed implementation of the full `VolumeBackend ⊂ SnapshotBackend ⊂
CloneBackend` stack. It passes the full conformance suite and needs no privileges
or external services, so it's the zero-dependency way to script against an
ONTAP-shaped backend.

## Install

```bash
pip install nessie-backend-mem      # Python
cargo add nessie-backend-mem        # Rust
```

## Usage (Python)

```python
from nessie_backend_mem import MemBackend

b = MemBackend()
vol = b.create_volume("build-cache", size_bytes=50 * 2**30)
snap = b.create_snapshot(vol["uuid"], "pre-build")
clone = b.create_clone(vol["uuid"], snap["uuid"], "build-cache-pr1234")
assert clone["clone"]["parent_volume"] == "build-cache"
```

Methods return plain dicts in the substrate-neutral **domain** shape (`size_bytes`,
`vol_type`, `clone={parent_volume, parent_snapshot}`) and raise `NessieError` on
failure. The ONTAP **wire** shape (`size`, `type`, `clone.is_flexclone`, HAL
`_links`) is produced by `nessie-ontap-protocol`.

## Usage (Rust)

```rust
use nessie_backend_core::{VolumeBackend, VolumeSpec};
use nessie_backend_mem::MemBackend;

let b = MemBackend::new();
let vol = b.create_volume(VolumeSpec::named("build-cache")).unwrap();
```

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
