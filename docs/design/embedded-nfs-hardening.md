# Embedded NFS hardening — prerequisites for serving live agent workspaces

**Status:** Design / spec (2026-06-20)
**Why now:** the airship FlexClone-per-worker fleet
(`airship/docs/decisions/0001-flexclone-per-worker-fleet.md`) chose to serve every
worker's clone over nessie-store's **embedded** NFS server rather than fall back to
a host kernel nfsd — to keep nessie-store self-contained and replaceable. That
choice makes the items below a hard prerequisite: until they land, the embedded
server is safe only for **scratch/ephemeral** trees, not for git history, SQLite,
or any data whose loss matters.

An adversarial code-reading pass found five issues. Two are **blockers** (silent
data loss; ownership regression); two are **correctness gaps** that any
clone-and-mount workflow trips; one is **defense-in-depth** for the deployment.
Each item below gives the defect with file:line, the fix, and the acceptance test
that must pass before the contract is trusted.

> Scope note: this doc specifies *what must be true and how to prove it*. It does
> not change code. The fixes should land as separate PRs, each with its test.

## Summary

| ID | Issue | Severity | Area |
|----|-------|----------|------|
| F1 | A clone/volume is not mountable from a single `POST` (no mountpoint set at create) | High (correctness) | `nessie-backend-zfs`, `nessie-store::volumes` |
| F2 | `write()` never fsyncs (durability lie) | **Blocker** | `nessie-nfs` |
| F3 | `NFSPROC3_COMMIT` unimplemented → `PROC_UNAVAIL`, while WRITE reports `committed: FILE_SYNC` | **Blocker** | `nfsserve` (vendor/fork) |
| F4 | File handle is a bare inode with no `st_dev` → aliasing across sibling datasets | Medium (correctness) | `nessie-nfs` |
| F5 | AUTH_UNIX creds parsed but ignored + root daemon → files land `root:root` | **Blocker** | `nessie-nfs` (+ deploy securityContext) |

Plus a deployment-level note: the data + control planes are unauthenticated /
cluster-reachable; the network is the only boundary (see "Deployment prerequisites").

---

## F1 — Place clones/volumes under the export root at create time

**Defect.** `create_clone` runs only `zfs clone <src> <dst>` with no `-o mountpoint`
(`crates/nessie-backend-zfs/src/zfs.rs:570-572`); `create_volume` likewise runs
`zfs create [-o quota] <full>` with no mountpoint (`zfs.rs:338-353`). Per
`man zfs-clone`, such a dataset mounts at the mountpoint **inherited from its
parent** — for a default pool that is `/<pool>/<name>` (e.g. `/ontap-sim/clone1`),
**not** under `nfs_export_root` (`/srv`). The NFS server only ever serves the
`/srv` tree (`crates/nessie-store/src/main.rs:86-100`,
`crates/nessie-store/src/config.rs:61-63`). Placement under `/srv` happens **only**
in `patch_volume` via `set_mountpoint`, and only when a `junction_path` arrives
(`zfs.rs:407-412`, target `srv_root + jp` at `zfs.rs:408`); the REST clone branch
(`crates/nessie-store/src/volumes.rs:141`) never sets one. Result: a fresh clone is
unreachable over NFS until a **second** `PATCH …/{uuid} {nas:{path:…}}`.

**Fix (choose one, prefer auto-junction).**
- In `create_clone` (`zfs.rs:546-583`): pass `-o mountpoint=<srv_root>/<new_name>`
  to `zfs clone`, or call `set_mountpoint(new_name, srv_root/new_name)` immediately
  after the clone succeeds; **or**
- In the REST clone branch (`volumes.rs:111-149`): auto-assign a default junction
  (`/<name>`) so the dataset lands under `/srv` with no second call.
- Mirror the same default for `create_volume` if fresh volumes are expected to
  mount directly.

**Acceptance test.** `POST` a clone, then mount the export **without** any
intervening `PATCH` — the clone's contents are visible.

**Interim (zero-code) workaround for callers** until F1 lands: always issue
`PATCH /api/storage/volumes/{uuid} {"nas":{"path":"/<name>"}}` after every clone,
and verify the export answers before mounting.

---

## F2 — `write()` must fsync (BLOCKER)

**Defect.** `PassthroughFs::write` does only `file.write_all(data)` + `file.flush()`
(`crates/nessie-nfs/src/lib.rs:188-201`) — `flush()` drains the userspace buffer but
never calls `sync_all`/`sync_data`, so data sits in the page cache. Combined with F3
this means an acknowledged write can be lost on a nessie/pod/node crash, and git
object/pack files or SQLite databases can tear mid-update.

**Fix.** Call `file.sync_data()` (or `sync_all()`) on write, **or** honor the NFS
WRITE `stable` flag: for `FILE_SYNC`/`DATA_SYNC` requests, fsync before replying;
for `UNSTABLE` requests, defer the fsync to F3's `COMMIT`. (The latter is the
correct, performant design and pairs with F3.)

**Acceptance test.** Write + ack, hard-kill the daemon (and drop caches), restart,
read back — the data is present. A torn-write fuzz over git pack files shows no
corruption.

---

## F3 — Implement `NFSPROC3_COMMIT` (BLOCKER)

**Defect.** `nfsserve` does not handle `NFSPROC3_COMMIT` — it falls through to
`proc_unavail_reply_message` (vendored `nfsserve/src/nfs_handlers.rs` dispatch,
~`:133-160`), so a client `fsync()` (which issues NFS `COMMIT`) gets `PROC_UNAVAIL`.
Worse, WRITE replies hardcode `committed: stable_how::FILE_SYNC`
(`nfs_handlers.rs:1197`) regardless of the requested `stable` flag, so the client
*believes* its data is durable. This is the durability lie that makes F2 dangerous.

**Fix.** Implement `COMMIT` in the nfsserve layer (upstream patch or a maintained
fork): on `COMMIT`, fsync the target file (or the whole export) and return a stable
write-verifier consistent across the daemon's lifetime; stop hardcoding `FILE_SYNC`
for UNSTABLE writes. Track the fork pin in `Cargo.toml`.

**Acceptance test.** A client `fsync()` returns success **only after** a real flush;
a crash immediately after a successful `fsync()` loses nothing.

---

## F4 — Fold `st_dev` into the file handle (aliasing)

**Defect.** The NFS `fileid` and file handle are derived solely from `st_ino`
(`crates/nessie-nfs/src/lib.rs:100,309`) and the handle is the bare 8-byte inode
with no device id (`lib.rs:361-374`). Each ZFS dataset is a distinct filesystem
with its own inode namespace, so a parent volume and its clones reuse low inode
numbers — handles collide across datasets. A single `/srv` export spanning many
sibling clones therefore cannot reliably distinguish files (wrong-file or
`NFS3ERR_STALE`).

**Fix (either).**
- **Operational (no code):** serve **one export per clone** (a separate root /
  `serve()` per dataset). The fleet design wants this anyway (per-worker isolation),
  and it sidesteps aliasing entirely. *Document this as the supported topology.*
- **Code:** incorporate `st_dev` into the fileid/handle (e.g. `hash(st_dev, st_ino)`
  into the 8 bytes, or widen the handle) so a single export can safely span sibling
  datasets.

**Acceptance test.** Two clones mounted under one export: reading the same relative
path in each returns that clone's file, never the sibling's, and no `ESTALE`.

Related: the in-memory `inode → path` map (`lib.rs:68-95`) starts empty on restart,
so handles for not-yet-rewalked inodes return `ESTALE` transiently after a restart.
With `hard` mounts the kernel re-LOOKUPs and self-heals; document the expectation so
brief restarts don't abort in-flight git ops.

---

## F5 — Honor AUTH_UNIX ownership (BLOCKER)

**Defect.** PassthroughFs never chowns: `create` calls `path_setattr`, which
deliberately refuses uid/gid (vendored `nfsserve/src/fs_util.rs:146-151` logs
"Set uid/gid not implemented"); `mkdir` is a bare `create_dir` with no setattr
(`lib.rs:240-252`). The AUTH_UNIX credential is parsed into `context.auth`
(`nfsserve/src/rpcwire.rs:30-32`) but read only by a `Debug` impl — no VFS op
consumes it and there is no `seteuid`/`setfsuid` anywhere. The shipped deployment
runs the container `privileged: true` with **no** `runAsUser`
(`deploy/k8s/deployment.yaml`), i.e. as root. Net: every file created through the
embedded server lands `root:root`.

This breaks the agent-workspace co-ownership contract (drake UID 1000 / GID 100
`users` / supplementary 1001 `agents`, share mode `2775` set-GID so pod and gnuc
both read/write): gnuc's `hartsock` (UID 1000) cannot modify root-owned files, and
git fires `detected dubious ownership` on **every** repo — a hard regression versus
netapp-sim's kernel export (`no_root_squash` + real AUTH_UNIX mapping).

**Fix.**
- Thread `context.auth` (the AUTH_UNIX uid/gid) into the VFS trait and implement
  `chown(uid, gid)` in `create`/`mkdir`/`setattr` using the client credential
  (`libc::chown` / `std::os::unix`), since `fs_util` will not.
- Explicitly set mode on `mkdir`/`create` and **propagate the parent set-GID bit**
  (or chown gid to 100 and `chmod g+s` on new dirs).
- Run the daemon with a fixed identity capable of `chown` (root-with-explicit-chown,
  or a pinned uid + `CAP_CHOWN`). Note: a non-root daemon cannot `chown` to
  arbitrary uids without `CAP_CHOWN`.
- Set `dataset_owner = 1000:1001` and `dataset_mode = 2775` (today both `None`,
  `zfs.rs:60-61`) so the volume root carries the contract.

**Acceptance test (the contract test).** Mount the export, create a file as a
UID-1000 AUTH_UNIX client, and assert the on-disk owner is `1000:100` with the
set-GID-inherited group; `git status` is clean and a one-line edit yields a
**non-empty** `git diff`. Set `safe.directory='*'` on both pod and gnuc regardless.

---

## Deployment prerequisites (defense in depth)

Independent of the code fixes, the embedded planes are unauthenticated and
cluster-reachable as shipped:

- 2049 (NFS) is AUTH_UNIX/anon with **no** per-client ACL
  (`crates/nessie-nfs/src/lib.rs:31-32`); `mount` grants a handle to anyone who
  names the export.
- REST is a single shared admin Basic-auth (`crates/nessie-store/src/auth.rs`);
  `secret.example.yaml` defaults to `change-me`. One credential = full ONTAP
  control, including `DELETE` (→ `zfs destroy -r -f`).
- The Service fronts both ports as ClusterIP with **no NetworkPolicy** in
  `deploy/k8s/`; the Deployment has no node pin (only `pv.example.yaml` does, and
  it's opt-in).

Before the agent fleet uses it: add a **NetworkPolicy** restricting 8443/2049
ingress to known consumers; set a **real, rotated admin password** via Vault
Secrets Operator; pin the pod (`nodeSelector`/affinity) to the ZFS node; keep
ClusterIP (no `hostNetwork`); and deny `pods/exec`/`pods/attach` on the namespace.
Per-worker isolation must come from **each worker's own export**, never from
uid/gid.

## Test policy

Per the workspace rule, unit tests must not touch a real filesystem; the
ownership/durability/aliasing assertions above are **integration tests** that mount
a real export and run single-threaded in the release gate (not in the parallel
unit pass).

## References

- airship ADR 0001 (the consumer of this work).
- Source: `crates/nessie-nfs/src/lib.rs`,
  `crates/nessie-backend-zfs/src/zfs.rs`,
  `crates/nessie-store/src/{volumes,main,config,auth}.rs`,
  `deploy/k8s/deployment.yaml`; vendored `nfsserve` (`nfs_handlers.rs`,
  `fs_util.rs`, `rpcwire.rs`).
