//! The real `nessie-store serve` with `[tls] mtls = true`: ONTAP REST keeps Basic
//! auth and asks for no client certificate; `/cas/v1` lives only on the mTLS
//! peer listener.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_mesh_protocol::{AgentMetadata, Caveats, GitHubBinding, UserKey};
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};
use rand::RngCore as _;
use ssh_key::PrivateKey;
use ssh_key::private::{Ed25519Keypair, KeypairData};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn node(ssh: &PrivateKey) -> NodeIdentity {
    let user = UserKey::generate();
    let binding = GitHubBinding::sign(&user.public(), ssh, None).unwrap();
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

#[test]
fn ontap_keeps_basic_auth_and_cas_needs_a_client_cert() {
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let ssh = PrivateKey::new(KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed)), "").unwrap();
    let line = ssh.public_key().to_openssh().unwrap();

    let dir = tempfile::tempdir().unwrap();
    node(&ssh).save(&dir.path().join("identity")).unwrap();
    std::fs::write(dir.path().join("authorized_keys"), format!("{line}\n")).unwrap();

    let (ontap, peer) = (free_port(), free_port());
    let cfg = dir.path().join("nessie-store.toml");
    std::fs::write(
        &cfg,
        format!(
            "listen = \"127.0.0.1:{ontap}\"\ndata_dir = {:?}\n\
             [tls]\nmtls = true\npeer_listen = \"127.0.0.1:{peer}\"\n\
             [cas]\npeer_api = true\nbyte_budget = 1048576\n",
            dir.path()
        ),
    )
    .unwrap();
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_nessie-store"))
            .args(["serve", "--config"])
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    // ONTAP: HTTPS, Basic auth, no client certificate.
    let plain = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let cluster = format!("https://127.0.0.1:{ontap}/api/cluster");
    let started = Instant::now();
    let ok = loop {
        match plain
            .get(&cluster)
            .basic_auth("admin", Some("admin"))
            .send()
        {
            Ok(r) => break r,
            Err(e) if started.elapsed() > Duration::from_secs(30) => panic!("daemon: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(
        ok.status().is_success(),
        "ONTAP with Basic auth: {}",
        ok.status()
    );
    assert_eq!(
        plain.get(&cluster).send().unwrap().status(),
        401,
        "still Basic-gated"
    );

    // The peer API is not on the ONTAP listener.
    let blob = "blake3:0000000000000000000000000000000000000000000000000000000000000000";
    let on_ontap = plain
        .head(format!("https://127.0.0.1:{ontap}/cas/v1/blobs/{blob}"))
        .basic_auth("admin", Some("admin"))
        .send()
        .unwrap();
    assert_eq!(on_ontap.status(), 404, "/cas/v1 is not mounted on ONTAP");

    // The peer listener refuses a client with no certificate...
    let peer_url = format!("https://127.0.0.1:{peer}/cas/v1/blobs/{blob}");
    assert!(
        plain.head(&peer_url).send().is_err(),
        "no client cert: handshake fails"
    );

    // ...and serves one the operator vouches for.
    let v = Arc::new(MeshVerifier::new(AuthorizedKeys::parse(&line)));
    let tls = nessie_mtls::client_config(&node(&ssh), v).unwrap();
    let mesh = reqwest::blocking::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .unwrap();
    assert_eq!(
        mesh.head(&peer_url).send().unwrap().status(),
        404,
        "admitted; blob absent"
    );
}
