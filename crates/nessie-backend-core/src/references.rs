//! The reachability out-edge seam for content-addressed garbage collection.
//!
//! nessie's CAS is *opaque* bytes keyed by [`Digest`](crate::Digest) — unlike a git
//! object there is no self-describing header, so a blob's children cannot be
//! sniffed from its bytes. Structure is therefore known only where a *structured*
//! value asserts its out-edges, via this trait. Opaque leaf blobs deliberately do
//! **not** implement it (their edge set is empty by absence).
//!
//! This is the pure, per-type half of the reachability seam ([`Referenced`] on core
//! domain types); the engine crate pairs it with a `ReferenceResolver` that decodes
//! opaque local bytes back into the typed value.

use crate::digest::Digest;

/// A structured CAS blob that enumerates its Merkle-DAG out-edges.
///
/// The contract is a **superset**: [`references`](Referenced::references) may return
/// *extra* digests but must never *omit* a real one. Over-approximation is safe for
/// GC (it retains extra blobs); under-approximation is not (it could sweep a
/// reachable blob). This one-directional safety is the invariant the whole
/// garbage collector rests on.
pub trait Referenced {
    /// A superset of the digests this object references.
    fn references(&self) -> Vec<Digest>;
}
