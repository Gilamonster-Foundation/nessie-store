//! This node's agent-mesh identity on disk.
//!
//! Layout under `<data_dir>/identity/`: `agent.seed` (32 raw bytes, mode 0600),
//! `agent.chain.json` (the signed `CertChain`) and `binding.json` (the
//! operator's `GitHubBinding`), both the serde JSON the mesh already speaks.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_mesh_protocol::{
    AgentKey, AgentMetadata, CertChain, Fingerprint, GitHubBinding, UserKey,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use zeroize::Zeroizing;

use crate::proof::pkcs8_der;
use crate::{MtlsError, PeerProof};

const SEED: &str = "agent.seed";
const CHAIN: &str = "agent.chain.json";
const BINDING: &str = "binding.json";

/// An agent-mesh `AgentKey` plus the operator's `GitHubBinding`, and the
/// self-signed certificate derived from them (minted once, at construction).
pub struct NodeIdentity {
    key: AgentKey,
    cert: rcgen::Certificate,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("agent", &self.key.fingerprint().short())
            .finish_non_exhaustive()
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> MtlsError + '_ {
    move |e| MtlsError::Io(path.to_path_buf(), e)
}

impl NodeIdentity {
    fn new(key: AgentKey, binding: GitHubBinding) -> Result<Self, MtlsError> {
        key.cert()
            .verify()
            .map_err(|e| MtlsError::BadChain(e.to_string()))?;
        if binding.user_pubkey != key.cert().root_user_pubkey() {
            return Err(MtlsError::BindingUserMismatch);
        }
        let seed = Zeroizing::new(key.signing_key_bytes());
        let proof = PeerProof {
            chain: key.cert().clone(),
            binding,
        };
        Ok(Self {
            cert: proof.build(&seed)?,
            key,
        })
    }

    fn proof(&self) -> Result<PeerProof, MtlsError> {
        // The binding lives inside the certificate extension; recover it from there.
        PeerProof::from_certificate(self.cert.der()).map(|(_, p)| p)
    }

    /// Issue a fresh agent key from `user`, carrying `binding` (which must be for
    /// `user`'s key, or no peer could ever verify this node).
    pub fn issue(
        user: &UserKey,
        binding: GitHubBinding,
        metadata: AgentMetadata,
    ) -> Result<Self, MtlsError> {
        if binding.user_pubkey != user.public() {
            return Err(MtlsError::BindingUserMismatch);
        }
        Self::new(AgentKey::issue(user, metadata), binding)
    }

    /// Write `agent.seed` (0600, created atomically), `agent.chain.json` and
    /// `binding.json` into `dir`, creating it. Refuses to overwrite an existing
    /// seed. Never logs the seed.
    pub fn save(&self, dir: &Path) -> Result<(), MtlsError> {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
        let seed = Zeroizing::new(self.key.signing_key_bytes());
        write_new(&dir.join(SEED), &*seed, true)?;
        let proof = self.proof()?;
        write_json(&dir.join(CHAIN), &proof.chain)?;
        write_json(&dir.join(BINDING), &proof.binding)
    }

    /// Load the identity from `dir`. Refuses a seed file readable by group or
    /// others, a seed that is not 32 bytes, and one that does not match the chain.
    pub fn load(dir: &Path) -> Result<Self, MtlsError> {
        let seed = read_seed(&dir.join(SEED))?;
        let chain: CertChain = read_json(&dir.join(CHAIN))?;
        let binding: GitHubBinding = read_json(&dir.join(BINDING))?;
        let key = AgentKey::from_seed_and_cert(&seed, chain)
            .map_err(|e| MtlsError::Mesh(e.to_string()))?;
        Self::new(key, binding)
    }

    /// The agent's fingerprint: the identity peers see and logs record.
    pub fn fingerprint(&self) -> Fingerprint {
        self.key.fingerprint()
    }

    /// The self-signed certificate, DER.
    pub fn certificate(&self) -> Result<CertificateDer<'static>, MtlsError> {
        Ok(self.cert.der().clone())
    }

    /// The certificate and signing key as a rustls `CertifiedKey`.
    pub fn node_certified_key(&self) -> Result<Arc<CertifiedKey>, MtlsError> {
        let seed = Zeroizing::new(self.key.signing_key_bytes());
        let signer = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
            pkcs8_der(&seed)?,
        ))
        .map_err(|e| MtlsError::Generate(e.to_string()))?;
        Ok(Arc::new(CertifiedKey::new(
            vec![self.certificate()?],
            signer,
        )))
    }

    /// The certificate as PEM.
    pub fn cert_pem(&self) -> String {
        self.cert.pem()
    }

    /// The private key as PKCS#8 PEM. This is the seed: never log it.
    pub fn key_pem(&self) -> Result<Zeroizing<String>, MtlsError> {
        use ed25519_dalek::pkcs8::{EncodePrivateKey, spki::der::pem::LineEnding};
        let seed = Zeroizing::new(self.key.signing_key_bytes());
        ed25519_dalek::SigningKey::from_bytes(&seed)
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| MtlsError::Generate(e.to_string()))
    }
}

fn write_new(path: &Path, bytes: &[u8], secret: bool) -> Result<(), MtlsError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if secret { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut f = opts.open(path).map_err(io(path))?;
    f.write_all(bytes).map_err(io(path))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), MtlsError> {
    let json =
        serde_json::to_vec_pretty(value).map_err(|e| MtlsError::Json(path.to_path_buf(), e))?;
    write_new(path, &json, false)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, MtlsError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    serde_json::from_slice(&bytes).map_err(|e| MtlsError::Json(path.to_path_buf(), e))
}

/// Read the seed through one open handle so the permission check and the read
/// see the same file.
fn read_seed(path: &Path) -> Result<Zeroizing<[u8; 32]>, MtlsError> {
    let mut f = std::fs::File::open(path).map_err(io(path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = f.metadata().map_err(io(path))?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(MtlsError::SeedPermissions(PathBuf::from(path), mode));
        }
    }
    let mut buf = Zeroizing::new(Vec::with_capacity(33));
    Read::by_ref(&mut f)
        .take(33)
        .read_to_end(&mut buf)
        .map_err(io(path))?;
    let seed: [u8; 32] = buf
        .as_slice()
        .try_into()
        .map_err(|_| MtlsError::SeedLength(path.to_path_buf()))?;
    Ok(Zeroizing::new(seed))
}
