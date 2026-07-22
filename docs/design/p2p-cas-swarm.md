# Design: nessie-store as a P2P content-addressed swarm

**Status:** design **settled + slice-1 built**. The gating decisions are settled
(2026-07-20, operator — see [Decisions](#decisions--settled-2026-07-20-operator)),
including node storage modes. The single-node CAS spine is **merged to `main`**
(`Digest` #81, `CasBackend` #84, `MemCas` + `nessie-cas-conformance` #85,
`AccessHandle::CasBlob` #86; formal model #83). The swarm layer is next — see
[Implementation status](#implementation-status).
Board card `knowledge/board/nessie-store/2026-07-20_p2p-cas-reapi-design-handoff.md`
(P1) · supersedes the near-term REAPI listing in
`knowledge/board/nessie-store/2026-05-17_direction.md`.

This doc opens the P2P design context. It is deliberately a *design* artifact,
not a landed slice: it names the data model, the one genuinely hard problem, and
the decisions that gate implementation. Near-term ONTAP-sim / SnapMirror work is
unaffected and continues in parallel.

## Problem

nessie's stated role is *"the storage substrate for the gilamonster swarm"*
(CLAUDE.md), and the stack already leans on NATS. But the P2P/CAS data model has
never been spelled out — the repo today is a volume-centric ONTAP REST daemon
over a `VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend` trait stack. The direction
is a **P2P content-addressed swarm: headless nodes, no central management complex,
"comes up like Napster/Limewire."**

The load-bearing insight that makes this one design instead of two:
**content-addressing *is* the P2P-native data model.** Napster, BitTorrent, and
IPFS all work because a hash is *location-independent* — any peer holding blob `H`
can serve it, integrity is self-verifying, dedup is automatic. "Headless swarm,
no central complex" and "Merkle-DAG CAS" are the same design. The CAS is *what
makes the swarm work*.

REAPI (the Bazel remote-cache API) is **not** the core. It is a long-term
aspirational protocol *face* bolted on top of the CAS core — see
[REAPI as a face](#reapi-as-a-long-term-face-not-the-core) below.

## The data model — CAS is a distinct backend role, not a volume tier

The existing trait stack is *volume-centric*: its noun is a `VolumeUuid`, an ONTAP
volume. CAS's noun is a **digest** — a hash of bytes. Forcing CAS into
`VolumeBackend` would be a category error (a digest is not a mutable dataset).
So the CAS is a **new, cohesive backend family** that sits *beside* the volume
stack, not above or below it:

```rust
/// Immutable content-addressed blob storage. The P2P-native substrate.
pub trait CasBackend: Send + Sync {
    /// Does this node hold the blob for `digest`? (local check, no fetch)
    fn has(&self, digest: &Digest) -> Result<bool, BackendError>;
    /// Read a blob by digest. Verifies bytes hash to `digest` before returning.
    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError>;
    /// Store bytes; the digest is *computed*, not supplied. Returns the digest.
    fn put(&self, bytes: &mut dyn Read) -> Result<Digest, BackendError>;
}

/// The mutable-ish half: "action X produced result Y". Reached only where a
/// CasBackend can also attest results. This is the HARD tier (see below).
pub trait ActionCacheBackend: CasBackend {
    fn get_action_result(&self, action: &Digest) -> Result<Option<ActionResult>, BackendError>;
    /// Record a *signed attestation* that `action` produced `result`.
    /// Not a blind overwrite — attestations accumulate (see the CRDT below).
    fn attest_action_result(&self, action: &Digest, att: Attestation) -> Result<(), BackendError>;
}
```

This mirrors the repo's existing discipline exactly:

- **Honest capabilities.** `Capabilities` grows a `content_addressed: bool` (and,
  later, `action_cache: bool`). A backend that can store blobs but not attest
  results advertises `content_addressed = true, action_cache = false`, and the
  REAPI/NFS faces return "feature not supported" for the AC surface — the same
  pattern `snapshots`/`clones`/`replication` already follow.
- **`mem` first.** A `HashMap<Digest, Bytes>` `CasBackend` is the reference impl
  and the conformance sanity check, exactly as `nessie-backend-mem` is for volumes.
- **Substrates opt in.** `s3` is content-addressed *natively* (key = digest); `zfs`
  can store blobs as files under a digest-fanned path. Each passes a new
  `nessie-cas-conformance` suite chosen from `capabilities()`.
- **Loosely coupled, functionally cohesive.** The CAS trait family is cohesive
  around content-addressed blobs and fully decoupled from the ONTAP volume trait.
  Nothing in `VolumeBackend` changes.

### The digest — a self-describing multihash, not a bare algorithm

The `Digest` newtype follows the `ids.rs` pattern but is **content-addressed and
self-describing** rather than random:

```rust
/// A self-describing content digest: <multihash-code><length><bytes>.
/// Default profile: BLAKE3-256. The code travels *with* the digest so a peer
/// can verify without out-of-band agreement, and the default can forward-ratchet
/// (BLAKE3 today) without a flag day.
pub struct Digest(/* multihash bytes */);
```

Rationale (workspace law: *multihash over specific algos* — laws name
properties, profiles pin algos, identifiers self-describe; *freeze minimally* —
today's BLAKE3 pin rotates via a forward ratchet, never a flag day). The **native**
CAS speaks self-describing multihash. This becomes load-bearing at the REAPI
boundary, where it collides with a protocol constraint — see the face section.

## The one hard problem — AC over P2P — chase this first

The two halves of the model split cleanly by difficulty:

| Layer | P2P difficulty | Why |
|---|---|---|
| **CAS** (immutable blobs) | **easy — P2P-native** | this is literally BitTorrent/IPFS; a blob is self-verifying and any holder can serve it |
| **ActionCache** (`action → result`) | **hard** | it *looks* like a mutable register, and consistency over a gossip swarm is a real distributed-systems problem |

Standard REAPI assumes a **central, strongly-consistent** CAS+AC. The swarm has
neither. The crux is the AC. The rest of this section argues that the crux is
**less hard than it first appears**, and that the honest solution is *also* the
keystone dogfood — one mechanism, two payoffs.

### Reframe: for deterministic actions, AC is write-idempotent, not mutable

An AC entry maps `digest(Action) → ActionResult`, where the Action digest pins the
*entire* input root Merkle tree — command, environment, and every input blob,
**including the immutable spec**. REAPI already assumes only **deterministic**
actions are cacheable (`do_not_cache` marks the rest). For a deterministic action
there is exactly **one** correct result. Therefore:

> AC is not an arbitrary mutable map. It is a **partial function being
> incrementally discovered.** Two honest peers can only ever write the *same*
> value for a given Action digest. A *conflicting* write signals
> non-determinism or a lying peer — not a race to be linearized.

This collapses the hard problem:

- **Correctness needs no consensus.** A stale AC read never yields a *wrong*
  build — at worst it yields a *missing* entry, and the cost of a miss is a
  **redundant re-execution** (wasted compute), which is idempotent. So "accept
  eventual consistency + make completion idempotent" is not a compromise; for
  deterministic actions it is *correct by construction*.
- **AC becomes a CRDT.** Model each AC entry as a **grow-only set of signed
  attestations**: `action_digest → { Attestation{ result_digest, executor_id,
  sig }, ... }`. Set-union is a join-semilattice — it converges under gossip with
  no coordinator. Reads resolve an entry as *confirmed* iff **≥ k independent,
  agreeing signatures** are present.
- **Trust and non-determinism fall out of the same gate.** A single peer asserting
  a result is *unconfirmed* (`k = 1` is opportunistic cache, fine for reuse, not
  for trust). Non-determinism surfaces as **disagreeing** attestations under one
  key — detectable, not silently wrong. A malicious peer forging a result cannot
  reach the `k`-agreement threshold without `k` colluding signers.

**Recommendation for the crux:** eventual-consistency CAS/AC over gossip, AC as a
grow-only set of signed attestations, `k`-of-`n` agreement as the confirmation
gate. **No global consensus, no lease service.** This satisfies workspace *law
minimalism* — the semilattice converges on its own; nothing needs a consensus law
above the mechanism line. Signatures/identity come from **agent-mesh** (signed
peers, caveat-scoped access, no central auth server) — the swarm already has the
identity primitive it needs; nessie consumes it rather than inventing one.

### The keystone this unlocks — ungameable completion

The same `k`-of-`n` attested AC entry *is* the answer to the ceiling-paper
problem (`✓ plan complete` is a forgeable claim; the harness watched five runs
pass while building nothing). "Did the agent complete task X correctly?" becomes:

> Is there a **confirmed** AC entry for `digest(Action_X)` whose output digests
> verify — where `Action_X`'s digest pins the immutable spec?

This **cannot be faked by weakening the spec**, because the spec is *inside* the
Action digest: change the spec and you are asking about a *different* action. This
is `--locked-verify` generalized to the protocol layer. It is why nessie-CAS is a
*verification substrate* for the swarm's work, not merely a storage product — and
it is the same mechanism as the consistency solution above, not a second feature.

The agent-mesh context bus (`knowledge/board/newt-agent/MEMO_2026-07-20_agent-mesh-context-bus.md`)
persists here: its `Delegate`/`Status`/`Note` messages are CAS objects, and a task
**state-flip (open → done) becomes an attested AC action.**

## Content routing — the swarm-shape decision

"Who holds digest `H`?" is the one question a headless swarm must answer. This is
the classic Napster (central index) vs Gnutella/Limewire (flood) vs BitTorrent-DHT
(Kademlia) axis. The card frames it as **DHT vs NATS-rendezvous vs hybrid**.

The design move that makes this a *deferrable* decision rather than a *blocking*
one is to put it behind a narrow seam:

```rust
/// How the swarm answers "who has this digest?". CAS/AC never see the mechanism.
pub trait ContentRouter: Send + Sync {
    fn providers(&self, digest: &Digest) -> Result<Vec<PeerAddr>, BackendError>;
    fn announce(&self, digest: &Digest) -> Result<(), BackendError>;
}
```

- A **NATS** `ContentRouter` (request-reply on `cas.have.<digest>`, or a NATS-KV
  provider bucket) reuses infra already in the stack — fastest path to a *working*
  swarm. Honest caveat: a NATS cluster is a coordination point — lighter than
  BuildBarn by orders of magnitude, but not literally zero-central. Say so.
- A **Kademlia** `ContentRouter` is the truly Napster-free variant (IPFS/BitTorrent
  lineage). More to build: NAT traversal, cold-start, provider-record TTLs.

**Decision (settled — hybrid, both routers first-class):** ship **both** a NATS
rendezvous router *and* a Kademlia DHT router as first-class implementations of
`ContentRouter` from the start, selectable (and composable) per deployment. The
NATS router serves environments that already run the infra; the DHT router serves
open peers and delivers the literal "no central complex" pitch. Both sit behind
the one seam, so CAS/AC never see which is in play, and a node can consult both
(local NATS first, DHT fallback) without either layer knowing. This front-loads a
little more surface than a NATS-only v0, but settles the Napster-vs-Gnutella-vs-DHT
axis by *refusing to pick* — the deployment picks. The `ContentRouter` seam is what
makes that cost bounded.

The data plane is already P2P from day one regardless of router — `AccessHandle`
gains a `CasBlob { digest, providers: Vec<PeerAddr> }` variant, so a client fetches
bytes *directly* from a holding peer (matching the repo's existing "daemon does not
broker bytes" discipline). Only *discovery* differs between the two routers.

## Node storage modes — cache vs durable store of record

A nessie node runs its local disk in one of two modes. The two modes differ in
their **retention rule** — *what is allowed to leave local disk, and why* — and
the design deliberately borrows git's object-store model, because git already *is*
a CAS (blobs/trees/commits keyed by hash, reachable from refs).

- **Durable store-of-record mode — git-style reachability GC.** The node is a
  source of truth: it retains every blob that is **reachable** and never relies on
  a peer to restore anything. It is *not* "never delete" — it runs a **git-style
  mark-and-sweep garbage collector** that reclaims only **unreachable** blobs
  (garbage: aborted uploads, superseded intermediate trees, orphaned outputs no
  root points at). Exactly `git gc`: reachable from the roots is kept, everything
  else is swept.
- **Cache mode — LRU, leaning on a durable node.** Local disk is a **bounded**
  cache. Recently-used blobs stay local; cold blobs are **evicted by LRU** and
  "float" to the swarm — even *reachable* blobs, because a later read re-fetches
  them by digest. This is safe precisely because at least one **durable node**
  holds the restore bits. Content-addressing makes the round trip trustworthy: a
  re-fetched blob is self-verifying (`Digest::verify`), so restoring from *any*
  peer is sound, and eviction stays a purely local decision.

### Reachability — the root set

A blob is **reachable** iff some **root**'s Merkle-DAG closure includes its digest
(the transitive `root → … → tree → blob` walk, identical to git's
`ref → commit → tree → blob`). The roots are the mutable pointers into the
immutable DAG:

- **confirmed AC entries** and the output DAGs they name (the ungameable-completion
  keystone — these anchor the results worth keeping),
- **named refs / tags / explicit pins**,
- **agent-mesh identity material**,
- **actively-referenced Merkle roots** (live workspaces, open sessions).

GC marks from these roots and sweeps the rest. The root set is small and typed;
the reachable closure is whatever the swarm's live work still points at.

### The paired safety property

Two rules, one guarantee — *no reachable blob is ever lost, swarm-wide*:

1. **Durable GC never collects a reachable blob** (mark-and-sweep correctness: only
   the unmarked, unreachable set is swept).
2. **Cache eviction never loses a reachable blob**, because a durable node retains
   it (eviction is gated on the
   [`ContentRouter`](#content-routing--the-swarm-shape-decision) confirming ≥ 1
   durable holder — or ≥ R providers where no durable node is present; an
   otherwise-last copy is pinned and replicated outward before it may be evicted).

So a swarm stays lossless as long as every reachable blob has a durable home (or
R-way cache replication). That is a **deployment property the design surfaces and
the testbed below measures** — not a hope. These two rules are stated as
machine-checked obligations in the formal models (`formal/` — see the
formal-methods PR track).

### Mechanics and config

Reachability GC and LRU eviction sit behind one seam over `CasBackend` — a
`CasStore` wrapper carrying a `RetentionPolicy` (three-Cs: Configuration picks the
*mode*, Composition picks the GC/eviction *policy*; LRU is the cache default). A
`[cas]` block (workspace config law: lean core, typed knobs) selects
`mode = "durable" | "cache"`, a GC schedule for durable mode, and — for cache mode
— a byte budget, eviction `policy`, and replication factor `R`. This is a **later
slice**: it needs the `ContentRouter` (to check durable/replica holders), so it
lands after the swarm layer, not in the single-node first slice.

### Validation testbed — nuc1 / nuc2 / gnuc

The float-to-swarm and restore-from-durable behaviour is testable **for real** on
the home swarm: run one node **durable** (the store of record, GC only) and the
others **cache** (LRU, evicting under a tight byte budget), then assert that a
blob evicted from a cache node is transparently restored by digest from the
durable node, that durable GC reclaims only unreachable blobs, and that killing a
cache node loses nothing. A three-node layout (**nuc1 + nuc2 + gnuc**) exercises
multi-holder routing and the ≥ R path, not just a single durable fallback. This
is the integration counterpart to the formal safety proofs — the proofs say the
rules are sound; the testbed says the implementation obeys them.

## REAPI as a long-term face, not the core

REAPI v2 (`build.bazel.remote.execution.v2`) is CAS + ActionCache + Execution +
Capabilities. nessie targets the **cache subset — CAS + AC only** (remote *cache*,
no remote *execution*): a small, well-trodden target that delivers dedup +
integrity + the keystone. Full remote Execution (gnuc-as-executor) is a later,
much larger step.

Why it is a customer unlock, and why the pitch *is* the P2P angle: every REAPI
deployment today (BuildBarn, BuildGrid, BuildBuddy) is a *central, highly-available
CAS+AC complex you must run.* nessie's pitch: **"a Bazel remote cache with no
BuildBarn to stand up — it's a self-organizing swarm."** That differentiator is
exactly the headless-no-central-complex goal, sold to the Bazel customer base.

Architecturally the REAPI face is a **gRPC server over `Arc<dyn CasBackend>` +
`Arc<dyn ActionCacheBackend>` + `Arc<dyn ContentRouter>`** — the exact symmetry
with how the ONTAP REST face sits over `Arc<dyn VolumeBackend>`. The NFS
read-through surface from the 2026-05-17 direction doc becomes a *third* face over
the same CAS. Faces are interchangeable; the CAS core is the spine.

**The one real friction — the digest function at the boundary.** REAPI v2 pins its
digest function per-instance (SHA-256 is the near-universal default; the set is
negotiated via `GetCapabilities`). nessie's *native* CAS speaks self-describing
multihash (BLAKE3 default). These do not have to agree: the REAPI face **pins the
REAPI-negotiated function (SHA-256) as its wire contract** and translates to/from
the native multihash at the boundary — the same way `AccessHandle` variants are
substrate-native while the REST shape is protocol-native. This keeps *multihash
over specific algos* intact internally while honoring REAPI's fixed contract
externally. (A node serving REAPI must index blobs under both digests, or compute
the SHA-256 view lazily on REAPI reads — a measurable tradeoff to settle when the
face is built, not now.)

## Convergence — this connects the board, it does not add a project

- **nessie-store** — the P2P CAS swarm substrate (its stated role).
- **agent-mesh** — signed peers + caveat-scoped access, no central auth server:
  supplies the identity/signature primitive the `k`-of-`n` AC gate consumes. Its
  context bus persists as CAS objects.
- **NATS** — already in the stack; the v0 content-router rendezvous and the swarm's
  nervous system.
- **kyln** — P4 CLN → CAS hash: Perforce changelists become addressable, dedup-able
  content in the same swarm.
- **monty-tui** — the swarm + storage dashboard (already exists).
- **honest-gate / ceiling paper** — ungameable completion via the confirmed AC
  entry above.

## Decisions — settled (2026-07-20, operator)

These gated the first implementable slice; all four are now settled.

1. **Content routing / swarm shape → hybrid.** Both a NATS rendezvous router *and*
   a Kademlia DHT router are first-class behind the `ContentRouter` seam; the
   deployment selects (or composes) them. The axis is settled by refusing to pick
   one — the seam makes carrying both affordable.
2. **AC consistency model → eventual + attestation CRDT.** AC is a grow-only set of
   signed attestations, confirmed at `k`-of-`n` agreement. No consensus, no lease.
   This is also the ungameable-completion keystone — one mechanism, two payoffs.
3. **Backend and face → both.** CAS is a new **backend** trait family (`CasBackend`
   / `ActionCacheBackend`) beside the volume stack, and REAPI is a **gRPC face**
   over it — symmetric with ONTAP-REST-over-`VolumeBackend`.
4. **Native digest ↔ REAPI digest → multihash native, SHA-256 at the boundary.**
   Self-describing multihash (BLAKE3 default) internally; the REAPI face pins
   SHA-256 as its wire contract and translates at the boundary.
5. **Node storage modes → cache and durable store of record.** A node runs local
   disk either as a bounded **LRU cache** (cold blobs float to the swarm, restored
   by digest from a durable node) or as a **durable store of record** that retains
   all *reachable* blobs and runs **git-style reachability GC** to reclaim only
   *unreachable* garbage. Paired safety: durable GC never collects a reachable
   blob and cache eviction never loses one (a durable holder retains it) — so no
   reachable blob is lost swarm-wide. Validated on the nuc1/nuc2/gnuc testbed.
   See [Node storage modes](#node-storage-modes--cache-vs-durable-store-of-record).

## First implementable slice — CAS spine (built ✅)

Deliberately CAS-only, single-node, no swarm — the smallest honest step, now
merged to `main`:

1. ✅ `Digest` multihash newtype in `nessie-backend-core` (#81).
2. ✅ `CasBackend` trait (#84) + `nessie-cas-conformance` suite (#85).
3. ✅ `mem` `CasBackend` (`MemCas`, `HashMap<Digest, Vec<u8>>`) passing the suite (#85).
4. ✅ `AccessHandle::CasBlob { digest, providers }` variant (#86; providers empty
   single-node).

It proved the CAS spine against real code, exactly as `mem` proved the volume
trait. The remaining layers follow in dependency order below.

## Implementation status

A living tracker for the build (operator directive: build the whole roadmap,
merge each slice on green, in dependency order).

| Layer | Slice | Status |
|---|---|---|
| Design | this doc + machine-checked formal model | ✅ #80, #82, #83 |
| CAS spine | `Digest`, `CasBackend`, `MemCas`+conformance, `AccessHandle::CasBlob` | ✅ #81, #84, #85, #86 |
| AC CRDT | `Attestation`, grow-only set, k-of-n `Confirmed`, `ActionCacheBackend` + mem + conformance (signature verification behind a seam) | ✅ #88, #89 |
| Content routing | `ContentRouter` seam + in-process `MemRouter`/`MemSwarm` | ✅ #90 |
| Reachability | `Referenced` + `ReclaimableCas` seams; `ReferenceResolver` + `RootSource` + `reachable_closure` (a general Merkle `Tree` deferred to the REAPI slice) | ✅ #91, #92 |
| Storage modes | `CasStore` + durable reachability GC + cache replica-gated LRU eviction | ✅ #93, #94 |
| Formal PO-GC | Lean mark-sweep + TLA+ concurrency/eviction safety (+ boundary counterexamples) | ✅ #95 |
| Daemon wiring | `[cas]` config + `cas_node` + scheduled GC/evict maintenance in `nessie-store` (in-memory node; persistent backend + face are follow-ups) | ✅ #97 |
| **NATS router** | `async-nats` rendezvous provider records (a real `ContentRouter`) | 🔜 next |
| Kademlia router | `libp2p` DHT (a real `ContentRouter`) | ⏳ |
| REAPI face | gRPC CAS + ActionCache subset (`tonic`); SHA-256 at the boundary; a Merkle `Tree` type | ⏳ (long-term) |

The single-node substrate is complete, machine-checked, **and runnable**: the CAS
spine, the ActionCache attestation-CRDT (the ungameable-completion keystone), the
content-router seam, both storage modes (durable reachability GC + cache replica-gated
eviction) with the paired safety property proved in `formal/`, and the daemon that runs
a configured CAS node with scheduled maintenance. What remains is *distribution and a
protocol face*: the two real network routers (so the swarm finds peers off-box), and the
REAPI face (the long-term Bazel-customer unlock).

## Open questions (deferred, tracked)

- **`k` for the AC confirmation gate** — fixed, per-instance-configurable, or
  policy-scoped via agent-mesh caveats? Starts at a config knob.
- **Attestation garbage collection** — grow-only sets grow; when is an AC entry's
  attestation set compacted, and under whose authority?
- **REAPI dual-digest indexing** — index blobs under both multihash and SHA-256, or
  compute the SHA-256 view lazily on REAPI reads? Settle when the face is built.
- **NFS read-through vs CAS** — the 2026-05-17 read-through surface is a third face;
  its manifest (path → digest) ownership (kyln vs embedded) is still open there.
- **Blob chunking** — fixed-size vs content-defined (Rabin) chunking for large-blob
  dedup and partial fetch. BitTorrent-style fixed pieces are the simpler v0.
- **Eviction victim selection at scale** — LRU is the default policy, but the
  replica-count check it depends on costs a `ContentRouter` round-trip per
  candidate; batch the check, cache provider counts, or gossip a coarse replica
  estimate? Settle when the `CasStore` slice is built.
- **Durable-node sufficiency** — how does a swarm *know* it has enough durable
  holders for its replication factor before cache nodes start evicting? A health
  signal (monty-tui) vs a hard admission gate on the first eviction.
