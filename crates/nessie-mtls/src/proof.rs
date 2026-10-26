//! The self-signed certificate carrying a node's mesh proof.

use agent_mesh_protocol::{CertChain, GitHubBinding};
use rustls::pki_types::CertificateDer;
use serde::{Deserialize, Serialize};

use crate::MtlsError;

/// The extension OID, `2.25.8763957256297098986` (see the crate docs).
pub const PROOF_OID: &[u64] = &[2, 25, 8_763_957_256_297_098_986];

/// Upper bound on the extension value, a guard against oversized handshakes.
pub const MAX_PROOF_LEN: usize = 64 * 1024;

/// What the certificate extension carries: the already-signed records, unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerProof {
    /// The agent's certificate chain.
    pub chain: CertChain,
    /// The operator's cross-signature binding their user key to an SSH key.
    pub binding: GitHubBinding,
}

impl PeerProof {
    /// Mint the self-signed certificate for the agent key `seed`, embedding
    /// `self` as the extension. The certificate key is `seed`'s ed25519 key.
    pub fn certificate(&self, _seed: &[u8; 32]) -> Result<CertificateDer<'static>, MtlsError> {
        todo!()
    }

    /// Parse a peer certificate into the ed25519 key it presents and the proof
    /// it carries. Does not judge trust; see [`crate::MeshVerifier`].
    pub fn from_certificate(_der: &[u8]) -> Result<([u8; 32], Self), MtlsError> {
        todo!()
    }
}
