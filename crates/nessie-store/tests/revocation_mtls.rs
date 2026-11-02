//! #155: a revoked agent is refused on the peer API, on REAPI, and (as a server)
//! by the dialling peer; an open connection is refused on its next request once
//! the log on disk gains a revocation; an unusable log refuses everyone.

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use agent_mesh_protocol::{
    AgentMetadata, Caveats, GitHubBinding, RevocationEntry, RevocationGuard, RevocationLog, UserKey,
};
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};
use nessie_reapi::reapi::{GetCapabilitiesRequest, capabilities_client::CapabilitiesClient};
use nessie_store::face_tls::{FaceTls, serve_reapi_tls};
use nessie_store::peer_tls::serve_peer;
use rustls::pki_types::ServerName;
use ssh_key::private::{Ed25519Keypair, KeypairData};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tonic::transport::Endpoint;

/// One operator, a revocation log on disk, and a verifier backed by it.
struct Fx {
    user: UserKey,
    ssh: ssh_key::PrivateKey,
    dir: tempfile::TempDir,
    verifier: Arc<MeshVerifier>,
}

impl Fx {
    fn new() -> Self {
        let user = UserKey::generate();
        let ssh = ssh_key::PrivateKey::new(
            KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
            "",
        )
        .unwrap();
        let line = ssh.public_key().to_openssh().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocation.json");
        let genesis = RevocationLog::genesis(&user).unwrap();
        fs::write(&path, serde_json::to_vec(&genesis).unwrap()).unwrap();
        let guard = RevocationGuard::open(&path, user.fingerprint()).unwrap();
        let verifier = Arc::new(MeshVerifier::with_revocation(
            AuthorizedKeys::parse(&line),
            guard,
        ));
        Self {
            user,
            ssh,
            dir,
            verifier,
        }
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("revocation.json")
    }

    /// A fresh agent of the operator.
    fn node(&self) -> NodeIdentity {
        let binding = GitHubBinding::sign(&self.user.public(), &self.ssh, None).unwrap();
        let meta = AgentMetadata {
            role: "t".into(),
            host: "h".into(),
            capabilities: vec![],
            issued_at: "2026-10-26T00:00:00Z".into(),
            expires_at: None,
            caveats: Caveats::top(),
        };
        NodeIdentity::issue(&self.user, binding, meta).unwrap()
    }

    fn revoke(&self, n: &NodeIdentity) {
        let mut log: RevocationLog =
            serde_json::from_slice(&fs::read(self.log()).unwrap()).unwrap();
        log.append(
            &self.user,
            RevocationEntry::Revoke {
                agent: n.fingerprint(),
            },
        )
        .unwrap();
        fs::write(self.log(), serde_json::to_vec(&log).unwrap()).unwrap();
    }
}

fn listener() -> (TcpListener, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

/// Serve `GET /ping` on the peer API as `server`.
fn start_peer(fx: &Fx, server: &NodeIdentity) -> std::net::SocketAddr {
    let (l, addr) = listener();
    let cfg =
        nessie_mtls::server_config(server, Some(fx.verifier.clone()), &[b"http/1.1"]).unwrap();
    let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
    let v = fx.verifier.clone();
    tokio::spawn(async move { serve_peer(l, app, Arc::new(cfg), v).await });
    addr
}

async fn dial(
    fx: &Fx,
    client: &NodeIdentity,
    addr: std::net::SocketAddr,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let cfg = nessie_mtls::client_config(client, fx.verifier.clone()).unwrap();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
}

/// `GET /ping` on an open keep-alive connection; the status line's code.
async fn ping<S: AsyncReadExt + AsyncWriteExt + Unpin>(s: &mut S) -> std::io::Result<u16> {
    s.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\n\r\n")
        .await?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    Ok(head.split(' ').nth(1).unwrap().parse().unwrap())
}

/// A TLS 1.3 client-cert rejection reaches the client on its first read or write.
async fn handshake_then_ping(
    fx: &Fx,
    client: &NodeIdentity,
    addr: std::net::SocketAddr,
) -> std::io::Result<u16> {
    let mut s = dial(fx, client, addr).await?;
    ping(&mut s).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_client_is_refused_on_the_peer_api_and_its_sibling_is_not() {
    let fx = Fx::new();
    let addr = start_peer(&fx, &fx.node());
    let (bad, good) = (fx.node(), fx.node());
    fx.revoke(&bad);
    assert!(handshake_then_ping(&fx, &bad, addr).await.is_err());
    assert_eq!(handshake_then_ping(&fx, &good, addr).await.unwrap(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_server_is_refused_by_the_dialling_peer() {
    let fx = Fx::new();
    let server = fx.node();
    let addr = start_peer(&fx, &server);
    fx.revoke(&server);
    assert!(dial(&fx, &fx.node(), addr).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_peer_connection_is_refused_on_its_next_request_after_a_revocation() {
    let fx = Fx::new();
    let addr = start_peer(&fx, &fx.node());
    let client = fx.node();
    let mut s = dial(&fx, &client, addr).await.unwrap();
    assert_eq!(ping(&mut s).await.unwrap(), 200);
    fx.revoke(&client);
    assert_eq!(
        ping(&mut s).await.unwrap(),
        403,
        "same connection, no reconnect"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_log_refuses_new_handshakes_and_open_connections() {
    let fx = Fx::new();
    let addr = start_peer(&fx, &fx.node());
    let client = fx.node();
    let mut s = dial(&fx, &client, addr).await.unwrap();
    assert_eq!(ping(&mut s).await.unwrap(), 200);
    fs::remove_file(fx.log()).unwrap();
    assert_eq!(ping(&mut s).await.unwrap(), 403);
    assert!(handshake_then_ping(&fx, &fx.node(), addr).await.is_err());
}

/// Dial REAPI as `client`, returning a channel that stays open for further calls.
async fn reapi_channel(
    fx: &Fx,
    client: &NodeIdentity,
    addr: std::net::SocketAddr,
) -> Result<tonic::transport::Channel, String> {
    let cfg = {
        let mut c = nessie_mtls::client_config(client, fx.verifier.clone()).unwrap();
        c.alpn_protocols = vec![b"h2".to_vec()];
        Arc::new(c)
    };
    let connector = tower::service_fn(move |_: http::Uri| {
        let cfg = cfg.clone();
        async move {
            let tcp = tokio::net::TcpStream::connect(addr).await?;
            let tls = TlsConnector::from(cfg)
                .connect(ServerName::try_from("localhost").unwrap(), tcp)
                .await?;
            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(tls))
        }
    });
    Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await
        .map_err(|e| e.to_string())
}

async fn caps(ch: &tonic::transport::Channel) -> Result<(), String> {
    CapabilitiesClient::new(ch.clone())
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn start_reapi(fx: &Fx) -> std::net::SocketAddr {
    let (l, addr) = listener();
    let backend = nessie_store::faces::build_cas_backend(None).unwrap().0;
    let router = nessie_reapi::build_router(backend, None, Default::default());
    let face = FaceTls::with_verifier(fx.node(), fx.verifier.clone(), &[]).unwrap();
    tokio::spawn(async move { serve_reapi_tls(router, l, &face).await });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_client_is_refused_on_reapi_and_an_open_channel_dies_on_revocation() {
    let fx = Fx::new();
    let addr = start_reapi(&fx);
    let (bad, live) = (fx.node(), fx.node());
    fx.revoke(&bad);
    let refused = match reapi_channel(&fx, &bad, addr).await {
        Ok(ch) => caps(&ch).await,
        Err(e) => Err(e),
    };
    assert!(refused.is_err());

    let ch = reapi_channel(&fx, &live, addr).await.unwrap();
    caps(&ch).await.expect("admitted before revocation");
    fx.revoke(&live);
    assert!(
        caps(&ch).await.is_err(),
        "same channel, next request refused"
    );
}

#[test]
fn identity_show_reports_the_log_head_or_that_none_is_configured() {
    let fx = Fx::new();
    let mut cfg = nessie_store::config::Config {
        data_dir: fx.dir.path().to_path_buf(),
        ..Default::default()
    };
    fx.node().save(&cfg.mesh_identity_dir()).unwrap();
    assert_eq!(
        nessie_store::face_tls::revocation_summary(&cfg).unwrap(),
        "no revocation log configured"
    );
    cfg.tls = Some(nessie_store::config::TlsConfig {
        revocation_log: Some(fx.log()),
        ..Default::default()
    });
    assert_eq!(
        nessie_store::face_tls::revocation_summary(&cfg).unwrap(),
        "revocation log counter 1 generation 0"
    );
}
