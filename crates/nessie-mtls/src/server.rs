//! The rustls server configuration the cache faces terminate TLS with.

use std::sync::Arc;

use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::{MeshVerifier, MtlsError, NodeIdentity};

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// A TLS 1.3-only server configuration presenting `node`'s certificate.
///
/// With `clients` the client certificate is **mandatory** and judged by that
/// verifier (mTLS); with `None` there is no client authentication (server TLS
/// only, for faces that authenticate some other way). `alpn` is the protocol
/// list to offer, e.g. `[b"h2"]`.
pub fn server_config(
    node: &NodeIdentity,
    clients: Option<Arc<MeshVerifier>>,
    alpn: &[&[u8]],
) -> Result<ServerConfig, MtlsError> {
    let builder = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|e| MtlsError::Generate(e.to_string()))?;
    let builder = match clients {
        Some(v) => builder.with_client_cert_verifier(v),
        None => builder.with_no_client_auth(),
    };
    let mut cfg = builder.with_cert_resolver(Arc::new(Fixed(node.node_certified_key()?)));
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(cfg)
}
