//! The `nessie-store` daemon library.
//!
//! Speaks a faithful subset of the NetApp ONTAP REST API over a pluggable
//! storage backend. The binary (`src/main.rs`) is a thin CLI over this library;
//! tests drive [`app`] in-process.

#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod error;
pub mod identity;
pub mod routes;
pub mod snapshots;
pub mod state;
pub mod volumes;

pub use routes::app;
pub use state::AppState;
