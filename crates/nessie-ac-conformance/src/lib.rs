//! Generic conformance suite for [`ActionCacheBackend`] implementations, plus the
//! test doubles ([`TestKeyring`]/[`TestSigner`]) that drive it.
//!
//! Any AC backend — the in-memory reference, a future swarm-gossiping one — must
//! behave identically at the contract level: signed attestations accumulate into a
//! grow-only CRDT, a result is returned only when **confirmed** at k distinct
//! admitted signers, an unverified attestation never counts, and a genuine
//! disagreement is surfaced rather than silently resolved. Each property refines a
//! machine-checked obligation in `formal/` — see the per-suite doc comments.
//!
//! [`run_all`] takes a **factory** (not a bare backend) because the suite must
//! build backends at several thresholds `k` and mint matching-verifier
//! attestations to exercise the k-boundary and Byzantine-cohort cases.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::num::NonZeroUsize;
//! # use nessie_backend_core::{ActionCacheBackend, SignatureVerifier};
//! # fn make(_v: Arc<dyn SignatureVerifier>, _k: NonZeroUsize) -> Box<dyn ActionCacheBackend> { unimplemented!() }
//! nessie_ac_conformance::run_all(&make);
//! ```
//!
//! **The [`TestKeyring`] "signature" is a per-signer MAC, not real cryptography.**
//! It proves the seam and the gate logic — never unforgeability. Real security is
//! agent-mesh's ed25519 + sybil-resistant membership. Never wire this into a daemon.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use nessie_backend_core::{
    ActionCacheBackend, ActionResult, Attestation, BackendError, Digest, OutputFile, Signature,
    SignatureVerifier, SignedAttestation, SignerId, statement_signing_bytes,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// Builds an [`ActionCacheBackend`] enforcing threshold `k` and verifying against
/// `verifier`. The suite calls this repeatedly to get fresh, independent backends.
pub type BackendFactory<'a> =
    dyn Fn(Arc<dyn SignatureVerifier>, NonZeroUsize) -> Box<dyn ActionCacheBackend> + 'a;

// --- Test doubles: a deterministic MAC keyring with a membership roster. ---

fn mac(secret: &[u8], message: &[u8]) -> Signature {
    let mut buf = Vec::with_capacity(secret.len() + message.len());
    buf.extend_from_slice(secret);
    buf.extend_from_slice(message);
    Signature::from_bytes(Digest::compute(&buf).as_bytes().to_vec())
}

fn derive(kind: &str, name: &str) -> Vec<u8> {
    Digest::compute(format!("nessie-ac-conformance/{kind}/{name}").as_bytes())
        .as_bytes()
        .to_vec()
}

/// A test signer: a stable [`SignerId`] and the secret its MAC "signatures" use.
/// Deterministic in its name, so a signer re-minted with the same name is the same.
pub struct TestSigner {
    id: SignerId,
    secret: Vec<u8>,
}

impl TestSigner {
    fn from_name(name: &str) -> Self {
        Self {
            id: SignerId::from_bytes(derive("id", name)),
            secret: derive("secret", name),
        }
    }

    /// This signer's identity.
    #[must_use]
    pub fn id(&self) -> &SignerId {
        &self.id
    }

    /// A [`SignedAttestation`] that `action` produced `result`, signed over
    /// `statement_signing_bytes(action, result)` with this signer's MAC key.
    #[must_use]
    pub fn attest(&self, action: &Digest, result: &Digest) -> SignedAttestation {
        SignedAttestation {
            attestation: Attestation {
                signer: self.id.clone(),
                result: result.clone(),
            },
            signature: mac(&self.secret, &statement_signing_bytes(action, result)),
        }
    }

    /// Sign a *different* statement than the attestation claims — a forged
    /// signature the verifier must reject. `claimed` is put on the attestation;
    /// the signature is over `signed_over` instead.
    #[must_use]
    pub fn attest_forged(
        &self,
        action: &Digest,
        claimed: &Digest,
        signed_over: &Digest,
    ) -> SignedAttestation {
        SignedAttestation {
            attestation: Attestation {
                signer: self.id.clone(),
                result: claimed.clone(),
            },
            signature: mac(&self.secret, &statement_signing_bytes(action, signed_over)),
        }
    }
}

/// A roster of admitted test signers. Its [`TestKeyring::verifier`] accepts a MAC
/// signature only from a signer that was [`admit`](TestKeyring::admit)ted.
#[derive(Default)]
pub struct TestKeyring {
    secrets: BTreeMap<SignerId, Vec<u8>>,
}

impl TestKeyring {
    /// An empty roster.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a signer named `name` **and admit it** to the roster.
    pub fn admit(&mut self, name: &str) -> TestSigner {
        let s = TestSigner::from_name(name);
        self.secrets.insert(s.id.clone(), s.secret.clone());
        s
    }

    /// Mint a signer named `name` **without** admitting it — a non-member whose
    /// attestations the verifier must reject.
    #[must_use]
    pub fn outsider(&self, name: &str) -> TestSigner {
        TestSigner::from_name(name)
    }

    /// A verifier that authenticates the MAC and enforces roster membership.
    #[must_use]
    pub fn verifier(&self) -> Arc<dyn SignatureVerifier> {
        Arc::new(MacVerifier {
            secrets: self.secrets.clone(),
        })
    }
}

struct MacVerifier {
    secrets: BTreeMap<SignerId, Vec<u8>>,
}

impl SignatureVerifier for MacVerifier {
    fn verify(&self, signer: &SignerId, message: &[u8], signature: &Signature) -> bool {
        match self.secrets.get(signer) {
            None => false, // not an admitted member
            Some(secret) => &mac(secret, message) == signature,
        }
    }
}

// --- Suite helpers ---

fn k(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("k > 0")
}

/// A distinct sample result per tag (a stdout digest is enough to differ).
fn result(tag: &[u8]) -> ActionResult {
    let mut outputs = BTreeMap::new();
    outputs.insert(
        "out".to_string(),
        OutputFile {
            digest: Digest::compute(tag),
            is_executable: false,
        },
    );
    ActionResult {
        outputs,
        exit_code: 0,
        stdout_digest: Some(Digest::compute(tag)),
        stderr_digest: None,
    }
}

/// Store the result body in CAS (idempotent) then submit `signer`'s attestation.
fn deposit_and_attest(
    backend: &dyn ActionCacheBackend,
    signer: &TestSigner,
    action: &Digest,
    r: &ActionResult,
) {
    backend
        .put(&mut Cursor::new(r.to_canonical_bytes()))
        .expect("put result body");
    backend
        .attest_action_result(action, signer.attest(action, &r.result_digest()))
        .expect("attest");
}

// --- The suite ---

/// Run every [`ActionCacheBackend`] conformance suite against backends built by
/// `make`. Panics on the first violation with a message naming the invariant.
pub fn run_all(make: &BackendFactory<'_>) {
    it_is_also_a_cas(make);
    unknown_action_is_a_clean_miss(make);
    k_of_n_confirms_the_keystone(make);
    below_threshold_is_unconfirmed(make);
    duplicate_signer_does_not_count(make);
    attest_order_is_irrelevant(make);
    redelivery_is_idempotent(make);
    confirmation_is_monotone(make);
    unverified_attestation_is_rejected(make);
    action_key_isolates_the_spec(make);
    distinct_results_do_not_cross_count(make);
    conflict_is_surfaced_not_silently_wrong(make);
    byzantine_cohort_of_k_forges_the_boundary(make);
}

/// The AC tier must not regress the CAS contract of its `CasBackend` supertrait.
fn it_is_also_a_cas(make: &BackendFactory<'_>) {
    let keyring = TestKeyring::new();
    let backend = make(keyring.verifier(), k(1));
    nessie_cas_conformance::run_all(backend.as_ref());
}

/// An action with zero attestations is a clean miss (`Ok(None)`), never an error.
fn unknown_action_is_a_clean_miss(make: &BackendFactory<'_>) {
    let keyring = TestKeyring::new();
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"unknown-action");
    assert_eq!(backend.get_action_result(&action).expect("get"), None);
}

/// k distinct valid signers confirm — and the confirmed body is materialized
/// byte-for-byte from CAS (the ungameable-completion keystone).
fn k_of_n_confirms_the_keystone(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let s2 = keyring.admit("s2");
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"action-X");
    let r = result(b"R");
    deposit_and_attest(backend.as_ref(), &s1, &action, &r);
    assert_eq!(
        backend.get_action_result(&action).expect("get"),
        None,
        "one signer is below k=2"
    );
    deposit_and_attest(backend.as_ref(), &s2, &action, &r);
    assert_eq!(
        backend.get_action_result(&action).expect("get"),
        Some(r),
        "two distinct signers confirm, and the body materializes from CAS"
    );
}

/// k-1 distinct signers is a miss — the gate is exact (`>= k`).
fn below_threshold_is_unconfirmed(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let s2 = keyring.admit("s2");
    let backend = make(keyring.verifier(), k(3));
    let action = Digest::compute(b"action");
    let r = result(b"R");
    deposit_and_attest(backend.as_ref(), &s1, &action, &r);
    deposit_and_attest(backend.as_ref(), &s2, &action, &r);
    assert_eq!(backend.get_action_result(&action).expect("get"), None);
}

/// One signer attesting k times must not confirm — distinct signers count.
fn duplicate_signer_does_not_count(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"action");
    let r = result(b"R");
    deposit_and_attest(backend.as_ref(), &s1, &action, &r);
    deposit_and_attest(backend.as_ref(), &s1, &action, &r); // same signer again
    assert_eq!(backend.get_action_result(&action).expect("get"), None);
}

/// Any order of the same attestations yields the same confirmation.
fn attest_order_is_irrelevant(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let s2 = keyring.admit("s2");
    let action = Digest::compute(b"action");
    let r = result(b"R");

    let b1 = make(keyring.verifier(), k(2));
    deposit_and_attest(b1.as_ref(), &s1, &action, &r);
    deposit_and_attest(b1.as_ref(), &s2, &action, &r);

    let b2 = make(keyring.verifier(), k(2));
    deposit_and_attest(b2.as_ref(), &s2, &action, &r); // reversed order
    deposit_and_attest(b2.as_ref(), &s1, &action, &r);

    assert_eq!(
        b1.get_action_result(&action).expect("get"),
        b2.get_action_result(&action).expect("get")
    );
}

/// Re-delivering the identical attestation is harmless (idempotent).
fn redelivery_is_idempotent(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let backend = make(keyring.verifier(), k(1));
    let action = Digest::compute(b"action");
    let r = result(b"R");
    deposit_and_attest(backend.as_ref(), &s1, &action, &r);
    let once = backend.get_action_result(&action).expect("get");
    deposit_and_attest(backend.as_ref(), &s1, &action, &r); // exact re-delivery
    assert_eq!(backend.get_action_result(&action).expect("get"), once);
    assert_eq!(once, Some(r));
}

/// Once confirmed, further valid attestations never un-confirm or switch the result.
fn confirmation_is_monotone(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let s2 = keyring.admit("s2");
    let s3 = keyring.admit("s3");
    let s4 = keyring.admit("s4");
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"action");
    let r = result(b"R");
    deposit_and_attest(backend.as_ref(), &s1, &action, &r);
    deposit_and_attest(backend.as_ref(), &s2, &action, &r);
    assert_eq!(
        backend.get_action_result(&action).expect("get"),
        Some(r.clone())
    );
    // Add a redundant signer for R and a lone signer for a different result
    // (below k), then re-check: still the same confirmed R.
    deposit_and_attest(backend.as_ref(), &s3, &action, &r);
    deposit_and_attest(backend.as_ref(), &s4, &action, &result(b"other"));
    assert_eq!(backend.get_action_result(&action).expect("get"), Some(r));
}

/// An attestation that fails the verifier is rejected and never counts.
fn unverified_attestation_is_rejected(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let outsider = keyring.outsider("intruder"); // never admitted
    let backend = make(keyring.verifier(), k(1));
    let action = Digest::compute(b"action");
    let r = result(b"R");
    backend
        .put(&mut Cursor::new(r.to_canonical_bytes()))
        .expect("put");

    // (a) Unknown (non-member) signer → rejected.
    let err = backend
        .attest_action_result(&action, outsider.attest(&action, &r.result_digest()))
        .unwrap_err();
    assert!(matches!(err, BackendError::AttestationUnverified { .. }));

    // (b) Admitted signer but a forged signature (signed over a different result).
    let err = backend
        .attest_action_result(
            &action,
            s1.attest_forged(&action, &r.result_digest(), &Digest::compute(b"elsewhere")),
        )
        .unwrap_err();
    assert!(matches!(err, BackendError::AttestationUnverified { .. }));

    // Neither counted: still a miss.
    assert_eq!(backend.get_action_result(&action).expect("get"), None);
}

/// The Action digest is the sole key: attestations under A never confirm B, and a
/// signature minted for A does not verify when replayed under B.
fn action_key_isolates_the_spec(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let backend = make(keyring.verifier(), k(1));
    let action_a = Digest::compute(b"action-A");
    let action_b = Digest::compute(b"action-B");
    let r = result(b"R");
    deposit_and_attest(backend.as_ref(), &s1, &action_a, &r);
    assert_eq!(
        backend.get_action_result(&action_a).expect("get"),
        Some(r.clone())
    );
    assert_eq!(
        backend.get_action_result(&action_b).expect("get"),
        None,
        "an attestation under A must never confirm B"
    );
    // Replay A's signature under B: the backend verifies against
    // statement_signing_bytes(B, R), so A's signature fails.
    let a_sig = s1.attest(&action_a, &r.result_digest());
    let err = backend.attest_action_result(&action_b, a_sig).unwrap_err();
    assert!(matches!(err, BackendError::AttestationUnverified { .. }));
}

/// Signers split across two results: neither confirms unless k agree on ONE.
fn distinct_results_do_not_cross_count(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let s2 = keyring.admit("s2");
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"action");
    deposit_and_attest(backend.as_ref(), &s1, &action, &result(b"good"));
    deposit_and_attest(backend.as_ref(), &s2, &action, &result(b"evil"));
    assert_eq!(
        backend.get_action_result(&action).expect("get"),
        None,
        "one signer each on two results confirms neither"
    );
}

/// Two results each reaching k are surfaced as a conflict error, never a silent
/// pick or miss (only reachable when the minority hypothesis is violated).
fn conflict_is_surfaced_not_silently_wrong(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let s1 = keyring.admit("s1");
    let s2 = keyring.admit("s2");
    let s3 = keyring.admit("s3");
    let s4 = keyring.admit("s4");
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"action");
    deposit_and_attest(backend.as_ref(), &s1, &action, &result(b"good"));
    deposit_and_attest(backend.as_ref(), &s2, &action, &result(b"good"));
    deposit_and_attest(backend.as_ref(), &s3, &action, &result(b"evil"));
    deposit_and_attest(backend.as_ref(), &s4, &action, &result(b"evil"));
    let err = backend.get_action_result(&action).unwrap_err();
    assert!(matches!(err, BackendError::ActionResultConflict { .. }));
}

/// k colluding valid signers on one false result DO confirm it — documenting that
/// k signatures is exactly the security boundary (the Rust analog of PO-AC-B).
fn byzantine_cohort_of_k_forges_the_boundary(make: &BackendFactory<'_>) {
    let mut keyring = TestKeyring::new();
    let b1 = keyring.admit("byz1");
    let b2 = keyring.admit("byz2");
    let backend = make(keyring.verifier(), k(2));
    let action = Digest::compute(b"action");
    let forged = result(b"false-but-agreed");
    deposit_and_attest(backend.as_ref(), &b1, &action, &forged);
    deposit_and_attest(backend.as_ref(), &b2, &action, &forged);
    assert_eq!(
        backend.get_action_result(&action).expect("get"),
        Some(forged),
        "|Byzantine| = k forges: the minority hypothesis is load-bearing"
    );
}
