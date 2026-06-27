//! Non-Unix fallback for the embedded NFS server.
//!
//! The real implementation ([`crate::unix`]) depends on Unix-only filesystem
//! features (`std::os::unix`, `tokio::fs::symlink`, AUTH_UNIX `chown`), so on
//! other platforms we ship only this stub. It keeps the crate compiling as part
//! of `cargo build --workspace` (e.g. the Windows nightly) while honestly failing
//! at runtime if something tries to start the server.

use std::path::PathBuf;

/// Serve `root` over NFSv3 — **unsupported on non-Unix targets**.
///
/// The embedded server relies on Unix filesystem semantics; on this platform it
/// returns [`std::io::ErrorKind::Unsupported`] so the daemon fails cleanly rather
/// than the crate failing to compile. The signature matches the Unix
/// [`crate::unix::serve`] so callers are platform-agnostic.
pub async fn serve(
    _root: impl Into<PathBuf>,
    _bind: &str,
    _export_name: &str,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "nessie-nfs: the embedded NFS server is only supported on Unix targets",
    ))
}
