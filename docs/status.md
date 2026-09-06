# Status

Current implementation caveats beyond the
[crate inventory](../README.md#crate-inventory) — that table is the
machine-checked source of truth for which crates exist; this page is for the
"works, but with a catch" details that don't fit a table row.

## SnapMirror moves real bytes now

Relationships, peers, and transfers are tracked, and a SnapMirror transfer
moves real bytes: the `ReplicationBackend` capability tier (`send_stream` /
`receive_stream`) is implemented by the `zfs` backend (`zfs send [-i base]` /
`zfs receive`) and the `mem` backend (an in-memory logical stream). See
[REPLICATION.md](REPLICATION.md) to run two instances end to end, and
[design/snapmirror-data-plane.md](design/snapmirror-data-plane.md) for the
fan-out/cascade design.

## Client bindings

The current PyO3 wheels are per-crate bindings, not the unified `nessie-client`
typed client — that's still planned (#72).

## Designed but unbuilt

Two demand-paged NFS gateways (git-LFS, Perforce) are designed but not built —
see [design/demand-paged-nfs-gateways.md](design/demand-paged-nfs-gateways.md)
(#71).
