//! Attestations and the signature/identity verification seam.
//!
//! An [`Attestation`] is the element of the ActionCache CRDT: a signer asserts
//! that some (externally-keyed) action produced a result, named by its content
//! [`Digest`]. It is deliberately *exactly* `{signer, result}` — the signature
//! lives only on the ingress [`SignedAttestation`] envelope and is verified and
//! discarded from set identity, so the CRDT merge stays a literal set union that
//! refines the machine-checked model (`formal/lean/NessieFormal.lean` `Att`,
//! `formal/tla/AcCrdt.tla` `Attestation == [signer, result]`) with zero drift.
//!
//! Real ed25519 signing and swarm-membership live behind the [`SignatureVerifier`]
//! seam, filled by agent-mesh's signed-peer primitive; this crate defines only
//! the interface and links no crypto.

use crate::digest::Digest;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Opaque signer identity minted by agent-mesh's signed-peer primitive (a public
/// key / self-describing key fingerprint). The core never interprets the bytes —
/// it only tests equality (distinct-signer counting) and orders them (for a
/// deterministic set). Refines the abstract `Signer` in the formal model.
/// Serializes as a lowercase-hex string, keeping wire shapes plain text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignerId(Vec<u8>);

/// Opaque signature bytes over the [`statement_signing_bytes`] message. Real bytes
/// are ed25519 from agent-mesh; the core stores and forwards them but never
/// verifies them itself — that is the [`SignatureVerifier`] seam's job.
/// Serializes as a lowercase-hex string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Signature(Vec<u8>);

macro_rules! hex_byte_newtype {
    ($name:ident, $label:literal) => {
        impl $name {
            #[doc = concat!("Wrap raw ", $label, " bytes.")]
            #[must_use]
            pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
                Self(bytes.into())
            }

            #[doc = concat!("Borrow the raw ", $label, " bytes.")]
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&crate::hex::encode(&self.0))
            }
        }

        impl FromStr for $name {
            type Err = HexIdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                crate::hex::decode(s).map(Self).ok_or(HexIdError)
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&crate::hex::encode(&self.0))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_byte_newtype!(SignerId, "signer-id");
hex_byte_newtype!(Signature, "signature");

/// A hex byte-newtype (`SignerId` / `Signature`) failed to parse from its string
/// form (odd length or a non-hex character).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("value is not valid lowercase hex")]
pub struct HexIdError;

/// The element of the grow-only ActionCache CRDT set: a signer asserts the (single,
/// externally-keyed) action produced `result`. Exactly `{signer, result}` — no
/// signature, no action — so the merge is a literal set union. Refines the formal
/// `Att SignerId Digest` field-for-field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Attestation {
    /// The signing identity.
    pub signer: SignerId,
    /// The attested result, as its canonical content digest.
    pub result: Digest,
}

/// The verified-at-ingress envelope: an [`Attestation`] plus the signature that
/// authenticates it. Carries no `action` — the action is the AC entry key, and the
/// signature is verified against [`statement_signing_bytes`] reconstructed from
/// that key. The inner [`Attestation`] (sans signature) is what enters the set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAttestation {
    /// The formal element `{signer, result}` that, once verified, is inserted.
    pub attestation: Attestation,
    /// Opaque signature over `statement_signing_bytes(action, attestation.result)`.
    pub signature: Signature,
}

/// Domain tag binding a signed statement to this scheme and version.
const STMT_DOMAIN: &[u8] = b"nessie.Attestation.v1\x00";

/// The exact message a signer signs to attest that `action` produced `result`.
///
/// Domain-separated and binding *both* the action and the result via their
/// self-describing multihash bytes, so a signature minted for `(A, R)` cannot be
/// replayed under a different action or result. Reconstructed from the action key
/// at verify time — a wrong action argument simply fails verification (safe, never
/// silently wrong). A frozen wire contract.
#[must_use]
pub fn statement_signing_bytes(action: &Digest, result: &Digest) -> Vec<u8> {
    let mut msg = Vec::with_capacity(STMT_DOMAIN.len() + 68);
    msg.extend_from_slice(STMT_DOMAIN);
    msg.extend_from_slice(&action.to_multihash_bytes());
    msg.extend_from_slice(&result.to_multihash_bytes());
    msg
}

/// The signature/identity verification seam. Real ed25519 and swarm-membership
/// live *behind* this trait, in agent-mesh; the core defines only the interface,
/// so `nessie-backend-core` builds with no crypto crate and no network.
///
/// [`SignatureVerifier::verify`] returning `true` means, as one predicate, that
/// `signer` is an **admitted, sybil-resisted swarm member** *and* the signature
/// authenticates `message` under that member's key — folding the two
/// responsibilities the formal assumptions delegate to agent-mesh. The
/// ActionCache counts a distinct signer toward its k-of-n threshold only for
/// attestations this returns `true` on.
pub trait SignatureVerifier: Send + Sync {
    /// `true` iff `signer` is an admitted member and `signature` authenticates
    /// `message` under that member's key.
    fn verify(&self, signer: &SignerId, message: &[u8], signature: &Signature) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signer_id_hex_roundtrips_through_display_and_fromstr() {
        let s = SignerId::from_bytes(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(s.to_string(), "deadbeef");
        assert_eq!("deadbeef".parse::<SignerId>().unwrap(), s);
    }

    #[test]
    fn signer_id_serde_is_a_plain_hex_string() {
        let s = SignerId::from_bytes(vec![0x01, 0x02]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"0102\"");
        assert_eq!(serde_json::from_str::<SignerId>(&json).unwrap(), s);
    }

    #[test]
    fn attestation_serde_roundtrips() {
        let a = Attestation {
            signer: SignerId::from_bytes(vec![1, 2, 3]),
            result: Digest::compute(b"r"),
        };
        let back: Attestation = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn signing_bytes_bind_action_and_result() {
        let a1 = Digest::compute(b"action-1");
        let a2 = Digest::compute(b"action-2");
        let r1 = Digest::compute(b"result-1");
        let r2 = Digest::compute(b"result-2");
        // Different action OR different result => different message (no replay).
        assert_ne!(
            statement_signing_bytes(&a1, &r1),
            statement_signing_bytes(&a2, &r1)
        );
        assert_ne!(
            statement_signing_bytes(&a1, &r1),
            statement_signing_bytes(&a1, &r2)
        );
        // Same (action, result) => stable message.
        assert_eq!(
            statement_signing_bytes(&a1, &r1),
            statement_signing_bytes(&a1, &r1)
        );
    }
}
