//! #152: REAPI over mutual TLS and S3 over server TLS, through the daemon's own
//! `face_tls` serve functions, with real rustls clients on loopback.

use std::net::TcpListener;
use std::sync::Arc;

use agent_mesh_protocol::{AgentMetadata, Caveats, GitHubBinding, UserKey};
use hyper_util::rt::TokioIo;
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};
use nessie_reapi::reapi::{GetCapabilitiesRequest, capabilities_client::CapabilitiesClient};
use nessie_store::face_tls::{FaceTls, serve_reapi_tls, serve_s3_tls};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, sign::CertifiedKey};
use ssh_key::private::{Ed25519Keypair, KeypairData};
use tokio_rustls::TlsConnector;
use tonic::transport::Endpoint;

fn ssh(n: u8) -> (ssh_key::PrivateKey, String) {
    let key = ssh_key::PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[n; 32])),
        "",
    )
    .unwrap();
    let line = key.public_key().to_openssh().unwrap();
    (key, line)
}

fn meta() -> AgentMetadata {
    AgentMetadata {
        role: "t".into(),
        host: "h".into(),
        capabilities: vec![],
        issued_at: "2026-10-26T00:00:00Z".into(),
        expires_at: None,
        caveats: Caveats::top(),
    }
}

/// An operator whose binding is signed by ssh key `n`, and a node issued from it.
fn operator_node(n: u8) -> NodeIdentity {
    let user = UserKey::generate();
    let (key, _) = ssh(n);
    let binding = GitHubBinding::sign(&user.public(), &key, None).unwrap();
    NodeIdentity::issue(&user, binding, meta()).unwrap()
}

/// The server's node is operator 1; the trust list names ssh key 1 only.
fn face() -> Arc<FaceTls> {
    let (_, line) = ssh(1);
    Arc::new(FaceTls::new(operator_node(1), AuthorizedKeys::parse(&line), &[]).unwrap())
}

fn backend() -> Arc<dyn nessie_backend_core::CasBackend> {
    nessie_store::faces::build_cas_backend(None).unwrap().0
}

fn listener() -> (TcpListener, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);
impl rustls::client::ResolvesClientCert for Fixed {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

/// A client that trusts the server through the mesh verifier (server key 1) and
/// presents `client` if given.
fn client_cfg(client: Option<&NodeIdentity>) -> Arc<ClientConfig> {
    let (_, line) = ssh(1);
    let b = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(MeshVerifier::new(AuthorizedKeys::parse(
            &line,
        ))));
    let mut cfg = match client {
        Some(c) => b.with_client_cert_resolver(Arc::new(Fixed(c.node_certified_key().unwrap()))),
        None => b.with_no_client_auth(),
    };
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(cfg)
}

async fn get_capabilities(
    addr: std::net::SocketAddr,
    cfg: Arc<ClientConfig>,
) -> Result<(), String> {
    let connector = tower::service_fn(move |_: http::Uri| {
        let cfg = cfg.clone();
        async move {
            let tcp = tokio::net::TcpStream::connect(addr).await?;
            let tls = TlsConnector::from(cfg)
                .connect(ServerName::try_from("localhost").unwrap(), tcp)
                .await?;
            Ok::<_, std::io::Error>(TokioIo::new(tls))
        }
    });
    let ch = Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    CapabilitiesClient::new(ch)
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .map(|_| ())
        .map_err(|e| format!("rpc: {e}"))
}

async fn start_reapi(face: Arc<FaceTls>) -> std::net::SocketAddr {
    let (l, addr) = listener();
    let router = nessie_reapi::build_router(backend(), None, Default::default());
    tokio::spawn(async move { serve_reapi_tls(router, l, &face).await });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn reapi_admits_a_listed_identity_and_refuses_unlisted_and_certless_clients() {
    let addr = start_reapi(face()).await;

    // Same operator as the server (ssh key 1, listed): a different agent, admitted.
    let listed = operator_node(1);
    get_capabilities(addr, client_cfg(Some(&listed)))
        .await
        .expect("listed identity calls GetCapabilities");

    // An operator bound to ssh key 2, which is not in authorized_keys.
    let unlisted = operator_node(2);
    get_capabilities(addr, client_cfg(Some(&unlisted)))
        .await
        .expect_err("unlisted identity must fail the handshake");

    get_capabilities(addr, client_cfg(None))
        .await
        .expect_err("a client with no certificate must fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn exported_client_cert_is_a_delegated_chain_the_server_accepts_without_the_node_seed() {
    let node_dir = tempfile::tempdir().unwrap();
    let node = operator_node(1);
    node.save(node_dir.path()).unwrap();
    let out = tempfile::tempdir().unwrap();
    let fp = nessie_store::identity_cmd::export_client_cert(node_dir.path(), out.path()).unwrap();
    assert_ne!(fp, node.fingerprint(), "a delegate, not the node itself");

    let cert = CertificateDer::from_pem_file(out.path().join("client.crt")).unwrap();
    let key = PrivateKeyDer::from_pem_file(out.path().join("client.key")).unwrap();
    let node_seed = std::fs::read(node_dir.path().join("agent.seed")).unwrap();
    for f in std::fs::read_dir(out.path()).unwrap() {
        let bytes = std::fs::read(f.unwrap().path()).unwrap();
        assert!(!bytes.windows(32).any(|w| w == node_seed.as_slice()));
    }
    assert!(
        !key.secret_der()
            .windows(32)
            .any(|w| w == node_seed.as_slice())
    );

    // A direct child of the node: delegated by it, not issued from the user key.
    let (_, proof) = nessie_mtls::PeerProof::from_certificate(&cert).unwrap();
    let (node_key, _) =
        nessie_mtls::PeerProof::from_certificate(&node.certificate().unwrap()).unwrap();
    match proof.chain.issuer {
        agent_mesh_protocol::Issuer::Agent { pubkey, parent } => {
            assert_eq!(pubkey, node_key, "issued by the node's agent key");
            assert_eq!(parent.agent_pubkey, node_key);
        }
        agent_mesh_protocol::Issuer::User(_) => panic!("issued straight from the user key"),
    }

    // The chain verifies and chains to the operator.
    let (_, line) = ssh(1);
    MeshVerifier::new(AuthorizedKeys::parse(&line))
        .verify_peer(&cert)
        .expect("exported cert admitted");

    // And it works as a TLS client credential against the server.
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    let ck = Arc::new(CertifiedKey::new(vec![cert], signer));
    let (_, l1) = ssh(1);
    let b = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(MeshVerifier::new(AuthorizedKeys::parse(&l1))))
        .with_client_cert_resolver(Arc::new(Fixed(ck)));
    let mut cfg = b;
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    let addr = start_reapi(face()).await;
    get_capabilities(addr, Arc::new(cfg))
        .await
        .expect("exported credential calls GetCapabilities");
}

#[tokio::test(flavor = "multi_thread")]
async fn s3_round_trips_over_https_with_a_pinned_certificate_and_refuses_plain_http() {
    use object_store::aws::AmazonS3Builder;
    use object_store::path::Path as OsPath;
    use object_store::{ClientOptions, ObjectStoreExt, PutPayload};

    let face = face();
    let (l, addr) = listener();
    let sc = nessie_store::config::S3ServerConfig {
        secret_key: "not-the-default".into(),
        ..Default::default()
    };
    let app = nessie_store::faces::s3_app(&sc, backend());
    let f = face.clone();
    tokio::spawn(async move { serve_s3_tls(app, l, &f).await });

    let pinned = object_store::Certificate::from_pem(face.cert_pem().as_bytes()).unwrap();
    // The fingerprint a client pins is the SHA-256 of the certificate it was given.
    assert_eq!(face.cert_sha256().len(), 64);
    let build = |endpoint: String, opts: ClientOptions| {
        AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_region("us-east-1")
            .with_bucket_name("cache")
            .with_access_key_id(&sc.access_key)
            .with_secret_access_key(&sc.secret_key)
            .with_client_options(opts)
            .with_retry(object_store::RetryConfig {
                max_retries: 0,
                ..Default::default()
            })
            .build()
            .unwrap()
    };
    let blob: &'static [u8] = b"served over https";
    let digest =
        nessie_backend_core::Digest::compute_with(nessie_backend_core::DigestAlgo::Sha256, blob);
    let hash = digest.to_string().rsplit_once(':').unwrap().1.to_owned();
    let key = OsPath::from(format!("cas/{}/{hash}", &hash[..2]));

    let tls = build(
        format!("https://localhost:{}", addr.port()),
        ClientOptions::new().with_root_certificate(pinned),
    );
    tls.put(&key, PutPayload::from_static(blob)).await.unwrap();
    let got = tls.get(&key).await.unwrap().bytes().await.unwrap();
    assert_eq!(&got[..], blob);

    let plain = build(
        format!("http://localhost:{}", addr.port()),
        ClientOptions::new().with_allow_http(true),
    );
    plain
        .get(&key)
        .await
        .expect_err("plain http to the TLS port is refused");
}

#[test]
fn faces_stay_as_today_unless_mtls_is_on_and_fail_closed_without_an_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = nessie_store::config::Config {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    assert!(FaceTls::from_config(&cfg).unwrap().is_none(), "default off");
    cfg.tls = Some(nessie_store::config::TlsConfig {
        mtls: true,
        ..Default::default()
    });
    let err = FaceTls::from_config(&cfg).err().expect("no identity yet");
    assert!(format!("{err:#}").contains("identity issue"), "{err:#}");
    operator_node(1).save(&cfg.mesh_identity_dir()).unwrap();
    let on = FaceTls::from_config(&cfg)
        .unwrap()
        .expect("identity present");
    let (_, shown) = nessie_store::face_tls::served_cert_sha256(&cfg).unwrap();
    assert_eq!(
        shown,
        on.cert_sha256(),
        "`identity show` prints what is served"
    );
}
