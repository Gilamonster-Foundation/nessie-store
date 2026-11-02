//! Retention configuration — the `[cas]` block, as a typed value.
//!
//! A node runs its local store in exactly one [`StorageMode`]: **lru** (a bounded
//! cache that evicts coldest-first) or **write-only** (retain everything, refuse
//! writes at capacity). Three-Cs: Configuration picks the mode here; Composition
//! picks the eviction behaviour.

use std::time::Duration;

/// How a node retains its local blobs.
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// The storage mode this node runs in.
    pub mode: StorageMode,
}

impl RetentionPolicy {
    /// A bounded LRU cache: evict coldest-first down to `byte_budget`, blobs with
    /// another known holder before sole copies. Pinned roots and in-flight writes
    /// are never evicted; writes younger than `gc_grace` are spared.
    #[must_use]
    pub fn lru(byte_budget: u64, gc_grace: Duration) -> Self {
        Self {
            mode: StorageMode::Lru(LruPolicy {
                byte_budget,
                gc_grace,
            }),
        }
    }

    /// A write-only store: never reclaims; a write that would exceed `capacity`
    /// bytes is refused before anything is committed.
    #[must_use]
    pub fn write_only(capacity: u64) -> Self {
        Self {
            mode: StorageMode::WriteOnly(WriteOnlyPolicy { capacity }),
        }
    }
}

/// The two retention modes.
#[derive(Debug, Clone)]
pub enum StorageMode {
    /// Bounded cache — coldest-first eviction; sole copies may be dropped.
    Lru(LruPolicy),
    /// Retain-all — never reclaims, refuses writes past capacity.
    WriteOnly(WriteOnlyPolicy),
}

/// LRU-mode tuning.
#[derive(Debug, Clone)]
pub struct LruPolicy {
    /// Target local byte ceiling; eviction runs when the store exceeds it.
    pub byte_budget: u64,
    /// Blobs written more recently than this are never evicted — the cross-restart
    /// backstop for the put→reference race. The in-process write-guard is primary.
    pub gc_grace: Duration,
}

/// Write-only-mode tuning.
#[derive(Debug, Clone)]
pub struct WriteOnlyPolicy {
    /// Hard byte capacity; a write past it is refused, never made room for.
    pub capacity: u64,
}
