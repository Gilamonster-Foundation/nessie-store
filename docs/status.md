# Status

Caveats beyond the [crate inventory](../README.md#crate-inventory), which is
the machine-checked list of what exists.

## Content-addressed store, REAPI and S3

- **Storage.** `[cas] backend = "mem"` (default) loses blobs on restart;
  `"disk"` (`nessie-cas-disk`) keeps them under `path`. REAPI, S3 and
  maintenance share the one store.
- **Retention.** `[cas] mode = "lru"` (default) evicts coldest first over
  `byte_budget`: blobs another holder has go before sole copies; pinned and
  in-flight blobs never. A sole copy can be dropped; it is a cache.
  `"write_only"` never deletes; `byte_budget` is a hard capacity and a write
  past it is refused (S3 507, REAPI `RESOURCE_EXHAUSTED`). `durable` is gone.
- **Coming in v0.4.0.** The `write_only` mirror guarantee (a write is
  acknowledged after R verified copies, #139) and scrub/repair (#140). Until
  then `write_only` on one node has no redundancy. Pins persist across
  restart only after #137.
- **Auth is lab-only.** S3 uses one static SigV4 key; the daemon refuses to
  start it on the default secret (`NESSIE_S3_SECRET_KEY`). REAPI is
  unauthenticated and off by default. mTLS is not built.
- **S3 scope.** `PutObject`, `GetObject`, `HeadObject` on content-addressed
  keys (`cas/{xx}/{sha256}` or flat `{prefix}{sha256}`). No `ListObjectsV2`,
  no arbitrary key names; a body that doesn't hash to its key is refused.
- **ActionCache.** Result bodies live in the shared store; attestations are
  in memory and lost on restart.
- **Peers.** Static peers shipped (#127): see [CAS peer plane](#cas-peer-plane-static-peers)
  below. HTTP-only and unauthenticated until mTLS. The pin endpoint is still
  coming in v0.4.0 (#128).

See [design/cas-disk-s3-peers.md](design/cas-disk-s3-peers.md).

## CAS peer plane (static peers)

`[cas] peers = [{ url = "http://host:port", durable = true }]` (`durable` is a hint, verified: see below) turns on fetch-through
(`SwarmCas`), a HEAD-backed durability oracle and a PUT replicator; `peer_api = true`
mounts `GET|HEAD|PUT /cas/v1/blobs/{algo}:{hex}` so others can reach this node.
**Lab only: the routes are unauthenticated, and the peer client does not trust the
daemon's self-signed certificate, so peers must reach each other over plain `http`
(run the daemon with `--no-tls`). mTLS is the fast-follow.** Integrity never depends on the wire: every byte is verified against
its digest before it is stored or served.

**Probe cost.** Cache eviction asks every peer, one after another, whether it holds
each candidate blob: O(candidate blobs × peers) `HEAD` requests per maintenance pass,
no caching or batching. Each request is bounded by a 10 s connect / 30 s total
timeout, so an unresponsive peer can add up to 30 s per candidate. Keep peer lists
short; set `maintenance_interval_secs` well above a worst-case pass.

**Reads and probes.** `GET|HEAD|PUT /cas/v1/blobs/...` answer from this node's local
storage only (never by asking its own peers), so two nodes listing each other cannot
recurse. A node's `has` is local-only (it means "held and counted here"); `size` falls
through to peers on a local miss (a `HEAD`, no body) and `get` does the verified
fetch-through, which is how the S3 face serves a locally evicted blob. `peers` must list each URL once; set
`advertise_url` to this node's own URL to share one peer list across nodes (the
matching entry is ignored).

**Durability is verified, not configured.** `/cas/v1` HEAD and GET answer with
`x-nessie-mode: lru|write_only` from the serving node's own config. The durability
oracle counts a peer only if it holds the blob *and* reports `write_only`, so a peer
listed `durable = true` that actually runs `lru` is not a permanent home. `durable`
stays only as a hint of whom to probe and replicate to.

**Fetch-through and capacity.** An `lru` node stores a peer-fetched blob (eviction
manages its budget). A `write_only` node serves the verified bytes without storing
them, so a read can never push it past its capacity, and writing a blob a peer already
holds still passes the capacity gate. `push()` counts a peer only when it reports
`write_only`.
