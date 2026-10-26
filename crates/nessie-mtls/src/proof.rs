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
    pub fn certificate(&self, seed: &[u8; 32]) -> Result<CertificateDer<'static>, MtlsError> {
        Ok(self.build(seed)?.der().clone())
    }

    /// As [`certificate`](Self::certificate), keeping the rcgen handle for PEM.
    pub(crate) fn build(&self, seed: &[u8; 32]) -> Result<rcgen::Certificate, MtlsError> {
        let gen_err = |e: &dyn std::fmt::Display| MtlsError::Generate(e.to_string());
        let pkcs8 = pkcs8_der(seed)?;
        let key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ED25519)
            .map_err(|e| gen_err(&e))?;
        let json = serde_json::to_vec(self).map_err(|e| gen_err(&e))?;
        let mut params =
            rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| gen_err(&e))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "nessie-mtls");
        params
            .custom_extensions
            .push(rcgen::CustomExtension::from_oid_content(PROOF_OID, json));
        params.self_signed(&key).map_err(|e| gen_err(&e))
    }

    /// Parse a peer certificate into the ed25519 key it presents and the proof
    /// it carries. Does not judge trust; see [`crate::MeshVerifier`].
    pub fn from_certificate(der: &[u8]) -> Result<([u8; 32], Self), MtlsError> {
        let (rest, cert) = x509_parser::parse_x509_certificate(der)
            .map_err(|e| MtlsError::BadCertificate(e.to_string()))?;
        if !rest.is_empty() {
            return Err(MtlsError::BadCertificate(
                "trailing bytes after certificate".into(),
            ));
        }
        let spki = cert.public_key();
        let key: [u8; 32] =
            if spki.algorithm.algorithm == x509_parser::oid_registry::OID_SIG_ED25519 {
                spki.subject_public_key
                    .data
                    .as_ref()
                    .try_into()
                    .map_err(|_| MtlsError::NotEd25519)?
            } else {
                return Err(MtlsError::NotEd25519);
            };
        let oid = x509_parser::der_parser::oid::Oid::from(PROOF_OID)
            .map_err(|_| MtlsError::BadExtension("bad OID constant".into()))?;
        let mut found = cert.extensions().iter().filter(|e| e.oid == oid);
        let ext = found
            .next()
            .ok_or_else(|| MtlsError::BadExtension("absent".into()))?;
        if found.next().is_some() {
            return Err(MtlsError::BadExtension("duplicated".into()));
        }
        if ext.value.len() > MAX_PROOF_LEN {
            return Err(MtlsError::BadExtension("oversized".into()));
        }
        let proof = serde_json::from_slice(ext.value)
            .map_err(|e| MtlsError::BadExtension(e.to_string()))?;
        Ok((key, proof))
    }
}

/// PKCS#8 DER for the ed25519 key with this seed. Zeroised on drop.
pub(crate) fn pkcs8_der(
    seed: &[u8; 32],
) -> Result<rustls::pki_types::PrivatePkcs8KeyDer<'static>, MtlsError> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    let doc = ed25519_dalek::SigningKey::from_bytes(seed)
        .to_pkcs8_der()
        .map_err(|e| MtlsError::Generate(e.to_string()))?;
    Ok(rustls::pki_types::PrivatePkcs8KeyDer::from(
        doc.as_bytes().to_vec(),
    ))
}
