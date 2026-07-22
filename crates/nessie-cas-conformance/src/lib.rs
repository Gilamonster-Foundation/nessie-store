//! Generic conformance suite for [`CasBackend`] implementations.
//!
//! Any content-addressed backend — the in-memory reference, an S3-backed store, a
//! ZFS blob store — must behave identically at the contract level: `put` names a
//! blob by its content digest, `get` returns exactly those bytes (verified), and
//! `has` tracks presence. This harness asserts that behaviour so every substrate
//! validates against one shared definition of "a CAS", exactly as
//! `nessie-backend-conformance` does for the volume trait stack.
//!
//! Suites panic with a descriptive message on the first violation, so call
//! [`run_all`] from a `#[test]` in the backend crate:
//!
//! ```no_run
//! # use nessie_backend_core::CasBackend;
//! # fn make_cas() -> Box<dyn CasBackend> { unimplemented!() }
//! let cas = make_cas();
//! nessie_cas_conformance::run_all(cas.as_ref());
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use nessie_backend_core::{BackendError, CasBackend, Digest, DigestAlgo, VolumeUuid};
use std::io::{Cursor, Read};

/// Distinct blob content per assertion, so suites never collide with prior state
/// on a persistent substrate (content addressing means identical bytes would
/// otherwise alias the same blob across runs).
fn unique_bytes(tag: &str) -> Vec<u8> {
    format!("nessie-cas-conformance/{tag}/{}", VolumeUuid::new()).into_bytes()
}

fn store(cas: &dyn CasBackend, bytes: &[u8]) -> Digest {
    cas.put(&mut Cursor::new(bytes.to_vec()))
        .expect("put should succeed")
}

fn fetch(cas: &dyn CasBackend, digest: &Digest) -> Vec<u8> {
    let mut out = Vec::new();
    cas.get(digest)
        .expect("get should succeed for a present blob")
        .read_to_end(&mut out)
        .expect("reading the blob stream should succeed");
    out
}

/// Run every [`CasBackend`] conformance suite against `cas`. Panics on the first
/// violation with a message naming the broken invariant.
pub fn run_all(cas: &dyn CasBackend) {
    put_returns_the_content_digest(cas);
    absent_before_put_present_after(cas);
    get_returns_the_exact_verified_bytes(cas);
    put_is_idempotent(cas);
    get_on_an_absent_digest_errors(cas);
    distinct_blobs_coexist(cas);
    the_empty_blob_roundtrips(cas);
    reclaimable_round_trip_if_supported(cas);
    size_reports_stored_bytes(cas);
    put_keyed_and_sha256_native_if_supported(cas);
}

/// `size` reports a stored blob's byte length and `None` for an absent digest.
fn size_reports_stored_bytes(cas: &dyn CasBackend) {
    let bytes = unique_bytes("size");
    let digest = store(cas, &bytes);
    assert_eq!(
        cas.size(&digest).expect("size should succeed"),
        Some(bytes.len() as u64),
        "size reports the stored byte length"
    );
    let absent = Digest::compute(&unique_bytes("size-absent"));
    assert_eq!(
        cas.size(&absent).expect("size should succeed"),
        None,
        "size of an absent blob is None"
    );
}

/// If keyed storage is supported, `put_keyed` verifies the bytes against the
/// caller's digest, is idempotent, rejects a mismatch, and round-trips a
/// **SHA-256-keyed** blob (the REAPI-native path) through has/get/size.
fn put_keyed_and_sha256_native_if_supported(cas: &dyn CasBackend) {
    let bytes = unique_bytes("put-keyed");
    let digest = Digest::compute(&bytes);
    match cas.put_keyed(&digest, &mut Cursor::new(bytes.clone())) {
        Err(BackendError::FeatureNotSupported { .. }) => return, // backend declines; skip
        Err(e) => panic!("put_keyed returned an unexpected error: {e}"),
        Ok(()) => {}
    }
    assert!(cas.has(&digest).expect("has"), "keyed blob is present");
    assert_eq!(fetch(cas, &digest), bytes, "keyed blob reads back exactly");
    // Idempotent re-put.
    cas.put_keyed(&digest, &mut Cursor::new(bytes.clone()))
        .expect("re-put is idempotent");
    // A digest the bytes do not hash to is rejected, nothing stored.
    let wrong = Digest::compute(&unique_bytes("put-keyed-wrong"));
    assert!(
        matches!(
            cas.put_keyed(&wrong, &mut Cursor::new(bytes.clone())),
            Err(BackendError::InvalidArgument(_))
        ),
        "a digest mismatch is rejected"
    );
    assert!(
        !cas.has(&wrong).expect("has"),
        "a rejected keyed put stores nothing"
    );

    // The REAPI-native path: store under a SHA-256 digest (not the BLAKE3 default).
    let native = unique_bytes("sha256-native");
    let sha = Digest::compute_with(DigestAlgo::Sha256, &native);
    cas.put_keyed(&sha, &mut Cursor::new(native.clone()))
        .expect("sha256-keyed put");
    assert_eq!(sha.algo(), DigestAlgo::Sha256);
    assert!(cas.has(&sha).expect("has"));
    assert_eq!(cas.size(&sha).expect("size"), Some(native.len() as u64));
    let out = fetch(cas, &sha);
    assert_eq!(out, native);
    assert!(
        sha.verify(&out),
        "sha256-keyed bytes verify against their digest"
    );
}

/// If the backend advertises the reclaimable tier (`as_reclaimable() == Some`),
/// `iter_local` reflects stored blobs with correct sizes and `reclaim` removes a
/// blob idempotently. Backends without the tier skip this suite by design.
fn reclaimable_round_trip_if_supported(cas: &dyn CasBackend) {
    let Some(rec) = cas.as_reclaimable() else {
        return;
    };
    let bytes = unique_bytes("reclaimable");
    let digest = rec
        .put(&mut Cursor::new(bytes.clone()))
        .expect("put should succeed");

    let local = rec.iter_local().expect("iter_local should succeed");
    let entry = local
        .iter()
        .find(|b| b.digest == digest)
        .expect("a just-put blob must be enumerated by iter_local");
    assert_eq!(
        entry.size_bytes,
        bytes.len() as u64,
        "iter_local must report the exact local byte size"
    );

    assert!(
        rec.reclaim(&digest).expect("reclaim should succeed"),
        "reclaim of a present blob returns true"
    );
    assert!(
        !rec.has(&digest).expect("has should succeed"),
        "a reclaimed blob is gone"
    );
    assert!(
        !rec.reclaim(&digest).expect("reclaim should succeed"),
        "reclaim is idempotent: an absent blob returns false"
    );
}

/// `put` must return the digest *computed from the content*, not an arbitrary id.
fn put_returns_the_content_digest(cas: &dyn CasBackend) {
    let bytes = unique_bytes("put-digest");
    let got = store(cas, &bytes);
    assert_eq!(
        got,
        Digest::compute(&bytes),
        "put must return the content digest of the stored bytes"
    );
}

/// A blob is absent before it is put and present after.
fn absent_before_put_present_after(cas: &dyn CasBackend) {
    let bytes = unique_bytes("presence");
    let digest = Digest::compute(&bytes);
    assert!(
        !cas.has(&digest).expect("has should succeed"),
        "a never-stored blob must be absent"
    );
    let stored = store(cas, &bytes);
    assert_eq!(stored, digest);
    assert!(
        cas.has(&digest).expect("has should succeed"),
        "a stored blob must be present"
    );
}

/// `get` returns exactly the bytes that were stored, and they verify against the
/// digest (the tamper-proof-read guarantee).
fn get_returns_the_exact_verified_bytes(cas: &dyn CasBackend) {
    let bytes = unique_bytes("roundtrip");
    let digest = store(cas, &bytes);
    let out = fetch(cas, &digest);
    assert_eq!(out, bytes, "get must return the exact stored bytes");
    assert!(
        digest.verify(&out),
        "returned bytes must verify against their digest"
    );
}

/// Storing identical bytes twice yields the same digest and leaves the blob
/// present — `put` is idempotent (dedup).
fn put_is_idempotent(cas: &dyn CasBackend) {
    let bytes = unique_bytes("idempotent");
    let first = store(cas, &bytes);
    let second = store(cas, &bytes);
    assert_eq!(
        first, second,
        "putting identical bytes twice must yield the same digest"
    );
    assert!(
        cas.has(&first).expect("has should succeed"),
        "the blob must remain present after a repeat put"
    );
}

/// `get` on a digest that was never stored must error, not return empty bytes.
fn get_on_an_absent_digest_errors(cas: &dyn CasBackend) {
    let digest = Digest::compute(&unique_bytes("never-stored"));
    assert!(
        cas.get(&digest).is_err(),
        "get on an absent digest must error"
    );
}

/// Distinct contents get distinct digests and are independently retrievable.
fn distinct_blobs_coexist(cas: &dyn CasBackend) {
    let a = unique_bytes("coexist-a");
    let b = unique_bytes("coexist-b");
    let da = store(cas, &a);
    let db = store(cas, &b);
    assert_ne!(da, db, "distinct contents must have distinct digests");
    assert_eq!(fetch(cas, &da), a);
    assert_eq!(fetch(cas, &db), b);
}

/// The empty blob is a valid, addressable blob.
fn the_empty_blob_roundtrips(cas: &dyn CasBackend) {
    let empty: Vec<u8> = Vec::new();
    let digest = store(cas, &empty);
    assert_eq!(digest, Digest::compute(&empty));
    assert!(
        cas.has(&digest).expect("has should succeed"),
        "the empty blob must be present after put"
    );
    assert!(
        fetch(cas, &digest).is_empty(),
        "the empty blob must read back as zero bytes"
    );
}
