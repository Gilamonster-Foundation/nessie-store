//! The REAPI gRPC service implementations (a sub-manifest).
//!
//! Each service wraps the backends behind a `Sha256Boundary` and the shared
//! translation layer. `Capabilities` lands first (the smallest RPC); CAS,
//! ByteStream, and ActionCache follow.

mod capabilities;

pub use capabilities::CapabilitiesSvc;
