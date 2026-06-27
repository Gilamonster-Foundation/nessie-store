//! Embedded userspace NFSv3 server for nessie-store.
//!
//! Serves a real on-disk directory tree over NFSv3 **in-process** — no host
//! kernel NFS server. The implementation relies on Unix filesystem semantics
//! (inode-stable handles, AUTH_UNIX ownership/`chown`, symlinks), so it is
//! **Unix-only**: it lives in the [`unix`] module and is re-exported on
//! `#[cfg(unix)]`.
//!
//! On non-Unix targets the crate still compiles (so `cargo build --workspace`
//! stays green on Windows), but there is no `PassthroughFs` and [`serve`] returns
//! an [`std::io::ErrorKind::Unsupported`] error instead.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{PassthroughFs, serve};

#[cfg(not(unix))]
mod nonunix;
#[cfg(not(unix))]
pub use nonunix::serve;
