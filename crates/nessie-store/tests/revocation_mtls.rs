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
        self.revoke_in(&self.log(), n);
    }

    /// A second, independent view of the operator's log (its own file, guard
    /// and `Seen`), as another node would hold it. Returns its path and verifier.
    fn other_named(&self, name: &str) -> (PathBuf, Arc<MeshVerifier>) {
        let path = self.dir.path().join(name);
        fs::copy(self.log(), &path).unwrap();
        let line = self.ssh.public_key().to_openssh().unwrap();
        let guard = RevocationGuard::open(&path, self.user.fingerprint()).unwrap();
        let v = MeshVerifier::with_revocation(AuthorizedKeys::parse(&line), guard);
        (path, Arc::new(v))
    }

    fn other(&self) -> (PathBuf, Arc<MeshVerifier>) {
        self.other_named("other.json")
    }

    fn revoke_in(&self, path: &std::path::Path, n: &NodeIdentity) {
        let mut log: RevocationLog = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        log.append(
            &self.user,
            RevocationEntry::Revoke {
                agent: n.fingerprint(),
            },
        )
        .unwrap();
        fs::write(path, serde_json::to_vec(&log).unwrap()).unwrap();
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

// ---- resumption: a resumed handshake must not skip admit() -------------------

fn shared_client(fx: &Fx, node: &NodeIdentity) -> Arc<rustls::ClientConfig> {
    Arc::new(nessie_mtls::client_config(node, fx.verifier.clone()).unwrap())
}

/// One request on a fresh connection from `cfg`; reading the reply also consumes
/// any session ticket the server sent, so a later connection can try to resume.
async fn fresh_ping(
    cfg: &Arc<rustls::ClientConfig>,
    addr: std::net::SocketAddr,
) -> std::io::Result<u16> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let mut s = TlsConnector::from(cfg.clone())
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await?;
    ping(&mut s).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_client_cannot_resume_a_session() {
    let fx = Fx::new();
    let addr = start_peer(&fx, &fx.node());
    let client = fx.node();
    let cfg = shared_client(&fx, &client);
    assert_eq!(fresh_ping(&cfg, addr).await.unwrap(), 200);
    fx.revoke(&client);
    assert!(
        fresh_ping(&cfg, addr).await.is_err(),
        "handshake itself refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_server_cannot_be_resumed_into() {
    let fx = Fx::new();
    let server = fx.node();
    let addr = start_peer(&fx, &server);
    let cfg = shared_client(&fx, &fx.node());
    assert_eq!(fresh_ping(&cfg, addr).await.unwrap(), 200);
    fx.revoke(&server);
    assert!(
        fresh_ping(&cfg, addr).await.is_err(),
        "client re-judges the server"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_log_stops_resumption() {
    let fx = Fx::new();
    let addr = start_peer(&fx, &fx.node());
    let cfg = shared_client(&fx, &fx.node());
    assert_eq!(fresh_ping(&cfg, addr).await.unwrap(), 200);
    fs::remove_file(fx.log()).unwrap();
    assert!(fresh_ping(&cfg, addr).await.is_err());
}

// ---- outgoing: the peer client re-judges the server on every request ---------

/// B (durable, holds `blobs`) served over mTLS with its own log view; A, a cache
/// node with its own log view, fetching from B over a pooled client.
struct Pair {
    a: nessie_store::cas_node::CasNode,
    a_log: PathBuf,
    b_node: NodeIdentity,
    blobs: Vec<nessie_backend_core::Digest>,
    _rt: tokio::runtime::Runtime,
}

fn pair(fx: &Fx) -> Pair {
    use nessie_backend_core::{CasBackend as _, PeerId};
    use nessie_store::cas_node::CasNode;
    use nessie_store::config::{CasConfig, CasMode, CasPeer};
    let (a_log, va) = fx.other();
    let (_, vb) = fx.other_named("b.json");
    let (b_node, a_node) = (fx.node(), fx.node());
    let b_cfg = CasConfig {
        mode: CasMode::WriteOnly,
        replication_factor: Some(1),
        byte_budget: Some(1 << 30),
        peer_api: true,
        gc_grace_secs: 0,
        ..CasConfig::default()
    };
    let b = Arc::new(CasNode::from_config_tls(&b_cfg, PeerId::new("b"), None).unwrap());
    let blobs = (0..3u8)
        .map(|i| b.put(&mut &[i; 64][..]).unwrap())
        .collect();
    let (l, addr) = listener();
    let server = nessie_mtls::server_config(&b_node, Some(vb.clone()), &[b"http/1.1"]).unwrap();
    let app = nessie_store::cas_routes::peer_routes(b);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    rt.spawn(serve_peer(l, app, Arc::new(server), vb));
    let a_cfg = CasConfig {
        mode: CasMode::Lru,
        byte_budget: Some(1),
        gc_grace_secs: 0,
        peer_api: true,
        peers: vec![CasPeer {
            url: format!("https://{addr}"),
            durable: true,
        }],
        ..CasConfig::default()
    };
    let client = Arc::new(nessie_mtls::client_config(&a_node, va).unwrap());
    let a = CasNode::from_config_tls(&a_cfg, PeerId::new("a"), Some(client)).unwrap();
    Pair {
        a,
        a_log,
        b_node,
        blobs,
        _rt: rt,
    }
}

#[test]
fn a_server_revoked_in_my_log_is_refused_on_the_pooled_connections_next_request() {
    use nessie_backend_core::CasBackend as _;
    let fx = Fx::new();
    let p = pair(&fx);
    assert!(p.a.get(&p.blobs[0]).is_ok(), "fetched over the pool");
    fx.revoke_in(&p.a_log, &p.b_node); // only A's log; B is untouched
    assert!(
        p.a.get(&p.blobs[1]).is_err(),
        "next request refused, no reconnect needed"
    );
}

#[test]
fn an_unusable_log_stops_the_pooled_connection_too() {
    use nessie_backend_core::CasBackend as _;
    let fx = Fx::new();
    let p = pair(&fx);
    assert!(p.a.get(&p.blobs[0]).is_ok());
    fs::remove_file(&p.a_log).unwrap();
    assert!(p.a.get(&p.blobs[2]).is_err());
}
