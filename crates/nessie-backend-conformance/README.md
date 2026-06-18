# nessie-backend-conformance

The substrate-agnostic conformance suite every
[nessie-store](https://github.com/Gilamonster-Foundation/nessie-store) backend
must pass — and the **inside extension point**: write a storage backend in
Python and validate it against the exact same suites a Rust backend passes.

## Install

```bash
pip install nessie-backend-conformance      # Python
cargo add nessie-backend-conformance         # Rust (dev-dependency)
```

## Usage (Python) — validate a Python-authored backend

```python
from nessie_backend_conformance import run_all, ConformanceError

class MyBackend:
    def capabilities(self): return {"snapshots": True, "clones": True, "replication": False}
    def create_volume(self, name, size_bytes): ...
    def get_volume(self, uuid): ...        # return None when absent
    # … list_volumes / delete_volume / patch_volume / access_handle
    # … create_snapshot / list_snapshots / get_snapshot / delete_snapshot / create_clone

run_all(MyBackend())   # raises ConformanceError on the first violation
```

Methods exchange plain **domain-shaped** dicts (`{uuid, name, state, style,
vol_type, size_bytes?, clone?}` for volumes). A Rust adapter wraps the Python
object and runs the suites by calling back into Python under the GIL. See
[`examples/python_backend.py`](examples/python_backend.py) for a complete
reference backend.

## Usage (Rust)

```rust
nessie_backend_conformance::run_all(&my_backend);  // panics on the first violation
```

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
