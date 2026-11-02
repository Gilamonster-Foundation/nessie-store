//! #155: `MeshVerifier` honours the user's agent-mesh revocation log.
//!
//! Red until `MeshVerifier::with_revocation` exists. The log logic itself is
//! agent-mesh-protocol's `RevocationGuard`; these tests pin only how nessie plugs it in.

mod common;

use std::fs;
use std::path::PathBuf;

use agent_mesh_protocol::{AgentKey, RevocationEntry, RevocationGuard, RevocationLog, Scope};
use common::*;
use nessie_mtls::{MeshVerifier, MtlsError, PeerProof};

/// A user, a persisted genesis log, and a verifier backed by it.
struct Fx {
    peer: Peer,
    dir: tempfile::TempDir,
    verifier: MeshVerifier,
}

impl Fx {
    fn new() -> Self {
        let peer = peer();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocation.json");
        let genesis = RevocationLog::genesis(&peer.user).unwrap();
        fs::write(&path, serde_json::to_vec(&genesis).unwrap()).unwrap();
        let guard = RevocationGuard::open(&path, peer.user.fingerprint()).unwrap();
        let verifier = MeshVerifier::with_revocation(authorized(&[&peer.ssh_line]), guard);
        Self {
            peer,
            dir,
            verifier,
        }
    }

    fn log_path(&self) -> PathBuf {
        self.dir.path().join("revocation.json")
    }

    fn read(&self) -> RevocationLog {
        serde_json::from_slice(&fs::read(self.log_path()).unwrap()).unwrap()
    }

    fn write(&self, log: &RevocationLog) {
        fs::write(self.log_path(), serde_json::to_vec(log).unwrap()).unwrap();
    }

    fn append(&self, e: RevocationEntry) {
        let mut log = self.read();
        log.append(&self.peer.user, e).unwrap();
        self.write(&log);
    }

    /// The certificate of a fresh agent of the same operator, optionally scoped
    /// to one generation.
    fn agent_cert(&self, generation: Option<u64>) -> Vec<u8> {
        let mut meta = metadata("nessie");
        if let Some(g) = generation {
            meta.caveats.valid_for_generation = Scope::Only([g].into());
        }
        let agent = AgentKey::issue(&self.peer.user, meta);
        let proof = PeerProof {
            chain: agent.cert().clone(),
            binding: binding(&self.peer.user, &self.peer.ssh),
        };
        proof
            .certificate(&agent.signing_key_bytes())
            .unwrap()
            .to_vec()
    }
}

#[test]
fn an_unrevoked_agent_is_admitted() {
    let fx = Fx::new();
    fx.verifier.verify_peer(&cert(&fx.peer)).unwrap();
}

#[test]
fn a_revoked_agent_is_refused_but_its_sibling_is_not() {
    let fx = Fx::new();
    let sibling = fx.agent_cert(None);
    fx.append(RevocationEntry::Revoke {
        agent: fx.peer.agent.fingerprint(),
    });
    let err = fx.verifier.verify_peer(&cert(&fx.peer)).unwrap_err();
    assert!(matches!(err, MtlsError::Revoked(_)), "{err:?}");
    fx.verifier.verify_peer(&sibling).unwrap();
}

#[test]
fn a_generation_bump_refuses_the_old_chain_and_admits_a_reissued_one() {
    let fx = Fx::new();
    let old = fx.agent_cert(Some(0));
    fx.verifier.verify_peer(&old).unwrap();
    fx.append(RevocationEntry::Generation { current: 1 });
    assert!(fx.verifier.verify_peer(&old).is_err());
    let reissued = fx.agent_cert(Some(1));
    fx.verifier.verify_peer(&reissued).unwrap();
}

#[test]
fn a_revocation_appended_while_running_applies_without_rebuilding_the_verifier() {
    let fx = Fx::new();
    fx.verifier.verify_peer(&cert(&fx.peer)).unwrap();
    fx.append(RevocationEntry::Revoke {
        agent: fx.peer.agent.fingerprint(),
    });
    assert!(fx.verifier.verify_peer(&cert(&fx.peer)).is_err());
}

#[test]
fn a_deleted_log_refuses_everyone() {
    let fx = Fx::new();
    fs::remove_file(fx.log_path()).unwrap();
    let err = fx.verifier.verify_peer(&cert(&fx.peer)).unwrap_err();
    assert!(matches!(err, MtlsError::Revocation(_)), "{err:?}");
}

#[test]
fn a_log_rolled_back_to_an_older_one_refuses_everyone() {
    let fx = Fx::new();
    let old = fx.read();
    fx.append(RevocationEntry::Generation { current: 1 });
    fx.verifier.verify_peer(&fx.agent_cert(Some(1))).unwrap();
    fx.write(&old);
    assert!(fx.verifier.verify_peer(&fx.agent_cert(Some(1))).is_err());
}

#[test]
fn without_a_guard_behaviour_is_unchanged() {
    let p = peer();
    let v = verifier(&[&p.ssh_line]);
    v.verify_peer(&cert(&p)).unwrap();
}
