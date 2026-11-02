//! Flat `<sha256>` keys, driven by a real S3 client (`object_store::aws::AmazonS3`).
//!
//! Issue #129's gate: kyln's `{prefix}{sha256}` layout must work over the wire, be
//! the *same object* as the sharded `cas/{xx}/{sha256}` key for the same digest, and
//! still be refused when the bytes do not hash to the key.

use nessie_backend_core::{Digest, DigestAlgo};
use nessie_backend_mem::MemCas;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use std::sync::Arc;

const BLOB: &[u8] = b"the bytes kyln stored under a flat key";

fn sha256_hex(data: &[u8]) -> String {
    Digest::compute_with(DigestAlgo::Sha256, data)
        .to_string()
        .rsplit_once(':')
        .expect("a digest renders as algo:hex")
        .1
        .to_owned()
}

/// Serve the S3 face on a loopback port and return a client pointed at it.
async fn client() -> impl ObjectStore {
    let service = nessie_s3::build_service(Arc::new(MemCas::new()), "AKIDTEST", "secret-test");
    let app = axum::Router::new().fallback_service(
        axum::error_handling::HandleError::<_, _, ()>::new(service, |_e: s3s::HttpError| async {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });

    AmazonS3Builder::new()
        .with_endpoint(format!("http://127.0.0.1:{port}"))
        .with_allow_http(true)
        .with_region("us-east-1")
        .with_bucket_name("cache")
        .with_access_key_id("AKIDTEST")
        .with_secret_access_key("secret-test")
        .build()
        .expect("build the S3 client")
}

#[tokio::test]
async fn a_flat_key_round_trips_and_is_the_same_object_as_the_sharded_key() {
    let s3 = client().await;
    let hash = sha256_hex(BLOB);
    let flat = Path::from(hash.as_str());
    let sharded = Path::from(format!("cas/{}/{hash}", &hash[..2]));

    s3.put(&flat, PutPayload::from_static(BLOB))
        .await
        .expect("PUT under the flat key");

    let meta = s3.head(&flat).await.expect("HEAD the flat key");
    assert_eq!(meta.size, BLOB.len() as u64);

    let via_flat = s3.get(&flat).await.unwrap().bytes().await.unwrap();
    let via_sharded = s3.get(&sharded).await.unwrap().bytes().await.unwrap();
    assert_eq!(via_flat, BLOB);
    assert_eq!(via_sharded, via_flat, "one digest, one object");
}

#[tokio::test]
async fn mismatched_bytes_under_a_flat_key_are_refused() {
    let s3 = client().await;
    let flat = Path::from(sha256_hex(BLOB));

    let err = s3
        .put(&flat, PutPayload::from_static(b"not the bytes"))
        .await
        .expect_err("a body that does not hash to its flat key must be rejected");
    assert!(
        err.to_string().contains("InvalidDigest"),
        "refused by the digest check, not as an unknown key: {err}"
    );
    s3.head(&flat)
        .await
        .expect_err("the rejected write must not have been stored");
}
