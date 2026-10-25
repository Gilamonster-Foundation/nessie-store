
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
recurse. A node's own `has`/`size` fall through to peers on a local miss (a `HEAD`,
no body), which is what lets the S3 face serve a locally evicted blob; `get` still
does the verified fetch-through. `peers` must list each URL once; set
`advertise_url` to this node's own URL to share one peer list across nodes (the
matching entry is ignored).

**Durability is verified, not configured.** `/cas/v1` HEAD and GET answer with
`x-nessie-mode: lru|write_only` from the serving node's own config. The durability
oracle counts a peer only if it holds the blob *and* reports `write_only`, so a peer
listed `durable = true` that actually runs `lru` is not a permanent home. `durable`
stays only as a hint of whom to probe and replicate to.
