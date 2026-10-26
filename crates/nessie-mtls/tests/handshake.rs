//! Real rustls client/server pairs over `tokio::io::duplex`.

mod common;

use std::sync::Arc;

use agent_mesh_protocol::UserKey;
use common::*;
use nessie_mtls::{MeshVerifier, NodeIdentity};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

fn node(user: &UserKey, ssh: &ssh_key::PrivateKey) -> NodeIdentity {
    NodeIdentity::issue(user, binding(user, ssh), metadata("nessie")).unwrap()
}

fn configs(
    server: &NodeIdentity,
    server_trusts: &[&str],
    client: &NodeIdentity,
    client_trusts: &[&str],
) -> (Arc<ServerConfig>, Arc<ClientConfig>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let sc = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(Arc::new(verifier(server_trusts)))
        .with_cert_resolver(Arc::new(StaticResolver(
            server.node_certified_key().unwrap(),
        )));
    let cc = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier(client_trusts)))
        .with_client_cert_resolver(Arc::new(StaticResolver(
            client.node_certified_key().unwrap(),
        )));
    (Arc::new(sc), Arc::new(cc))
}

#[derive(Debug)]
struct StaticResolver(Arc<rustls::sign::CertifiedKey>);
impl rustls::server::ResolvesServerCert for StaticResolver {
    fn resolve(
        &self,
        _: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
}
impl rustls::client::ResolvesClientCert for StaticResolver {
    fn resolve(
        &self,
        _: &[&[u8]],
        _: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

async fn run(
    sc: Arc<ServerConfig>,
    cc: Arc<ClientConfig>,
) -> (std::io::Result<Vec<u8>>, std::io::Result<Vec<u8>>) {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let mut s = TlsAcceptor::from(sc).accept(a).await?;
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await?;
        s.write_all(b"pong").await?;
        s.flush().await?;
        Ok(buf.to_vec())
    });
    let client = async {
        let mut c = TlsConnector::from(cc)
            .connect(ServerName::try_from("nessie.invalid").unwrap(), b)
            .await?;
        c.write_all(b"ping").await?;
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await?;
        Ok(buf.to_vec())
    };
    let c: std::io::Result<Vec<u8>> = client.await;
    (server.await.unwrap(), c)
}

#[tokio::test]
async fn handshake_completes_both_ways_for_listed_users() {
    let (u1, u2) = (UserKey::generate(), UserKey::generate());
    let ((s1, l1), (s2, l2)) = (ssh_key(), ssh_key());
    let (server, client) = (node(&u1, &s1), node(&u2, &s2));
    // Separate user roots, each bound to its own listed SSH key: both admitted.
    let (sc, cc) = configs(&server, &[&l1, &l2], &client, &[&l1, &l2]);
    let (s, c) = run(sc, cc).await;
    assert_eq!(s.unwrap(), b"ping");
    assert_eq!(c.unwrap(), b"pong");
}

#[tokio::test]
async fn handshake_fails_when_server_does_not_list_the_client() {
    let (u1, u2) = (UserKey::generate(), UserKey::generate());
    let ((s1, l1), (s2, l2)) = (ssh_key(), ssh_key());
    let (server, client) = (node(&u1, &s1), node(&u2, &s2));
    let (sc, cc) = configs(&server, &[&l1], &client, &[&l1, &l2]);
    let (s, c) = run(sc, cc).await;
    let why = s
        .expect_err("server must refuse the unlisted client")
        .to_string();
    assert!(
        why.contains("NotAuthorized"),
        "refused for the wrong reason: {why}"
    );
    assert!(c.is_err(), "client must see the refusal");
}

#[tokio::test]
async fn handshake_fails_when_client_does_not_list_the_server() {
    let (u1, u2) = (UserKey::generate(), UserKey::generate());
    let ((s1, l1), (s2, l2)) = (ssh_key(), ssh_key());
    let (server, client) = (node(&u1, &s1), node(&u2, &s2));
    let (sc, cc) = configs(&server, &[&l1, &l2], &client, &[&l2]);
    let (_s, c) = run(sc, cc).await;
    let why = c
        .expect_err("client must refuse the unlisted server")
        .to_string();
    assert!(
        why.contains("NotAuthorized"),
        "refused for the wrong reason: {why}"
    );
}

#[tokio::test]
async fn handshake_fails_when_client_presents_no_certificate() {
    let (u1, s1) = (UserKey::generate(), ssh_key());
    let server = node(&u1, &s1.0);
    let client = node(&u1, &s1.0);
    let (sc, _) = configs(&server, &[&s1.1], &client, &[&s1.1]);
    let cc =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(MeshVerifier::new(authorized(&[&s1.1]))))
            .with_no_client_auth();
    let (s, c) = run(sc, Arc::new(cc)).await;
    assert!(s.is_err());
    assert!(c.is_err());
}

fn forged_key(of: &NodeIdentity) -> Arc<rustls::sign::CertifiedKey> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    // The authorized certificate, but CertificateVerify is signed by another key.
    let other = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
    let der = other.to_pkcs8_der().unwrap();
    let signer = rustls::crypto::ring::sign::any_supported_type(
        &rustls::pki_types::PrivateKeyDer::Pkcs8(der.as_bytes().to_vec().into()),
    )
    .unwrap();
    Arc::new(rustls::sign::CertifiedKey::new(
        of.node_certified_key().unwrap().cert.clone(),
        signer,
    ))
}

fn tls13_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[tokio::test]
async fn handshake_fails_when_server_signs_with_a_different_key() {
    let (u1, u2) = (UserKey::generate(), UserKey::generate());
    let ((s1, l1), (s2, l2)) = (ssh_key(), ssh_key());
    let (server, client) = (node(&u1, &s1), node(&u2, &s2));
    let (sc, cc) = configs(&server, &[&l1, &l2], &client, &[&l1, &l2]);
    let mut sc = Arc::try_unwrap(sc).ok().unwrap();
    sc.cert_resolver = Arc::new(StaticResolver(forged_key(&server)));
    let (_s, c) = run(Arc::new(sc), cc).await;
    let why = c
        .expect_err("client must reject a forged CertificateVerify")
        .to_string();
    assert!(why.contains("BadSignature"), "wrong reason: {why}");
}

#[tokio::test]
async fn handshake_fails_when_client_signs_with_a_different_key() {
    let (u1, u2) = (UserKey::generate(), UserKey::generate());
    let ((s1, l1), (s2, l2)) = (ssh_key(), ssh_key());
    let (server, client) = (node(&u1, &s1), node(&u2, &s2));
    let (sc, cc) = configs(&server, &[&l1, &l2], &client, &[&l1, &l2]);
    let mut cc = Arc::try_unwrap(cc).ok().unwrap();
    cc.client_auth_cert_resolver = Arc::new(StaticResolver(forged_key(&client)));
    let (s, _c) = run(sc, Arc::new(cc)).await;
    let why = s
        .expect_err("server must reject a forged CertificateVerify")
        .to_string();
    assert!(why.contains("BadSignature"), "wrong reason: {why}");
}

#[tokio::test]
async fn tls12_only_peers_fail_the_handshake_in_both_directions() {
    let (u1, s1) = (UserKey::generate(), ssh_key());
    let (server, client) = (node(&u1, &s1.0), node(&u1, &s1.0));
    let t12 = &[&rustls::version::TLS12];

    // Both sides TLS 1.2 only: the client's ServerCertVerifier refuses 1.2.
    let sc = ServerConfig::builder_with_provider(tls13_provider())
        .with_protocol_versions(t12)
        .unwrap()
        .with_client_cert_verifier(Arc::new(verifier(&[&s1.1])))
        .with_cert_resolver(Arc::new(StaticResolver(
            server.node_certified_key().unwrap(),
        )));
    let cc = ClientConfig::builder_with_provider(tls13_provider())
        .with_protocol_versions(t12)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier(&[&s1.1])))
        .with_client_cert_resolver(Arc::new(StaticResolver(
            client.node_certified_key().unwrap(),
        )));
    let (_s, c) = run(Arc::new(sc), Arc::new(cc)).await;
    let why = c
        .expect_err("client verifier must refuse TLS 1.2")
        .to_string();
    assert!(why.contains("Tls12Refused"), "wrong reason: {why}");

    // A TLS 1.2 client that accepts any server: the server's ClientCertVerifier refuses 1.2.
    let sc = ServerConfig::builder_with_provider(tls13_provider())
        .with_protocol_versions(t12)
        .unwrap()
        .with_client_cert_verifier(Arc::new(verifier(&[&s1.1])))
        .with_cert_resolver(Arc::new(StaticResolver(
            server.node_certified_key().unwrap(),
        )));
    let cc = ClientConfig::builder_with_provider(tls13_provider())
        .with_protocol_versions(t12)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_client_cert_resolver(Arc::new(StaticResolver(
            client.node_certified_key().unwrap(),
        )));
    let (s, _c) = run(Arc::new(sc), Arc::new(cc)).await;
    let why = s
        .expect_err("server verifier must refuse TLS 1.2")
        .to_string();
    assert!(why.contains("Tls12Refused"), "wrong reason: {why}");
}

/// Test-only server verifier that accepts everything, so the *server's* TLS 1.2
/// client-certificate path can be reached.
#[derive(Debug)]
struct AcceptAnyServer;
impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &tls13_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &tls13_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        tls13_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
