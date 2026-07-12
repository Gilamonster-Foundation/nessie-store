# Design: demand-paged NFS gateways (git-LFS + Perforce)

**Status:** experimental / future · cargo-feature-gated, default **off** · off the
v1.0 path. Tracking epic: **#71**.

## Motivation

Some backing stores are too large to sync in full onto the node that wants to read
them: a git-LFS-backed repository whose history dwarfs local disk, or a Perforce
view spanning millions of files. But a workload usually touches only a small *hot
set*. This design presents such a store as a **single NFS mount** while keeping
only the working set resident locally — cold files are fetched on demand and
evicted under budget.

Two concrete surfaces are in scope:

- **`gitlfs-nfs-gateway`** — a git repository (at a chosen ref/commit), git-LFS
  objects fetched lazily.
- **`p4d-nfs-gateway`** — a Perforce view pinned at a changelist.

## The shared pattern

Both are the *same* thing over two backing stores:

> a **demand-paged `NFSFileSystem`** whose namespace is synthesized from remote
> metadata, whose file content is fetched on first read into a
> **content-addressable LRU working-set cache**, and which is served over the
> daemon's existing embedded NFS listener.

So the plan builds the **shared core once** and adds two thin adapters — mirroring
how `#52` is the shared root for the CIFS and NFSv4.1 epics. The core is a second
implementation of `nessie_nfsserve::NFSFileSystem` (a sibling to `PassthroughFs`),
plus a cache crate.

### Three layers

1. **Namespace (metadata-only).** Resolve the source to an immutable tree —
   git `ref → commit → root tree`, or `p4 fstat //view/...@change`. Map it to the
   NFS namespace: trees/directories → NFS dirs, blobs/files → NFS files. `getattr`,
   `lookup`, and `readdir` answer entirely from metadata and **never fetch
   content** — an `ls -lR` triggers zero downloads. File sizes come from the git-LFS
   pointer's declared size / `p4 fstat` size, so `stat` is correct without paging
   anything in. Stable NFS fileids are derived with the `PassthroughFs`
   `splitmix64` mixer, but keyed on the tree path (`hash(source_id, path[, rev])`)
   rather than `(st_dev, st_ino)` — cold files have no local inode.

2. **Demand paging.** `read(id, off, count)` resolves the id to a content key and
   asks the cache for the bytes. A miss fetches the object (git promisor / LFS batch
   API, or `p4 print`), verifies it against the known size/hash, stores it, and
   serves the range. Concurrent readers of the same key are coalesced single-flight
   (a `dashmap` of in-flight fetches) so a thundering herd yields one fetch.

3. **LRU working-set cache.** A byte-budgeted (and inode-budgeted) on-disk store,
   keyed by **content identity** (git blob / LFS `sha256`, or `(depot_path, rev)`).
   LRU eviction under budget, with a per-entry **pin refcount** so a file with an
   open NFS handle (or an in-flight partial read) is never evicted mid-serve.
   Because the key is content-addressed, two refs/tags/clones that share a blob
   share one cache entry — the FlexClone "shares blocks until divergence" property
   falls out for free.

## Faithfulness

nessie-store's discipline is *"the daemon does not broker bytes"* — a backend hands
out an `AccessHandle` and the client reads the substrate directly. Taken literally,
an in-process NFS server that serves file bytes contradicts that. **But the repo
already crossed that line, deliberately:** `nessie-nfs` is the ZFS data plane — the
daemon runs an embedded userspace NFSv3 server so operators need no host-kernel NFS.
The discipline is preserved not by refusing to serve bytes but by keeping the
**REST control plane byte-free**: REST returns an `AccessHandle::NfsExport`, and
bytes flow over a *separate* NFS data plane.

The demand-paged gateways occupy exactly that role — a data plane, not the control
plane — and reuse the same `AccessHandle::NfsExport` variant. They are a **thicker**
adapter than `PassthroughFs` (which reads bytes already on local disk): they own a
cache, an eviction policy, and a materialization step, so they own correctness
properties passthrough never had (miss latency, eviction-vs-read races, coherency).
The design keeps that honest rather than papering over it:

- **Read-only first.** Write-back (`git`/`p4` commit-on-flush) is not implemented;
  writes return `NFS3ERR_ROFS`. Pretending to be writeable would be a lie.
- **Immutable-snapshot regime.** A git ref pinned to a commit, or a `p4` view pinned
  at a changelist, is immutable — so a cache hit is provably never stale, and there
  is nothing to emulate. This maps 1:1 onto the `SnapshotBackend` tier
  (volume=branch/stream, snapshot=tag/changelist).
- **Honest latency.** A cold read that outruns a staging deadline returns
  `NFS3ERR_JUKEBOX` — the protocol's own "near-line storage is staging, retry" code
  (already present in `nfs.rs`) — instead of blocking indefinitely or lying.
- **Zero-byte metadata ops.** `create_snapshot` = a git tag / a pinned changelist;
  `create_clone` = a new branch ref. Pure metadata; no bytes moved. This is the
  "don't broker bytes" spirit expressed at the control plane.

Net: a justified, bounded, documented extension of the embedded-NFS lineage — not a
silent contradiction of the `AccessHandle` thesis. The default git-LFS/p4d backend
access handles (`GitRef` / `P4Stream`) stay the byte-honest path; the NFS gateway is
the opt-in inversion for clients that physically cannot materialize the whole store.

## Scope & non-goals (first cut)

**In:** read-only serving; immutable (pinned) sources; LRU cache with eviction +
pinning; `NFS3ERR_JUKEBOX` staging; AUTH_SYS ownership synthesized from config
(git/p4 carry no per-file Unix uid/gid — a static owner is the honest mapping).

**Out (deferred, each behind its own feature + ADR):** write-back
(commit/submit-on-flush — changelist management, conflict/resolve, push auth);
head-follow of unpinned sources; Kerberos; pNFS.

## Key risks

- **Cold-read latency vs NFS client timeouts** — a multi-GB object cannot
  materialize inside one READ RPC. Mitigated by `JUKEBOX` + readahead prefetch;
  needs real-client tuning.
- **Reactor starvation** — git/p4 subprocess + hash verification are blocking/CPU
  work and must go through `spawn_blocking`; a stray blocking call would stall the
  shared tokio runtime that also serves REST.
- **Eviction-vs-read correctness** — the pin refcount and single-flight interaction
  must be airtight, or a file evicted mid-read serves truncated/wrong bytes.
- **Backing-store availability** — partial clone / p4d make the remote a runtime
  dependency of every cold read; the failure mode differs from local-disk ZFS and
  must be surfaced honestly.
- **Credentials** — private repos / LFS batch API / p4 tickets are secrets the
  daemon now holds; they must not leak into an `AccessHandle` or logs.
- **Dependency weight** — `gix`/`git2` + `reqwest` + an LFS client materially grow
  the build; the cargo feature must keep them out of default builds.

## Open questions

- One mount = one pinned ref/changelist (simplest, immutable, stable fileids), or a
  multiplexing root whose top-level dirs are refs/views?
- Cache location + sizing: the cache must live on local disk (never the quota-bound
  workspace NFS); what are sane default byte/inode budgets and eviction policy
  (size- vs time-based)?
- Per-file size larger than the whole cache budget — reject, stream-through, or
  reserve a per-file minimum?
- Should `nessie-p4-client` / the git layer be shared crates consumed by both the
  planned `nessie-backend-p4d` / `nessie-backend-gitlfs` backends **and** the
  gateways, or standalone until those backends exist (#72)?
- Does write-back ever land, and if so, whose identity authors the commit, and how
  does that reconcile with AUTH_SYS caller ownership on the NFS side?

## Relationship to other work

- **`#72`** — the plain `nessie-backend-p4d` / `nessie-backend-gitlfs` backends
  (which hand out `P4Stream` / `GitRef` handles) are a *different presentation* of
  the same substrates; the gateways can reuse their p4/git client layers.
- **`nessie-nfs` / `nessie-nfsserve`** — the embedded NFSv3 server the gateways
  serve over; `PassthroughFs` is the reference `NFSFileSystem` impl to mirror.
- **[embedded-nfs-hardening.md](embedded-nfs-hardening.md)** — the F1–F5 write
  durability / ownership work whose ownership (F5) contract the gateways follow.
