# Replicating between two nessie-store instances

SnapMirror moves a volume's snapshots from a **source** instance to a
**destination** instance over HTTP: the source opens a replication stream
(`zfs send`, or the in-memory backend's logical stream), POSTs it to the
destination's internal receive endpoint, and the destination applies it.
Authentication is a **per-peer replication token** (passphrase) that both sides of
a pair hold — the destination validates it; the source presents it.

This walkthrough stands up two instances on one host with the in-memory backend
and plain HTTP (`--no-tls`) so you can watch a volume replicate end to end. (The
automated version of this is `crates/nessie-store/tests/replication_e2e.rs`.)

## 1. Two configs

`a.toml` (source) and `b.toml` (destination) — the in-memory backend is the
default, so no `[backend]` block is needed:

```toml
# a.toml
listen = "127.0.0.1:8443"
data_dir = "/tmp/nessie-a"
nfs_enabled = false
```

```toml
# b.toml
listen = "127.0.0.1:8444"
data_dir = "/tmp/nessie-b"
nfs_enabled = false
```

## 2. Start both

```bash
nessie-store serve --config a.toml --no-tls &   # source → :8443
nessie-store serve --config b.toml --no-tls &   # dest   → :8444
```

Default credentials are `admin:admin`.

## 3. Pair them with a shared token

Register a peer on **each** side with the same passphrase:

```bash
TOKEN="demo-passphrase"

# DESTINATION (B): a peer for the source, holding the token B will accept.
curl -su admin:admin -X POST http://127.0.0.1:8444/api/cluster/peers \
  -H 'content-type: application/json' \
  -d '{"name":"clusterA","ip_address":"http://127.0.0.1:8443","authentication":{"passphrase":"'"$TOKEN"'"}}'

# SOURCE (A): a peer for the destination, with B's address + the same token.
curl -su admin:admin -X POST http://127.0.0.1:8443/api/cluster/peers \
  -H 'content-type: application/json' \
  -d '{"name":"clusterB","ip_address":"http://127.0.0.1:8444","authentication":{"passphrase":"'"$TOKEN"'"}}'
```

## 4. Create a source volume + relationship, then transfer

```bash
# A source volume on A.
curl -su admin:admin -X POST http://127.0.0.1:8443/api/storage/volumes \
  -H 'content-type: application/json' -d '{"name":"src"}'

# A relationship src -> dst, resolving peer clusterB.
REL=$(curl -su admin:admin -X POST http://127.0.0.1:8443/api/snapmirror/relationships \
  -H 'content-type: application/json' \
  -d '{"source":{"path":"svm0:src","cluster":{"name":"clusterB"}},"destination":{"path":"svm0:dst"}}' \
  | jq -r .record.uuid)

# Fire an on-demand transfer: A snapshots src and streams it to B.
curl -su admin:admin -X POST \
  http://127.0.0.1:8443/api/snapmirror/relationships/$REL/transfers | jq .record
```

The transfer record shows `state: "success"` and a non-zero `bytes_transferred`.

## 5. Verify on the destination

```bash
curl -su admin:admin http://127.0.0.1:8444/api/storage/volumes | jq '.records[].name'
```

`dst` now appears on **B** — the volume replicated. A second transfer sends only
the incremental delta from the previous snapshot (the relationship tracks a
per-pair base cursor).

## Notes

- **TLS**: drop `--no-tls` to serve HTTPS (self-signed by default). Peer-to-peer
  replication accepts self-signed peer certs (trust is network-gated); `curl` then
  needs `-k`.
- **ZFS**: set `backend = "zfs"` (+ `zfs_pool`) on both sides to replicate real
  datasets via `zfs send | zfs receive` instead of the in-memory logical stream.
- **Fan-out / cascade**: a source can hold several relationships (one per peer),
  and a destination can itself be a source for a further-downstream instance —
  each relationship tracks its own base cursor. See
  [design/snapmirror-data-plane.md](design/snapmirror-data-plane.md).
```
