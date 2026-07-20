# formal/ — machine-checked design logic for the P2P CAS/AC swarm

The design in [`docs/design/p2p-cas-swarm.md`](../docs/design/p2p-cas-swarm.md)
rests on a few load-bearing claims about a system that has no central authority.
Prose can assert them; this directory **proves** them, so the design logic is
enforced rather than believed. Two tools, split by what each is good at:

- **TLA+ / TLC** — *distributed safety*: what a swarm of nodes with gossip and a
  Byzantine minority can and cannot do.
- **Lean 4** — *algebraic facts*: that the CRDT merge is a join-semilattice and
  that confirmation is monotone. Lean-core only (no Mathlib) so it builds offline.

Run everything: `./check.sh` (needs `lake` on PATH; fetches `tla2tools.jar` into
`.cache/` if `$TLA2TOOLS_JAR` is unset).

## Proof obligations

| ID | Claim (from the design doc) | Artifact | Tool | Status |
|----|------------------------------|----------|------|--------|
| **PO-AC-1** | No-forgery: with a Byzantine signer minority (`\|Byzantine\| < K`), no false result is ever confirmed at any node. | `tla/AcCrdt.tla` · `NoForgery` | TLC | ✅ checked |
| **PO-AC-2** | Agreement: no two nodes confirm different results for the same action. | `tla/AcCrdt.tla` · `Agreement` | TLC | ✅ checked |
| **PO-AC-3** | Monotonic confirmation: the store only grows, so a confirmation is never revoked. | `tla/AcCrdt.tla` · `MonotoneStore` **+** `lean` · `confirmed_monotone`, `confirmed_stable_under_merge` | TLC + Lean | ✅ checked |
| **PO-AC-4** | Convergence: the merge is a join-semilattice (commutative, associative, idempotent) ⇒ strong eventual consistency; duplicate delivery is harmless. | `lean` · `union_comm`/`union_assoc`/`union_idem`, `merge_confluent`, `merge_absorb_redelivery` | Lean | ✅ checked |
| **PO-AC-B** | Boundary/tightness: at `\|Byzantine\| = K` the guarantee **breaks** — a Byzantine cohort of size `K` forges a confirmation. Proves the minority hypothesis is load-bearing, not decorative. | `tla/AcCrdt_ByzThreshold.cfg` · counterexample to `ForgeryFree` | TLC | ✅ demonstrated |
| **PO-GC-1** | Durable-mode GC never collects a *reachable* blob (git-style mark-and-sweep correctness). | *(planned — `tla/Gc.tla`)* | — | ⏳ stated, not yet formalized |
| **PO-GC-2** | Cache-mode eviction never loses a *reachable* blob (a durable holder retains it) ⇒ no reachable blob lost swarm-wide. | *(planned — `tla/Eviction.tla`)* | — | ⏳ stated, not yet formalized |

The ⏳ rows are the storage-mode safety pair from the design doc's
[Node storage modes](../docs/design/p2p-cas-swarm.md) section. They are named here
so the gap is visible; formalizing them is the next formal slice. Nothing above
claims them as proved.

## Assumptions (hypotheses, not proved here)

These proofs are only as good as their premises. Stated explicitly:

- **Signed-peer membership.** The signer set and the honest/Byzantine split are
  *given*. Establishing them — who may sign, sybil resistance, how a node joins —
  is the **swarm-join / signed-peer primitive** (agent-mesh), shared with the
  swarm's other members. These models consume its output; they do not model it. If
  the membership primitive fails to bound the Byzantine set below `K`, PO-AC-1
  falls (and PO-AC-B shows exactly how). This is the one place the nessie CAS/AC
  design and agent-mesh must share a primitive rather than each rolling their own.
- **Action determinism.** Only deterministic actions are cacheable (REAPI's
  `do_not_cache` marks the rest), so a given action has a single true result —
  the premise that makes AC a partial function rather than a mutable register.
- **Digest collision-resistance.** Content addresses are treated as injective
  names; the cryptographic hardness of the hash is assumed, not proved.

## Layout

```
formal/
  check.sh                     run all checks; exit 0 iff all expectations met
  tla/
    AcCrdt.tla                 the AC attestation-CRDT + its safety properties
    AcCrdt.cfg                 main model: 2 nodes, 3 signers, 1 Byzantine, K=2 (PASS)
    AcCrdt_ByzThreshold.cfg    boundary model: 2 Byzantine, K=2 (FAILS by design)
  lean/
    NessieFormal.lean          semilattice + confirmation-monotonicity proofs
    lakefile.toml              no deps (Lean core only)
    lean-toolchain             pinned Lean 4.32.0
```

## Enforcement

`./check.sh` is the single gate; it is intended to run as a CI `formal` job on
every PR that touches `formal/` or the CAS/AC design. Wiring that job into
`.github/workflows/ci.yml` (and the push hook, per the repo's
[hook-parity governance](../CLAUDE.md)) is a follow-up — it touches CI config, so
it lands as its own reviewed change rather than riding in with the specs.
