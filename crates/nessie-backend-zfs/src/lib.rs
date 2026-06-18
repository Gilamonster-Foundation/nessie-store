//! ZFS-backed storage backend for `nessie-store`.
//!
//! Implements the `VolumeBackend ⊂ SnapshotBackend ⊂ CloneBackend` stack over
//! `zfs`/`zpool`/`exportfs`, dispatched through a [`CommandRunner`] seam so the
//! exact command lines are unit-testable (and a real [`SystemRunner`] runs them).
//! Volumes are datasets, snapshots are `zfs snapshot`, FlexClones are `zfs clone`,
//! and the data plane is an NFS export (the access handle).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "python")]
mod python;
mod runner;
mod zfs;

pub use runner::{CommandOutput, CommandRunner, SystemRunner};
pub use zfs::{ZfsBackend, ZfsConfig};
