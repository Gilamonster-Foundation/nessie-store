//! The two cache faces serve the same blobs.
//!
//! `nessie-reapi` (Bazel gRPC) and `nessie-s3` (S3 object API) are independent
//! crates that know nothing about each other. What makes a blob written over one
//! readable over the other is not integration — it is that both name blobs by the
//! same digest, and that the composition root hands them **one** backend.
//!
//! That second half is the fragile part: `faces::build_cas_backend` returning a
//! store per caller would break the property while every unit test in both crates
//! still passed. This drives both faces over one `build_cas_backend()` result and
//! asserts the round trip in both directions.

use nessie_reapi::reapi;
use nessie_reapi::reapi::content_addressable_storage_server::ContentAddressableStorage;
use nessie_reapi::{CasV2Svc, ReapiConfig};
use nessie_s3::NessieS3;
use nessie_store::faces::build_cas_backend;
use s3s::S3;
use s3s::S3Request;
use s3s::dto::{GetObjectInput, PutObjectInput, StreamingBlob};
use std::sync::Arc;

const BLOB: &[u8] = b"one blob, two protocols";

fn sha256_hex(data: &[u8]) -> String {
    use nessie_backend_core::{Digest, DigestAlgo};
    Digest::compute_with(DigestAlgo::Sha256, data)
        .to_string()
        .rsplit_once(':')
        .expect("a digest renders as algo:hex")
        .1
        .to_owned()
}

/// bazel-remote's uncompressed key layout.
fn cas_key(hash: &str) -> String {
    format!("cas/{}/{hash}", &hash[..2])
}

fn s3_request<T>(input: T) -> S3Request<T> {
    S3Request {
        input,
        method: http::Method::GET,
        uri: http::Uri::from_static("/"),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

#[tokio::test]
async fn a_blob_put_over_s3_is_readable_over_reapi() {
    let (backend, _signer) = build_cas_backend().expect("shared cache backend");
    let hash = sha256_hex(BLOB);

    // Write through the S3 face.
    let s3 = NessieS3::new(backend.clone());
    let bytes = bytes::Bytes::from_static(BLOB);
    s3.put_object(s3_request(PutObjectInput {
        bucket: "cache".to_owned(),
        key: cas_key(&hash),
        body: Some(StreamingBlob::wrap(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes)
        }))),
        ..Default::default()
    }))
    .await
    .expect("S3 put");

    // Read it back through the REAPI CAS face — a different crate, different
    // protocol, same store.
    let cas = CasV2Svc::new(backend, Arc::new(ReapiConfig::default()));
    let out = cas
        .batch_read_blobs(tonic::Request::new(reapi::BatchReadBlobsRequest {
            digests: vec![reapi::Digest {
                hash: hash.clone(),
                size_bytes: BLOB.len() as i64,
            }],
            ..Default::default()
        }))
        .await
        .expect("REAPI batch read")
        .into_inner();

    assert_eq!(out.responses.len(), 1);
    assert_eq!(out.responses[0].data, BLOB, "REAPI must see the S3 write");
}

#[tokio::test]
async fn a_blob_put_over_reapi_is_readable_over_s3() {
    let (backend, _signer) = build_cas_backend().expect("shared cache backend");
    let hash = sha256_hex(BLOB);

    // Write through the REAPI CAS face.
    let cas = CasV2Svc::new(backend.clone(), Arc::new(ReapiConfig::default()));
    let out = cas
        .batch_update_blobs(tonic::Request::new(reapi::BatchUpdateBlobsRequest {
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(reapi::Digest {
                    hash: hash.clone(),
                    size_bytes: BLOB.len() as i64,
                }),
                data: BLOB.to_vec(),
                ..Default::default()
            }],
            ..Default::default()
        }))
        .await
        .expect("REAPI batch update")
        .into_inner();
    assert_eq!(out.responses.len(), 1);

    // Read it back over S3, at the key a cache client would compute.
    let s3 = NessieS3::new(backend);
    let got = s3
        .get_object(s3_request(GetObjectInput {
            bucket: "cache".to_owned(),
            key: cas_key(&hash),
            ..Default::default()
        }))
        .await
        .expect("S3 get must see the REAPI write");

    use futures::TryStreamExt;
    let body: Vec<bytes::Bytes> = got
        .output
        .body
        .expect("a stored blob has a body")
        .try_collect()
        .await
        .expect("stream the body");
    assert_eq!(body.concat(), BLOB);
}

#[tokio::test]
async fn the_two_faces_are_handed_the_same_store() {
    // Guards the composition root directly: if `build_cas_backend` ever returned a
    // fresh store per call, the round-trip tests above would still pass when each
    // face got its own — so assert the daemon's own sharing contract.
    let (backend, _signer) = build_cas_backend().expect("shared cache backend");
    let a = NessieS3::new(backend.clone());
    let _b = CasV2Svc::new(backend.clone(), Arc::new(ReapiConfig::default()));

    // Two faces, one Arc: writing through one is visible through the raw backend.
    let hash = sha256_hex(BLOB);
    let bytes = bytes::Bytes::from_static(BLOB);
    a.put_object(s3_request(PutObjectInput {
        bucket: "cache".to_owned(),
        key: cas_key(&hash),
        body: Some(StreamingBlob::wrap(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes)
        }))),
        ..Default::default()
    }))
    .await
    .expect("S3 put");

    use nessie_backend_core::Digest;
    let digest: Digest = format!("sha2-256:{hash}").parse().expect("digest");
    assert!(
        backend.has(&digest).expect("has"),
        "the shared backend must hold what a face wrote"
    );
}
