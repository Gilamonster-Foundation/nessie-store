//! Core domain types and the storage-backend supertrait stack for `nessie-store`.
//!
//! `nessie-store` speaks the NetApp ONTAP REST API over a pluggable storage
//! substrate. This crate is the contract every substrate implements and every
//! higher layer (the REST protocol crate, the daemon, the conformance harness)
//! depends on. It contains **types and traits only — no implementations** — so it
//! stays free of any substrate dependency (and free of PyO3) forever.
//!
//! # The supertrait stack
//!
//! Capability tiers are expressed as a stack of supertraits rather than runtime
//! "not supported" errors, so the type system records what a backend can do:
//!
//! ```text
//! VolumeBackend  ⊂  SnapshotBackend  ⊂  CloneBackend
//! ```
//!
//! A backend implements only the tier it can honor and advertises that via
//! [`Capabilities`]. The [`VolumeBackend::as_snapshot`] / [`SnapshotBackend::as_clone`]
//! accessors return `None` by default; a backend that can honor a higher tier
//! overrides them to return `Some(self)`. The REST router downcasts at dispatch
//! and returns the documented ONTAP "feature not supported" response when the
//! substrate lacks the capability — no silent emulation.
//!
//! The [`ReplicationBackend`] tier (SnapMirror-style cross-instance streaming)
//! branches from [`SnapshotBackend`], reached via [`SnapshotBackend::as_replication`];
//! a later export tier is still planned. The [`AccessHandle`] enum already carries
//! the data-plane contract.
//!
//! # Data-plane discipline
//!
//! The daemon never brokers bytes. A backend hands out an [`AccessHandle`]
//! (an NFS export for ZFS, a presigned URL for S3, …) and the client reads/writes
//! directly against the substrate.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod access;
mod action_cache;
mod action_result;
mod attestation;
mod attestation_set;
mod capabilities;
mod cas;
mod digest;
mod error;
mod hex;
mod ids;
mod traits;
mod types;

#[cfg(feature = "python")]
mod python;

pub use access::AccessHandle;
pub use action_cache::ActionCacheBackend;
pub use action_result::{ActionResult, OutputFile};
pub use attestation::{
    Attestation, HexIdError, Signature, SignatureVerifier, SignedAttestation, SignerId,
    statement_signing_bytes,
};
pub use attestation_set::{AcResolution, AttestationSet};
pub use capabilities::Capabilities;
pub use cas::CasBackend;
pub use digest::{Digest, DigestAlgo, DigestParseError};
pub use error::BackendError;
pub use ids::{SnapshotUuid, VolumeUuid};
pub use traits::{CloneBackend, ReplicationBackend, SnapshotBackend, VolumeBackend};
pub use types::{
    CloneOrigin, Snapshot, Volume, VolumePatch, VolumeSpec, VolumeState, VolumeStyle, VolumeType,
};
