# Changelog

All notable changes to nessie-store are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Planned
- Cross-instance binary `zfs send` → HTTP → `zfs receive` streaming (the live data plane).
- Live-ZFS / Trident-on-k3s acceptance gate.
- SMB support (v0.4.0).

## [0.3.1] — 2026-06-26

Embedded-NFS hardening — the F1–F5 prerequisites for serving live agent
workspaces over the in-process NFS server (epic #44).

### Added
- **`nessie-nfsserve`**: a vendored, hardened fork of HuggingFace's `nfsserve`
  0.11.0 (BSD-3-Clause, preserved) as a workspace crate. Replaces the external
  `nfsserve` dependency so the embedded NFS plane can carry the F2/F3/F5 hardening
  fixes (write durability, `NFSPROC3_COMMIT`, AUTH_UNIX ownership) that the
  upstream `NFSFileSystem` trait cannot express. No behavior change in this step —
  it is the enabling refactor (airship ADR 0001: keep nessie-store self-contained).

### Fixed
- **NFS write durability (F2/F3, blockers).** The embedded NFS server no longer
  loses acknowledged writes on a crash:
  - `WRITE` now honors the client's `stable` flag and reports the **honest**
    `committed` level (`FILE_SYNC` only when the data is actually on stable
    storage, else `UNSTABLE`) instead of unconditionally claiming `FILE_SYNC`.
    `FILE_SYNC`/`DATA_SYNC` writes `fdatasync` before acknowledging.
  - `NFSPROC3_COMMIT` is now implemented (previously dispatched to
    `PROC_UNAVAIL`), so a client `fsync()` succeeds **only after** a real flush.
    `PassthroughFs::commit` fsyncs the target; unstable writes are durable after
    the following commit. New `NFSFileSystem::{write_stable, commit}` trait hooks
    (defaulting to the safe, honest behavior) carry this.
- **NFS AUTH_UNIX ownership (F5, blocker).** Files and directories created over
  the embedded NFS server are now owned by the **calling client** instead of
  landing `root:root`:
  - The parsed AUTH_UNIX credential is exposed as `nessie_nfsserve::UnixCred` and
    threaded into new `NFSFileSystem::{create_with_cred, create_exclusive_with_cred,
    mkdir_with_cred}` hooks (defaulting to the credential-less behavior).
  - `PassthroughFs` chowns each new object to the caller's uid; the group is left
    to set-GID inheritance under a mode-2775 parent (the shared pod/host workspace
    contract) and otherwise set to the caller's gid. A `SETATTR` carrying uid/gid
    now actually chowns (previously silently ignored). Chowning new objects is
    best-effort (logs if the daemon lacks `CAP_CHOWN`); the daemon must run with
    `CAP_CHOWN` (k8s `privileged: true`) to own files as arbitrary callers.
  - `deploy/config.example.toml` documents the recommended `zfs_dataset_owner` /
    `zfs_dataset_mode` (`1000:1001` / `2775`) for shared agent workspaces.
- **NFS handle aliasing across datasets (F4).** The NFS fileid is now derived
  from the `(st_dev, st_ino)` pair (mixed with SplitMix64), not `st_ino` alone, so
  a single export spanning sibling ZFS datasets — which each have an independent
  inode namespace and reuse low inode numbers — no longer aliases one clone's file
  onto another's (or returns spurious `NFS3ERR_STALE`). The fileid remains stable
  across a daemon restart, so cached client handles keep working.
- **Clone/volume mountability (F1).** `create_volume` and `create_clone` now pass
  `-o mountpoint=<srv_root>/<name>` so a freshly created volume or FlexClone lands
  under the NFS export root and is mountable from a **single** `POST` — previously
  it inherited `/<pool>/<name>` and was unreachable over NFS until a second
  `PATCH …/{uuid} {nas:{path:…}}` set a junction.

### Notes
- The cross-user ownership, crash-recovery, and two-dataset aliasing assertions
  require a real kernel NFS mount (and, for ownership, `CAP_CHOWN` / root); they
  are covered by **live/root-only** integration tests, not the hermetic CI suite.

## [0.3.0] — 2026-06-20

Host-kernel-free NFS export plane.

### Added
- **Embedded userspace NFSv3 server** (`nessie-nfs`): nessie-store serves NFS
  **itself**, in-process — **no host kernel NFS server** (`rpc.nfsd`, `exportfs`,
  `rpcbind`) required. Built on `nfsserve` (BSD-3). `PassthroughFs` exports a real
  directory tree with **stable file handles** (fileid = inode, no generation
  number, fixed `serverid`) so mounts survive a daemon restart, and a readdir that
  orders by fileid so cookies never drop/duplicate entries.
- **`[nfs]` config**: `nfs_enabled`, `nfs_listen` (default `0.0.0.0:2049`),
  `nfs_export_root` (default `/srv`), `nfs_export_name`. The daemon spawns the NFS
  server alongside the HTTP control plane.

### Changed
- When the embedded NFS server is on, the ZFS backend no longer drives the host
  kernel export table (`ZfsConfig.manage_kernel_exports = false`): no
  `/etc/exports.d/` writes, no `exportfs`.
- **Packaging dropped the host kernel NFS dependency**: `.deb` no longer depends on
  `nfs-kernel-server`, `.rpm` no longer requires `nfs-utils`, the container no
  longer installs it (and `EXPOSE`s 2049). Docker, the setup wizard, and
  `config.example.toml` enable the embedded server by default; the k8s Service +
  Deployment expose 2049 so the data plane rides the Service (no `hostNetwork`).
- Clients mount with `nfsvers=3,proto=tcp,port=2049,mountport=2049,nolock,noacl`.

### Notes
- The embedded server is NFSv3 only, with no NLM locking (`nolock`) and AUTH_UNIX
  (gate access at the network layer). Set `nfs_enabled = false` to fall back to the
  legacy host-kernel `exportfs` path.

## [0.2.1] — 2026-06-18

Cross-platform Python wheels.

### Added
- **Wheels for macOS (universal2), Linux (x86_64 + aarch64), and Windows (x64)** for
  every crate — Python developers on any OS can `pip install` any component of the stack.
- **abi3 (stable ABI)** wheels: one wheel per platform works on CPython 3.10+.

### Changed
- The `nessie-store` crate gained a default `daemon` feature; the PyO3 wheel builds
  `--no-default-features` so it ships **light** (`Config` + `identity` only — no TLS/HTTP
  stack), which keeps it small and lets it cross-compile to every platform. The daemon
  binary and the crates.io crate are unchanged (full `daemon` by default).

## [0.2.0] — 2026-06-18

The enhancement cycle.

### Added
- **PyO3 wheels for every crate** (`nessie-backend-core`, `nessie-ontap-protocol`,
  `nessie-backend-mem`, `nessie-backend-conformance`, `nessie-backend-zfs`, `nessie-store`) —
  each pip-installable with a runnable example, smoke tests, and README code examples.
- **Inside extension points**: `nessie_backend_conformance.run_all(backend)` validates a
  **Python-authored** storage backend against the conformance suite; `nessie_backend_zfs.ZfsBackend`
  accepts a Python `runner(argv)` callable so every ZFS command is routed through your code (mock,
  audit, or sudo-wrap).
- **Outside extension points**: drive the substrates from Python — `ZfsBackend` over real
  `zfs`/`zpool`, and `nessie_store.Config` to generate/validate the daemon's `config.toml` +
  `mint_identity()` for tooling.
- **Deploy artifacts**: multi-stage `Dockerfile` with a ZFS vdev-bootstrap entrypoint, a systemd
  unit + example config/environment, and `docs/DEPLOY.md`.
- **`release.yml`**: tag-triggered pipeline — crates.io publish (dep order), PyO3 wheels → PyPI,
  the daemon binary, **`.deb` + `.rpm` packages**, a GHCR image, and a GitHub Release. Every
  publish step is gated on its secret and no-ops without it. Internal path deps now carry explicit
  versions (for `cargo publish`).
- **Installers**: a `curl | sudo bash` `install.sh`, an interactive `nessie-store-setup` wizard
  (systemd), and `.deb`/`.rpm` packages (via cargo-deb / cargo-generate-rpm) that ship the unit.
- **Kubernetes / k3s deploy surface**: `deploy/k8s/` manifests (namespace, PVC, privileged
  Deployment, Service, example Secret + static PV, `kustomization.yaml`) so the GHCR image runs
  as a pod backed by a PV/PVC. `docs/DEPLOY_K8S.md` covers storage, the NFS data plane, and a
  non-ZFS control-plane-only variant.

## [0.1.0] — 2026-06-18

**Feature parity with [`netapp-sim`](https://github.com/hartsock/netapp-sim).**

A high-performance Rust ONTAP-on-ZFS daemon — a cheap ONTAP on-ramp for home and
small business. An unmodified ONTAP client (Trident/CSI, the `netapp.ontap`
Ansible collection, the `netapp-ontap` Python SDK) can drive the full workflow.

### Added
- **Backend trait stack** (`nessie-backend-core`): `VolumeBackend ⊂
  SnapshotBackend ⊂ CloneBackend`, `Capabilities`, `AccessHandle`, `BackendError`.
- **In-memory reference backend** (`nessie-backend-mem`) + a substrate-agnostic,
  capability-honest **conformance harness** (`nessie-backend-conformance`).
- **ONTAP wire layer** (`nessie-ontap-protocol`): HAL envelope, job + ONTAP-native
  error envelopes, domain→wire record mapping.
- **ZFS substrate** (`nessie-backend-zfs`): real datasets/snapshots/FlexClones over
  a `CommandRunner` seam, NFS export to `/etc/exports.d/`, with the hard-won
  invariants (idempotent mountpoint, unexport-before-destroy, busy-retry,
  path-traversal-safe export names) regression-tested. Gated `live-zfs` tier.
- **The daemon** (`nessie-store`): ONTAP REST over a pluggable backend —
  - discovery: cluster / nodes / SVM / aggregates / network LIF / jobs, with
    stable mint-once identity;
  - HTTP **Basic auth** (constant-time) + **TLS** (Vault PKI → existing →
    self-signed cert tiers);
  - **volumes** CRUD + FlexClone, **snapshots** CRUD + delta;
  - **SnapMirror** relationships + cluster peers + transfers;
  - subprocess backend calls on `spawn_blocking`.
- **PyO3 bindings** for `nessie-backend-core` → `pip install nessie-backend-core`
  (the start of the per-crate wheel pass).

### Notes
- The cross-instance binary `zfs send`/`receive` byte movement is the live-only
  data plane (the control surface is complete); it lands in the 0.2.x cycle.

[0.3.1]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.3.1
[0.3.0]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.3.0
[0.2.1]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.2.1
[0.2.0]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.2.0
[0.1.0]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.1.0
