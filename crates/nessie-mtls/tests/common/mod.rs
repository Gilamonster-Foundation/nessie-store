//! Fixtures: every key is generated in the test body, nothing is checked in.
#![allow(dead_code)]

use agent_mesh_protocol::{AgentKey, AgentMetadata, Caveats, CertChain, GitHubBinding, UserKey};
use nessie_mtls::{AuthorizedKeys, MeshVerifier, PeerProof};
use rand::RngCore;
use rand::rngs::OsRng;
use ssh_key::PrivateKey;
use ssh_key::private::{Ed25519Keypair, KeypairData};

pub fn metadata(role: &str) -> AgentMetadata {
    AgentMetadata {
        role: role.into(),
        host: "h".into(),
        capabilities: vec![],
        issued_at: "2026-10-26T00:00:00Z".into(),
        expires_at: None,
        caveats: Caveats::top(),
    }
}

/// A fresh SSH ed25519 key and its `authorized_keys` line.
pub fn ssh_key() -> (PrivateKey, String) {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let kp = Ed25519Keypair::from_seed(&seed);
    let key = PrivateKey::new(KeypairData::Ed25519(kp), "").unwrap();
    let line = key.public_key().to_openssh().unwrap();
    (key, line)
}

pub fn binding(user: &UserKey, ssh: &PrivateKey) -> GitHubBinding {
    GitHubBinding::sign(&user.public(), ssh, Some("someone".into())).unwrap()
}

pub fn authorized(lines: &[&str]) -> AuthorizedKeys {
    AuthorizedKeys::parse(&lines.join("\n"))
}

pub fn verifier(lines: &[&str]) -> MeshVerifier {
    MeshVerifier::new(authorized(lines))
}

/// A seed and the proof (root chain + binding) for it, plus the user and ssh key.
pub struct Peer {
    pub seed: [u8; 32],
    pub agent: AgentKey,
    pub user: UserKey,
    pub ssh: PrivateKey,
    pub ssh_line: String,
    pub proof: PeerProof,
}

pub fn peer() -> Peer {
    let user = UserKey::generate();
    let (ssh, ssh_line) = ssh_key();
    peer_for(user, ssh, ssh_line)
}

pub fn peer_for(user: UserKey, ssh: PrivateKey, ssh_line: String) -> Peer {
    let agent = AgentKey::issue(&user, metadata("nessie"));
    let proof = PeerProof {
        chain: agent.cert().clone(),
        binding: binding(&user, &ssh),
    };
    Peer {
        seed: agent.signing_key_bytes(),
        agent,
        user,
        ssh,
        ssh_line,
        proof,
    }
}

pub fn cert(p: &Peer) -> Vec<u8> {
    p.proof.certificate(&p.seed).unwrap().to_vec()
}

pub fn chain_of(p: &Peer) -> CertChain {
    p.proof.chain.clone()
}
