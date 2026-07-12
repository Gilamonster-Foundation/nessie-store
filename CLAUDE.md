# nessie-store — agent context

A Rust daemon that speaks the real ONTAP REST API in front of a pluggable storage backend. See [README.md](README.md) for the user-facing description.

The implementation is staged via small drake-swarm sorties (~50 total across 4 phases). The full plan is at `~/.claude/plans/splendid-swimming-shamir.md` on gnuc — sorties land here one at a time as drake workers complete them.

## Provenance

The Python predecessor [`netapp-sim`](https://github.com/hartsock/netapp-sim) implements the same ONTAP REST surface but is locked to a single ZFS backend (subprocess-wrapped `zfs`/`zpool` calls). nessie-store inherits the protocol fidelity verbatim and adds the trait abstraction `netapp-sim` never had.

When implementing a substrate, **read the corresponding chunk of `netapp-sim` first** — the operation list and subprocess shapes already exist there:

- `netapp-sim/src/netapp_sim/services/zfs_service.py` — canonical operation list. Every method here is a `StorageBackend` candidate. The zfs backend should follow the same subprocess patterns.
- `netapp-sim/src/netapp_sim/routes/volumes.py` — exact REST shape for volume CRUD + FlexClone. The protocol crate mirrors this path-for-path.
- `netapp-sim/src/netapp_sim/routes/snapshots.py` — snapshot REST shape.
- `netapp-sim/src/netapp_sim/routes/snapmirror.py` — replication surface (v0.2).
- `netapp-sim/src/netapp_sim/ontap.py` — HAL response envelope; the protocol crate's serde shapes match this byte-for-byte.
- `netapp-sim/tests/conftest.py` — fixture and subprocess-mocking patterns informing the Rust test harness.

## Working in this repo

- **Style guides are load-bearing.** Read [docs/STYLE_RUST.md](docs/STYLE_RUST.md), [docs/STYLE_PYTHON.md](docs/STYLE_PYTHON.md), and [docs/STYLE_PYO3.md](docs/STYLE_PYO3.md) before writing code. They describe the bar `just check` enforces.
- **TDD discipline.** Tests first. A bug fix without a regression test is incomplete. New trait implementations must pass the corresponding `nessie-backend-conformance` suite.
- **Zero warnings on `main`.** `cargo clippy -- -D warnings`, `ruff check`, `black --check`, `mypy --strict` (once python lands) all pass before merge.
- **`just check`** is the single gate. Pre-push hook runs it locally; CI runs it on push.

## Branch + commit policy

- **Agents MUST NOT push to `main`.** Only the human operator pushes/merges to main. All agent work goes through a feature branch + PR.
- One logical change per branch. Short-lived (hours to days). Split if growing.
- Drake-swarm sorties land on per-sortie branches (`s1.1/backend-core-types`, `s3-zfs.4/create-clone`, etc.) that auto-squash into per-phase PRs on green.
- Commit messages: imperative title ≤ 70 chars; body explains the *why* + how to verify.
- No `--no-verify` on git ops. Push hooks mirror CI.

## Architecture intent

See [README.md](README.md#architecture) for the crate layout. Brief version:

- `crates/nessie-backend-core/` — `VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend` supertrait stack + types + errors. NO implementations.
- `crates/nessie-backend-conformance/` — generic test harness; per-capability suites.
- `crates/nessie-backend-mem/` — HashMap reference impl. Always passes full conformance.
- `crates/nessie-backend-{zfs,s3,nfs,ontap,p4d,gitlfs}/` — substrate implementations.
- `crates/nessie-ontap-protocol/` — REST handlers + HAL JSON shapes.
- `crates/nessie-store/` — daemon binary (axum + TOML config + backend dispatch).
- `crates/nessie-client/` — Rust client + PyO3 wheel (later).

**Recommended order of operations (the implementation plan follows this exact sequence):**

1. **`nessie-backend-core`** first — settle types, the supertrait stack, the `BackendError` enum, and the `AccessHandle` enum. Nothing higher up has shape without these.
2. **`nessie-backend-mem`** — reference HashMap impl. Validates the trait against actual code.
3. **`nessie-backend-conformance`** — the test harness any substrate must pass. Mem backend is the sanity check.
4. **`nessie-ontap-protocol`** — HAL response types + REST handler shapes. Wired against `Arc<dyn VolumeBackend>` so the daemon can dispatch to any registered backend.
5. **`nessie-store`** binary — assemble axum server + TOML config. End-to-end with the mem backend: a working single-node ONTAP REST daemon.
6. **Substrate impls** — staged. Wave A (zfs + s3) first; Wave B (nfs-bare + ontap-passthrough + p4d + gitlfs) once the trait stabilizes.
7. **`nessie-client` + Python bindings** — typed client + PyO3 surface. After Rust substrates are stable.

## Trait shape — locked

Three-tier supertrait stack matching the substrate capability tiers exactly:

```rust
pub trait VolumeBackend: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    fn list_volumes(&self) -> Result<Vec<Volume>, BackendError>;
    fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError>;
    fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError>;
    fn delete_volume(&self, uuid: &VolumeUuid) -> Result<(), BackendError>;
    fn patch_volume(&self, uuid: &VolumeUuid, patch: VolumePatch) -> Result<Volume, BackendError>;
    fn access_handle(&self, uuid: &VolumeUuid) -> Result<AccessHandle, BackendError>;
    fn as_snapshot(&self) -> Option<&dyn SnapshotBackend> { None }
}

pub trait SnapshotBackend: VolumeBackend {
    fn list_snapshots(&self, vol: &VolumeUuid) -> Result<Vec<Snapshot>, BackendError>;
    fn create_snapshot(&self, vol: &VolumeUuid, name: &str) -> Result<Snapshot, BackendError>;
    fn get_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid) -> Result<Snapshot, BackendError>;
    fn delete_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid) -> Result<(), BackendError>;
    fn as_clone(&self) -> Option<&dyn CloneBackend> { None }
}

pub trait CloneBackend: SnapshotBackend {
    fn create_clone(&self, parent_vol: &VolumeUuid, parent_snap: &SnapshotUuid,
                    new_name: &str) -> Result<Volume, BackendError>;
}
```

`as_snapshot()` and `as_clone()` return `None` by default so any `VolumeBackend` impl that can't honor higher tiers gets a correct default. Substrates that can implement those tiers override the accessors to return `Some(self)`. The REST router downcasts at dispatch and returns the documented ONTAP "feature not supported" response when the backend lacks the capability. This is Rust 1.86+ trait-upcasting territory; the repo MSRV is 1.88.

## Data plane discipline

The daemon does **not** broker bytes. Each backend exposes `access_handle(uuid) -> AccessHandle`, where `AccessHandle` is substrate-native:

- `NfsExport { server, path }` for zfs and nfs-bare (zfs writes `/etc/exports.d/` entries)
- `S3Presigned { url, expires_at }` for s3
- `GitRef { remote, ref_name }` for gitlfs
- `P4Stream { p4port, stream }` for p4d
- `OntapPassthrough { mgmt_lif, data_lif }` for the passthrough backend
- `InMemory` for mem (conformance-only)

Clients fetch the handle from the REST surface, then read/write directly against the substrate. This matches how real ONTAP works (control plane and data plane on separate networks) and keeps nessie-store from becoming a byte funnel.

## Threading model

- `tokio` for I/O (REST handlers, HTTP clients in the ontap-passthrough backend, async file ops).
- `rayon` for CPU-bound parallelism if/when it appears (parallel snapshot reconciliation, hash compute for content addressing). Most substrate backends are I/O-dominated; don't reach for `rayon` until measurement says it'll matter.
- Don't `tokio::spawn` for CPU-bound work; use `spawn_blocking` or `rayon::scope`.
- Hot paths use `dashmap` and `crossbeam-channel` where lock-free matters; `Arc<Mutex<T>>` is fine for cold control state.

## Capability discipline (substrate-level)

When implementing a new backend:

1. Declare what it can do via `capabilities()`. Be honest. NFS-bare has no native snapshots; advertise that.
2. Implement only the tier you can honor. Override `as_snapshot()` / `as_clone()` to return `Some(self)` only if you can.
3. Pass the corresponding conformance suite. `nessie-backend-conformance` chooses which suites to run from `capabilities()`. Volume-only backends skip snapshot/clone tests by design.
4. Return `AccessHandle` matching the substrate. The handle is the contract the data-plane client honors.

Don't emulate. If NFS-bare wanted snapshots, the answer would be "use ZFS-backed NFS," not "rsync-tree-copy and call it a snapshot." Telling the truth about substrate semantics is load-bearing for everything downstream — SnapMirror in particular assumes snapshot atomicity.

## Sortie discipline (for drake-swarm workers)

Each sortie is one file-narrow goal (~50–200 LOC + tests), gated by `cargo test -p <crate> --locked`. The drake foreman wraps the grader to enforce:

- **Empty diff → crash.** A `git diff --stat` that shows no changes disqualifies the worker pre-arbiter.
- **Tests must reference new symbols.** The grader greps the test files for the new function/type names. Stub tests that don't exercise the new code path fail the gate.
- **One crate per sortie.** Workers should not edit two backends in one patch; that's a planning failure that needs a re-split.
- **180s default timeout, 5 rounds max.** If a sortie can't fit in that window, the plan is wrong and the sortie should be split (e.g., `S3-p4d.4a` and `.4b`).

## Ecosystem context

nessie-store is the storage substrate for the gilamonster swarm:

- [monty-tui](https://github.com/hartsock/monty-tui) — terminal dashboard that surfaces backend health, capability matrices, and SnapMirror state.
- [scrybe](https://github.com/hartsock/scrybe) — collaborative editor; can use nessie-store for document storage.

Where content-addressed identity matters (mainly for backend implementations that key blobs by hash, like s3), nessie-store relies on a content-addressable trait supplied by the wider toolchain.

## Versioning + release

- Scheme: **SemVer** (`MAJOR.MINOR.PATCH`). `v0.1.0` marks feature parity with
  `netapp-sim`; `0.2.x` carries the enhancement cycle. Pre-1.0, minor bumps may
  carry breaking changes.
- crates.io + PyPI publish from the same tag. Release flow in [docs/STYLE_PYO3.md](docs/STYLE_PYO3.md).

## Operator notes

- **Canonical remote: `https://github.com/Gilamonster-Foundation/nessie-store` (HTTPS).**
  Re-homed under the Gilamonster Foundation 2026-06-14. SSH to github is blocked in the
  agent sandbox — use `gh`/HTTPS for all fetch/pull/push.
- **License: dual `MIT OR Apache-2.0`** (Foundation convention) — `LICENSE-APACHE` + `LICENSE-MIT`.
- The repo lives on `agent-workspace` NFS, visible inside harness pods at `/workspaces/nessie-store` and on gnuc at `/mnt/agent-workspace/nessie-store`. Convenience symlink at `~/workspaces/nessie-store` on gnuc. **Do NOT `git worktree add/remove` here** — on this NFS-symlinked checkout it can flip `core.bare=true` and break the working tree.
- **Build vehicle (2026-06-14): incremental, one PR per phase, operator-driven** (not
  drake-swarm). The active plan is `~/.claude/plans/nessie-store-ontap-rust-rewrite.md` on
  gnuc; the older sortie plan `splendid-swimming-shamir.md` is superseded.
- **Current state (as of 0.3.1) — plan vs. reality.** The README
  [crate inventory](README.md#crate-inventory) is the machine-checked source of truth
  (`scripts/check-doc-drift.sh`). **Shipped:** the ONTAP REST *control* plane
  (discovery, volumes CRUD + FlexClone, snapshots CRUD + delta, HTTP Basic auth, TLS,
  mint-once identity) over the trait stack; the `zfs` + `mem` backends; and an embedded
  hardened NFSv3 *data* plane (`nessie-nfs` + `nessie-nfsserve`). **Not yet done, despite
  older prose:** SnapMirror is control-plane only — the `zfs send → zfs receive` byte
  movement is stubbed (#69); the live-ZFS / Trident acceptance gate is unbuilt (#70);
  the s3 / nfs-bare / ontap-passthrough / p4d / gitlfs backends and the unified
  `nessie-client` do not exist (#72); `fpolicy` and NFS `export-policies` REST parity
  with `netapp-sim` is absent. **v1.0 = full `netapp-sim` parity is the *goal*, not the
  current state;** embedded Python (Face B) is the post-parity fast-follow. Two
  experimental demand-paged NFS gateways (git-LFS, Perforce) are designed but unbuilt
  (#71).
