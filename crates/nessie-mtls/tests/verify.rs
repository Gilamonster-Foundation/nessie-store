//! One rejecting test per acceptance rule (issue #150), plus the accepts.

mod common;

use agent_mesh_protocol::{AgentKey, UserKey};
use common::*;
use nessie_mtls::{MtlsError, PeerProof};
use rcgen::{CertificateParams, CustomExtension, KeyPair};

#[test]
fn accepts_chain_and_binding_signed_by_listed_ssh_key() {
    let p = peer();
    let v = verifier(&[&p.ssh_line]);
    let fp = v.verify_peer(&cert(&p)).expect("admitted");
    assert_eq!(fp, p.agent.fingerprint());
}

#[test]
fn accepts_authorized_keys_line_with_options_comment_and_other_algorithms() {
    let p = peer();
    let text = format!(
        "# comment\n\nssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC bogus\nno-pty,command=\"x\" {} me@host\n",
        p.ssh_line
    );
    let v = nessie_mtls::MeshVerifier::new(nessie_mtls::AuthorizedKeys::parse(&text));
    v.verify_peer(&cert(&p)).expect("admitted");
}

#[test]
fn accepts_delegated_chain_rooted_at_listed_user() {
    let root = peer();
    let child = root.agent.delegate(metadata("child")).unwrap();
    let proof = PeerProof {
        chain: child.cert().clone(),
        binding: root.proof.binding.clone(),
    };
    let der = proof.certificate(&child.signing_key_bytes()).unwrap();
    verifier(&[&root.ssh_line])
        .verify_peer(&der)
        .expect("delegated chain admitted");
}

#[test]
fn rejects_ssh_key_not_listed() {
    let p = peer();
    let (_, other_line) = ssh_key();
    let err = verifier(&[&other_line]).verify_peer(&cert(&p)).unwrap_err();
    assert!(matches!(err, MtlsError::NotAuthorized), "{err}");
}

#[test]
fn rejects_empty_or_unparseable_authorized_keys() {
    let p = peer();
    for text in ["", "garbage\nssh-ed25519 !!!notbase64", "ssh-rsa AAAA x"] {
        let v = nessie_mtls::MeshVerifier::new(nessie_mtls::AuthorizedKeys::parse(text));
        assert!(
            matches!(v.verify_peer(&cert(&p)), Err(MtlsError::NotAuthorized)),
            "{text:?}"
        );
    }
}

#[test]
fn rejects_missing_authorized_keys_file() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nessie_mtls::AuthorizedKeys::load(&dir.path().join("nope"));
    assert!(keys.keys().is_empty());
    let p = peer();
    let v = nessie_mtls::MeshVerifier::new(keys);
    assert!(matches!(
        v.verify_peer(&cert(&p)),
        Err(MtlsError::NotAuthorized)
    ));
}

#[test]
fn rejects_binding_for_a_different_user_key() {
    let p = peer();
    let other_user = UserKey::generate();
    // A binding the listed SSH key really signed — but for someone else's user key.
    let forged = PeerProof {
        chain: chain_of(&p),
        binding: binding(&other_user, &p.ssh),
    };
    let der = forged.certificate(&p.seed).unwrap();
    let err = verifier(&[&p.ssh_line]).verify_peer(&der).unwrap_err();
    assert!(matches!(err, MtlsError::BindingUserMismatch), "{err}");
}

#[test]
fn rejects_chain_whose_agent_key_differs_from_cert_key() {
    let p = peer();
    let other = AgentKey::issue(&p.user, metadata("other"));
    // Cert signed with `other`'s key but carrying `p`'s chain.
    let der = p.proof.certificate(&other.signing_key_bytes()).unwrap();
    let err = verifier(&[&p.ssh_line]).verify_peer(&der).unwrap_err();
    assert!(matches!(err, MtlsError::KeyMismatch), "{err}");
}

#[test]
fn rejects_tampered_chain_signature() {
    let p = peer();
    let mut proof = p.proof.clone();
    proof.chain.metadata.role = "admin".into(); // signed payload changed, sig not
    let der = proof.certificate(&p.seed).unwrap();
    let err = verifier(&[&p.ssh_line]).verify_peer(&der).unwrap_err();
    assert!(matches!(err, MtlsError::BadChain(_)), "{err}");
}

#[test]
fn rejects_non_ed25519_certificate() {
    let p = peer();
    let key = KeyPair::generate().unwrap(); // ECDSA P-256
    let mut params = CertificateParams::new(vec![]).unwrap();
    let json = serde_json::to_vec(&p.proof).unwrap();
    params
        .custom_extensions
        .push(CustomExtension::from_oid_content(
            nessie_mtls::proof::PROOF_OID,
            json,
        ));
    let der = params.self_signed(&key).unwrap();
    let err = verifier(&[&p.ssh_line]).verify_peer(der.der()).unwrap_err();
    assert!(matches!(err, MtlsError::NotEd25519), "{err}");
}

fn cert_with_extensions(p: &Peer, exts: Vec<CustomExtension>) -> Vec<u8> {
    let sk = ed25519_dalek::SigningKey::from_bytes(&p.seed);
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    let der = sk.to_pkcs8_der().unwrap();
    let pk = rustls::pki_types::PrivatePkcs8KeyDer::from(der.as_bytes().to_vec());
    let key = KeyPair::from_pkcs8_der_and_sign_algo(&pk, &rcgen::PKCS_ED25519).unwrap();
    let mut params = CertificateParams::new(vec![]).unwrap();
    params.custom_extensions = exts;
    params.self_signed(&key).unwrap().der().to_vec()
}

#[test]
fn rejects_missing_extension() {
    let p = peer();
    let der = cert_with_extensions(&p, vec![]);
    assert!(matches!(
        verifier(&[&p.ssh_line]).verify_peer(&der),
        Err(MtlsError::BadExtension(_))
    ));
}

#[test]
fn rejects_unparseable_extension() {
    let p = peer();
    let ext =
        CustomExtension::from_oid_content(nessie_mtls::proof::PROOF_OID, b"{not json".to_vec());
    let der = cert_with_extensions(&p, vec![ext]);
    assert!(matches!(
        verifier(&[&p.ssh_line]).verify_peer(&der),
        Err(MtlsError::BadExtension(_))
    ));
}

#[test]
fn rejects_duplicated_extension() {
    let p = peer();
    let json = serde_json::to_vec(&p.proof).unwrap();
    let mk = || CustomExtension::from_oid_content(nessie_mtls::proof::PROOF_OID, json.clone());
    let der = cert_with_extensions(&p, vec![mk(), mk()]);
    let err = verifier(&[&p.ssh_line]).verify_peer(&der).unwrap_err();
    assert!(
        matches!(
            err,
            MtlsError::BadExtension(_) | MtlsError::BadCertificate(_)
        ),
        "{err}"
    );
}

#[test]
fn rejects_oversized_extension() {
    let p = peer();
    let mut json = serde_json::to_vec(&p.proof).unwrap();
    json.extend(std::iter::repeat_n(b' ', nessie_mtls::proof::MAX_PROOF_LEN));
    let ext = CustomExtension::from_oid_content(nessie_mtls::proof::PROOF_OID, json);
    let der = cert_with_extensions(&p, vec![ext]);
    assert!(matches!(
        verifier(&[&p.ssh_line]).verify_peer(&der),
        Err(MtlsError::BadExtension(_))
    ));
}

#[test]
fn rejects_garbage_and_trailing_bytes() {
    let p = peer();
    let v = verifier(&[&p.ssh_line]);
    assert!(matches!(
        v.verify_peer(b"not a cert"),
        Err(MtlsError::BadCertificate(_))
    ));
    let mut der = cert(&p);
    der.push(0);
    assert!(matches!(
        v.verify_peer(&der),
        Err(MtlsError::BadCertificate(_))
    ));
}

#[test]
fn rejects_chain_with_bounded_generation_scope() {
    use agent_mesh_protocol::Scope;
    let user = UserKey::generate();
    let (ssh, line) = ssh_key();
    let mut md = metadata("nessie");
    md.caveats.valid_for_generation = Scope::only([7u64]);
    let agent = AgentKey::issue(&user, md);
    let proof = PeerProof {
        chain: agent.cert().clone(),
        binding: binding(&user, &ssh),
    };
    let der = proof.certificate(&agent.signing_key_bytes()).unwrap();
    let err = verifier(&[&line]).verify_peer(&der).unwrap_err();
    assert!(matches!(err, MtlsError::BadChain(_)), "{err}");
}
