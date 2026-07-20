/-!
# nessie-store — algebraic core of the AC attestation-CRDT

Machine-checked companion to `../tla/AcCrdt.tla`. The TLA+ model checks the
*distributed safety* of k-of-n confirmation (no-forgery under a Byzantine
minority, agreement, monotonic store). This Lean development proves the
*algebraic* facts that make the eventual-consistency story sound:

* **PO-AC-4** — the CRDT merge is a **join-semilattice** (commutative,
  associative, idempotent). This is the CvRDT convergence theorem: replicas that
  have observed the same updates converge regardless of gossip order, and
  duplicate delivery is harmless.
* **PO-AC-3 (algebraic half)** — **confirmation is monotone** under the merge
  order: once ≥ K distinct signers agree on a result, no later merge can revoke
  that confirmation. Paired with "the store only grows" (`subset_union_left`),
  this is the "confirmation is never un-said" safety the TLA+ model exhibits
  operationally.

Everything below uses **only Lean core** — no Mathlib, no Batteries — so it
builds offline and cheaply. `PredSet` and `Inj` are defined locally for the same
reason.

The identity/membership of the signer set (who may sign, and the honest/Byzantine
split) is *assumed*, supplied by the swarm-join / signed-peer primitive
(agent-mesh); it is a hypothesis of these proofs, not a subject of them.
-/

namespace NessieFormal

universe u

/-- A set over `α`, encoded as its membership predicate. -/
def PredSet (α : Type u) : Type u := α → Prop

namespace PredSet

variable {α : Type u}

/-- `s ⊆ t`. -/
def Subset (s t : PredSet α) : Prop := ∀ a, s a → t a

/-- The CRDT merge: pointwise union. This is the join of the state semilattice. -/
def union (s t : PredSet α) : PredSet α := fun a => s a ∨ t a

/-- Merge is **commutative** — gossip order does not matter. -/
theorem union_comm (s t : PredSet α) : union s t = union t s := by
  funext a
  apply propext
  constructor
  · intro h; exact Or.symm h
  · intro h; exact Or.symm h

/-- Merge is **associative**. -/
theorem union_assoc (s t u : PredSet α) :
    union (union s t) u = union s (union t u) := by
  funext a
  apply propext
  constructor
  · intro h
    cases h with
    | inl h1 =>
        cases h1 with
        | inl h => exact Or.inl h
        | inr h => exact Or.inr (Or.inl h)
    | inr h => exact Or.inr (Or.inr h)
  · intro h
    cases h with
    | inl h => exact Or.inl (Or.inl h)
    | inr h1 =>
        cases h1 with
        | inl h => exact Or.inl (Or.inr h)
        | inr h => exact Or.inr h

/-- Merge is **idempotent** — re-merging a replica with itself is a no-op. -/
theorem union_idem (s : PredSet α) : union s s = s := by
  funext a
  apply propext
  constructor
  · intro h
    cases h with
    | inl h => exact h
    | inr h => exact h
  · intro h; exact Or.inl h

/-- A replica only grows under merge: `s ⊆ s ∪ t`. -/
theorem subset_union_left (s t : PredSet α) : Subset s (union s t) :=
  fun _ h => Or.inl h

/-- **Convergence / confluence.** Merging in either order yields the same state.
(A restatement of commutativity, named for what it means operationally.) -/
theorem merge_confluent (s t : PredSet α) : union s t = union t s :=
  union_comm s t

/-- **Duplicate delivery is harmless.** Re-merging `t` into `s ∪ t` changes
nothing — the CvRDT tolerance of at-least-once gossip. -/
theorem merge_absorb_redelivery (s t : PredSet α) :
    union (union s t) t = union s t := by
  rw [union_assoc, union_idem]

end PredSet

/-- Injectivity, defined locally to avoid any non-core dependency. Two
independent universes so `Fin K → Signer` (mixing `Type 0` and `Type u`) fits. -/
def Inj {A : Type u} {B : Type v} (f : A → B) : Prop := ∀ a b, f a = f b → a = b

/-- An attestation: a signer asserts a result for the (single, deterministic)
action. Distinct actions are independent instances, exactly as in the TLA+ model. -/
structure Att (Signer Result : Type u) where
  signer : Signer
  result : Result

/-- `r` is **confirmed** in state `s` iff at least `K` *distinct* signers attest
it — witnessed by an injection `Fin K → Signer` whose every image attests `r`. -/
def Confirmed {Signer Result : Type u} (K : Nat)
    (s : PredSet (Att Signer Result)) (r : Result) : Prop :=
  ∃ f : Fin K → Signer, Inj f ∧ ∀ i, s { signer := f i, result := r }

/-- **PO-AC-3 (algebraic).** Confirmation is monotone under the merge order:
if `s ⊆ t` and `r` is confirmed in `s`, it is confirmed in `t`. The same witness
carries over, so no merge can revoke a confirmation. -/
theorem confirmed_monotone {Signer Result : Type u} {K : Nat}
    {s t : PredSet (Att Signer Result)} {r : Result}
    (hsub : PredSet.Subset s t) :
    Confirmed K s r → Confirmed K t r := by
  intro h
  cases h with
  | intro f hf => exact ⟨f, hf.1, fun i => hsub _ (hf.2 i)⟩

/-- Corollary: a gossip merge (which only adds attestations) never un-confirms a
result already confirmed at a node. -/
theorem confirmed_stable_under_merge {Signer Result : Type u} {K : Nat}
    (s t : PredSet (Att Signer Result)) {r : Result} :
    Confirmed K s r → Confirmed K (PredSet.union s t) r :=
  confirmed_monotone (PredSet.subset_union_left s t)

end NessieFormal
