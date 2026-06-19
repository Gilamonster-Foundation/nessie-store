# Changelog

All notable changes to nessie-store are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Planned
- Cross-instance binary `zfs send` → HTTP → `zfs receive` streaming (the live data plane).
- Live-ZFS / Trident-on-k3s acceptance gate.

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

[0.2.1]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.2.1
[0.2.0]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.2.0
[0.1.0]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.1.0
