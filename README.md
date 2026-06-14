# nessie-store

**Speaks the ONTAP REST API. Runs on whatever storage you actually have.**

A Rust daemon that exposes a faithful subset of NetApp's published [ONTAP REST API](https://docs.netapp.com/us-en/ontap-restapi/) over a pluggable storage backend. ZFS, S3, real ONTAP arrays, NFS, Perforce, git-lfs — pick the substrate that fits the environment, keep the API.

Sister project to [monty-tui](https://github.com/hartsock/monty-tui) and [scrybe](https://github.com/hartsock/scrybe). They share toolchain, code-style discipline, and release cadence.

## Status

**v0.x — pre-release, private.** Skeleton + plan. The repo goes public when v1.0 ships and the protocol surface is stable.

[`netapp-sim`](https://github.com/hartsock/netapp-sim) is the Python predecessor — it implements the same ONTAP REST surface but is locked to a single ZFS backend. nessie-store inherits the protocol fidelity and adds the trait abstraction `netapp-sim` never had, so the same API can drive any substrate.

## What it is

A daemon that:

- **Speaks real ONTAP REST.** Paths like `/api/storage/volumes`, `/api/storage/volumes/{uuid}/snapshots`, `/api/snapmirror/relationships`. HAL response shape with `_links` and `records`/`num_records`. Existing clients — `netapp.ontap` Ansible collection, the NetApp Terraform provider, `netapp-ontap-python` SDK — plug in without modification.
- **Dispatches to a pluggable backend.** Volumes, snapshots, and FlexClones map onto whatever the substrate can do natively. ZFS gets `zfs snapshot` + `zfs clone`. S3 gets versioned objects + `copy_object`. A real ONTAP cluster gets a passthrough proxy. Git-lfs gets refs as volumes and tags as snapshots.
- **Honors substrate capabilities.** A backend that can't take snapshots (bare NFS) advertises that fact via its `capabilities()` declaration; the REST surface returns the documented ONTAP "feature not licensed" response for ops the substrate can't honor. No silent emulation, no lying about atomicity.
- **Lets backends own their data plane.** Reads and writes don't flow through the daemon. Each backend returns an `AccessHandle` — an NFS export for ZFS, a presigned URL for S3, a git remote for git-lfs — and the client mounts/fetches directly. Matches how real ONTAP works (control plane and data plane are separate networks).

## Why

Tools that need ONTAP semantics — snapshot, clone, replicate — shouldn't be locked to ONTAP hardware. Tools that need cheap storage shouldn't have to learn six different APIs to use it. nessie-store collapses both: write to one API; the operator picks the substrate at deployment time.

This is the telescope-not-the-sky pattern from the workspace design philosophy. The protocol stays narrow and well-known; the things that change underneath stay decoupled.

## Architecture

```
crates/
  nessie-backend-core/        Trait: VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend.
                              Types: Volume, Snapshot, VolumeSpec, AccessHandle,
                              BackendError, Capabilities. NO implementations.
  nessie-backend-conformance/ Generic test harness. Per-capability suites
                              (volume / snapshot / clone). Any backend impl
                              must pass the suites its capabilities() promises.
  nessie-backend-mem/         HashMap-backed reference impl. Passes every suite.
                              Used by daemon unit tests + as the substrate sanity
                              check in CI.
  nessie-backend-zfs/         zfs/zpool subprocess backend. CloneBackend.
                              Native snapshot + native clone.
  nessie-backend-s3/          object_store crate. CloneBackend.
                              Snapshots = object-version markers. Clones lazy
                              via copy_object + overlay manifest.
  nessie-backend-nfs/         Bare NFS mount. VolumeBackend only — no native
                              snapshots; backend advertises only the volume
                              capability.
  nessie-backend-ontap/       Passthrough to a real ONTAP cluster's REST.
                              CloneBackend. Lets nessie-store stand in front of
                              an ONTAP fleet as a homogenizing proxy.
  nessie-backend-p4d/         Perforce streams as volumes, labels as snapshots,
                              sparse stream copies as clones. CloneBackend.
  nessie-backend-gitlfs/      git refs as volumes, tags as snapshots,
                              `git clone --reference` as clones. CloneBackend.
  nessie-ontap-protocol/      REST handlers + HAL JSON shapes. Byte-identical
                              to the ONTAP REST spec.
  nessie-store/               Daemon binary. axum HTTP server, TOML config,
                              CLI (`init`, `serve`), backend dispatch.
  nessie-client/              Rust client + PyO3 wheel (later).
```

Build with `cargo build --release`. Run with `nessie-store serve --config /etc/nessie/config.toml`. The default config selects the in-memory backend; flip to `[backend.zfs]` or `[backend.s3]` when there are real bytes to store.

## What this is not

The supported surface covers volumes, snapshots, FlexClones, and SnapMirror (v0.2). LUNs, qtrees, ACL models, multi-SVM tenancy, SnapVault — none of those are in scope. The Python predecessor scoped itself the same way for the same reason: this is the slice that's load-bearing for content-addressable and snapshot-driven workflows. Anything outside that slice is a non-goal until a real use case forces it back in.

## Install

```bash
# Rust users
cargo install nessie-store

# Python client (later — once nessie-client lands a wheel)
pip install nessie-store-client
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

Or:

```toml
[backend.s3]
endpoint = "https://s3.amazonaws.com"
bucket = "my-volumes"
region = "us-east-1"
```

Or pass through to an existing ONTAP cluster:

```toml
[backend.ontap]
cluster_mgmt_lif = "https://ontap-01.internal/"
auth_secret_ref = "vault:secret/ontap/cluster-01/api"
```

## Build from source

```bash
just check     # fmt + clippy + tests for everything
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

- [netapp-sim](https://github.com/hartsock/netapp-sim) — Python predecessor. Same protocol surface, single ZFS backend. nessie-store inherits the protocol shape and lifts most of the operation set from `netapp-sim/src/netapp_sim/services/zfs_service.py`.
- [monty-tui](https://github.com/hartsock/monty-tui) — terminal dashboard for the swarm and the storage layer underneath.
- [NetApp ONTAP REST API spec](https://docs.netapp.com/us-en/ontap-restapi/) — the surface nessie-store mirrors.
