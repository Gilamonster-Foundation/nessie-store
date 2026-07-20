//! The grow-only ActionCache CRDT and its k-of-n resolution.
//!
//! [`AttestationSet`] is the CvRDT state for **one** action — a join-semilattice
//! whose merge is set union. It is a pure value type (no `Mutex`, no `dyn`, no
//! I/O) so it is the Aeneas/Charon refinement target and mirrors the formal model
//! directly:
//!
//! - merge = union → commutative / associative / idempotent
//!   (`formal/lean/NessieFormal.lean` `union_comm`/`union_assoc`/`union_idem`,
//!   PO-AC-4);
//! - there is deliberately **no** `remove`/`clear`/`retain`, so "the store only
//!   grows" (TLA+ `MonotoneStore`, Lean `subset_union_left`) holds by construction;
//! - `is_confirmed` counts **distinct** signers ≥ k (TLA+
//!   `Confirmed == Cardinality(SignersFor) >= K`, Lean `Confirmed K`).

use crate::attestation::{Attestation, SignerId};
use crate::digest::Digest;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;

/// The grow-only CRDT state for one action: the set of `{signer, result}`
/// attestations observed so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttestationSet {
    atts: BTreeSet<Attestation>,
}

impl AttestationSet {
    /// An empty set (the formal `Init` state for one action).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Grow by a single attestation (the TLA+ `Attest` inner `@ ∪ {a}` — the merge
    /// of a singleton). Returns whether the element was newly added.
    pub fn insert(&mut self, att: Attestation) -> bool {
        self.atts.insert(att)
    }

    /// The CRDT merge = the semilattice join = set union. Commutative, associative,
    /// and idempotent by `BTreeSet` union — the refinement of Lean `PredSet.union`
    /// and TLA+ `Gossip` (PO-AC-4).
    pub fn merge(&mut self, other: &AttestationSet) {
        self.atts.extend(other.atts.iter().cloned());
    }

    /// `self ⊆ other` — the monotonicity backbone (Lean `subset_union_left`).
    #[must_use]
    pub fn is_subset(&self, other: &AttestationSet) -> bool {
        self.atts.is_subset(&other.atts)
    }

    /// The **distinct** signers backing `result` (TLA+ `SignersFor(n, r)`; the image
    /// of Lean's confirming injection). A `BTreeSet`, so one signer attesting the
    /// same result twice counts once.
    #[must_use]
    pub fn signers_for(&self, result: &Digest) -> BTreeSet<&SignerId> {
        self.atts
            .iter()
            .filter(|a| &a.result == result)
            .map(|a| &a.signer)
            .collect()
    }

    /// `result` is confirmed iff ≥ k **distinct** signers back it (TLA+
    /// `Confirmed(n, r)`, Lean `Confirmed K`). The comparison is `>= k` (strict
    /// threshold) — the exact security boundary.
    #[must_use]
    pub fn is_confirmed(&self, result: &Digest, k: NonZeroUsize) -> bool {
        self.signers_for(result).len() >= k.get()
    }

    /// Resolve this action's entry at threshold `k`. Under the Byzantine-minority
    /// hypothesis (`|Byzantine| < k`) at most one result is confirmed (PO-AC-2
    /// Agreement); [`AcResolution::Conflicting`] is reachable only when that
    /// hypothesis is violated (PO-AC-B) or the action is non-deterministic.
    #[must_use]
    pub fn resolve(&self, k: NonZeroUsize) -> AcResolution {
        let confirmed: BTreeSet<Digest> = self
            .atts
            .iter()
            .map(|a| &a.result)
            .filter(|r| self.is_confirmed(r, k))
            .cloned()
            .collect();
        match confirmed.len() {
            0 => AcResolution::Unconfirmed,
            1 => AcResolution::Confirmed(confirmed.into_iter().next().expect("len == 1")),
            _ => AcResolution::Conflicting(confirmed),
        }
    }

    /// Iterate the attestations (insertion-order-independent; `BTreeSet` order).
    pub fn iter(&self) -> impl Iterator<Item = &Attestation> {
        self.atts.iter()
    }

    /// The number of distinct attestations held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.atts.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.atts.is_empty()
    }
}

/// The outcome of resolving an action's attestation set at threshold k.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcResolution {
    /// No result reached k distinct signers — a cache miss (at worst a redundant
    /// re-execution, never a wrong result).
    Unconfirmed,
    /// Exactly one result reached k distinct signers. Unique under a Byzantine
    /// minority (PO-AC-2).
    Confirmed(Digest),
    /// Two or more distinct results each reached k — only when the minority
    /// hypothesis is violated (PO-AC-B) or the action is non-deterministic.
    /// Surfaced, never silently resolved to one side.
    Conflicting(BTreeSet<Digest>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn k(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("k > 0")
    }

    fn att(signer: &[u8], result: &[u8]) -> Attestation {
        Attestation {
            signer: SignerId::from_bytes(signer.to_vec()),
            result: Digest::compute(result),
        }
    }

    #[test]
    fn distinct_signers_confirm_but_a_repeat_signer_does_not() {
        let r = Digest::compute(b"R");
        let mut s = AttestationSet::new();
        s.insert(att(b"s1", b"R"));
        s.insert(att(b"s1", b"R")); // same signer again — must not raise the count
        assert!(
            !s.is_confirmed(&r, k(2)),
            "one distinct signer is below k=2"
        );
        s.insert(att(b"s2", b"R"));
        assert!(s.is_confirmed(&r, k(2)), "two distinct signers reach k=2");
    }

    #[test]
    fn resolve_surfaces_a_conflict_rather_than_picking() {
        // Two results each reach k=2 (a Byzantine-cohort / non-determinism case).
        let mut s = AttestationSet::new();
        for who in [b"s1".as_slice(), b"s2".as_slice()] {
            s.insert(att(who, b"good"));
        }
        for who in [b"s3".as_slice(), b"s4".as_slice()] {
            s.insert(att(who, b"evil"));
        }
        match s.resolve(k(2)) {
            AcResolution::Conflicting(set) => assert_eq!(set.len(), 2),
            other => panic!("expected Conflicting, got {other:?}"),
        }
    }

    // --- Proptests named after the formal theorems: the anti-drift tripwire. ---

    prop_compose! {
        fn arb_att()(s in 0u8..5, r in 0u8..4) -> Attestation {
            att(&[s], &[r])
        }
    }

    fn arb_set() -> impl Strategy<Value = AttestationSet> {
        prop::collection::vec(arb_att(), 0..12).prop_map(|v| {
            let mut s = AttestationSet::new();
            for a in v {
                s.insert(a);
            }
            s
        })
    }

    proptest! {
        /// PO-AC-4: `union_comm` — merge is commutative.
        #[test]
        fn union_comm(a in arb_set(), b in arb_set()) {
            let mut ab = a.clone(); ab.merge(&b);
            let mut ba = b.clone(); ba.merge(&a);
            prop_assert_eq!(ab, ba);
        }

        /// PO-AC-4: `union_assoc` — merge is associative.
        #[test]
        fn union_assoc(a in arb_set(), b in arb_set(), c in arb_set()) {
            let mut left = a.clone(); left.merge(&b); left.merge(&c);
            let mut bc = b.clone(); bc.merge(&c);
            let mut right = a.clone(); right.merge(&bc);
            prop_assert_eq!(left, right);
        }

        /// PO-AC-4: `union_idem` / `merge_absorb_redelivery` — re-merging is a no-op.
        #[test]
        fn union_idem(a in arb_set(), b in arb_set()) {
            let mut once = a.clone(); once.merge(&b);
            let mut twice = once.clone(); twice.merge(&b);
            prop_assert_eq!(once, twice);
        }

        /// `subset_union_left` — a replica only grows under merge.
        #[test]
        fn merge_grows_only(a in arb_set(), b in arb_set()) {
            let mut merged = a.clone(); merged.merge(&b);
            prop_assert!(a.is_subset(&merged));
            prop_assert!(b.is_subset(&merged));
        }

        /// PO-AC-3 `confirmed_monotone`: once confirmed, adding attestations keeps
        /// the SAME result confirmed (no un-confirmation, no switch).
        #[test]
        fn confirmed_is_monotone(base in arb_set(), extra in arb_set(), rr in 0u8..4, kk in 1usize..4) {
            let r = Digest::compute(&[rr]);
            let k = k(kk);
            prop_assume!(base.is_confirmed(&r, k));
            let mut grown = base.clone();
            grown.merge(&extra);
            prop_assert!(grown.is_confirmed(&r, k), "confirmation must never be revoked");
        }
    }
}
