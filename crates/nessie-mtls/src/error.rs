//! The reasons a peer or a local identity is refused.

use std::path::PathBuf;

/// Why an identity could not be loaded or a peer was refused. Messages never
/// contain key material.
#[derive(Debug, thiserror::Error)]
pub enum MtlsError {
    /// The certificate could not be parsed as X.509 (or has trailing bytes).
    #[error("peer certificate is not valid X.509: {0}")]
    BadCertificate(String),
    /// The certificate's subject key is not ed25519.
    #[error("peer certificate key is not ed25519")]
    NotEd25519,
    /// The mesh-proof extension is absent, duplicated, oversized or unparseable.
    #[error("peer certificate lacks a valid mesh-proof extension: {0}")]
    BadExtension(String),
    /// The certificate key differs from the chain's agent key.
    #[error("certificate key does not match the agent key in the cert chain")]
    KeyMismatch,
    /// `CertChain::verify` failed (bad signature, amplified caveats, ...).
    #[error("cert chain does not verify: {0}")]
    BadChain(String),
    /// The binding is for a different user key than the chain's root.
    #[error("github binding is for a different user key than the chain root")]
    BindingUserMismatch,
    /// No key in `authorized_keys` makes the binding verify.
    #[error("binding is not signed by any key in authorized_keys")]
    NotAuthorized,
    /// Only TLS 1.3 is accepted.
    #[error("TLS 1.2 handshakes are not accepted")]
    Tls12Refused,
    /// A seed file readable by group or others.
    #[error("seed file {0} is group/world accessible (mode {1:o}); expected 0600")]
    SeedPermissions(PathBuf, u32),
    /// The seed file is not exactly 32 bytes.
    #[error("seed file {0} is not 32 bytes")]
    SeedLength(PathBuf),
    /// Filesystem failure.
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    /// A JSON identity file failed to parse or serialise.
    #[error("{0}: {1}")]
    Json(PathBuf, #[source] serde_json::Error),
    /// agent-mesh refused the key/chain (e.g. seed does not match the chain).
    #[error("agent-mesh: {0}")]
    Mesh(String),
    /// Certificate generation or rustls key loading failed.
    #[error("certificate generation: {0}")]
    Generate(String),
}
