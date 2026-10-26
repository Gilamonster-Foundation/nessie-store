//! The rustls verifiers.

use agent_mesh_protocol::Fingerprint;

use crate::{AuthorizedKeys, MtlsError, PeerProof};

/// Admits a peer by the mesh-proof rules; implements both rustls verifier traits.
#[derive(Debug)]
pub struct MeshVerifier {
    authorized: AuthorizedKeys,
}

impl MeshVerifier {
    /// A verifier trusting exactly `authorized`.
    pub fn new(authorized: AuthorizedKeys) -> Self {
        Self { authorized }
    }

    /// Apply the acceptance rules to a peer certificate, returning the peer's
    /// agent fingerprint.
    ///
    /// Admitted iff, in order: the certificate's key is ed25519 and equals the
    /// chain's agent key; the chain verifies (signatures and attenuation, rooted
    /// at a user); the binding is for the chain's root user key; and the binding
    /// verifies against some key in `authorized_keys`. The handshake signature
    /// proving possession is checked by the rustls trait impls.
    pub fn verify_peer(&self, end_entity: &[u8]) -> Result<Fingerprint, MtlsError> {
        let (cert_key, proof) = PeerProof::from_certificate(end_entity)?;
        if cert_key != proof.chain.agent_pubkey {
            return Err(MtlsError::KeyMismatch);
        }
        proof
            .chain
            .verify()
            .map_err(|e| MtlsError::BadChain(e.to_string()))?;
        if proof.binding.user_pubkey != proof.chain.root_user_pubkey() {
            return Err(MtlsError::BindingUserMismatch);
        }
        if !self
            .authorized
            .keys()
            .iter()
            .any(|k| proof.binding.verify(k).is_ok())
        {
            return Err(MtlsError::NotAuthorized);
        }
        Ok(proof.chain.agent_fingerprint())
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
