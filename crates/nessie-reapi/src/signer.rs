//! Producing signed attestations for `UpdateActionResult`.
//!
//! The merged core ships only [`SignatureVerifier`](nessie_backend_core::SignatureVerifier)
//! (verify-only); a REAPI face writing an AC entry must *produce* a
//! [`SignedAttestation`]. [`AttestationSigner`] is that seam. [`DevSelfSigner`] fills it
//! for a single-node cache with a deterministic MAC keypair — **not real cryptography**;
//! a real ed25519 signer from agent-mesh replaces it in a swarm. It hands out a matching
//! [`SignatureVerifier`] so the AC backend admits exactly its own attestations.

use nessie_backend_core::{
    Attestation, Digest, Signature, SignatureVerifier, SignedAttestation, SignerId,
    statement_signing_bytes,
};
use std::sync::Arc;

/// Produces a signed self-attestation that an action produced a result.
pub trait AttestationSigner: Send + Sync {
    /// This signer's identity (the distinct signer the k-of-n gate counts).
    fn signer_id(&self) -> SignerId;

    /// Sign the statement "`action` produced `result`" — over
    /// `statement_signing_bytes(action, result)`.
    fn sign_statement(&self, action: &Digest, result: &Digest) -> SignedAttestation;
}

fn mac(secret: &[u8], message: &[u8]) -> Signature {
    let mut buf = Vec::with_capacity(secret.len() + message.len());
    buf.extend_from_slice(secret);
    buf.extend_from_slice(message);
    Signature::from_bytes(Digest::compute(&buf).as_bytes().to_vec())
}

/// A deterministic development signer (a per-name MAC keypair). **Not cryptography** —
/// it proves the write-through path on a single node; a real signer replaces it.
pub struct DevSelfSigner {
    id: SignerId,
    secret: Vec<u8>,
}

impl DevSelfSigner {
    /// A signer deterministically derived from `name`.
    #[must_use]
    pub fn new(name: &str) -> Self {
        let derive = |kind: &str| {
            Digest::compute(format!("nessie-reapi-dev-signer/{kind}/{name}").as_bytes())
                .as_bytes()
                .to_vec()
        };
        Self {
            id: SignerId::from_bytes(derive("id")),
            secret: derive("secret"),
        }
    }

    /// A verifier that admits exactly this signer's attestations. Wire it into the AC
    /// backend so its self-attestations verify.
    #[must_use]
    pub fn verifier(&self) -> Arc<dyn SignatureVerifier> {
        Arc::new(DevVerifier {
            id: self.id.clone(),
            secret: self.secret.clone(),
        })
    }
}

impl AttestationSigner for DevSelfSigner {
    fn signer_id(&self) -> SignerId {
        self.id.clone()
    }

    fn sign_statement(&self, action: &Digest, result: &Digest) -> SignedAttestation {
        SignedAttestation {
            attestation: Attestation {
                signer: self.id.clone(),
                result: result.clone(),
            },
            signature: mac(&self.secret, &statement_signing_bytes(action, result)),
        }
    }
}

struct DevVerifier {
    id: SignerId,
    secret: Vec<u8>,
}

impl SignatureVerifier for DevVerifier {
    fn verify(&self, signer: &SignerId, message: &[u8], signature: &Signature) -> bool {
        signer == &self.id && &mac(&self.secret, message) == signature
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::DigestAlgo;

    #[test]
    fn a_signers_attestation_verifies_under_its_own_verifier() {
        let signer = DevSelfSigner::new("node-a");
        let verifier = signer.verifier();
        let action = Digest::compute_with(DigestAlgo::Sha256, b"action");
        let result = Digest::compute(b"result");
        let signed = signer.sign_statement(&action, &result);
        assert_eq!(signed.attestation.signer, signer.signer_id());

        let message = statement_signing_bytes(&action, &result);
        assert!(verifier.verify(&signed.attestation.signer, &message, &signed.signature));
        // A different action's message does not verify (replay-safe).
        let other = statement_signing_bytes(&Digest::compute(b"other-action"), &result);
        assert!(!verifier.verify(&signed.attestation.signer, &other, &signed.signature));
    }

    #[test]
    fn a_different_signer_is_rejected() {
        let a = DevSelfSigner::new("a");
        let b = DevSelfSigner::new("b");
        let action = Digest::compute(b"x");
        let result = Digest::compute(b"y");
        let signed = a.sign_statement(&action, &result);
        let message = statement_signing_bytes(&action, &result);
        assert!(
            !b.verifier()
                .verify(&signed.attestation.signer, &message, &signed.signature)
        );
    }
}
