# nessie-store

The daemon at the heart of
[nessie-store](https://github.com/Gilamonster-Foundation/nessie-store) — it
speaks a faithful subset of the NetApp ONTAP REST API over a pluggable storage
backend. This crate is the daemon **binary** and its **library**; the pip wheel
exposes the library's config + identity surface for automation.

## Install

```bash
pip install nessie-store          # Python: config + identity tooling
cargo install nessie-store        # Rust: the daemon binary
```

## Run the daemon

```bash
nessie-store init --config config.toml      # write a default config
nessie-store serve --config config.toml     # serve ONTAP REST (HTTPS)
```

## Usage (Python)

Generate and validate the daemon's `config.toml` from Python, and mint the
stable cluster identity — handy for the container/k8s deploy paths and CI:

```python
from nessie_store import Config, mint_identity

# Build a config from a dict; unspecified keys take their defaults.
cfg = Config.from_dict({"backend": "zfs", "zfs_pool": "tank", "data_lif": "192.168.1.100"})
print(cfg.to_toml())                 # ready for `nessie-store serve --config`

# Round-trip / validate an existing file.
parsed = Config.from_toml(open("config.toml").read())
assert parsed.to_dict()["backend"] in ("mem", "zfs")

# Mint the stable UUIDs the control plane reports.
ident = mint_identity()              # {'cluster_uuid': ..., 'svm_uuid': ..., ...}
```

`Config` methods raise `NessieError` on bad input.

## Usage (Rust)

```rust
use nessie_store::config::Config;
use nessie_store::identity::Identity;

let cfg = Config::default();          // mem backend, 0.0.0.0:8443
println!("{}", cfg.to_toml());
let ident = Identity::mint();         // fresh cluster/svm/node/aggregate/lif UUIDs
```

The daemon binary builds the axum router via `nessie_store::app(state)`; tests
drive it in-process.

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
