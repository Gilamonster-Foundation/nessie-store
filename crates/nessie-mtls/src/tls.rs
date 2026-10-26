//! Ready-made rustls configs: the node certificate plus [`MeshVerifier`] on both
//! sides of a handshake. TLS 1.3 only (the verifier refuses 1.2 signatures anyway).

use std::sync::Arc;

use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ServerConfig};

use crate::{MeshVerifier, MtlsError, NodeIdentity};

/// Always answers with the node's one certificate.
#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl rustls::server::ResolvesServerCert for Fixed {
    fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

impl rustls::client::ResolvesClientCert for Fixed {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

fn gen_err(e: rustls::Error) -> MtlsError {
    MtlsError::Generate(e.to_string())
}

/// A server config that presents `identity` and **requires** a client certificate
/// `verifier` admits.
///
/// # Errors
///
/// [`MtlsError::Generate`] if rustls rejects the key or protocol selection.
pub fn server_config(
    identity: &NodeIdentity,
    verifier: Arc<MeshVerifier>,
) -> Result<Arc<ServerConfig>, MtlsError> {
    let mut cfg =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(gen_err)?
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(Arc::new(Fixed(identity.node_certified_key()?)));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// A client config that presents `identity` and accepts only servers `verifier` admits.
///
/// # Errors
///
/// [`MtlsError::Generate`] if rustls rejects the key or protocol selection.
pub fn client_config(
    identity: &NodeIdentity,
    verifier: Arc<MeshVerifier>,
) -> Result<Arc<ClientConfig>, MtlsError> {
    Ok(Arc::new(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(gen_err)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_cert_resolver(Arc::new(Fixed(identity.node_certified_key()?))),
    ))
}
