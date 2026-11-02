//! The swarm's live roots — what keeps a blob reachable.
//!
//! Reachability is computed from a [`RootSet`] supplied by a [`RootSource`], which
//! is the *one* place the storage engine couples to the AC tier and the pin
//! registry: it resolves confirmed action-cache entries, explicit pins, identity,
//! and live workspace roots into two plain digest sets. The walker and the eviction
//! engine then consume only those sets and never touch `ActionCacheBackend`, so both
//! stay AC-agnostic and unit-testable with a hand-built `RootSet`.

use nessie_backend_core::{BackendError, Digest};
use std::collections::BTreeSet;
use std::sync::Mutex;

/// A resolved root snapshot for one GC / eviction pass. `pinned ⊆ seed`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootSet {
    /// Seed digests for the durable reachable closure — confirmed-entry result
    /// bodies + pins + identity + live roots. `reachable_closure` expands them.
    pub seed: BTreeSet<Digest>,
    /// The never-evict pinned class (⊆ `seed`): the registered root digests
    /// themselves (confirmed AC entries + identity + live roots), **not** their
    /// output leaves. Cache eviction protects exactly these; other reachable blobs
    /// (output leaves) may float to the swarm and be re-fetched by digest.
    pub pinned: BTreeSet<Digest>,
}

/// Supplies the swarm's live roots as a resolved [`RootSet`] snapshot.
pub trait RootSource: Send + Sync {
    /// A point-in-time snapshot of the live roots.
    ///
    /// # Errors
    ///
    /// [`BackendError`] if the roots cannot be resolved (e.g. an AC query fails).
    fn roots(&self) -> Result<RootSet, BackendError>;
}

/// The node's local ref set — git's `refs/` for the swarm. Confirmed AC entries are
/// registered as roots when the node persists them; pins / identity / live workspace
/// roots are explicit registrations. Every registered root is pinned (and seeds the
/// durable closure). This is the first-slice [`RootSource`]: it needs no
/// `list_actions()` enumeration added to the AC trait.
///
/// A node that persists its pins also records here the root `Tree` blob its ref
/// names ([`replace`](Self::replace)); that blob is pinned too, so eviction never
/// removes what the ref points at. It is not one of the [`pins`](Self::pins).
#[derive(Default)]
pub struct RootRegistry {
    inner: Mutex<Refs>,
}

/// The pins and the root tree blob recording them, swapped together.
#[derive(Default)]
struct Refs {
    pins: BTreeSet<Digest>,
    tree: Option<Digest>,
}

impl RootRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `root` as a live root (git ref-update). Idempotent.
    pub fn register(&self, root: Digest) {
        self.refs().pins.insert(root);
    }

    /// Remove `root` (git ref-delete). Idempotent.
    pub fn unregister(&self, root: &Digest) {
        self.refs().pins.remove(root);
    }

    /// The registered pins, without the persisted root tree.
    #[must_use]
    pub fn pins(&self) -> BTreeSet<Digest> {
        self.refs().pins.clone()
    }

    /// Swap in a whole pin set and the root tree blob that now records it.
    pub fn replace(&self, pins: BTreeSet<Digest>, tree: Option<Digest>) {
        *self.refs() = Refs { pins, tree };
    }

    fn refs(&self) -> std::sync::MutexGuard<'_, Refs> {
        self.inner.lock().expect("root registry mutex poisoned")
    }
}

impl RootSource for RootRegistry {
    fn roots(&self) -> Result<RootSet, BackendError> {
        let set: BTreeSet<Digest> = {
            let refs = self.refs();
            refs.pins.iter().chain(&refs.tree).cloned().collect()
        };
        Ok(RootSet {
            seed: set.clone(),
            pinned: set,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roots_are_both_seed_and_pinned() {
        let reg = RootRegistry::new();
        let a = Digest::compute(b"root-a");
        let b = Digest::compute(b"root-b");
        reg.register(a.clone());
        reg.register(b.clone());
        let roots = reg.roots().unwrap();
        assert_eq!(roots.seed, roots.pinned);
        assert!(roots.seed.contains(&a) && roots.seed.contains(&b));
    }

    #[test]
    fn register_is_idempotent_and_unregister_removes() {
        let reg = RootRegistry::new();
        let a = Digest::compute(b"root");
        reg.register(a.clone());
        reg.register(a.clone());
        assert_eq!(reg.roots().unwrap().seed.len(), 1);
        reg.unregister(&a);
        assert!(reg.roots().unwrap().seed.is_empty());
        reg.unregister(&a); // idempotent
    }

    #[test]
    fn replaced_tree_is_pinned_but_is_not_a_pin() {
        let reg = RootRegistry::new();
        let pin = Digest::compute(b"pin");
        let tree = Digest::compute(b"tree");
        reg.replace(BTreeSet::from([pin.clone()]), Some(tree.clone()));
        assert_eq!(reg.pins(), BTreeSet::from([pin.clone()]));
        assert_eq!(reg.roots().unwrap().pinned, BTreeSet::from([pin, tree]));
    }
}
