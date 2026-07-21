/-!
# nessie-store — mark-and-sweep garbage collection (PO-GC-1, algebraic core)

Machine-checked companion to `../../crates/nessie-cas-store/src/gc.rs`. The Rust
durable GC computes `reachable_closure(roots)` (MARK) and reclaims exactly the
local blobs outside it (SWEEP). This development proves the algebraic heart of
**PO-GC-1**: the sweep never reclaims a reachable blob, so reachability is
preserved.

Reachability is the least set containing the roots and closed under the
out-edges (`Reach`), mirroring the BFS in `reach.rs`. `swept b := held b ∧ ¬Reach b`
mirrors `swept = local \ live`, and `kept b := held b ∧ ¬swept b`. The theorems
below are the direct refinement targets for the `gc()` sweep loop.

Lean core only — no Mathlib — so `lake build` stays fast and offline, exactly as
`NessieFormal.lean`.

The put→reference *race* (the operational half, PO-GC-1-op) is the concurrency
concern proved separately in `../tla/Gc.tla`; here the store is a fixed snapshot.
-/

namespace NessieGc

universe u

variable {Blob : Type u}

/-- The reachable set: the least predicate containing the roots and closed under the
out-edge relation `edges`. Refines the least-fixpoint `Reach` the BFS computes. -/
inductive Reach (edges : Blob → Blob → Prop) (roots : Blob → Prop) : Blob → Prop
  /-- Every root is reachable. -/
  | root {b : Blob} : roots b → Reach edges roots b
  /-- An out-edge of a reachable blob is reachable. -/
  | step {a b : Blob} : Reach edges roots a → edges a b → Reach edges roots b

variable {edges : Blob → Blob → Prop} {roots held : Blob → Prop}

/-- What the sweep reclaims: a **held** blob that is **not reachable** — exactly the
`local \ live` set of `gc()`. -/
def swept (edges : Blob → Blob → Prop) (roots held : Blob → Prop) (b : Blob) : Prop :=
  held b ∧ ¬ Reach edges roots b

/-- What survives a GC pass: held and not swept. -/
def kept (edges : Blob → Blob → Prop) (roots held : Blob → Prop) (b : Blob) : Prop :=
  held b ∧ ¬ swept edges roots held b

/-- **PO-GC-1 (disjointness).** The swept set is disjoint from the reachable set:
nothing reachable is ever reclaimed. -/
theorem swept_disjoint_reachable {b : Blob}
    (hs : swept edges roots held b) (hr : Reach edges roots b) : False :=
  hs.2 hr

/-- **PO-GC-1 (preservation).** A reachable, held blob survives the sweep. This is
the safety the durable `gc()` loop must honor: `if live.contains(&digest) { continue }`. -/
theorem reachable_survives {b : Blob}
    (hheld : held b) (hr : Reach edges roots b) : kept edges roots held b :=
  ⟨hheld, fun hs => hs.2 hr⟩

/-- Every root is kept (roots are reachable by `Reach.root`), so registering a blob
as a root — what the store does for a confirmed AC entry — guarantees it survives GC. -/
theorem root_survives {b : Blob} (hheld : held b) (hroot : roots b) :
    kept edges roots held b :=
  reachable_survives hheld (Reach.root hroot)

/-- Conversely, whatever the sweep *does* reclaim was genuinely unreachable — the GC
only ever removes garbage. -/
theorem swept_is_unreachable {b : Blob} (hs : swept edges roots held b) :
    ¬ Reach edges roots b :=
  hs.2

end NessieGc
