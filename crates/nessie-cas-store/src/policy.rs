//! Retention configuration — the `[cas]` block, as a typed value.
//!
//! A node runs its local store in exactly one [`StorageMode`]: **durable** (a store
//! of record — reachability GC only) or **cache** (a bounded LRU that lets cold
//! blobs float to the swarm). Three-Cs: Configuration picks the mode here;
//! Composition picks the eviction policy.

use std::num::NonZeroUsize;
use std::time::Duration;

/// How a node retains its local blobs.
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// The storage mode this node runs in.
    pub mode: StorageMode,
}

impl RetentionPolicy {
    /// A durable store-of-record: retain everything reachable, GC only unreachable
    /// garbage older than `gc_grace`.
    #[must_use]
    pub fn durable(gc_grace: Duration) -> Self {
        Self {
            mode: StorageMode::Durable(DurablePolicy { gc_grace }),
        }
    }

    /// A bounded LRU cache: evict cold, sufficiently-replicated, non-pinned blobs
    /// down to `byte_budget`, never dropping the last reachable copy (`R` other
    /// holders required).
    #[must_use]
    pub fn cache(byte_budget: u64, replication_factor: NonZeroUsize, gc_grace: Duration) -> Self {
        Self {
            mode: StorageMode::Cache(CachePolicy {
                byte_budget,
                replication_factor,
                eviction: EvictionPolicy::Lru,
                gc_grace,
            }),
        }
    }
}

/// The two retention modes.
#[derive(Debug, Clone)]
pub enum StorageMode {
    /// Store of record — git-style reachability GC, no eviction.
    Durable(DurablePolicy),
    /// Bounded cache — LRU eviction with a replica gate, plus GC of unreachable garbage.
    Cache(CachePolicy),
}

/// Durable-mode tuning.
#[derive(Debug, Clone)]
pub struct DurablePolicy {
    /// Blobs written more recently than this are never swept — the cross-restart
    /// backstop for the put→reference race (git `gc.pruneExpire`). The in-process
    /// write-guard is the primary, airtight defense; this covers restarts.
    pub gc_grace: Duration,
}

/// Cache-mode tuning.
#[derive(Debug, Clone)]
pub struct CachePolicy {
    /// Target local byte ceiling; eviction runs when the store exceeds it.
    pub byte_budget: u64,
    /// `R` — the number of **other** holders required before a reachable blob may be
    /// evicted. `NonZeroUsize` makes `R = 0` (evict-the-last-copy) unrepresentable,
    /// exactly as the ActionCache uses `NonZeroUsize` for `k`.
    pub replication_factor: NonZeroUsize,
    /// Which cold blob to evict first.
    pub eviction: EvictionPolicy,
    /// Same put-race backstop as durable mode.
    pub gc_grace: Duration,
}

/// The order in which cold blobs are chosen for eviction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Least-recently-used first (the default).
    #[default]
    Lru,
}
