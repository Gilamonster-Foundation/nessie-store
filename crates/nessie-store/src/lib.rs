//! The `nessie-store` daemon library.
//!
//! Speaks a faithful subset of the NetApp ONTAP REST API over a pluggable
//! storage backend. The binary (`src/main.rs`) is a thin CLI over this library;
//! tests drive [`app`] in-process.

#![forbid(unsafe_code)]

// Always available — the light surface the PyO3 wheel exposes.
pub mod config;
pub mod identity;

#[cfg(feature = "python")]
mod python;

// The daemon: HTTP/TLS server + control plane. Gated so the PyO3 wheel (and any
// config-only consumer) need not compile the TLS/HTTP stack.
#[cfg(feature = "daemon")]
pub mod auth;
#[cfg(feature = "daemon")]
pub mod blocking;
#[cfg(feature = "daemon")]
pub mod cas_node;
#[cfg(feature = "daemon")]
pub mod error;
#[cfg(feature = "daemon")]
pub mod routes;
#[cfg(feature = "daemon")]
pub mod snapmirror;
#[cfg(feature = "daemon")]
pub mod snapshots;
#[cfg(feature = "daemon")]
pub mod state;
#[cfg(feature = "daemon")]
pub mod tls;
#[cfg(feature = "daemon")]
pub mod volumes;

#[cfg(feature = "daemon")]
pub use routes::app;
#[cfg(feature = "daemon")]
pub use state::AppState;
