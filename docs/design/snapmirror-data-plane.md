# Design: SnapMirror live data plane

**Status:** in progress · tracking issue **#69** · this doc lands with Slice 1
(the `ReplicationBackend` tier).

## Problem

SnapMirror is **control-plane only** today. `crates/nessie-store/src/snapmirror.rs`
tracks relationships / peers / transfers and takes a real *source* snapshot per
transfer, but no bytes move: `create_transfer` reports `bytes_transferred: 0`, and
`POST /internal/snapmirror/receive` acknowledges the request without applying a
`zfs receive`. The cross-instance byte movement has been listed under CHANGELOG
`[Unreleased] → Planned` since v0.1.0. This makes the flagship replication feature
hollow — the base-first roadmap decision is to finish it before widening the
protocol surface (CIFS/NFSv4.1).

## Approach

Move bytes with the substrate's **native** replication primitive (`zfs send` →
HTTP → `zfs receive`), exposed through a new capability tier so the daemon stays
substrate-agnostic and honest: only backends that advertise `replication` can
replicate; others return the documented ONTAP "feature not supported".

### The `ReplicationBackend` tier (Slice 1 — landed)

`Capabilities::replication` already existed and `core/lib.rs` already named
`ReplicationBackend` as the next tier. It branches from `SnapshotBackend` (needs
snapshots) and is independent of `CloneBackend`:

```rust
pub trait ReplicationBackend: SnapshotBackend {
    fn send_stream(&self, vol: &VolumeUuid, snap: &str, base: Option<&str>)
        -> Result<Box<dyn std::io::Read + Send>, BackendError>;
    fn receive_stream(&self, dest: &str, stream: &mut dyn std::io::Read)
        -> Result<u64, BackendError>;
}
// reached via SnapshotBackend::as_replication() -> Option<&dyn ReplicationBackend>
```

**Snapshot *names* are the cross-instance contract.** The SnapMirror layer names
snapshots deterministically (`snapmirror.<rel8>.<seq>`), so both instances share
names; an incremental stream names the common base, which the destination must
already hold. Backends address replication snapshots by name, not by the local
`SnapshotUuid` (which differs per instance).

### Streaming command seam (Slice 3)

The current `CommandRunner` is buffered (`stdout: String`, UTF-8 lossy) — unusable
for a binary `zfs send`. Add streaming primitives to the seam (mockable, so unit
tests assert argv + stream without a real pool):

```rust
fn spawn_stdout(&self, argv: &[&str]) -> Result<Box<dyn Read + Send>, BackendError>;
fn run_stdin(&self, argv: &[&str], input: &mut dyn Read) -> Result<u64, BackendError>;
```

Defaulted to `Unsupported` so only `SystemRunner` (and the test mock) implement them.

### ZFS implementation (Slice 4)

`ZfsBackend` implements `ReplicationBackend`, advertises `Capabilities::all()`:
- `send_stream(vol, snap, base)` → `zfs send [-i <pool>/<vol>@<base>] <pool>/<vol>@<snap>`
  via `spawn_stdout`.
- `receive_stream(dest, stream)` → `zfs receive -F <pool>/<dest>` via `run_stdin`.

### Daemon wiring (Slice 5)

- `create_transfer`: after taking the source snapshot, resolve the relationship's
  peer address, look up the common base (the last snapshot successfully transferred
  for this relationship — tracked in `SnapMirrorStore`), open `send_stream`, and
  **stream** it to the peer. On success record the honest `bytes_transferred` and
  advance the base; on failure mark the transfer failed + relationship unhealthy.
- `internal_receive`: switch from a buffered `Bytes` body to a streaming body fed
  into `receive_stream`; return the honest applied byte count.
- If the backend lacks `as_replication()` (e.g. `mem` without the Slice-2 impl, or a
  volume-only substrate), return the documented "feature not supported" — no faked
  success.

### Reference impl + hermetic test (Slice 2)

`MemBackend` implements `ReplicationBackend` by serializing a volume's snapshot
state to a byte buffer and applying it — so the **whole data plane is testable
hermetically**: two in-process `AppState`s, instance A's `create_transfer` streams
to instance B's `internal_receive`, and B ends up with the volume + honest bytes.
`zfs` makes it real; `mem` makes it CI-testable without a pool.

### Acceptance (Slice 6 → folds into #70)

The live-ZFS / Trident gate gains a two-instance real-`zfs` replication check
(full + incremental), asserting the destination dataset materializes.

## Topology: fan-out, cascade, hub-and-spoke

A nessie instance is not single-source / single-destination. One instance can hold
many volumes (and, later, many demand-paged views — p4 / git / "lore" — surfaced
over one or several NFS exports, see #71), and it can be **both** a destination of
upstream instances and a source for downstream ones. The peer + relationship model
is already N:N (both are collections keyed by UUID), so the data plane must support:

- **Fan-out (hub → spokes):** one source volume replicated to many destinations —
  N relationships sharing a source. Each transfer streams independently to its peer.
- **Cascade (A → B → C):** a destination that is itself a source. Instance B holds a
  receive-relationship (from A) and a send-relationship (to C) for the same volume.
  This is why **snapshot names are the cross-instance contract**: the deterministic
  `snapmirror.<rel8>.<seq>` names propagate down the chain, so B can offer a received
  snapshot as C's incremental base without renaming.
- **Hub-and-spoke fleet:** the general case — a hub fans out to many spokes, some of
  which cascade further. Per-peer replication tokens (the chosen auth model) scale to
  this: a node holds a token as a spoke of each upstream and issues a token as a hub
  to each downstream — no shared fleet-wide secret.

Constraints this imposes on the data plane:

- Transfers are **per-relationship** and must run concurrently without head-of-line
  blocking — a slow or down spoke must not stall the hub's other transfers.
- Common-base tracking is **per-relationship, not per-volume**: a volume fanned out
  to three peers has three independent base cursors.
- A cascade node re-derives its downstream stream from its **local** copy of the
  snapshot; it never proxies the upstream byte stream.

## Wire contract + auth (decided)

`POST /internal/snapmirror/receive` is nessie-store's own peer protocol (not an
ONTAP path). Contract:

- **Body:** the raw replication stream (chunked; not buffered).
- **Headers:** `X-Destination-Volume`, `X-Snapshot`, `X-Base-Snapshot` (optional,
  for incrementals).
- **Auth (decided): a dedicated per-peer replication token**, not the admin Basic
  credential. Each peer record carries a token; the sender presents the
  destination's token on `/internal/snapmirror/receive`, and the receiver accepts
  the peer endpoint **only** with a valid peer token (the admin credential does not
  authorize replication). This keeps replication auth separate from admin auth and
  scales to a hub-and-spoke fleet without a shared fleet-wide secret (see Topology).
  Transport is HTTPS.

Only Slice 5 depends on this; Slices 1–4 do not.

## Staged PRs

1. **Slice 1** — `ReplicationBackend` tier in `nessie-backend-core` *(this PR)*.
2. **Slice 2** — `MemBackend` replication impl + conformance replication suite.
3. **Slice 3** — streaming `CommandRunner` primitives (`nessie-backend-zfs`).
4. **Slice 4** — `ZfsBackend: ReplicationBackend` (`zfs send`/`receive`).
5. **Slice 5** — daemon wiring: streaming transfer + receive, honest bytes/state,
   incremental base tracking, peer transport + auth (per the decision above).
6. **Slice 6** — live two-instance acceptance (folds into #70).
