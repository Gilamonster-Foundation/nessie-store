//! What the cache faces need from the identity: a pinnable certificate
//! fingerprint, names a TLS client can check, and a delegated client credential.

mod common;

use agent_mesh_protocol::UserKey;
use common::*;
use nessie_mtls::NodeIdentity;

fn node() -> (NodeIdentity, String) {
    let user = UserKey::generate();
    let (ssh, line) = ssh_key();
    (
        NodeIdentity::issue(&user, binding(&user, &ssh), metadata("nessie")).unwrap(),
        line,
    )
}

#[test]
fn cert_sha256_is_hex_and_stable_across_save_and_load() {
    let (n, _) = node();
    let fp = n.cert_sha256();
    assert_eq!(fp.len(), 64);
    assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
    let dir = tempfile::tempdir().unwrap();
    n.save(dir.path()).unwrap();
    assert_eq!(NodeIdentity::load(dir.path()).unwrap().cert_sha256(), fp);
}

#[test]
fn names_change_the_certificate_but_not_the_agent_and_stay_admitted() {
    let (n, line) = node();
    let plain = n.cert_sha256();
    let agent = n.fingerprint();
    let named = n.with_names(&["cache.example".into()]).unwrap();
    assert_ne!(named.cert_sha256(), plain);
    assert_eq!(named.fingerprint(), agent);
    verifier(&[&line])
        .verify_peer(&named.certificate().unwrap())
        .expect("named cert is still admitted");
}

#[test]
fn a_delegated_client_chains_to_the_node_and_is_admitted_without_sharing_its_seed() {
    let (n, line) = node();
    let child = n.delegate(metadata("bazel-client")).unwrap();
    assert_ne!(child.fingerprint(), n.fingerprint());
    let der = child.certificate().unwrap();
    verifier(&[&line])
        .verify_peer(&der)
        .expect("child admitted");
    // Not admitted by a verifier that trusts a different operator key.
    let (_, other) = ssh_key();
    verifier(&[&other]).verify_peer(&der).unwrap_err();
    let (child_dir, node_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    n.save(node_dir.path()).unwrap();
    child.save(child_dir.path()).unwrap();
    assert_ne!(
        std::fs::read(child_dir.path().join("agent.seed")).unwrap(),
        std::fs::read(node_dir.path().join("agent.seed")).unwrap()
    );
}
