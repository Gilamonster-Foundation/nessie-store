//! This node's agent-mesh identity on disk.

use std::path::Path;
use std::sync::Arc;

use agent_mesh_protocol::{AgentKey, AgentMetadata, GitHubBinding, UserKey};
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;

use crate::{MtlsError, PeerProof};

/// An agent-mesh `AgentKey` plus the operator's `GitHubBinding`.
pub struct NodeIdentity {
    key: AgentKey,
    binding: GitHubBinding,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("agent", &self.key.fingerprint().short())
            .finish_non_exhaustive()
    }
}

impl NodeIdentity {
    /// Issue a fresh agent key from `user`, carrying `binding`.
    pub fn issue(
        _user: &UserKey,
        _binding: GitHubBinding,
        _metadata: AgentMetadata,
    ) -> Result<Self, MtlsError> {
        todo!()
    }

    /// Write `agent.seed` (0600), `agent.chain.json` and `binding.json` into
    /// `dir`, refusing to overwrite an existing seed. Never logs the seed.
    pub fn save(&self, _dir: &Path) -> Result<(), MtlsError> {
        todo!()
    }

    /// Load the identity from `dir`; refuses a group/world-accessible seed.
    pub fn load(_dir: &Path) -> Result<Self, MtlsError> {
        todo!()
    }

    /// The agent's fingerprint.
    pub fn fingerprint(&self) -> agent_mesh_protocol::Fingerprint {
        self.key.fingerprint()
    }

    /// The self-signed certificate (regenerated on every call).
    pub fn certificate(&self) -> Result<CertificateDer<'static>, MtlsError> {
        let _ = PeerProof {
            chain: self.key.cert().clone(),
            binding: self.binding.clone(),
        };
        todo!()
    }

    /// The certificate and signing key as a rustls `CertifiedKey`.
    pub fn node_certified_key(&self) -> Result<Arc<CertifiedKey>, MtlsError> {
        todo!()
    }

    /// The certificate as PEM.
    pub fn cert_pem(&self) -> Result<String, MtlsError> {
        todo!()
    }

    /// The private key as PKCS#8 PEM. This is the seed: never log it.
    pub fn key_pem(&self) -> Result<zeroize::Zeroizing<String>, MtlsError> {
        todo!()
    }
}
