//! The ActionCache tier: a supertrait of [`CasBackend`].
//!
//! Not a blind register — signed attestations accumulate into a grow-only CRDT
//! ([`AttestationSet`](crate::AttestationSet)) and a result is returned only when
//! **confirmed** at k distinct signers. This is the ungameable-completion keystone:
//! "did the agent complete action X?" becomes "is there a confirmed AC entry for
//! `digest(Action_X)` whose result verifies?", and the Action digest pins the
//! immutable spec, so it cannot be faked by weakening the spec.

use crate::action_result::ActionResult;
use crate::attestation::SignedAttestation;
use crate::cas::CasBackend;
use crate::digest::Digest;
use crate::error::BackendError;
use std::num::NonZeroUsize;

/// A [`CasBackend`] that also records and resolves signed action→result
/// attestations. Being a **supertrait** of `CasBackend` is load-bearing:
/// `get_action_result` materializes the confirmed [`ActionResult`] body through
/// `self.get`, and an attester first `self.put`s that body — the AC keys the CAS.
///
/// Reached from a `CasBackend` via [`CasBackend::as_action_cache`]. Supertrait
/// upcasting (`&dyn ActionCacheBackend` → `&dyn CasBackend`) is relied upon
/// (stable on MSRV 1.88, as the volume stack already relies on it).
pub trait ActionCacheBackend: CasBackend {
    /// The k-of-n confirmation threshold this node enforces (refines the formal
    /// `K`). `NonZeroUsize` makes the catastrophic `k = 0` — which would "confirm"
    /// every result — unrepresentable.
    fn confirmation_threshold(&self) -> NonZeroUsize;

    /// The **confirmed** result for `action`, materialized from CAS. Refines the
    /// formal `resolve(k)`; maps to REAPI `GetActionResult`.
    ///
    /// - `Ok(Some(result))` — exactly one result reached k signers (confirmed).
    /// - `Ok(None)` — no result reached k (a cache miss).
    ///
    /// # Errors
    ///
    /// - [`BackendError::ActionResultConflict`] — ≥ 2 results each reached k
    ///   (only outside the Byzantine-minority hypothesis, or under
    ///   non-determinism); surfaced, never silently resolved to one side.
    /// - [`BackendError::BlobNotFound`] — confirmed, but the result body is not
    ///   held locally yet (cache mode); re-fetch it from the swarm by digest. A
    ///   caller must **not** read this as "unconfirmed".
    /// - [`BackendError::InvalidArgument`] — the stored result body failed to
    ///   decode as a canonical `ActionResult`.
    fn get_action_result(&self, action: &Digest) -> Result<Option<ActionResult>, BackendError>;

    /// Verify one signed attestation through the
    /// [`SignatureVerifier`](crate::SignatureVerifier) seam — against
    /// `statement_signing_bytes(action, signed.attestation.result)` — then, only if
    /// it verifies, merge its `{signer, result}` element into `action`'s grow-only
    /// set. Refines TLA+ `Attest` (verified) composed with the grow-only insert;
    /// maps to REAPI `UpdateActionResult` (a plain update = a `k = 1` self-attest).
    ///
    /// The `ActionResult` body named by the attestation must already be in CAS
    /// (`self.put`) for a later `get_action_result` to materialize it.
    ///
    /// # Errors
    ///
    /// - [`BackendError::AttestationUnverified`] — the seam rejected the signature
    ///   or the signer is not an admitted member; the set is left unchanged, so an
    ///   unverified attestation never counts toward k.
    fn attest_action_result(
        &self,
        action: &Digest,
        signed: SignedAttestation,
    ) -> Result<(), BackendError>;
}
