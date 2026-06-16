# nessie-backend-core

Core domain vocabulary for [nessie-store](https://github.com/Gilamonster-Foundation/nessie-store)
— a cheap ONTAP on-ramp. This crate defines the substrate-neutral types and the
`VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend` supertrait stack that every
storage backend implements. The PyO3 bindings expose the domain vocabulary to
Python so you can script against the system (the "outside" extension surface).

## Install

```bash
pip install nessie-backend-core      # Python
cargo add nessie-backend-core        # Rust
```

## Usage (Python)

```python
import nessie_backend_core as core

# A request to create a 50 GiB volume.
spec = core.VolumeSpec("build-cache", size_bytes=50 * 2**30)
print(spec)                       # VolumeSpec(name="build-cache", size_bytes=...)

# Declare what a backend can do — and check the tiers are self-consistent.
caps = core.Capabilities(snapshots=True, clones=True)
assert caps.is_consistent()
assert caps.snapshots and caps.clones

# A sparse PATCH (resize + set the NFS junction).
patch = core.VolumePatch(size_bytes=100 * 2**30, junction_path="/build")
assert not patch.is_empty()
```

## Usage (Rust)

```rust
use nessie_backend_core::{Capabilities, VolumeSpec};

let spec = VolumeSpec::named("build-cache");
assert!(Capabilities::clones().is_consistent());
```

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
