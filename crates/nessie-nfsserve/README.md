# nessie-nfsserve

A vendored, **hardened fork** of [`nfsserve`](https://github.com/huggingface/nfsserve)
(v0.11.0) — the NFSv3 wire/transport layer behind nessie-store's embedded NFS
data plane.

## Why a fork?

nessie-store serves NFS **in-process**, with no host kernel NFS server, so that
the daemon stays self-contained and replaceable (airship ADR 0001). Backing live
agent workspaces over it surfaced gaps the upstream crate does not address, and
which cannot be fixed from a downstream `NFSFileSystem` impl alone:

- **`NFSPROC3_COMMIT`** — upstream dispatches it to `PROC_UNAVAIL`, so a client
  `fsync()` fails. The fork dispatches it to a new `NFSFileSystem::commit` hook.
- **Stable-flag-aware `WRITE`** — upstream hardcodes `committed: FILE_SYNC` on
  every reply regardless of the requested stability, a durability lie. The fork
  threads the client's `stable` flag through to the filesystem and reports the
  honest `committed` level.
- **AUTH_UNIX credential threading** — upstream parses the caller's uid/gid into
  `RPCContext::auth` but never exposes it to the filesystem. The fork passes a
  public `UnixCred` into `create`/`mkdir`/`setattr` so files can be owned by the
  caller instead of the daemon (`root:root`).

See `docs/design/embedded-nfs-hardening.md` (F2/F3/F5) in the workspace root.

## Provenance & license

This crate is derived from `nfsserve` 0.11.0. The upstream source is
BSD-3-Clause (Copyright © 2023 XetData, © 2025 Hugging Face); that license is
preserved verbatim in `LICENSE-UPSTREAM-BSD3` and continues to govern the
upstream-derived portions. nessie-store's additions are dual-licensed
`MIT OR Apache-2.0` like the rest of the workspace. The combined crate is
therefore `BSD-3-Clause AND (MIT OR Apache-2.0)`.

The public crate name is `nessie-nfsserve`; the library is imported as
`nessie_nfsserve` to make the fork explicit at every call site.
