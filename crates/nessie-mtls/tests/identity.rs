//! Node identity on disk: issue, load, permissions, no seed leakage.

mod common;

use agent_mesh_protocol::UserKey;
use common::*;
use nessie_mtls::{MtlsError, NodeIdentity};
use std::os::unix::fs::PermissionsExt;

fn issued() -> (tempfile::TempDir, NodeIdentity, String) {
    let user = UserKey::generate();
    let (ssh, line) = ssh_key();
    let node = NodeIdentity::issue(&user, binding(&user, &ssh), metadata("nessie")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    node.save(dir.path()).unwrap();
    (dir, node, line)
}

#[test]
fn save_then_load_round_trips_and_certificate_is_admitted() {
    let (dir, node, line) = issued();
    let mode = std::fs::metadata(dir.path().join("agent.seed"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let loaded = NodeIdentity::load(dir.path()).unwrap();
    assert_eq!(loaded.fingerprint(), node.fingerprint());
    let der = loaded.certificate().unwrap();
    verifier(&[&line])
        .verify_peer(&der)
        .expect("own cert admitted");
}

#[test]
fn issue_refuses_binding_for_another_user_key() {
    let user = UserKey::generate();
    let other = UserKey::generate();
    let (ssh, _) = ssh_key();
    let err = NodeIdentity::issue(&user, binding(&other, &ssh), metadata("x")).unwrap_err();
    assert!(matches!(err, MtlsError::BindingUserMismatch), "{err}");
}

#[test]
fn load_refuses_group_or_world_readable_seed() {
    for mode in [0o640, 0o604, 0o644, 0o660] {
        let (dir, _, _) = issued();
        let seed = dir.path().join("agent.seed");
        std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = NodeIdentity::load(dir.path()).unwrap_err();
        assert!(
            matches!(err, MtlsError::SeedPermissions(..)),
            "{mode:o}: {err}"
        );
    }
}

#[test]
fn load_refuses_wrong_length_seed_and_missing_files() {
    let (dir, _, _) = issued();
    std::fs::write(dir.path().join("agent.seed"), b"short").unwrap();
    std::fs::set_permissions(
        dir.path().join("agent.seed"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(matches!(
        NodeIdentity::load(dir.path()),
        Err(MtlsError::SeedLength(_))
    ));
    let empty = tempfile::tempdir().unwrap();
    assert!(NodeIdentity::load(empty.path()).is_err());
}

#[test]
fn load_refuses_seed_that_does_not_match_chain() {
    let (dir, _, _) = issued();
    let (other_dir, _, _) = issued();
    std::fs::copy(
        other_dir.path().join("agent.seed"),
        dir.path().join("agent.seed"),
    )
    .unwrap();
    assert!(matches!(
        NodeIdentity::load(dir.path()),
        Err(MtlsError::Mesh(_))
    ));
}

#[test]
fn save_refuses_to_overwrite_existing_seed() {
    let (dir, node, _) = issued();
    assert!(node.save(dir.path()).is_err());
}

#[test]
fn seed_never_appears_in_debug_or_errors() {
    let (dir, node, _) = issued();
    let seed = std::fs::read(dir.path().join("agent.seed")).unwrap();
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!format!("{node:?}").contains(&hex));
    let seed_path = dir.path().join("agent.seed");
    std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = NodeIdentity::load(dir.path()).unwrap_err().to_string();
    assert!(!err.contains(&hex));
}

#[test]
fn pem_accessors_and_certified_key_are_usable() {
    let (_dir, node, _) = issued();
    assert!(node.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(
        node.key_pem()
            .unwrap()
            .starts_with("-----BEGIN PRIVATE KEY-----")
    );
    assert_eq!(node.node_certified_key().unwrap().cert.len(), 1);
}
