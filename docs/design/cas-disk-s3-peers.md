# CAS on disk, S3 face, peers (v0.4.0)

Epic #122. Extends [p2p-cas-swarm.md](p2p-cas-swarm.md) with persistence and
an S3 face. Peer sharing is still being built and is not described here.

## Decisions

| | |
|---|---|
| Disk store | Small `nessie-cas-disk` crate, git-loose-object layout, temp + rename. No RocksDB. |
| S3 face | `nessie-s3` on s3s; bazel-remote `cas/{xx}/{hex}` keys plus flat `{prefix}{sha256}` for kyln. |
| Peer sharing | Static peer list in TOML, HTTP fetch-through behind `ContentRouter`. Mesh router later. |
| Auth | S3: one static SigV4 key, secret from `NESSIE_S3_SECRET_KEY`. REAPI and peer API unauthenticated, lab-only, default off. mTLS next. |
| Retention | `lru` (default): coldest first, replicated before sole copies, pins and in-flight never; a sole copy can be dropped. `write_only`: never deletes, refuses writes at capacity. Reachability-GC `durable` retired. |
| Mirror | `write_only` acknowledges after R verified copies, with scrub and repair in every maintenance pass (#139, #140). |
| Keypair plane | agent-mesh owns transport and identity; nessie later ships a `ContentRouter` impl. |

## What the disk store borrows from 3FS

Three chunk-engine properties:

1. **Bytes before metadata commit.** A blob is written and synced to a temp
   file, then renamed into place. A crash leaves no blob or a whole one.
2. **Temp swept on open.** Uncommitted writes never survive a reopen.
3. **A read survives a concurrent delete.** An open read keeps its bytes.

Not taken: RocksDB metadata (the filesystem path is the index), CRAQ chain
replication (peers fetch by digest instead), and a slab allocator (tiny blobs
are one file each; revisit if that measures badly).

## Just want S3?

Deploy [Garage](https://garagehq.deuxfleurs.fr/). nessie's S3 face exists so
digest-named blobs are one store for REAPI, kyln and peers. It has no
listing, no arbitrary keys and one static credential.
