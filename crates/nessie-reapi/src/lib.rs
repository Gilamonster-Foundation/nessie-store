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

mod proto;

pub use proto::build;
pub use proto::{bytestream, google, reapi, rpc};
