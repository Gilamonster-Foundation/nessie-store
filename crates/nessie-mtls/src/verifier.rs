//! The rustls verifiers.

use agent_mesh_protocol::{CertChain, Fingerprint, GitHubBinding, GuardError, RevocationGuard};

use crate::{AuthorizedKeys, MtlsError, PeerProof};

/// Admits a peer by the mesh-proof rules; implements both rustls verifier traits.
///
/// With a [`RevocationGuard`] ([`with_revocation`](Self::with_revocation)) every
/// chain is checked against the user's revocation log as it stands *now*: a
/// revoked agent or an out-of-generation chain is refused, and so is every peer
/// while the log is missing, unreadable or rolled back (fail closed). Without a
/// guard no revocation is checked, and a chain with a bounded generation scope
/// is refused outright since no current generation is known.
#[derive(Debug)]
pub struct MeshVerifier {
    authorized: AuthorizedKeys,
    revocation: Option<RevocationGuard>,
}

impl MeshVerifier {
    /// A verifier trusting exactly `authorized`, checking no revocation.
    pub fn new(authorized: AuthorizedKeys) -> Self {
        Self {
            authorized,
            revocation: None,
        }
    }

    /// A verifier trusting `authorized` that also honours `guard`'s revocation log.
    pub fn with_revocation(authorized: AuthorizedKeys, guard: RevocationGuard) -> Self {
        Self {
            authorized,
            revocation: Some(guard),
        }
    }

    /// The guard, if revocation is configured.
    pub fn revocation(&self) -> Option<&RevocationGuard> {
        self.revocation.as_ref()
    }

    /// Apply the acceptance rules to a peer certificate, returning the peer's
    /// agent fingerprint.
    ///
    /// Admitted iff the certificate's key is ed25519 and equals the chain's agent
    /// key, and [`admit`](Self::admit) accepts the chain. The handshake signature
    /// proving possession is checked by the rustls trait impls.
    pub fn verify_peer(&self, end_entity: &[u8]) -> Result<Fingerprint, MtlsError> {
        let (cert_key, proof) = PeerProof::from_certificate(end_entity)?;
        if cert_key != proof.chain.agent_pubkey {
            return Err(MtlsError::KeyMismatch);
        }
        self.admit(&proof.chain, &proof.binding)
    }

    /// The one acceptance point: the chain verifies (against the current
    /// revocation state when a guard is configured, else plainly), the binding
    /// is for the chain's root user and is signed by a listed key.
    pub fn admit(
        &self,
        chain: &CertChain,
        binding: &GitHubBinding,
    ) -> Result<Fingerprint, MtlsError> {
        match &self.revocation {
            Some(g) => g.check(chain).map_err(|e| match e {
                GuardError::Revoked(a) => MtlsError::Revoked(a),
                GuardError::BadCertChain(m) => MtlsError::BadChain(m),
                GuardError::Unavailable(m) => MtlsError::Revocation(m),
            })?,
            None => chain
                .verify()
                .map_err(|e| MtlsError::BadChain(e.to_string()))?,
        }
        if binding.user_pubkey != chain.root_user_pubkey() {
            return Err(MtlsError::BindingUserMismatch);
        }
        if !self
            .authorized
            .keys()
            .iter()
            .any(|k| binding.verify(k).is_ok())
        {
            return Err(MtlsError::NotAuthorized);
        }
        Ok(chain.agent_fingerprint())
    }
}

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, OtherError, SignatureScheme};

fn algs() -> WebPkiSupportedAlgorithms {
    ring::default_provider().signature_verification_algorithms
}

fn reject(e: MtlsError) -> Error {
    tracing::warn!(reason = %e, "mTLS peer refused");
    Error::InvalidCertificate(rustls::CertificateError::Other(OtherError(Arc::new(e))))
}

fn tls12() -> Error {
    reject(MtlsError::Tls12Refused)
}

impl ServerCertVerifier for MeshVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, Error> {
        let peer = self.verify_peer(end_entity).map_err(reject)?;
        tracing::debug!(peer = %peer.short(), "mTLS server admitted");
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(tls12())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algs())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        algs().supported_schemes()
    }
}

impl ClientCertVerifier for MeshVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let peer = self.verify_peer(end_entity).map_err(reject)?;
        tracing::debug!(peer = %peer.short(), "mTLS client admitted");
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(tls12())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algs())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        algs().supported_schemes()
    }
}
