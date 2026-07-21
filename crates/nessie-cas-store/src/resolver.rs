//! Decoding an opaque local blob into its Merkle-DAG out-edges.
//!
//! The reachability walk needs a blob's children, but the CAS is opaque bytes with
//! no type header ([`Referenced`](nessie_backend_core::Referenced) is only
//! available where a *typed value* is in hand). A [`ReferenceResolver`] bridges the
//! gap: it decodes raw bytes back into a known structured type and returns that
//! type's references. This is the one place that holds the catalog of structured
//! CAS types, so the walk stays type-agnostic — a future REAPI `Tree` decoder is
//! added *here* and nothing in the walker, GC, or eviction changes.

use nessie_backend_core::{ActionResult, Digest, Referenced};

/// Turns an opaque local blob into its DAG out-edges.
///
/// The result MUST be a **superset** of the true edges (never omit a real one) for
/// GC safety — an over-approximation only retains extra blobs, an under-approximation
/// could sweep a reachable one.
pub trait ReferenceResolver: Send + Sync {
    /// The out-edges of the blob `digest` whose bytes are `bytes`, or an empty list
    /// for a leaf or an unrecognized type.
    fn references_of(&self, digest: &Digest, bytes: &[u8]) -> Vec<Digest>;
}

/// Resolves references by trying each known canonical type in turn. Today the
/// catalog is a single type: a blob that decodes as a canonical [`ActionResult`]
/// yields that result's references; anything else is treated as an opaque leaf.
///
/// Adding a REAPI `Tree` is one more decode arm here.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonicalResolver;

impl ReferenceResolver for CanonicalResolver {
    fn references_of(&self, _digest: &Digest, bytes: &[u8]) -> Vec<Digest> {
        match ActionResult::from_canonical_bytes(bytes) {
            Ok(result) => result.references(),
            Err(_) => Vec::new(), // not a known structured type => leaf
        }
    }
}

/// Treats **every** blob as an opaque leaf (no out-edges). Useful for a pure blob
/// store, or to bound a GC pass to depth-1 reachability from explicit roots.
#[derive(Debug, Clone, Copy, Default)]
pub struct LeafResolver;

impl ReferenceResolver for LeafResolver {
    fn references_of(&self, _digest: &Digest, _bytes: &[u8]) -> Vec<Digest> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::OutputFile;
    use std::collections::BTreeMap;

    fn action_result_bytes() -> (Vec<u8>, Vec<Digest>) {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            OutputFile {
                digest: Digest::compute(b"leaf-a"),
                is_executable: false,
            },
        );
        let r = ActionResult {
            outputs,
            exit_code: 0,
            stdout_digest: Some(Digest::compute(b"leaf-b")),
            stderr_digest: None,
        };
        (r.to_canonical_bytes(), r.references())
    }

    #[test]
    fn canonical_resolver_returns_action_result_references() {
        let (bytes, expected) = action_result_bytes();
        let got = CanonicalResolver.references_of(&Digest::compute(&bytes), &bytes);
        assert_eq!(got, expected);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn canonical_resolver_treats_a_raw_blob_as_a_leaf() {
        let bytes = b"just some opaque bytes".to_vec();
        assert!(
            CanonicalResolver
                .references_of(&Digest::compute(&bytes), &bytes)
                .is_empty()
        );
    }

    #[test]
    fn leaf_resolver_never_returns_edges() {
        let (bytes, _) = action_result_bytes();
        assert!(
            LeafResolver
                .references_of(&Digest::compute(&bytes), &bytes)
                .is_empty()
        );
    }
}
