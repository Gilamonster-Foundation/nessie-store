# Changelog

All notable changes to nessie-store are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased] — 0.2.0 (in progress)

The enhancement cycle. Planned:
- Cross-instance binary `zfs send` → HTTP → `zfs receive` streaming (the live data plane).
- PyO3 wheels for `nessie-ontap-protocol`, `nessie-backend-mem`, `nessie-backend-conformance`,
  plus the inside-extension `PyBackend` (write a storage backend in Python).
- Live-ZFS / Trident-on-k3s acceptance gate; deploy artifacts (Docker vdev-bootstrap, RPM/systemd).

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

[0.1.0]: https://github.com/Gilamonster-Foundation/nessie-store/releases/tag/v0.1.0
