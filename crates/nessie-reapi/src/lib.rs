//! REAPI (Bazel Remote Execution API v2) gRPC face over the nessie CAS /
//! ActionCache backends — the **cache subset** (CAS + ByteStream + ActionCache +
//! Capabilities; no remote Execution).
//!
//! The pitch: *a Bazel remote cache with no BuildBarn to stand up.* This crate holds
//! the tonic-generated wire types and (in following slices) the four service impls,
//! keyed under a **SHA-256-native** instance so a blob the client uploads by its
//! SHA-256 digest is stored under that digest with no index and no recompute (see
//! `docs/design/p2p-cas-swarm.md`, decision #4).
//!
//! This slice is the crate skeleton + codegen: the generated types compile. The
//! `Sha256Boundary`, the four services, and the daemon wiring land in later slices.

#![forbid(unsafe_code)]
// Every handler returns `Result<_, tonic::Status>`, and `Status` is a large type —
// that is the required gRPC error, so boxing it would be non-idiomatic noise.
#![allow(clippy::result_large_err)]

mod boundary;
mod config;
mod map;
mod proto;
mod resource;
mod services;
mod signer;
mod size;
mod status;

pub use boundary::Sha256Boundary;
pub use config::ReapiConfig;
pub use map::{ar_from_reapi, ar_to_reapi, dir_child_digests};
pub use proto::{build, bytestream, google, reapi, rpc};
pub use resource::ResourceName;
pub use services::{ActionCacheSvc, ByteStreamSvc, CapabilitiesSvc, CasV2Svc};
pub use signer::{AttestationSigner, DevSelfSigner};
pub use size::{CasSizeSource, SizeSource};
pub use status::status_from_backend;
