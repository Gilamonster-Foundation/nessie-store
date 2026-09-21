//! End-to-end round trip through the S3 trait, against the reference CAS.
//!
//! Exercises the face the way bazel-remote's S3 backend drives it — PutObject,
//! HeadObject (its `Contains` probe), GetObject — plus the two refusals that keep
//! the store content-addressed, and the one error code that decides whether a
//! cache miss is a miss or a failed build.

use bytes::Bytes;
use futures::TryStreamExt;
use http::{Extensions, HeaderMap, Method, Uri};
use nessie_backend_mem::MemCas;
use nessie_s3::NessieS3;
use s3s::S3;
use s3s::dto::{GetObjectInput, HeadObjectInput, PutObjectInput, StreamingBlob};
use s3s::{S3ErrorCode, S3Request};
use std::sync::Arc;

/// SHA-256 of `BLOB`, as a cache client would compute it before choosing the key.
const BLOB: &[u8] = b"the bytes a build action produced";

fn sha256_hex(data: &[u8]) -> String {
    use nessie_backend_core::{Digest, DigestAlgo};
    Digest::compute_with(DigestAlgo::Sha256, data)
        .to_string()
        .rsplit_once(':')
        .expect("a digest renders as algo:hex")
        .1
        .to_owned()
}

/// bazel-remote's uncompressed (v1) key layout: `{prefix}/{kind}/{xx}/{hash}`.
fn key(kind: &str, hash: &str) -> String {
    format!("bazel/{kind}/{}/{hash}", &hash[..2])
}

/// A bare request envelope — the face reads only `input`.
fn request<T>(input: T) -> S3Request<T> {
    S3Request {
        input,
        method: Method::GET,
        uri: Uri::from_static("/"),
        headers: HeaderMap::new(),
        extensions: Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn put_input(key: String, body: &[u8]) -> PutObjectInput {
    let bytes = Bytes::copy_from_slice(body);
    PutObjectInput {
        bucket: "cache".to_owned(),
        key,
        body: Some(StreamingBlob::wrap(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes)
        }))),
        ..Default::default()
    }
}

fn face() -> NessieS3 {
    NessieS3::new(Arc::new(MemCas::new()))
}

#[tokio::test]
async fn a_blob_written_under_its_digest_reads_back_byte_identical() {
    let s3 = face();
    let hash = sha256_hex(BLOB);
    let k = key("cas", &hash);

    s3.put_object(request(put_input(k.clone(), BLOB)))
        .await
        .expect("put a content-addressed blob");

    // HEAD — the client's existence probe — reports the true stored length.
    let head = s3
        .head_object(request(HeadObjectInput {
            bucket: "cache".to_owned(),
            key: k.clone(),
            ..Default::default()
        }))
        .await
        .expect("head a stored blob");
    assert_eq!(head.output.content_length, Some(BLOB.len() as i64));

    let got = s3
        .get_object(request(GetObjectInput {
            bucket: "cache".to_owned(),
            key: k,
            ..Default::default()
        }))
        .await
        .expect("get a stored blob");
    assert_eq!(got.output.content_length, Some(BLOB.len() as i64));

    let body: Vec<Bytes> = got
        .output
        .body
        .expect("a stored blob has a body")
        .try_collect()
        .await
        .expect("stream the body");
    assert_eq!(body.concat(), BLOB);
}

#[tokio::test]
async fn a_body_that_does_not_hash_to_its_key_is_refused() {
    // The security property of the whole face: the client names the digest, the
    // store recomputes it. Nothing is ever filed under a digest it does not have.
    let s3 = face();
    let k = key("cas", &sha256_hex(BLOB));

    let err = s3
        .put_object(request(put_input(k.clone(), b"different bytes entirely")))
        .await
        .expect_err("a mismatched body must not be stored");
    assert_eq!(*err.code(), S3ErrorCode::InvalidDigest);

    // And the rejected write left nothing behind.
    let err = s3
        .get_object(request(GetObjectInput {
            bucket: "cache".to_owned(),
            key: k,
            ..Default::default()
        }))
        .await
        .expect_err("the rejected blob must be absent");
    assert_eq!(*err.code(), S3ErrorCode::NoSuchKey);
}

#[tokio::test]
async fn a_framed_blob_is_refused_with_the_client_flag_that_fixes_it() {
    // bazel-remote's default --s3.storage_mode zstd writes cas.v2 objects whose
    // bytes do not hash to their key. Refusing them silently would be a cold cache
    // nobody can explain, so the message has to name the setting.
    let s3 = face();
    let err = s3
        .put_object(request(put_input(key("cas.v2", &sha256_hex(BLOB)), BLOB)))
        .await
        .expect_err("a framed blob is not content-addressed");
    assert_eq!(*err.code(), S3ErrorCode::InvalidRequest);
    let msg = err.message().expect("the refusal explains itself");
    assert!(
        msg.contains("--s3.storage_mode uncompressed"),
        "the refusal must name the flag; got {msg:?}"
    );
}

#[tokio::test]
async fn action_cache_keys_read_as_a_plain_miss() {
    // Regression guard for the one code that matters operationally: bazel-remote
    // turns NoSuchKey into a cache miss and *every other error* into an internal
    // error that fails the Bazel build. This face serves the CAS prefix only, so
    // ac/ reads must be a miss, never a 500.
    let s3 = face();
    for kind in ["ac", "raw"] {
        let err = s3
            .get_object(request(GetObjectInput {
                bucket: "cache".to_owned(),
                key: key(kind, &sha256_hex(BLOB)),
                ..Default::default()
            }))
            .await
            .expect_err("this face holds no action-cache entries");
        assert_eq!(*err.code(), S3ErrorCode::NoSuchKey, "kind {kind:?}");
    }
}

#[tokio::test]
async fn the_same_blob_is_reachable_under_any_client_prefix() {
    // Content addressing makes prefix configuration unnecessary rather than
    // supported: a write under one prefix is readable under another, because both
    // name the same digest.
    let s3 = face();
    let hash = sha256_hex(BLOB);

    s3.put_object(request(put_input(
        format!("teamA/cas/{}/{hash}", &hash[..2]),
        BLOB,
    )))
    .await
    .expect("store under one prefix");

    let got = s3
        .get_object(request(GetObjectInput {
            bucket: "cache".to_owned(),
            key: format!("teamB/nested/cas/{}/{hash}", &hash[..2]),
            ..Default::default()
        }))
        .await
        .expect("read under another prefix");
    assert_eq!(got.output.content_length, Some(BLOB.len() as i64));
}
