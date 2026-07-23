<p align="center">
  <img src="docs/logos/nessie-256.png" alt="nessie-store mascot" width="256" height="256">
</p>

# nessie-store

**Speaks the ONTAP REST API. Runs on whatever storage you actually have.**

A Rust daemon that exposes a faithful subset of NetApp's published [ONTAP REST API](https://docs.netapp.com/us-en/ontap-restapi/) over a pluggable storage backend. **ZFS works today**; S3, real ONTAP arrays, bare NFS, Perforce, and git-lfs are planned — pick the substrate that fits the environment, keep the API.

Sister project to [monty-tui](https://github.com/hartsock/monty-tui) and [scrybe](https://github.com/hartsock/scrybe). They share toolchain, code-style discipline, and release cadence.

## Status

**v0.3.1 — early development, public.** The ONTAP REST control plane and one real
substrate (ZFS) work end to end, alongside an embedded userspace NFSv3 data plane.
The [crate inventory](#crate-inventory) is the source of truth for what is
implemented today versus planned. Pre-1.0, the protocol surface may still shift.

[`netapp-sim`](https://github.com/hartsock/netapp-sim) is the Python predecessor — it implements the same ONTAP REST surface but is locked to a single ZFS backend. nessie-store inherits the protocol fidelity and adds the trait abstraction `netapp-sim` never had, so the same API can drive any substrate.

## What it is

A daemon that:

- **Speaks real ONTAP REST.** Paths like `/api/storage/volumes`, `/api/storage/volumes/{uuid}/snapshots`, `/api/snapmirror/relationships`. HAL response shape with `_links` and `records`/`num_records`. Existing clients — `netapp.ontap` Ansible collection, the NetApp Terraform provider, `netapp-ontap-python` SDK — plug in without modification.
- **Dispatches to a pluggable backend.** Volumes, snapshots, and FlexClones map onto whatever the substrate can do natively. ZFS gets `zfs snapshot` + `zfs clone` (implemented today). The same rule shapes the planned backends: S3 → versioned objects + `copy_object`; a real ONTAP cluster → passthrough proxy; git-lfs → refs as volumes, tags as snapshots.
- **Honors substrate capabilities.** A backend that can't take snapshots (bare NFS) advertises that fact via its `capabilities()` declaration; the REST surface returns the documented ONTAP "feature not licensed" response for ops the substrate can't honor. No silent emulation, no lying about atomicity.
- **Lets backends own their data plane.** Reads and writes don't flow through the daemon's control plane. Each backend returns an `AccessHandle` — an NFS export for ZFS, a presigned URL for S3, a git remote for git-lfs — and the client mounts/fetches directly. Matches how real ONTAP works (control plane and data plane are separate networks).

## Why

Tools that need ONTAP semantics — snapshot, clone, replicate — shouldn't be locked to ONTAP hardware. Tools that need cheap storage shouldn't have to learn six different APIs to use it. nessie-store collapses both: write to one API; the operator picks the substrate at deployment time.

This is the telescope-not-the-sky pattern from the workspace design philosophy. The protocol stays narrow and well-known; the things that change underneath stay decoupled.

## Architecture

The workspace splits into a backend-trait core, a conformance harness, the REST
protocol layer, the daemon, and an embedded NFS data plane. The crates that exist
today:

```
crates/
  nessie-backend-core/        Trait: VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend.
                              Types: Volume, Snapshot, VolumeSpec, AccessHandle,
                              BackendError, Capabilities. NO implementations.
  nessie-backend-conformance/ Generic test harness. Per-capability suites
                              (volume / snapshot / clone). Any backend impl
                              must pass the suites its capabilities() promises.
  nessie-backend-mem/         HashMap-backed reference impl. Passes every suite.
                              Used by daemon unit tests + the substrate sanity
                              check in CI.
  nessie-backend-zfs/         zfs/zpool subprocess backend. CloneBackend.
                              Native snapshot + native clone.
  nessie-ontap-protocol/      REST handlers + HAL JSON shapes. Byte-identical
                              to the ONTAP REST spec.
  nessie-store/               Daemon binary. axum HTTP server, TOML config,
                              CLI (`init`, `serve`), backend dispatch, HTTP
                              Basic auth, TLS, mint-once identity.
  nessie-nfsserve/            Vendored, hardened fork of the `nfsserve` userspace
                              NFSv3 server (durable writes + NFSPROC3_COMMIT +
                              AUTH_UNIX ownership fixes upstream can't express).
  nessie-nfs/                 Embedded NFSv3 data-plane server (PassthroughFs),
                              wired into the daemon. No host kernel NFS needed.
```

Build with `cargo build --release`. Run with `nessie-store serve --config /etc/nessie/config.toml`. The default config selects the in-memory backend; flip to `[backend.zfs]` when there are real bytes to store.

### Crate inventory

The single source of truth for implementation status. A drift check
([`scripts/check-doc-drift.sh`](scripts/check-doc-drift.sh)) — run by the pre-push
hook and `just doc-check` — asserts this inventory matches `crates/` on disk, so it
cannot silently drift out of sync.

**Implemented** — in the workspace today:

<!-- crate-inventory:implemented BEGIN -->
| Crate | Role |
| --- | --- |
| `nessie-backend-core` | trait stack + types + `AccessHandle` (no impls) |
| `nessie-backend-conformance` | capability-driven test harness |
| `nessie-cas-conformance` | conformance suite for the `CasBackend` contract |
| `nessie-ac-conformance` | conformance suite for the `ActionCacheBackend` contract |
| `nessie-cas-store` | retention engine: reachability GC + replica-gated cache eviction |
| `nessie-reapi` | Bazel REAPI v2 (cache subset) gRPC face over the CAS/AC backends |
| `nessie-backend-mem` | HashMap reference backend |
| `nessie-backend-zfs` | ZFS substrate — native snapshot + clone |
| `nessie-ontap-protocol` | HAL / ONTAP REST wire shapes |
| `nessie-store` | axum daemon: REST, config, auth, TLS, identity |
| `nessie-nfsserve` | vendored, hardened userspace NFSv3 server |
| `nessie-nfs` | embedded NFSv3 data plane |
<!-- crate-inventory:implemented END -->

**Planned** — described here as design intent; **not yet implemented**:

<!-- crate-inventory:planned BEGIN -->
| Crate | Role | Tracking |
| --- | --- | --- |
| `nessie-backend-s3` | object store: versioned objects, lazy clones | #72 |
| `nessie-backend-nfs` | bare NFS mount — VolumeBackend only | #72 |
| `nessie-backend-ontap` | passthrough proxy to a real ONTAP cluster | #72 |
| `nessie-backend-p4d` | Perforce streams / labels / sparse clones | #72, #71 |
| `nessie-backend-gitlfs` | git refs / tags, `git clone --reference` clones | #72, #71 |
| `nessie-client` | typed Rust client + PyO3 wheel | #72 |
<!-- crate-inventory:planned END -->

> **SnapMirror is control-plane only today.** Relationships, peers, and transfers
> are tracked and a real source snapshot is taken per transfer, but the
> cross-instance `zfs send → zfs receive` data movement is not yet wired (#69).
> The current PyO3 wheels are per-crate bindings, not the unified `nessie-client`
> above. Two experimental demand-paged NFS gateways (git-LFS, Perforce) are
> designed but unbuilt (#71).

## What this is not

The supported surface covers volumes, snapshots, FlexClones, and SnapMirror (control plane — see the data-plane note above). LUNs, qtrees, ACL models, multi-SVM tenancy, SnapVault — none of those are in scope. The Python predecessor scoped itself the same way for the same reason: this is the slice that's load-bearing for content-addressable and snapshot-driven workflows. Anything outside that slice is a non-goal until a real use case forces it back in.

## Install

```bash
# Rust daemon — from source until the first crates.io release
cargo install --git https://github.com/Gilamonster-Foundation/nessie-store nessie-store

# Python client — planned, not yet published (see nessie-client, #72)
# pip install nessie-store-client
```

## Run

```bash
nessie-store init --config-dir ~/.config/nessie     # write a default config
nessie-store serve --config ~/.config/nessie/config.toml
```

A minimal config:

```toml
listen = "0.0.0.0:8443"

[backend.zfs]
pool = "tank"
```

The in-memory backend is the default when no `[backend.*]` is set. The `[backend.s3]`
and `[backend.ontap]` blocks below describe **planned** backends (#72) — the config
schema is reserved, but the substrates are not yet implemented:

```toml
# planned — not yet implemented (#72)
[backend.s3]
endpoint = "https://s3.amazonaws.com"
bucket = "my-volumes"
region = "us-east-1"
```

```toml
# planned — not yet implemented (#72)
[backend.ontap]
cluster_mgmt_lif = "https://ontap-01.example.com/"
auth_secret_ref = "vault:secret/ontap/cluster-01/api"
```

## Build from source

```bash
just check     # fmt + clippy + tests + doc-drift for everything
just build     # cargo build --release
just maturin   # build the python wheel (when nessie-client ships one)
```

## Design philosophy

The daemon doesn't own bytes. It owns *the API*. Bytes live wherever the operator chose to put them; the trait just enforces enough discipline that a snapshot is a snapshot and a clone diverges. The substrate handles the substrate's job.

This is the telescope-not-the-sky pattern from the workspace CLAUDE.md. The instrument (the REST surface) stays well-known and replaceable. The data (volumes, snapshots, clones) is sovereign and lives where it lives.

## License

Dual-licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT)
at your option — the Gilamonster Foundation convention. Unless you explicitly
state otherwise, any contribution intentionally submitted for inclusion in this
crate by you, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.

## Related

- [NetApp ONTAP REST API spec](https://docs.netapp.com/us-en/ontap-restapi/) — the surface nessie-store mirrors.
