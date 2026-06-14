//! What a backend can do, declared honestly.
//!
//! A backend reports its [`Capabilities`] so the conformance harness knows which
//! suites to run and the REST router knows which operations to allow. Be honest:
//! a bare-NFS backend that cannot take snapshots advertises `snapshots = false`,
//! and the router returns the documented ONTAP "feature not supported" response
//! rather than emulating a snapshot. Telling the truth about substrate semantics
//! is load-bearing for everything downstream (SnapMirror in particular assumes
//! snapshot atomicity).

use serde::{Deserialize, Serialize};

/// The capability tiers a backend advertises.
///
/// The tiers are cumulative in the same order as the supertrait stack: a backend
/// that advertises `clones` must also honor `snapshots`, and any backend honors
/// the base volume tier. [`Capabilities::is_consistent`] checks this invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Native snapshots (`SnapshotBackend`).
    pub snapshots: bool,
    /// Native FlexClone-style clones (`CloneBackend`); implies `snapshots`.
    pub clones: bool,
    /// SnapMirror-style replication (a later tier).
    pub replication: bool,
}

impl Capabilities {
    /// A volume-only backend (no snapshots, clones, or replication).
    #[must_use]
    pub const fn volume_only() -> Self {
        Self {
            snapshots: false,
            clones: false,
            replication: false,
        }
    }

    /// A backend that supports snapshots but not clones.
    #[must_use]
    pub const fn snapshots() -> Self {
        Self {
            snapshots: true,
            clones: false,
            replication: false,
        }
    }

    /// A backend that supports snapshots and clones (the common ZFS/S3 tier).
    #[must_use]
    pub const fn clones() -> Self {
        Self {
            snapshots: true,
            clones: true,
            replication: false,
        }
    }

    /// Every capability (the full ONTAP-faithful tier: clones + replication).
    #[must_use]
    pub const fn all() -> Self {
        Self {
            snapshots: true,
            clones: true,
            replication: true,
        }
    }

    /// True if the advertised tiers respect the cumulative ordering
    /// (`clones` ⇒ `snapshots`, `replication` ⇒ `snapshots`).
    #[must_use]
    pub const fn is_consistent(&self) -> bool {
        // The higher tiers (clones, replication) each require snapshots.
        if self.clones || self.replication {
            self.snapshots
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_consistent() {
        assert!(Capabilities::volume_only().is_consistent());
        assert!(Capabilities::snapshots().is_consistent());
        assert!(Capabilities::clones().is_consistent());
        assert!(Capabilities::all().is_consistent());
    }

    #[test]
    fn clones_without_snapshots_is_inconsistent() {
        let bogus = Capabilities {
            snapshots: false,
            clones: true,
            replication: false,
        };
        assert!(!bogus.is_consistent());
    }

    #[test]
    fn capabilities_serde_roundtrip() {
        let c = Capabilities::clones();
        let json = serde_json::to_string(&c).expect("serialize");
        let back: Capabilities = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back);
    }
}
