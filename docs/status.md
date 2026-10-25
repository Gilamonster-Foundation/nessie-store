# Status

Caveats beyond the [crate inventory](../README.md#crate-inventory), which is
the machine-checked list of what exists.

## Content-addressed store, REAPI and S3

- **Storage.** `[cas] backend = "mem"` (default) loses blobs on restart;
  `"disk"` (`nessie-cas-disk`) keeps them under `path`. REAPI, S3 and
  maintenance share the one store.
- **Retain-all.** A durable node with a face enabled never sweeps: face
  writes have no persistent roots, so GC would delete them after a restart.
  Disk use only grows until roots persist (#137). Cache mode still evicts,
  and only blobs another holder has.
- **Auth is lab-only.** S3 uses one static SigV4 key; the daemon refuses to
  start it on the default secret (`NESSIE_S3_SECRET_KEY`). REAPI is
  unauthenticated and off by default. mTLS is not built.
- **S3 scope.** `PutObject`, `GetObject`, `HeadObject` on content-addressed
  keys (`cas/{xx}/{sha256}` or flat `{prefix}{sha256}`). No `ListObjectsV2`,
  no arbitrary key names; a body that doesn't hash to its key is refused.
- **ActionCache.** Result bodies live in the shared store; attestations are
  in memory and lost on restart.
- **Peers.** Not built. Peer sharing and the pin endpoint are coming in
  v0.4.0 (#127, #128).

See [design/cas-disk-s3-peers.md](design/cas-disk-s3-peers.md).
