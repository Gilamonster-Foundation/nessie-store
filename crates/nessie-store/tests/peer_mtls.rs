//! The three-node swarm over `https` with mutual TLS, and the nodes it must refuse.
//!
//! Every key is generated here; nothing is checked in. Nodes share an operator
//! (one SSH key listed in each `authorized_keys`) except the intruder, whose SSH
//! key nobody lists.

use std::net::TcpListener;
use std::sync::Arc;

use agent_mesh_protocol::{AgentMetadata, Caveats, GitHubBinding, UserKey};
use nessie_backend_core::{CasBackend as _, Digest, PeerId};
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};
use nessie_store::cas_node::CasNode;
use nessie_store::cas_routes::peer_routes;
use nessie_store::config::{CasConfig, CasMode, CasPeer};
use nessie_store::peer_tls::serve_peer;
use rand::RngCore as _;
use ssh_key::PrivateKey;
use ssh_key::private::{Ed25519Keypair, KeypairData};

/// An operator: an SSH key plus the `authorized_keys` line that lists it.
struct Operator {
    ssh: PrivateKey,
    line: String,
}

impl Operator {
    fn new() -> Self {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let ssh =
            PrivateKey::new(KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed)), "").unwrap();
        let line = ssh.public_key().to_openssh().unwrap();
        Self { ssh, line }
    }

    /// A node identity whose GitHub binding this operator signed.
    fn node(&self) -> NodeIdentity {
        let user = UserKey::generate();
        let binding = GitHubBinding::sign(&user.public(), &self.ssh, None).unwrap();
        let meta = AgentMetadata {
            role: "nessie".into(),
            host: "h".into(),
            capabilities: vec![],
            issued_at: "2026-10-26T00:00:00Z".into(),
            expires_at: None,
            caveats: Caveats::top(),
        };
        NodeIdentity::issue(&user, binding, meta).unwrap()
    }
}

fn verifier(trusted: &[&Operator]) -> Arc<MeshVerifier> {
    let text = trusted.iter().map(|o| o.line.as_str()).collect::<Vec<_>>();
    Arc::new(MeshVerifier::new(AuthorizedKeys::parse(&text.join("\n"))))
}

/// A node plus the thread serving its mTLS peer listener.
struct Daemon {
    url: String,
    node: Arc<CasNode>,
}

impl Daemon {
    fn reserve() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    /// `me` presents `id`; it trusts and is trusted per `trusts`.
    fn start(cfg: &CasConfig, name: &str, id: &NodeIdentity, trusts: &[&Operator]) -> Self {
        let (listener, url) = Self::reserve();
        Self::serve(listener, url, cfg, name, id, trusts)
    }

    fn serve(
        listener: TcpListener,
        url: String,
        cfg: &CasConfig,
        name: &str,
        id: &NodeIdentity,
        trusts: &[&Operator],
    ) -> Self {
        let v = verifier(trusts);
        let client = nessie_mtls::client_config(id, v.clone()).unwrap();
        let server = nessie_mtls::server_config(id, v).unwrap();
        let node =
            Arc::new(CasNode::from_config_tls(cfg, PeerId::new(name), Some(client)).unwrap());
        let app = peer_routes(node.clone());
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(serve_peer(listener, app, server))
                .unwrap();
        });
        Self { url, node }
    }
}

fn durable() -> CasConfig {
    CasConfig {
        mode: CasMode::WriteOnly,
        replication_factor: Some(1),
        byte_budget: Some(1 << 30),
        peer_api: true,
        gc_grace_secs: 0,
        ..CasConfig::default()
    }
}

fn cache(peer_url: &str) -> CasConfig {
    CasConfig {
        mode: CasMode::Lru,
        byte_budget: Some(1),
        gc_grace_secs: 0,
        peer_api: true,
        peers: vec![CasPeer {
            url: peer_url.to_owned(),
            durable: true,
        }],
        ..CasConfig::default()
    }
}

fn read(node: &CasNode, d: &Digest) -> Vec<u8> {
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut node.get(d).unwrap(), &mut v).unwrap();
    v
}

#[test]
fn three_nodes_evict_and_restore_over_mutual_tls() {
    let op = Operator::new();
    let d = Daemon::start(&durable(), "durable", &op.node(), &[&op]);
    let a = Daemon::start(&cache(&d.url), "cache-a", &op.node(), &[&op]);
    let b = Daemon::start(&cache(&d.url), "cache-b", &op.node(), &[&op]);

    let digest = a.node.put(&mut &b"over mutual tls"[..]).unwrap();
    a.node.maintain_once().unwrap(); // push to the durable node, then evict
    assert!(d.node.has(&digest).unwrap(), "pushed over https");
    assert!(!a.node.local().has(&digest).unwrap(), "evicted from a");
    assert_eq!(
        read(&b.node, &digest),
        b"over mutual tls",
        "b fetches over https"
    );
}

/// The intruder is trusted by nobody. It trusts the operator, so only the *server*
/// side of each handshake can be what refuses it.
#[test]
fn a_node_nobody_lists_cannot_push_to_or_fetch_from_the_swarm() {
    let op = Operator::new();
    let evil = Operator::new();
    let d = Daemon::start(&durable(), "durable", &op.node(), &[&op]);
    let held = d.node.put(&mut &b"the swarm's secret"[..]).unwrap();
    let i = Daemon::start(&cache(&d.url), "intruder", &evil.node(), &[&op, &evil]);

    // fetch: the intruder asks the durable node for a blob it holds.
    assert!(i.node.get(&held).is_err(), "handshake refused, no bytes");
    assert!(!i.node.local().has(&held).unwrap(), "nothing cached");

    // push: the intruder offers a blob; the durable node must not receive it.
    let mine = i.node.put(&mut &b"smuggled"[..]).unwrap();
    let _ = i.node.maintain_once();
    assert!(!d.node.has(&mine).unwrap(), "no blob moved to the swarm");
}

/// The mirror image: the intruder *serves*, and an honest node refuses to talk to it.
#[test]
fn an_honest_node_will_not_fetch_from_or_push_to_an_unlisted_server() {
    let op = Operator::new();
    let evil = Operator::new();
    let i = Daemon::start(&durable(), "intruder", &evil.node(), &[&op, &evil]);
    let planted = i.node.put(&mut &b"planted by the intruder"[..]).unwrap();
    let a = Daemon::start(&cache(&i.url), "cache-a", &op.node(), &[&op]);

    // fetch: a would-be read of a blob only the intruder holds fails closed.
    assert!(a.node.get(&planted).is_err(), "client refused the server");
    assert!(!a.node.local().has(&planted).unwrap(), "nothing cached");

    // push: a's eviction pass cannot reach the intruder, so the blob is not lost
    // *and* nothing lands there.
    let mine = a.node.put(&mut &b"honest data"[..]).unwrap();
    let _ = a.node.maintain_once();
    assert!(!i.node.has(&mine).unwrap(), "no blob moved to the intruder");
}
