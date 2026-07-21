//! The reachability closure — the "mark" of mark-and-sweep.

use crate::resolver::ReferenceResolver;
use nessie_backend_core::{BackendError, CasBackend, Digest};
use std::collections::BTreeSet;
use std::io::Read;

/// Every digest reachable from `seed` by walking the CAS Merkle-DAG: the transitive
/// closure of the out-edges a [`ReferenceResolver`] decodes from each held blob.
/// This is the direct refinement of the least-fixpoint `Reach` in the formal model
/// (`formal/lean/Gc.lean`, PO-GC-1) — the set durable GC must preserve.
///
/// A seed or edge that is **not held locally** is marked reachable but not expanded
/// (git marks a missing wanted object without recursing), so the closure is exact
/// over what the node can see and never errors on a legitimately-absent blob.
/// Cycles and shared children are handled by the visited set.
///
/// # Errors
///
/// [`BackendError`] if reading a held blob fails for any reason other than absence.
pub fn reachable_closure(
    seed: &BTreeSet<Digest>,
    cas: &dyn CasBackend,
    resolver: &dyn ReferenceResolver,
) -> Result<BTreeSet<Digest>, BackendError> {
    let mut visited: BTreeSet<Digest> = BTreeSet::new();
    let mut stack: Vec<Digest> = seed.iter().cloned().collect();

    while let Some(digest) = stack.pop() {
        if !visited.insert(digest.clone()) {
            continue; // already expanded
        }
        match cas.get(&digest) {
            Ok(mut reader) => {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).map_err(|e| {
                    BackendError::Internal(format!("reachability read of {digest}: {e}"))
                })?;
                for child in resolver.references_of(&digest, &bytes) {
                    if !visited.contains(&child) {
                        stack.push(child);
                    }
                }
            }
            // Reachable but not held here: mark it, do not expand.
            Err(BackendError::BlobNotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(visited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{CanonicalResolver, LeafResolver};
    use nessie_backend_core::{ActionResult, OutputFile};
    use nessie_backend_mem::MemCas;
    use std::collections::BTreeMap;
    use std::io::Cursor;

    fn store(cas: &MemCas, bytes: &[u8]) -> Digest {
        cas.put(&mut Cursor::new(bytes.to_vec())).unwrap()
    }

    fn action_result_over(leaf_a: &Digest, leaf_b: &Digest) -> ActionResult {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            OutputFile {
                digest: leaf_a.clone(),
                is_executable: false,
            },
        );
        ActionResult {
            outputs,
            exit_code: 0,
            stdout_digest: Some(leaf_b.clone()),
            stderr_digest: None,
        }
    }

    #[test]
    fn closure_includes_body_and_its_leaves_but_not_an_orphan() {
        let cas = MemCas::new();
        let leaf_a = store(&cas, b"leaf-a");
        let leaf_b = store(&cas, b"leaf-b");
        let result = action_result_over(&leaf_a, &leaf_b);
        let body = store(&cas, &result.to_canonical_bytes());
        let orphan = store(&cas, b"orphan-garbage");

        let seed: BTreeSet<Digest> = [body.clone()].into_iter().collect();
        let reach = reachable_closure(&seed, &cas, &CanonicalResolver).unwrap();

        assert!(reach.contains(&body), "the seed body is reachable");
        assert!(
            reach.contains(&leaf_a) && reach.contains(&leaf_b),
            "its leaves are reachable"
        );
        assert!(
            !reach.contains(&orphan),
            "an unreferenced blob is NOT reachable"
        );
        assert_eq!(reach.len(), 3);
    }

    #[test]
    fn a_missing_seed_is_marked_not_expanded() {
        let cas = MemCas::new();
        let absent = Digest::compute(b"never-stored");
        let seed: BTreeSet<Digest> = [absent.clone()].into_iter().collect();
        let reach = reachable_closure(&seed, &cas, &CanonicalResolver).unwrap();
        assert_eq!(
            reach, seed,
            "absent seed is reachable but yields no children"
        );
    }

    #[test]
    fn leaf_resolver_bounds_reachability_to_the_seed() {
        let cas = MemCas::new();
        let leaf_a = store(&cas, b"leaf-a");
        let leaf_b = store(&cas, b"leaf-b");
        let body = store(
            &cas,
            &action_result_over(&leaf_a, &leaf_b).to_canonical_bytes(),
        );
        let seed: BTreeSet<Digest> = [body.clone()].into_iter().collect();
        // With LeafResolver the body has no edges, so only the seed is reachable.
        let reach = reachable_closure(&seed, &cas, &LeafResolver).unwrap();
        assert_eq!(reach, seed);
    }

    #[test]
    fn dedup_and_shared_children_are_safe() {
        // Two result bodies sharing a leaf; both seeded — the leaf is visited once.
        let cas = MemCas::new();
        let shared = store(&cas, b"shared-leaf");
        let other = store(&cas, b"other-leaf");
        let body1 = store(
            &cas,
            &action_result_over(&shared, &other).to_canonical_bytes(),
        );
        let body2 = store(
            &cas,
            &action_result_over(&shared, &shared).to_canonical_bytes(),
        );
        let seed: BTreeSet<Digest> = [body1.clone(), body2.clone()].into_iter().collect();
        let reach = reachable_closure(&seed, &cas, &CanonicalResolver).unwrap();
        assert!(reach.contains(&shared) && reach.contains(&other));
        assert!(reach.contains(&body1) && reach.contains(&body2));
    }
}
