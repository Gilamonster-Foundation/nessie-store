//! Retention engine for nessie-store: durable-mode reachability GC and cache-mode
//! replica-gated LRU eviction over any [`CasBackend`](nessie_backend_core::CasBackend).
//!
//! This crate holds the storage-modes *engine* — loosely coupled from the daemon and
//! the protocol faces, cohesive around retention. It consumes the seams that live in
//! `nessie-backend-core` ([`Referenced`](nessie_backend_core::Referenced),
//! [`ReclaimableCas`](nessie_backend_core::ReclaimableCas),
//! [`ContentRouter`](nessie_backend_core::ContentRouter)) and adds the ones intrinsic
//! to retention: the reachability [`ReferenceResolver`], the [`RootSource`], and the
//! reachability closure. The durable GC and cache eviction algorithms land on top in
//! following slices, each machine-checked against the PO-GC obligations in `formal/`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod clock;
mod evict;
mod gc;
mod policy;
mod reach;
mod resolver;
mod roots;
mod store;

pub use clock::{Clock, MockClock, SystemClock};
pub use evict::{DurabilityOracle, EvictReport, NoDurableOracle, RefuseToReplicate, Replicator};
pub use gc::GcReport;
pub use policy::{CachePolicy, DurablePolicy, EvictionPolicy, RetentionPolicy, StorageMode};
pub use reach::reachable_closure;
pub use resolver::{CanonicalResolver, LeafResolver, ReferenceResolver};
pub use roots::{RootRegistry, RootSet, RootSource};
pub use store::CasStore;
