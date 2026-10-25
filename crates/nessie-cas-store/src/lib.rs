//! Retention engine for nessie-store: a [`CasStore`] that gives any
//! [`CasBackend`](nessie_backend_core::CasBackend) one of two retention modes.
//!
//! - **`lru`** — a bounded cache. Over `byte_budget`, [`CasStore::evict_to_budget`]
//!   drops the coldest blobs, those with another known holder first and sole copies
//!   after. Pinned roots ([`RootRegistry`]), in-flight writes and writes inside
//!   `gc_grace` are never evicted.
//! - **`write_only`** — retain everything. Nothing is ever reclaimed; `byte_budget`
//!   is a hard capacity, and a write past it fails with
//!   [`CapacityExceeded`](nessie_backend_core::BackendError::CapacityExceeded) before
//!   anything is committed.
//!
//! The crate is loosely coupled from the daemon and the protocol faces. It consumes
//! the seams in `nessie-backend-core` ([`ReclaimableCas`](nessie_backend_core::ReclaimableCas),
//! [`ContentRouter`](nessie_backend_core::ContentRouter)) and adds the retention
//! ones: [`DurabilityOracle`], [`Replicator`], [`RootSource`] and the reachability
//! closure ([`reachable_closure`] with its [`ReferenceResolver`]).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod clock;
mod evict;
mod policy;
mod reach;
mod resolver;
mod roots;
mod store;

pub use clock::{Clock, MockClock, SystemClock};
pub use evict::{DurabilityOracle, EvictReport, NoDurableOracle, RefuseToReplicate, Replicator};
pub use policy::{LruPolicy, RetentionPolicy, StorageMode, WriteOnlyPolicy};
pub use reach::reachable_closure;
pub use resolver::{CanonicalResolver, LeafResolver, ReferenceResolver};
pub use roots::{RootRegistry, RootSet, RootSource};
pub use store::CasStore;
